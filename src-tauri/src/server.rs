use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};

use crate::audio;

pub const MAGIC: &[u8; 4] = b"MSY1";

/// 语音激活阈值(帧内峰值电平 0.0~1.0)
pub const VAD_THRESHOLD: f32 = 0.03;
/// 静音挂起(毫秒):超过该时长无声音 → mic 退出激活,当前串流随之结束
pub const VAD_HANGOVER_MS: u64 = 1500;
/// mic 已激活但还没有客户端认领串流时,保留的起始音频上限(毫秒)
const ONSET_BACKLOG_MS: usize = 300;

/// 语音激活门:峰值超阈值即激活,持续静音超过挂起时长后退出激活
#[derive(Clone)]
pub struct VadGate {
    threshold: f32,
    hangover_ms: u64,
    active: bool,
    last_voice_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VadEvent {
    None,
    Activated,
    Deactivated,
}

impl VadGate {
    pub fn new(threshold: f32, hangover_ms: u64) -> Self {
        Self {
            threshold,
            hangover_ms,
            active: false,
            last_voice_ms: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// 输入一帧的峰值电平与当前时间戳(毫秒),返回状态跃迁
    pub fn update(&mut self, peak: f32, now_ms: u64) -> VadEvent {
        if peak >= self.threshold {
            self.last_voice_ms = now_ms;
            if !self.active {
                self.active = true;
                return VadEvent::Activated;
            }
        } else if self.active && now_ms.saturating_sub(self.last_voice_ms) >= self.hangover_ms {
            self.active = false;
            return VadEvent::Deactivated;
        }
        VadEvent::None
    }
}

/// 激活后、客户端认领前的起始音频缓存(超出上限丢最老,保住第一个字)
struct OnsetBuffer {
    frames: VecDeque<Arc<Vec<i16>>>,
    samples: usize,
    cap_samples: usize,
}

impl OnsetBuffer {
    fn new(cap_samples: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            samples: 0,
            cap_samples,
        }
    }

    fn set_cap(&mut self, cap_samples: usize) {
        self.cap_samples = cap_samples;
    }

    fn push(&mut self, frame: Arc<Vec<i16>>) {
        self.samples += frame.len();
        self.frames.push_back(frame);
        while self.samples > self.cap_samples {
            match self.frames.pop_front() {
                Some(f) => self.samples -= f.len(),
                None => break,
            }
        }
    }

    fn take_all(&mut self) -> Vec<Arc<Vec<i16>>> {
        self.samples = 0;
        self.frames.drain(..).collect()
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.samples = 0;
    }
}

/// 唯一串流槽位:同一时间只允许一个客户端收流
struct StreamSlot {
    tx: SyncSender<Arc<Vec<i16>>>,
    addr: SocketAddr,
}

struct Shared {
    mic_active: AtomicBool,
    level: AtomicU32,
    slot: Mutex<Option<StreamSlot>>,
    onset: Mutex<OnsetBuffer>,
    error: Mutex<Option<String>>,
}

pub struct ServerHandle {
    pub port: u16,
    pub device_name: String,
    pub sample_rate: u32,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl ServerHandle {
    pub fn mic_active(&self) -> bool {
        self.shared.mic_active.load(Ordering::Relaxed)
    }

    /// 当前收流客户端地址;None = 串流槽位空闲
    pub fn stream_addr(&self) -> Option<String> {
        self.shared
            .slot
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
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start(device_name: Option<String>, port: u16) -> Result<ServerHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared {
        mic_active: AtomicBool::new(false),
        level: AtomicU32::new(0),
        slot: Mutex::new(None),
        onset: Mutex::new(OnsetBuffer::new(48_000 * ONSET_BACKLOG_MS / 1000)),
        error: Mutex::new(None),
    });

    // 先绑定端口,失败立刻报错
    let listener = TcpListener::bind(("0.0.0.0", port))
        .map_err(|e| format!("端口 {port} 绑定失败: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("设置监听器失败: {e}"))?;

    // 采集线程:cpal Stream 不是 Send,必须在使用它的线程里创建并驻留
    let (init_tx, init_rx) = sync_channel::<Result<(String, u32), String>>(1);
    {
        let stop = stop.clone();
        let shared = shared.clone();
        thread::Builder::new()
            .name("mic-capture".into())
            .spawn(move || {
                capture_thread(device_name, stop, shared, init_tx);
            })
            .map_err(|e| format!("创建采集线程失败: {e}"))?;
    }

    // 等待采集流真正启动,拿到实际设备名与采样率
    let (actual_device, sample_rate) = init_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "麦克风初始化超时".to_string())?
        .map_err(|e| {
            stop.store(true, Ordering::SeqCst);
            e
        })?;

    // HTTP 接入线程:/health 状态查询 + /stream 音频串流
    {
        let stop = stop.clone();
        let shared = shared.clone();
        let device = actual_device.clone();
        thread::Builder::new()
            .name("mic-http".into())
            .spawn(move || {
                accept_thread(listener, stop, shared, sample_rate, device);
            })
            .map_err(|e| format!("创建监听线程失败: {e}"))?;
    }

    Ok(ServerHandle {
        port,
        device_name: actual_device,
        sample_rate,
        stop,
        shared,
    })
}

fn capture_thread(
    device_name: Option<String>,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
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

    shared
        .onset
        .lock()
        .unwrap()
        .set_cap(sample_rate as usize * ONSET_BACKLOG_MS / 1000);

    let on_mono = {
        let shared = shared.clone();
        let start = Instant::now();
        let mut vad = VadGate::new(VAD_THRESHOLD, VAD_HANGOVER_MS);
        move |mono: Vec<f32>| {
            let peak = audio::peak_level(&mono);
            shared
                .level
                .store(audio::encode_level(peak), Ordering::Relaxed);
            let now_ms = start.elapsed().as_millis() as u64;
            match vad.update(peak, now_ms) {
                VadEvent::Activated => {
                    shared.onset.lock().unwrap().clear();
                }
                VadEvent::Deactivated => {
                    // 静音收槽:丢弃发送端 → 串流线程收到 Disconnected 后结束本轮
                    shared.slot.lock().unwrap().take();
                    shared.onset.lock().unwrap().clear();
                }
                VadEvent::None => {}
            }
            shared.mic_active.store(vad.is_active(), Ordering::Relaxed);
            if !vad.is_active() {
                return;
            }
            let frame = Arc::new(audio::f32_to_i16(&mono));
            let mut slot = shared.slot.lock().unwrap();
            match slot.as_ref() {
                // 有客户端在收流:队列满(客户端太慢)就丢帧,断开则清槽
                Some(s) => match s.tx.try_send(frame) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => *slot = None,
                },
                // 尚无客户端认领:缓存起始音频,等认领时回补
                None => shared.onset.lock().unwrap().push(frame),
            }
        }
    };

    let err_fn = {
        let error = shared.clone();
        move |e: cpal::StreamError| {
            *error.error.lock().unwrap() = Some(format!("麦克风流错误: {e}"));
        }
    };

    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            {
                let mut on_mono = on_mono.clone();
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
                let mut on_mono = on_mono.clone();
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
                let mut on_mono = on_mono.clone();
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

    // 驻留直到停止;stream 随本线程退出而 drop
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(100));
    }
    drop(stream);
    shared.slot.lock().unwrap().take();
    shared.mic_active.store(false, Ordering::Relaxed);
}

fn accept_thread(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    sample_rate: u32,
    device_name: String,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, addr)) => {
                let stop = stop.clone();
                let shared = shared.clone();
                let device = device_name.clone();
                let _ = thread::Builder::new()
                    .name(format!("mic-http-{addr}"))
                    .spawn(move || {
                        handle_conn(stream, addr, stop, shared, sample_rate, &device);
                    });
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
    sample_rate: u32,
    device: &str,
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
            let _ = write_http(&mut stream, 400, "Bad Request", r#"{"error":"bad_request"}"#);
            return;
        }
    };

    match path.as_str() {
        "/health" => {
            let body = serde_json::json!({
                "status": "ok",
                "app": "micsync",
                "sample_rate": sample_rate,
                "device": device,
                "mic_active": shared.mic_active.load(Ordering::Relaxed),
                "streaming": shared.slot.lock().unwrap().is_some(),
            })
            .to_string();
            let _ = write_http(&mut stream, 200, "OK", &body);
        }
        "/stream" => handle_stream(stream, addr, stop, shared, sample_rate),
        _ => {
            let _ = write_http(&mut stream, 404, "Not Found", r#"{"error":"not_found"}"#);
        }
    }
}

fn handle_stream(
    mut stream: TcpStream,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    sample_rate: u32,
) {
    // 认领唯一串流槽位;mic 未激活或槽位已被占都拿不到流
    let rx = {
        if !shared.mic_active.load(Ordering::Relaxed) {
            let _ = write_http(&mut stream, 409, "Conflict", r#"{"error":"mic_idle"}"#);
            return;
        }
        let mut slot = shared.slot.lock().unwrap();
        if slot.is_some() {
            drop(slot);
            let _ = write_http(&mut stream, 409, "Conflict", r#"{"error":"busy"}"#);
            return;
        }
        let (tx, rx) = sync_channel::<Arc<Vec<i16>>>(64);
        // 回补激活起点起缓存的音频,尽量不丢第一个字
        for frame in shared.onset.lock().unwrap().take_all() {
            let _ = tx.try_send(frame);
        }
        *slot = Some(StreamSlot { tx, addr });
        rx
    };

    let handshake = (|| -> std::io::Result<()> {
        stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        )?;
        // 二进制流头: MAGIC(4) + sample_rate u32 LE + channels u16 LE + reserved u16
        let mut header = Vec::with_capacity(12);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&sample_rate.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        stream.write_all(&header)
    })();

    if handshake.is_ok() {
        write_frames(&mut stream, rx, &stop);
    }

    // 清理:仅清除仍属于本连接的槽位(VAD 静音可能已先行收槽)
    let mut slot = shared.slot.lock().unwrap();
    if slot.as_ref().map_or(false, |s| s.addr == addr) {
        *slot = None;
    }
}

/// 帧写循环:VAD 静音收槽(Disconnected)、客户端断开或服务端停止时退出
fn write_frames(stream: &mut TcpStream, rx: Receiver<Arc<Vec<i16>>>, stop: &AtomicBool) {
    let mut buf: Vec<u8> = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(500)) {
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
pub(crate) fn parse_request_path(head: &str) -> Option<String> {
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

    #[test]
    fn vad_gate_transitions() {
        let mut vad = VadGate::new(0.03, 1500);
        // 静音不激活
        assert_eq!(vad.update(0.01, 0), VadEvent::None);
        assert!(!vad.is_active());
        // 超阈值激活
        assert_eq!(vad.update(0.5, 100), VadEvent::Activated);
        assert!(vad.is_active());
        // 挂起期内静音仍保持激活
        assert_eq!(vad.update(0.0, 1000), VadEvent::None);
        assert!(vad.is_active());
        // 期间有声音会刷新挂起计时
        assert_eq!(vad.update(0.2, 1400), VadEvent::None);
        assert_eq!(vad.update(0.0, 2800), VadEvent::None);
        assert!(vad.is_active());
        // 静音超过挂起时长 → 退出激活
        assert_eq!(vad.update(0.0, 2901), VadEvent::Deactivated);
        assert!(!vad.is_active());
        // 再次说话再次激活
        assert_eq!(vad.update(0.9, 3000), VadEvent::Activated);
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

    #[test]
    fn onset_buffer_caps_and_drains() {
        let mut buf = OnsetBuffer::new(100);
        for i in 0..10 {
            buf.push(Arc::new(vec![i as i16; 30]));
        }
        // 上限 100 个采样 → 最多保留 3 帧(90 个采样),最老的被丢掉
        assert!(buf.samples <= 100, "samples={}", buf.samples);
        let frames = buf.take_all();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0][0], 7); // 只剩最新的 7、8、9
        assert_eq!(buf.samples, 0);
        assert!(buf.take_all().is_empty());
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

    fn http_get(addr: SocketAddr, path: &str) -> TcpStream {
        let mut s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n").as_bytes())
            .expect("write req");
        s
    }

    /// 端到端(不含声卡):单一串流槽位、409 互斥、/health 状态、静音收槽断流
    #[test]
    fn single_stream_slot_over_http() {
        let shared = Arc::new(Shared {
            mic_active: AtomicBool::new(true),
            level: AtomicU32::new(0),
            slot: Mutex::new(None),
            onset: Mutex::new(OnsetBuffer::new(4800)),
            error: Mutex::new(None),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        {
            let stop = stop.clone();
            let shared = shared.clone();
            thread::spawn(move || accept_thread(listener, stop, shared, 48000, "fake".into()));
        }

        // 第一个客户端拿到流
        let mut c1 = http_get(addr, "/stream");
        let head1 = read_test_head(&mut c1);
        assert!(head1.starts_with("HTTP/1.1 200"), "{head1}");
        let mut magic = [0u8; 12];
        c1.read_exact(&mut magic).expect("MSY1 header");
        assert_eq!(&magic[0..4], MAGIC);
        assert_eq!(u32::from_le_bytes([magic[4], magic[5], magic[6], magic[7]]), 48000);

        // 第二个客户端被拒:槽位已占
        let mut c2 = http_get(addr, "/stream");
        let head2 = read_test_head(&mut c2);
        assert!(head2.starts_with("HTTP/1.1 409"), "{head2}");

        // /health 报告串流占用中
        let mut c3 = http_get(addr, "/health");
        let mut resp = String::new();
        c3.read_to_string(&mut resp).expect("health resp");
        assert!(resp.contains(r#""streaming":true"#), "{resp}");
        assert!(resp.contains(r#""mic_active":true"#), "{resp}");
        assert!(resp.contains(r#""app":"micsync""#), "{resp}");

        // 通过槽位发一帧,客户端应收到
        {
            let guard = shared.slot.lock().unwrap();
            let tx = guard.as_ref().expect("slot occupied").tx.clone();
            tx.send(Arc::new(vec![7i16; 480])).expect("send frame");
        }
        let mut len_buf = [0u8; 4];
        c1.read_exact(&mut len_buf).expect("frame len");
        assert_eq!(u32::from_le_bytes(len_buf), 480);
        let mut frame = vec![0u8; 960];
        c1.read_exact(&mut frame).expect("frame body");
        assert_eq!(i16::from_le_bytes([frame[0], frame[1]]), 7);

        // 模拟 VAD 静音收槽 → 服务端关闭连接,客户端读到 EOF
        shared.slot.lock().unwrap().take();
        shared.mic_active.store(false, Ordering::Relaxed);
        let n = c1.read(&mut len_buf).expect("read after slot close");
        assert_eq!(n, 0, "stream should be closed after slot released");

        // mic 未激活时请求流 → 409 mic_idle
        let mut c4 = http_get(addr, "/stream");
        let head4 = read_test_head(&mut c4);
        assert!(head4.starts_with("HTTP/1.1 409"), "{head4}");

        stop.store(true, Ordering::SeqCst);
    }
}
