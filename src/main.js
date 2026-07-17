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
      await showLocalIps(status.port);
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

function applyServerStatus(s) {
  serverRunning = !!s.running;
  const btn = $("btn-server-toggle");
  btn.textContent = serverRunning ? "⏹ 停止共享" : "▶ 启动共享";
  btn.classList.toggle("running", serverRunning);
  $("server-info").classList.toggle("hidden", !serverRunning);
  $("input-device").disabled = serverRunning;
  $("server-port").disabled = serverRunning;
  if (serverRunning) {
    $("server-summary").textContent = `共享中 · ${s.device}`;
    $("server-mic").textContent = s.mic_active ? "🎙 激活(说话中)" : "静音待机";
    $("server-rate").textContent = `${s.sample_rate} Hz`;
    const streaming = !!s.stream_addr;
    $("stream-dot").className =
      "dot " + (streaming ? "green" : s.mic_active ? "yellow" : "gray");
    $("server-stream").textContent = streaming
      ? `正在串流 → ${s.stream_addr}`
      : s.mic_active
        ? "已激活 · 等待客户端认领串流"
        : "串流空闲 · 等待客户端认领";
    $("server-level").style.width = `${Math.min(100, s.level * 130)}%`;
    if (s.error) showError("server-error", s.error);
  } else {
    $("server-level").style.width = "0%";
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
  btn.textContent = clientConnected ? "⏹ 停止监听" : "👂 开始监听";
  btn.classList.toggle("running", clientConnected);
  $("client-info").classList.toggle("hidden", !clientConnected && !s.error);
  $("server-addr").disabled = clientConnected;
  $("output-device").disabled = clientConnected;
  if (clientConnected) {
    const view =
      {
        streaming: ["green", `正在接收 ${s.addr} → ${s.output_device}`],
        standby: ["yellow", `待机中 · 等待 ${s.addr} 的麦克风激活`],
        offline: ["red", `服务端不可达 · 自动重试中 (${s.addr})`],
      }[s.mode] || ["red", "未知状态"];
    $("client-dot").className = "dot " + view[0];
    $("client-summary").textContent = view[1];
    $("client-buffer").textContent = `${s.buffer_ms} ms`;
    $("client-rate").textContent = `${s.sample_rate} Hz`;
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
    if (activeTab === "server" && serverRunning) {
      applyServerStatus(await invoke("server_status"));
    } else if (activeTab === "client" && clientConnected) {
      const s = await invoke("client_status");
      applyClientStatus(s);
    }
  } catch (e) {
    /* 轮询失败静默 */
  }
}, 200);

// ---------- 启动 ----------
(async function init() {
  const lastAddr = localStorage.getItem("last-server-addr");
  if (lastAddr) $("server-addr").value = lastAddr;
  await refreshDevices();
  // 设备热插拔:每 3 秒刷新一次设备列表(运行中不刷新以免打断选择)
  setInterval(() => {
    if (!serverRunning && !clientConnected) refreshDevices();
  }, 3000);
})();
