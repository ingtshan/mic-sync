use cpal::traits::{DeviceTrait, HostTrait};

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

/// 按名称查找输出设备;None 时优先选 BlackHole,其次默认输出设备
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
        // 未指定设备时优先 BlackHole——这是虚拟麦克风的回环入口
        for d in devices {
            if d.name().map(|n| n.contains("BlackHole")).unwrap_or(false) {
                return Some(d);
            }
        }
    }
    host.default_output_device()
}

/// 解析输出设备名:None 时优先 BlackHole,其次系统默认(自动跟随用它确定检测目标)
pub fn resolve_output_name(name: Option<&str>) -> Option<String> {
    find_output_device(name).and_then(|d| d.name().ok())
}

/// 交错多声道 → 单声道(取各声道平均)
pub fn interleaved_to_mono_f32(data: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return data.to_vec();
    }
    data.chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

pub fn i16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples.iter().map(|&s| s as f32 / i16::MAX as f32).collect()
}

/// 流式线性插值重采样器(语音场景足够)
pub struct LinearResampler {
    /// 每个输出采样对应的输入步长 = in_rate / out_rate
    step: f64,
    /// 当前段内的小数位置 [0, 1)
    t: f64,
    prev: f32,
    has_prev: bool,
    passthrough: bool,
}

impl LinearResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: in_rate as f64 / out_rate as f64,
            t: 0.0,
            prev: 0.0,
            has_prev: false,
            passthrough: in_rate == out_rate,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        for &s in input {
            if !self.has_prev {
                self.prev = s;
                self.has_prev = true;
                continue;
            }
            while self.t < 1.0 {
                out.push(self.prev + (s - self.prev) * self.t as f32);
                self.t += self.step;
            }
            self.t -= 1.0;
            self.prev = s;
        }
    }
}

/// 计算峰值电平(0.0 ~ 1.0),用于 UI 电平表
pub fn peak_level(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()))
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
        let mut rs = LinearResampler::new(44100, 48000);
        let input = vec![0.5f32; 44100]; // 1 秒
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        // 输出应接近 48000 个采样(边界误差几个采样以内)
        assert!(
            (out.len() as i64 - 48000).abs() < 8,
            "expected ~48000, got {}",
            out.len()
        );
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-4));
    }

    #[test]
    fn resampler_down_48000_to_44100() {
        let mut rs = LinearResampler::new(48000, 44100);
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
        let mut rs = LinearResampler::new(48000, 48000);
        let input: Vec<f32> = (0..1000).map(|i| (i as f32).sin()).collect();
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn i16_f32_roundtrip() {
        let samples: Vec<f32> = vec![-1.0, -0.5, 0.0, 0.5, 1.0];
        let back = i16_to_f32(&f32_to_i16(&samples));
        for (a, b) in samples.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-3, "{a} vs {b}");
        }
    }

    #[test]
    fn mono_mix_averages_channels() {
        // 双声道 [L=1.0, R=0.0] → 0.5
        let stereo = vec![1.0f32, 0.0, 1.0, 0.0];
        let mono = interleaved_to_mono_f32(&stereo, 2);
        assert_eq!(mono, vec![0.5, 0.5]);
    }
}
