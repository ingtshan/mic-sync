//! Windows 实现:通过 WASAPI 音频会话枚举(IAudioSessionManager2)检测
//! 「有没有应用正在从虚拟麦克风的采集端录音」。
//! VB-Cable 是一对端点:客户端向渲染端「CABLE Input」播放,会议软件从
//! 采集端「CABLE Output」录音——检测目标是采集端。我们自己的播放流挂在
//! 渲染端,不会出现在采集端的会话列表里,所以两个信号取自同一次枚举:
//! device_running_somewhere = 采集端有任意活跃会话(待命期的"开始"信号),
//! other_process_capturing = 排除本进程后仍有活跃会话("使用中→结束"信号)。

use windows::core::{Interface, HSTRING};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

use super::MicUse;

/// 监视目标:虚拟麦克风采集端(如 CABLE Output)的端点 ID
#[derive(Clone)]
pub struct MonitorTarget {
    capture_id: HSTRING,
}

// 每线程一次 COM 初始化;调用方线程与应用同生命周期,不做反初始化
thread_local! {
    static COM_READY: () = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    };
}

fn enumerator() -> Option<IMMDeviceEnumerator> {
    COM_READY.with(|_| ());
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok() }
}

fn friendly_name(dev: &IMMDevice) -> Option<String> {
    unsafe {
        let store = dev.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let raw = PropVariantToStringAlloc(&value).ok()?;
        let name = raw.to_string().ok();
        CoTaskMemFree(Some(raw.as_ptr() as *const _));
        name.filter(|n| !n.is_empty())
    }
}

fn endpoint_id(dev: &IMMDevice) -> Option<HSTRING> {
    unsafe {
        let raw = dev.GetId().ok()?;
        let id = raw.to_string().ok();
        CoTaskMemFree(Some(raw.as_ptr() as *const _));
        id.map(|s| HSTRING::from(s.as_str()))
    }
}

/// 由客户端选择的「输出设备名」(渲染端,如 CABLE Input)推导要监视的采集端。
/// VB-Cable 命名约定是渲染端 Input / 采集端 Output 成对;找不到对应名字时
/// 退回「名字含 CABLE Output 的采集设备」。
pub fn find_monitor_target(output_device_name: &str) -> Option<MonitorTarget> {
    let en = enumerator()?;
    let paired = output_device_name.replace("Input", "Output");
    unsafe {
        let coll = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).ok()?;
        let count = coll.GetCount().ok()?;
        let mut fallback = None;
        for i in 0..count {
            let Ok(dev) = coll.Item(i) else { continue };
            let Some(name) = friendly_name(&dev) else {
                continue;
            };
            if name == paired || name == output_device_name {
                return endpoint_id(&dev).map(|id| MonitorTarget { capture_id: id });
            }
            if fallback.is_none() && name.contains("CABLE Output") {
                fallback = endpoint_id(&dev).map(|id| MonitorTarget { capture_id: id });
            }
        }
        fallback
    }
}

/// 枚举采集端上的音频会话;exclude_self 时跳过本进程的会话。
/// 端点被拔出/临时枚举失败返回 None,由调用方按空闲处理
fn sessions_active(target: &MonitorTarget, exclude_self: bool) -> Option<bool> {
    let en = enumerator()?;
    unsafe {
        let dev = en.GetDevice(&target.capture_id).ok()?;
        let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None).ok()?;
        let sessions = mgr.GetSessionEnumerator().ok()?;
        let count = sessions.GetCount().ok()?;
        let me = std::process::id();
        for i in 0..count {
            let Ok(ctl) = sessions.GetSession(i) else {
                continue;
            };
            if ctl.GetState().ok() != Some(AudioSessionStateActive) {
                continue;
            }
            if exclude_self {
                if let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() {
                    if ctl2.GetProcessId().ok() == Some(me) {
                        continue;
                    }
                }
            }
            return Some(true);
        }
        Some(false)
    }
}

/// 采集端上是否有任意进程的活跃录音会话(待命期的"开始使用"信号)
pub fn device_running_somewhere(target: &MonitorTarget) -> bool {
    sessions_active(target, false).unwrap_or(false)
}

/// 除本进程外,是否有进程正在从采集端录音("使用中→结束"信号)。
/// WASAPI 会话枚举自 Windows 7 起可用,不存在 Unsupported 情形
pub fn other_process_capturing(target: &MonitorTarget) -> MicUse {
    match sessions_active(target, true) {
        Some(true) => MicUse::InUse,
        _ => MicUse::Idle,
    }
}
