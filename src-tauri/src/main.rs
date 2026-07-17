#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod client;
mod follow;
mod micuse;
mod server;

use std::sync::Mutex;

use serde::Serialize;
use tauri::State;

pub const DEFAULT_PORT: u16 = 47800;

#[derive(Default)]
struct AppState {
    server: Mutex<Option<server::ServerHandle>>,
    /// 自动启动监听失败的原因(端口被占等),供 UI 展示
    server_error: Mutex<Option<String>>,
    client: Mutex<Option<client::ClientHandle>>,
    follow: Mutex<Option<follow::FollowHandle>>,
}

#[derive(Serialize)]
struct Devices {
    inputs: Vec<String>,
    outputs: Vec<String>,
    blackhole_installed: bool,
}

#[tauri::command]
fn list_devices() -> Devices {
    let outputs = audio::output_device_names();
    let blackhole_installed = outputs.iter().any(|n| n.contains("BlackHole"));
    Devices {
        inputs: audio::input_device_names(),
        outputs,
        blackhole_installed,
    }
}

/// 用系统默认浏览器打开 BlackHole 官网下载页(固定 URL,不接受任意地址)
#[tauri::command]
fn open_blackhole_download() -> Result<(), String> {
    std::process::Command::new("open")
        .arg("https://existential.audio/blackhole/")
        .spawn()
        .map_err(|e| format!("打开浏览器失败: {e}"))?;
    Ok(())
}

#[tauri::command]
fn local_ips() -> Vec<String> {
    match local_ip_address::list_afinet_netifas() {
        Ok(ifas) => ifas
            .into_iter()
            .filter(|(name, ip)| {
                ip.is_ipv4()
                    && !ip.is_loopback()
                    && !name.starts_with("utun")
                    && !name.starts_with("awdl")
                    && !name.starts_with("llw")
                    && !name.starts_with("bridge")
            })
            .map(|(_, ip)| ip.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[derive(Serialize)]
struct ServerStatus {
    running: bool,
    port: u16,
    /// 设备偏好或最近一次实际采集的设备
    device: String,
    /// 最近一次采集的采样率;0 = 还没有客户端用过
    sample_rate: u32,
    /// 当前收流客户端地址;空串 = 麦克风空闲(未开麦)
    stream_addr: String,
    level: f32,
    error: Option<String>,
}

/// (重新)启动 API 监听;麦克风此时不开启,等客户端请求才按需采集
#[tauri::command]
fn start_server(
    state: State<AppState>,
    device: Option<String>,
    port: Option<u16>,
) -> Result<ServerStatus, String> {
    let mut guard = state.server.lock().unwrap();
    if let Some(old) = guard.take() {
        old.stop();
    }
    let handle = server::start(device, port.unwrap_or(DEFAULT_PORT))?;
    *state.server_error.lock().unwrap() = None;
    let status = ServerStatus {
        running: true,
        port: handle.port,
        device: handle.device(),
        sample_rate: handle.sample_rate(),
        stream_addr: String::new(),
        level: 0.0,
        error: None,
    };
    *guard = Some(handle);
    Ok(status)
}

#[tauri::command]
fn stop_server(state: State<AppState>) {
    let mut guard = state.server.lock().unwrap();
    if let Some(handle) = guard.take() {
        handle.stop();
    }
}

/// 更换共享的麦克风设备,对下一次串流会话生效
#[tauri::command]
fn set_input_device(state: State<AppState>, device: Option<String>) {
    if let Some(h) = state.server.lock().unwrap().as_ref() {
        h.set_device(device);
    }
}

#[tauri::command]
fn server_status(state: State<AppState>) -> ServerStatus {
    let guard = state.server.lock().unwrap();
    match guard.as_ref() {
        Some(h) => ServerStatus {
            running: true,
            port: h.port,
            device: h.device(),
            sample_rate: h.sample_rate(),
            stream_addr: h.stream_addr().unwrap_or_default(),
            level: h.level(),
            error: h.take_error(),
        },
        None => ServerStatus {
            running: false,
            port: DEFAULT_PORT,
            device: String::new(),
            sample_rate: 0,
            stream_addr: String::new(),
            level: 0.0,
            error: state.server_error.lock().unwrap().clone(),
        },
    }
}

#[derive(Serialize)]
struct ClientStatus {
    connected: bool,
    /// 运行模式: "connecting" 连接/重连中 / "streaming" 正在收流 / "ended" 已结束(被接管或服务端停止)
    mode: String,
    addr: String,
    output_device: String,
    sample_rate: u32,
    buffer_ms: u32,
    level: f32,
    error: Option<String>,
}

fn make_client_status(h: Option<&client::ClientHandle>) -> ClientStatus {
    match h {
        Some(h) => ClientStatus {
            connected: h.is_connected(),
            mode: h.mode().as_str().into(),
            addr: h.addr.clone(),
            output_device: h.output_device.clone(),
            sample_rate: h.src_rate(),
            buffer_ms: h.buffer_ms(),
            level: h.level(),
            error: h.take_error(),
        },
        None => ClientStatus {
            connected: false,
            mode: "ended".into(),
            addr: String::new(),
            output_device: String::new(),
            sample_rate: 0,
            buffer_ms: 0,
            level: 0.0,
            error: None,
        },
    }
}

/// 手动使用远端麦克风(与自动跟随互斥,先停掉跟随)
#[tauri::command]
fn connect_client(
    state: State<AppState>,
    addr: String,
    output_device: Option<String>,
) -> Result<ClientStatus, String> {
    if let Some(f) = state.follow.lock().unwrap().take() {
        f.stop();
    }
    let mut guard = state.client.lock().unwrap();
    if let Some(old) = guard.take() {
        old.stop();
    }
    let handle = client::connect(addr, output_device)?;
    let status = make_client_status(Some(&handle));
    *guard = Some(handle);
    Ok(status)
}

#[tauri::command]
fn disconnect_client(state: State<AppState>) {
    let mut guard = state.client.lock().unwrap();
    if let Some(handle) = guard.take() {
        handle.stop();
    }
}

#[tauri::command]
fn client_status(state: State<AppState>) -> ClientStatus {
    make_client_status(state.client.lock().unwrap().as_ref())
}

#[derive(Serialize)]
struct FollowStatus {
    running: bool,
    /// "armed" 待命 / "active" 使用中 / "suppressed" 被接管抑制 /
    /// "device_missing" 找不到 BlackHole / "unsupported" 系统不支持
    phase: String,
    /// 本机是否有应用正在从 BlackHole 采集
    local_in_use: bool,
    addr: String,
    output_device: String,
    error: Option<String>,
    /// 内层认领会话的状态(未认领时 connected=false)
    client: ClientStatus,
}

/// 开启自动跟随:检测到本机应用使用 BlackHole 时自动认领远端麦克风
#[tauri::command]
fn start_follow(
    state: State<AppState>,
    addr: String,
    output_device: Option<String>,
) -> Result<FollowStatus, String> {
    // 与手动模式互斥
    if let Some(c) = state.client.lock().unwrap().take() {
        c.stop();
    }
    let mut guard = state.follow.lock().unwrap();
    if let Some(old) = guard.take() {
        old.stop();
    }
    // 未指定则优先选 BlackHole
    let device_name = match output_device {
        Some(n) if !n.is_empty() => n,
        _ => audio::resolve_output_name(None).ok_or_else(|| "找不到可用的输出设备".to_string())?,
    };
    let handle = follow::start(addr, device_name)?;
    let status = FollowStatus {
        running: true,
        phase: handle.phase().as_str().into(),
        local_in_use: false,
        addr: handle.addr.clone(),
        output_device: handle.output_device.clone(),
        error: None,
        client: make_client_status(None),
    };
    *guard = Some(handle);
    Ok(status)
}

#[tauri::command]
fn stop_follow(state: State<AppState>) {
    let mut guard = state.follow.lock().unwrap();
    if let Some(handle) = guard.take() {
        handle.stop();
    }
}

#[tauri::command]
fn follow_status(state: State<AppState>) -> FollowStatus {
    let guard = state.follow.lock().unwrap();
    match guard.as_ref() {
        Some(h) => FollowStatus {
            running: true,
            phase: h.phase().as_str().into(),
            local_in_use: h.local_in_use(),
            addr: h.addr.clone(),
            output_device: h.output_device.clone(),
            error: h.take_error(),
            client: h.with_client(make_client_status),
        },
        None => FollowStatus {
            running: false,
            phase: "armed".into(),
            local_in_use: false,
            addr: String::new(),
            output_device: String::new(),
            error: None,
            client: make_client_status(None),
        },
    }
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            // 应用启动即自动监听 API(真正的事件驱动:此时不碰麦克风,
            // 只有客户端请求 /stream 才按需开麦)
            use tauri::Manager;
            let state = app.state::<AppState>();
            match server::start(None, DEFAULT_PORT) {
                Ok(handle) => *state.server.lock().unwrap() = Some(handle),
                Err(e) => *state.server_error.lock().unwrap() = Some(e),
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_devices,
            local_ips,
            open_blackhole_download,
            start_server,
            stop_server,
            set_input_device,
            server_status,
            connect_client,
            disconnect_client,
            client_status,
            start_follow,
            stop_follow,
            follow_status,
        ])
        .run(tauri::generate_context!())
        .expect("MicSync 启动失败");
}
