//! macOS 实现:通过 CoreAudio HAL 的 Process Objects API(macOS 14+,
//! 即系统橙色麦克风指示灯的数据源)查询「除本进程外,是否有进程正在从指定设备
//! (通常是 BlackHole)采集输入」。BlackHole 自身没有 IPC 接口,但它是标准
//! CoreAudio 设备,会议软件把它当麦克风采集时在 HAL 里可见。
//! macOS 上播放目标与检测目标是同一个设备(BlackHole 回环),
//! 所以 MonitorTarget 就是该输出设备的 AudioObjectID。

use std::ffi::c_void;

use super::MicUse;

/// 监视目标:BlackHole 设备的 AudioObjectID
pub type MonitorTarget = u32;

type AudioObjectID = u32;
type OSStatus = i32;

/// kAudioObjectSystemObject
const SYSTEM_OBJECT: AudioObjectID = 1;
/// kAudioObjectPropertyScopeGlobal = 'glob'
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
/// kAudioObjectPropertyElementMain
const ELEMENT_MAIN: u32 = 0;
/// kAudioHardwarePropertyDevices = 'dev#'
const PROP_DEVICES: u32 = fourcc(b"dev#");
/// kAudioObjectPropertyName = 'lnam'
const PROP_NAME: u32 = fourcc(b"lnam");
/// kAudioHardwarePropertyProcessObjectList = 'prs#'(macOS 14+)
const PROP_PROCESS_LIST: u32 = fourcc(b"prs#");
/// kAudioProcessPropertyPID = 'ppid'
const PROP_PROC_PID: u32 = fourcc(b"ppid");
/// kAudioProcessPropertyDevices = 'pdv#'
const PROP_PROC_DEVICES: u32 = fourcc(b"pdv#");
/// kAudioProcessPropertyIsRunningInput = 'piri'
const PROP_PROC_IS_RUNNING_INPUT: u32 = fourcc(b"piri");
/// kAudioDevicePropertyDeviceIsRunningSomewhere = 'gone'
const PROP_DEV_RUNNING_SOMEWHERE: u32 = fourcc(b"gone");

const fn fourcc(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*b)
}

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: AudioObjectID,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        out_size: *mut u32,
    ) -> OSStatus;
    fn AudioObjectGetPropertyData(
        object: AudioObjectID,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        io_size: *mut u32,
        out_data: *mut c_void,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringGetCString(s: *const c_void, buf: *mut u8, size: isize, encoding: u32) -> u8;
    fn CFRelease(cf: *const c_void);
}
/// kCFStringEncodingUTF8
const CF_UTF8: u32 = 0x0800_0100;

fn addr(selector: u32) -> PropertyAddress {
    PropertyAddress {
        selector,
        scope: SCOPE_GLOBAL,
        element: ELEMENT_MAIN,
    }
}

/// 读 AudioObjectID/u32 数组型属性;属性不存在(如旧系统无 Process Objects)返回 None
fn get_vec_u32(object: AudioObjectID, selector: u32) -> Option<Vec<u32>> {
    unsafe {
        let a = addr(selector);
        let mut size: u32 = 0;
        if AudioObjectGetPropertyDataSize(object, &a, 0, std::ptr::null(), &mut size) != 0 {
            return None;
        }
        let count = size as usize / 4;
        if count == 0 {
            return Some(Vec::new());
        }
        let mut out = vec![0u32; count];
        if AudioObjectGetPropertyData(
            object,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            out.as_mut_ptr() as *mut c_void,
        ) != 0
        {
            return None;
        }
        out.truncate(size as usize / 4);
        Some(out)
    }
}

fn get_u32(object: AudioObjectID, selector: u32) -> Option<u32> {
    unsafe {
        let a = addr(selector);
        let mut value: u32 = 0;
        let mut size = 4u32;
        if AudioObjectGetPropertyData(
            object,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            &mut value as *mut u32 as *mut c_void,
        ) != 0
        {
            return None;
        }
        Some(value)
    }
}

/// 对象名称(设备名等);属性值为 CFStringRef,按 get 规则由调用方释放
fn object_name(object: AudioObjectID) -> Option<String> {
    unsafe {
        let a = addr(PROP_NAME);
        let mut cf: *const c_void = std::ptr::null();
        let mut size = std::mem::size_of::<*const c_void>() as u32;
        if AudioObjectGetPropertyData(
            object,
            &a,
            0,
            std::ptr::null(),
            &mut size,
            &mut cf as *mut *const c_void as *mut c_void,
        ) != 0
            || cf.is_null()
        {
            return None;
        }
        let mut buf = [0u8; 256];
        let ok = CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as isize, CF_UTF8);
        CFRelease(cf);
        if ok == 0 {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Some(String::from_utf8_lossy(&buf[..end]).into_owned())
    }
}

/// 由输出设备名找到监视目标(macOS 上即该设备本身;精确匹配优先,其次包含匹配)
pub fn find_monitor_target(output_device_name: &str) -> Option<MonitorTarget> {
    let devices = get_vec_u32(SYSTEM_OBJECT, PROP_DEVICES)?;
    let mut fuzzy = None;
    for id in devices {
        match object_name(id) {
            Some(n) if n == output_device_name => return Some(id),
            Some(n)
                if fuzzy.is_none()
                    && (n.contains(output_device_name) || output_device_name.contains(&n)) =>
            {
                fuzzy = Some(id)
            }
            _ => {}
        }
    }
    fuzzy
}

/// 设备上是否有任意进程(含本进程)正在跑 IO。
/// 待命期我们不持有 BlackHole 的任何流,此值变 true 即「有应用开始用它」——
/// 这是"开始使用"的精确信号;一旦我们自己开始播放,该值恒 true,失效。
pub fn device_running_somewhere(target: &MonitorTarget) -> bool {
    get_u32(*target, PROP_DEV_RUNNING_SOMEWHERE) == Some(1)
}

/// 除本进程外,是否有进程正在采集输入(macOS 14+ Process Objects)。
/// 用作"使用中→结束"的信号。注意:其他进程的设备列表(pdv#)通常因隐私
/// 不可见(返回空),此时无法归因到具体设备,只能按「有无进程在采集」近似;
/// 列表可见时才按设备过滤。
pub fn other_process_capturing(target: &MonitorTarget) -> MicUse {
    let Some(procs) = get_vec_u32(SYSTEM_OBJECT, PROP_PROCESS_LIST) else {
        return MicUse::Unsupported;
    };
    let me = std::process::id();
    for p in procs {
        if get_u32(p, PROP_PROC_PID) == Some(me) {
            continue;
        }
        if get_u32(p, PROP_PROC_IS_RUNNING_INPUT) != Some(1) {
            continue;
        }
        // 设备列表可见且明确不含目标设备 → 排除;否则(不可见/含目标)计入
        match get_vec_u32(p, PROP_PROC_DEVICES) {
            Some(ds) if !ds.is_empty() && !ds.contains(target) => continue,
            _ => return MicUse::InUse,
        }
    }
    MicUse::Idle
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// 活体验证配套:本进程从 BlackHole 采集 15 秒,供另一个 cargo test 进程
    /// 跑 probe_micuse 观察 InUse。运行:
    ///   cargo test hold_blackhole_capture -- --ignored & 稍后另开进程跑 probe_micuse
    #[test]
    #[ignore = "占用 BlackHole 输入 15 秒,配合 probe_micuse 活体验证"]
    fn hold_blackhole_capture() {
        use cpal::traits::{DeviceTrait, StreamTrait};
        let device =
            crate::audio::find_input_device(Some("BlackHole 2ch")).expect("BlackHole input");
        let name = device.name().unwrap_or_default();
        assert!(name.contains("BlackHole"), "got device {name}");
        let config = device.default_input_config().expect("config");
        let stream_config: cpal::StreamConfig = config.into();
        let stream = device
            .build_input_stream(
                &stream_config,
                |_data: &[f32], _: &cpal::InputCallbackInfo| {},
                |e| eprintln!("err: {e}"),
                None,
            )
            .expect("build input stream");
        stream.play().expect("play");
        println!("capturing from {name} for 15s...");
        std::thread::sleep(std::time::Duration::from_secs(15));
    }

    /// 手动探测(打真实 CoreAudio HAL):验证 FourCC/FFI 正确、耗时可接受。
    /// 运行: cargo test probe_micuse -- --ignored --nocapture
    #[test]
    #[ignore = "打真实 CoreAudio HAL,手动运行验证"]
    fn probe_micuse() {
        let t0 = Instant::now();
        let dev = find_monitor_target("BlackHole 2ch");
        println!("[{:?}] find_monitor_target(BlackHole 2ch) = {dev:?}", t0.elapsed());
        if let Some(dev) = dev {
            let t1 = Instant::now();
            let usage = other_process_capturing(&dev);
            println!("[{:?}] other_process_capturing = {usage:?}", t1.elapsed());
            let busy = device_running_somewhere(&dev);
            println!("device_running_somewhere = {busy}");
        }
        let t2 = Instant::now();
        let procs = get_vec_u32(SYSTEM_OBJECT, PROP_PROCESS_LIST);
        println!(
            "[{:?}] process objects: {:?} 个",
            t2.elapsed(),
            procs.as_ref().map(|p| p.len())
        );
        if let Some(procs) = procs {
            for p in procs.iter().take(30) {
                let pid = get_u32(*p, PROP_PROC_PID);
                let running_in = get_u32(*p, PROP_PROC_IS_RUNNING_INPUT);
                let devs = get_vec_u32(*p, PROP_PROC_DEVICES);
                if running_in == Some(1) || devs.as_ref().is_some_and(|d| !d.is_empty()) {
                    println!("  pid={pid:?} running_input={running_in:?} devices={devs:?}");
                }
            }
        }
    }
}
