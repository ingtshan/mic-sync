# MicSync 🎙️

**English** | [简体中文](README.zh-CN.md)

**One microphone, shared across every computer on your LAN.** The mic is physically plugged into one computer (the server); other computers (clients) on the local network use it as their own microphone through a virtual audio device — no more re-plugging cables. Desktop builds support **macOS and Windows** (using BlackHole / VB-Cable respectively as the virtual device) and can be mixed freely; a companion Android server app is also included.

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
│ listens on API when enabled; │  HTTP   │ user clicks "Use this microphone"    │
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
2. Open MicSync and switch to the "💻 Client" tab — it automatically searches the LAN for servers; click one to fill in its address, no typing IPs (hit "🔍 Search" to re-scan, or just type the address manually)
3. Choose **BlackHole 2ch** as the output device and click "Auto-follow" (or "Use this microphone" for manual mode)
4. In any app (Zoom / WeChat / Teams…), select **BlackHole 2ch** as the microphone

The client has two modes:

- **⚡ Auto-follow (recommended, macOS 14+)**: fully hands-free — MicSync watches BlackHole's usage state through the CoreAudio HAL; the moment a local app starts capturing from it (e.g. you join a meeting), the client automatically requests the microphone from the server; ~2 s after the app stops capturing, it releases automatically and the remote mic closes
- **🎙 Manual**: click to request immediately, click "Stop" to release

In both modes **only one device uses the mic at a time**: a request from another device takes over automatically and the previous one stops after being notified (auto-follow waits for the current local session to end before re-arming, so it never tugs the stream back) — the microphone follows you. Network blips and server restarts reconnect automatically.

**Background running (macOS)**: closing the window does not quit the app — MicSync retreats to a menu-bar tray icon and keeps serving (the Dock icon is hidden meanwhile); both the server listener and client streams stay alive. Click the menu-bar icon to reopen the main window or quit for real.

**Mode memory**: MicSync remembers the mode you last quit in — quit as a client and the next launch opens as a client, with no server listening. Fresh installs likewise never start the server by default (silently becoming discoverable on the LAN right after install would be a surprise); the server starts listening only after you click resume, and from then on it is restored according to how you last quit.

### Windows PC (as server or client)

MicSync ships a Windows build (NSIS installer, built by the repo's GitHub Actions workflow); macOS and Windows machines can be mixed freely:

- **As server**: identical to macOS — listens while sharing is enabled, opens the mic only on request
- **As client**: the virtual device is the free [VB-Cable](https://vb-audio.com/Cable/) (the app has a guided button): download VBCABLE_Driver_Pack, extract, right-click `VBCABLE_Setup_x64.exe` → "Run as administrator", then **reboot**. VB-Cable is a device pair: set MicSync's output to **CABLE Input** (remote audio is written here) and pick **CABLE Output** as the microphone in your meeting app
- **Auto-follow** works too: it detects local apps recording from CABLE Output via WASAPI audio-session enumeration, with no OS version gate

### Android phone as the server (share the phone's mic)

The `android/` directory contains a companion Android app that speaks the same protocol — the phone's microphone becomes the shared mic, and Mac clients use it exactly like a Mac server. (macOS Continuity can only lend an **iPhone**'s mic to a Mac; this covers the Android case, over plain LAN.)

1. Install the APK on the phone (sideload; see [Development](#development) for building it)
2. Open the app and tap **Start sharing** — grant the microphone permission when prompted
3. On the Mac client, enter the `IP:47800` shown on the phone's screen
4. Semantics match the Mac server: the mic stays fully closed until a client requests it, the newest request preempts, and the persistent notification shows who is receiving the stream

The app runs as a foreground service (microphone type) holding a CPU + Wi-Fi lock, so streaming survives the screen turning off. Keep the phone on the same Wi-Fi / subnet as the Macs.

### iPhone as the server (share the iPhone's mic)

The iOS build is the same Tauri app (same Rust code, same protocol) packaged for iPhone — the iPhone's microphone becomes the shared mic and Mac clients connect as usual, without Continuity's same-Apple-ID restriction. iOS has no system-wide virtual audio device (no BlackHole equivalent), so the iPhone can only act as a server; the app shows the server panel only.

1. Install the app on the iPhone (self-signed sideload; see [Development](#development) for building it)
2. Open the app and allow the microphone permission prompt — it then listens on the LAN automatically
3. On the Mac client, enter the `IP:47800` shown on the iPhone's screen
4. Semantics match the Mac server: the mic stays fully closed until a client requests it, and the newest request preempts

While the app is in the foreground the screen never auto-locks, keeping the server reachable; during an active stream the background-audio mode (UIBackgroundModes audio) keeps it flowing after backgrounding/locking. **When idle in the background, though, iOS suspends the app** and remote requests won't get through — bring the app back to the foreground when needed. Keep the iPhone on the same Wi-Fi / subnet as the Macs.

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

# Build .app / .dmg (on macOS) / NSIS installer (on Windows)
npx @tauri-apps/cli@latest build
# Artifacts land in src-tauri/target/release/bundle/
# Or use the repo's GitHub Actions workflow (.github/workflows/build.yml) to build both platforms at once

# Android server app (requires JDK 17 + Android SDK)
cd android
./gradlew test           # protocol tests run on the JVM with a fake capture — no device needed
./gradlew assembleDebug  # APK lands in app/build/outputs/apk/debug/

# iOS server app (requires Xcode + rustup; once: rustup target add aarch64-apple-ios aarch64-apple-ios-sim)
npm install                      # installs the tauri CLI, which the Xcode build script invokes
npx tauri ios dev "iPhone 17"    # run on the simulator
npx tauri ios build              # device .ipa (set a signing team in tauri.conf.json)
```

On first run the server triggers macOS's microphone-permission prompt; a client's first connection triggers the local-network-permission prompt. Allow both.

## Technical notes

- **Tauri 2** + pure static frontend (no Node dependency)
- **cpal** for audio capture/playback (CoreAudio on macOS, WASAPI on Windows)
- Minimal HTTP server (hand-written on std, no framework): `/health` liveness + `/stream` streaming; stream body is an `MSY1` magic header + length-prefixed PCM i16 frames, `TCP_NODELAY` for low latency
- **Mic on demand**: at launch the server only listens on the API and never touches the microphone; each `/stream` session exclusively owns one cpal capture stream and releases it the moment the session ends (disconnect / preemption) — the system's mic indicator only lights up during actual use
- **Auto-follow detection** (client): while armed it reads `kAudioDevicePropertyDeviceIsRunningSomewhere` on the BlackHole device (the client holds no stream then, so it flipping true means "some app just started using the virtual mic"); while streaming its own playback keeps that property true, so it switches to the macOS 14+ Process Objects API (`kAudioHardwarePropertyProcessObjectList` — the data source behind the orange mic indicator) to ask "is any process other than me still capturing input". Local HAL property polls take ~3 ms, sampled every 300 ms. On Windows the same state machine runs on a WASAPI backend: VB-Cable is a render (CABLE Input) / capture (CABLE Output) endpoint pair, so playback and detection are naturally separated — both signals come from one `IAudioSessionManager2` enumeration of active audio sessions on the capture endpoint
- **Service discovery** (LocalSend's two-track design; the client fills in the address with one click):
  - **Multicast query/response**: the client sends a query to `224.0.0.167:47801` and live servers answer by unicast. The group address follows LocalSend's hard-won default — it sits inside `224.0.0.0/24`, and some Android implementations drop multicast aimed outside that block; the port is our own, distinct from LocalSend's 53317 (sharing a group address is harmless — the kernel demuxes by port). Unlike LocalSend's periodic announce, this is client-initiated: when nobody is searching there is zero multicast traffic, in the same spirit as mic-on-demand. A server also announces once at startup so already-open clients see it appear
  - **`/health` subnet scan**: probes every IP in each local /24 concurrently. This is not merely a fallback for networks that block multicast — **it is the only way an iOS server can be found**: iOS requires a restricted, Apple-approved entitlement (`com.apple.developer.networking.multicast`) to send or receive multicast, which self-signed sideloads cannot get. `/health` already returns `{"app":"micsync"}`, so it doubles as the discovery signature. Assumes /24 (same trade-off LocalSend makes: the netmask isn't available; larger subnets rely on multicast)
  - Both tracks run concurrently and merge. A desktop is both server and client, so it always discovers itself — filtered out via the fingerprint in `/health` (more reliable than matching local IPs: multi-homed hosts and loopback can't slip through). Servers older than 0.5.0 have no such field, so they fall back to a local-IP check — otherwise a mixed-version LAN would show "can't find it, but typing the IP works"
  - The scan runs only on an explicit "Search" click or once when the Client tab is first opened — never on a timer, since it opens connections across the whole /24
- **Consent gate** (auto-discovery puts "borrowing" someone's mic one click away, so a human has to approve before the mic opens):
  - A client's `/stream` request carries its device name and token. **An unapproved request is held** while the server prompts its owner: Deny / Allow once / Always allow — **nothing touches the microphone until approval**, not even claiming a session. No answer within 30 s counts as a denial, so the other side isn't left hanging
  - **Two identities, never interchangeable**: `device_id` is public (it appears in `/health` and multicast announcements) and only identifies *yourself* during discovery; trust rests on a **random token the server issues** on "Always allow", one per server-client pair, never exposed on any public endpoint. Doing it the other way around — keying trust on the public id — would let anyone read a peer's `/health` and impersonate them, turning "Always allow" into a false sense of security
  - Tokens are issued by the server and stored by the client under that server's `device_id`, so each pair is independent: a malicious server that learns your token still cannot impersonate you to a different server
  - **Denial is terminal**: on 403 the client stops immediately and never auto-retries — otherwise it would re-prompt the owner every 1.5 seconds, which is harassment, not consent
  - A peer's device name is untrusted input: control characters (newlines would break HTTP headers) are stripped and the name is capped at 40 chars before it reaches the dialog
- **Preemptive single stream**: only one stream exists at a time; a new request takes over and the old client receives an explicit end frame and stops (it does not grab the stream back, avoiding tug-of-war between devices)
- Client uses a 60 ms start-playback watermark + 300 ms buffer cap (drops oldest data over the cap to preserve latency); streaming linear resampling handles any sample-rate combination
- Bandwidth during streaming is ~0.8 Mbps (48 kHz · 16-bit · mono); at rest there is zero audio traffic and zero mic usage
- The virtual microphone is provided by the [BlackHole](https://github.com/ExistentialAudio/BlackHole) loopback driver; a custom CoreAudio HAL driver (no BlackHole install needed) is a future direction
- **iOS build**: the same Rust code with the client/auto-follow roles compiled out via `cfg` (iOS has no virtual audio device); configures AVAudioSession (PlayAndRecord + MixWithOthers) and requests the record permission at launch; capture still goes through cpal (AudioUnit); AudioToolbox/CoreAudio/AVFoundation are linked explicitly in `gen/apple/project.yml` (a Rust staticlib's `#[link]` attributes don't reach the Xcode link step)

## Known limitations / roadmap

- One-time delay between requesting the mic and hearing audio: mic initialization (usually <1 s) + the 60 ms start watermark; once established the stream is continuous low latency
- Auto-follow's "stopped" signal is an approximation: which device another process captures from is privacy-hidden (`pdv#` returns empty), so if some other app is still capturing from any microphone (e.g. dictation), the remote mic stays held until it stops too
- The approval step solves "someone used your mic without asking". MicSync is designed for a trusted LAN; traffic is not encrypted
- Takeover between *approved* devices is unconditional (any approved device's request preempts the current stream) — that is the intent: the mic follows you
- Possible next steps: Opus compression (cross-subnet / weak networks), mDNS auto-discovery of the server, custom HAL driver
