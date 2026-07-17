package com.micsync.android

import android.annotation.SuppressLint
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import java.util.concurrent.atomic.AtomicBoolean

/**
 * 真实采集:AudioRecord 打开手机麦克风,20ms 一帧推送 mono i16。
 * 与 macOS 版 cpal 采集角色对应;close 即停录并释放麦克风。
 */
class AndroidCaptureFactory : CaptureFactory {
    @SuppressLint("MissingPermission") // RECORD_AUDIO 在 MainActivity 启动服务前已授权
    override fun start(onFrame: (ShortArray) -> Unit): CaptureHandle {
        var record: AudioRecord? = null
        var rate = 0
        for (r in intArrayOf(48000, 44100, 16000)) {
            val minBuf = AudioRecord.getMinBufferSize(
                r, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT
            )
            if (minBuf <= 0) continue
            val rec = try {
                AudioRecord(
                    MediaRecorder.AudioSource.MIC,
                    r,
                    AudioFormat.CHANNEL_IN_MONO,
                    AudioFormat.ENCODING_PCM_16BIT,
                    maxOf(minBuf * 2, r / 5 * 2), // ≥200ms 缓冲,防读取线程偶发调度延迟丢样
                )
            } catch (_: Exception) {
                continue
            }
            if (rec.state == AudioRecord.STATE_INITIALIZED) {
                record = rec
                rate = r
                break
            }
            rec.release()
        }
        val rec = record ?: throw Exception("无法打开麦克风(AudioRecord 初始化失败,请检查录音权限)")

        rec.startRecording()
        if (rec.recordingState != AudioRecord.RECORDSTATE_RECORDING) {
            rec.release()
            throw Exception("麦克风启动失败(可能被其他应用占用)")
        }

        val stop = AtomicBoolean(false)
        Thread({
            val frame = ShortArray(rate / 50) // 20ms
            while (!stop.get()) {
                var off = 0
                while (off < frame.size && !stop.get()) {
                    val n = rec.read(frame, off, frame.size - off)
                    if (n <= 0) {
                        stop.set(true)
                        break
                    }
                    off += n
                }
                if (off > 0) onFrame(frame.copyOf(off))
            }
            runCatching { rec.stop() }
            rec.release()
        }, "mic-capture").start()

        return object : CaptureHandle {
            override val deviceName = "手机麦克风"
            override val sampleRate = rate
            override fun close() {
                stop.set(true)
            }
        }
    }
}
