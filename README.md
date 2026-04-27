# GBA Orca

Stream Dolphin's GBA windows to phones and tablets on your local network.

When you play GameCube games that use GBA-link features, Dolphin opens separate GBA windows on your PC. GBA Orca picks up those windows and streams each one to a browser on any device connected to the same Wi-Fi or LAN

## How it works

```
[Dolphin] ──GBA windows──▶ [GBA Orca on Windows] ──HTTP──▶ [Phones / tablets]
```

GBA Orca finds the GBA windows automatically, captures each one with FFmpeg, and serves them as MJPEG streams from a single HTTP server on port 8080. Players just open a URL in their browser.

## Requirements

- Windows 10 or 11
- Dolphin running a game with GBA windows
- PC and phones on the same Wi-Fi/LAN

## How to use it

1. Download the installer from [Releases](https://github.com/regitkin/dolphin-gba-orca/releases).
2. Start Dolphin and a game with controllers set to GBA (Integrated). Dolphin will open the GBA windows.
3. Open GBA Orca — it lists every GBA window it sees.
4. Click **Start stream** on each one you want to share.
5. Send each player the stream URL shown in the app (something like `http://192.168.1.42:8080/v/1`).
6. On the phone, the round button at the bottom-right rotates the video 90° for landscape play.

7. You can select the streaming mode:
- **MJPEG** — Best quality, but high latency  
- **WebRTC** — Poor quality  
- **WebRTC++** — Surprisingly good balance  
- **WebRTC (VP9)** — Good quality, but CPU intensive and not widely supported

The app rescans every 3 seconds, so closing or restarting Dolphin mid-session is fine — the list updates on its own.


## Build from source

```bash
git clone https://github.com/regitkin/dolphin-gba-orca.git
cd dolphin-gba-orca
npm install
```

Download an FFmpeg build for Windows ([gyan.dev essentials](https://www.gyan.dev/ffmpeg/builds/)), then drop `ffmpeg.exe` into `src-tauri/binaries/` renamed as:

```
src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe
```

Then:

```bash
npm run tauri dev      # development
npm run tauri build    # production installer in src-tauri/target/release/bundle/
```

## Stack

Tauri 2 + Rust backend, Svelte frontend. FFmpeg (`gdigrab` + `mpdecimate`) for capture, MJPEG over HTTP for the stream, axum + tokio for the server. Window enumeration uses the `windows` crate; LAN interface discovery uses `local-ip-address`.

The axum server proxies each FFmpeg process so multiple viewers can watch the same stream — FFmpeg's built-in HTTP server can't do that. If an FFmpeg process dies (window closed, fullscreen, etc.) the session is cleaned up automatically.

## Limitations

- **Windows only.** Capture uses `gdigrab`. macOS/Linux would need `avfoundation` or `x11grab` plus a new window-enumeration module.
- **Unencrypted.** Stream is plain HTTP on the LAN — meant for home use.

## Roadmap

- Custom APP for Android and IOS with PIN-based routing (4-digit code instead of full URL)
- Linux build (X11 and Wayland)

## Contributing

Issues and PRs welcome. Project is early-stage and the internals are still moving.
