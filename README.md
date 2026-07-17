# MicSync 🎙️

**English** | [简体中文](README.zh-CN.md)

**One microphone, shared across every Mac on your LAN.** The mic is physically plugged into one computer (the server); other computers (clients) on the local network use it as their own microphone through a virtual audio device — no more re-plugging cables.

## Why MicSync

Anyone with more than one Mac on their desk runs into the same annoyance: **there's only one good microphone**.

- Today's meeting is on the work MacBook, tomorrow's is on the personal Mac mini — the mic has to follow you around; frequently re-plugging a USB mic or audio interface is tedious and wears out ports
- The moment a Bluetooth headset enables its mic, audio quality drops to "phone call" level (HFP profile) — no headset can escape that
- macOS Continuity can only use an **iPhone** as a microphone; it cannot borrow the mic from another **Mac**
- Existing network-audio solutions (NDI, Dante, etc.) are heavyweight or paid, and most of them **stream continuously** — burning bandwidth and CPU even when nobody is talking

MicSync solves exactly one small problem: **make the microphone attached to one Mac appear as a "local microphone" on every other Mac in the LAN**. And it stays quiet: zero audio traffic while nobody speaks, connects the instant you talk, and disconnects when you stop.

## Use cases

- **Two-Mac desk setup**: a work MacBook and a personal desktop Mac share one desk; the mic is plugged into one of them. When the other needs to join a meeting, just select BlackHole as the input — zero re-plugging
- **Rotating between Macs for meetings**: the same person joins calls from different Macs (switching between company and client environments); all clients stay listening in standby, and whichever Mac you sit at just works — voice-triggered, no manual switching
- **Sharing a high-quality recording chain**: a condenser mic + audio interface lives on your recording/streaming rig; the Mac next to it can borrow that chain's sound for an impromptu call
- **Multi-device standby without double audio**: multiple clients can listen to the same server simultaneously, but only one receives the stream at a time (first come, first served, serial rotation) — the same voice never gets fed into two meetings at once

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
    GET /health poll (250ms)  │  GET /stream (exclusive; 409 if taken)
       ┌──────────────┬───────┴───────┬──────────────┐
       │ Client Mac A │ Client Mac B  │ Client Mac C │
       │ BlackHole    │ BlackHole     │ BlackHole    │
       │ → Zoom       │ → WeChat      │ → Teams      │
       └──────────────┴───────────────┴──────────────┘
        Only one client holds the stream at any moment
```

The server only listens; clients are event-driven. There is no audio flow at rest — **voice activity detection (VAD) on the server triggers clients to pull the stream automatically via the API**; after 1.5 s of silence the stream ends and everyone returns to standby. Data flow in detail:

```
┌─ Server Mac ─────────────────┐         ┌─ Client Mac × N ─────────────────────┐
│ real mic (captured locally   │  HTTP   │ poll GET /health (is service up?     │
│ for detection only)          │←────────│ mic active? stream slot taken?)      │
│   ↓ voice activation (VAD)   │         │   ↓ when active and slot free        │
│ speak → active;              │  HTTP   │ GET /stream claims the exclusive     │
│ 1.5s silence → stream ends   ├────────→│ stream (409 if already taken)        │
│ PCM 16-bit mono frames       │  LAN    │   ↓ resample + jitter buffer         │
└──────────────────────────────┘         │ play into BlackHole (loopback)       │
                                         │   ↓                                  │
                                         │ Zoom/WeChat/any meeting app selects  │
                                         │ "BlackHole 2ch" as microphone ✓      │
                                         └──────────────────────────────────────┘
```

## How to use

### Server (the Mac with the microphone)

1. Open MicSync and switch to the "📡 Server" tab
2. Pick the microphone to share and click "Start sharing"
3. Tell the clients the `IP:port` shown in the UI (click to copy)

### Client (the Mac that wants to use that microphone)

1. Install the BlackHole virtual audio driver (one-time; the app has a guided button):
   - **Recommended**: get the free graphical installer from <https://existential.audio/blackhole/> (email required), then double-click the `.pkg` and follow the prompts — the GUI installer forces the admin authorization through and cannot fail silently
   - Alternative: `brew install blackhole-2ch` (note: if the sudo prompt is not completed during install, brew may report "installed" while the driver actually isn't; verify with `ls /Library/Audio/Plug-Ins/HAL/` and check that `BlackHole2ch.driver` exists)

   > ⚠️ After installation, **restart the Mac** (or run `sudo killall coreaudiod` to restart the audio service) before BlackHole shows up in the device list.
2. Open MicSync and switch to the "💻 Client" tab
3. Enter the server address, choose **BlackHole 2ch** as the output device, and click "Start listening"
4. In any app (Zoom / WeChat / Teams…), select **BlackHole 2ch** as the microphone

After "Start listening" the client sits in standby: streaming starts automatically when the server mic detects speech, and stops (after 1.5 s of silence) back to standby. Multiple clients can listen to the same server, but **only one receives the stream at a time** (first come, first served, serial rotation), so the same voice is never fed into a meeting from two machines. If the server restarts mid-way, clients reconnect automatically.

## HTTP API

The server exposes two endpoints on its listening port (default `47800`):

| Endpoint | Description |
| --- | --- |
| `GET /health` | Liveness + status: `{"status":"ok","app":"micsync","sample_rate":48000,"device":"...","mic_active":false,"streaming":false}` |
| `GET /stream` | Claim the exclusive stream. On success returns `200` (`application/octet-stream`, body is an `MSY1` header + PCM frames until silence ends the stream); returns `409` if the mic is inactive or the slot is taken |

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
- **Voice activation (VAD)**: peak level ≥ 0.03 activates; 1.5 s of silence deactivates and ends the stream; from the moment of activation the server buffers 300 ms of audio and backfills it when a client claims the stream, so the first syllable is never lost
- **Single stream slot**: only one client can hold `/stream` at a time; the rest poll in standby and grab the slot first-come-first-served once it frees up
- Client uses a 60 ms start-playback watermark + 300 ms buffer cap (drops oldest data over the cap to preserve latency); streaming linear resampling handles any sample-rate combination
- Bandwidth during streaming is ~0.8 Mbps (48 kHz · 16-bit · mono); at rest there is only a /health poll every 250 ms — essentially zero
- The virtual microphone is provided by the [BlackHole](https://github.com/ExistentialAudio/BlackHole) loopback driver; a custom CoreAudio HAL driver (no BlackHole install needed) is a future direction

## Known limitations / roadmap

- The VAD threshold is currently fixed (peak 0.03): noisy rooms may trigger it falsely, very quiet speech may not trigger it; adaptive noise-floor tracking is a future direction
- Inherent latency between speech onset and client playback: one polling cycle (≤250 ms) + the 60 ms start watermark; the first 300 ms is backfilled by the server
- No encryption/authentication yet — use only on trusted LANs
- Possible next steps: Opus compression (cross-subnet / weak networks), mDNS auto-discovery of the server, custom HAL driver
