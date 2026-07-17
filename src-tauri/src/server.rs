use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};

use crate::audio;
use crate::settings;

pub const MAGIC: &[u8; 4] = b"MSY1";

/// 优雅结束帧(0 长度帧)之后的原因码
pub const END_SERVER_CLOSING: u32 = 0;
pub const END_PREEMPTED: u32 = 1;

/// 等待用户确认授权的时限。太短会让不在电脑前的人错过请求,
/// 太长会把对方的客户端吊死在那里——30 秒够按一下弹窗了
const CONSENT_TIMEOUT: Duration = Duration::from_secs(30);

/// 预授权(配对)请求不设裁决时限——不占麦克风,挂到用户处理为止无害。
/// 但对方一旦不在了(关掉跟随/退出应用/休眠)就该自动撤销,否则弹窗和
/// Busy 会永远挂着。对方等待期间每 ~2 秒发一个心跳字节,这么久没心跳视为放弃
const AUTHORIZE_ABANDON: Duration = Duration::from_secs(15);

/// 采集启动器:为一次串流会话按需启动 mic 采集(采集流驻留在自己的线程里,
/// 停止信号置位后释放麦克风),返回 (实际设备名, 采样率)。
/// 抽象成函数指针以便测试注入假采集,不依赖真实声卡。
type CaptureFn = fn(
    Option<String>,            // 设备偏好
    Arc<AtomicBool>,           // 本会话采集停止信号
    SyncSender<Arc<Vec<i16>>>, // 帧输出
    Arc<audio::LevelMeter>,    // 电平/波形上报
) -> Result<(String, u32), String>;

/// 当前串流会话;同一时间最多一个,新请求会接管(抢占)旧会话
struct Session {
    id: u64,
    addr: SocketAddr,
    /// 置位 = 被新会话抢占,旧 writer 应发结束帧并收尾
    end: Arc<AtomicBool>,
}

/// 用户对一次授权请求的裁决
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    /// 只允许这一次,下次再问
    Once,
    /// 记住这台设备:签发令牌,以后直接放行
    Always,
    Deny,
}

/// 授权请求的种类:决定确认弹窗的文案——同意后立即开麦,
/// 还是自动跟随的预授权(先同意,之后用到时才开麦)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReqKind {
    Stream,
    Authorize,
}

impl ReqKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReqKind::Stream => "stream",
            ReqKind::Authorize => "authorize",
        }
    }
}

/// 一个正在等用户确认的授权请求
struct Pending {
    id: u64,
    /// 对方自报的设备名(不可信输入,已过滤控制字符并限长)
    name: String,
    addr: SocketAddr,
    kind: ReqKind,
    decided: SyncSender<Decision>,
}

/// 预授权时用户选「允许一次」留下的一次性放行券:同一设备(IP+名字)的
/// 下一次 /stream 直接放行并作废,再往后仍要重新确认——「一次」就是一轮使用
struct OnceGrant {
    ip: std::net::IpAddr,
    name: String,
}

struct Shared {
    /// UI 选择的麦克风设备,下一次采集生效
    device_pref: Mutex<Option<String>>,
    session: Mutex<Option<Session>>,
    /// 等待用户确认的授权请求;同一时间最多一个
    pending: Mutex<Option<Pending>>,
    /// 预授权「允许一次」的一次性放行券;同一时间最多一张
    once_grant: Mutex<Option<OnceGrant>>,
    next_id: AtomicU64,
    meter: Arc<audio::LevelMeter>,
    /// 最近一次采集的实际设备名与采样率(状态展示)
    last_device: Mutex<String>,
    last_rate: AtomicU32,
    error: Mutex<Option<String>>,
    capture: CaptureFn,
}

pub struct ServerHandle {
    pub port: u16,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl ServerHandle {
    /// 更换麦克风设备偏好,对下一次串流会话生效
    pub fn set_device(&self, name: Option<String>) {
        *self.shared.device_pref.lock().unwrap() = name;
    }

    /// 展示用设备名:优先 UI 偏好,其次最近一次实际采集的设备
    pub fn device(&self) -> String {
        if let Some(pref) = self.shared.device_pref.lock().unwrap().clone() {
            return pref;
        }
        let last = self.shared.last_device.lock().unwrap().clone();
        if last.is_empty() {
            "系统默认".into()
        } else {
            last
        }
    }

    /// 最近一次采集的采样率;0 = 还没有客户端用过
    pub fn sample_rate(&self) -> u32 {
        self.shared.last_rate.load(Ordering::Relaxed)
    }

    /// 当前收流客户端地址;None = 麦克风空闲
    pub fn stream_addr(&self) -> Option<String> {
        self.shared
            .session
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.addr.to_string())
    }

    pub fn level(&self) -> f32 {
        self.shared.meter.level()
    }

    /// 波形快照:(最近 ~1.5 秒每块音频的峰值序列, 累计块计数),移动端波形 UI 用
    pub fn wave(&self) -> (Vec<f32>, u64) {
        self.shared.meter.wave_snapshot()
    }

    pub fn take_error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    /// 当前等待用户确认的授权请求 (设备名, 来源地址, 种类);None = 没有待确认的
    pub fn pending(&self) -> Option<(String, String, ReqKind)> {
        self.shared
            .pending
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| (p.name.clone(), p.addr.to_string(), p.kind))
    }

    /// 用户裁决当前的授权请求;没有待确认请求时静默忽略(UI 可能慢一拍)
    pub fn decide(&self, decision: Decision) {
        if let Some(p) = self.shared.pending.lock().unwrap().take() {
            let _ = p.decided.try_send(decision);
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // 让在场会话尽快发结束帧收尾
        if let Some(s) = self.shared.session.lock().unwrap().as_ref() {
            s.end.store(true, Ordering::SeqCst);
        }
        // 服务停了就别再吊着等确认的请求
        if let Some(p) = self.shared.pending.lock().unwrap().take() {
            let _ = p.decided.try_send(Decision::Deny);
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动 API 监听。此时不碰麦克风——只有客户端请求 /stream 才按需开启采集
pub fn start(device_name: Option<String>, port: u16) -> Result<ServerHandle, String> {
    start_with(device_name, port, real_capture)
}

fn start_with(
    device_name: Option<String>,
    port: u16,
    capture: CaptureFn,
) -> Result<ServerHandle, String> {
    // 上一次监听刚停止时端口可能还没释放(线程 ~50ms 内退出),小步重试
    let listener = {
        let mut attempt = 0;
        loop {
            match TcpListener::bind(("0.0.0.0", port)) {
                Ok(l) => break l,
                Err(_) if attempt < 10 => {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("端口 {port} 绑定失败: {e}")),
            }
        }
    };
    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("设置监听器失败: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        device_pref: Mutex::new(device_name),
        session: Mutex::new(None),
        pending: Mutex::new(None),
        once_grant: Mutex::new(None),
        next_id: AtomicU64::new(1),
        meter: Arc::new(audio::LevelMeter::new()),
        last_device: Mutex::new(String::new()),
        last_rate: AtomicU32::new(0),
        error: Mutex::new(None),
        capture,
    });

    {
        let stop = stop.clone();
        let shared = shared.clone();
        thread::Builder::new()
            .name("mic-http".into())
            .spawn(move || accept_thread(listener, stop, shared))
            .map_err(|e| format!("创建监听线程失败: {e}"))?;
    }

    Ok(ServerHandle {
        port: actual_port,
        stop,
        shared,
    })
}

fn accept_thread(listener: TcpListener, stop: Arc<AtomicBool>, shared: Arc<Shared>) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, addr)) => {
                let stop = stop.clone();
                let shared = shared.clone();
                let _ = thread::Builder::new()
                    .name(format!("mic-http-{addr}"))
                    .spawn(move || handle_conn(stream, addr, stop, shared));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(300));
            }
        }
    }
}

fn handle_conn(
    mut stream: TcpStream,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));

    let head = match read_head(&mut stream) {
        Some(h) => h,
        None => return,
    };
    let path = match parse_request_path(&head) {
        Some(p) => p,
        None => {
            let _ = write_http(
                &mut stream,
                400,
                "Bad Request",
                r#"{"error":"bad_request"}"#,
            );
            graceful_close(stream);
            return;
        }
    };

    match path.as_str() {
        "/health" => {
            let (streaming, client) = {
                let guard = shared.session.lock().unwrap();
                (
                    guard.is_some(),
                    guard
                        .as_ref()
                        .map(|s| s.addr.to_string())
                        .unwrap_or_default(),
                )
            };
            // alias/device_type/fingerprint 供客户端子网扫描发现用:
            // /health 本身就是发现签名,fingerprint 让扫描方认出「这是我自己」
            let body = serde_json::json!({
                "status": "ok",
                "app": "micsync",
                "streaming": streaming,
                "client": client,
                "sample_rate": shared.last_rate.load(Ordering::Relaxed),
                "alias": crate::discovery::alias(),
                "device_type": crate::discovery::device_type(),
                "device_id": crate::discovery::device_id(),
            })
            .to_string();
            let _ = write_http(&mut stream, 200, "OK", &body);
            graceful_close(stream);
        }
        "/stream" => handle_stream(stream, addr, &head, stop, shared),
        "/authorize" => handle_authorize(stream, addr, &head, stop, shared),
        _ => {
            let _ = write_http(&mut stream, 404, "Not Found", r#"{"error":"not_found"}"#);
            graceful_close(stream);
        }
    }
}

/// 取请求头的值(HTTP 头名大小写不敏感);缺失返回空串
fn header_value(head: &str, name: &str) -> String {
    let want = name.to_ascii_lowercase();
    head.lines()
        .skip(1) // 跳过请求行
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k.trim().to_ascii_lowercase() == want).then(|| v.trim().to_string())
        })
        .unwrap_or_default()
}

/// 授权闸门结果
enum Consent {
    /// 放行;Some(token) = 用户选了「始终允许」,需把新令牌交给对方
    Allowed(Option<String>),
    Denied,
    Timeout,
    /// 另一个请求正在等确认——不排队,让对方稍后重试
    Busy,
}

/// 授权闸门:已信任的客户端直接放行,否则挂起请求交给用户裁决。
/// 这一步必须在认领会话与开麦之前——没同意之前不能碰麦克风。
/// probe:预授权请求的连接。/stream 的确认吊着即将串流的连接,30 秒不决作罢;
/// 预授权不设时限,靠在 probe 上收心跳字节判断对方还在不在等——
/// 断开(EOF)或心跳断绝都视为放弃,当场撤销请求
fn await_consent(
    shared: &Arc<Shared>,
    name: &str,
    addr: SocketAddr,
    kind: ReqKind,
    stop: &AtomicBool,
    mut probe: Option<&mut TcpStream>,
) -> Consent {
    let (tx, rx) = sync_channel::<Decision>(1);
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    {
        let mut g = shared.pending.lock().unwrap();
        if g.is_some() {
            return Consent::Busy;
        }
        *g = Some(Pending {
            id,
            name: name.to_string(),
            addr,
            kind,
            decided: tx,
        });
    }

    let deadline = match kind {
        ReqKind::Stream => Some(std::time::Instant::now() + CONSENT_TIMEOUT),
        ReqKind::Authorize => None,
    };
    if let Some(s) = probe.as_deref_mut() {
        let _ = s.set_read_timeout(Some(Duration::from_millis(50)));
    }
    let mut last_heartbeat = std::time::Instant::now();

    // 分段等待:既能到点超时/察觉对方放弃,也能在服务停止时立刻收手
    let outcome = loop {
        if stop.load(Ordering::SeqCst) {
            break None;
        }
        let wait = match deadline {
            Some(d) => {
                let left = d.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    break None;
                }
                left.min(Duration::from_millis(200))
            }
            None => Duration::from_millis(200),
        };
        if let Some(s) = probe.as_deref_mut() {
            let mut buf = [0u8; 16];
            match s.read(&mut buf) {
                Ok(0) => break None, // 对方断开:放弃
                Ok(_) => last_heartbeat = std::time::Instant::now(),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break None,
            }
            if last_heartbeat.elapsed() >= AUTHORIZE_ABANDON {
                break None; // 心跳断绝(对方休眠/网络消失):放弃
            }
        }
        match rx.recv_timeout(wait) {
            Ok(d) => break Some(d),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            // 发送端被丢弃(如 stop 里 take 走了):当作拒绝
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
        }
    };

    // 无论结果如何都要摘掉自己的 pending,否则后续请求会一直撞 Busy。
    // 认 id:decide() 可能已经取走了,别误删别人的
    {
        let mut g = shared.pending.lock().unwrap();
        if g.as_ref().map_or(false, |p| p.id == id) {
            *g = None;
        }
    }

    match outcome {
        Some(Decision::Once) => Consent::Allowed(None),
        Some(Decision::Always) => Consent::Allowed(Some(settings::trust_client(name))),
        Some(Decision::Deny) => Consent::Denied,
        None => Consent::Timeout,
    }
}

/// 预授权(配对):只走同意闸门,不开麦。自动跟随在武装时调用,把「要不要让
/// 对方用我的麦克风」的决定从会议开始的瞬间挪到设置跟随的时刻。
/// 请求挂到用户处理为止(无裁决时限);等待期间对方每 ~2 秒发一个心跳字节,
/// 断开或心跳断绝视为放弃,自动撤销。响应:
/// 200 已授权(「始终允许」附带 X-MicSync-Token;「允许一次」发一张一次性放行券) /
/// 403 拒绝(终态,对方不得自动重试) / 409 另一请求正在等确认(对方稍后重试)
fn handle_authorize(
    mut stream: TcpStream,
    addr: SocketAddr,
    head: &str,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
) {
    let token = header_value(head, "x-micsync-token");
    let name = settings::sanitize_name(&header_value(head, "x-micsync-name"));
    let name = if name.is_empty() {
        format!("未命名设备({})", addr.ip())
    } else {
        name
    };

    if settings::trusted_by_token(&token, &name) {
        let _ = write_authorized(&mut stream, None);
        graceful_close(stream);
        return;
    }

    match await_consent(
        &shared,
        &name,
        addr,
        ReqKind::Authorize,
        &stop,
        Some(&mut stream),
    ) {
        Consent::Allowed(issued) => {
            if issued.is_none() {
                // 「允许一次」:发一张一次性放行券,对方下一轮 /stream 免确认;
                // 那之后再用仍会重新弹确认——「一次」就是一轮使用
                *shared.once_grant.lock().unwrap() = Some(OnceGrant {
                    ip: addr.ip(),
                    name: name.clone(),
                });
            }
            let _ = write_authorized(&mut stream, issued.as_deref());
        }
        Consent::Denied => {
            let _ = write_http(
                &mut stream,
                403,
                "Forbidden",
                r#"{"error":"denied","message":"对方拒绝了自动跟随的授权请求"}"#,
            );
        }
        // 预授权没有时限,走到这里只可能是对方已放弃或服务停止——没有可靠的
        // 收件人,不用回什么了
        Consent::Timeout => {}
        Consent::Busy => {
            let _ = write_http(
                &mut stream,
                409,
                "Conflict",
                r#"{"error":"busy","message":"另一台设备的请求正在等待确认,请稍后重试"}"#,
            );
        }
    }
    graceful_close(stream);
}

/// 消费「允许一次」的放行券:同一设备(IP+名字)的下一次 /stream 直接放行并作废
fn consume_once_grant(shared: &Shared, addr: &SocketAddr, name: &str) -> bool {
    let mut g = shared.once_grant.lock().unwrap();
    if g.as_ref()
        .map_or(false, |o| o.ip == addr.ip() && o.name == name)
    {
        *g = None;
        return true;
    }
    false
}

fn handle_stream(
    mut stream: TcpStream,
    addr: SocketAddr,
    head: &str,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
) {
    // 授权闸门:先确认对方有权使用本机麦克风,再谈开麦。
    // 令牌是服务端签发的私密凭据(不出现在 /health),名字只是展示用的不可信输入
    let token = header_value(head, "x-micsync-token");
    let name = settings::sanitize_name(&header_value(head, "x-micsync-name"));
    let name = if name.is_empty() {
        format!("未命名设备({})", addr.ip())
    } else {
        name
    };

    let mut issued_token = None;
    if !settings::trusted_by_token(&token, &name) && !consume_once_grant(&shared, &addr, &name) {
        match await_consent(&shared, &name, addr, ReqKind::Stream, &stop, None) {
            Consent::Allowed(t) => issued_token = t,
            Consent::Denied => {
                let _ = write_http(
                    &mut stream,
                    403,
                    "Forbidden",
                    r#"{"error":"denied","message":"对方拒绝了本次麦克风使用请求"}"#,
                );
                graceful_close(stream);
                return;
            }
            Consent::Timeout => {
                let _ = write_http(
                    &mut stream,
                    403,
                    "Forbidden",
                    r#"{"error":"timeout","message":"对方未在时限内确认,请求已取消"}"#,
                );
                graceful_close(stream);
                return;
            }
            Consent::Busy => {
                let _ = write_http(
                    &mut stream,
                    409,
                    "Conflict",
                    r#"{"error":"busy","message":"另一台设备的请求正在等待确认,请稍后重试"}"#,
                );
                graceful_close(stream);
                return;
            }
        }
    }

    // 认领会话(抢占式):新请求接管旧串流——同一个人换设备,最新请求在哪人就在哪
    let (my_id, my_end) = {
        let mut guard = shared.session.lock().unwrap();
        if let Some(old) = guard.as_ref() {
            old.end.store(true, Ordering::SeqCst);
        }
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let end = Arc::new(AtomicBool::new(false));
        *guard = Some(Session {
            id,
            addr,
            end: end.clone(),
        });
        (id, end)
    };

    // 按需开启 mic 采集,本会话独占;会话结束即释放麦克风
    let cap_stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = sync_channel::<Arc<Vec<i16>>>(64);
    let pref = shared.device_pref.lock().unwrap().clone();
    let init = (shared.capture)(pref, cap_stop.clone(), tx, shared.meter.clone());

    let (device, rate) = match init {
        Ok(v) => v,
        Err(e) => {
            cap_stop.store(true, Ordering::SeqCst);
            *shared.error.lock().unwrap() = Some(e.clone());
            let body = serde_json::json!({"error": "mic_failed", "message": e}).to_string();
            let _ = write_http(&mut stream, 503, "Service Unavailable", &body);
            release_session(&shared, my_id);
            graceful_close(stream);
            return;
        }
    };
    *shared.last_device.lock().unwrap() = device;
    shared.last_rate.store(rate, Ordering::Relaxed);
    *shared.error.lock().unwrap() = None;

    let handshake = (|| -> std::io::Result<()> {
        // 用户选了「始终允许」时,把签发的令牌随响应头交给对方;
        // 对方存下来,以后凭它直接放行,不用再打扰用户
        let token_header = match &issued_token {
            Some(t) => format!("X-MicSync-Token: {t}\r\n"),
            None => String::new(),
        };
        stream.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nCache-Control: no-store\r\n{token_header}Connection: close\r\n\r\n").as_bytes(),
        )?;
        // 二进制流头: MAGIC(4) + sample_rate u32 LE + channels u16 LE + reserved u16
        let mut header = Vec::with_capacity(12);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&rate.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        stream.write_all(&header)
    })();

    if handshake.is_ok() {
        write_frames(&mut stream, rx, &stop, &my_end);
    }

    // 会话收尾:关麦克风、清电平/波形、释放会话(可能已被新会话顶替)
    cap_stop.store(true, Ordering::SeqCst);
    shared.meter.reset();
    release_session(&shared, my_id);
    // 优雅关闭,确保结束帧(原因码)送达对端后再断
    graceful_close(stream);
}

fn release_session(shared: &Shared, id: u64) {
    let mut guard = shared.session.lock().unwrap();
    if guard.as_ref().map_or(false, |s| s.id == id) {
        *guard = None;
    }
}

/// 帧写循环。退出场景:服务停止/被抢占(发 0 长度结束帧告知原因)、
/// 客户端断开(写失败)、采集中断(Disconnected,直接断连让客户端重试)
fn write_frames(
    stream: &mut TcpStream,
    rx: Receiver<Arc<Vec<i16>>>,
    stop: &AtomicBool,
    end: &AtomicBool,
) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            send_end(stream, END_SERVER_CLOSING);
            return;
        }
        if end.load(Ordering::SeqCst) {
            send_end(stream, END_PREEMPTED);
            return;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                // 帧: 样本数 u32 LE + i16 LE 样本
                buf.clear();
                buf.extend_from_slice(&(frame.len() as u32).to_le_bytes());
                for s in frame.iter() {
                    buf.extend_from_slice(&s.to_le_bytes());
                }
                if stream.write_all(&buf).is_err() {
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// 优雅结束帧:0 长度 + 原因码,客户端据此决定是否自动重连
fn send_end(stream: &mut TcpStream, reason: u32) {
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&reason.to_le_bytes());
    let _ = stream.write_all(&buf);
}

/// 优雅关闭:先 FIN 再排空读到对端 EOF。直接 drop 可能触发 RST,
/// 让对端丢掉还没读完的响应(如 503 正文、被接管的结束帧原因码)
fn graceful_close(mut stream: TcpStream) {
    use std::net::Shutdown;
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let mut buf = [0u8; 256];
    for _ in 0..64 {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// 真实采集:spawn 驻留线程持有 cpal 输入流,握手返回 (设备名, 采样率)
fn real_capture(
    device_pref: Option<String>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<Arc<Vec<i16>>>,
    meter: Arc<audio::LevelMeter>,
) -> Result<(String, u32), String> {
    let (init_tx, init_rx) = sync_channel::<Result<(String, u32), String>>(1);
    thread::Builder::new()
        .name("mic-capture".into())
        .spawn(move || capture_thread(device_pref, stop, tx, meter, init_tx))
        .map_err(|e| format!("创建采集线程失败: {e}"))?;
    init_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "麦克风初始化超时".to_string())?
}

fn capture_thread(
    device_name: Option<String>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<Arc<Vec<i16>>>,
    meter: Arc<audio::LevelMeter>,
    init_tx: SyncSender<Result<(String, u32), String>>,
) {
    let device = match audio::find_input_device(device_name.as_deref()) {
        Some(d) => d,
        None => {
            let _ = init_tx.send(Err("找不到可用的输入设备(麦克风)".into()));
            return;
        }
    };
    let name = device.name().unwrap_or_else(|_| "未知设备".into());
    let config = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("读取麦克风配置失败: {e}")));
            return;
        }
    };
    let sample_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();

    let on_mono = {
        move |mono: Vec<f32>| {
            meter.push(audio::peak_level(&mono));
            // 队列满(客户端太慢)或会话已收尾都直接丢帧,由停止信号结束本线程
            let _ = tx.try_send(Arc::new(audio::f32_to_i16(&mono)));
        }
    };

    let err_fn = move |e: cpal::StreamError| {
        eprintln!("麦克风流错误: {e}");
    };

    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            {
                let on_mono = on_mono.clone();
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    on_mono(audio::interleaved_to_mono_f32(data, channels));
                }
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            {
                let on_mono = on_mono.clone();
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let f: Vec<f32> = audio::i16_to_f32(data);
                    on_mono(audio::interleaved_to_mono_f32(&f, channels));
                }
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            {
                let on_mono = on_mono.clone();
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let f: Vec<f32> = data
                        .iter()
                        .map(|&s| (s as f32 - 32768.0) / 32768.0)
                        .collect();
                    on_mono(audio::interleaved_to_mono_f32(&f, channels));
                }
            },
            err_fn,
            None,
        ),
        other => {
            let _ = init_tx.send(Err(format!("不支持的采样格式: {other:?}")));
            return;
        }
    };

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(format!("创建麦克风采集流失败: {e}")));
            return;
        }
    };
    if let Err(e) = stream.play() {
        let _ = init_tx.send(Err(format!("启动麦克风采集失败: {e}")));
        return;
    }

    let _ = init_tx.send(Ok((name, sample_rate)));

    // 驻留直到会话结束;stream 随本线程退出而 drop,麦克风随之释放
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
}

/// 读 HTTP 请求头(到空行为止,上限 8KB);失败或超时返回 None
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if buf.len() >= 8192 {
            return None;
        }
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    String::from_utf8(buf).ok()
}

/// 从请求头解析 GET 路径(去掉 query);非 GET 返回 None
fn parse_request_path(head: &str) -> Option<String> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let path = parts.next()?;
    Some(path.split('?').next().unwrap_or(path).to_string())
}

/// 预授权通过的 200 响应;「始终允许」时把签发的令牌随响应头交给对方
fn write_authorized(stream: &mut TcpStream, token: Option<&str>) -> std::io::Result<()> {
    let body = r#"{"status":"ok"}"#;
    let token_header = token
        .map(|t| format!("X-MicSync-Token: {t}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{token_header}Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())
}

fn write_http(stream: &mut TcpStream, code: u16, reason: &str, body: &str) -> std::io::Result<()> {
    write_http_ex(stream, code, reason, "", body)
}

/// 同 write_http,但可附加额外响应头(如签发令牌);extra 每行须以 \r\n 结尾
fn write_http_ex(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    extra: &str,
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// 当前活跃的假采集线程数,用来断言「按需开麦、用完关麦」。
    /// 全局计数:凡断言它的测试必须持有 FAKE_CAPTURE_LOCK 串行运行,避免互相污染
    static ACTIVE_CAPTURES: AtomicUsize = AtomicUsize::new(0);
    static FAKE_CAPTURE_LOCK: Mutex<()> = Mutex::new(());

    /// 假采集:44100Hz 正弦波,10ms 一帧,直到停止信号
    fn fake_capture(
        _pref: Option<String>,
        stop: Arc<AtomicBool>,
        tx: SyncSender<Arc<Vec<i16>>>,
        meter: Arc<audio::LevelMeter>,
    ) -> Result<(String, u32), String> {
        ACTIVE_CAPTURES.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            let mut phase = 0.0f32;
            while !stop.load(Ordering::SeqCst) {
                let frame: Vec<i16> = (0..441)
                    .map(|_| {
                        let s = (phase * std::f32::consts::TAU).sin() * 0.5;
                        phase = (phase + 440.0 / 44100.0).fract();
                        (s * i16::MAX as f32) as i16
                    })
                    .collect();
                meter.push(0.5);
                let _ = tx.try_send(Arc::new(frame));
                thread::sleep(Duration::from_millis(10));
            }
            ACTIVE_CAPTURES.fetch_sub(1, Ordering::SeqCst);
        });
        Ok(("假麦克风".into(), 44100))
    }

    fn failing_capture(
        _pref: Option<String>,
        _stop: Arc<AtomicBool>,
        _tx: SyncSender<Arc<Vec<i16>>>,
        _meter: Arc<audio::LevelMeter>,
    ) -> Result<(String, u32), String> {
        Err("假装麦克风坏了".into())
    }

    fn http_get_with(port: u16, path: &str, extra: &str) -> TcpStream {
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        // 放宽超时:整套并行跑(尤其机器上还有别的构建)时 CPU 争抢厉害,
        // 5s 实测会偶发不够——这里等的是本地回环,宁可给足也不要假失败
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: t\r\n{extra}Connection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("write req");
        s
    }

    fn http_get(port: u16, path: &str) -> TcpStream {
        http_get_with(port, path, "")
    }

    /// 带已授权令牌的请求:跳过确认闸门,用于测试串流本身的逻辑。
    /// 单测里 settings 全程在内存中(没有 CONFIG_PATH),不会污染真实配置
    fn http_get_trusted(port: u16, path: &str) -> TcpStream {
        let token = settings::trust_client("测试设备");
        http_get_with(port, path, &format!("X-MicSync-Token: {token}\r\n"))
    }

    fn read_test_head(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).unwrap_or_else(|e| {
                panic!(
                    "读响应头失败({e:?});已读到 {:?}",
                    String::from_utf8_lossy(&buf)
                )
            });
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(buf).expect("utf8")
    }

    /// 读一帧;返回 None 表示收到 0 长度结束帧(附带原因码)
    fn read_frame(stream: &mut TcpStream) -> Result<Vec<i16>, u32> {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("frame len");
        let n = u32::from_le_bytes(len_buf) as usize;
        if n == 0 {
            let mut reason = [0u8; 4];
            stream.read_exact(&mut reason).expect("end reason");
            return Err(u32::from_le_bytes(reason));
        }
        let mut body = vec![0u8; n * 2];
        stream.read_exact(&mut body).expect("frame body");
        Ok(body
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect())
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout_ms: u64) -> bool {
        let mut waited = 0;
        while waited < timeout_ms {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
            waited += 50;
        }
        cond()
    }

    #[test]
    fn parse_request_path_variants() {
        assert_eq!(
            parse_request_path("GET /health HTTP/1.1\r\nHost: x\r\n\r\n").as_deref(),
            Some("/health")
        );
        assert_eq!(
            parse_request_path("GET /stream?x=1 HTTP/1.1\r\n\r\n").as_deref(),
            Some("/stream")
        );
        assert!(parse_request_path("POST /health HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_request_path("").is_none());
    }

    /// 核心链路:空闲不开麦 → 请求即开麦串流 → 新请求抢占旧串流 → 断开即关麦
    #[test]
    fn mic_on_demand_and_preemption() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let port = handle.port;

        // 空闲:不开麦,health 报告空闲
        assert_eq!(
            ACTIVE_CAPTURES.load(Ordering::SeqCst),
            0,
            "mic must be off when idle"
        );
        let mut c = http_get(port, "/health");
        let mut resp = String::new();
        c.read_to_string(&mut resp).expect("health");
        assert!(resp.contains(r#""streaming":false"#), "{resp}");

        // 第一个客户端请求 → 自动开麦并串流
        let mut c1 = http_get_trusted(port, "/stream");
        let head1 = read_test_head(&mut c1);
        assert!(head1.starts_with("HTTP/1.1 200"), "{head1}");
        let mut magic = [0u8; 12];
        c1.read_exact(&mut magic).expect("MSY1 header");
        assert_eq!(&magic[0..4], MAGIC);
        assert_eq!(
            u32::from_le_bytes([magic[4], magic[5], magic[6], magic[7]]),
            44100
        );
        let frame = read_frame(&mut c1).expect("should receive audio");
        assert_eq!(frame.len(), 441);
        assert!(
            wait_until(|| ACTIVE_CAPTURES.load(Ordering::SeqCst) == 1, 1000),
            "capture should be running"
        );

        // health 报告使用中
        let mut c = http_get(port, "/health");
        let mut resp = String::new();
        c.read_to_string(&mut resp).expect("health");
        assert!(resp.contains(r#""streaming":true"#), "{resp}");

        // 第二个客户端请求 → 接管:c1 收到被抢占的结束帧,c2 正常收流
        let mut c2 = http_get_trusted(port, "/stream");
        let head2 = read_test_head(&mut c2);
        assert!(head2.starts_with("HTTP/1.1 200"), "{head2}");
        let mut magic2 = [0u8; 12];
        c2.read_exact(&mut magic2).expect("c2 MSY1 header");
        assert_eq!(&magic2[0..4], MAGIC);
        let reason = loop {
            match read_frame(&mut c1) {
                Ok(_) => continue, // 抢占前残留的音频帧
                Err(reason) => break reason,
            }
        };
        assert_eq!(reason, END_PREEMPTED, "c1 should be told it was preempted");
        read_frame(&mut c2).expect("c2 should receive audio");

        // c2 断开 → 会话释放、麦克风关闭
        drop(c2);
        assert!(
            wait_until(
                || {
                    let mut c = http_get(port, "/health");
                    let mut resp = String::new();
                    let _ = c.read_to_string(&mut resp);
                    resp.contains(r#""streaming":false"#)
                },
                3000
            ),
            "session should be released after client disconnects"
        );
        assert!(
            wait_until(|| ACTIVE_CAPTURES.load(Ordering::SeqCst) == 0, 3000),
            "mic must be released after last client leaves"
        );

        handle.stop();
    }

    /// 服务停止:在场客户端收到 END_SERVER_CLOSING 结束帧
    #[test]
    fn server_stop_notifies_client() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let mut c1 = http_get_trusted(handle.port, "/stream");
        let head = read_test_head(&mut c1);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let mut magic = [0u8; 12];
        c1.read_exact(&mut magic).expect("header");
        read_frame(&mut c1).expect("audio");

        handle.stop();
        let reason = loop {
            match read_frame(&mut c1) {
                Ok(_) => continue,
                Err(reason) => break reason,
            }
        };
        assert_eq!(reason, END_SERVER_CLOSING);
        assert!(
            wait_until(|| ACTIVE_CAPTURES.load(Ordering::SeqCst) == 0, 3000),
            "mic must be released after server stops"
        );
    }

    /// 麦克风打开失败 → 503 + 错误信息,会话释放
    #[test]
    fn mic_failure_returns_503() {
        let handle = start_with(None, 0, failing_capture).expect("start");
        let mut c1 = http_get_trusted(handle.port, "/stream");
        let head = read_test_head(&mut c1);
        assert!(head.starts_with("HTTP/1.1 503"), "{head}");
        let mut body = String::new();
        let _ = c1.read_to_string(&mut body);
        assert!(body.contains("mic_failed"), "{body}");

        // 会话已释放,health 回到空闲
        let mut c = http_get(handle.port, "/health");
        let mut resp = String::new();
        c.read_to_string(&mut resp).expect("health");
        assert!(resp.contains(r#""streaming":false"#), "{resp}");
        handle.stop();
    }
    // ---------- 授权闸门 ----------

    fn read_status(stream: &mut TcpStream) -> u16 {
        let head = read_test_head(stream);
        head.lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .expect("status line")
    }

    /// 等到出现待确认请求;超时返回 None
    fn wait_pending(h: &ServerHandle, ms: u64) -> Option<(String, String, ReqKind)> {
        let mut waited = 0;
        while waited < ms {
            if let Some(p) = h.pending() {
                return Some(p);
            }
            thread::sleep(Duration::from_millis(20));
            waited += 20;
        }
        h.pending()
    }

    /// 核心安全属性:没点同意之前,麦克风一下都不能开
    #[test]
    fn unauthorized_request_does_not_open_mic_until_approved() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let port = handle.port;

        let mut c = http_get_with(port, "/stream", "X-MicSync-Name: 张三的 Mac\r\n");
        let pending = wait_pending(&handle, 2000).expect("应出现待确认请求");
        assert_eq!(pending.0, "张三的 Mac", "弹窗要显示对方设备名");

        // 关键:挂起期间麦克风必须是关的
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            ACTIVE_CAPTURES.load(Ordering::SeqCst),
            0,
            "未经同意绝不能开麦"
        );
        assert!(handle.stream_addr().is_none(), "未经同意不能占用会话");

        handle.decide(Decision::Deny);
        assert_eq!(read_status(&mut c), 403, "拒绝应返回 403");
        assert_eq!(ACTIVE_CAPTURES.load(Ordering::SeqCst), 0, "拒绝后仍不开麦");
        handle.stop();
    }

    /// 「允许一次」放行本次,但不留信任——下次还得再问
    #[test]
    fn allow_once_streams_but_is_not_remembered() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let port = handle.port;

        let mut c = http_get_with(port, "/stream", "X-MicSync-Name: 一次性设备\r\n");
        wait_pending(&handle, 2000).expect("应出现待确认请求");
        handle.decide(Decision::Once);

        let head = read_test_head(&mut c);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            !head.to_ascii_lowercase().contains("x-micsync-token"),
            "「允许一次」不该签发令牌:{head}"
        );
        let mut magic = [0u8; 12];
        c.read_exact(&mut magic).expect("MSY1 header");
        assert_eq!(&magic[0..4], MAGIC);
        drop(c);

        // 同一台设备再来:因为没被记住,应再次进入待确认
        let _c2 = http_get_with(port, "/stream", "X-MicSync-Name: 一次性设备\r\n");
        assert!(
            wait_pending(&handle, 2000).is_some(),
            "「允许一次」之后应重新确认"
        );
        handle.stop();
    }

    /// 「始终允许」签发令牌;之后凭令牌直接放行,不再打扰用户
    #[test]
    fn allow_always_issues_token_that_skips_the_prompt() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let port = handle.port;

        let mut c = http_get_with(port, "/stream", "X-MicSync-Name: 我的 iPhone\r\n");
        wait_pending(&handle, 2000).expect("应出现待确认请求");
        handle.decide(Decision::Always);

        let head = read_test_head(&mut c);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let token = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("x-micsync-token")
                    .then(|| v.trim().to_string())
            })
            .expect("「始终允许」应随响应签发令牌");
        assert!(!token.is_empty());
        drop(c);

        // 凭令牌重来:不该再出现待确认,直接串流
        let mut c2 = http_get_with(port, "/stream", &format!("X-MicSync-Token: {token}\r\n"));
        let head2 = read_test_head(&mut c2);
        assert!(head2.starts_with("HTTP/1.1 200"), "{head2}");
        assert!(handle.pending().is_none(), "已授权设备不该再弹确认");

        // 撤销之后应恢复到需要确认
        settings::revoke_trusted(&token);
        drop(c2);
        let _c3 = http_get_with(port, "/stream", &format!("X-MicSync-Token: {token}\r\n"));
        assert!(
            wait_pending(&handle, 2000).is_some(),
            "撤销授权后应重新确认"
        );
        handle.stop();
    }

    /// 伪造/过期令牌不能蒙混过关,得走确认
    #[test]
    fn bogus_token_still_needs_approval() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let _c = http_get_with(
            handle.port,
            "/stream",
            "X-MicSync-Token: deadbeefdeadbeef\r\nX-MicSync-Name: 冒充者\r\n",
        );
        assert!(
            wait_pending(&handle, 2000).is_some(),
            "未知令牌必须走确认流程"
        );
        assert_eq!(ACTIVE_CAPTURES.load(Ordering::SeqCst), 0, "期间不开麦");
        handle.stop();
    }

    /// 已有请求等确认时,第二个请求得到 409 而不是排队把人吊死
    #[test]
    fn second_request_while_pending_gets_conflict() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let port = handle.port;

        let _c1 = http_get_with(port, "/stream", "X-MicSync-Name: 甲\r\n");
        wait_pending(&handle, 2000).expect("第一个请求应挂起");

        let mut c2 = http_get_with(port, "/stream", "X-MicSync-Name: 乙\r\n");
        assert_eq!(read_status(&mut c2), 409, "并发的第二个请求应得 409");
        handle.stop();
    }

    /// 设备名是对方给的不可信输入:换行会破坏 HTTP 头,超长会撑破弹窗
    #[test]
    fn hostile_device_name_is_sanitized() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        // 名字里塞控制字符与超长内容
        let _c = http_get_with(handle.port, "/stream", "X-MicSync-Name: 坏\u{7}设备\r\n");
        let (name, _, _) = wait_pending(&handle, 2000).expect("应出现待确认请求");
        assert!(!name.contains('\u{7}'), "控制字符应被滤掉: {name:?}");
        assert!(name.chars().count() <= 40, "名字应限长");
        handle.stop();
    }

    /// 没报名字的请求也要能显示来源,否则用户不知道在给谁授权
    #[test]
    fn nameless_request_falls_back_to_address() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let _c = http_get(handle.port, "/stream");
        let (name, addr, _) = wait_pending(&handle, 2000).expect("应出现待确认请求");
        assert!(name.contains("未命名设备"), "got {name}");
        assert!(addr.starts_with("127.0.0.1:"), "got {addr}");
        handle.stop();
    }

    // ---------- 预授权 /authorize ----------

    /// /authorize 走同一道确认闸门,但全程不开麦、不占会话——它只是把
    /// 确认弹窗提前,不是变相开麦
    #[test]
    fn authorize_prompts_without_touching_the_mic() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");

        let mut c = http_get_with(handle.port, "/authorize", "X-MicSync-Name: 跟随的 Mac\r\n");
        let (name, _, kind) = wait_pending(&handle, 2000).expect("应出现待确认请求");
        assert_eq!(name, "跟随的 Mac");
        assert_eq!(kind, ReqKind::Authorize, "弹窗要能区分这是预授权");

        // 等确认期间与拒绝之后,麦克风和会话都必须原封不动
        thread::sleep(Duration::from_millis(200));
        assert_eq!(ACTIVE_CAPTURES.load(Ordering::SeqCst), 0, "预授权不开麦");
        assert!(handle.stream_addr().is_none(), "预授权不占会话");

        handle.decide(Decision::Deny);
        assert_eq!(read_status(&mut c), 403, "拒绝应返回 403");
        assert_eq!(ACTIVE_CAPTURES.load(Ordering::SeqCst), 0);
        handle.stop();
    }

    /// 「始终允许」的预授权签发令牌,之后 /stream 凭令牌直接串流不再弹窗
    #[test]
    fn authorize_always_enables_silent_stream_later() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let port = handle.port;

        let mut c = http_get_with(port, "/authorize", "X-MicSync-Name: 跟随的 Mac\r\n");
        wait_pending(&handle, 2000).expect("应出现待确认请求");
        handle.decide(Decision::Always);
        let head = read_test_head(&mut c);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        let token = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("x-micsync-token")
                    .then(|| v.trim().to_string())
            })
            .expect("「始终允许」应随响应签发令牌");
        drop(c);

        let mut c2 = http_get_with(port, "/stream", &format!("X-MicSync-Token: {token}\r\n"));
        let head2 = read_test_head(&mut c2);
        assert!(head2.starts_with("HTTP/1.1 200"), "{head2}");
        assert!(handle.pending().is_none(), "预授权过的设备不该再弹确认");
        handle.stop();
    }

    /// 「允许一次」的预授权 = 允许接下来的一轮使用:下一次 /stream 免弹窗,
    /// 再往后恢复确认——否则那次同意就成了什么都没换来的空头承诺
    #[test]
    fn authorize_once_covers_exactly_one_stream() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let port = handle.port;

        let mut c = http_get_with(port, "/authorize", "X-MicSync-Name: 跟随的 Mac\r\n");
        wait_pending(&handle, 2000).expect("应出现待确认请求");
        handle.decide(Decision::Once);
        let head = read_test_head(&mut c);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(
            !head.to_ascii_lowercase().contains("x-micsync-token"),
            "「允许一次」不该签发令牌:{head}"
        );
        drop(c);

        // 第一次 /stream(同名同 IP):兑现放行券,直接串流
        let mut c2 = http_get_with(port, "/stream", "X-MicSync-Name: 跟随的 Mac\r\n");
        let head2 = read_test_head(&mut c2);
        assert!(head2.starts_with("HTTP/1.1 200"), "{head2}");
        assert!(handle.pending().is_none(), "放行券在手不该弹确认");
        drop(c2);

        // 第二次:券已作废,恢复确认
        let _c3 = http_get_with(port, "/stream", "X-MicSync-Name: 跟随的 Mac\r\n");
        assert!(
            wait_pending(&handle, 2000).is_some(),
            "「允许一次」只覆盖一轮使用"
        );
        handle.stop();
    }

    /// 放行券绑定设备名:别的名字(可能是另一台机器)不能冒领
    #[test]
    fn once_grant_is_not_transferable() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let port = handle.port;

        let mut c = http_get_with(port, "/authorize", "X-MicSync-Name: 甲\r\n");
        wait_pending(&handle, 2000).expect("应出现待确认请求");
        handle.decide(Decision::Once);
        assert_eq!(read_status(&mut c), 200);
        drop(c);

        let _c2 = http_get_with(port, "/stream", "X-MicSync-Name: 乙\r\n");
        assert!(
            wait_pending(&handle, 2000).is_some(),
            "名字不匹配不能兑现放行券"
        );
        handle.stop();
    }

    /// 预授权的确认没有时限,但对方断开(关掉跟随/退出应用)要立刻
    /// 撤销待确认请求,弹窗不能永远挂着堵住别人
    #[test]
    fn authorize_abandoned_when_client_disconnects() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let c = http_get_with(handle.port, "/authorize", "X-MicSync-Name: 甲\r\n");
        wait_pending(&handle, 2000).expect("应出现待确认请求");
        drop(c);
        assert!(
            wait_until(|| handle.pending().is_none(), 3000),
            "对方断开后应撤销待确认请求"
        );
        handle.stop();
    }

    /// 已信任(有令牌)的客户端预授权直接放行,不打扰用户
    #[test]
    fn authorize_with_valid_token_returns_immediately() {
        let _guard = FAKE_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let handle = start_with(None, 0, fake_capture).expect("start");
        let token = settings::trust_client("已信任设备");
        let mut c = http_get_with(
            handle.port,
            "/authorize",
            &format!("X-MicSync-Token: {token}\r\n"),
        );
        assert_eq!(read_status(&mut c), 200);
        assert!(handle.pending().is_none(), "已信任设备不该弹确认");
        settings::revoke_trusted(&token);
        handle.stop();
    }

    #[test]
    fn header_value_is_case_insensitive_and_trims() {
        let head = "GET /stream HTTP/1.1\r\nHost: t\r\nX-MicSync-Name:  我的 Mac \r\n\r\n";
        assert_eq!(header_value(head, "x-micsync-name"), "我的 Mac");
        assert_eq!(header_value(head, "X-MicSync-Name"), "我的 Mac");
        assert_eq!(header_value(head, "x-micsync-token"), "");
        // 请求行里的冒号不该被当成头
        assert_eq!(header_value(head, "GET /stream HTTP/1.1"), "");
    }
}
