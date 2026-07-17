//! 其他平台占位:没有虚拟声卡使用检测的实现,自动跟随不可用。
//! find 返回占位目标,让 follow 线程走到 other_process_capturing
//! 拿到 Unsupported 后以明确的提示退出(而不是一直报"设备缺失")。

use super::MicUse;

#[derive(Clone)]
pub struct MonitorTarget;

pub fn find_monitor_target(_output_device_name: &str) -> Option<MonitorTarget> {
    Some(MonitorTarget)
}

pub fn device_running_somewhere(_target: &MonitorTarget) -> bool {
    false
}

pub fn other_process_capturing(_target: &MonitorTarget) -> MicUse {
    MicUse::Unsupported
}
