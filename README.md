# MicSync 🎙️

**English** | [简体中文](README.zh-CN.md)

**One microphone, shared across every Mac on your LAN.** The mic is physically plugged into one computer (the server); other computers (clients) on the local network use it as their own microphone through a virtual audio device — no more re-plugging cables.

## Why MicSync

Anyone with more than one Mac on their desk runs into the same annoyance: **there's only one good microphone**.

- Today's meeting is on the work MacBook, tomorrow's is on the personal Mac mini — the mic has to follow you around; frequently re-plugging a USB mic or audio interface is tedious and wears out ports
- The moment a Bluetooth headset enables its mic, audio quality drops to "phone call" level (HFP profile) — no headset can escape that
- macOS Continuity can only use an **iPhone** as a microphone; it cannot borrow the mic from another **Mac**
- Existing network-audio solutions (NDI, Dante, etc.) are heavyweight or paid, and most of them **stream continuously** — burning bandwidth and CPU even when nobody is talking

MicSync solves exactly one small problem: **make the microphone attached to one Mac appear as a "local microphone" on every other Mac in the LAN**. And it stays quiet: the server keeps the mic **fully closed with zero traffic at rest** — a client's one-click request opens the mic on demand, and stopping closes it immediately.

## Use cases

- **Two-Mac desk setup**: a work MacBook and a personal desktop Mac share one desk; the mic is plugged into one of them. When the other needs to join a meeting, just select BlackHole as the input — zero re-plugging
- **Rotating between Macs for meetings**: the same person joins calls from different Macs (switching between company and client environments); whichever Mac you sit at, one click on "Use this microphone" takes over — the newest request preempts the old stream and the previous device stops automatically
- **Sharing a high-quality recording chain**: a condenser mic + audio interface lives on your recording/streaming rig; the Mac next to it can borrow that chain's sound for an impromptu call
- **No double audio**: only one client receives the stream at any moment (newest request takes over, serial rotation) — the same voice never gets fed into two meetings at once

## Device topology

One **server Mac** (where the physical microphone lives) + N **client Macs**, all on the same LAN, in a star topology. Each client installs the [BlackHole](https://existential.audio/blackhole/) loopback driver as its "virtual microphone" outlet:

```
                        LAN (same subnet)
                              │
              ┌───────────────┴───────────────┐
              │         Server Mac × 1        │
              │  🎙️ physical mic (USB/built-in)│
              │  MicSync "Server" :47800       │
              └───────────────┬───────────────┘
    GET /health liveness      │  GET /stream (exclusive; newest request takes over)
       ┌──────────────┬───────┴───────┬──────────────┐
       │ Client Mac A │ Client Mac B  │ Client Mac C │
       │ BlackHole    │ BlackHole     │ BlackHole    │
       │ → Zoom       │ → WeChat      │ → Teams      │
       └──────────────┴───────────────┴──────────────┘
     Only one client holds the stream (newest request preempts)
```

The server only listens on the API; everything is driven by client request events. **At rest the microphone stays fully closed and no audio flows; the instant a client requests `/stream`, the server opens the mic on demand** and closes it when the client disconnects. A new request takes over the old stream — the microphone follows whichever device you are at. Data flow in detail:

```
┌─ Server Mac ─────────────────┐         ┌─ Client Mac × N ─────────────────────┐
│ listens on API from launch;  │  HTTP   │ user clicks "Use this microphone"    │
│ mic stays closed             │←────────│   ↓ GET /stream (the request event)  │
│                              │         │ server opens the mic, stream starts  │
│ /stream → open mic on demand │  HTTP   │   ↓ resample + jitter buffer         │
│   ↓ cpal capture             ├────────→│ play into BlackHole (loopback)       │
│ PCM 16-bit mono frames       │  LAN    │   ↓                                  │
│                              │         │ Zoom/WeChat/any meeting app selects  │
│ client leaves / preempted    │         │ "BlackHole 2ch" as microphone ✓      │
│   → mic closed immediately   │         │                                      │
└──────────────────────────────┘         └──────────────────────────────────────┘
```

## How to use

### Server (the Mac with the microphone)

1. Just open MicSync — it starts listening on the LAN automatically, no clicks needed; use the "📡 Server" tab to change the shared microphone device or pause the service
2. Tell the clients the `IP:port` shown in the UI (click to copy)

### Client (the Mac that wants to use that microphone)

1. Install the BlackHole virtual audio driver (one-time; the app has a guided button):
   - **Recommended**: get the free graphical installer from <https://existential.audio/blackhole/> (email required), then double-click the `.pkg` and follow the prompts — the GUI installer forces the admin authorization through and cannot fail silently
   - Alternative: `brew install blackhole-2ch` (note: if the sudo prompt is not completed during install, brew may report "installed" while the driver actually isn't; verify with `ls /Library/Audio/Plug-Ins/HAL/` and check that `BlackHole2ch.driver` exists)

   > ⚠️ After installation, **restart the Mac** (or run `sudo killall coreaudiod` to restart the audio service) before BlackHole shows up in the device list.
2. Open MicSync and switch to the "💻 Client" tab
3. Enter the server address, choose **BlackHole 2ch** as the output device, and click "Auto-follow" (or "Use this microphone" for manual mode)
4. In any app (Zoom / WeChat / Teams…), select **BlackHole 2ch** as the microphone

The client has two modes:

- **⚡ Auto-follow (recommended, macOS 14+)**: fully hands-free — MicSync watches BlackHole's usage state through the CoreAudio HAL; the moment a local app starts capturing from it (e.g. you join a meeting), the client automatically requests the microphone from the server; ~2 s after the app stops capturing, it releases automatically and the remote mic closes
- **🎙 Manual**: click to request immediately, click "Stop" to release

In both modes **only one device uses the mic at a time**: a request from another device takes over automatically and the previous one stops after being notified (auto-follow waits for the current local session to end before re-arming, so it never tugs the stream back) — the microphone follows you. Network blips and server restarts reconnect automatically.

## HTTP API

The server exposes two endpoints on its listening port (default `47800`):

| Endpoint | Description |
| --- | --- |
| `GET /health` | Liveness + status: `{"status":"ok","app":"micsync","streaming":false,"client":"","sample_rate":48000}` |
| `GET /stream` | Request the microphone (the event): the server opens the mic on demand and returns `200` (`application/octet-stream`, body is an `MSY1` header + length-prefixed PCM frames). If a stream already exists, **the new request takes over** and the old connection receives a zero-length end frame with a reason code (preempted / server closing); returns `503` if the mic fails to open |

## Development

```bash
# Dev run (requires Rust; the frontend is pure static pages, no npm install needed)
npx @tauri-apps/cli@latest dev

# Build .app / .dmg
npx @tauri-apps/cli@latest build
# Artifacts land in src-tauri/target/release/bundle/
```

On first run the server triggers macOS's microphone-permission prompt; a client's first connection triggers the local-network-permission prompt. Allow both.

## Technical notes

- **Tauri 2** + pure static frontend (no Node dependency)
- **cpal** for audio capture/playback (CoreAudio)
- Minimal HTTP server (hand-written on std, no framework): `/health` liveness + `/stream` streaming; stream body is an `MSY1` magic header + length-prefixed PCM i16 frames, `TCP_NODELAY` for low latency
- **Mic on demand**: at launch the server only listens on the API and never touches the microphone; each `/stream` session exclusively owns one cpal capture stream and releases it the moment the session ends (disconnect / preemption) — the system's mic indicator only lights up during actual use
- **Auto-follow detection** (client): while armed it reads `kAudioDevicePropertyDeviceIsRunningSomewhere` on the BlackHole device (the client holds no stream then, so it flipping true means "some app just started using the virtual mic"); while streaming its own playback keeps that property true, so it switches to the macOS 14+ Process Objects API (`kAudioHardwarePropertyProcessObjectList` — the data source behind the orange mic indicator) to ask "is any process other than me still capturing input". Local HAL property polls take ~3 ms, sampled every 300 ms
- **Preemptive single stream**: only one stream exists at a time; a new request takes over and the old client receives an explicit end frame and stops (it does not grab the stream back, avoiding tug-of-war between devices)
- Client uses a 60 ms start-playback watermark + 300 ms buffer cap (drops oldest data over the cap to preserve latency); streaming linear resampling handles any sample-rate combination
- Bandwidth during streaming is ~0.8 Mbps (48 kHz · 16-bit · mono); at rest there is zero audio traffic and zero mic usage
- The virtual microphone is provided by the [BlackHole](https://github.com/ExistentialAudio/BlackHole) loopback driver; a custom CoreAudio HAL driver (no BlackHole install needed) is a future direction

## Known limitations / roadmap

- One-time delay between requesting the mic and hearing audio: mic initialization (usually <1 s) + the 60 ms start watermark; once established the stream is continuous low latency
- Auto-follow's "stopped" signal is an approximation: which device another process captures from is privacy-hidden (`pdv#` returns empty), so if some other app is still capturing from any microphone (e.g. dictation), the remote mic stays held until it stops too
- Takeover is unconditional (any client on the LAN can preempt) and there is no encryption/authentication yet — use only on trusted LANs
- Possible next steps: Opus compression (cross-subnet / weak networks), mDNS auto-discovery of the server, custom HAL driver
