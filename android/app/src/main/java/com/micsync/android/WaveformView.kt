package com.micsync.android

import android.content.Context
import android.graphics.Canvas
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Shader
import android.util.AttributeSet
import android.view.View
import kotlin.math.min
import kotlin.math.pow
import kotlin.math.sin

/**
 * 实时收音波形(与桌面/iOS 网页版同款视觉):右进左出的峰值条,
 * 竖向 蓝→绿→蓝 渐变、两端淡出;空闲时是呼吸的点状基线。
 * 数据来自 MicServer 的峰值历史,按 seq 增量对齐——只做平滑滚动,
 * 不生造数据,画的是真实收音包络。
 */
class WaveformView @JvmOverloads constructor(
    context: Context,
    attrs: AttributeSet? = null,
) : View(context, attrs) {

    /** 数据源:返回 (峰值 0~1 序列, 累计块计数);null = 服务未运行 */
    var source: (() -> Pair<FloatArray, Long>?)? = null

    /** 快速序号源:只读累计块计数,没有新块就跳过整份快照拷贝(60fps 下省 GC) */
    var seqSource: (() -> Long?)? = null

    /** 空闲基线是否呼吸(服务待命 true / 已暂停 false) */
    var breathing = false

    /** 是否处于收音状态(画波形还是基线) */
    var streaming = false
        set(value) {
            if (field != value) {
                field = value
                if (!value) {
                    bars.clear()
                    total = 0
                    shown = 0.0
                    lastSeq = 0
                }
            }
        }

    private val bars = ArrayDeque<Float>()
    private var total = 0L
    private var shown = 0.0
    private var lastSeq = 0L
    private var lastFrameNs = 0L

    private val paint = Paint(Paint.ANTI_ALIAS_FLAG)
    private var gradient: Shader? = null
    private var gradientH = 0

    private fun ingest() {
        val quickSeq = seqSource?.invoke()
        if (quickSeq != null && quickSeq == lastSeq) return
        val (arr, seq) = source?.invoke() ?: return
        var fresh = seq - lastSeq
        lastSeq = seq
        if (fresh <= 0 || arr.isEmpty()) return
        if (fresh > arr.size) fresh = arr.size.toLong()
        for (i in (arr.size - fresh.toInt()) until arr.size) bars.addLast(arr[i])
        total += fresh
        while (bars.size > MAX_BARS) bars.removeFirst()
        // 首次接入或卡顿后落后太多,直接跳到最新,避免长时间快进
        if (total - shown > 60) shown = (total - 20).toDouble()
    }

    override fun onDraw(canvas: Canvas) {
        val w = width.toFloat()
        val h = height.toFloat()
        if (w <= 0f || h <= 0f) return
        val now = System.nanoTime()
        val dt = if (lastFrameNs == 0L) 0.0 else min(0.1, (now - lastFrameNs) / 1e9)
        lastFrameNs = now
        val midY = h / 2f
        val d = resources.displayMetrics.density

        if (!streaming) {
            // 空闲基线:待命时轻微呼吸,暂停时暗淡定格
            paint.shader = null
            paint.style = Paint.Style.FILL
            paint.color = 0xFF8B90A0.toInt()
            val breath = if (breathing) 0.22 + 0.12 * sin(now / 8e8) else 0.10
            paint.alpha = (255 * breath).toInt()
            var x = 8f * d
            while (x < w - 8f * d) {
                canvas.drawCircle(x, midY, 1.6f * d, paint)
                x += 9f * d
            }
            paint.alpha = 255
            // 空闲基线不必 60fps:呼吸 ~15fps 已够顺滑,暂停态更是几乎静止
            postInvalidateDelayed(if (breathing) 66L else 250L)
            return
        }

        ingest()
        // 平滑追赶最新数据:速度与落后量成正比,把块到达的抖动吃掉
        shown += (total - shown) * min(1.0, dt * 6)

        if (gradient == null || gradientH != height) {
            gradientH = height
            gradient = LinearGradient(
                0f, 0f, 0f, h,
                intArrayOf(0xFF5B8CFF.toInt(), 0xFF3DDC84.toInt(), 0xFF5B8CFF.toInt()),
                floatArrayOf(0f, 0.5f, 1f),
                Shader.TileMode.CLAMP,
            )
        }
        paint.shader = gradient
        paint.style = Paint.Style.STROKE
        paint.strokeWidth = 2.2f * d
        paint.strokeCap = Paint.Cap.ROUND

        val slot = 3.5f * d
        val rightX = w - 12f * d
        val base = total - bars.size
        val maxH = midY - 6f * d
        for (j in bars.indices) {
            val x = (rightX - (shown - (base + j)) * slot).toFloat()
            if (x < -slot) continue
            if (x > w) break
            // 0.6 次方拉高小信号,轻声说话也看得见起伏
            val bh = maxOf(1.8f * d, bars[j].pow(0.6f) * maxH)
            // 左右两端淡出
            val edge = minOf(1f, x / (70f * d), (w - x) / (26f * d))
            paint.alpha = (255 * maxOf(0.08f, edge)).toInt()
            canvas.drawLine(x, midY - bh, x, midY + bh, paint)
        }
        paint.alpha = 255
        postInvalidateOnAnimation()
    }

    private companion object {
        /** 本地保留的峰值条数上限,够铺满屏幕 */
        const val MAX_BARS = 480
    }
}
