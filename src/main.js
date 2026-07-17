const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);

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
tabs.server.addEventListener("click", () => switchTab("server"));
tabs.client.addEventListener("click", () => switchTab("client"));

// ---------- 设备列表 ----------
async function refreshDevices() {
  try {
    const devices = await invoke("list_devices");
    fillSelect($("input-device"), devices.inputs);
    fillSelect($("output-device"), devices.outputs, (name) =>
      name.includes("BlackHole")
    );
    $("blackhole-banner").classList.toggle("hidden", devices.blackhole_installed);
  } catch (e) {
    console.error("list_devices failed", e);
  }
}

function fillSelect(select, names, preferFn) {
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

$("btn-server-toggle").addEventListener("click", async () => {
  const btn = $("btn-server-toggle");
  btn.disabled = true;
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
    btn.disabled = false;
  }
});

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
  const btn = $("btn-server-toggle");
  btn.textContent = serverRunning ? "⏸ 暂停服务" : "▶ 恢复服务";
  btn.classList.toggle("running", serverRunning);
  $("server-info").classList.toggle("hidden", !serverRunning);
  $("server-port").disabled = serverRunning;
  if (serverRunning) {
    $("server-summary").textContent = `服务运行中 · 端口 ${s.port}`;
    const streaming = !!s.stream_addr;
    $("stream-dot").className = "dot " + (streaming ? "green" : "gray");
    $("server-stream").textContent = streaming
      ? `🎙 麦克风使用中 → ${s.stream_addr}`
      : "麦克风空闲 · 未开麦,等待客户端请求";
    $("server-device").textContent = s.device || "系统默认";
    $("server-rate").textContent = s.sample_rate ? `${s.sample_rate} Hz` : "—";
    $("server-level").style.width = `${Math.min(100, s.level * 130)}%`;
    if (s.port !== lastIpsPort) {
      lastIpsPort = s.port;
      showLocalIps(s.port);
    }
    if (s.error) showError("server-error", s.error);
  } else {
    lastIpsPort = null;
    $("server-level").style.width = "0%";
    if (s.error) showError("server-error", s.error);
  }
}

async function showLocalIps(port) {
  try {
    const ips = await invoke("local_ips");
    const wrap = $("server-ips");
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

// ---------- 客户端 ----------
let clientConnected = false;
let followRunning = false;

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
    armed: ["gray", `自动待命 · 本机应用一用麦克风就自动接入(${s.addr})`],
    active:
      c.mode === "streaming"
        ? ["green", `本机应用使用麦克风中 · 正在使用 ${s.addr} 的麦克风`]
        : ["yellow", "检测到本机应用使用麦克风 · 正在连接服务端…"],
    suppressed: ["yellow", "已被其他设备接管 · 本轮结束后恢复自动待命"],
    device_missing: ["red", "未找到 BlackHole 输出设备,自动跟随不可用"],
    unsupported: ["red", "系统不支持自动跟随(需 macOS 14+),请用手动模式"],
  };
  const view = views[s.phase] || ["red", "未知状态"];
  $("client-dot").className = "dot " + view[0];
  $("client-summary").textContent = view[1];
  $("client-buffer").textContent = `${c.buffer_ms || 0} ms`;
  $("client-rate").textContent = c.sample_rate ? `${c.sample_rate} Hz` : "—";
  $("client-level").style.width = `${Math.min(100, (c.level || 0) * 130)}%`;
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
        connecting: ["yellow", `连接 ${s.addr} 中 · 自动重试`],
        ended: ["red", "本次使用已结束"],
      }[s.mode] || ["red", "未知状态"];
    $("client-dot").className = "dot " + view[0];
    $("client-summary").textContent = view[1];
    $("client-buffer").textContent = `${s.buffer_ms} ms`;
    $("client-rate").textContent = s.sample_rate ? `${s.sample_rate} Hz` : "—";
    $("client-level").style.width = `${Math.min(100, s.level * 130)}%`;
    if (s.error) showError("client-error", s.error);
    else hideError("client-error");
  } else {
    $("client-level").style.width = "0%";
    if (s.error) {
      $("client-dot").className = "dot red";
      $("client-summary").textContent = "已停止";
      showError("client-error", s.error);
    }
  }
}

// ---------- BlackHole 安装提示 ----------
$("btn-download-blackhole").addEventListener("click", async () => {
  try {
    await invoke("open_blackhole_download");
  } catch (e) {
    showError("client-error", e);
  }
});

$("btn-copy-brew").addEventListener("click", () => {
  navigator.clipboard.writeText("brew install blackhole-2ch");
  $("btn-copy-brew").textContent = "✓ 已复制";
  setTimeout(() => ($("btn-copy-brew").textContent = "复制命令"), 1200);
});

// ---------- 错误提示 ----------
function showError(id, msg) {
  const el = $(id);
  el.textContent = typeof msg === "string" ? msg : JSON.stringify(msg);
  el.classList.remove("hidden");
}

function hideError(id) {
  $(id).classList.add("hidden");
}

// ---------- 状态轮询 ----------
setInterval(async () => {
  try {
    if (activeTab === "server") {
      applyServerStatus(await invoke("server_status"));
    } else {
      const fs = await invoke("follow_status");
      if (fs.running || followRunning) applyFollowStatus(fs);
      if (!fs.running && clientConnected) {
        applyClientStatus(await invoke("client_status"));
      }
    }
  } catch (e) {
    /* 轮询失败静默 */
  }
}, 200);

// ---------- 启动 ----------
(async function init() {
  const lastAddr = localStorage.getItem("last-server-addr");
  if (lastAddr) $("server-addr").value = lastAddr;
  // 服务端随应用自动启动,立即同步一次状态
  try {
    applyServerStatus(await invoke("server_status"));
  } catch (e) {
    /* 忽略 */
  }
  await refreshDevices();
  // 设备热插拔:每 3 秒刷新一次设备列表(客户端使用中不刷新以免打断)
  setInterval(() => {
    if (!clientConnected) refreshDevices();
  }, 3000);
})();
