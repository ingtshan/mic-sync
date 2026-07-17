use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};

use crate::audio;
use crate::server::{END_PREEMPTED, MAGIC};
use crate::settings;

/// 收流缓冲上限(毫秒)——超过则丢最老的数据,防止延迟无限增长
const MAX_BUFFER_MS: usize = 300;
/// 起播水位(毫秒)——攒够这些数据才开始出声,吸收网络抖动
const PREBUFFER_MS: usize = 60;
/// 连接中断后的重连间隔(毫秒)
const RETRY_LOST_MS: u64 = 800;
/// 服务端不可达时的重试间隔(毫秒)
const RETRY_OFFLINE_MS: u64 = 1500;

/// 客户端运行模式:连接/重连中 / 等待对方确认授权 / 正在收流 /
/// 已结束(被接管或服务端停止) / 被拒绝(终态,不再重试)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Connecting = 0,
    Waiting = 1,
    Streaming = 2,
    Ended = 3,
    Denied = 4,
}

impl Mode {
    fn from_u8(v: u8) -> Mode {
        match v {
            1 => Mode::Waiting,
            2 => Mode::Streaming,
            3 => Mode::Ended,
            4 => Mode::Denied,
            _ => Mode::Connecting,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Connecting => "connecting",
            Mode::Waiting => "waiting",
            Mode::Streaming => "streaming",
            Mode::Ended => "ended",
            Mode::Denied => "denied",
        }
    }
}

/// 认领 /stream 的失败原因——决定要不要重试。
/// 分清这个很重要:被拒绝还不停重试 = 每 1.5 秒骚扰对方一次
enum OpenErr {
    /// 对方拒绝或未在时限内确认:终态,等用户重新发起,绝不自动重试
    Denied(String),
    /// 网络不可达、开麦失败等:可以重试
    Retry(String),
}

/// 对端服务端的公开身份(发起使用时从 /health 取)
struct ServerIdent {
    /// 公开 device_id;空 = 旧版服务端,记不住授权
    id: String,
    /// 设备名,用于「等待 X 确认」这类提示
    name: String,
}

/// 一轮串流会话的结束方式
enum StreamEnd {
    /// 服务端发来 0 长度结束帧(被抢占 / 服务端停止),不应自动重连
    Graceful(u32),
    /// 网络中断/服务端异常退出,应自动重连
    Lost,
    /// 用户主动停止
    Stopped,
}

struct Shared {
    mode: AtomicU8,
    /// 服务端采样率(串流握手里读到的)
    src_rate: AtomicU32,
    /// 本机输出设备采样率;0 = 播放流尚未就绪
    out_rate: AtomicU32,
    level: AtomicU32,
    buffer: Mutex<VecDeque<f32>>,
    /// 起播状态:false 时静音蓄水
    started: AtomicBool,
    /// 本轮以「被其他设备接管」结束(区别于服务端主动停止)
    preempted: AtomicBool,
    error: Mutex<Option<String>>,
}

pub struct ClientHandle {
    pub addr: String,
    pub output_device: String,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl ClientHandle {
    /// 使用是否仍在进行(用户停止或被接管后为 false;断线重连中仍为 true)
    pub fn is_connected(&self) -> bool {
        !self.stop.load(Ordering::SeqCst)
    }

    pub fn mode(&self) -> Mode {
        Mode::from_u8(self.shared.mode.load(Ordering::Relaxed))
    }

    pub fn src_rate(&self) -> u32 {
        self.shared.src_rate.load(Ordering::Relaxed)
    }

    pub fn buffer_ms(&self) -> u32 {
        let out_rate = self.shared.out_rate.load(Ordering::Relaxed);
        if out_rate == 0 {
            return 0;
        }
        let len = self.shared.buffer.lock().unwrap().len();
        (len as u64 * 1000 / out_rate as u64) as u32
    }

    pub fn level(&self) -> f32 {
        audio::decode_level(self.shared.level.load(Ordering::Relaxed))
    }

    pub fn take_error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    /// 本轮是否因被其他设备接管而结束
    pub fn preempted(&self) -> bool {
        self.shared.preempted.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 解析服务端地址:允许省略端口,支持 IP 和主机名(含 .local mDNS 名称)
fn resolve_addr(addr: &str) -> Result<(SocketAddr, String), String> {
    let full_addr = if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{}", crate::DEFAULT_PORT)
    };
    let sock_addr = full_addr
        .to_socket_addrs()
        .map_err(|e| format!("地址解析失败 {full_addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("地址无法解析: {full_addr}"))?;
    Ok((sock_addr, full_addr))
}

/// 开始使用远端麦克风:先 /health 校验服务端,随后后台线程认领 /stream。
/// 请求即是事件——服务端收到后才会打开麦克风;停止使用即关闭。
pub fn connect(addr: String, output_device: Option<String>) -> Result<ClientHandle, String> {
    let (sock_addr, full_addr) = resolve_addr(&addr)?;

    // 立即校验对方是 MicSync 服务端,并取回其公开身份(用于匹配授权令牌)
    let (server_id, server_name) = fetch_health(&sock_addr, &full_addr, Duration::from_secs(3))?;
    let server = ServerIdent {
        id: server_id,
        name: server_name,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        mode: AtomicU8::new(Mode::Connecting as u8),
        src_rate: AtomicU32::new(0),
        out_rate: AtomicU32::new(0),
        level: AtomicU32::new(0),
        buffer: Mutex::new(VecDeque::new()),
        started: AtomicBool::new(false),
        preempted: AtomicBool::new(false),
        error: Mutex::new(None),
    });

    // 播放线程:cpal Stream 非 Send,在线程内创建并驻留;跨串流会话复用
    let (init_tx, init_rx) = sync_channel::<Result<String, String>>(1);
    {
        let stop = stop.clone();
        let shared = shared.clone();
        let device_name = output_device.clone();
        thread::Builder::new()
            .name("mic-playback".into())
            .spawn(move || playback_thread(device_name, stop, shared, init_tx))
            .map_err(|e| format!("创建播放线程失败: {e}"))?;
    }
    let actual_output = init_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "输出设备初始化超时".to_string())?
        .map_err(|e| {
            stop.store(true, Ordering::SeqCst);
            e
        })?;

    // 认领线程:请求 /stream 触发服务端开麦;断线自动重连,被接管则停止
    {
        let stop = stop.clone();
        let shared = shared.clone();
        let host = full_addr.clone();
        thread::Builder::new()
            .name("mic-claim".into())
            .spawn(move || claim_thread(sock_addr, host, server, stop, shared))
            .map_err(|e| format!("创建认领线程失败: {e}"))?;
    }

    Ok(ClientHandle {
        addr: full_addr,
        output_device: actual_output,
        stop,
        shared,
    })
}

fn claim_thread(
    sock_addr: SocketAddr,
    host: String,
    server: ServerIdent,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
) {
    while !stop.load(Ordering::SeqCst) {
        // 没有令牌 = 对方还没授权过本机,这次请求要等人点确认
        let token = settings::token_for_server(&server.id).unwrap_or_default();
        if token.is_empty() {
            shared.mode.store(Mode::Waiting as u8, Ordering::Relaxed);
            *shared.error.lock().unwrap() = Some(format!("等待「{}」确认授权…", server.name));
        }
        match open_stream(&sock_addr, &host, &token) {
            Ok((stream, src_rate, new_token)) => {
                // 对方选了「始终允许」:记下令牌,以后不再打扰他
                if let Some(t) = new_token {
                    settings::remember_server(&server.id, &t, &server.name);
                }
                shared.src_rate.store(src_rate, Ordering::Relaxed);
                *shared.error.lock().unwrap() = None;
                shared.mode.store(Mode::Streaming as u8, Ordering::Relaxed);
                match run_stream(stream, &stop, &shared) {
                    StreamEnd::Graceful(reason) => {
                        // 服务端明确结束:不自动重连,等用户重新发起
                        let msg = if reason == END_PREEMPTED {
                            shared.preempted.store(true, Ordering::Relaxed);
                            "麦克风已被其他设备接管,本机已停止使用"
                        } else {
                            "服务端已停止共享"
                        };
                        *shared.error.lock().unwrap() = Some(msg.into());
                        shared.mode.store(Mode::Ended as u8, Ordering::Relaxed);
                        stop.store(true, Ordering::SeqCst);
                        return;
                    }
                    StreamEnd::Stopped => return,
                    StreamEnd::Lost => {
                        shared.mode.store(Mode::Connecting as u8, Ordering::Relaxed);
                        *shared.error.lock().unwrap() = Some("连接中断,自动重连中".into());
                        sleep_check(&stop, RETRY_LOST_MS);
                    }
                }
            }
            // 对方拒绝/未确认:终态。绝不自动重试——那等于每 1.5 秒再弹一次窗骚扰他
            Err(OpenErr::Denied(msg)) => {
                *shared.error.lock().unwrap() = Some(msg);
                shared.mode.store(Mode::Denied as u8, Ordering::Relaxed);
                stop.store(true, Ordering::SeqCst);
                return;
            }
            Err(OpenErr::Retry(e)) => {
                shared.mode.store(Mode::Connecting as u8, Ordering::Relaxed);
                *shared.error.lock().unwrap() = Some(format!("服务端不可达,自动重试中: {e}"));
                sleep_check(&stop, RETRY_OFFLINE_MS);
            }
        }
    }
}

/// 分段睡眠,期间发现停止立即返回
fn sleep_check(stop: &AtomicBool, ms: u64) {
    let mut left = ms;
    while left > 0 && !stop.load(Ordering::SeqCst) {
        let step = left.min(100);
        thread::sleep(Duration::from_millis(step));
        left -= step;
    }
}

/// GET /health:确认对方是 MicSync 服务端,并取回它的公开身份 (device_id, 设备名)。
/// device_id 用来找出本机在该服务端上的授权令牌——公开可读,不是凭据
fn fetch_health(
    sock_addr: &SocketAddr,
    host: &str,
    timeout: Duration,
) -> Result<(String, String), String> {
    let mut stream =
        TcpStream::connect_timeout(sock_addr, timeout).map_err(|e| format!("连接失败: {e}"))?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("设置超时失败: {e}"))?;
    let _ = stream.set_write_timeout(Some(timeout));

    stream
        .write_all(
            format!("GET /health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .map_err(|e| format!("发送请求失败: {e}"))?;

    let mut resp = String::new();
    stream
        .read_to_string(&mut resp)
        .map_err(|e| format!("读取响应失败: {e}"))?;
    let (code, body) = split_http_response(&resp)?;
    if code != 200 {
        return Err(format!("服务端返回 HTTP {code}"));
    }
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| "响应不是有效 JSON,对方可能不是 MicSync 服务端".to_string())?;
    if v.get("app").and_then(|x| x.as_str()) != Some("micsync") {
        return Err("对方不是 MicSync 服务端".into());
    }
    // 0.5.0 之前的服务端没有这两个字段:仍可连(会走一次授权确认),只是记不住
    let id = v
        .get("device_id")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let name = v
        .get("alias")
        .and_then(|x| x.as_str())
        .unwrap_or("MicSync")
        .to_string();
    Ok((id, name))
}

/// 从响应头里取服务端签发的令牌(「始终允许」时才有)
fn token_from_head(head: &str) -> Option<String> {
    head.lines()
        .skip(1)
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k.trim().eq_ignore_ascii_case("x-micsync-token")).then(|| v.trim().to_string())
        })
        .filter(|t| !t.is_empty())
}

/// 预授权的结果
pub enum Preauth {
    /// 已获授权:本就有效的令牌、对方刚同意(「始终允许」已把新令牌存好;
    /// 「允许一次」则对方记了一张一次性放行券)
    Granted,
    /// 对方拒绝或未在时限内确认:终态,别再自动重试骚扰对方
    Denied(String),
    /// 旧版服务端没有 /authorize:退回「用到时再确认」的老流程
    Unsupported,
}

/// 心跳间隔:等对方确认预授权期间,隔这么久发一个字节表明「我还在等」。
/// 必须远小于服务端的放弃判定时限(15 秒)
const PREAUTH_HEARTBEAT_MS: u64 = 2000;

/// GET /authorize:预先请求对方授权本机使用其麦克风,但不开麦、不占会话。
/// 自动跟随开启时先走这一步——确认弹窗不该拖到用户开口说话那一刻。
/// 预授权的确认不设时限(挂到对方处理为止),等待期间发心跳字节表明还在等;
/// stop 置位即断开连接放弃,对方据此撤掉弹窗。
/// 网络类失败返回 Err(可重试);拒绝是 Ok(Denied) 终态
pub fn preauthorize(addr: &str, stop: &AtomicBool) -> Result<Preauth, String> {
    let (sock_addr, full_addr) = resolve_addr(addr)?;
    // 取对方公开身份,以便把签发的令牌记在它名下
    let (server_id, server_name) = fetch_health(&sock_addr, &full_addr, Duration::from_secs(3))?;
    // 手里已有的令牌也带上:有效则对方直接放行,失效(被撤销)则重新走确认
    let token = settings::token_for_server(&server_id).unwrap_or_default();

    let mut stream = TcpStream::connect_timeout(&sock_addr, Duration::from_secs(2))
        .map_err(|e| format!("连接失败: {e}"))?;
    let _ = stream.set_nodelay(true);
    // 短读超时轮询:既能及时发心跳,也能在用户停止跟随时立刻收手
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| format!("设置超时失败: {e}"))?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));

    let name = settings::device_name();
    let token_header = if token.is_empty() {
        String::new()
    } else {
        format!("X-MicSync-Token: {token}\r\n")
    };
    stream
        .write_all(
            format!(
                "GET /authorize HTTP/1.1\r\nHost: {full_addr}\r\nX-MicSync-Name: {name}\r\n{token_header}Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .map_err(|e| format!("发送请求失败: {e}"))?;

    // 读响应直到对方关连接(Connection: close);等待期间按节奏发心跳
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    let mut last_beat = std::time::Instant::now();
    loop {
        if stop.load(Ordering::SeqCst) {
            // 用户停止跟随:直接断开,对方读到 EOF 就会撤销确认弹窗
            return Err("已停止等待授权".into());
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if last_beat.elapsed() >= Duration::from_millis(PREAUTH_HEARTBEAT_MS) {
                    stream
                        .write_all(b".")
                        .map_err(|e| format!("等待确认时连接中断: {e}"))?;
                    last_beat = std::time::Instant::now();
                }
            }
            Err(e) => return Err(format!("读取响应失败: {e}")),
        }
    }
    let resp = String::from_utf8_lossy(&raw).to_string();
    let (code, body) = split_http_response(&resp)?;
    let message = |fallback: &str| {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| fallback.to_string())
    };
    match code {
        200 => {
            // 「始终允许」随响应签发令牌,记下来以后免确认
            let head = resp.split("\r\n\r\n").next().unwrap_or("");
            if let Some(t) = token_from_head(head) {
                settings::remember_server(&server_id, &t, &server_name);
            }
            Ok(Preauth::Granted)
        }
        // 拒绝/超时未确认是人的决定,不是网络故障——终态
        403 => Ok(Preauth::Denied(message("对方拒绝了麦克风授权请求"))),
        // 旧版服务端没有这个接口
        404 => Ok(Preauth::Unsupported),
        _ => Err(format!(
            "服务端拒绝授权请求: {}",
            message(&format!("HTTP {code}"))
        )),
    }
}

/// 拆 HTTP 响应:返回 (状态码, body)
fn split_http_response(resp: &str) -> Result<(u16, &str), String> {
    let (head, body) = resp
        .split_once("\r\n\r\n")
        .ok_or_else(|| "HTTP 响应不完整".to_string())?;
    let status_line = head.lines().next().unwrap_or("");
    if !status_line.starts_with("HTTP/1.") {
        return Err("对方不是 MicSync 服务端(非 HTTP 响应)".into());
    }
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| "HTTP 状态行异常".to_string())?;
    Ok((code, body))
}

/// GET /stream:请求使用麦克风(服务端据此按需开麦,新请求接管旧串流)。
/// 带上本机设备名与令牌:未授权时服务端会挂起请求等对方确认,故读超时要盖过确认时限。
/// 返回 (流, 采样率, 服务端新签发的令牌)
fn open_stream(
    sock_addr: &SocketAddr,
    host: &str,
    token: &str,
) -> Result<(TcpStream, u32, Option<String>), OpenErr> {
    let mut stream = TcpStream::connect_timeout(sock_addr, Duration::from_secs(2))
        .map_err(|e| OpenErr::Retry(format!("连接失败: {e}")))?;
    let _ = stream.set_nodelay(true);
    // 对方可能要等用户点确认(最长 30 秒),再加开麦约 5 秒——读超时必须盖过这段,
    // 否则我们会先超时断开,让对方点了确认却发现请求已经没了
    stream
        .set_read_timeout(Some(Duration::from_secs(40)))
        .map_err(|e| OpenErr::Retry(format!("设置超时失败: {e}")))?;

    // 设备名让对方知道是谁在请求;令牌是对方之前签发给本机的放行凭据(可能没有)
    let name = settings::device_name();
    let token_header = if token.is_empty() {
        String::new()
    } else {
        format!("X-MicSync-Token: {token}\r\n")
    };
    stream
        .write_all(
            format!(
                "GET /stream HTTP/1.1\r\nHost: {host}\r\nX-MicSync-Name: {name}\r\n{token_header}Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .map_err(|e| OpenErr::Retry(format!("发送请求失败: {e}")))?;

    // 逐字节读响应头(空行之后即二进制音频流,不能多读)
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if head.len() >= 8192 {
            return Err(OpenErr::Retry("串流响应头过长".into()));
        }
        match stream.read(&mut byte) {
            Ok(0) => return Err(OpenErr::Retry("串流握手被中断".into())),
            Ok(_) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(e) => return Err(OpenErr::Retry(format!("读取串流响应失败: {e}"))),
        }
    }
    let head = String::from_utf8_lossy(&head);
    let status_line = head.lines().next().unwrap_or("");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| OpenErr::Retry("串流响应状态行异常".into()))?;
    if code != 200 {
        // 带上服务端给的原因(如 503 开麦失败、403 对方拒绝)
        let mut body = String::new();
        let _ = stream.read_to_string(&mut body);
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
            .unwrap_or_else(|| format!("HTTP {code}"));
        // 403 = 对方拒绝或没在时限内确认。这是人的决定,不是网络故障——
        // 自动重试只会每隔一秒半再弹一次窗骚扰对方
        return Err(match code {
            403 => OpenErr::Denied(msg),
            _ => OpenErr::Retry(format!("服务端拒绝串流: {msg}")),
        });
    }

    // 「始终允许」时对方会随响应签发令牌,存下来以后免确认
    let new_token = token_from_head(&head);

    // 读二进制流头并校验
    let mut header = [0u8; 12];
    stream
        .read_exact(&mut header)
        .map_err(|e| OpenErr::Retry(format!("读取串流头失败: {e}")))?;
    if &header[0..4] != MAGIC {
        return Err(OpenErr::Retry(
            "对方不是 MicSync 服务端(协议头不匹配)".into(),
        ));
    }
    let src_rate = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if !(8000..=192_000).contains(&src_rate) {
        return Err(OpenErr::Retry(format!("服务端采样率异常: {src_rate}")));
    }
    Ok((stream, src_rate, new_token))
}

/// 一轮串流会话:读帧 → 重采样 → 进抖动缓冲,直到会话结束
fn run_stream(mut stream: TcpStream, stop: &AtomicBool, shared: &Shared) -> StreamEnd {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));

    // 等待播放线程就绪,拿到输出采样率后才能建重采样器
    let out_rate = loop {
        let r = shared.out_rate.load(Ordering::Relaxed);
        if r != 0 {
            break r;
        }
        if stop.load(Ordering::SeqCst) {
            return StreamEnd::Stopped;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let src_rate = shared.src_rate.load(Ordering::Relaxed);
    let mut resampler = audio::LinearResampler::new(src_rate, out_rate);
    let max_buffer = out_rate as usize * MAX_BUFFER_MS / 1000;

    let mut len_buf = [0u8; 4];
    let mut byte_buf: Vec<u8> = Vec::new();
    let mut resampled: Vec<f32> = Vec::new();

    loop {
        if stop.load(Ordering::SeqCst) {
            return StreamEnd::Stopped;
        }
        // 读帧头(样本数)
        match stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => return StreamEnd::Lost,
        }
        let n_samples = u32::from_le_bytes(len_buf) as usize;
        if n_samples == 0 {
            // 优雅结束帧:后随 u32 原因码(被接管 / 服务端停止)
            let mut reason = [0u8; 4];
            let reason = match stream.read_exact(&mut reason) {
                Ok(()) => u32::from_le_bytes(reason),
                Err(_) => crate::server::END_SERVER_CLOSING,
            };
            return StreamEnd::Graceful(reason);
        }
        if n_samples > 1 << 20 {
            *shared.error.lock().unwrap() = Some("收到异常数据帧,本轮串流已断开".into());
            return StreamEnd::Lost;
        }
        byte_buf.resize(n_samples * 2, 0);
        if stream.read_exact(&mut byte_buf).is_err() {
            return StreamEnd::Lost;
        }

        let samples: Vec<i16> = byte_buf
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let mono = audio::i16_to_f32(&samples);

        resampled.clear();
        resampler.process(&mono, &mut resampled);

        let mut buf = shared.buffer.lock().unwrap();
        buf.extend(resampled.iter().copied());
        // 超出上限 → 丢最老数据,保持低延迟
        while buf.len() > max_buffer {
            buf.pop_front();
        }
    }
}

fn playback_thread(
    device_name: Option<String>,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    init_tx: std::sync::mpsc::SyncSender<Result<String, String>>,
) {
    let device = match audio::find_output_device(device_name.as_deref()) {
        Some(d) => d,
        None => {
            let _ = init_tx.send(Err("找不到可用的输出设备".into()));
            return;
        }
    };
    let name = device.name().unwrap_or_else(|_| "未知设备".into());
    let config = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("读取输出设备配置失败: {e}")));
            return;
        }
    };
    let out_rate = config.sample_rate().0;
    let channels = config.channels() as usize;
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();

    let prebuffer = out_rate as usize * PREBUFFER_MS / 1000;

    // 出声回调:从抖动缓冲取单声道数据,复制到所有声道
    let fill = {
        let shared = shared.clone();
        move |out_frames: usize, write: &mut dyn FnMut(usize, f32)| {
            let mut buf = shared.buffer.lock().unwrap();
            let started = shared.started.load(Ordering::Relaxed);
            if !started && buf.len() >= prebuffer {
                shared.started.store(true, Ordering::Relaxed);
            }
            let mut peak = 0.0f32;
            for i in 0..out_frames {
                let s = if shared.started.load(Ordering::Relaxed) {
                    match buf.pop_front() {
                        Some(s) => s,
                        None => {
                            // 欠载:回到蓄水状态
                            shared.started.store(false, Ordering::Relaxed);
                            0.0
                        }
                    }
                } else {
                    0.0
                };
                peak = peak.max(s.abs());
                write(i, s);
            }
            shared
                .level
                .store(audio::encode_level(peak), Ordering::Relaxed);
        }
    };

    let err_fn = {
        let shared = shared.clone();
        move |e: cpal::StreamError| {
            *shared.error.lock().unwrap() = Some(format!("播放流错误: {e}"));
        }
    };

    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &stream_config,
            {
                let fill = fill.clone();
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    fill(frames, &mut |i, s| {
                        for c in 0..channels {
                            data[i * channels + c] = s;
                        }
                    });
                }
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            &stream_config,
            {
                let fill = fill.clone();
                move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    fill(frames, &mut |i, s| {
                        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        for c in 0..channels {
                            data[i * channels + c] = v;
                        }
                    });
                }
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            &stream_config,
            {
                let fill = fill.clone();
                move |data: &mut [u16], _: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    fill(frames, &mut |i, s| {
                        let v = ((s.clamp(-1.0, 1.0) * 0.5 + 0.5) * u16::MAX as f32) as u16;
                        for c in 0..channels {
                            data[i * channels + c] = v;
                        }
                    });
                }
            },
            err_fn,
            None,
        ),
        other => {
            let _ = init_tx.send(Err(format!("不支持的输出采样格式: {other:?}")));
            return;
        }
    };

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(format!("创建播放流失败: {e}")));
            return;
        }
    };
    if let Err(e) = stream.play() {
        let _ = init_tx.send(Err(format!("启动播放失败: {e}")));
        return;
    }

    // 播放流就绪,公布输出采样率(收流会话据此建重采样器)
    shared.out_rate.store(out_rate, Ordering::Relaxed);
    let _ = init_tx.send(Ok(name));

    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }
    drop(stream);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::END_SERVER_CLOSING;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicUsize;

    /// 构造测试用共享状态;out_rate 预置为已就绪,绕过真实声卡
    /// (无头测试进程里 CoreAudio 设备枚举可能耗时近 1 分钟,不能依赖)
    fn test_shared(out_rate: u32) -> Arc<Shared> {
        Arc::new(Shared {
            mode: AtomicU8::new(Mode::Connecting as u8),
            src_rate: AtomicU32::new(0),
            out_rate: AtomicU32::new(out_rate),
            level: AtomicU32::new(0),
            buffer: Mutex::new(VecDeque::new()),
            started: AtomicBool::new(false),
            preempted: AtomicBool::new(false),
            error: Mutex::new(None),
        })
    }

    /// 起认领线程。默认伪装成「已获授权」(本机存有该服务端的令牌),
    /// 这些用例验的是串流本身,不是授权闸门
    fn spawn_claimer(addr: SocketAddr, shared: &Arc<Shared>) -> Arc<AtomicBool> {
        let server_id = format!("test-server-{}", addr.port());
        settings::remember_server(&server_id, "test-token", "测试服务端");
        spawn_claimer_as(addr, shared, &server_id)
    }

    fn spawn_claimer_as(
        addr: SocketAddr,
        shared: &Arc<Shared>,
        server_id: &str,
    ) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        let server = ServerIdent {
            id: server_id.to_string(),
            name: "测试服务端".into(),
        };
        {
            let stop = stop.clone();
            let shared = shared.clone();
            thread::spawn(move || claim_thread(addr, addr.to_string(), server, stop, shared));
        }
        stop
    }

    fn mode_of(shared: &Shared) -> Mode {
        Mode::from_u8(shared.mode.load(Ordering::Relaxed))
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

    fn write_health(stream: &mut TcpStream) {
        let body =
            r#"{"status":"ok","app":"micsync","streaming":false,"client":"","sample_rate":44100}"#;
        let _ = stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        );
    }

    fn write_stream_header(stream: &mut TcpStream) -> bool {
        let mut resp = Vec::new();
        resp.extend_from_slice(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        );
        resp.extend_from_slice(MAGIC);
        resp.extend_from_slice(&44100u32.to_le_bytes());
        resp.extend_from_slice(&1u16.to_le_bytes());
        resp.extend_from_slice(&0u16.to_le_bytes());
        stream.write_all(&resp).is_ok()
    }

    /// 实时节奏发 10ms 一帧的 440Hz 正弦波(幅度低,喇叭里只有轻微提示音)
    fn write_sine_frames(stream: &mut TcpStream, n: usize) -> bool {
        let frame_len = 441usize;
        let mut phase = 0.0f32;
        for _ in 0..n {
            let mut buf = Vec::with_capacity(4 + frame_len * 2);
            buf.extend_from_slice(&(frame_len as u32).to_le_bytes());
            for _ in 0..frame_len {
                let s = (phase * std::f32::consts::TAU).sin() * 0.15;
                phase = (phase + 440.0 / 44100.0).fract();
                buf.extend_from_slice(&((s * i16::MAX as f32) as i16).to_le_bytes());
            }
            if stream.write_all(&buf).is_err() {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn write_end(stream: &mut TcpStream, reason: u32) {
        let mut buf = Vec::with_capacity(8);
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&reason.to_le_bytes());
        let _ = stream.write_all(&buf);
    }

    /// 假服务端:/health 应答;/stream 按 plan 行为。返回 (地址, /stream 计数)
    #[derive(Clone, Copy)]
    enum StreamPlan {
        /// 持续发帧(约 n 帧后正常继续下一连接)
        Frames(usize),
        /// 发几帧后发结束帧(原因码)
        EndAfter(usize, u32),
        /// 发几帧后直接断开(模拟网络中断/服务端崩溃)
        DropAfter(usize),
    }

    fn spawn_fake_server(plan: StreamPlan) -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let claims = Arc::new(AtomicUsize::new(0));
        let claims_ret = claims.clone();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let head = read_req(&mut stream);
                if head.starts_with("GET /health") {
                    write_health(&mut stream);
                } else if head.starts_with("GET /stream") {
                    claims.fetch_add(1, Ordering::SeqCst);
                    if !write_stream_header(&mut stream) {
                        continue;
                    }
                    match plan {
                        StreamPlan::Frames(n) => {
                            let _ = write_sine_frames(&mut stream, n);
                        }
                        StreamPlan::EndAfter(n, reason) => {
                            let _ = write_sine_frames(&mut stream, n);
                            write_end(&mut stream, reason);
                        }
                        StreamPlan::DropAfter(n) => {
                            let _ = write_sine_frames(&mut stream, n);
                            // 直接 drop 连接
                        }
                    }
                }
            }
        });
        (addr, claims_ret)
    }

    /// 认领即收流:发起使用后进入 Streaming,音频进入抖动缓冲
    #[test]
    fn claimer_streams_on_demand() {
        let (addr, claims) = spawn_fake_server(StreamPlan::Frames(150));
        let shared = test_shared(44100);
        let stop = spawn_claimer(addr, &shared);

        thread::sleep(Duration::from_millis(700));
        assert_eq!(mode_of(&shared), Mode::Streaming, "should be streaming");
        assert_eq!(shared.src_rate.load(Ordering::Relaxed), 44100);
        assert!(claims.load(Ordering::SeqCst) >= 1);
        assert!(
            !shared.buffer.lock().unwrap().is_empty(),
            "jitter buffer should be filling"
        );
        let err = shared.error.lock().unwrap().clone();
        assert!(err.is_none(), "unexpected error: {err:?}");
        stop.store(true, Ordering::SeqCst);
    }

    /// 被其他设备接管:收到结束帧后停止使用,不再自动重连
    #[test]
    fn claimer_stops_when_preempted() {
        let (addr, claims) = spawn_fake_server(StreamPlan::EndAfter(10, END_PREEMPTED));
        let shared = test_shared(44100);
        let stop = spawn_claimer(addr, &shared);

        thread::sleep(Duration::from_millis(800));
        assert_eq!(mode_of(&shared), Mode::Ended, "should end after preemption");
        assert!(stop.load(Ordering::SeqCst), "claimer should stop itself");
        let err = shared.error.lock().unwrap().clone().unwrap_or_default();
        assert!(err.contains("接管"), "error should mention takeover: {err}");
        assert!(
            shared.preempted.load(Ordering::Relaxed),
            "preempted flag should be set"
        );
        // 结束后不再发起新的认领
        let claimed = claims.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(600));
        assert_eq!(claims.load(Ordering::SeqCst), claimed, "must not reclaim");
    }

    /// 服务端停止:收到结束帧后停止使用
    #[test]
    fn claimer_stops_when_server_closes() {
        let (addr, _claims) = spawn_fake_server(StreamPlan::EndAfter(5, END_SERVER_CLOSING));
        let shared = test_shared(44100);
        let stop = spawn_claimer(addr, &shared);

        thread::sleep(Duration::from_millis(600));
        assert_eq!(mode_of(&shared), Mode::Ended);
        assert!(stop.load(Ordering::SeqCst));
        assert!(
            !shared.preempted.load(Ordering::Relaxed),
            "server close is not a preemption"
        );
    }

    /// 网络中断(无结束帧):自动重连,再次认领
    #[test]
    fn claimer_reclaims_after_drop() {
        let (addr, claims) = spawn_fake_server(StreamPlan::DropAfter(5));
        let shared = test_shared(44100);
        let stop = spawn_claimer(addr, &shared);

        thread::sleep(Duration::from_millis(2500));
        assert!(
            claims.load(Ordering::SeqCst) >= 2,
            "should auto-reclaim after connection drop, claims={}",
            claims.load(Ordering::SeqCst)
        );
        stop.store(true, Ordering::SeqCst);
    }

    #[test]
    fn connect_rejects_non_micsync_server() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.write_all(b"NOT-HTTP-AT-ALL\r\n\r\n");
            thread::sleep(Duration::from_millis(200));
        });
        let result = connect(addr.to_string(), None);
        assert!(result.is_err(), "should reject non-MicSync server");
    }

    #[test]
    fn connect_reports_unreachable_server() {
        // TEST-NET-1 地址,必然连不上
        let result = connect("192.0.2.1:47800".into(), None);
        assert!(result.is_err());
    }

    /// 端到端(打开真实声卡出声)。无头环境下 CoreAudio 设备枚举极慢会超时,
    /// 手动验证: cargo test -- --ignored
    #[test]
    #[ignore = "需要真实输出声卡,手动运行"]
    fn client_streams_end_to_end() {
        let (addr, _claims) = spawn_fake_server(StreamPlan::Frames(150));
        let handle = connect(addr.to_string(), None).expect("client should connect to fake server");
        assert!(handle.is_connected());

        // 等待认领串流、越过起播水位并真正出声
        thread::sleep(Duration::from_millis(900));
        assert_eq!(handle.mode(), Mode::Streaming, "should be streaming");
        assert_eq!(handle.src_rate(), 44100);
        assert!(
            handle.level() > 0.01,
            "playback level should be non-zero, got {}",
            handle.level()
        );
        let err = handle.take_error();
        assert!(err.is_none(), "unexpected error: {err:?}");

        handle.stop();
        thread::sleep(Duration::from_millis(300));
        assert!(!handle.is_connected(), "stop() should disconnect");
    }
    /// 被拒绝是人的决定,不是网络故障:必须停在终态。
    /// 若走重试逻辑,对方会每 1.5 秒被弹一次窗——比没有授权还糟
    #[test]
    fn denied_is_terminal_and_does_not_retry() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicU32::new(0));
        {
            let hits = hits.clone();
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut s) = stream else { continue };
                    hits.fetch_add(1, Ordering::SeqCst);
                    let _ = read_req(&mut s);
                    let body = r#"{"error":"denied","message":"对方拒绝了本次麦克风使用请求"}"#;
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    );
                }
            });
        }

        let shared = test_shared(48000);
        let stop = spawn_claimer_as(addr, &shared, "unknown-server");
        // 给足够时间:若会重试,RETRY_OFFLINE_MS(1.5s)内必然打出第二次请求
        thread::sleep(Duration::from_millis(2500));

        assert_eq!(mode_of(&shared), Mode::Denied, "被拒绝应停在 denied 终态");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "被拒绝后绝不能重试骚扰对方");
        assert!(stop.load(Ordering::SeqCst), "认领线程应已自行收手");
        let err = shared.error.lock().unwrap().clone().unwrap_or_default();
        assert!(err.contains("拒绝"), "应把对方给的原因带给用户: {err}");
    }

    /// 预授权「始终允许」:随响应签发的令牌要存到对方名下,之后免确认
    #[test]
    fn preauthorize_remembers_issued_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let device_id = format!("preauth-server-{}", addr.port());
        {
            let device_id = device_id.clone();
            thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut s) = conn else { continue };
                    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                    let head = read_req(&mut s);
                    if head.starts_with("GET /health") {
                        let body = format!(
                            r#"{{"status":"ok","app":"micsync","streaming":false,"client":"","sample_rate":0,"alias":"测试端","device_id":"{device_id}"}}"#
                        );
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        );
                    } else if head.starts_with("GET /authorize") {
                        let body = r#"{"status":"authorized"}"#;
                        let _ = s.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-MicSync-Token: preauth-token-1\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        );
                    }
                }
            });
        }

        match preauthorize(&addr.to_string(), &AtomicBool::new(false)) {
            Ok(Preauth::Granted) => {}
            Ok(Preauth::Denied(m)) => panic!("不该被拒绝: {m}"),
            Ok(Preauth::Unsupported) => panic!("不该判为旧版服务端"),
            Err(e) => panic!("不该失败: {e}"),
        }
        assert_eq!(
            settings::token_for_server(&device_id).as_deref(),
            Some("preauth-token-1"),
            "签发的令牌应存到对方名下"
        );
    }

    /// 服务端不可达属于网络故障,这条路仍应重试(别把重试逻辑一起改坏了)
    #[test]
    fn unreachable_server_still_retries() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // 端口无人监听

        let shared = test_shared(48000);
        let stop = spawn_claimer_as(addr, &shared, "unknown-server");
        thread::sleep(Duration::from_millis(300));
        assert_eq!(mode_of(&shared), Mode::Connecting, "不可达应保持重连态");
        assert!(!stop.load(Ordering::SeqCst), "不该自行收手");
        stop.store(true, Ordering::SeqCst);
    }
}
