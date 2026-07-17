//! 设备身份、可改设备名与信任列表(持久化到应用配置目录)。
//!
//! 这里有意区分两种身份,别混用:
//!
//! - **`device_id`(公开)**:发现用。会出现在 `/health` 与组播通告里,任何人都能读到,
//!   因此**不能**用作信任凭据——否则扫一下对方 `/health` 拿到 id 就能冒充他被自动放行,
//!   「始终允许」会变成虚假的安全感。它只用来在发现结果里认出「这是我自己」。
//! - **`token`(私密)**:服务端在用户选「始终允许」时为该客户端签发的随机令牌,
//!   一服务端一客户端一个。只在 `/stream` 请求头里出现,从不出现在任何公开接口。
//!   服务端按令牌认客户端;客户端按对方 `device_id` 存自己拿到的令牌。
//!   每对设备各自独立,恶意服务端拿到的令牌也冒充不了你去连别的服务端。
//!
//! 前提是局域网可信,传输不加密:这套机制要挡的是「没打招呼就用了你的麦克风」。

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// 设备名展示上限:超长名字会撑破授权弹窗,且对方名字是不可信输入
const MAX_NAME_LEN: usize = 40;

/// 服务端信任的一个客户端
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trusted {
    /// 本机为该客户端签发的令牌(私密,信任的真正凭据)
    pub token: String,
    /// 授权时对方的设备名,仅供 UI 展示与撤销时辨认
    pub name: String,
    /// 授权时间(Unix 秒),UI 展示用
    pub added_at: u64,
}

/// 本机作为客户端时,从某个服务端拿到的令牌
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownServer {
    /// 对方的公开 device_id
    pub server_id: String,
    pub token: String,
    /// 对方设备名,展示用
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// 用户可改的设备名,默认取主机名
    pub device_name: String,
    /// 公开身份:发现用
    pub device_id: String,
    /// 本机作为服务端信任的客户端
    pub trusted: Vec<Trusted>,
    /// 本机作为客户端已获授权的服务端
    pub known_servers: Vec<KnownServer>,
    /// 上次退出时所处的界面模式("server" / "client");
    /// None = 新装用户还没用过任何模式,启动时不该替用户选「共享麦克风」
    #[serde(default)]
    pub last_mode: Option<String>,
    /// 上次退出时服务端是否在监听。新装默认 false:装上就默默开始
    /// 被局域网发现,对只想用别人麦克风的用户是意外行为
    #[serde(default)]
    pub server_was_running: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            device_id: random_hex(),
            trusted: Vec::new(),
            known_servers: Vec::new(),
            last_mode: None,
            server_was_running: false,
        }
    }
}

fn default_device_name() -> String {
    let raw = gethostname::gethostname()
        .into_string()
        .unwrap_or_else(|_| "MicSync 设备".into());
    // macOS 主机名常带 .local 后缀,展示时去掉更干净
    let trimmed = raw.strip_suffix(".local").unwrap_or(&raw);
    let name = sanitize_name(trimmed);
    if name.is_empty() {
        "MicSync 设备".into()
    } else {
        name
    }
}

/// 设备名是跨设备传来的不可信输入:去掉控制字符(含换行,会破坏 HTTP 头)并限长
pub fn sanitize_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_LEN)
        .collect::<String>()
        .trim()
        .to_string()
}

/// 随机十六进制串。只用于设备标识与令牌,不是密码——
/// RandomState 每进程随机播种,叠加纳秒时间戳与 pid 足够避免碰撞
pub fn random_hex() -> String {
    let mut out = String::with_capacity(32);
    for i in 0..2u64 {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        h.write_u32(std::process::id());
        h.write_u64(i);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 配置文件路径。未设置(单测)时全程内存操作,save 静默跳过
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();
static SETTINGS: OnceLock<Mutex<Settings>> = OnceLock::new();

/// 由 lib.rs 在启动时调用,传入应用配置目录
pub fn init(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = CONFIG_PATH.set(dir.join("settings.json"));
}

fn cell() -> &'static Mutex<Settings> {
    SETTINGS.get_or_init(|| Mutex::new(load()))
}

fn load() -> Settings {
    let Some(path) = CONFIG_PATH.get() else {
        return Settings::default();
    };
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            // 配置损坏不该让应用起不来,退回默认(下次 save 会覆盖)
            eprintln!("配置解析失败,使用默认设置: {e}");
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

fn save(s: &Settings) {
    let Some(path) = CONFIG_PATH.get() else {
        return; // 单测:不落盘
    };
    match serde_json::to_string_pretty(s) {
        Ok(text) => {
            if let Err(e) = std::fs::write(path, text) {
                eprintln!("配置保存失败: {e}");
            }
        }
        Err(e) => eprintln!("配置序列化失败: {e}"),
    }
}

pub fn get() -> Settings {
    cell().lock().unwrap().clone()
}

pub fn device_name() -> String {
    cell().lock().unwrap().device_name.clone()
}

pub fn device_id() -> String {
    cell().lock().unwrap().device_id.clone()
}

/// 改设备名;空名字退回主机名,避免 UI 上出现无名设备
pub fn set_device_name(name: &str) -> String {
    let mut clean = sanitize_name(name);
    if clean.is_empty() {
        clean = default_device_name();
    }
    let mut g = cell().lock().unwrap();
    g.device_name = clean.clone();
    save(&g);
    clean
}

/// 上次退出时的界面模式;None = 新装用户
pub fn last_mode() -> Option<String> {
    cell().lock().unwrap().last_mode.clone()
}

/// 记录当前所处模式,重启后恢复到用户离开时的形态。只认 "server"/"client",
/// 其他值直接忽略——这个值会决定启动行为,不能被随意写脏
pub fn set_last_mode(mode: &str) {
    if mode != "server" && mode != "client" {
        return;
    }
    let mut g = cell().lock().unwrap();
    if g.last_mode.as_deref() == Some(mode) {
        return;
    }
    g.last_mode = Some(mode.to_string());
    save(&g);
}

/// 上次退出时服务端是否在监听(下次启动是否自动恢复监听的依据)
pub fn server_was_running() -> bool {
    cell().lock().unwrap().server_was_running
}

pub fn set_server_was_running(running: bool) {
    let mut g = cell().lock().unwrap();
    if g.server_was_running == running {
        return;
    }
    g.server_was_running = running;
    save(&g);
}

/// 服务端:令牌是否属于已信任的客户端;命中则顺带刷新对方设备名
pub fn trusted_by_token(token: &str, name: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let mut g = cell().lock().unwrap();
    let Some(t) = g.trusted.iter_mut().find(|t| t.token == token) else {
        return false;
    };
    if !name.is_empty() && t.name != name {
        t.name = name.to_string(); // 对方改了名字,信任仍按令牌走
        let snapshot = g.clone();
        drop(g);
        save(&snapshot);
    }
    true
}

/// 服务端:用户选「始终允许」——签发令牌并记住这个客户端
pub fn trust_client(name: &str) -> String {
    let token = random_hex();
    let mut g = cell().lock().unwrap();
    g.trusted.push(Trusted {
        token: token.clone(),
        name: sanitize_name(name),
        added_at: now_secs(),
    });
    save(&g);
    token
}

/// 服务端:撤销某个已信任客户端(按令牌定位)
pub fn revoke_trusted(token: &str) {
    let mut g = cell().lock().unwrap();
    let before = g.trusted.len();
    g.trusted.retain(|t| t.token != token);
    if g.trusted.len() != before {
        save(&g);
    }
}

pub fn trusted_list() -> Vec<Trusted> {
    cell().lock().unwrap().trusted.clone()
}

/// 客户端:取本机在该服务端上的令牌
pub fn token_for_server(server_id: &str) -> Option<String> {
    if server_id.is_empty() {
        return None;
    }
    cell()
        .lock()
        .unwrap()
        .known_servers
        .iter()
        .find(|s| s.server_id == server_id)
        .map(|s| s.token.clone())
}

/// 客户端:记下某服务端签发给本机的令牌(同一服务端重复授权则覆盖)
pub fn remember_server(server_id: &str, token: &str, name: &str) {
    if server_id.is_empty() || token.is_empty() {
        return;
    }
    let mut g = cell().lock().unwrap();
    if let Some(existing) = g.known_servers.iter_mut().find(|s| s.server_id == server_id) {
        existing.token = token.to_string();
        existing.name = sanitize_name(name);
    } else {
        g.known_servers.push(KnownServer {
            server_id: server_id.to_string(),
            token: token.to_string(),
            name: sanitize_name(name),
        });
    }
    save(&g);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_control_chars_and_limits_length() {
        // 换行会破坏 HTTP 头,必须滤掉
        assert_eq!(sanitize_name("我的\r\nMac"), "我的Mac");
        assert_eq!(sanitize_name("  张三的 Mac  "), "张三的 Mac");
        assert_eq!(sanitize_name(&"啊".repeat(100)).chars().count(), MAX_NAME_LEN);
        assert_eq!(sanitize_name("\u{0}\u{7}"), "");
    }

    #[test]
    fn random_hex_is_unique_and_long_enough() {
        let a = random_hex();
        let b = random_hex();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b, "两次生成不应相同");
    }

    #[test]
    fn last_mode_rejects_unknown_values_and_defaults_to_fresh_install() {
        // 新装用户:没有模式记录,也不该自动开服务
        // (单测共享同一个进程级 SETTINGS,先于任何 set_* 断言默认值)
        assert_eq!(last_mode(), None);
        assert!(!server_was_running());
        set_last_mode("attacker-controlled");
        assert_eq!(last_mode(), None, "非法模式值不能落库");
        set_last_mode("client");
        assert_eq!(last_mode().as_deref(), Some("client"));
        set_last_mode("server");
        assert_eq!(last_mode().as_deref(), Some("server"));
        set_server_was_running(true);
        assert!(server_was_running());
        set_server_was_running(false);
        assert!(!server_was_running());
    }

    #[test]
    fn old_config_without_mode_fields_still_parses() {
        // 升级场景:旧版 settings.json 没有 last_mode/server_was_running,
        // 反序列化必须成功并落到「新装」默认值,不能整份配置回退默认丢掉授权
        let old = r#"{
            "device_name": "旧设备",
            "device_id": "abc123",
            "trusted": [],
            "known_servers": []
        }"#;
        let s: Settings = serde_json::from_str(old).expect("旧配置应能解析");
        assert_eq!(s.device_name, "旧设备");
        assert_eq!(s.last_mode, None);
        assert!(!s.server_was_running);
    }

    #[test]
    fn default_name_drops_local_suffix() {
        // 直接验行为:主机名带 .local 时去掉后缀
        let n = default_device_name();
        assert!(!n.ends_with(".local"), "got {n}");
        assert!(!n.is_empty());
    }
}
