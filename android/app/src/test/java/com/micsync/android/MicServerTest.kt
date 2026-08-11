package com.micsync.android

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.InputStream
import java.net.Socket
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/**
 * 协议测试,与 src-tauri/src/server.rs 的测试一一对应:
 * 空闲不开麦 → 请求即开麦串流 → 新请求抢占 → 断开即关麦;
 * 全部走假采集,不碰真实麦克风。
 */
class MicServerTest {
    /** 当前活跃的假采集线程数,用来断言「按需开麦、用完关麦」 */
    private val activeCaptures = AtomicInteger(0)

    /** 假采集:44100Hz 正弦波,10ms 一帧,直到停止信号 */
    private val fakeCapture = CaptureFactory { onFrame ->
        activeCaptures.incrementAndGet()
        val stop = AtomicBoolean(false)
        Thread {
            var phase = 0.0
            while (!stop.get()) {
                val frame = ShortArray(441) {
                    val s = kotlin.math.sin(phase * 2.0 * Math.PI) * 0.5
                    phase = (phase + 440.0 / 44100.0) % 1.0
                    (s * Short.MAX_VALUE).toInt().toShort()
                }
                onFrame(frame)
                Thread.sleep(10)
            }
            activeCaptures.decrementAndGet()
        }.start()
        object : CaptureHandle {
            override val deviceName = "假麦克风"
            override val sampleRate = 44100
            override fun close() {
                stop.set(true)
            }
        }
    }

    private val failingCapture = CaptureFactory { throw Exception("假装麦克风坏了") }

    private fun httpGet(port: Int, path: String): Socket {
        val s = Socket("127.0.0.1", port)
        s.soTimeout = 5000
        s.getOutputStream().write(
            "GET $path HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n".toByteArray()
        )
        return s
    }

    private fun readAll(s: Socket): String = s.getInputStream().readBytes().toString(Charsets.UTF_8)

    private fun readHead(input: InputStream): String {
        val buf = ArrayList<Byte>()
        while (true) {
            val b = input.read()
            check(b >= 0) { "EOF while reading head" }
            buf.add(b.toByte())
            val n = buf.size
            if (n >= 4 &&
                buf[n - 4] == '\r'.code.toByte() && buf[n - 3] == '\n'.code.toByte() &&
                buf[n - 2] == '\r'.code.toByte() && buf[n - 1] == '\n'.code.toByte()
            ) break
        }
        return String(buf.toByteArray())
    }

    private fun readExact(input: InputStream, n: Int): ByteArray {
        val buf = ByteArray(n)
        var off = 0
        while (off < n) {
            val r = input.read(buf, off, n - off)
            check(r > 0) { "EOF while reading $n bytes" }
            off += r
        }
        return buf
    }

    private fun leInt(b: ByteArray, off: Int): Int =
        (b[off].toInt() and 0xFF) or
            ((b[off + 1].toInt() and 0xFF) shl 8) or
            ((b[off + 2].toInt() and 0xFF) shl 16) or
            ((b[off + 3].toInt() and 0xFF) shl 24)

    /** 读一帧;audio 非 null 为音频帧,否则 endReason 是结束帧原因码 */
    private class Frame(val audio: ShortArray?, val endReason: Int)

    private fun readFrame(input: InputStream): Frame {
        val n = leInt(readExact(input, 4), 0)
        if (n == 0) return Frame(null, leInt(readExact(input, 4), 0))
        val body = readExact(input, n * 2)
        val samples = ShortArray(n) { i ->
            ((body[i * 2].toInt() and 0xFF) or (body[i * 2 + 1].toInt() shl 8)).toShort()
        }
        return Frame(samples, -1)
    }

    /** 跳过残留音频帧直到结束帧,返回原因码 */
    private fun readEndReason(input: InputStream): Int {
        while (true) {
            val f = readFrame(input)
            if (f.audio == null) return f.endReason
        }
    }

    private fun waitUntil(timeoutMs: Long, cond: () -> Boolean): Boolean {
        var waited = 0L
        while (waited < timeoutMs) {
            if (cond()) return true
            Thread.sleep(50)
            waited += 50
        }
        return cond()
    }

    @Test
    fun parseRequestPathVariants() {
        assertEquals("/health", parseRequestPath("GET /health HTTP/1.1\r\nHost: x\r\n\r\n"))
        assertEquals("/stream", parseRequestPath("GET /stream?x=1 HTTP/1.1\r\n\r\n"))
        assertNull(parseRequestPath("POST /health HTTP/1.1\r\n\r\n"))
        assertNull(parseRequestPath(""))
    }

    @Test
    fun peakPermilleBounds() {
        assertEquals(0, peakPermille(ShortArray(10)))
        assertEquals(1000, peakPermille(shortArrayOf(Short.MAX_VALUE)))
        assertEquals(1000, peakPermille(shortArrayOf(Short.MIN_VALUE)))
    }

    /** 核心链路:空闲不开麦 → 请求即开麦串流 → 新请求抢占旧串流 → 断开即关麦 */
    @Test
    fun micOnDemandAndPreemption() {
        val server = MicServer(0, fakeCapture)
        val port = server.port

        // 空闲:不开麦,health 报告空闲
        assertEquals("mic must be off when idle", 0, activeCaptures.get())
        var resp = readAll(httpGet(port, "/health"))
        assertTrue(resp, resp.contains("\"streaming\":false"))
        assertTrue(resp, resp.contains("\"app\":\"micsync\""))

        // 第一个客户端请求 → 自动开麦并串流
        val c1 = httpGet(port, "/stream")
        val in1 = c1.getInputStream()
        val head1 = readHead(in1)
        assertTrue(head1, head1.startsWith("HTTP/1.1 200"))
        val magic = readExact(in1, 12)
        assertEquals('M'.code.toByte(), magic[0])
        assertEquals('S'.code.toByte(), magic[1])
        assertEquals('Y'.code.toByte(), magic[2])
        assertEquals('1'.code.toByte(), magic[3])
        assertEquals(44100, leInt(magic, 4))
        val frame = readFrame(in1)
        assertEquals(441, frame.audio!!.size)
        assertTrue("capture should be running", waitUntil(1000) { activeCaptures.get() == 1 })

        // health 报告使用中
        resp = readAll(httpGet(port, "/health"))
        assertTrue(resp, resp.contains("\"streaming\":true"))

        // 第二个客户端请求 → 接管:c1 收到被抢占的结束帧,c2 正常收流
        val c2 = httpGet(port, "/stream")
        val in2 = c2.getInputStream()
        val head2 = readHead(in2)
        assertTrue(head2, head2.startsWith("HTTP/1.1 200"))
        readExact(in2, 12)
        assertEquals(
            "c1 should be told it was preempted",
            MicServer.END_PREEMPTED, readEndReason(in1)
        )
        assertTrue(readFrame(in2).audio != null)

        // c2 断开 → 会话释放、麦克风关闭
        c2.close()
        assertTrue(
            "session should be released after client disconnects",
            waitUntil(3000) { readAll(httpGet(port, "/health")).contains("\"streaming\":false") }
        )
        assertTrue(
            "mic must be released after last client leaves",
            waitUntil(3000) { activeCaptures.get() == 0 }
        )

        c1.close()
        server.close()
    }

    /** 线格式锁定:与桌面版 audio.rs 的 adpcm_known_vector_locks_wire_format
     *  用同一输入、期望同一串字节——任何一端改了状态机都会在两边同时翻车 */
    @Test
    fun adpcmKnownVectorMatchesDesktop() {
        val samples = shortArrayOf(0, 1000, 3000, 6000, 10000, 6000, 0, -6000, -10000, -3000)
        val enc = ImaAdpcmEncoder()
        val buf = java.nio.ByteBuffer.allocate(5)
        enc.encodeInto(samples, buf)
        assertEquals(5, buf.position())
        val expected = byteArrayOf(0x70, 0x77, 0x77, 0xFE.toByte(), 0x0F)
        assertTrue(
            "编码字节应与桌面版一致: ${buf.array().joinToString()}",
            buf.array().contentEquals(expected)
        )
    }

    /** 客户端声明支持 ADPCM → 流头回告 codec=1,帧载荷压到每样本半字节 */
    @Test
    fun streamNegotiatesAdpcm() {
        val server = MicServer(0, fakeCapture)
        val s = Socket("127.0.0.1", server.port)
        s.soTimeout = 5000
        s.getOutputStream().write(
            ("GET /stream HTTP/1.1\r\nHost: t\r\nX-MicSync-Codec: adpcm\r\n" +
                "Connection: close\r\n\r\n").toByteArray()
        )
        val input = s.getInputStream()
        assertTrue(readHead(input).startsWith("HTTP/1.1 200"))
        val magic = readExact(input, 12)
        assertEquals(
            "流头应回告 ADPCM",
            MicServer.CODEC_ADPCM,
            (magic[10].toInt() and 0xFF) or ((magic[11].toInt() and 0xFF) shl 8)
        )
        // 一帧 441 样本 → 221 字节载荷;若载荷不是 4:1,下一个帧头读到的就是错位垃圾
        val n = leInt(readExact(input, 4), 0)
        assertEquals(441, n)
        readExact(input, (n + 1) / 2)
        assertEquals("载荷长度必须是每样本半字节", 441, leInt(readExact(input, 4), 0))
        s.close()
        server.close()
    }

    /** 不带 X-MicSync-Codec 的旧客户端照旧收 PCM,流头 codec 位为 0 */
    @Test
    fun streamWithoutCodecHeaderStaysPcm() {
        val server = MicServer(0, fakeCapture)
        val c = httpGet(server.port, "/stream")
        val input = c.getInputStream()
        assertTrue(readHead(input).startsWith("HTTP/1.1 200"))
        val magic = readExact(input, 12)
        assertEquals(
            "旧客户端必须回落 PCM",
            MicServer.CODEC_PCM16,
            (magic[10].toInt() and 0xFF) or ((magic[11].toInt() and 0xFF) shl 8)
        )
        assertEquals(441, readFrame(input).audio!!.size)
        c.close()
        server.close()
    }

    /** 服务停止:在场客户端收到 END_SERVER_CLOSING 结束帧 */
    @Test
    fun serverStopNotifiesClient() {
        val server = MicServer(0, fakeCapture)
        val c1 = httpGet(server.port, "/stream")
        val in1 = c1.getInputStream()
        assertTrue(readHead(in1).startsWith("HTTP/1.1 200"))
        readExact(in1, 12)
        assertTrue(readFrame(in1).audio != null)

        server.close()
        assertEquals(MicServer.END_SERVER_CLOSING, readEndReason(in1))
        assertTrue(
            "mic must be released after server stops",
            waitUntil(3000) { activeCaptures.get() == 0 }
        )
        c1.close()
    }

    /** 麦克风打开失败 → 503 + 错误信息,会话释放 */
    @Test
    fun micFailureReturns503() {
        val server = MicServer(0, failingCapture)
        val resp = readAll(httpGet(server.port, "/stream"))
        assertTrue(resp, resp.startsWith("HTTP/1.1 503"))
        assertTrue(resp, resp.contains("mic_failed"))
        assertTrue(resp, resp.contains("假装麦克风坏了"))

        // 会话已释放,health 回到空闲
        val health = readAll(httpGet(server.port, "/health"))
        assertTrue(health, health.contains("\"streaming\":false"))
        server.close()
    }
}
