package com.micsync.android

import java.net.DatagramPacket
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.MulticastSocket
import java.net.NetworkInterface
import java.net.SocketAddress
import java.net.SocketTimeoutException
import java.util.concurrent.atomic.AtomicBoolean

/**
 * 组播发现应答器,协议与桌面版 src-tauri/src/discovery.rs 逐字节兼容:
 * 组播组 224.0.0.167:47801,JSON 报文。客户端发查询,我们单播应答;
 * 启动时向组播组通告一次上线,让已经开着的客户端立刻看到。
 * 失败不影响服务本身——客户端的子网扫描(/health)仍能发现我们。
 *
 * 注意:Android 上收组播需要先持有 WifiManager.MulticastLock(调用方负责)。
 */
class DiscoveryResponder(
    private val serverPort: Int,
    private val alias: () -> String,
    private val deviceId: String,
) : AutoCloseable {
    companion object {
        const val PORT = 47801
        const val PROTO_V = 1L
        val GROUP: InetAddress = InetAddress.getByName("224.0.0.167")
    }

    private val stop = AtomicBoolean(false)
    private val socket = MulticastSocket(PORT)

    init {
        // 多网卡(Wi-Fi + 热点 + VPN)逐个加入,只靠默认路由会漏掉其他网段的客户端
        val group = InetSocketAddress(GROUP, PORT)
        var joined = 0
        runCatching {
            for (nif in NetworkInterface.getNetworkInterfaces()) {
                if (!nif.isUp || nif.isLoopback) continue
                if (runCatching { socket.joinGroup(group, nif) }.isSuccess) joined++
            }
        }
        if (joined == 0) {
            @Suppress("DEPRECATION")
            socket.joinGroup(GROUP)
        }
        // 超时是常态(平时没人搜索),借它回到循环顶部查停止标志
        socket.soTimeout = 300

        // 上线通告一次:已经打开着的客户端列表里立刻多出我们
        send(encodeAnnounce(alias(), deviceId, serverPort), InetSocketAddress(GROUP, PORT))

        Thread({ loop() }, "mic-discovery").apply {
            isDaemon = true
            start()
        }
    }

    private fun loop() {
        val buf = ByteArray(2048)
        while (!stop.get()) {
            val packet = DatagramPacket(buf, buf.size)
            try {
                socket.receive(packet)
            } catch (_: SocketTimeoutException) {
                continue
            } catch (_: Exception) {
                break // close() 关闭套接字后 receive 抛异常退出
            }
            val raw = String(packet.data, 0, packet.length, Charsets.UTF_8)
            // 只应答查询;别人的应答/通告与自己的回环包都忽略
            if (jsonString(raw, "app") != "micsync" || jsonNumber(raw, "v") != PROTO_V) continue
            if (jsonBool(raw, "query") != true) continue
            if (jsonString(raw, "device_id") == deviceId) continue
            send(encodeAnnounce(alias(), deviceId, serverPort), packet.socketAddress)
        }
    }

    private fun send(payload: ByteArray, to: SocketAddress) {
        runCatching { socket.send(DatagramPacket(payload, payload.size, to)) }
    }

    override fun close() {
        stop.set(true)
        runCatching { socket.close() }
    }
}

/** 上线通告/查询应答报文(与桌面版 encode_announce 一致) */
internal fun encodeAnnounce(alias: String, deviceId: String, port: Int): ByteArray =
    ("""{"app":"micsync","v":1,"query":false,"alias":"${jsonEscape(alias)}",""" +
        """"device_type":"mobile","device_id":"${jsonEscape(deviceId)}","port":$port}""")
        .toByteArray(Charsets.UTF_8)

// ---------- 扁平 JSON 字段提取 ----------
// 发现报文都是我们自己应用发的扁平对象;手写提取器避免在纯 JVM 单测里
// 依赖 Android 才有的 org.json

/** 找到 "key": 之后第一个非空白字符的位置;找不到返回 null */
private fun jsonValueStart(raw: String, key: String): Int? {
    val needle = "\"$key\""
    var from = 0
    while (true) {
        val k = raw.indexOf(needle, from)
        if (k < 0) return null
        var i = k + needle.length
        while (i < raw.length && raw[i].isWhitespace()) i++
        if (i < raw.length && raw[i] == ':') {
            i++
            while (i < raw.length && raw[i].isWhitespace()) i++
            return i
        }
        from = k + 1
    }
}

internal fun jsonString(raw: String, key: String): String? {
    var i = jsonValueStart(raw, key) ?: return null
    if (i >= raw.length || raw[i] != '"') return null
    i++
    val sb = StringBuilder()
    while (i < raw.length) {
        when (val c = raw[i]) {
            '"' -> return sb.toString()
            '\\' -> {
                i++
                if (i >= raw.length) return null
                when (val e = raw[i]) {
                    'n' -> sb.append('\n')
                    'r' -> sb.append('\r')
                    't' -> sb.append('\t')
                    'u' -> {
                        if (i + 4 >= raw.length) return null
                        val code = raw.substring(i + 1, i + 5).toIntOrNull(16) ?: return null
                        sb.append(code.toChar())
                        i += 4
                    }
                    else -> sb.append(e) // 覆盖 \" \\ \/ 与未知转义
                }
            }
            else -> sb.append(c)
        }
        i++
    }
    return null
}

internal fun jsonNumber(raw: String, key: String): Long? {
    val i = jsonValueStart(raw, key) ?: return null
    var j = i
    if (j < raw.length && raw[j] == '-') j++
    while (j < raw.length && raw[j].isDigit()) j++
    return raw.substring(i, j).toLongOrNull()
}

internal fun jsonBool(raw: String, key: String): Boolean? {
    val i = jsonValueStart(raw, key) ?: return null
    return when {
        raw.startsWith("true", i) -> true
        raw.startsWith("false", i) -> false
        else -> null
    }
}
