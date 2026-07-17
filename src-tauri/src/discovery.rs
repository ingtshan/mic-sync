//! 局域网服务发现(借鉴 LocalSend 的双路设计):
//!
//! 1. **组播查询/应答**:客户端向组播组发一条查询,在场的服务端单播应答。
//!    与 LocalSend 的周期性 announce 不同,这里做成客户端发起——契合 MicSync
//!    「平时零流量」的性质:没人搜索时组播网络上一个包都没有。服务端启动时
//!    额外广播一次 announce,让已经打开着的客户端立刻看到它上线。
//! 2. **子网扫描回退**:并发探测本机各 /24 内每个 IP 的 `/health`。这条路
//!    不只是「组播被封时的备胎」——**iOS 服务端只能靠它被发现**:iOS 收发
//!    组播需要 Apple 特批的受限授权(com.apple.developer.networking.multicast),
//!    自签侧载拿不到。`/health` 本来就返回 `{"app":"micsync",...}`,天然是发现签名。
//!
//! 两路并发跑、按 `ip:port` 合并去重。自己发现自己(桌面端同时是服务端和客户端)
//! 靠公开的 device_id 过滤——比按本机 IP 过滤更可靠,多网卡/回环都不会漏。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::settings;

/// 组播组:沿用 LocalSend 的 224.0.0.167——它在 224.0.0.0/24(本地网络控制块)内,
/// 这个范围是 LocalSend 踩出来的经验:部分 Android 实现会直接丢弃该段之外的组播。
/// 端口用我们自己的,和 LocalSend 的 53317 错开:组地址可以共用,内核按端口分流,
/// 两个应用互相收不到对方的包。
const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 167);
pub const DISCOVERY_PORT: u16 = 47801;

/// 协议版本:将来改报文格式时用它拒掉不兼容的旧包
const PROTO_V: u32 = 1;

/// 扫描单个 IP 的连接超时。局域网内 RTT 通常 <5ms,300ms 足够宽容;
/// 太长会拖慢整轮扫描(不可达 IP 要等满这个时间)
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// 子网扫描的并发线程数。装了 VPN/代理的机器可能有近十个网卡地址,
/// 每个 /24 都要扫 253 个,总量上千——并发开小了整轮要好几秒。
/// 线程都是短命的 TCP connect,开到 128 成本可接受
const SCAN_THREADS: usize = 128;
/// 组播应答收集窗口
const MULTICAST_WAIT: Duration = Duration::from_millis(400);

/// 发现到的服务端,给前端点一下就填好地址
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Peer {
    /// 用户可改的设备名
    pub alias: String,
    /// "ip:port",可直接作为客户端连接地址
    pub addr: String,
    /// "desktop" / "mobile"
    pub device_type: String,
    /// 对方的公开身份;客户端据此找出自己在该服务端上的授权令牌
    pub device_id: String,
    /// 本机是否已获该服务端授权(UI 用来标「已授权 / 需对方确认」)
    pub authorized: bool,
}

/// 本机公开身份:发现时用来把「自己」摘掉。公开可读,**不能**当信任凭据
/// (信任走服务端签发的令牌,见 settings 模块的说明)
pub fn device_id() -> String {
    settings::device_id()
}

/// 展示名:用户可改的设备名
pub fn alias() -> String {
    settings::device_name()
}

pub fn device_type() -> &'static str {
    if cfg!(any(target_os = "ios", target_os = "android")) {
        "mobile"
    } else {
        "desktop"
    }
}

/// 报文:查询(客户端发起)或应答/上线通告(服务端发出)
#[derive(Debug, PartialEq)]
enum Msg {
    Query {
        device_id: String,
    },
    Announce {
        alias: String,
        device_type: String,
        device_id: String,
        port: u16,
    },
}

fn encode_query(fp: &str) -> Vec<u8> {
    serde_json::json!({
        "app": "micsync",
        "v": PROTO_V,
        "query": true,
        "device_id": fp,
    })
    .to_string()
    .into_bytes()
}

fn encode_announce(alias: &str, fp: &str, port: u16) -> Vec<u8> {
    serde_json::json!({
        "app": "micsync",
        "v": PROTO_V,
        "query": false,
        "alias": alias,
        "device_type": device_type(),
        "device_id": fp,
        "port": port,
    })
    .to_string()
    .into_bytes()
}

/// 解析报文;非 micsync / 版本不符 / 字段缺失都返回 None(网络上什么包都可能来)
fn parse_msg(buf: &[u8]) -> Option<Msg> {
    let v: serde_json::Value = serde_json::from_slice(buf).ok()?;
    if v.get("app")?.as_str()? != "micsync" || v.get("v")?.as_u64()? != PROTO_V as u64 {
        return None;
    }
    let device_id = v.get("device_id")?.as_str()?.to_string();
    if v.get("query")?.as_bool()? {
        return Some(Msg::Query { device_id });
    }
    Some(Msg::Announce {
        alias: v.get("alias")?.as_str()?.to_string(),
        device_type: v.get("device_type")?.as_str()?.to_string(),
        device_id,
        port: u16::try_from(v.get("port")?.as_u64()?).ok()?,
    })
}

pub struct ResponderHandle {
    stop: Arc<AtomicBool>,
}

impl ResponderHandle {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for ResponderHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动组播应答器:收到查询就单播应答,并在启动时通告一次上线。
/// 失败(端口被占、iOS 无组播授权)不影响服务本身——子网扫描仍能发现我们,
/// 所以这里返回 Err 由调用方决定是否忽略。
pub fn start_responder(server_port: u16) -> Result<ResponderHandle, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT))
        .map_err(|e| format!("绑定发现端口 {DISCOVERY_PORT} 失败: {e}"))?;
    // 多网卡(Wi-Fi + 有线 + VPN)逐个加入,只靠默认路由会漏掉其他网段的客户端
    let mut joined = 0;
    for ip in local_ipv4s() {
        if socket.join_multicast_v4(&GROUP, &ip).is_ok() {
            joined += 1;
        }
    }
    if joined == 0 {
        // 退一步用默认接口;再失败就是真不支持组播(如 iOS 无授权)
        socket
            .join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)
            .map_err(|e| format!("加入组播组失败: {e}"))?;
    }
    socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .map_err(|e| format!("设置发现套接字超时失败: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        thread::Builder::new()
            .name("mic-discovery".into())
            .spawn(move || responder_thread(socket, server_port, stop))
            .map_err(|e| format!("创建发现线程失败: {e}"))?;
    }
    Ok(ResponderHandle { stop })
}

fn responder_thread(socket: UdpSocket, server_port: u16, stop: Arc<AtomicBool>) {
    let me = device_id();
    let name = alias();

    // 上线通告一次:已经开着的客户端列表里立刻多出我们,不用等用户手动刷新
    let _ = socket.send_to(
        &encode_announce(&name, &me, server_port),
        SocketAddr::from((GROUP, DISCOVERY_PORT)),
    );

    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::SeqCst) {
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            // 超时是常态(平时没人搜索),借它回到循环顶部查停止标志
            Err(_) => continue,
        };
        // 只应答查询;别人的应答/通告与自己的回环包都忽略
        if let Some(Msg::Query { device_id: from_id }) = parse_msg(&buf[..n]) {
            if from_id == me {
                continue;
            }
            let _ = socket.send_to(&encode_announce(&name, &me, server_port), from);
        }
    }
}

/// 搜索局域网内的服务端:组播查询与子网扫描并发跑,合并去重。
/// `server_port` 是扫描时探测的目标端口(与本机服务端口一致的约定)。
pub fn discover(server_port: u16) -> Vec<Peer> {
    let me = device_id();
    let scan = {
        let me = me.clone();
        thread::spawn(move || scan_subnets(&me, server_port))
    };
    // 组播在本线程跑,扫描在旁边并发,总耗时取两者较大者而非相加
    let mut found: HashMap<String, Peer> = HashMap::new();
    for p in multicast_query(&me) {
        found.insert(p.addr.clone(), p);
    }
    if let Ok(peers) = scan.join() {
        for p in peers {
            // 组播应答里有主机名,信息比扫描来的更全,已有就不覆盖
            found.entry(p.addr.clone()).or_insert(p);
        }
    }
    let mut list: Vec<Peer> = found.into_values().collect();
    list.sort_by(|a, b| a.addr.cmp(&b.addr));
    list
}

/// 向组播组发查询并收集应答。每块网卡各用一个套接字发送——
/// 绑定到具体本机 IP 才能强制指定出口接口,只靠默认路由会漏掉其他网段。
fn multicast_query(me: &str) -> Vec<Peer> {
    let query = encode_query(me);
    let mut sockets = Vec::new();
    for ip in local_ipv4s() {
        let Ok(s) = UdpSocket::bind((ip, 0)) else {
            continue;
        };
        if s.set_read_timeout(Some(Duration::from_millis(50))).is_err() {
            continue;
        }
        // 同机的服务端也要能收到(桌面端自己就是服务端),靠 device_id 过滤自己
        let _ = s.set_multicast_loop_v4(true);
        if s.send_to(&query, SocketAddr::from((GROUP, DISCOVERY_PORT)))
            .is_ok()
        {
            sockets.push(s);
        }
    }

    let mut peers = Vec::new();
    let deadline = Instant::now() + MULTICAST_WAIT;
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        let mut got_any = false;
        for s in &sockets {
            let Ok((n, from)) = s.recv_from(&mut buf) else {
                continue;
            };
            got_any = true;
            let Some(Msg::Announce {
                alias,
                device_type,
                device_id,
                port,
            }) = parse_msg(&buf[..n])
            else {
                continue;
            };
            if device_id == me {
                continue; // 自己的服务端
            }
            peers.push(Peer {
                authorized: settings::token_for_server(&device_id).is_some(),
                alias,
                addr: format!("{}:{}", from.ip(), port),
                device_type,
                device_id,
            });
        }
        if !got_any {
            thread::sleep(Duration::from_millis(20));
        }
    }
    peers
}

/// 本机可用于局域网通信的 IPv4(过滤回环/隧道/AWDL/蜂窝——客户端从这些口连不进来)
fn local_ipv4s() -> Vec<Ipv4Addr> {
    let Ok(ifas) = local_ip_address::list_afinet_netifas() else {
        return Vec::new();
    };
    ifas.into_iter()
        .filter_map(|(name, ip)| match ip {
            IpAddr::V4(v4)
                if !v4.is_loopback()
                    && !v4.is_link_local()
                    && !name.starts_with("utun")
                    && !name.starts_with("awdl")
                    && !name.starts_with("llw")
                    && !name.starts_with("bridge")
                    && !name.starts_with("pdp_ip") =>
            {
                Some(v4)
            }
            _ => None,
        })
        .collect()
}

/// 枚举一个 IPv4 所在 /24 的全部主机地址(.1~.254,跳过自己)。
/// 假定 /24 是 LocalSend 同款取舍:拿不到子网掩码,而家庭/办公 LAN 绝大多数是 /24;
/// 更大的网段(/16)扫描不现实,那种环境靠组播。
fn subnet_hosts(ip: Ipv4Addr) -> Vec<Ipv4Addr> {
    let o = ip.octets();
    (1u8..=254)
        .map(|last| Ipv4Addr::new(o[0], o[1], o[2], last))
        .filter(|c| *c != ip)
        .collect()
}

/// 并发扫描本机各 /24,靠 `/health` 认出 MicSync 服务端
fn scan_subnets(me: &str, port: u16) -> Vec<Peer> {
    let mut targets: Vec<Ipv4Addr> = Vec::new();
    for ip in local_ipv4s() {
        for host in subnet_hosts(ip) {
            if !targets.contains(&host) {
                targets.push(host);
            }
        }
    }
    if targets.is_empty() {
        return Vec::new();
    }

    let chunk = targets.len().div_ceil(SCAN_THREADS).max(1);
    let mut handles = Vec::new();
    for part in targets.chunks(chunk) {
        let part: Vec<Ipv4Addr> = part.to_vec();
        let me = me.to_string();
        let mine = local_ipv4s();
        handles.push(thread::spawn(move || {
            part.into_iter()
                .filter_map(|ip| probe_health(SocketAddr::from((ip, port)), &me, &mine))
                .collect::<Vec<Peer>>()
        }));
    }
    handles
        .into_iter()
        .filter_map(|h| h.join().ok())
        .flatten()
        .collect()
}

/// 探测单个地址的 /health:是 MicSync 且不是自己才算数。
/// `mine` 是本机 IPv4,用于给没有 device_id 的旧版服务端兜底判断「是不是自己」
fn probe_health(addr: SocketAddr, me: &str, mine: &[Ipv4Addr]) -> Option<Peer> {
    let mut s = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).ok()?;
    s.set_read_timeout(Some(PROBE_TIMEOUT)).ok()?;
    s.set_write_timeout(Some(PROBE_TIMEOUT)).ok()?;
    s.write_all(b"GET /health HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n")
        .ok()?;

    // 响应很小(几百字节),读满 8KB 或对端关闭为止
    let mut resp = Vec::new();
    let mut buf = [0u8; 1024];
    while resp.len() < 8192 {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => resp.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8(resp).ok()?;
    let body = text.split("\r\n\r\n").nth(1)?;
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("app")?.as_str()? != "micsync" {
        return None;
    }
    // 0.5.0 之前的服务端 /health 里没有 device_id。不能因此把它们当作
    // 「不是 MicSync」丢掉——那样混版本局域网(如 Mac 已更新、iPhone 侧载的还是旧版)
    // 会搜不到却又手输可连,徒增困惑。退回按本机 IP 判断是不是自己。
    let device_id = match v.get("device_id").and_then(|f| f.as_str()) {
        Some(f) if f == me => return None, // 扫到了自己
        Some(f) => f.to_string(),
        None => {
            if let IpAddr::V4(v4) = addr.ip() {
                if mine.contains(&v4) {
                    return None;
                }
            }
            String::new()
        }
    };
    Some(Peer {
        alias: v
            .get("alias")
            .and_then(|a| a.as_str())
            .unwrap_or("MicSync")
            .to_string(),
        addr: addr.to_string(),
        device_type: v
            .get("device_type")
            .and_then(|d| d.as_str())
            .unwrap_or("desktop")
            .to_string(),
        authorized: settings::token_for_server(&device_id).is_some(),
        device_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn device_id_is_stable_within_process() {
        assert_eq!(device_id(), device_id());
        assert!(!device_id().is_empty());
    }

    #[test]
    fn query_roundtrip() {
        let buf = encode_query("abc123");
        assert_eq!(
            parse_msg(&buf),
            Some(Msg::Query {
                device_id: "abc123".into()
            })
        );
    }

    #[test]
    fn announce_roundtrip() {
        let buf = encode_announce("我的 Mac", "fp1", 47800);
        assert_eq!(
            parse_msg(&buf),
            Some(Msg::Announce {
                alias: "我的 Mac".into(),
                device_type: device_type().into(),
                device_id: "fp1".into(),
                port: 47800,
            })
        );
    }

    /// 网络上什么包都可能打到这个端口,解析必须只认自己的
    #[test]
    fn parse_rejects_foreign_and_malformed() {
        assert!(parse_msg(b"not json").is_none());
        assert!(parse_msg(br#"{"app":"localsend","v":1,"query":true,"device_id":"x"}"#).is_none());
        // 版本不符
        assert!(parse_msg(br#"{"app":"micsync","v":99,"query":true,"device_id":"x"}"#).is_none());
        // 缺字段
        assert!(parse_msg(br#"{"app":"micsync","v":1,"query":false}"#).is_none());
    }

    #[test]
    fn subnet_hosts_covers_24_excluding_self() {
        let hosts = subnet_hosts(Ipv4Addr::new(192, 168, 1, 10));
        assert_eq!(hosts.len(), 253, "1..=254 去掉自己");
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 1, 1)));
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 1, 254)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 1, 10)), "不扫自己");
        assert!(
            !hosts.contains(&Ipv4Addr::new(192, 168, 1, 255)),
            "不扫广播地址"
        );
    }

    /// 起一个假 /health 服务,验证探测能认出 MicSync 并带回 alias
    fn spawn_fake_health(body: &'static str) -> u16 {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in l.incoming().take(1) {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        port
    }

    fn probe_local(port: u16, me: &str) -> Option<Peer> {
        probe_health(SocketAddr::from((Ipv4Addr::LOCALHOST, port)), me, &[])
    }

    #[test]
    fn probe_identifies_micsync_server() {
        let port = spawn_fake_health(
            r#"{"status":"ok","app":"micsync","alias":"张三的 Mac","device_type":"desktop","device_id":"remote-fp"}"#,
        );
        let peer = probe_local(port, "my-fp").expect("应认出 micsync 服务端");
        assert_eq!(peer.alias, "张三的 Mac");
        assert_eq!(peer.device_id, "remote-fp");
        assert_eq!(peer.addr, format!("127.0.0.1:{port}"));
    }

    /// 桌面端自己就是服务端,扫描一定会扫到自己——必须靠 device_id 摘掉
    #[test]
    fn probe_filters_self_by_device_id() {
        let port = spawn_fake_health(
            r#"{"status":"ok","app":"micsync","alias":"我自己","device_type":"desktop","device_id":"same-fp"}"#,
        );
        assert!(
            probe_local(port, "same-fp").is_none(),
            "不应把自己列进发现结果"
        );
    }

    /// 老版本服务端(0.5.0 前)的 /health 没有 device_id,仍应能被发现,
    /// 否则混版本局域网里会「搜不到但手输能连」
    #[test]
    fn probe_finds_legacy_server_without_fingerprint() {
        let port = spawn_fake_health(r#"{"status":"ok","app":"micsync","streaming":false}"#);
        let peer = probe_local(port, "my-fp").expect("老版本服务端也应被发现");
        assert_eq!(peer.alias, "MicSync", "没有 alias 时退化成通用名");
        assert!(peer.device_id.is_empty());
    }

    /// 老版本没有 device_id 时,靠本机 IP 兜底认出自己
    #[test]
    fn probe_filters_legacy_self_by_local_ip() {
        let port = spawn_fake_health(r#"{"status":"ok","app":"micsync","streaming":false}"#);
        let mine = [Ipv4Addr::LOCALHOST];
        assert!(
            probe_health(
                SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                "my-fp",
                &mine
            )
            .is_none(),
            "本机 IP 上的老版本服务端就是自己"
        );
    }

    /// 局域网里的其他 HTTP 服务不该被误认成 MicSync
    #[test]
    fn probe_ignores_non_micsync_http() {
        let port = spawn_fake_health(r#"{"status":"ok","app":"something-else"}"#);
        assert!(probe_local(port, "my-fp").is_none());
    }

    #[test]
    fn probe_ignores_closed_port() {
        // 绑了又立刻释放的端口,基本可以认定没人监听
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        assert!(probe_local(port, "my-fp").is_none());
    }
}
