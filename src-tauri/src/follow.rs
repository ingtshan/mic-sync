//! 自动跟随:监视本机应用对 BlackHole 的采集(micuse 模块),
//! 检测到有应用把它当麦克风用 → 自动向服务端认领 /stream(远端开麦);
//! 应用停止采集(静默挂起时长后)→ 自动释放(远端关麦)。
//! 被其他设备接管时进入抑制态,等本轮本机使用结束后再重新武装,避免设备间拉锯。

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::client::{self, ClientHandle};
use crate::micuse::{self, MicUse};

/// 本机应用停止采集后,再等这么久才释放远端麦克风(挂断-重连抖动保护)
const RELEASE_HANGOVER_MS: u64 = 2000;
/// 检测轮询间隔(查本机 HAL 属性,开销极小)
const POLL_MS: u64 = 300;
/// 认领失败(服务端不可达等)后的重试间隔
const CLAIM_RETRY_MS: u64 = 1500;

/// 展示用状态
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    /// 待命:等待本机应用开始使用虚拟麦克风
    Armed = 0,
    /// 本机应用使用中,已认领(或正在认领)远端麦克风
    Active = 1,
    /// 被其他设备接管:等本轮本机使用结束后回到待命
    Suppressed = 2,
    /// 找不到目标输出设备(BlackHole 未安装/被拔出)
    DeviceMissing = 3,
    /// 系统不支持(macOS < 14 无 Process Objects API)
    Unsupported = 4,
}

impl Phase {
    fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::Active,
            2 => Phase::Suppressed,
            3 => Phase::DeviceMissing,
            4 => Phase::Unsupported,
            _ => Phase::Armed,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Armed => "armed",
            Phase::Active => "active",
            Phase::Suppressed => "suppressed",
            Phase::DeviceMissing => "device_missing",
            Phase::Unsupported => "unsupported",
        }
    }
}

/// 跟随状态机(与 CoreAudio / 网络解耦,可单测)
struct FollowGate {
    phase: Phase,
    /// Active 中本机停用的起始时刻;None = 使用中
    idle_since_ms: Option<u64>,
    hangover_ms: u64,
}

#[derive(Debug, PartialEq)]
enum FollowAction {
    None,
    /// 认领远端麦克风
    Claim,
    /// 释放远端麦克风
    Release,
}

impl FollowGate {
    fn new(hangover_ms: u64) -> Self {
        Self {
            phase: Phase::Armed,
            idle_since_ms: None,
            hangover_ms,
        }
    }

    /// 双信号驱动:
    /// device_busy —— BlackHole 上有任意进程在跑 IO('gone';待命期我们不持有流,
    ///   它变 true 即「本机应用开始用虚拟麦克风」,是精确的开始信号);
    /// other_capturing —— 除本进程外有进程在采集输入(使用中我们自己在播放,
    ///   device_busy 恒 true 失效,用它判断本机应用是否已停止)。
    fn step(&mut self, device_busy: bool, other_capturing: bool, now_ms: u64) -> FollowAction {
        match self.phase {
            Phase::Armed => {
                if device_busy {
                    self.phase = Phase::Active;
                    self.idle_since_ms = None;
                    FollowAction::Claim
                } else {
                    FollowAction::None
                }
            }
            Phase::Active => {
                if other_capturing {
                    self.idle_since_ms = None;
                    return FollowAction::None;
                }
                match self.idle_since_ms {
                    None => {
                        self.idle_since_ms = Some(now_ms);
                        FollowAction::None
                    }
                    Some(t) if now_ms.saturating_sub(t) >= self.hangover_ms => {
                        // 释放后进入抑制态排空:等我们自己的播放流关闭、
                        // device_busy 回落后再武装,避免把自己的余量当新事件
                        self.phase = Phase::Suppressed;
                        self.idle_since_ms = None;
                        FollowAction::Release
                    }
                    Some(_) => FollowAction::None,
                }
            }
            // 抑制态:设备完全空闲(本机应用与我们的播放都停了)才重新武装,
            // 既做释放后的排空,也避免被接管后与接管方拉锯
            Phase::Suppressed => {
                if !device_busy {
                    self.phase = Phase::Armed;
                }
                FollowAction::None
            }
            Phase::DeviceMissing | Phase::Unsupported => FollowAction::None,
        }
    }

    /// 内层会话被服务端结束(被接管/服务停止)
    fn on_session_ended(&mut self) {
        if self.phase == Phase::Active {
            self.phase = Phase::Suppressed;
            self.idle_since_ms = None;
        }
    }

    /// 认领失败(服务端不可达等):回到待命,由外层退避后重试
    fn on_claim_failed(&mut self) {
        if self.phase == Phase::Active {
            self.phase = Phase::Armed;
            self.idle_since_ms = None;
        }
    }
}

struct Shared {
    phase: AtomicU8,
    local_in_use: AtomicBool,
    client: Mutex<Option<ClientHandle>>,
    error: Mutex<Option<String>>,
}

pub struct FollowHandle {
    pub addr: String,
    pub output_device: String,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl FollowHandle {
    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.shared.phase.load(Ordering::Relaxed))
    }

    pub fn local_in_use(&self) -> bool {
        self.shared.local_in_use.load(Ordering::Relaxed)
    }

    pub fn take_error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    /// 对内层认领会话做只读访问(状态展示)
    pub fn with_client<T>(&self, f: impl FnOnce(Option<&ClientHandle>) -> T) -> T {
        f(self.shared.client.lock().unwrap().as_ref())
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.shared.client.lock().unwrap().take() {
            h.stop();
        }
    }
}

impl Drop for FollowHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动自动跟随。output_device 必须是 BlackHole 一类的回环设备名,
/// 检测目标与播放目标是同一个设备。
pub fn start(addr: String, output_device: String) -> Result<FollowHandle, String> {
    if !output_device.contains("BlackHole") {
        return Err("自动跟随需要选择 BlackHole 输出设备(检测的就是它的使用状态)".into());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        phase: AtomicU8::new(Phase::Armed as u8),
        local_in_use: AtomicBool::new(false),
        client: Mutex::new(None),
        error: Mutex::new(None),
    });

    {
        let stop = stop.clone();
        let shared = shared.clone();
        let addr = addr.clone();
        let device = output_device.clone();
        thread::Builder::new()
            .name("mic-follow".into())
            .spawn(move || follow_thread(addr, device, stop, shared))
            .map_err(|e| format!("创建跟随线程失败: {e}"))?;
    }

    Ok(FollowHandle {
        addr,
        output_device,
        stop,
        shared,
    })
}

fn follow_thread(addr: String, device_name: String, stop: Arc<AtomicBool>, shared: Arc<Shared>) {
    let mut gate = FollowGate::new(RELEASE_HANGOVER_MS);
    let start = Instant::now();
    let mut device_id: Option<u32> = None;

    while !stop.load(Ordering::SeqCst) {
        // 找目标设备(支持热插拔/晚装 BlackHole)
        if device_id.is_none() {
            device_id = micuse::find_device_by_name(&device_name);
            if device_id.is_none() {
                shared.phase.store(Phase::DeviceMissing as u8, Ordering::Relaxed);
                sleep_check(&stop, 1000);
                continue;
            }
        }

        let dev = device_id.unwrap();
        let device_busy = micuse::device_running_somewhere(dev);
        let other_capturing = match micuse::other_process_capturing(dev) {
            MicUse::InUse => true,
            MicUse::Idle => false,
            MicUse::Unsupported => {
                shared.phase.store(Phase::Unsupported as u8, Ordering::Relaxed);
                *shared.error.lock().unwrap() =
                    Some("自动跟随需要 macOS 14 及以上(Process Objects API),请改用手动模式".into());
                return;
            }
        };
        // 展示口径:待命/抑制期看设备占用,使用中看是否仍有应用在采集
        let in_use_display = if gate.phase == Phase::Active {
            other_capturing
        } else {
            device_busy
        };
        shared.local_in_use.store(in_use_display, Ordering::Relaxed);

        // 内层会话被服务端结束(被接管/服务停止)→ 进入抑制态
        {
            let mut guard = shared.client.lock().unwrap();
            if let Some(h) = guard.as_ref() {
                if !h.is_connected() {
                    *shared.error.lock().unwrap() = h.take_error();
                    guard.take();
                    gate.on_session_ended();
                }
            }
        }

        let now_ms = start.elapsed().as_millis() as u64;
        match gate.step(device_busy, other_capturing, now_ms) {
            FollowAction::Claim => {
                match client::connect(addr.clone(), Some(device_name.clone())) {
                    Ok(h) => {
                        *shared.client.lock().unwrap() = Some(h);
                        *shared.error.lock().unwrap() = None;
                    }
                    Err(e) => {
                        *shared.error.lock().unwrap() = Some(format!("认领远端麦克风失败: {e}"));
                        gate.on_claim_failed();
                        sleep_check(&stop, CLAIM_RETRY_MS);
                    }
                }
            }
            FollowAction::Release => {
                if let Some(h) = shared.client.lock().unwrap().take() {
                    h.stop();
                }
                *shared.error.lock().unwrap() = None;
            }
            FollowAction::None => {}
        }

        shared.phase.store(gate.phase as u8, Ordering::Relaxed);
        sleep_check(&stop, POLL_MS);
    }

    if let Some(h) = shared.client.lock().unwrap().take() {
        h.stop();
    }
}

fn sleep_check(stop: &AtomicBool, ms: u64) {
    let mut left = ms;
    while left > 0 && !stop.load(Ordering::SeqCst) {
        let step = left.min(100);
        thread::sleep(Duration::from_millis(step));
        left -= step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_claims_on_local_use_and_releases_after_hangover() {
        let mut g = FollowGate::new(2000);
        // 待命:设备空闲不动作
        assert_eq!(g.step(false, false, 0), FollowAction::None);
        // 本机应用开始用 BlackHole(device_busy)→ 认领
        assert_eq!(g.step(true, true, 100), FollowAction::Claim);
        assert_eq!(g.phase, Phase::Active);
        // 使用中:应用仍在采集(device_busy 因我们自己播放恒 true,不参与判断)
        assert_eq!(g.step(true, true, 500), FollowAction::None);
        // 应用停止采集:挂起期内不释放
        assert_eq!(g.step(true, false, 1000), FollowAction::None);
        assert_eq!(g.step(true, false, 2500), FollowAction::None);
        // 期间恢复采集 → 挂起计时清零
        assert_eq!(g.step(true, true, 2800), FollowAction::None);
        assert_eq!(g.step(true, false, 3000), FollowAction::None);
        // 持续停用超过挂起时长 → 释放,进入抑制态排空
        assert_eq!(g.step(true, false, 5100), FollowAction::Release);
        assert_eq!(g.phase, Phase::Suppressed);
        // 我们自己的播放流还没关完(device_busy)→ 继续等
        assert_eq!(g.step(true, false, 5200), FollowAction::None);
        assert_eq!(g.phase, Phase::Suppressed);
        // 设备完全空闲 → 重新武装
        assert_eq!(g.step(false, false, 5500), FollowAction::None);
        assert_eq!(g.phase, Phase::Armed);
        // 再次使用 → 再次认领
        assert_eq!(g.step(true, true, 6000), FollowAction::Claim);
    }

    #[test]
    fn gate_suppressed_after_preemption_until_local_idle() {
        let mut g = FollowGate::new(2000);
        assert_eq!(g.step(true, true, 0), FollowAction::Claim);
        // 被其他设备接管 → 抑制:本机应用仍在采集也不再认领(不拉锯)
        g.on_session_ended();
        assert_eq!(g.phase, Phase::Suppressed);
        assert_eq!(g.step(true, true, 500), FollowAction::None);
        assert_eq!(g.step(true, true, 5000), FollowAction::None);
        // 本机这轮使用结束、设备空闲 → 重新武装
        assert_eq!(g.step(false, false, 6000), FollowAction::None);
        assert_eq!(g.phase, Phase::Armed);
        // 新一轮本机使用 → 恢复自动认领
        assert_eq!(g.step(true, true, 7000), FollowAction::Claim);
    }

    #[test]
    fn gate_retries_after_claim_failure() {
        let mut g = FollowGate::new(2000);
        assert_eq!(g.step(true, true, 0), FollowAction::Claim);
        // 认领失败(服务端不可达)→ 回到待命,下一轮仍在使用则重试
        g.on_claim_failed();
        assert_eq!(g.phase, Phase::Armed);
        assert_eq!(g.step(true, true, 300), FollowAction::Claim);
    }
}
