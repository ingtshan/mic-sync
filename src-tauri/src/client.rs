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
use crate::server::MAGIC;

/// 收流缓冲上限(毫秒)——超过则丢最老的数据,防止延迟无限增长
const MAX_BUFFER_MS: usize = 300;
/// 起播水位(毫秒)——攒够这些数据才开始出声,吸收网络抖动
const PREBUFFER_MS: usize = 60;
/// 待机时 /health 轮询间隔(毫秒)
const POLL_STANDBY_MS: u64 = 250;
/// 服务端不可达时的重试间隔(毫秒)
const POLL_OFFLINE_MS: u64 = 1500;

/// 客户端运行模式:离线重试 / 待机轮询 / 正在收流
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Offline = 0,
    Standby = 1,
    Streaming = 2,
}

impl Mode {
    fn from_u8(v: u8) -> Mode {
        match v {
            2 => Mode::Streaming,
            1 => Mode::Standby,
            _ => Mode::Offline,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Offline => "offline",
            Mode::Standby => "standby",
            Mode::Streaming => "streaming",
        }
    }
}

struct Shared {
    mode: AtomicU8,
    /// 服务端采样率(握手/健康检查里读到的)
    src_rate: AtomicU32,
    /// 本机输出设备采样率;0 = 播放流尚未就绪
    out_rate: AtomicU32,
    level: AtomicU32,
    buffer: Mutex<VecDeque<f32>>,
    /// 起播状态:false 时静音蓄水
    started: AtomicBool,
    error: Mutex<Option<String>>,
}

/// /health 响应里客户端关心的字段
struct Health {
    mic_active: bool,
    streaming: bool,
    sample_rate: u32,
}

pub struct ClientHandle {
    pub addr: String,
    pub output_device: String,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl ClientHandle {
    /// 监听是否仍在运行(停止后为 false;离线重试中仍为 true)
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

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for ClientHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 开始监听服务端:先做一次 /health 校验,之后由后台线程事件驱动收流
pub fn connect(addr: String, output_device: Option<String>) -> Result<ClientHandle, String> {
    // 允许省略端口
    let full_addr = if addr.contains(':') {
        addr.clone()
    } else {
        format!("{addr}:{}", crate::DEFAULT_PORT)
    };
    // 支持 IP 和主机名(含 .local mDNS 名称)
    let sock_addr = full_addr
        .to_socket_addrs()
        .map_err(|e| format!("地址解析失败 {full_addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("地址无法解析: {full_addr}"))?;

    // 立即校验对方是 MicSync 服务端,失败直接报错给用户
    let health = fetch_health(&sock_addr, &full_addr, Duration::from_secs(3))?;

    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        mode: AtomicU8::new(Mode::Standby as u8),
        src_rate: AtomicU32::new(health.sample_rate),
        out_rate: AtomicU32::new(0),
        level: AtomicU32::new(0),
        buffer: Mutex::new(VecDeque::new()),
        started: AtomicBool::new(false),
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

    // 监听线程:轮询 /health,mic 激活且串流空闲时认领 /stream
    {
        let stop = stop.clone();
        let shared = shared.clone();
        let host = full_addr.clone();
        thread::Builder::new()
            .name("mic-watch".into())
            .spawn(move || watcher_thread(sock_addr, host, stop, shared))
            .map_err(|e| format!("创建监听线程失败: {e}"))?;
    }

    Ok(ClientHandle {
        addr: full_addr,
        output_device: actual_output,
        stop,
        shared,
    })
}

fn watcher_thread(sock_addr: SocketAddr, host: String, stop: Arc<AtomicBool>, shared: Arc<Shared>) {
    while !stop.load(Ordering::SeqCst) {
        match fetch_health(&sock_addr, &host, Duration::from_millis(1500)) {
            Err(e) => {
                shared.mode.store(Mode::Offline as u8, Ordering::Relaxed);
                *shared.error.lock().unwrap() = Some(format!("服务端不可达,自动重试中: {e}"));
                sleep_check(&stop, POLL_OFFLINE_MS);
            }
            Ok(h) => {
                if Mode::from_u8(shared.mode.load(Ordering::Relaxed)) == Mode::Offline {
                    // 服务端恢复:清掉离线错误
                    *shared.error.lock().unwrap() = None;
                }
                shared.mode.store(Mode::Standby as u8, Ordering::Relaxed);
                if h.sample_rate != 0 {
                    shared.src_rate.store(h.sample_rate, Ordering::Relaxed);
                }
                if h.mic_active && !h.streaming {
                    match open_stream(&sock_addr, &host) {
                        Ok(Some((stream, src_rate))) => {
                            shared.src_rate.store(src_rate, Ordering::Relaxed);
                            *shared.error.lock().unwrap() = None;
                            shared.mode.store(Mode::Streaming as u8, Ordering::Relaxed);
                            run_stream(stream, &stop, &shared);
                            // 本轮结束(服务端静音收流/断开)→ 回待机继续轮询
                            shared.mode.store(Mode::Standby as u8, Ordering::Relaxed);
                        }
                        // 槽位被别的设备抢先认领 → 回待机
                        Ok(None) => sleep_check(&stop, POLL_STANDBY_MS),
                        Err(e) => {
                            *shared.error.lock().unwrap() = Some(e);
                            sleep_check(&stop, POLL_STANDBY_MS);
                        }
                    }
                } else {
                    sleep_check(&stop, POLL_STANDBY_MS);
                }
            }
        }
    }
    shared.mode.store(Mode::Offline as u8, Ordering::Relaxed);
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

/// GET /health:确认对方是 MicSync 服务端并读取状态
fn fetch_health(sock_addr: &SocketAddr, host: &str, timeout: Duration) -> Result<Health, String> {
    let mut stream = TcpStream::connect_timeout(sock_addr, timeout)
        .map_err(|e| format!("连接失败: {e}"))?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("设置超时失败: {e}"))?;
    let _ = stream.set_write_timeout(Some(timeout));

    stream
        .write_all(format!("GET /health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes())
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
    Ok(Health {
        mic_active: v.get("mic_active").and_then(|x| x.as_bool()).unwrap_or(false),
        streaming: v.get("streaming").and_then(|x| x.as_bool()).unwrap_or(false),
        sample_rate: v.get("sample_rate").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
    })
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

/// GET /stream:认领串流。Ok(None) = 槽位被占/mic 刚转静音,稍后重试
fn open_stream(sock_addr: &SocketAddr, host: &str) -> Result<Option<(TcpStream, u32)>, String> {
    let mut stream = TcpStream::connect_timeout(sock_addr, Duration::from_secs(2))
        .map_err(|e| format!("连接失败: {e}"))?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| format!("设置超时失败: {e}"))?;

    stream
        .write_all(format!("GET /stream HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes())
        .map_err(|e| format!("发送请求失败: {e}"))?;

    // 逐字节读响应头(空行之后即二进制音频流,不能多读)
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if head.len() >= 8192 {
            return Err("串流响应头过长".into());
        }
        match stream.read(&mut byte) {
            Ok(0) => return Err("串流握手被中断".into()),
            Ok(_) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(e) => return Err(format!("读取串流响应失败: {e}")),
        }
    }
    let head = String::from_utf8_lossy(&head);
    let status_line = head.lines().next().unwrap_or("");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| "串流响应状态行异常".to_string())?;
    if code == 409 {
        return Ok(None);
    }
    if code != 200 {
        return Err(format!("串流请求被拒: HTTP {code}"));
    }

    // 读二进制流头并校验
    let mut header = [0u8; 12];
    stream
        .read_exact(&mut header)
        .map_err(|e| format!("读取串流头失败: {e}"))?;
    if &header[0..4] != MAGIC {
        return Err("对方不是 MicSync 服务端(协议头不匹配)".into());
    }
    let src_rate = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if !(8000..=192_000).contains(&src_rate) {
        return Err(format!("服务端采样率异常: {src_rate}"));
    }
    Ok(Some((stream, src_rate)))
}

/// 一轮串流会话:读帧 → 重采样 → 进抖动缓冲;流结束(服务端静音收流)即返回
fn run_stream(mut stream: TcpStream, stop: &AtomicBool, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));

    // 等待播放线程就绪,拿到输出采样率后才能建重采样器
    let out_rate = loop {
        let r = shared.out_rate.load(Ordering::Relaxed);
        if r != 0 {
            break r;
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let src_rate = shared.src_rate.load(Ordering::Relaxed);
    let mut resampler = audio::LinearResampler::new(src_rate, out_rate);
    let max_buffer = out_rate as usize * MAX_BUFFER_MS / 1000;

    let mut len_buf = [0u8; 4];
    let mut byte_buf: Vec<u8> = Vec::new();
    let mut resampled: Vec<f32> = Vec::new();

    while !stop.load(Ordering::SeqCst) {
        // 读帧头(样本数);EOF/断开 = 本轮串流结束,不算错误
        match stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => return,
        }
        let n_samples = u32::from_le_bytes(len_buf) as usize;
        if n_samples == 0 || n_samples > 1 << 20 {
            *shared.error.lock().unwrap() = Some("收到异常数据帧,本轮串流已断开".into());
            return;
        }
        byte_buf.resize(n_samples * 2, 0);
        if stream.read_exact(&mut byte_buf).is_err() {
            return;
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
                            // 欠载:回到蓄水状态(串流间歇期为常态)
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
    use std::net::TcpListener;

    /// 构造测试用共享状态;out_rate 预置为已就绪,绕过真实声卡
    /// (无头测试进程里 CoreAudio 设备枚举可能耗时近 1 分钟,不能依赖)
    fn test_shared(out_rate: u32) -> Arc<Shared> {
        Arc::new(Shared {
            mode: AtomicU8::new(Mode::Standby as u8),
            src_rate: AtomicU32::new(0),
            out_rate: AtomicU32::new(out_rate),
            level: AtomicU32::new(0),
            buffer: Mutex::new(VecDeque::new()),
            started: AtomicBool::new(false),
            error: Mutex::new(None),
        })
    }

    fn spawn_watcher(addr: SocketAddr, shared: &Arc<Shared>) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        {
            let stop = stop.clone();
            let shared = shared.clone();
            thread::spawn(move || watcher_thread(addr, addr.to_string(), stop, shared));
        }
        stop
    }

    fn mode_of(shared: &Shared) -> Mode {
        Mode::from_u8(shared.mode.load(Ordering::Relaxed))
    }

    /// 极简假服务端:循环应答 /health;/stream 发 1.5 秒 440Hz 正弦波
    /// mic_active/streaming 由调用方指定;serve_stream=false 时 /stream 一律 409
    fn spawn_fake_server(mic_active: bool, streaming: bool, serve_stream: bool) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { continue };
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                // 读请求头
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
                let head = String::from_utf8_lossy(&head).to_string();
                if head.starts_with("GET /health") {
                    let body = format!(
                        r#"{{"status":"ok","app":"micsync","sample_rate":44100,"device":"fake","mic_active":{mic_active},"streaming":{streaming}}}"#
                    );
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                } else if head.starts_with("GET /stream") {
                    if !serve_stream {
                        let _ = stream.write_all(
                            b"HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"error\":\"busy\"}",
                        );
                        continue;
                    }
                    let mut resp = Vec::new();
                    resp.extend_from_slice(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                    );
                    resp.extend_from_slice(MAGIC);
                    resp.extend_from_slice(&44100u32.to_le_bytes());
                    resp.extend_from_slice(&1u16.to_le_bytes());
                    resp.extend_from_slice(&0u16.to_le_bytes());
                    if stream.write_all(&resp).is_err() {
                        continue;
                    }
                    // 实时节奏发 10ms 帧,共 1.5 秒(幅度调低,测试时喇叭里只有轻微提示音)
                    let frame_len = 441usize;
                    let mut phase = 0.0f32;
                    for _ in 0..150 {
                        let mut buf = Vec::with_capacity(4 + frame_len * 2);
                        buf.extend_from_slice(&(frame_len as u32).to_le_bytes());
                        for _ in 0..frame_len {
                            let s = (phase * std::f32::consts::TAU).sin() * 0.15;
                            phase = (phase + 440.0 / 44100.0).fract();
                            buf.extend_from_slice(&((s * i16::MAX as f32) as i16).to_le_bytes());
                        }
                        if stream.write_all(&buf).is_err() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        });
        addr
    }

    /// health 显示 mic 激活 → 监听线程自动认领串流,音频进入抖动缓冲
    #[test]
    fn watcher_streams_when_mic_active() {
        let addr = spawn_fake_server(true, false, true);
        let shared = test_shared(44100);
        let stop = spawn_watcher(addr, &shared);

        thread::sleep(Duration::from_millis(700));
        assert_eq!(mode_of(&shared), Mode::Streaming, "should be streaming");
        assert_eq!(shared.src_rate.load(Ordering::Relaxed), 44100);
        assert!(
            !shared.buffer.lock().unwrap().is_empty(),
            "jitter buffer should be filling"
        );
        let err = shared.error.lock().unwrap().clone();
        assert!(err.is_none(), "unexpected error: {err:?}");
        stop.store(true, Ordering::SeqCst);
    }

    /// 串流被其他设备占用:保持待机,不报错、不抢流
    #[test]
    fn watcher_stays_standby_when_stream_busy() {
        let addr = spawn_fake_server(true, true, false);
        let shared = test_shared(48000);
        let stop = spawn_watcher(addr, &shared);

        thread::sleep(Duration::from_millis(600));
        assert_eq!(mode_of(&shared), Mode::Standby, "should stay standby");
        assert!(shared.buffer.lock().unwrap().is_empty(), "no audio expected");
        let err = shared.error.lock().unwrap().clone();
        assert!(err.is_none(), "unexpected error: {err:?}");
        stop.store(true, Ordering::SeqCst);
    }

    /// mic 未激活:待机等待,不请求串流
    #[test]
    fn watcher_waits_while_mic_idle() {
        let addr = spawn_fake_server(false, false, false);
        let shared = test_shared(48000);
        let stop = spawn_watcher(addr, &shared);

        thread::sleep(Duration::from_millis(600));
        assert_eq!(mode_of(&shared), Mode::Standby);
        assert!(shared.buffer.lock().unwrap().is_empty(), "no audio expected");
        stop.store(true, Ordering::SeqCst);
    }

    /// 服务端不可达:进入离线模式并带错误信息,持续重试
    #[test]
    fn watcher_reports_offline() {
        // 拿一个刚释放的端口,必然连接被拒
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().unwrap()
        };
        let shared = test_shared(48000);
        let stop = spawn_watcher(addr, &shared);

        thread::sleep(Duration::from_millis(500));
        assert_eq!(mode_of(&shared), Mode::Offline);
        assert!(shared.error.lock().unwrap().is_some(), "offline error expected");
        stop.store(true, Ordering::SeqCst);
    }

    /// 端到端(打开真实声卡出声)。无头环境下 CoreAudio 设备枚举极慢会超时,
    /// 手动验证: cargo test -- --ignored
    #[test]
    #[ignore = "需要真实输出声卡,手动运行"]
    fn client_streams_end_to_end() {
        let addr = spawn_fake_server(true, false, true);
        let handle =
            connect(addr.to_string(), None).expect("client should connect to fake server");
        assert!(handle.is_connected());
        assert_eq!(handle.src_rate(), 44100);

        // 等待认领串流、越过起播水位并真正出声
        thread::sleep(Duration::from_millis(900));
        assert_eq!(handle.mode(), Mode::Streaming, "should be streaming");
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
}
