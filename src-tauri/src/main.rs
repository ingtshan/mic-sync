#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod client;
mod server;

use std::sync::Mutex;

use serde::Serialize;
use tauri::State;

pub const DEFAULT_PORT: u16 = 47800;

#[derive(Default)]
struct AppState {
    server: Mutex<Option<server::ServerHandle>>,
    client: Mutex<Option<client::ClientHandle>>,
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
    device: String,
    sample_rate: u32,
    /// 语音激活状态(VAD)
    mic_active: bool,
    /// 当前收流客户端地址;空串 = 串流槽位空闲
    stream_addr: String,
    level: f32,
    error: Option<String>,
}

#[tauri::command]
fn start_server(
    state: State<AppState>,
    device: Option<String>,
    port: Option<u16>,
) -> Result<ServerStatus, String> {
    let mut guard = state.server.lock().unwrap();
    if guard.is_some() {
        return Err("服务端已在运行".into());
    }
    let handle = server::start(device, port.unwrap_or(DEFAULT_PORT))?;
    let status = ServerStatus {
        running: true,
        port: handle.port,
        device: handle.device_name.clone(),
        sample_rate: handle.sample_rate,
        mic_active: false,
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

#[tauri::command]
fn server_status(state: State<AppState>) -> ServerStatus {
    let guard = state.server.lock().unwrap();
    match guard.as_ref() {
        Some(h) => ServerStatus {
            running: true,
            port: h.port,
            device: h.device_name.clone(),
            sample_rate: h.sample_rate,
            mic_active: h.mic_active(),
            stream_addr: h.stream_addr().unwrap_or_default(),
            level: h.level(),
            error: h.take_error(),
        },
        None => ServerStatus {
            running: false,
            port: DEFAULT_PORT,
            device: String::new(),
            sample_rate: 0,
            mic_active: false,
            stream_addr: String::new(),
            level: 0.0,
            error: None,
        },
    }
}

#[derive(Serialize)]
struct ClientStatus {
    connected: bool,
    /// 运行模式: "offline" 离线重试 / "standby" 待机轮询 / "streaming" 正在收流
    mode: String,
    addr: String,
    output_device: String,
    sample_rate: u32,
    buffer_ms: u32,
    level: f32,
    error: Option<String>,
}

#[tauri::command]
fn connect_client(
    state: State<AppState>,
    addr: String,
    output_device: Option<String>,
) -> Result<ClientStatus, String> {
    let mut guard = state.client.lock().unwrap();
    if let Some(old) = guard.take() {
        old.stop();
    }
    let handle = client::connect(addr, output_device)?;
    let status = ClientStatus {
        connected: true,
        mode: handle.mode().as_str().into(),
        addr: handle.addr.clone(),
        output_device: handle.output_device.clone(),
        sample_rate: handle.src_rate(),
        buffer_ms: 0,
        level: 0.0,
        error: None,
    };
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
    let guard = state.client.lock().unwrap();
    match guard.as_ref() {
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
            mode: "offline".into(),
            addr: String::new(),
            output_device: String::new(),
            sample_rate: 0,
            buffer_ms: 0,
            level: 0.0,
            error: None,
        },
    }
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            list_devices,
            local_ips,
            open_blackhole_download,
            start_server,
            stop_server,
            server_status,
            connect_client,
            disconnect_client,
            client_status,
        ])
        .run(tauri::generate_context!())
        .expect("MicSync 启动失败");
}
