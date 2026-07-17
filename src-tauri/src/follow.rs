//! 自动跟随:监视本机应用对虚拟声卡(BlackHole / VB-Cable)的采集(micuse 模块),
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
    /// 找不到目标输出设备(虚拟声卡未安装/被拔出)
    DeviceMissing = 3,
    /// 系统不支持(如 macOS < 14 无 Process Objects API)
    Unsupported = 4,
    /// 本轮使用结束(正常释放或服务端停止)后的排空:等设备空闲再重新武装
    Draining = 5,
    /// 启动预授权中:已向服务端请求授权,等对方确认(不开麦)
    Authorizing = 6,
    /// 预授权被对方拒绝:终态,等用户自己停掉/重开跟随
    AuthDenied = 7,
}

impl Phase {
    fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::Active,
            2 => Phase::Suppressed,
            3 => Phase::DeviceMissing,
            4 => Phase::Unsupported,
            5 => Phase::Draining,
            6 => Phase::Authorizing,
            7 => Phase::AuthDenied,
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
            Phase::Draining => "draining",
            Phase::Authorizing => "authorizing",
            Phase::AuthDenied => "auth_denied",
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
    /// device_busy —— 虚拟声卡上有任意进程在跑 IO('gone')。单独不可作开始信号:
    ///   模拟器麦克风桥接、系统音频路由等会把它长期顶成 true;
    /// other_capturing —— 除本进程外有进程在采集输入(Process Objects;隐私遮蔽下
    ///   无法归因到具体设备)。单独也不可靠:别的应用用真麦克风也会为 true。
    /// 待命期两者同时成立才认领(设备在跑 + 有人在采集 ≈ 有应用把它当麦克风);
    /// 使用中/排空/抑制期只看 other_capturing——我们自己的播放会把 device_busy
    /// 顶成恒 true,而纯输出的占用(含自己播放的余量)不该阻塞状态机。
    fn step(&mut self, device_busy: bool, other_capturing: bool, now_ms: u64) -> FollowAction {
        match self.phase {
            Phase::Armed => {
                if device_busy && other_capturing {
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
                        // 释放后进入排空态:等我们自己的播放流关闭、
                        // device_busy 回落后再武装,避免把自己的余量当新事件
                        self.phase = Phase::Draining;
                        self.idle_since_ms = None;
                        FollowAction::Release
                    }
                    Some(_) => FollowAction::None,
                }
            }
            // 排空/抑制:等本机采集停止(本轮结束)再重新武装。不等设备空闲——
            // 设备可能被模拟器桥接/系统路由长期占用,等空闲会永远卡死;
            // 认领条件已含 other_capturing,自己播放的余量(纯输出)不会误触发
            Phase::Draining | Phase::Suppressed => {
                if !other_capturing {
                    self.phase = Phase::Armed;
                }
                FollowAction::None
            }
            // 预授权阶段由外层控制,状态机本体不会进入这两个状态
            Phase::DeviceMissing | Phase::Unsupported | Phase::Authorizing | Phase::AuthDenied => {
                FollowAction::None
            }
        }
    }

    /// 内层会话被服务端结束:被接管进抑制态,服务端停止只做排空
    fn on_session_ended(&mut self, preempted: bool) {
        if self.phase == Phase::Active {
            self.phase = if preempted {
                Phase::Suppressed
            } else {
                Phase::Draining
            };
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

/// 启动自动跟随。output_device 必须是虚拟回环声卡的输出端名称;
/// 检测目标由 micuse 平台实现从它推导(macOS 即 BlackHole 本身,
/// Windows 为 CABLE Input 成对的采集端 CABLE Output)。
pub fn start(addr: String, output_device: String) -> Result<FollowHandle, String> {
    if !crate::audio::is_virtual_mic_output(&output_device) {
        return Err(format!(
            "自动跟随需要选择 {} 输出设备(检测的就是它的使用状态)",
            crate::audio::VIRTUAL_MIC_LABEL
        ));
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

/// 系统不支持自动跟随时给用户的提示
#[cfg(target_os = "macos")]
const UNSUPPORTED_MSG: &str = "自动跟随需要 macOS 14 及以上(Process Objects API),请改用手动模式";
#[cfg(not(target_os = "macos"))]
const UNSUPPORTED_MSG: &str = "当前系统不支持自动跟随,请改用手动模式";

/// 启动预授权:通过 /authorize 先取得对方同意(不开麦),再进入待命——
/// 确认弹窗不该拖到用户开口说话那一刻才在对面弹出来。
/// 返回 false = 不进入监视(被拒绝终态,或用户停止);网络失败自动重试。
/// 旧版服务端没有 /authorize:退回「用到时再确认」的老流程,照常返回 true
fn preauth_gate(addr: &str, stop: &AtomicBool, shared: &Shared) -> bool {
    shared
        .phase
        .store(Phase::Authorizing as u8, Ordering::Relaxed);
    while !stop.load(Ordering::SeqCst) {
        match client::preauthorize(addr, stop) {
            Ok(client::Preauth::Granted) | Ok(client::Preauth::Unsupported) => {
                *shared.error.lock().unwrap() = None;
                return true;
            }
            // 拒绝是人的决定:终态,别再自动重试骚扰对方
            Ok(client::Preauth::Denied(msg)) => {
                *shared.error.lock().unwrap() = Some(msg);
                shared
                    .phase
                    .store(Phase::AuthDenied as u8, Ordering::Relaxed);
                return false;
            }
            Err(e) => {
                // 用户停止跟随导致的中断不是错误,别留下误导性的重试提示
                if stop.load(Ordering::SeqCst) {
                    return false;
                }
                *shared.error.lock().unwrap() = Some(format!("请求授权失败,自动重试中: {e}"));
                sleep_check(stop, CLAIM_RETRY_MS);
            }
        }
    }
    false
}

fn follow_thread(addr: String, device_name: String, stop: Arc<AtomicBool>, shared: Arc<Shared>) {
    if !preauth_gate(&addr, &stop, &shared) {
        return;
    }
    let mut gate = FollowGate::new(RELEASE_HANGOVER_MS);
    let start = Instant::now();
    let mut target: Option<micuse::MonitorTarget> = None;

    while !stop.load(Ordering::SeqCst) {
        // 找监视目标(支持热插拔/晚装虚拟声卡)
        if target.is_none() {
            target = micuse::find_monitor_target(&device_name);
            if target.is_none() {
                shared
                    .phase
                    .store(Phase::DeviceMissing as u8, Ordering::Relaxed);
                sleep_check(&stop, 1000);
                continue;
            }
        }

        let dev = target.as_ref().unwrap();
        let device_busy = micuse::device_running_somewhere(dev);
        let other_capturing = match micuse::other_process_capturing(dev) {
            MicUse::InUse => true,
            MicUse::Idle => false,
            MicUse::Unsupported => {
                shared
                    .phase
                    .store(Phase::Unsupported as u8, Ordering::Relaxed);
                *shared.error.lock().unwrap() = Some(UNSUPPORTED_MSG.into());
                return;
            }
        };
        // 展示口径:使用中看是否仍有应用在采集;其余阶段两信号同时成立才算"在用"
        // (设备被桥接/路由长期占用时,单看 device_busy 会常亮误导)
        let in_use_display = if gate.phase == Phase::Active {
            other_capturing
        } else {
            device_busy && other_capturing
        };
        shared.local_in_use.store(in_use_display, Ordering::Relaxed);

        // 内层会话被服务端结束(被接管/服务停止)→ 进入抑制态
        {
            let mut guard = shared.client.lock().unwrap();
            if let Some(h) = guard.as_ref() {
                if !h.is_connected() {
                    *shared.error.lock().unwrap() = h.take_error();
                    let preempted = h.preempted();
                    guard.take();
                    gate.on_session_ended(preempted);
                }
            }
        }

        let now_ms = start.elapsed().as_millis() as u64;
        match gate.step(device_busy, other_capturing, now_ms) {
            FollowAction::Claim => match client::connect(addr.clone(), Some(device_name.clone())) {
                Ok(h) => {
                    *shared.client.lock().unwrap() = Some(h);
                    *shared.error.lock().unwrap() = None;
                }
                Err(e) => {
                    *shared.error.lock().unwrap() = Some(format!("认领远端麦克风失败: {e}"));
                    gate.on_claim_failed();
                    sleep_check(&stop, CLAIM_RETRY_MS);
                }
            },
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
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};

    fn test_shared() -> Arc<Shared> {
        Arc::new(Shared {
            phase: AtomicU8::new(Phase::Armed as u8),
            local_in_use: AtomicBool::new(false),
            client: Mutex::new(None),
            error: Mutex::new(None),
        })
    }

    fn read_req(stream: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        String::from_utf8_lossy(&head).to_string()
    }

    fn write_resp(stream: &mut TcpStream, status: &str, extra: &str, body: &str) {
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
    }

    /// 假服务端:/health 正常应答;/authorize 按给定状态码回应
    fn spawn_auth_server(authorize_status: u16) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let device_id = format!("follow-test-{}", addr.port());
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { continue };
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let head = read_req(&mut s);
                if head.starts_with("GET /health") {
                    let body = format!(
                        r#"{{"status":"ok","app":"micsync","streaming":false,"client":"","sample_rate":0,"alias":"测试端","device_type":"desktop","device_id":"{device_id}"}}"#
                    );
                    write_resp(&mut s, "200 OK", "", &body);
                } else if head.starts_with("GET /authorize") {
                    match authorize_status {
                        200 => write_resp(&mut s, "200 OK", "", r#"{"status":"authorized_once"}"#),
                        403 => write_resp(
                            &mut s,
                            "403 Forbidden",
                            "",
                            r#"{"error":"denied","message":"对方拒绝了麦克风授权请求"}"#,
                        ),
                        _ => write_resp(&mut s, "404 Not Found", "", r#"{"error":"not_found"}"#),
                    }
                }
            }
        });
        addr
    }

    /// 对方同意(或本就有授权)→ 进入监视待命
    #[test]
    fn preauth_gate_proceeds_when_granted() {
        let addr = spawn_auth_server(200);
        let shared = test_shared();
        let stop = AtomicBool::new(false);
        assert!(preauth_gate(&addr.to_string(), &stop, &shared));
        assert!(shared.error.lock().unwrap().is_none());
    }

    /// 对方拒绝 → 终态:不进入监视,留下拒绝原因
    #[test]
    fn preauth_gate_denied_is_terminal() {
        let addr = spawn_auth_server(403);
        let shared = test_shared();
        let stop = AtomicBool::new(false);
        assert!(!preauth_gate(&addr.to_string(), &stop, &shared));
        assert_eq!(
            Phase::from_u8(shared.phase.load(Ordering::Relaxed)),
            Phase::AuthDenied
        );
        let err = shared.error.lock().unwrap().clone().unwrap_or_default();
        assert!(err.contains("拒绝"), "应把拒绝原因带给用户: {err}");
    }

    /// 旧版服务端没有 /authorize(404)→ 照常进入监视,退回用时确认
    #[test]
    fn preauth_gate_falls_back_on_old_server() {
        let addr = spawn_auth_server(404);
        let shared = test_shared();
        let stop = AtomicBool::new(false);
        assert!(preauth_gate(&addr.to_string(), &stop, &shared));
    }

    #[test]
    fn gate_claims_on_local_use_and_releases_after_hangover() {
        let mut g = FollowGate::new(2000);
        // 待命:设备空闲不动作
        assert_eq!(g.step(false, false, 0), FollowAction::None);
        // 本机应用开始采集虚拟声卡(设备在跑 + 有进程采集)→ 认领
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
        // 持续停用超过挂起时长 → 释放,进入排空态
        assert_eq!(g.step(true, false, 5100), FollowAction::Release);
        assert_eq!(g.phase, Phase::Draining);
        // 采集已停止 → 回到待命;自己播放的余量(device_busy)不再阻塞
        assert_eq!(g.step(true, false, 5400), FollowAction::None);
        assert_eq!(g.phase, Phase::Armed);
        // 待命期播放余量只有 IO 没有采集 → 不误认领
        assert_eq!(g.step(true, false, 5700), FollowAction::None);
        // 再次使用 → 再次认领
        assert_eq!(g.step(true, true, 6000), FollowAction::Claim);
    }

    /// 设备被第三方长期占用(模拟器麦克风桥接/系统音频路由)但无人采集:
    /// 不误认领、不卡排空——"重启后自动跟随失灵"的根因回归测试
    #[test]
    fn gate_ignores_persistently_busy_device_without_capture() {
        let mut g = FollowGate::new(2000);
        // 设备一直忙但没有进程在采集 → 一直待命,不认领
        assert_eq!(g.step(true, false, 0), FollowAction::None);
        assert_eq!(g.step(true, false, 10_000), FollowAction::None);
        assert_eq!(g.phase, Phase::Armed);
        // 真正的采集出现 → 认领
        assert_eq!(g.step(true, true, 11_000), FollowAction::Claim);
        // 采集结束 → 释放;设备仍被占用也能回到待命
        assert_eq!(g.step(true, false, 12_000), FollowAction::None);
        assert_eq!(g.step(true, false, 14_100), FollowAction::Release);
        assert_eq!(g.step(true, false, 14_400), FollowAction::None);
        assert_eq!(g.phase, Phase::Armed);
    }

    #[test]
    fn gate_suppressed_after_preemption_until_local_idle() {
        let mut g = FollowGate::new(2000);
        assert_eq!(g.step(true, true, 0), FollowAction::Claim);
        // 被其他设备接管 → 抑制:本机应用仍在采集也不再认领(不拉锯)
        g.on_session_ended(true);
        assert_eq!(g.phase, Phase::Suppressed);
        assert_eq!(g.step(true, true, 500), FollowAction::None);
        assert_eq!(g.step(true, true, 5000), FollowAction::None);
        // 本机应用停止采集 → 本轮结束,重新武装(设备被他人占着也不阻塞)
        assert_eq!(g.step(true, false, 6000), FollowAction::None);
        assert_eq!(g.phase, Phase::Armed);
        // 新一轮本机使用 → 恢复自动认领
        assert_eq!(g.step(true, true, 7000), FollowAction::Claim);
    }

    #[test]
    fn gate_drains_when_server_stops_without_preemption() {
        let mut g = FollowGate::new(2000);
        assert_eq!(g.step(true, true, 0), FollowAction::Claim);
        // 服务端主动停止(非被接管)→ 排空,而不是"被接管"抑制
        g.on_session_ended(false);
        assert_eq!(g.phase, Phase::Draining);
        // 本机应用还在采集 → 等本轮结束
        assert_eq!(g.step(true, true, 500), FollowAction::None);
        assert_eq!(g.phase, Phase::Draining);
        // 采集停止 → 重新武装
        assert_eq!(g.step(true, false, 3000), FollowAction::None);
        assert_eq!(g.phase, Phase::Armed);
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
