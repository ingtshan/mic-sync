// iOS 上客户端角色被裁掉,只剩服务端会用到的一半函数;其余保留给桌面端与单测
#![cfg_attr(target_os = "ios", allow(dead_code))]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use cpal::traits::{DeviceTrait, HostTrait};

/// 虚拟回环声卡的展示名:macOS 用 BlackHole;Windows 用 VB-Cable
/// (客户端向渲染端 CABLE Input 播放,会议软件从采集端 CABLE Output 录音)
#[cfg(target_os = "windows")]
pub const VIRTUAL_MIC_LABEL: &str = "CABLE Input(VB-Cable)";
#[cfg(not(target_os = "windows"))]
pub const VIRTUAL_MIC_LABEL: &str = "BlackHole";

/// 判定输出设备名是否是虚拟回环声卡(即"虚拟麦克风"的写入口)
pub fn is_virtual_mic_output(name: &str) -> bool {
    #[cfg(target_os = "windows")]
    return name.contains("CABLE");
    #[cfg(not(target_os = "windows"))]
    name.contains("BlackHole")
}

/// 列出所有输入设备名称
pub fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// 列出所有输出设备名称
pub fn output_device_names() -> Vec<String> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devices) => devices.filter_map(|d| d.name().ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// 按名称查找输入设备;None 或找不到时回退到默认输入设备
pub fn find_input_device(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                if d.name().map(|n| n == name).unwrap_or(false) {
                    return Some(d);
                }
            }
        }
    }
    host.default_input_device()
}

/// 按名称查找输出设备;None 时优先选虚拟回环声卡,其次默认输出设备
pub fn find_output_device(name: Option<&str>) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = name {
        if let Ok(devices) = host.output_devices() {
            for d in devices {
                if d.name().map(|n| n == name).unwrap_or(false) {
                    return Some(d);
                }
            }
        }
    } else if let Ok(devices) = host.output_devices() {
        // 未指定设备时优先虚拟回环声卡——这是虚拟麦克风的写入口
        for d in devices {
            if d.name().map(|n| is_virtual_mic_output(&n)).unwrap_or(false) {
                return Some(d);
            }
        }
    }
    host.default_output_device()
}

/// 解析输出设备名:None 时优先虚拟回环声卡,其次系统默认(自动跟随用它确定检测目标)
pub fn resolve_output_name(name: Option<&str>) -> Option<String> {
    find_output_device(name).and_then(|d| d.name().ok())
}

/// 交错多声道样本 → 单声道 i16 帧,顺带算峰值电平(0~1)。
/// 采集回调每 ~10ms 一次,这里单趟完成混音/量化/电平,只分配一次输出帧,
/// 不产生任何中间缓冲(此前 f32→混音→i16 三趟各分配一次)
pub fn mono_i16_frame<T: Copy>(
    data: &[T],
    channels: usize,
    to_f32: impl Fn(T) -> f32,
) -> (Vec<i16>, f32) {
    let ch = channels.max(1);
    let mut out = Vec::with_capacity(data.len() / ch);
    let mut peak = 0.0f32;
    for frame in data.chunks_exact(ch) {
        let m = frame.iter().map(|&s| to_f32(s)).sum::<f32>() / ch as f32;
        peak = peak.max(m.abs());
        out.push((m.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
    }
    (out, peak.min(1.0))
}

/// 流式重采样器:4 点 Catmull-Rom 三次插值。
/// 相比线性插值,高频失真显著更低(见单测的正弦误差对比),
/// 状态只多两个样本的窗口,仍是 O(n) 单趟、无额外分配
pub struct StreamResampler {
    /// 每个输出采样对应的输入步长 = in_rate / out_rate
    step: f64,
    /// 当前段内的小数位置 [0, 1)
    t: f64,
    /// 滑动窗口 [p0, p1, p2, p3]:在 p1→p2 之间插值,p0/p3 提供曲率
    w: [f32; 4],
    /// 已进入窗口的样本数;≥4 才开始出样
    primed: usize,
    passthrough: bool,
}

impl StreamResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: in_rate as f64 / out_rate as f64,
            t: 0.0,
            w: [0.0; 4],
            primed: 0,
            passthrough: in_rate == out_rate,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        for &s in input {
            self.w = [self.w[1], self.w[2], self.w[3], s];
            self.primed += 1;
            if self.primed < 4 {
                continue;
            }
            let [p0, p1, p2, p3] = self.w;
            while self.t < 1.0 {
                let t = self.t as f32;
                // Catmull-Rom:过 p1、p2 的三次样条
                let out_s = p1
                    + 0.5
                        * t
                        * ((p2 - p0)
                            + t * ((2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3)
                                + t * (3.0 * (p1 - p2) + p3 - p0)));
                out.push(out_s);
                self.t += self.step;
            }
            self.t -= 1.0;
        }
    }
}

/// IMA ADPCM 编解码器(编码端与解码端共用同一份自适应状态机)。
/// 16 位 PCM → 每样本 4 位,4:1 压缩;语音在 44.1k/48k 采样率下听感良好。
/// 帧内低半字节在前(与 WAV/IMA 惯例一致);同一条 TCP 串流内状态跨帧延续,
/// 编解码两端各自从零状态开始,天然同步。Android 端有字节兼容的 Kotlin 实现
pub struct ImaAdpcm {
    predictor: i32,
    index: i32,
}

const IMA_INDEX_TABLE: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];
const IMA_STEP_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408,
    449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066,
    2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630,
    9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];

impl ImaAdpcm {
    pub fn new() -> Self {
        Self {
            predictor: 0,
            index: 0,
        }
    }

    /// 编码一个样本为 4 位码,并同步更新预测器状态
    fn encode_sample(&mut self, sample: i16) -> u8 {
        let step = IMA_STEP_TABLE[self.index as usize];
        let mut diff = sample as i32 - self.predictor;
        let mut code = 0u8;
        if diff < 0 {
            code = 8;
            diff = -diff;
        }
        if diff >= step {
            code |= 4;
            diff -= step;
        }
        if diff >= step >> 1 {
            code |= 2;
            diff -= step >> 1;
        }
        if diff >= step >> 2 {
            code |= 1;
        }
        self.advance(code, step);
        code
    }

    /// 按 4 位码推进预测器(编解码共用,保证两端状态一致)
    fn advance(&mut self, code: u8, step: i32) -> i32 {
        let mut delta = step >> 3;
        if code & 4 != 0 {
            delta += step;
        }
        if code & 2 != 0 {
            delta += step >> 1;
        }
        if code & 1 != 0 {
            delta += step >> 2;
        }
        if code & 8 != 0 {
            delta = -delta;
        }
        self.predictor = (self.predictor + delta).clamp(i16::MIN as i32, i16::MAX as i32);
        self.index = (self.index + IMA_INDEX_TABLE[code as usize]).clamp(0, 88);
        self.predictor
    }

    /// 编码整帧,追加到 out:每两个样本合一字节(低半字节在前),奇数补零
    pub fn encode_into(&mut self, frame: &[i16], out: &mut Vec<u8>) {
        out.reserve(frame.len().div_ceil(2));
        let mut pairs = frame.chunks_exact(2);
        for pair in &mut pairs {
            let lo = self.encode_sample(pair[0]);
            let hi = self.encode_sample(pair[1]);
            out.push(lo | (hi << 4));
        }
        if let [last] = pairs.remainder() {
            out.push(self.encode_sample(*last));
        }
    }

    /// 解码 n_samples 个样本(f32,-1~1),追加到 out
    pub fn decode_into_f32(&mut self, data: &[u8], n_samples: usize, out: &mut Vec<f32>) {
        out.reserve(n_samples);
        for i in 0..n_samples {
            let byte = data[i / 2];
            let code = if i % 2 == 0 { byte & 0x0F } else { byte >> 4 };
            let step = IMA_STEP_TABLE[self.index as usize];
            let s = self.advance(code, step);
            out.push(s as f32 / i16::MAX as f32);
        }
    }
}

/// 帧到达抖动估计器(RFC 3550 式滑动平均):
/// 每帧观测「实际到达间隔 − 帧时长」的偏差,J += (|d| − J) / 16。
/// 客户端据此把起播水位调到刚好吸收抖动的深度——网络稳就低延迟,网络抖就少爆音
pub struct JitterEstimator {
    jitter_ms: f64,
}

impl JitterEstimator {
    pub fn new() -> Self {
        Self { jitter_ms: 0.0 }
    }

    /// 观测一帧:delta_ms 为与上一帧的到达间隔,frame_ms 为帧的音频时长
    pub fn observe(&mut self, delta_ms: f64, frame_ms: f64) {
        let d = (delta_ms - frame_ms).abs();
        self.jitter_ms += (d - self.jitter_ms) / 16.0;
    }

    pub fn jitter_ms(&self) -> f64 {
        self.jitter_ms
    }

    /// 建议的起播水位(毫秒):2 倍抖动 + 20ms 余量,再夹进 [floor, 200]。
    /// floor 由调用方在欠载后抬高,避免在坏网络上反复欠载
    pub fn target_prebuffer_ms(&self, floor_ms: u32) -> u32 {
        ((20.0 + 2.0 * self.jitter_ms) as u32).clamp(floor_ms, 200)
    }
}

/// 波形历史容量:采集回调普遍 ~10ms 一块,150 块约 1.5 秒
const WAVE_CAP: usize = 150;

/// 采集电平/波形上报器:瞬时峰值电平 + 最近若干块音频的峰值历史。
/// 音频回调线程写,UI 轮询线程读快照(移动端据此渲染真实收音波形)。
pub struct LevelMeter {
    level: AtomicU32,
    /// 每块音频一个峰值(千分比)的环形历史
    wave: Mutex<VecDeque<u32>>,
    /// 成功写入 wave 的累计块数,前端据此对齐两次快照之间的新增部分
    seq: AtomicU64,
}

impl LevelMeter {
    pub fn new() -> Self {
        Self {
            level: AtomicU32::new(0),
            wave: Mutex::new(VecDeque::with_capacity(WAVE_CAP)),
            seq: AtomicU64::new(0),
        }
    }

    /// 音频回调里调用:上报一块音频的峰值。
    /// 拿不到锁(UI 正在读快照)就丢这一块,绝不阻塞音频线程
    pub fn push(&self, peak: f32) {
        self.level.store(encode_level(peak), Ordering::Relaxed);
        if let Ok(mut wave) = self.wave.try_lock() {
            if wave.len() >= WAVE_CAP {
                wave.pop_front();
            }
            wave.push_back(encode_level(peak));
            self.seq.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn level(&self) -> f32 {
        decode_level(self.level.load(Ordering::Relaxed))
    }

    /// 会话结束时清零电平与波形(seq 保留,前端不会把旧数据当增量)
    pub fn reset(&self) {
        self.level.store(0, Ordering::Relaxed);
        if let Ok(mut wave) = self.wave.lock() {
            wave.clear();
        }
    }

    /// 波形快照:(峰值 0~1 序列, 累计块计数)
    pub fn wave_snapshot(&self) -> (Vec<f32>, u64) {
        let wave = match self.wave.lock() {
            Ok(w) => w.iter().map(|&v| decode_level(v)).collect(),
            Err(_) => Vec::new(),
        };
        (wave, self.seq.load(Ordering::Relaxed))
    }
}

/// 将 f32 电平(0~1)编码进 AtomicU32(存千分比)
pub fn encode_level(level: f32) -> u32 {
    (level.clamp(0.0, 1.0) * 1000.0) as u32
}

pub fn decode_level(raw: u32) -> f32 {
    raw as f32 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_up_44100_to_48000() {
        let mut rs = StreamResampler::new(44100, 48000);
        let input = vec![0.5f32; 44100]; // 1 秒
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        // 输出应接近 48000 个采样(起步窗口与边界误差几个采样以内)
        assert!(
            (out.len() as i64 - 48000).abs() < 8,
            "expected ~48000, got {}",
            out.len()
        );
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-4));
    }

    #[test]
    fn resampler_down_48000_to_44100() {
        let mut rs = StreamResampler::new(48000, 44100);
        let input = vec![0.25f32; 48000];
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert!(
            (out.len() as i64 - 44100).abs() < 8,
            "expected ~44100, got {}",
            out.len()
        );
    }

    #[test]
    fn resampler_passthrough_same_rate() {
        let mut rs = StreamResampler::new(48000, 48000);
        let input: Vec<f32> = (0..1000).map(|i| (i as f32).sin()).collect();
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert_eq!(out, input);
    }

    /// 换三次插值的理由要可回归:对 1kHz 正弦做 44.1k→48k,
    /// 与理想正弦的误差必须显著小于线性插值
    #[test]
    fn cubic_resampler_beats_linear_on_sine() {
        use std::f64::consts::TAU;
        let (fs_in, fs_out, freq) = (44100u32, 48000u32, 1000.0f64);
        let n = 4410; // 100ms
        let input: Vec<f32> = (0..n)
            .map(|i| (TAU * freq * i as f64 / fs_in as f64).sin() as f32)
            .collect();
        let step = fs_in as f64 / fs_out as f64;
        let ideal = |t_in: f64| (TAU * freq * t_in / fs_in as f64).sin();
        let rms = |e: &[f64]| (e.iter().map(|x| x * x).sum::<f64>() / e.len() as f64).sqrt();

        let mut rs = StreamResampler::new(fs_in, fs_out);
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        // 本实现的输出时间轴:第 k 个输出对应输入时刻 1 + k*step(见滑动窗口定义)
        let cubic: Vec<f64> = out
            .iter()
            .enumerate()
            .map(|(k, &v)| v as f64 - ideal(1.0 + k as f64 * step))
            .collect();

        // 线性插值参照(旧实现的逐段 lerp,时间轴为 k*step)
        let mut lin_out = Vec::new();
        let (mut t, mut prev) = (0.0f64, input[0]);
        for &s in &input[1..] {
            while t < 1.0 {
                lin_out.push(prev as f64 + (s - prev) as f64 * t);
                t += step;
            }
            t -= 1.0;
            prev = s;
        }
        let linear: Vec<f64> = lin_out
            .iter()
            .enumerate()
            .map(|(k, &v)| v - ideal(k as f64 * step))
            .collect();

        let (c, l) = (rms(&cubic), rms(&linear));
        assert!(c < 1e-3, "三次插值误差应低于 1e-3,实测 {c}");
        assert!(c * 3.0 < l, "三次插值应显著优于线性:cubic={c} linear={l}");
    }

    // ---------- IMA ADPCM ----------

    /// 跨帧编解码往返:两端各自从零状态开始应保持同步,误差在语音可用范围
    #[test]
    fn adpcm_roundtrip_tracks_sine_across_frames() {
        use std::f64::consts::TAU;
        let n = 9600; // 200ms @48k
        let src: Vec<i16> = (0..n)
            .map(|i| ((TAU * 440.0 * i as f64 / 48000.0).sin() * 0.5 * i16::MAX as f64) as i16)
            .collect();
        let mut enc = ImaAdpcm::new();
        let mut dec = ImaAdpcm::new();
        let mut bytes = Vec::new();
        let mut out = Vec::new();
        // 按 10ms 一帧走,验证状态跨帧延续(与串流协议一致)
        for frame in src.chunks(480) {
            bytes.clear();
            enc.encode_into(frame, &mut bytes);
            assert_eq!(bytes.len(), frame.len().div_ceil(2), "4:1 压缩(半字节/样本)");
            dec.decode_into_f32(&bytes, frame.len(), &mut out);
        }
        assert_eq!(out.len(), n);
        // 跳过起始自适应爬坡(~5ms),之后 RMS 误差应低于 0.02(约 25dB SNR 下限)
        let errs: Vec<f64> = (240..n)
            .map(|i| out[i] as f64 - src[i] as f64 / i16::MAX as f64)
            .collect();
        let rms = (errs.iter().map(|e| e * e).sum::<f64>() / errs.len() as f64).sqrt();
        assert!(rms < 0.02, "ADPCM 往返误差过大: {rms}");
    }

    /// 奇数样本数:最后一个样本占低半字节,字节数为 ceil(n/2)
    #[test]
    fn adpcm_handles_odd_frame_length() {
        let mut enc = ImaAdpcm::new();
        let mut dec = ImaAdpcm::new();
        let mut bytes = Vec::new();
        enc.encode_into(&[100, -200, 300], &mut bytes);
        assert_eq!(bytes.len(), 2);
        let mut out = Vec::new();
        dec.decode_into_f32(&bytes, 3, &mut out);
        assert_eq!(out.len(), 3);
    }

    /// 线格式锁定:这串字节同时写死在 Android 端单测里,
    /// 两端实现必须逐字节一致,改任何一边的状态机都会在这里翻车
    #[test]
    fn adpcm_known_vector_locks_wire_format() {
        let samples: [i16; 10] = [0, 1000, 3000, 6000, 10000, 6000, 0, -6000, -10000, -3000];
        let mut enc = ImaAdpcm::new();
        let mut bytes = Vec::new();
        enc.encode_into(&samples, &mut bytes);
        assert_eq!(bytes, vec![0x70u8, 0x77, 0x77, 0xFE, 0x0F]);
    }

    /// 热路径分配预算的回归防线(见 lib.rs 的计数分配器):
    /// 采集侧每帧恰好 1 次分配(发出去的帧本身),
    /// 收流侧解码+重采样在缓冲复用后稳态零分配——改坏任何一处都会在这里现形
    #[test]
    fn hot_paths_hold_allocation_budget() {
        let allocs = crate::test_alloc::allocs_on_this_thread;

        // 采集路径:mono_i16_frame(10ms 立体声 @48k)
        let input = vec![0.1f32; 960];
        std::hint::black_box(mono_i16_frame(&input, 2, |s| s));
        let before = allocs();
        for _ in 0..100 {
            let (frame, _) = mono_i16_frame(&input, 2, |s| s);
            std::hint::black_box(&frame);
        }
        assert_eq!(allocs() - before, 100, "采集帧路径应为每帧恰好 1 次分配");

        // 收流路径:ADPCM 解码 → 重采样,全部缓冲跨帧复用
        let mut enc = ImaAdpcm::new();
        let frame: Vec<i16> = (0..480).map(|i| ((i % 7) * 1000) as i16).collect();
        let mut bytes = Vec::new();
        enc.encode_into(&frame, &mut bytes);
        let mut dec = ImaAdpcm::new();
        let mut rs = StreamResampler::new(48000, 44100);
        let mut mono: Vec<f32> = Vec::new();
        let mut resampled: Vec<f32> = Vec::new();
        for _ in 0..4 {
            // 热身:缓冲长到稳态容量
            mono.clear();
            dec.decode_into_f32(&bytes, frame.len(), &mut mono);
            resampled.clear();
            rs.process(&mono, &mut resampled);
        }
        let before = allocs();
        for _ in 0..100 {
            mono.clear();
            dec.decode_into_f32(&bytes, frame.len(), &mut mono);
            resampled.clear();
            rs.process(&mono, &mut resampled);
            std::hint::black_box(&resampled);
        }
        assert_eq!(allocs() - before, 0, "收流解码+重采样路径稳态必须零分配");
    }

    // ---------- 抖动估计 ----------

    #[test]
    fn jitter_estimator_tracks_arrival_variance() {
        let mut j = JitterEstimator::new();
        // 平稳到达:抖动趋近 0,水位贴着下限
        for _ in 0..200 {
            j.observe(20.0, 20.0);
        }
        assert!(j.jitter_ms() < 0.01, "平稳网络抖动应趋近 0: {}", j.jitter_ms());
        assert_eq!(j.target_prebuffer_ms(40), 40);
        // 到达间隔在 0/40ms 间摆动(帧时长 20ms)→ 抖动收敛到 ~20ms,水位抬高
        for i in 0..200 {
            j.observe(if i % 2 == 0 { 0.0 } else { 40.0 }, 20.0);
        }
        assert!(j.jitter_ms() > 10.0, "高抖动应被察觉: {}", j.jitter_ms());
        let t = j.target_prebuffer_ms(40);
        assert!(t > 40 && t <= 200, "水位应随抖动抬高: {t}");
        // 欠载后抬高的下限必须生效
        assert!(j.target_prebuffer_ms(120) >= 120);
    }

    #[test]
    fn mono_frame_quantizes_accurately() {
        let samples: Vec<f32> = vec![-1.0, -0.5, 0.0, 0.5, 1.0];
        let (frame, peak) = mono_i16_frame(&samples, 1, |s| s);
        for (a, b) in samples.iter().zip(frame.iter()) {
            let back = *b as f32 / i16::MAX as f32;
            assert!((a - back).abs() < 1e-3, "{a} vs {back}");
        }
        assert!((peak - 1.0).abs() < 1e-6);
    }

    #[test]
    fn mono_frame_averages_channels_and_reports_peak() {
        // 双声道 [L=1.0, R=0.0] → 0.5
        let stereo = vec![1.0f32, 0.0, 1.0, 0.0];
        let (frame, peak) = mono_i16_frame(&stereo, 2, |s| s);
        let half = (0.5 * i16::MAX as f32) as i16;
        assert_eq!(frame, vec![half, half]);
        assert!((peak - 0.5).abs() < 1e-6);
    }

    #[test]
    fn mono_frame_converts_integer_sources() {
        // i16 输入经 to_f32 归一化后应无损往返(单声道)
        let samples: Vec<i16> = vec![i16::MIN + 1, -1234, 0, 1234, i16::MAX];
        let (frame, peak) = mono_i16_frame(&samples, 1, |s: i16| s as f32 / i16::MAX as f32);
        assert_eq!(frame, samples);
        assert!((peak - 1.0).abs() < 1e-4);
    }

    #[test]
    fn level_meter_wave_ring_and_seq() {
        let m = LevelMeter::new();
        assert_eq!(m.wave_snapshot(), (Vec::new(), 0));

        m.push(0.5);
        m.push(0.25);
        assert!((m.level() - 0.25).abs() < 1e-3);
        let (wave, seq) = m.wave_snapshot();
        assert_eq!(seq, 2);
        assert!((wave[0] - 0.5).abs() < 1e-3 && (wave[1] - 0.25).abs() < 1e-3);

        // 超出容量后旧数据被挤出,seq 持续累计
        for _ in 0..WAVE_CAP {
            m.push(1.0);
        }
        let (wave, seq) = m.wave_snapshot();
        assert_eq!(wave.len(), WAVE_CAP);
        assert_eq!(seq, 2 + WAVE_CAP as u64);
        assert!(wave.iter().all(|&v| v > 0.99));

        // reset 清空波形与电平,seq 保留
        m.reset();
        assert_eq!(m.level(), 0.0);
        let (wave, seq) = m.wave_snapshot();
        assert!(wave.is_empty());
        assert_eq!(seq, 2 + WAVE_CAP as u64);
    }
}
