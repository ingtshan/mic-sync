mod audio;
#[cfg(not(target_os = "ios"))]
mod client;
mod discovery;
#[cfg(not(target_os = "ios"))]
mod follow;
#[cfg(target_os = "ios")]
mod ios;
#[cfg(not(target_os = "ios"))]
mod micuse;
mod server;
mod settings;

use std::sync::Mutex;

use serde::Serialize;
use tauri::State;

pub const DEFAULT_PORT: u16 = 47800;

#[derive(Default)]
struct AppState {
    server: Mutex<Option<server::ServerHandle>>,
    /// 自动启动监听失败的原因(端口被占等),供 UI 展示
    server_error: Mutex<Option<String>>,
    /// 组播应答器,与服务监听同生命周期:暂停服务 = 不再被发现
    responder: Mutex<Option<discovery::ResponderHandle>>,
    #[cfg(not(target_os = "ios"))]
    client: Mutex<Option<client::ClientHandle>>,
    #[cfg(not(target_os = "ios"))]
    follow: Mutex<Option<follow::FollowHandle>>,
}

#[derive(Serialize)]
struct Devices {
    inputs: Vec<String>,
    outputs: Vec<String>,
    /// 虚拟回环声卡(macOS: BlackHole / Windows: VB-Cable)是否已安装
    virtual_mic_installed: bool,
}

#[tauri::command]
fn list_devices() -> Devices {
    let outputs = audio::output_device_names();
    let virtual_mic_installed = outputs.iter().any(|n| audio::is_virtual_mic_output(n));
    Devices {
        inputs: audio::input_device_names(),
        outputs,
        virtual_mic_installed,
    }
}

/// 前端据此区分桌面/iOS 形态("macos" / "ios" / ...)
#[tauri::command]
fn platform() -> &'static str {
    std::env::consts::OS
}

/// 用系统默认浏览器打开虚拟声卡驱动官网下载页(固定 URL,不接受任意地址):
/// macOS 为 BlackHole,Windows 为 VB-Cable
#[cfg(not(target_os = "ios"))]
#[tauri::command]
fn open_driver_download() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open")
        .arg("https://existential.audio/blackhole/")
        .spawn();
    #[cfg(target_os = "windows")]
    let spawned = {
        // CREATE_NO_WINDOW:避免闪出一个 cmd 控制台窗口
        use std::os::windows::process::CommandExt;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", "https://vb-audio.com/Cable/"])
            .creation_flags(0x0800_0000)
            .spawn()
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let spawned = std::process::Command::new("xdg-open")
        .arg("https://existential.audio/blackhole/")
        .spawn();
    spawned.map_err(|e| format!("打开浏览器失败: {e}"))?;
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
                    // iOS 蜂窝数据接口,客户端连不进来
                    && !name.starts_with("pdp_ip")
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
    /// 最近 ~1.5 秒每块音频(~10ms)的峰值序列(0~1),移动端真实波形 UI 用
    wave: Vec<f32>,
    /// wave 的累计块计数,前端据此对齐两次轮询之间的新增部分
    wave_seq: u64,
    /// 正在等本机用户确认的授权请求;None = 没有
    pending: Option<PendingRequest>,
    error: Option<String>,
}

/// 一个等待确认的授权请求(谁想用本机麦克风)
#[derive(Serialize)]
struct PendingRequest {
    /// 对方自报的设备名——不可信输入,后端已滤掉控制字符并限长
    name: String,
    addr: String,
    /// "stream" 同意后立即开麦;"authorize" 自动跟随的预授权(用到时才开麦)
    kind: String,
}

fn pending_of(h: &server::ServerHandle) -> Option<PendingRequest> {
    h.pending().map(|(name, addr, kind)| PendingRequest {
        name,
        addr,
        kind: kind.as_str().into(),
    })
}

/// 启动组播应答器,让客户端能搜到本机。失败不算错误——组播被网络禁掉、
/// 端口被占、iOS 没有组播授权都会走到这里,客户端的子网扫描仍能发现我们
fn arm_responder(state: &AppState, port: u16) {
    let mut guard = state.responder.lock().unwrap();
    if let Some(old) = guard.take() {
        old.stop();
    }
    match discovery::start_responder(port) {
        Ok(h) => *guard = Some(h),
        Err(e) => eprintln!("组播发现不可用(客户端仍可通过子网扫描找到本机): {e}"),
    }
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
    arm_responder(&state, handle.port);
    *state.server_error.lock().unwrap() = None;
    // 用户主动开了服务:记下来,下次启动自动恢复监听
    settings::set_server_was_running(true);
    let status = ServerStatus {
        running: true,
        port: handle.port,
        device: handle.device(),
        sample_rate: handle.sample_rate(),
        stream_addr: String::new(),
        level: 0.0,
        wave: Vec::new(),
        wave_seq: 0,
        pending: None,
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
    // 服务停了就不该再应答发现查询,否则客户端会连到一个不听的端口
    if let Some(r) = state.responder.lock().unwrap().take() {
        r.stop();
    }
    // 用户主动暂停了服务:下次启动尊重这个选择,不自动开回来
    settings::set_server_was_running(false);
}

/// 搜索局域网内的 MicSync 服务端,供客户端一键填入地址。
/// 组播查询与 /health 子网扫描并发跑,约 1.5 秒返回合并结果
#[cfg(not(target_os = "ios"))]
#[tauri::command]
async fn discover_servers(port: Option<u16>) -> Vec<discovery::Peer> {
    let port = port.unwrap_or(DEFAULT_PORT);
    // 发现是阻塞的(线程扫描 + UDP 等待),挪到阻塞线程池别卡住异步运行时
    tauri::async_runtime::spawn_blocking(move || discovery::discover(port))
        .await
        .unwrap_or_default()
}

#[derive(Serialize)]
struct DeviceInfo {
    name: String,
    device_type: String,
}

/// 本机设备名与类型(前端展示与编辑用)
#[tauri::command]
fn device_info() -> DeviceInfo {
    DeviceInfo {
        name: settings::device_name(),
        device_type: discovery::device_type().into(),
    }
}

/// 改设备名(对方在授权弹窗里看到的就是它);返回清洗后的实际值
#[tauri::command]
fn set_device_name(name: String) -> String {
    settings::set_device_name(&name)
}

/// 上次退出时的界面模式("server"/"client");None = 新装用户。
/// 前端启动时据此恢复到用户离开时的 tab
#[tauri::command]
fn last_mode() -> Option<String> {
    settings::last_mode()
}

/// 前端在用户切换模式时调用,随切随记——退出时无需钩子,记录天然是最后状态
#[tauri::command]
fn set_last_mode(mode: String) {
    settings::set_last_mode(&mode);
}

/// 裁决当前的授权请求:allow=false 拒绝;remember=true 记住这台设备(签发令牌)
#[tauri::command]
fn decide_request(state: State<AppState>, allow: bool, remember: bool) {
    let decision = match (allow, remember) {
        (false, _) => server::Decision::Deny,
        (true, true) => server::Decision::Always,
        (true, false) => server::Decision::Once,
    };
    if let Some(h) = state.server.lock().unwrap().as_ref() {
        h.decide(decision);
    }
}

/// 已授权(始终允许)的客户端列表
#[tauri::command]
fn trusted_devices() -> Vec<settings::Trusted> {
    settings::trusted_list()
}

/// 撤销某台设备的长期授权,它下次再来会重新弹确认
#[tauri::command]
fn revoke_trusted(token: String) {
    settings::revoke_trusted(&token);
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
        Some(h) => {
            let (wave, wave_seq) = h.wave();
            ServerStatus {
                running: true,
                port: h.port,
                device: h.device(),
                sample_rate: h.sample_rate(),
                stream_addr: h.stream_addr().unwrap_or_default(),
                level: h.level(),
                wave,
                wave_seq,
                pending: pending_of(h),
                error: h.take_error(),
            }
        }
        None => ServerStatus {
            running: false,
            port: DEFAULT_PORT,
            device: String::new(),
            sample_rate: 0,
            stream_addr: String::new(),
            level: 0.0,
            wave: Vec::new(),
            wave_seq: 0,
            pending: None,
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

#[cfg(not(target_os = "ios"))]
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
#[cfg(not(target_os = "ios"))]
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

#[cfg(not(target_os = "ios"))]
#[tauri::command]
fn disconnect_client(state: State<AppState>) {
    let mut guard = state.client.lock().unwrap();
    if let Some(handle) = guard.take() {
        handle.stop();
    }
}

#[cfg(not(target_os = "ios"))]
#[tauri::command]
fn client_status(state: State<AppState>) -> ClientStatus {
    make_client_status(state.client.lock().unwrap().as_ref())
}

#[derive(Serialize)]
struct FollowStatus {
    running: bool,
    /// "authorizing" 启动预授权中 / "auth_denied" 预授权被拒(终态) /
    /// "armed" 待命 / "active" 使用中 / "suppressed" 被接管抑制 /
    /// "draining" 本轮结束后排空 / "device_missing" 找不到虚拟声卡 /
    /// "unsupported" 系统不支持
    phase: String,
    /// 本机是否有应用正在从虚拟声卡采集
    local_in_use: bool,
    addr: String,
    output_device: String,
    error: Option<String>,
    /// 内层认领会话的状态(未认领时 connected=false)
    client: ClientStatus,
}

/// 开启自动跟随:检测到本机应用使用虚拟声卡时自动认领远端麦克风
#[cfg(not(target_os = "ios"))]
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
    // 未指定则优先选虚拟回环声卡
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

#[cfg(not(target_os = "ios"))]
#[tauri::command]
fn stop_follow(state: State<AppState>) {
    let mut guard = state.follow.lock().unwrap();
    if let Some(handle) = guard.take() {
        handle.stop();
    }
}

#[cfg(not(target_os = "ios"))]
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

/// 唤回主窗口(macOS 上同时恢复 Dock 图标,与后台 Accessory 状态对应)
#[cfg(desktop)]
fn show_main_window(app: &tauri::AppHandle) {
    use tauri::Manager;
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// 托盘菜单。pending = 待确认请求的设备名:有的话在最上面加一个直达入口,
/// 常驻托盘的用户点一下就能到确认弹窗
#[cfg(target_os = "macos")]
fn tray_menu(
    app: &tauri::AppHandle,
    pending: Option<&str>,
) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    let show = MenuItem::with_id(app, "show", "打开 MicSync", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出 MicSync", true, None::<&str>)?;
    match pending {
        Some(name) => {
            let attend = MenuItem::with_id(
                app,
                "pending",
                format!("处理「{name}」的请求…"),
                true,
                None::<&str>,
            )?;
            Menu::with_items(
                app,
                &[
                    &attend,
                    &PredefinedMenuItem::separator(app)?,
                    &show,
                    &PredefinedMenuItem::separator(app)?,
                    &quit,
                ],
            )
        }
        None => Menu::with_items(app, &[&show, &PredefinedMenuItem::separator(app)?, &quit]),
    }
}

/// macOS:菜单栏托盘,关窗后台运行时的唯一管理入口
#[cfg(target_os = "macos")]
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::tray::TrayIconBuilder;
    let menu = tray_menu(app.handle(), None)?;
    // 单色线条模板图标(SF Symbol "mic" 渲染),as_template 让系统
    // 按菜单栏深浅色自动着色,与原生菜单栏图标同一视觉语言
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .icon_as_template(true)
        .tooltip("MicSync")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" | "pending" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// 托盘注意态:有待确认请求时换成带圆点徽标的图标、tooltip 点名、
/// 菜单加直达入口;处理完复原。托盘与菜单是 AppKit 对象,必须在主线程碰
#[cfg(target_os = "macos")]
fn set_tray_attention(app: &tauri::AppHandle, pending: Option<String>) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        let Some(tray) = app.tray_by_id("main") else {
            return;
        };
        let bytes: &[u8] = if pending.is_some() {
            include_bytes!("../icons/tray-attention.png")
        } else {
            include_bytes!("../icons/tray.png")
        };
        if let Ok(icon) = tauri::image::Image::from_bytes(bytes) {
            let _ = tray.set_icon(Some(icon));
            let _ = tray.set_icon_as_template(true);
        }
        let _ = tray.set_tooltip(Some(match &pending {
            Some(name) => format!("MicSync — 「{name}」等待确认"),
            None => "MicSync".into(),
        }));
        if let Ok(menu) = tray_menu(&app, pending.as_deref()) {
            let _ = tray.set_menu(Some(menu));
        }
    });
}

/// 系统通知:主窗聚焦时弹窗本身就是提醒,不发;其余情况都发——
/// 彻底隐藏(常驻托盘)自不必说,「可见但未聚焦」同样看不见弹窗:窗口被全屏
/// 会议挡住、在别的桌面/屏幕上,且 macOS 会节流被遮挡 WebView 的定时器,
/// 弹窗根本刷不出来。之前只判 is_visible 漏掉了这一大类,用户毫无感知。
/// 通知权限第一次真正要用时才申请,不在启动时骚扰;被拒绝就靠托盘徽标兜底
#[cfg(desktop)]
fn notify_pending(app: &tauri::AppHandle, name: &str, kind: &str) {
    use tauri::Manager as _;
    use tauri_plugin_notification::{NotificationExt, PermissionState};
    let win = app.get_webview_window("main");
    let visible = win
        .as_ref()
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    let focused = win
        .as_ref()
        .and_then(|w| w.is_focused().ok())
        .unwrap_or(false);
    if visible && focused {
        return;
    }
    let n = app.notification();
    let granted = matches!(n.permission_state(), Ok(PermissionState::Granted))
        || matches!(n.request_permission(), Ok(PermissionState::Granted));
    if !granted {
        return;
    }
    let body = if kind == "authorize" {
        "对方开启了自动跟随,请求预先授权;同意之前不会开麦,不急。点击菜单栏的 MicSync 图标处理。"
    } else {
        "30 秒内未处理将自动拒绝。点击菜单栏的 MicSync 图标处理。"
    };
    let _ = n
        .builder()
        .title(format!("「{name}」想使用这台设备的麦克风"))
        .body(body)
        .show();
}

/// 待确认请求的哨兵:服务端常驻托盘时,同意弹窗渲染在隐藏窗口里,用户毫无
/// 感知(macOS 还会节流隐藏 WebView 的定时器,前端轮询靠不住)。这里在 Rust
/// 侧轮询 pending,出现沿换托盘徽标 + 发系统通知,消失沿复原。
/// 轮询而不是回调:server 模块不该依赖 tauri
#[cfg(desktop)]
fn spawn_pending_watcher(app: tauri::AppHandle) {
    use tauri::Manager as _;
    let _ = std::thread::Builder::new()
        .name("pending-watch".into())
        .spawn(move || {
            let mut last: Option<(String, String)> = None;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let now = {
                    let state = app.state::<AppState>();
                    let guard = state.server.lock().unwrap();
                    guard
                        .as_ref()
                        .and_then(|h| h.pending())
                        .map(|(name, _addr, kind)| (name, kind.as_str().to_string()))
                };
                if now == last {
                    continue;
                }
                #[cfg(target_os = "macos")]
                set_tray_attention(&app, now.as_ref().map(|(n, _)| n.clone()));
                if let Some((name, kind)) = &now {
                    notify_pending(&app, name, kind);
                }
                last = now;
            }
        });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    // 单实例守护(桌面端):关窗后 App 驻留托盘,此时再次启动(尤其是另一份
    // .app 拷贝)会出现两个实例的自动跟随互相抢占麦克风、界面谎报"被其他
    // 设备接管"。二次启动一律只唤回已有实例的窗口
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        show_main_window(app);
    }));
    // 系统通知:待确认请求出现而主窗又不可见时,靠它提醒用户
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_notification::init());
    let builder = builder
        .manage(AppState::default())
        .setup(|app| {
            use tauri::Manager;
            // 先落定配置目录:设备名与授权列表都要持久化,晚于任何 settings 调用
            // 就会先用上内存默认值,重启后授权全丢
            match app.path().app_config_dir() {
                Ok(dir) => settings::init(dir),
                Err(e) => eprintln!("取应用配置目录失败,设备名与授权将无法保存: {e}"),
            }
            // iOS:先配好 AVAudioSession 并申请录音权限,cpal 采集才有输入
            #[cfg(target_os = "ios")]
            ios::setup_audio_session();
            #[cfg(target_os = "macos")]
            setup_tray(app)?;
            // 待确认请求的托盘徽标与系统通知(常驻托盘时弹窗没人看得见)
            #[cfg(desktop)]
            spawn_pending_watcher(app.handle().clone());
            // 恢复上次退出时的形态:上次服务开着(且不是以客户端模式退出)
            // 才自动监听。新装用户不自动开服务——装上就默默被局域网发现,
            // 对只想用别人麦克风的用户是意外行为。监听依旧不碰麦克风,
            // 只有客户端请求 /stream 才按需开麦
            let restore_server = settings::server_was_running()
                && settings::last_mode().as_deref() != Some("client");
            if restore_server {
                let state = app.state::<AppState>();
                match server::start(None, DEFAULT_PORT) {
                    Ok(handle) => {
                        arm_responder(&state, handle.port);
                        *state.server.lock().unwrap() = Some(handle);
                    }
                    Err(e) => *state.server_error.lock().unwrap() = Some(e),
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // macOS:关闭窗口只是隐藏,采集/串流继续后台运行;
            // 同时切到 Accessory 隐藏 Dock 图标,只留菜单栏托盘
            #[cfg(target_os = "macos")]
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                use tauri::Manager;
                api.prevent_close();
                let _ = window.hide();
                let _ = window
                    .app_handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            #[cfg(not(target_os = "macos"))]
            let _ = (window, event);
        });

    #[cfg(not(target_os = "ios"))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        list_devices,
        platform,
        local_ips,
        open_driver_download,
        device_info,
        set_device_name,
        last_mode,
        set_last_mode,
        decide_request,
        trusted_devices,
        revoke_trusted,
        start_server,
        stop_server,
        set_input_device,
        server_status,
        discover_servers,
        connect_client,
        disconnect_client,
        client_status,
        start_follow,
        stop_follow,
        follow_status,
    ]);
    // iOS 只有服务端角色:没有虚拟声卡,客户端/自动跟随不可用
    #[cfg(target_os = "ios")]
    let builder = builder.invoke_handler(tauri::generate_handler![
        list_devices,
        platform,
        local_ips,
        device_info,
        set_device_name,
        last_mode,
        set_last_mode,
        decide_request,
        trusted_devices,
        revoke_trusted,
        start_server,
        stop_server,
        set_input_device,
        server_status,
    ]);

    let app = builder
        .build(tauri::generate_context!())
        .expect("MicSync 启动失败");
    app.run(|_app, _event| {
        // macOS:后台运行时从 Finder/启动台再次打开应用,唤回隐藏的主窗口
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = _event {
            show_main_window(_app);
        }
    });
}
