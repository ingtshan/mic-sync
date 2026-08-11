package com.micsync.android

import java.io.InputStream
import java.io.OutputStream
import java.net.ServerSocket
import java.net.Socket
import java.net.SocketTimeoutException
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference

/**
 * MicSync 服务端协议(与 macOS 版 src-tauri/src/server.rs 逐字节兼容):
 * GET /health → JSON 状态;GET /stream → HTTP 头 + MSY1 二进制流头 + 帧流。
 * 纯 JVM 实现,采集经 [CaptureFactory] 注入,便于用假采集做协议测试。
 */

/** 采集句柄:close 即释放麦克风 */
interface CaptureHandle : AutoCloseable {
    val deviceName: String
    val sampleRate: Int
}

/** 采集工厂:按需开麦,帧回调推送 mono i16;开麦失败抛异常(带用户可读信息) */
fun interface CaptureFactory {
    @Throws(Exception::class)
    fun start(onFrame: (ShortArray) -> Unit): CaptureHandle
}

/** 用户对一次授权请求的裁决(与桌面版 server.rs 的 Decision 对应) */
enum class Decision { ONCE, ALWAYS, DENY }

/**
 * 服务端信任列表:按令牌认客户端。令牌是私密凭据,从不出现在公开接口;
 * 公开的 device_id 只用于发现,不能当信任依据(语义同桌面版 settings.rs)
 */
interface TrustStore {
    /** 令牌是否可信;可信且对方改了名则顺手更新展示名 */
    fun trustedByToken(token: String, name: String): Boolean

    /** 用户选了「始终允许」:签发并持久化一个新令牌 */
    fun trustClient(name: String): String
}

/** 一个正在等用户确认的授权请求;UI 轮询到它就弹确认框 */
class PendingRequest internal constructor(val name: String, val addr: String) {
    internal val decision = java.util.concurrent.CompletableFuture<Decision>()
}

class MicServer(
    requestedPort: Int,
    private val capture: CaptureFactory,
    /** 展示名与公开身份(/health 与发现应答用);默认值供纯协议单测 */
    private val alias: () -> String = { "Android" },
    private val deviceId: String = "",
    /** 授权闸门与信任列表;null = 不设闸(纯协议单测),生产环境必传 */
    private val trust: TrustStore? = null,
    /** 确认超时;单测把它调短以免等 30 秒 */
    private val consentTimeoutMs: Long = 30_000,
) : AutoCloseable {
    companion object {
        val MAGIC = byteArrayOf(0x4D, 0x53, 0x59, 0x31) // "MSY1"

        /** 优雅结束帧(0 长度帧)之后的原因码 */
        const val END_SERVER_CLOSING = 0
        const val END_PREEMPTED = 1

        /** 帧内音频编码,写在流头原「保留」u16 里(语义同桌面版 server.rs 的 Codec):
         *  客户端在 X-MicSync-Codec 请求头声明支持才启用 ADPCM,否则回落 PCM */
        const val CODEC_PCM16 = 0
        const val CODEC_ADPCM = 1

        /** 波形历史容量:20ms 一块,150 块约 3 秒 */
        private const val WAVE_CAP = 150
    }

    private val listener = ServerSocket(requestedPort)
    val port: Int get() = listener.localPort

    private val stopFlag = AtomicBoolean(false)

    /** 当前串流会话;同一时间最多一个,新请求会接管(抢占)旧会话 */
    private class Session(val id: Long, val addr: String, val end: AtomicBoolean)

    private val sessionLock = Any()
    private var session: Session? = null
    private val nextId = AtomicLong(1)

    /** 状态展示:峰值电平(千分比)、最近采样率、最近错误 */
    val levelPermille = AtomicInteger(0)
    val lastRate = AtomicInteger(0)
    val lastError = AtomicReference<String?>(null)

    /** 波形历史:每块音频(20ms)一个峰值(千分比),约 3 秒;UI 按 seq 增量对齐 */
    private val waveLock = Any()
    private val wave = ArrayDeque<Int>()
    private var waveSeq = 0L

    /** 等待用户确认的授权请求;同一时间最多一个 */
    private val pendingRef = AtomicReference<PendingRequest?>(null)

    /** 当前收流客户端地址;null = 麦克风空闲 */
    fun streamAddr(): String? = synchronized(sessionLock) { session?.addr }

    fun level(): Float = levelPermille.get() / 1000f

    /** 当前等待用户确认的授权请求;null = 没有 */
    fun pending(): PendingRequest? = pendingRef.get()

    /** 用户裁决当前授权请求;没有待确认请求时静默忽略(UI 可能慢一拍) */
    fun decide(d: Decision) {
        pendingRef.get()?.decision?.complete(d)
    }

    /** 波形快照:(峰值 0~1 序列, 累计块计数),移动端波形 UI 用 */
    fun waveSnapshot(): Pair<FloatArray, Long> = synchronized(waveLock) {
        Pair(FloatArray(wave.size) { i -> wave[i] / 1000f }, waveSeq)
    }

    /** 波形累计块计数。UI 每帧先查它,没有新数据就不取快照(免 60fps 的数组拷贝) */
    fun waveSeq(): Long = synchronized(waveLock) { waveSeq }

    private fun pushWave(permille: Int) = synchronized(waveLock) {
        if (wave.size >= WAVE_CAP) wave.removeFirst()
        wave.addLast(permille)
        waveSeq++
    }

    private fun clearWave() = synchronized(waveLock) { wave.clear() }

    init {
        Thread({ acceptLoop() }, "mic-http").apply {
            isDaemon = true
            start()
        }
    }

    /** 停止服务:在场会话发结束帧收尾,监听器关闭令 accept 退出 */
    override fun close() {
        stopFlag.set(true)
        synchronized(sessionLock) { session?.end?.set(true) }
        // 服务停了就别再吊着等确认的请求
        pendingRef.get()?.decision?.complete(Decision.DENY)
        runCatching { listener.close() }
    }

    private fun acceptLoop() {
        while (!stopFlag.get()) {
            val sock = try {
                listener.accept()
            } catch (_: Exception) {
                break // close() 关闭监听器后 accept 抛异常退出
            }
            Thread({ handleConn(sock) }, "mic-http-${sock.remoteSocketAddress}").apply {
                isDaemon = true
                start()
            }
        }
    }

    private fun handleConn(sock: Socket) {
        runCatching { sock.tcpNoDelay = true }
        runCatching { sock.soTimeout = 3000 }

        val input = sock.getInputStream()
        val output = sock.getOutputStream()

        val head = readHead(input) ?: run { runCatching { sock.close() }; return }
        val path = parseRequestPath(head)
        if (path == null) {
            writeHttp(output, 400, "Bad Request", """{"error":"bad_request"}""")
            gracefulClose(sock)
            return
        }

        when (path) {
            "/health" -> {
                val (streaming, client) = synchronized(sessionLock) {
                    Pair(session != null, session?.addr ?: "")
                }
                // alias/device_type/device_id 供客户端发现用(语义同桌面版):
                // /health 本身就是发现签名,device_id 让扫描方认出「这是我自己」
                val body =
                    """{"status":"ok","app":"micsync","streaming":$streaming,"client":"$client","sample_rate":${lastRate.get()},"alias":"${jsonEscape(alias())}","device_type":"mobile","device_id":"${jsonEscape(deviceId)}"}"""
                writeHttp(output, 200, "OK", body)
                gracefulClose(sock)
            }
            "/stream" -> handleStream(sock, head, output)
            else -> {
                writeHttp(output, 404, "Not Found", """{"error":"not_found"}""")
                gracefulClose(sock)
            }
        }
    }

    private fun handleStream(sock: Socket, head: String, output: OutputStream) {
        val addr = sock.remoteSocketAddress?.toString()?.removePrefix("/") ?: "?"

        // 编码协商:对方声明支持 ADPCM 才用(4:1 带宽);否则回落 PCM
        val codec = if (headerValue(head, "x-micsync-codec").contains("adpcm")) {
            CODEC_ADPCM
        } else {
            CODEC_PCM16
        }

        // 授权闸门:先确认对方有权使用本机麦克风,再谈认领会话与开麦(语义同桌面版)。
        // 令牌是服务端签发的私密凭据,名字只是展示用的不可信输入
        var issuedToken: String? = null
        val gate = trust
        if (gate != null) {
            val token = headerValue(head, "x-micsync-token")
            val name = sanitizeName(headerValue(head, "x-micsync-name"))
                .ifEmpty { "未命名设备(${addr.substringBefore(':')})" }
            if (token.isEmpty() || !gate.trustedByToken(token, name)) {
                when (awaitConsent(name, addr)) {
                    ConsentOutcome.ONCE -> {}
                    ConsentOutcome.ALWAYS -> issuedToken = gate.trustClient(name)
                    ConsentOutcome.DENIED -> {
                        writeHttp(
                            output, 403, "Forbidden",
                            """{"error":"denied","message":"对方拒绝了本次麦克风使用请求"}"""
                        )
                        gracefulClose(sock)
                        return
                    }
                    ConsentOutcome.TIMEOUT -> {
                        writeHttp(
                            output, 403, "Forbidden",
                            """{"error":"timeout","message":"对方未在时限内确认,请求已取消"}"""
                        )
                        gracefulClose(sock)
                        return
                    }
                    ConsentOutcome.BUSY -> {
                        writeHttp(
                            output, 409, "Conflict",
                            """{"error":"busy","message":"另一台设备的请求正在等待确认,请稍后重试"}"""
                        )
                        gracefulClose(sock)
                        return
                    }
                }
            }
        }

        // 认领会话(抢占式):新请求接管旧串流——同一个人换设备,最新请求在哪人就在哪
        val myEnd = AtomicBoolean(false)
        val myId = nextId.getAndIncrement()
        synchronized(sessionLock) {
            session?.end?.set(true)
            session = Session(myId, addr, myEnd)
        }

        // 按需开启 mic 采集,本会话独占;会话结束即释放麦克风。
        // 队列满(客户端太慢)直接丢帧,由停止信号结束采集
        val queue = ArrayBlockingQueue<ShortArray>(64)
        val handle = try {
            capture.start { frame ->
                val peak = peakPermille(frame)
                levelPermille.set(peak)
                pushWave(peak)
                queue.offer(frame)
            }
        } catch (e: Exception) {
            val msg = e.message ?: "打开麦克风失败"
            lastError.set(msg)
            writeHttp(
                output, 503, "Service Unavailable",
                """{"error":"mic_failed","message":"${jsonEscape(msg)}"}"""
            )
            releaseSession(myId)
            gracefulClose(sock)
            return
        }
        lastRate.set(handle.sampleRate)
        lastError.set(null)

        // 用户选了「始终允许」时,把签发的令牌随响应头交给对方;
        // 对方存下来,以后凭它直接放行,不用再打扰用户
        val tokenHeader = issuedToken?.let { "X-MicSync-Token: $it\r\n" } ?: ""
        val handshakeOk = runCatching {
            output.write(
                ("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n" +
                    "Cache-Control: no-store\r\n" + tokenHeader + "Connection: close\r\n\r\n").toByteArray()
            )
            // 二进制流头: MAGIC(4) + sample_rate u32 LE + channels u16 LE + codec u16 LE
            // (codec 位原为保留字段恒 0,旧客户端忽略它 = 恰好按 PCM 解读)
            val header = ByteBuffer.allocate(12).order(ByteOrder.LITTLE_ENDIAN)
            header.put(MAGIC)
            header.putInt(handle.sampleRate)
            header.putShort(1)
            header.putShort(codec.toShort())
            output.write(header.array())
            output.flush()
        }.isSuccess

        if (handshakeOk) {
            writeFrames(output, queue, myEnd, codec)
        }

        // 会话收尾:关麦克风、清电平/波形、释放会话(可能已被新会话顶替)
        runCatching { handle.close() }
        levelPermille.set(0)
        clearWave()
        releaseSession(myId)
        // 优雅关闭,确保结束帧(原因码)送达对端后再断
        gracefulClose(sock)
    }

    private fun releaseSession(id: Long) {
        synchronized(sessionLock) {
            if (session?.id == id) session = null
        }
    }

    /** 授权闸门的内部结果 */
    private enum class ConsentOutcome { ONCE, ALWAYS, DENIED, TIMEOUT, BUSY }

    /**
     * 挂起请求交给用户裁决:UI 轮询 [pending] 弹确认框、调 [decide]。
     * 同一时间只挂一个,撞上就让对方稍后重试(409);超时视作拒绝。
     */
    private fun awaitConsent(name: String, addr: String): ConsentOutcome {
        val req = PendingRequest(name, addr)
        if (!pendingRef.compareAndSet(null, req)) return ConsentOutcome.BUSY
        val decision = try {
            req.decision.get(consentTimeoutMs, TimeUnit.MILLISECONDS)
        } catch (_: Exception) {
            null
        } finally {
            // 无论结果如何都要摘掉自己的 pending,否则后续请求会一直撞 Busy
            pendingRef.compareAndSet(req, null)
        }
        return when (decision) {
            Decision.ONCE -> ConsentOutcome.ONCE
            Decision.ALWAYS -> ConsentOutcome.ALWAYS
            Decision.DENY -> ConsentOutcome.DENIED
            null -> ConsentOutcome.TIMEOUT
        }
    }

    /** 帧写循环。退出场景:服务停止/被抢占(发 0 长度结束帧告知原因)、客户端断开(写失败)。
     *  编码在这条网络线程做,不占采集回调;ADPCM 状态跨帧延续,随会话建立/销毁 */
    private fun writeFrames(
        output: OutputStream,
        queue: ArrayBlockingQueue<ShortArray>,
        end: AtomicBoolean,
        codec: Int,
    ) {
        // 编码缓冲跨帧复用(帧长固定 20ms,50 帧/秒逐帧分配纯属 GC 压力)
        var buf = ByteBuffer.allocate(0).order(ByteOrder.LITTLE_ENDIAN)
        val encoder = if (codec == CODEC_ADPCM) ImaAdpcmEncoder() else null
        while (true) {
            if (stopFlag.get()) {
                sendEnd(output, END_SERVER_CLOSING)
                return
            }
            if (end.get()) {
                sendEnd(output, END_PREEMPTED)
                return
            }
            val frame = queue.poll(100, TimeUnit.MILLISECONDS) ?: continue
            // 帧: 样本数 u32 LE + 载荷(PCM:每样本 2 字节;ADPCM:每样本半字节)
            val need = 4 + if (encoder != null) (frame.size + 1) / 2 else frame.size * 2
            if (buf.capacity() < need) {
                buf = ByteBuffer.allocate(need).order(ByteOrder.LITTLE_ENDIAN)
            }
            buf.clear()
            buf.putInt(frame.size)
            if (encoder != null) {
                encoder.encodeInto(frame, buf)
            } else {
                for (s in frame) buf.putShort(s)
            }
            try {
                output.write(buf.array(), 0, buf.position())
            } catch (_: Exception) {
                return
            }
        }
    }

    /** 优雅结束帧:0 长度 + 原因码,客户端据此决定是否自动重连 */
    private fun sendEnd(output: OutputStream, reason: Int) {
        val buf = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN)
        buf.putInt(0)
        buf.putInt(reason)
        runCatching {
            output.write(buf.array())
            output.flush()
        }
    }

    /** 优雅关闭:先 FIN 再排空读到对端 EOF。直接 close 可能触发 RST,
     *  让对端丢掉还没读完的响应(如 503 正文、被接管的结束帧原因码) */
    private fun gracefulClose(sock: Socket) {
        runCatching {
            sock.shutdownOutput()
            sock.soTimeout = 800
            val input = sock.getInputStream()
            val buf = ByteArray(256)
            for (i in 0 until 64) {
                val n = try {
                    input.read(buf)
                } catch (_: Exception) {
                    break
                }
                if (n <= 0) break
            }
        }
        runCatching { sock.close() }
    }

    /** 读 HTTP 请求头(到空行为止,上限 8KB);失败或超时返回 null */
    private fun readHead(input: InputStream): String? {
        // ByteArrayOutputStream 存原始字节,不像 ArrayList<Byte> 那样逐字节装箱
        val buf = java.io.ByteArrayOutputStream(256)
        var matched = 0 // 已连续匹配 \r\n\r\n 的前缀长度
        while (true) {
            if (buf.size() >= 8192) return null
            val b = try {
                input.read()
            } catch (_: SocketTimeoutException) {
                return null
            } catch (_: Exception) {
                return null
            }
            if (b < 0) return null
            buf.write(b)
            matched = when {
                b == '\r'.code && (matched == 0 || matched == 2) -> matched + 1
                b == '\n'.code && (matched == 1 || matched == 3) -> matched + 1
                b == '\r'.code -> 1
                else -> 0
            }
            if (matched == 4) break
        }
        return runCatching { String(buf.toByteArray(), Charsets.UTF_8) }.getOrNull()
    }

    private fun writeHttp(output: OutputStream, code: Int, reason: String, body: String) {
        val bytes = body.toByteArray(Charsets.UTF_8)
        val head = "HTTP/1.1 $code $reason\r\nContent-Type: application/json\r\n" +
            "Content-Length: ${bytes.size}\r\nConnection: close\r\n\r\n"
        runCatching {
            output.write(head.toByteArray())
            output.write(bytes)
            output.flush()
        }
    }
}

/** 从请求头解析 GET 路径(去掉 query);非 GET 返回 null */
internal fun parseRequestPath(head: String): String? {
    val line = head.lineSequence().firstOrNull() ?: return null
    val parts = line.split(Regex("\\s+")).filter { it.isNotEmpty() }
    if (parts.size < 2 || parts[0] != "GET") return null
    return parts[1].substringBefore('?')
}

/** 取请求头的值(HTTP 头名大小写不敏感);缺失返回空串 */
internal fun headerValue(head: String, name: String): String {
    val want = name.lowercase()
    return head.lineSequence().drop(1).firstNotNullOfOrNull { line ->
        val i = line.indexOf(':')
        if (i < 0) {
            null
        } else {
            val key = line.substring(0, i).trim().lowercase()
            if (key == want) line.substring(i + 1).trim() else null
        }
    } ?: ""
}

/** 设备名是跨设备传来的不可信输入:去控制字符(含换行)并限长(与桌面版一致) */
internal fun sanitizeName(raw: String): String =
    raw.filter { !it.isISOControl() }.take(40).trim()

/** 峰值电平(千分比),用于 UI 电平表 */
internal fun peakPermille(frame: ShortArray): Int {
    var peak = 0
    for (s in frame) {
        val a = if (s == Short.MIN_VALUE) 32767 else if (s < 0) (-s).toInt() else s.toInt()
        if (a > peak) peak = a
    }
    return peak * 1000 / 32767
}

internal fun jsonEscape(s: String): String =
    buildString {
        for (c in s) when (c) {
            '"' -> append("\\\"")
            '\\' -> append("\\\\")
            '\n' -> append("\\n")
            '\r' -> append("\\r")
            '\t' -> append("\\t")
            else -> if (c < ' ') append("\\u%04x".format(c.code)) else append(c)
        }
    }
