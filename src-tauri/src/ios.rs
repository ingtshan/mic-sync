//! iOS 专属:AVAudioSession 配置与录音权限。
//! cpal 在 iOS 上只负责 AudioUnit 采集,会话类别/激活/权限必须由 App 自己完成,
//! 否则输入流拿到的是静音。

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};
use objc2_foundation::{NSError, NSString};

// 强制链接框架,保证 AVAudioSession / UIApplication 类在运行时可见
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}
#[link(name = "UIKit", kind = "framework")]
extern "C" {}

/// AVAudioSessionCategoryOptionMixWithOthers:开麦采集时不打断本机正在播放的音频
const OPTION_MIX_WITH_OTHERS: usize = 0x1;

pub fn setup_audio_session() {
    unsafe {
        let session: *mut AnyObject = msg_send![class!(AVAudioSession), sharedInstance];

        // PlayAndRecord + MixWithOthers;类别常量的值就是它的名字字符串
        let category = NSString::from_str("AVAudioSessionCategoryPlayAndRecord");
        let set_cat: Result<(), Retained<NSError>> = msg_send![
            session,
            setCategory: &*category,
            withOptions: OPTION_MIX_WITH_OTHERS,
            error: _
        ];
        if let Err(e) = set_cat {
            eprintln!("AVAudioSession setCategory 失败: {e}");
        }
        let activate: Result<(), Retained<NSError>> = msg_send![session, setActive: true, error: _];
        if let Err(e) = activate {
            eprintln!("AVAudioSession setActive 失败: {e}");
        }

        // 启动即申请录音权限,别等第一次远端请求才弹窗(那会让首次开麦超时)
        let handler = block2::RcBlock::new(|granted: Bool| {
            if !granted.as_bool() {
                eprintln!("录音权限被拒绝,远端将无法使用本机麦克风");
            }
        });
        let _: () = msg_send![session, requestRecordPermission: &*handler];

        // 服务端要保持可达:App 在前台时禁止自动锁屏
        let app: *mut AnyObject = msg_send![class!(UIApplication), sharedApplication];
        let _: () = msg_send![app, setIdleTimerDisabled: true];
    }
}
