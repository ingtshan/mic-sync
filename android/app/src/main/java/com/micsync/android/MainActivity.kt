package com.micsync.android

import android.Manifest
import android.animation.ValueAnimator
import android.app.Activity
import android.app.AlertDialog
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.ColorStateList
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.View
import android.widget.FrameLayout
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import java.net.Inet4Address
import java.net.NetworkInterface

/**
 * 单页 UI(与桌面/iOS 网页版同款设计):中央大录音按钮一键启停共享;
 * 待命时呼吸基线,使用中按钮变绿、圆环扩散、后方滚动真实收音波形;
 * 有人请求用麦时弹授权确认框(仅此一次 / 始终允许 / 拒绝)。
 */
class MainActivity : Activity() {
    private companion object {
        const val REQ_PERMS = 10
    }

    private lateinit var waveform: WaveformView
    private lateinit var btnMic: FrameLayout
    private lateinit var micIcon: ImageView
    private lateinit var pulse1: View
    private lateinit var pulse2: View
    private lateinit var statusDot: View
    private lateinit var statusText: TextView
    private lateinit var subText: TextView
    private lateinit var connCard: LinearLayout
    private lateinit var ipText: TextView

    private val pulseAnimators = mutableListOf<ValueAnimator>()
    private var consentDialog: AlertDialog? = null
    private var shownPending: PendingRequest? = null

    private val handler = Handler(Looper.getMainLooper())
    private val refresh = object : Runnable {
        override fun run() {
            updateUi()
            handler.postDelayed(this, 300)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        // 布局是全暗色设计:去掉系统 ActionBar,状态栏/导航栏融入背景、图标用浅色
        actionBar?.hide()
        window.statusBarColor = getColor(R.color.bg)
        window.navigationBarColor = getColor(R.color.bg)
        @Suppress("DEPRECATION")
        window.decorView.systemUiVisibility = 0

        waveform = findViewById(R.id.waveform)
        btnMic = findViewById(R.id.btn_mic)
        micIcon = findViewById(R.id.mic_icon)
        pulse1 = findViewById(R.id.pulse1)
        pulse2 = findViewById(R.id.pulse2)
        statusDot = findViewById(R.id.status_dot)
        statusText = findViewById(R.id.status_text)
        subText = findViewById(R.id.sub_text)
        connCard = findViewById(R.id.conn_card)
        ipText = findViewById(R.id.ip_text)

        // 波形直接从服务进程内的峰值历史取数,画的是真实收音包络
        waveform.source = { MicServerService.instance?.server?.waveSnapshot() }

        btnMic.setOnClickListener {
            if (MicServerService.instance != null) {
                stopService(Intent(this, MicServerService::class.java))
            } else {
                startWithPermissions()
            }
        }
    }

    override fun onResume() {
        super.onResume()
        handler.post(refresh)
    }

    override fun onPause() {
        super.onPause()
        handler.removeCallbacks(refresh)
    }

    override fun onDestroy() {
        stopPulse()
        consentDialog?.dismiss()
        consentDialog = null
        super.onDestroy()
    }

    private fun startWithPermissions() {
        val missing = mutableListOf<String>()
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            missing.add(Manifest.permission.RECORD_AUDIO)
        }
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            missing.add(Manifest.permission.POST_NOTIFICATIONS)
        }
        if (missing.isNotEmpty()) {
            requestPermissions(missing.toTypedArray(), REQ_PERMS)
            return
        }
        startForegroundService(Intent(this, MicServerService::class.java))
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode != REQ_PERMS) return
        val micGranted = permissions.indices.none {
            permissions[it] == Manifest.permission.RECORD_AUDIO &&
                grantResults[it] != PackageManager.PERMISSION_GRANTED
        }
        if (micGranted) {
            startForegroundService(Intent(this, MicServerService::class.java))
        } else {
            statusText.text = getString(R.string.err_no_mic_permission)
        }
    }

    private fun updateUi() {
        val service = MicServerService.instance
        val server = service?.server
        val streamAddr = server?.streamAddr()
        val streaming = streamAddr != null

        waveform.streaming = streaming
        waveform.breathing = server != null

        // 大按钮三态 + 使用中的扩散圆环
        when {
            server == null -> {
                btnMic.setBackgroundResource(R.drawable.bg_mic_paused)
                stopPulse()
            }
            streaming -> {
                btnMic.setBackgroundResource(R.drawable.bg_mic_live)
                startPulse()
            }
            else -> {
                btnMic.setBackgroundResource(R.drawable.bg_mic_armed)
                stopPulse()
            }
        }
        micIcon.alpha = if (server == null) 0.45f else 1f

        when {
            server == null -> {
                setDot(R.color.text_dim)
                statusText.text = getString(R.string.status_paused)
                subText.text = service?.fatalError ?: getString(R.string.sub_paused)
                connCard.visibility = View.GONE
            }
            streaming -> {
                setDot(R.color.green)
                statusText.text = getString(R.string.status_live)
                val rate = server.lastRate.get()
                subText.text = getString(R.string.sub_live, streamAddr) +
                    if (rate > 0) " · $rate Hz" else ""
                connCard.visibility = View.VISIBLE
                updateIps(server.port)
            }
            else -> {
                setDot(R.color.accent)
                statusText.text = getString(R.string.status_armed, server.port)
                subText.text = server.lastError.get()?.let { getString(R.string.status_error, it) }
                    ?: getString(R.string.sub_armed)
                connCard.visibility = View.VISIBLE
                updateIps(server.port)
            }
        }

        // 授权确认:轮询到新的待确认请求就弹框;请求消失(裁决/超时)就收
        val pending = server?.pending()
        if (pending != null && pending !== shownPending) {
            shownPending = pending
            showConsent(server, pending)
        } else if (pending == null && consentDialog != null) {
            consentDialog?.dismiss()
            consentDialog = null
            shownPending = null
        }
    }

    private fun setDot(colorRes: Int) {
        statusDot.backgroundTintList = ColorStateList.valueOf(getColor(colorRes))
    }

    private fun updateIps(port: Int) {
        val text = lanAddresses().joinToString("\n") { "$it:$port" }
        ipText.text = text.ifEmpty { getString(R.string.no_network) }
    }

    private fun showConsent(server: MicServer, req: PendingRequest) {
        consentDialog?.dismiss()
        consentDialog = AlertDialog.Builder(this)
            .setTitle(getString(R.string.consent_title, req.name))
            .setMessage(getString(R.string.consent_message, req.addr))
            .setPositiveButton(R.string.consent_once) { _, _ -> server.decide(Decision.ONCE) }
            .setNeutralButton(R.string.consent_always) { _, _ -> server.decide(Decision.ALWAYS) }
            .setNegativeButton(R.string.consent_deny) { _, _ -> server.decide(Decision.DENY) }
            .setCancelable(false)
            .show()
    }

    private fun startPulse() {
        if (pulseAnimators.isNotEmpty()) return
        listOf(pulse1 to 0L, pulse2 to 1200L).forEach { (view, delay) ->
            view.visibility = View.VISIBLE
            view.alpha = 0f
            val anim = ValueAnimator.ofFloat(0f, 1f).apply {
                duration = 2400
                startDelay = delay
                repeatCount = ValueAnimator.INFINITE
                addUpdateListener { a ->
                    val t = a.animatedValue as Float
                    view.scaleX = 1f + 0.45f * t
                    view.scaleY = 1f + 0.45f * t
                    view.alpha = (1f - t) * 0.5f
                }
                start()
            }
            pulseAnimators.add(anim)
        }
    }

    private fun stopPulse() {
        if (pulseAnimators.isEmpty()) return
        pulseAnimators.forEach { it.cancel() }
        pulseAnimators.clear()
        pulse1.visibility = View.GONE
        pulse2.visibility = View.GONE
    }

    /** 枚举本机 IPv4 站内地址(Wi-Fi/以太网),客户端在电脑上填它 */
    private fun lanAddresses(): List<String> = try {
        NetworkInterface.getNetworkInterfaces().toList()
            .filter { it.isUp && !it.isLoopback }
            .flatMap { it.inetAddresses.toList() }
            .filterIsInstance<Inet4Address>()
            .filter { it.isSiteLocalAddress }
            .map { it.hostAddress ?: "" }
            .filter { it.isNotEmpty() }
    } catch (_: Exception) {
        emptyList()
    }
}
