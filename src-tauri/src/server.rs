use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};

use crate::audio;

pub const MAGIC: &[u8; 4] = b"MSY1";

/// 优雅结束帧(0 长度帧)之后的原因码
pub const END_SERVER_CLOSING: u32 = 0;
pub const END_PREEMPTED: u32 = 1;

/// 采集启动器:为一次串流会话按需启动 mic 采集(采集流驻留在自己的线程里,
/// 停止信号置位后释放麦克风),返回 (实际设备名, 采样率)。
/// 抽象成函数指针以便测试注入假采集,不依赖真实声卡。
type CaptureFn = fn(
    Option<String>,             // 设备偏好
    Arc<AtomicBool>,            // 本会话采集停止信号
    SyncSender<Arc<Vec<i16>>>,  // 帧输出
    Arc<AtomicU32>,             // 电平上报
) -> Result<(String, u32), String>;

/// 当前串流会话;同一时间最多一个,新请求会接管(抢占)旧会话
struct Session {
    id: u64,
    addr: SocketAddr,
    /// 置位 = 被新会话抢占,旧 writer 应发结束帧并收尾
    end: Arc<AtomicBool>,
}

struct Shared {
    /// UI 选择的麦克风设备,下一次采集生效
    device_pref: Mutex<Option<String>>,
    session: Mutex<Option<Session>>,
    next_id: AtomicU64,
    level: Arc<AtomicU32>,
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
        audio::decode_level(self.shared.level.load(Ordering::Relaxed))
    }

    pub fn take_error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // 让在场会话尽快发结束帧收尾
        if let Some(s) = self.shared.session.lock().unwrap().as_ref() {
            s.end.store(true, Ordering::SeqCst);
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
        next_id: AtomicU64::new(1),
        level: Arc::new(AtomicU32::new(0)),
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

fn handle_conn(mut stream: TcpStream, addr: SocketAddr, stop: Arc<AtomicBool>, shared: Arc<Shared>) {
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
            let _ = write_http(&mut stream, 400, "Bad Request", r#"{"error":"bad_request"}"#);
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
            let body = serde_json::json!({
                "status": "ok",
                "app": "micsync",
                "streaming": streaming,
                "client": client,
                "sample_rate": shared.last_rate.load(Ordering::Relaxed),
            })
            .to_string();
            let _ = write_http(&mut stream, 200, "OK", &body);
            graceful_close(stream);
        }
        "/stream" => handle_stream(stream, addr, stop, shared),
        _ => {
            let _ = write_http(&mut stream, 404, "Not Found", r#"{"error":"not_found"}"#);
            graceful_close(stream);
        }
    }
}

fn handle_stream(mut stream: TcpStream, addr: SocketAddr, stop: Arc<AtomicBool>, shared: Arc<Shared>) {
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
    let init = (shared.capture)(pref, cap_stop.clone(), tx, shared.level.clone());

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
        stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
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

    // 会话收尾:关麦克风、清电平、释放会话(可能已被新会话顶替)
    cap_stop.store(true, Ordering::SeqCst);
    shared.level.store(0, Ordering::Relaxed);
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
    level: Arc<AtomicU32>,
) -> Result<(String, u32), String> {
    let (init_tx, init_rx) = sync_channel::<Result<(String, u32), String>>(1);
    thread::Builder::new()
        .name("mic-capture".into())
        .spawn(move || capture_thread(device_pref, stop, tx, level, init_tx))
        .map_err(|e| format!("创建采集线程失败: {e}"))?;
    init_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "麦克风初始化超时".to_string())?
}

fn capture_thread(
    device_name: Option<String>,
    stop: Arc<AtomicBool>,
    tx: SyncSender<Arc<Vec<i16>>>,
    level: Arc<AtomicU32>,
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
            level.store(audio::encode_level(audio::peak_level(&mono)), Ordering::Relaxed);
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

fn write_http(stream: &mut TcpStream, code: u16, reason: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
        level: Arc<AtomicU32>,
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
                level.store(500, Ordering::Relaxed);
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
        _level: Arc<AtomicU32>,
    ) -> Result<(String, u32), String> {
        Err("假装麦克风坏了".into())
    }

    fn http_get(port: u16, path: &str) -> TcpStream {
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        // 放宽超时:并行套件冷启动时 CPU 争抢可能让响应偶发超过 2s
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .expect("write req");
        s
    }

    fn read_test_head(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read head");
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
        assert_eq!(ACTIVE_CAPTURES.load(Ordering::SeqCst), 0, "mic must be off when idle");
        let mut c = http_get(port, "/health");
        let mut resp = String::new();
        c.read_to_string(&mut resp).expect("health");
        assert!(resp.contains(r#""streaming":false"#), "{resp}");

        // 第一个客户端请求 → 自动开麦并串流
        let mut c1 = http_get(port, "/stream");
        let head1 = read_test_head(&mut c1);
        assert!(head1.starts_with("HTTP/1.1 200"), "{head1}");
        let mut magic = [0u8; 12];
        c1.read_exact(&mut magic).expect("MSY1 header");
        assert_eq!(&magic[0..4], MAGIC);
        assert_eq!(u32::from_le_bytes([magic[4], magic[5], magic[6], magic[7]]), 44100);
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
        let mut c2 = http_get(port, "/stream");
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
        let mut c1 = http_get(handle.port, "/stream");
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
        let mut c1 = http_get(handle.port, "/stream");
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
}
