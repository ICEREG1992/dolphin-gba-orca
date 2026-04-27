# Changelog

## v0.1.9 — Wayland Overhaul

*(see below for v0.1.5 baseline)*

## v0.1.5 — Windows & X11 Baseline

### Rationale
GBA Orca started as a **Windows-only** Tauri 2 app to capture Dolphin emulator GBA link windows and stream them to mobile browsers on the same LAN. The v0.1.5 release established the full architecture: Win32/X11 window enumeration, FFmpeg sidecar per slot, Axum HTTP server for MJPEG, MediaMTX for WebRTC, and a minimal SvelteKit UI.

---

### Backend

#### `windows_api.rs` — Win32 window management
- `EnumWindows` callback enumerates all visible windows, reads titles via `GetWindowTextW`, extracts PID and geometry.
- `detect_gba_slot()` parses "GBA1"–"GBA4" from window titles for automatic slot detection.
- `is_window_alive`, `is_window_minimized`, `restore_window_silent` — keepalive helpers.
- `restore_window_silent` uses `SW_SHOWNOACTIVATE` + `SetWindowPos(HWND_BOTTOM)` to unminimize without focus theft.

**Why:** Dolphin's GBA windows are small (~240×160) and often minimized during gameplay. Restoring them silently keeps the stream alive without interrupting the user.

#### `x11_api.rs` — X11 window management via `x11rb`
- Pure-Rust X11 client using `x11rb`. Queries `_NET_CLIENT_LIST`, `_NET_WM_NAME`, `_NET_WM_PID`, `_NET_WM_STATE`.
- `is_window_minimized` checks both ICCCM `WM_STATE` (IconicState) and EWMH `_NET_WM_STATE_HIDDEN`.
- `restore_window_silent` sends `_NET_WM_STATE` REMOVE + `StackMode::BELOW` to unminimize without focus theft.

**Why:** x11rb avoids linking libX11 and works on both X11 and XWayland (when `DISPLAY` is set). The dual minimized check covers WMs that don't set both flags.

#### `stream.rs` — FFmpeg sidecar lifecycle & 4 streaming modes
- `StreamMode` enum: MJPEG, WebRTC (H.264), WebRTC++ (2× upscale H.264), WebRTC VP9 (yuv444p).
- `build_ffmpeg_args_with_capture()` shares common capture flags and branches only on per-mode codec/filter/muxer.
- **MJPEG path**: FFmpeg → TCP localhost (9001–9004) → Rust `ingest_loop` with incremental `MjpegParser` → `tokio::sync::watch` broadcast → Axum `multipart/x-mixed-replace`.
- **WebRTC path**: FFmpeg → RTMP (`rtmp://127.0.0.1:1935/slotN`) → MediaMTX → WebRTC (`:8889/slotN`).
- **Keepalive task** (`keepalive_loop`): every 500 ms checks if source window is alive; if closed → auto-cleanup. If minimized → `restore_window_silent`.
- `shutdown_session`, `remove_and_cleanup`, `shutdown_all_webrtc`, `shutdown_all` — layered teardown.

**Why:** Four modes exist because GBA frames are tiny (~240 px). yuv420p chroma subsampling visibly degrades colors at that resolution, so WebRTC++ upscales 2× to compensate, and VP9 uses yuv444p for lossless chroma. MJPEG remains the lowest-latency option.

#### `http.rs` — Axum HTTP server (port 8080)
- `/stream/:slot` — MJPEG multipart stream via `WatchStream` (coalesces missed frames, drops slow viewers cleanly).
- `/v/:slot` — serves an inline mobile viewer:
  - MJPEG: `<img>` tag with pinch-to-zoom support, rotation button.
  - WebRTC: iframe pointing to MediaMTX's built-in player at `:8889/slotN`.
- CORS headers for LAN access from any origin.

**Why:** A single HTTP server on a predictable port makes mobile discovery trivial. The inline viewer requires zero client-side setup — open the URL and play.

#### `mediamtx.rs` — MediaMTX sidecar lifecycle
- `ensure()` starts MediaMTX on first WebRTC stream, polls RTMP port for readiness.
- Event task monitors MediaMTX stdout/stderr; on termination → clears child handle and tears down all WebRTC streams.
- `stop()` kills MediaMTX when no WebRTC streams remain.
- Config is static `mediamtx.yml` with `paths: all_others: source: publisher`.

**Why:** MediaMTX is spawned lazily (not at boot) because most users start with MJPEG. Tearing it down when idle saves RAM and avoids port conflicts.

#### `network.rs` — LAN IP discovery
- `score_interface()` heuristically ranks NICs: penalizes virtual adapters (VMware, WSL, Docker, Tailscale), boosts physical LAN (Wi-Fi, Ethernet), and prefers RFC 1918 ranges (`192.168.x` > `10.x` > `172.16.x`).
- `get_server_info` IPC command returns scored interfaces + ports.

**Why:** On machines with many virtual interfaces (Hyper-V, WSL2, Docker), showing the wrong IP breaks mobile access. Scoring surfaces the most likely LAN address first.

#### `lib.rs` — Entry point & process safety
- `SharedState` (`sessions: HashMap<u8, StreamSession>`, `mediamtx: MediamtxState`) is Tauri-managed and cloned into async tasks.
- **Windows Job Object** with `KILL_ON_JOB_CLOSE`: child processes (FFmpeg, MediaMTX) are killed by the kernel even if the app crashes or is force-closed via Task Manager.
- Linux: sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` and `WEBKIT_DISABLE_COMPOSITING_MODE=1` before WebView init to avoid black screen on broken GPU drivers.
- On `CloseRequested` → `shutdown_all()` + `mediamtx.shutdown()`.

**Why:** Job Objects guarantee cleanup even in pathological exits. Without them, orphaned FFmpeg processes would hold TCP ports and consume CPU indefinitely.

---

### Frontend (`src/routes/+page.svelte`)

- Single Svelte 5 component, `@sveltejs/adapter-static` (pure SPA, no SSR).
- Auto-scan every 3 s: `list_windows()` (or `list_gba_windows()` if filter enabled).
- Per-slot controls: mode selector (MJPEG / WebRTC / WebRTC++ / WebRTC VP9), start/stop, clickable URL.
- Bilingual: Italian / English with system-locale detection + manual override + `localStorage` persistence.
- Status bar shows window count, active stream count, and network interface count.

---

### Dependencies
- **Tauri 2** (`tauri`, `tauri-build`, `tauri-plugin-shell`, `tauri-plugin-opener`)
- **Axum** + **Tokio** for HTTP server and async runtime
- **x11rb** for X11 window queries
- **windows** crate (Win32 Foundation, UI, Job Objects, Threading, Security)
- **local-ip-address** for NIC enumeration
- **bytes**, **tokio-stream**, **futures-util** for MJPEG ingest pipeline

---

### Architecture Summary
```
Dolphin GBA windows
     ↑
FFmpeg sidecar (gdigrab / x11grab / libpipewire)
     ↑
MJPEG: TCP ingest → Rust parser → watch channel → Axum HTTP
WebRTC: RTMP → MediaMTX → WebRTC
     ↑
Mobile browser (/v/:slot viewer)
```

---

## v0.1.9 — Wayland Overhaul

### Rationale
The Wayland capture path was previously bloated, slow and unreliable:
- It tried to encode JPEG and convert pixels **in pure Rust**, causing massive latency on high-resolution captures.
- WebRTC never started because the portal returned **8K raw frames** and FFmpeg choked trying to encode them in real-time.
- The `ashpd` portal session was dropped after source selection, invalidating PipeWire node IDs on many compositors.
- There was no visual guidance for the user on how to select windows in the correct order.

This release rewrites the entire Wayland pipeline with a **"simplify everything"** philosophy: let FFmpeg do the heavy lifting, eliminate all intermediate buffering, and make the UI explicit and minimal.

---

### Backend

#### `pipewire_capture.rs` — stripped to the bone
- **Removed** `jpeg-encoder` dependency and all pixel-format conversion helpers (`bgrx_to_rgb`, `rgbx_to_rgb`, etc.).
- **Removed** dual `CaptureMode` enum (MJPEG vs WebRTC). Replaced with a single raw-frame pump.
- Added `probe_format(fd, node_id)` — opens a short-lived PipeWire stream, reads the negotiated video format (width/height/pix_fmt), then closes. This gives FFmpeg the exact parameters it needs.
- Added `start_raw_pump(fd, node_id, writer)` — captures frames directly from PipeWire and pushes them into a user-supplied `FnMut(&[u8]) -> bool`. **Zero channel buffering**, zero copies, zero tokio mpsc overhead.
- Writer returns `false` on failure → pump immediately calls `pw_main_loop_quit`, preventing hung threads.

**Why:** Encoding 8K frames in Rust was the #1 source of latency. Moving encoding to FFmpeg (C/SIMD) and removing the intermediate channel cut latency from seconds to milliseconds.

#### `stream.rs` — unified FFmpeg path for Wayland
- **Removed** separate `start_wayland_mjpeg` and `start_wayland_webrtc` functions.
- **Removed** `resolve_ffmpeg_path` helper (moved inline).
- Wayland now uses `std::process::Command` to spawn FFmpeg directly, giving us synchronous access to `stdin` for the zero-copy pump.
- New backend variant `CaptureBackend::FfmpegStd` for `std::process::Child` cleanup.
- Injected a **pre-filter** into FFmpeg args:
  ```
  fps=30,scale=1280:-2:flags=fast_bilinear
  ```
  This forces FFmpeg to downscale the portal's raw 4K/8K feed to HD **before** any encoder touches it, keeping CPU usage identical to the X11 path.
- Added low-latency input flags:
  - `-fflags nobuffer`
  - `-probesize 32`
  - `-analyzeduration 0`
  - `-thread_queue_size 512`

**Why:** Without the pre-filter, FFmpeg tried to encode 8192×4608 at 30 fps — impossible in real-time. `fast_bilinear` is the cheapest scaler and runs inside FFmpeg's native pipeline.

#### `wayland_api.rs` — persistent portal session
- `WaylandData` now stores the `Screencast` proxy and `Session` inside a `Box<dyn Any + Send + Sync>` so they are not dropped after `wayland_select_sources` returns.
- The PipeWire FD is kept alive inside `WaylandData` and duplicated on demand via `dup_fd()`.
- **Auto-assignment**: sources returned by the portal are automatically assigned slots 1–4 in **FIFO order** (first-clicked → GBA1). Most portals return streams in LIFO order, so we `.rev()` the stream list before enumerating.

**Why:** Dropping the portal session immediately invalidated the PipeWire nodes on GNOME/KDE, causing "probe timeout" errors on subsequent capture attempts.

---

### Frontend (`+page.svelte`)

- **Removed** the interactive preview modal (it was redundant once auto-assignment works).
- **Added** a persistent yellow banner on Wayland sessions:
  > **WAYLAND rilevato** — Seleziona nell'ordine: **GBA1**, **GBA2**, **GBA3**, **GBA4**
  - Left-aligned, bold, high-contrast colors for immediate readability.
- **Redesigned** the "Select GBA windows" button as a **primary action** (Windows 10 style: blue `#0078d7`, white text, subtle shadow). It stands out without being garish.
- Slot badges in the table now use the same blue accent for consistency.

**Why:** Users were confused about selection order and didn't notice the small hint text. The banner is impossible to miss and the button looks like the main action it is.

---

### Dependencies
- **Removed** `jpeg-encoder` from `Cargo.toml` (no longer needed).

---

### Lessons Learned — Wayland: DO NOT use FFmpeg `libpipewire` input

> **⚠️ MONITOR (Wayland only) — DO NOT attempt to use FFmpeg's `-f libpipewire -i <node_id>` to capture PipeWire streams granted by xdg-desktop-portal.**
>
> We tried it. It is terrible for real-time streaming:
> - **Massive latency** — introduces seconds of internal buffering with no way to tune it.
> - **Broken format negotiation** — ignores the portal's preferred resolution and pixel format, often feeding the encoder 8K frames even for a 240×160 window.
> - **No lifecycle control** — FFmpeg manages the PipeWire connection internally; if it stalls or mis-negotiates, the entire pipeline dies with no recovery hook.
>
> **The only reliable approach on Wayland:** capture raw frames via the PipeWire C API (or a safe binding like `pipewire-rs`) and **feed them to FFmpeg on `stdin` as `rawvideo`**. Probe the negotiated format once, then pump raw bytes directly into FFmpeg. FFmpeg should only handle encoding and muxing, never capture. This gives full control over buffering, scaling, resolution clamping, and clean shutdown.

---

### Known Limitations
- **Minimized windows on Wayland** will stop producing capture frames. This is a compositor-level restriction (GNOME/KDE stop rendering occluded windows). There is no API on Wayland to force a window to stay rendered in the background.
- **Window titles** are not available from the xdg-desktop-portal ScreenCast API. Slot assignment relies on selection order, not title matching.

---

### Compatibility
- **Windows / X11 paths are untouched.** All changes are gated behind `#[cfg(target_os = "linux")]` or isolated in Wayland-specific functions.
