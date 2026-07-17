package com.micsync.android

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import java.util.concurrent.atomic.AtomicBoolean

/**
 * 前台服务(microphone 类型)承载 MicServer:保证锁屏/后台时
 * 采集与网络串流不被系统冻结。启动即监听 API,麦克风仍按需开启。
 */
class MicServerService : Service() {
    companion object {
        const val PORT = 47800
        const val ACTION_STOP = "com.micsync.android.STOP"
        private const val CHANNEL_ID = "micsync_server"
        private const val NOTIF_ID = 1

        /** 供 UI 读取的运行实例;null = 服务未运行 */
        @Volatile
        var instance: MicServerService? = null
    }

    var server: MicServer? = null

    /** 服务启动失败原因(端口占用等),UI 展示后服务自行停止 */
    @Volatile
    var fatalError: String? = null

    private var responder: DiscoveryResponder? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var multicastLock: WifiManager.MulticastLock? = null
    private val stopped = AtomicBoolean(false)

    override fun onCreate() {
        super.onCreate()
        createChannel()
        val notification = buildNotification(getString(R.string.notif_idle))
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIF_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
        } else {
            startForeground(NOTIF_ID, notification)
        }

        // 设备身份与信任列表:授权闸门凭令牌认人,/health 与发现应答带公开身份
        val prefs = Prefs(this)
        server = try {
            MicServer(PORT, AndroidCaptureFactory(), { prefs.deviceName() }, prefs.deviceId, prefs)
        } catch (e: Exception) {
            fatalError = getString(R.string.err_port, PORT, e.message ?: "")
            stopSelf()
            return
        }
        instance = this

        // 串流期间锁 CPU 和 Wi-Fi,防省电策略掐断网络
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "micsync:server").apply { acquire() }
        val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        @Suppress("DEPRECATION")
        wifiLock = wm.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "micsync:server").apply { acquire() }

        // 组播发现应答:Android 收组播要先持有 MulticastLock。
        // 失败不影响服务本身——客户端的子网扫描(/health)仍能发现我们
        multicastLock = wm.createMulticastLock("micsync:discovery").apply { acquire() }
        responder = runCatching {
            DiscoveryResponder(PORT, { prefs.deviceName() }, prefs.deviceId)
        }.getOrNull()

        // 低频刷新通知文本:待命 / 正在送出麦克风
        Thread({
            var last = ""
            while (!stopped.get()) {
                val srv = server ?: break
                val addr = srv.streamAddr()
                val text = if (addr != null) getString(R.string.notif_streaming, addr)
                    else getString(R.string.notif_idle)
                if (text != last) {
                    last = text
                    val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
                    nm.notify(NOTIF_ID, buildNotification(text))
                }
                Thread.sleep(1000)
            }
        }, "notif-refresh").apply {
            isDaemon = true
            start()
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        return START_STICKY
    }

    override fun onDestroy() {
        stopped.set(true)
        instance = null
        responder?.close()
        responder = null
        server?.close()
        server = null
        wakeLock?.let { if (it.isHeld) it.release() }
        wifiLock?.let { if (it.isHeld) it.release() }
        multicastLock?.let { if (it.isHeld) it.release() }
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun createChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.notif_channel),
            NotificationManager.IMPORTANCE_LOW,
        )
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(channel)
    }

    private fun buildNotification(text: String): Notification {
        val openIntent = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val stopIntent = PendingIntent.getService(
            this, 1,
            Intent(this, MicServerService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mic)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setContentIntent(openIntent)
            .setOngoing(true)
            .addAction(
                Notification.Action.Builder(null, getString(R.string.action_stop), stopIntent).build()
            )
            .build()
    }
}
