package com.micsync.android

import java.nio.ByteBuffer

/**
 * IMA ADPCM 编码器(与桌面版 src-tauri/src/audio.rs 的 ImaAdpcm 逐字节兼容,
 * 两端各有一条锁定同一测试向量的单测互相钉死)。
 * 16 位 PCM → 每样本 4 位(4:1);低半字节在前,奇数样本补齐半字节。
 * 自适应状态跨帧延续:一条串流会话一个实例,解码端(桌面)同样从零状态起步。
 */
internal class ImaAdpcmEncoder {
    private var predictor = 0
    private var index = 0

    /** 编码一个样本为 4 位码,并按解码端同一规则推进预测器,保证两端状态一致 */
    private fun encodeSample(sample: Int): Int {
        val step = STEP_TABLE[index]
        var diff = sample - predictor
        var code = 0
        if (diff < 0) {
            code = 8
            diff = -diff
        }
        if (diff >= step) {
            code = code or 4
            diff -= step
        }
        if (diff >= step shr 1) {
            code = code or 2
            diff -= step shr 1
        }
        if (diff >= step shr 2) {
            code = code or 1
        }
        var delta = step shr 3
        if (code and 4 != 0) delta += step
        if (code and 2 != 0) delta += step shr 1
        if (code and 1 != 0) delta += step shr 2
        if (code and 8 != 0) delta = -delta
        predictor = (predictor + delta).coerceIn(Short.MIN_VALUE.toInt(), Short.MAX_VALUE.toInt())
        index = (index + INDEX_TABLE[code]).coerceIn(0, 88)
        return code
    }

    /** 编码整帧追加进 out(调用方复用缓冲):每两个样本合一字节,低半字节在前 */
    fun encodeInto(frame: ShortArray, out: ByteBuffer) {
        var i = 0
        while (i + 1 < frame.size) {
            val lo = encodeSample(frame[i].toInt())
            val hi = encodeSample(frame[i + 1].toInt())
            out.put((lo or (hi shl 4)).toByte())
            i += 2
        }
        if (i < frame.size) out.put(encodeSample(frame[i].toInt()).toByte())
    }

    private companion object {
        val INDEX_TABLE = intArrayOf(-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8)
        val STEP_TABLE = intArrayOf(
            7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55,
            60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307,
            337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411,
            1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358,
            5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500,
            20350, 22385, 24623, 27086, 29794, 32767,
        )
    }
}
