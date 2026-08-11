const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);

// ---------- 增量 DOM 写入 ----------
// 状态轮询每 100~300ms 跑一轮,大多数时候值根本没变;写入前先比对,
// 值不变就不碰 DOM——否则常驻后台的窗口会永远在触发样式重算与重绘
const setText = (el, s) => {
  if (el.__lastText !== s) {
    el.__lastText = s;
    el.textContent = s;
  }
};
const setClass = (el, s) => {
  if (el.__lastClass !== s) {
    el.__lastClass = s;
    el.className = s;
  }
};
const setWidth = (el, s) => {
  if (el.__lastWidth !== s) {
    el.__lastWidth = s;
    el.style.width = s;
  }
};

// 当前平台("macos" / "windows" / "ios" …);init 里从后端取,取不到按 macOS 处理
let OS = "macos";
// 是否移动端(iOS/Android):纯服务端形态,走大按钮 + 实时音波的专属布局
let MOBILE = false;

// 虚拟回环声卡:macOS 是 BlackHole,Windows 是 VB-Cable 的渲染端 CABLE Input
const isVirtualMic = (name) =>
  OS === "windows" ? name.includes("CABLE") : name.includes("BlackHole");

// ---------- Tab 切换 ----------
const tabs = { server: $("tab-server"), client: $("tab-client") };
const panels = { server: $("panel-server"), client: $("panel-client") };
let activeTab = "server";

function switchTab(name) {
  activeTab = name;
  for (const key of Object.keys(tabs)) {
    tabs[key].classList.toggle("active", key === name);
    panels[key].classList.toggle("hidden", key !== name);
  }
}
// 随切随记,重启后恢复到用户离开时的模式;旧后端没有这个命令,失败静默
const rememberMode = (mode) => invoke("set_last_mode", { mode }).catch(() => {});

tabs.server.addEventListener("click", () => {
  switchTab("server");
  rememberMode("server");
});
tabs.client.addEventListener("click", () => {
  switchTab("client");
  rememberMode("client");
  // 首次进客户端页自动搜一次:大多数情况下用户点进来就是要连服务端。
  // 只自动搜这一次——子网扫描会向 254 个地址发起连接,不适合挂在定时器上反复跑
  if (!discoveredOnce) {
    discoveredOnce = true;
    discoverServers();
  }
});

// ---------- 设备列表 ----------
async function refreshDevices() {
  try {
    const devices = await invoke("list_devices");
    fillSelect($("input-device"), devices.inputs);
    fillSelect($("output-device"), devices.outputs, isVirtualMic);
    const missing = !devices.virtual_mic_installed;
    $("blackhole-banner").classList.toggle("hidden", OS === "windows" || !missing);
    $("vbcable-banner").classList.toggle("hidden", OS !== "windows" || !missing);
  } catch (e) {
    console.error("list_devices failed", e);
  }
}

function fillSelect(select, names, preferFn) {
  // 设备列表每 3 秒轮询一次,没变就不重建 <option>——既省 DOM churn,
  // 也避免把用户正展开的下拉框轰掉
  const sig = names.join("\u0000");
  if (select.__lastSig === sig) return;
  select.__lastSig = sig;
  const prev = select.value;
  select.innerHTML = "";
  let preferred = -1;
  names.forEach((name, i) => {
    const opt = document.createElement("option");
    opt.value = name;
    opt.textContent = name;
    select.appendChild(opt);
    if (preferred < 0 && preferFn && preferFn(name)) preferred = i;
  });
  if (prev && names.includes(prev)) {
    select.value = prev;
  } else if (preferred >= 0) {
    select.selectedIndex = preferred;
  }
}

// ---------- 服务端 ----------
let serverRunning = false;
let lastIpsPort = null;

// 桌面的暂停/恢复按钮与移动端的大麦克风按钮共用这份开关逻辑
async function toggleServer() {
  const btns = [$("btn-server-toggle"), $("btn-mic")];
  btns.forEach((b) => (b.disabled = true));
  hideError("server-error");
  try {
    if (!serverRunning) {
      const port = parseInt($("server-port").value, 10) || 47800;
      const status = await invoke("start_server", {
        device: $("input-device").value || null,
        port,
      });
      applyServerStatus(status);
    } else {
      await invoke("stop_server");
      applyServerStatus({ running: false });
    }
  } catch (e) {
    showError("server-error", e);
  } finally {
    btns.forEach((b) => (b.disabled = false));
  }
}
$("btn-server-toggle").addEventListener("click", toggleServer);

// 更换共享设备即时生效(对下一次使用生效)
$("input-device").addEventListener("change", async () => {
  if (!serverRunning) return;
  try {
    await invoke("set_input_device", { device: $("input-device").value || null });
  } catch (e) {
    showError("server-error", e);
  }
});

function applyServerStatus(s) {
  serverRunning = !!s.running;
  applyPending(s.pending);
  if (MOBILE) applyMobileStatus(s);
  const btn = $("btn-server-toggle");
  setText(btn, serverRunning ? "⏸ 暂停服务" : "▶ 恢复服务");
  btn.classList.toggle("running", serverRunning);
  $("server-info").classList.toggle("hidden", !serverRunning);
  $("server-port").disabled = serverRunning;
  if (serverRunning) {
    setText($("server-summary"), `服务运行中 · 端口 ${s.port}`);
    const streaming = !!s.stream_addr;
    setClass($("stream-dot"), "dot " + (streaming ? "green" : "gray"));
    setText(
      $("server-stream"),
      streaming
        ? `🎙 麦克风使用中 → ${s.stream_addr}`
        : "麦克风空闲 · 未开麦,等待客户端请求"
    );
    setText($("server-device"), s.device || "系统默认");
    setText($("server-rate"), s.sample_rate ? `${s.sample_rate} Hz` : "—");
    setWidth($("server-level"), `${Math.min(100, s.level * 130)}%`);
    if (s.port !== lastIpsPort) {
      lastIpsPort = s.port;
      showLocalIps(s.port);
    }
    if (s.error) showError("server-error", s.error);
  } else {
    lastIpsPort = null;
    setWidth($("server-level"), "0%");
    if (s.error) showError("server-error", s.error);
  }
}

async function showLocalIps(port) {
  try {
    const ips = await invoke("local_ips");
    const wrap = $(MOBILE ? "mobile-ips" : "server-ips");
    if (MOBILE) $("mobile-conn").classList.toggle("hidden", !ips.length);
    wrap.innerHTML = "";
    if (!ips.length) {
      wrap.textContent = "未检测到局域网 IP";
      return;
    }
    ips.forEach((ip) => {
      const chip = document.createElement("span");
      chip.className = "ip-chip";
      chip.textContent = `${ip}:${port}`;
      chip.title = "点击复制";
      chip.addEventListener("click", () => {
        navigator.clipboard.writeText(`${ip}:${port}`);
        chip.textContent = "✓ 已复制";
        setTimeout(() => (chip.textContent = `${ip}:${port}`), 1000);
      });
      wrap.appendChild(chip);
    });
  } catch (e) {
    console.error("local_ips failed", e);
  }
}

// ---------- 移动端(iOS/Android):大按钮 + 真实收音音波 ----------
let mobileStreaming = false;
let mobileRunning = false;

// 波形数据:后端每块音频(~10ms)上报一个峰值,轮询按 wave_seq 增量对齐;
// 本地只做平滑滚动,不生造数据——画出来的是真实收音包络
const wv = {
  bars: [],
  total: 0,
  shown: 0,
  rate: 60,
  lastSeq: 0,
  lastPoll: 0,
  lastFrame: 0,
  lastIdle: 0,
  grad: null,
  gradH: 0,
};
const WV_MAX = 480; // 本地保留的峰值条数上限,够铺满屏幕

function ingestWave(arr, seq) {
  if (!Array.isArray(arr) || typeof seq !== "number") return;
  let fresh = seq - wv.lastSeq;
  wv.lastSeq = seq;
  if (fresh <= 0 || !arr.length) return;
  if (fresh > arr.length) fresh = arr.length;
  for (let i = arr.length - fresh; i < arr.length; i++) wv.bars.push(arr[i]);
  wv.total += fresh;
  if (wv.bars.length > WV_MAX) wv.bars.splice(0, wv.bars.length - WV_MAX);
  // 首次接入或轮询卡顿后落后太多,直接跳到最新,避免长时间快进
  if (wv.total - wv.shown > 90) wv.shown = wv.total - 30;
}

function resetWave() {
  wv.bars = [];
  wv.total = 0;
  wv.shown = 0;
  wv.lastPoll = 0;
}

function drawWave(ts) {
  requestAnimationFrame(drawWave);
  const canvas = $("wave-canvas");
  const dpr = window.devicePixelRatio || 1;
  const cw = canvas.clientWidth;
  const chh = canvas.clientHeight;
  if (!cw || !chh) return;
  const W = Math.round(cw * dpr);
  const H = Math.round(chh * dpr);
  if (canvas.width !== W || canvas.height !== H) {
    canvas.width = W;
    canvas.height = H;
  }
  const ctx = canvas.getContext("2d");
  const midY = H / 2;
  const dt = wv.lastFrame ? Math.min(0.1, (ts - wv.lastFrame) / 1000) : 0;
  wv.lastFrame = ts;

  if (!mobileStreaming) {
    // 空闲基线:待命时轻微呼吸,暂停时暗淡定格。
    // 呼吸动画 ~15fps 就够顺滑,不值得为它常驻 60fps 重绘耗电
    if (ts - wv.lastIdle < 66) return;
    wv.lastIdle = ts;
    ctx.clearRect(0, 0, W, H);
    ctx.globalAlpha = mobileRunning ? 0.22 + 0.12 * Math.sin(ts / 800) : 0.1;
    ctx.fillStyle = "#8b90a0";
    for (let x = 8 * dpr; x < W - 8 * dpr; x += 9 * dpr) {
      ctx.beginPath();
      ctx.arc(x, midY, 1.6 * dpr, 0, Math.PI * 2);
      ctx.fill();
    }
    ctx.globalAlpha = 1;
    return;
  }

  ctx.clearRect(0, 0, W, H);
  // 平滑追赶最新数据:速度与落后量成正比,把轮询到达的抖动吃掉
  wv.shown += (wv.total - wv.shown) * Math.min(1, dt * 6);

  const slot = 3.5 * dpr;
  const rightX = W - 12 * dpr;
  const base = wv.total - wv.bars.length; // bars[0] 的绝对序号
  // 渐变对象随画布高度缓存,不逐帧重建
  if (!wv.grad || wv.gradH !== H) {
    wv.gradH = H;
    const grad = ctx.createLinearGradient(0, 0, 0, H);
    grad.addColorStop(0, "#5b8cff");
    grad.addColorStop(0.5, "#3ddc84");
    grad.addColorStop(1, "#5b8cff");
    wv.grad = grad;
  }
  ctx.strokeStyle = wv.grad;
  ctx.lineWidth = 2.2 * dpr;
  ctx.lineCap = "round";
  const maxH = midY - 6 * dpr;
  for (let j = 0; j < wv.bars.length; j++) {
    const x = rightX - (wv.shown - (base + j)) * slot;
    if (x < -slot) continue;
    if (x > W) break;
    // 0.6 次方拉高小信号,轻声说话也看得见起伏
    const bh = Math.max(1.8 * dpr, Math.pow(wv.bars[j], 0.6) * maxH);
    // 左右两端淡出
    ctx.globalAlpha = Math.max(0.08, Math.min(1, x / (70 * dpr), (W - x) / (26 * dpr)));
    ctx.beginPath();
    ctx.moveTo(x, midY - bh);
    ctx.lineTo(x, midY + bh);
    ctx.stroke();
  }
  ctx.globalAlpha = 1;
}

function applyMobileStatus(s) {
  const streaming = !!(s.running && s.stream_addr);
  mobileRunning = !!s.running;
  if (streaming !== mobileStreaming) {
    mobileStreaming = streaming;
    if (!streaming) resetWave();
  }
  if (streaming) ingestWave(s.wave, s.wave_seq);
  // 收流时 100ms 轮询喂波形;空闲降到 300ms,省下常驻的 IPC 与重绘
  setPollInterval(streaming ? 100 : 300);
  const btn = $("btn-mic");
  btn.classList.toggle("live", streaming);
  btn.classList.toggle("armed", mobileRunning && !streaming);
  btn.classList.toggle("paused", !mobileRunning);
  const dot = $("mobile-dot");
  const sum = $("mobile-summary");
  const sub = $("mobile-sub");
  if (!mobileRunning) {
    setClass(dot, "dot gray");
    setText(sum, "共享已暂停");
    setText(sub, "点上方按钮恢复共享,电脑才能搜到并使用这台手机的麦克风。");
    $("mobile-conn").classList.add("hidden");
  } else if (streaming) {
    setClass(dot, "dot green");
    setText(sum, "麦克风使用中");
    setText(
      sub,
      `正在收音,实时传给 ${s.stream_addr}` + (s.sample_rate ? ` · ${s.sample_rate} Hz` : "")
    );
  } else {
    setClass(dot, "dot blue");
    setText(sum, `待命中 · 端口 ${s.port}`);
    setText(sub, "现在完全不收音。电脑端一发起使用,这里会自动开麦并跳出实时音波。");
  }
}

function setupMobile() {
  MOBILE = true;
  document.body.classList.add("mobile");
  document.querySelector(".subtitle").textContent =
    OS === "ios" ? "把 iPhone 的麦克风共享给局域网里的电脑" : "把手机的麦克风共享给局域网里的电脑";
  // 纯服务端形态:藏起桌面双栏,启用移动端专属面板
  panels.server.classList.add("hidden");
  panels.client.classList.add("hidden");
  $("panel-mobile").classList.remove("hidden");
  // 桌面面板里的设备/端口控件和错误条整体挪过来:同一套元素,免状态同步
  $("mobile-advanced-body").appendChild($("server-controls"));
  $("panel-mobile").appendChild($("server-error"));
  $("btn-mic").addEventListener("click", toggleServer);
  // 轮询频率由 applyMobileStatus 按需切换:收流 100ms 喂波形,空闲 300ms
  setPollInterval(300);
  requestAnimationFrame(drawWave);
}

// ---------- 设备名 ----------
// 对方在授权弹窗里看到的就是这个名字,所以值得让用户能改
async function loadDeviceName() {
  try {
    const info = await invoke("device_info");
    $("device-name").value = info.name;
  } catch (e) {
    console.error("device_info failed", e);
  }
}

// 失焦时保存(而非每次按键):后端会清洗并在空值时退回主机名,用返回值回填
$("device-name").addEventListener("blur", async () => {
  try {
    $("device-name").value = await invoke("set_device_name", {
      name: $("device-name").value,
    });
  } catch (e) {
    showError("server-error", e);
  }
});
$("device-name").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("device-name").blur();
});

// ---------- 授权确认 ----------
// 有人请求使用本机麦克风时弹出。服务端此刻仍未开麦——同意之后才会开
let consentShown = false;

function applyPending(p) {
  const overlay = $("consent-overlay");
  if (!p) {
    overlay.classList.add("hidden");
    consentShown = false;
    return;
  }
  // 已经在显示同一个请求就别重复刷 DOM
  if (!consentShown) {
    $("consent-name").textContent = p.name;
    $("consent-addr").textContent = p.addr;
    // 预授权(自动跟随开启时)与立即开麦的请求,同意的含义不同,文案要说清
    $("consent-verb").textContent =
      p.kind === "authorize"
        ? "开启了自动跟随,想在需要时使用这台设备的麦克风"
        : "想使用这台设备的麦克风";
    overlay.classList.remove("hidden");
    consentShown = true;
  }
}

async function decide(allow, remember) {
  // 先收起弹窗,避免用户狂点导致重复裁决
  $("consent-overlay").classList.add("hidden");
  consentShown = false;
  try {
    await invoke("decide_request", { allow, remember });
    if (allow && remember) loadTrusted();
  } catch (e) {
    showError("server-error", e);
  }
}

$("btn-consent-deny").addEventListener("click", () => decide(false, false));
$("btn-consent-once").addEventListener("click", () => decide(true, false));
$("btn-consent-always").addEventListener("click", () => decide(true, true));

// ---------- 已授权设备 ----------
async function loadTrusted() {
  try {
    const list = await invoke("trusted_devices");
    const wrap = $("trusted-list");
    $("trusted-card").classList.toggle("hidden", !list.length);
    wrap.innerHTML = "";
    list.forEach((t) => {
      const row = document.createElement("div");
      row.className = "trusted-row";
      const text = document.createElement("div");
      const name = document.createElement("div");
      name.className = "trusted-name";
      name.textContent = t.name;
      const meta = document.createElement("div");
      meta.className = "trusted-meta";
      meta.textContent = `授权于 ${new Date(t.added_at * 1000).toLocaleString()}`;
      text.append(name, meta);
      const btn = document.createElement("button");
      btn.className = "btn tiny";
      btn.textContent = "撤销";
      btn.addEventListener("click", async () => {
        try {
          await invoke("revoke_trusted", { token: t.token });
          loadTrusted();
        } catch (e) {
          showError("server-error", e);
        }
      });
      row.append(text, btn);
      wrap.appendChild(row);
    });
  } catch (e) {
    console.error("trusted_devices failed", e);
  }
}

// ---------- 服务发现 ----------
// 已发现的服务端;点一下就把地址填进输入框,省得手输 IP
let discovering = false;
let discoveredOnce = false;

async function discoverServers() {
  if (discovering) return;
  discovering = true;
  const btn = $("btn-discover");
  btn.disabled = true;
  btn.textContent = "搜索中…";
  const hint = $("discover-hint");
  hint.classList.add("hidden");
  try {
    const port = parseInt($("server-port").value, 10) || 47800;
    const peers = await invoke("discover_servers", { port });
    renderPeers(peers);
  } catch (e) {
    console.error("discover_servers failed", e);
    hint.textContent = `搜索失败: ${e}`;
    hint.classList.remove("hidden");
  } finally {
    discovering = false;
    btn.disabled = false;
    btn.textContent = "🔍 搜索";
  }
}

function renderPeers(peers) {
  const list = $("discover-list");
  const hint = $("discover-hint");
  list.innerHTML = "";
  if (!peers.length) {
    list.classList.add("hidden");
    hint.textContent =
      "没搜到服务端。确认对方已打开 MicSync 且在同一 Wi-Fi/网段;仍搜不到就手动填写地址。";
    hint.classList.remove("hidden");
    return;
  }
  hint.classList.add("hidden");
  list.classList.remove("hidden");
  peers.forEach((p) => {
    const row = document.createElement("div");
    row.className = "peer";
    if (p.addr === $("server-addr").value.trim()) row.classList.add("selected");
    const icon = document.createElement("span");
    icon.className = "peer-icon";
    icon.textContent = p.device_type === "mobile" ? "📱" : "💻";
    const text = document.createElement("div");
    text.className = "peer-text";
    const alias = document.createElement("span");
    alias.className = "peer-alias";
    alias.textContent = p.alias;
    const addr = document.createElement("span");
    addr.className = "peer-addr";
    addr.textContent = p.authorized ? `${p.addr} · 已授权` : `${p.addr} · 需对方确认`;
    text.append(alias, addr);
    row.append(icon, text);
    row.addEventListener("click", () => {
      // 连接/跟随进行中时地址锁定,避免改了地址却还连着旧的
      if (clientConnected || followRunning) return;
      $("server-addr").value = p.addr;
      localStorage.setItem("last-server-addr", p.addr);
      for (const el of list.children) el.classList.remove("selected");
      row.classList.add("selected");
    });
    list.appendChild(row);
  });
}

$("btn-discover").addEventListener("click", discoverServers);

// ---------- 客户端 ----------
let clientConnected = false;
let followRunning = false;

// 缓冲行顺带带出抖动与欠载:一眼看出当前延迟是吸收网络抖动换来的,
// 还是网络明明很稳却白垫着;欠载>0 说明水位曾不够,声音断过
const bufferLine = (buf, jitter, underruns) =>
  `${buf || 0} ms` +
  (jitter ? ` · 抖动 ${jitter} ms` : "") +
  (underruns ? ` · 欠载 ${underruns} 次` : "");

// 自动跟随:检测到本机应用使用 BlackHole 时自动认领远端麦克风
$("btn-follow-toggle").addEventListener("click", async () => {
  const btn = $("btn-follow-toggle");
  btn.disabled = true;
  hideError("client-error");
  try {
    if (!followRunning) {
      const addr = $("server-addr").value.trim();
      if (!addr) throw "请输入服务端地址";
      const status = await invoke("start_follow", {
        addr,
        outputDevice: $("output-device").value || null,
      });
      localStorage.setItem("last-server-addr", addr);
      applyFollowStatus(status);
    } else {
      await invoke("stop_follow");
      applyFollowStatus({ running: false, client: {} });
      applyClientStatus({ connected: false });
    }
  } catch (e) {
    showError("client-error", e);
  } finally {
    btn.disabled = false;
  }
});

function applyFollowStatus(s) {
  followRunning = !!s.running;
  const btn = $("btn-follow-toggle");
  btn.textContent = followRunning ? "⏹ 停止自动跟随" : "⚡ 自动跟随本机应用";
  btn.classList.toggle("running", followRunning);
  // 自动/手动互斥展示
  $("btn-client-toggle").classList.toggle("hidden", followRunning);
  $("server-addr").disabled = followRunning || clientConnected;
  $("output-device").disabled = followRunning || clientConnected;
  if (!followRunning) return;
  $("client-info").classList.remove("hidden");
  const c = s.client || {};
  const views = {
    authorizing: ["yellow", `正在请求 ${s.addr} 授权 · 等待对方确认…`],
    auth_denied: ["red", "对方未同意授权,自动跟随已停止"],
    armed: ["gray", `自动待命 · 本机应用一用麦克风就自动接入(${s.addr})`],
    active:
      c.mode === "streaming"
        ? ["green", `本机应用使用麦克风中 · 正在使用 ${s.addr} 的麦克风`]
        : c.mode === "waiting"
          ? ["yellow", "检测到本机应用使用麦克风 · 等待对方确认授权…"]
          : c.mode === "denied"
            ? ["red", "对方未同意使用其麦克风"]
            : ["yellow", "检测到本机应用使用麦克风 · 正在连接服务端…"],
    suppressed: ["yellow", "已被其他设备接管 · 本轮结束后恢复自动待命"],
    draining: ["gray", "本轮使用结束 · 应用停止采集后恢复自动待命"],
    device_missing: [
      "red",
      OS === "windows"
        ? "未找到 CABLE Input 输出设备,自动跟随不可用"
        : "未找到 BlackHole 输出设备,自动跟随不可用",
    ],
    unsupported: [
      "red",
      OS === "windows"
        ? "自动跟随不可用,请用手动模式"
        : "系统不支持自动跟随(需 macOS 14+),请用手动模式",
    ],
  };
  const view = views[s.phase] || ["red", "未知状态"];
  setClass($("client-dot"), "dot " + view[0]);
  setText($("client-summary"), view[1]);
  setText($("client-buffer"), bufferLine(c.buffer_ms, c.jitter_ms, c.underruns));
  setText($("client-rate"), c.sample_rate ? `${c.sample_rate} Hz` : "—");
  setWidth($("client-level"), `${Math.min(100, (c.level || 0) * 130)}%`);
  if (s.error) showError("client-error", s.error);
  else hideError("client-error");
}

$("btn-client-toggle").addEventListener("click", async () => {
  const btn = $("btn-client-toggle");
  btn.disabled = true;
  hideError("client-error");
  try {
    if (!clientConnected) {
      const addr = $("server-addr").value.trim();
      if (!addr) throw "请输入服务端地址";
      const status = await invoke("connect_client", {
        addr,
        outputDevice: $("output-device").value || null,
      });
      applyClientStatus(status);
      localStorage.setItem("last-server-addr", addr);
    } else {
      await invoke("disconnect_client");
      applyClientStatus({ connected: false });
    }
  } catch (e) {
    showError("client-error", e);
  } finally {
    btn.disabled = false;
  }
});

function applyClientStatus(s) {
  clientConnected = !!s.connected;
  const btn = $("btn-client-toggle");
  btn.textContent = clientConnected ? "⏹ 停止使用" : "🎙 手动使用这个麦克风";
  btn.classList.toggle("running", clientConnected);
  $("btn-follow-toggle").classList.toggle("hidden", clientConnected);
  $("client-info").classList.toggle("hidden", !clientConnected && !s.error);
  $("server-addr").disabled = clientConnected || followRunning;
  $("output-device").disabled = clientConnected || followRunning;
  if (clientConnected) {
    const view =
      {
        streaming: ["green", `正在使用 ${s.addr} 的麦克风 → ${s.output_device}`],
        waiting: ["yellow", "等待对方确认授权…"],
        connecting: ["yellow", `连接 ${s.addr} 中 · 自动重试`],
        ended: ["red", "本次使用已结束"],
        denied: ["red", "对方未同意本次使用"],
      }[s.mode] || ["red", "未知状态"];
    setClass($("client-dot"), "dot " + view[0]);
    setText($("client-summary"), view[1]);
    setText($("client-buffer"), bufferLine(s.buffer_ms, s.jitter_ms, s.underruns));
    setText($("client-rate"), s.sample_rate ? `${s.sample_rate} Hz` : "—");
    setWidth($("client-level"), `${Math.min(100, s.level * 130)}%`);
    if (s.error) showError("client-error", s.error);
    else hideError("client-error");
  } else {
    setWidth($("client-level"), "0%");
    if (s.error) {
      setClass($("client-dot"), "dot red");
      setText($("client-summary"), "已停止");
      showError("client-error", s.error);
    }
  }
}

// ---------- 虚拟声卡安装提示 ----------
for (const id of ["btn-download-blackhole", "btn-download-vbcable"]) {
  $(id).addEventListener("click", async () => {
    try {
      await invoke("open_driver_download");
    } catch (e) {
      showError("client-error", e);
    }
  });
}

$("btn-copy-brew").addEventListener("click", () => {
  navigator.clipboard.writeText("brew install blackhole-2ch");
  $("btn-copy-brew").textContent = "✓ 已复制";
  setTimeout(() => ($("btn-copy-brew").textContent = "复制命令"), 1200);
});

// ---------- 错误提示 ----------
function showError(id, msg) {
  const el = $(id);
  setText(el, typeof msg === "string" ? msg : JSON.stringify(msg));
  el.classList.remove("hidden");
}

function hideError(id) {
  $(id).classList.add("hidden");
}

// ---------- 状态轮询 ----------
async function pollStatus() {
  try {
    // 服务端状态无论停在哪个 tab 都要拉:授权确认弹窗是全局的,
    // 只在 server tab 轮询会让「界面停在客户端页的服务端」永远弹不出确认。
    // 波形序列只有移动端画,桌面端不必每轮拷贝+序列化 150 个峰值
    applyServerStatus(await invoke("server_status", { wave: MOBILE }));
    if (activeTab !== "server") {
      const fs = await invoke("follow_status");
      if (fs.running || followRunning) applyFollowStatus(fs);
      if (!fs.running && clientConnected) {
        applyClientStatus(await invoke("client_status"));
      }
    }
  } catch (e) {
    /* 轮询失败静默 */
  }
}
let pollMs = 200;
let pollTimer = setInterval(pollStatus, pollMs);

// 轮询频率随场景切换(移动端收流 100ms / 空闲 300ms;桌面恒 200ms)
function setPollInterval(ms) {
  if (pollMs === ms) return;
  pollMs = ms;
  clearInterval(pollTimer);
  pollTimer = setInterval(pollStatus, ms);
}

// ---------- 启动 ----------
(async function init() {
  try {
    OS = await invoke("platform");
  } catch (e) {
    /* 旧后端没有 platform 命令,按桌面端(macOS)处理 */
  }
  // 移动端(iOS/Android)只有服务端形态:大按钮 + 实时音波的专属布局
  if (OS === "ios" || OS === "android") {
    setupMobile();
  }
  // Windows 上虚拟声卡是 VB-Cable,文案跟着换
  if (OS === "windows") {
    $("output-device-label").textContent = "输出到(选 CABLE Input 即虚拟麦克风)";
    $("follow-hint").textContent =
      "「自动跟随」:检测到本机有应用从 CABLE Output 录音(如进入会议)时,自动向服务端请求麦克风;应用停止采集约 2 秒后自动释放、远端关麦——全程无需手动点击。「手动使用」则立即请求、手动停止。两种模式下,另一台设备发起请求都会自动接管,本机随之停止——人在哪台设备,麦克风就跟到哪台。";
    $("client-usage-hint").textContent =
      "✅ 现在在 Zoom / 微信 / 腾讯会议等应用中选择「CABLE Output (VB-Audio Virtual Cable)」作为麦克风即可。";
  }
  await loadDeviceName();
  await loadTrusted();
  const lastAddr = localStorage.getItem("last-server-addr");
  if (lastAddr) $("server-addr").value = lastAddr;
  // 恢复上次退出时的模式:上次在客户端就回客户端;新装用户(无记录)也
  // 落在客户端 tab——不替用户默认进「共享麦克风」的形态
  if (!MOBILE) {
    try {
      const mode = await invoke("last_mode");
      if (mode !== "server") {
        switchTab("client");
        // 回到客户端模式的老用户,进来就是要连服务端,照进 tab 的惯例自动搜一次;
        // 新装用户没表达过意图,不替他扫描全网段
        if (mode === "client" && !discoveredOnce) {
          discoveredOnce = true;
          discoverServers();
        }
      }
    } catch (e) {
      /* 旧后端没有 last_mode 命令,维持服务端 tab 的旧默认 */
    }
  }
  // 同步一次服务端状态(是否已在监听由上次退出时的形态决定)
  try {
    applyServerStatus(await invoke("server_status", { wave: MOBILE }));
  } catch (e) {
    /* 忽略 */
  }
  await refreshDevices();
  // 设备热插拔:每 3 秒刷新一次设备列表(客户端使用中不刷新以免打断)
  setInterval(() => {
    if (!clientConnected) refreshDevices();
  }, 3000);
})();
