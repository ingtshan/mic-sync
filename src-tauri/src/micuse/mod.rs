//! 本机麦克风使用检测(平台抽象):回答「本机有没有应用正在把虚拟声卡
//! 当麦克风采集」。macOS 通过 CoreAudio HAL 的 Process Objects API 检测
//! BlackHole;Windows 通过 WASAPI 音频会话枚举检测 VB-Cable 的采集端
//! (CABLE Output);其他平台恒返回 Unsupported,自动跟随不可用。

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{
    device_running_somewhere, find_monitor_target, other_process_capturing, MonitorTarget,
};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::{
    device_running_somewhere, find_monitor_target, other_process_capturing, MonitorTarget,
};

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use unsupported::{
    device_running_somewhere, find_monitor_target, other_process_capturing, MonitorTarget,
};

/// 检测结果
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MicUse {
    /// 有其他进程正在从该设备采集输入(会议软件在用这个"麦克风")
    InUse,
    /// 没有其他进程在采集
    Idle,
    /// 当前系统不支持检测(macOS < 14 无 Process Objects API,或未实现的平台)
    Unsupported,
}
