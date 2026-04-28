# AGENTS.md test

Compact guide for OpenCode sessions working in this repo.

## Commands

```bash
npm install              # install deps
npm run tauri dev        # dev: Vite on :1420 + Tauri app (hot reload)
npm run tauri build      # production NSIS installer
npm run check            # svelte-kit sync + svelte-check against jsconfig.json
npm run dev              # frontend only, no Tauri
```

Build outputs: frontend → `./build/`, Rust binary → `./src-tauri/target/release/`, installer → `./src-tauri/target/release/bundle/`.

## Architecture

**Tauri 2 + Rust backend, SvelteKit static SPA frontend.**

- Frontend: `src/routes/+page.svelte` (single page, no routing). Bilingual IT/EN. Legacy `$:` reactive syntax; not Svelte 5 runes.
- Backend: `src-tauri/src/lib.rs` entry point; modules `stream`, `http`, `mediamtx`, `network`, `input`, `windows_api`, `x11_api`, `wayland_api`, `pipewire_capture`.
- HTTP server: Axum on `0.0.0.0:8080` (spawned in `setup` hook). Routes: `/stream/:slot` (MJPEG multipart), `/v/:slot` (viewer page), `/ws/:slot` (gamepad input WebSocket).
- No SSR: `@sveltejs/adapter-static` with `fallback: "index.html"`, `ssr: false`.
- Vite dev server: port `1420`, `strictPort: true`, ignores `**/src-tauri/**`.
- No tests exist in this repo.

## Platform Split

| Platform | Window enum | Capture | FFmpeg input |
|---|---|---|---|
| Windows | `windows_api.rs` (Win32 `EnumWindows`) | `gdigrab` | `-i title=` |
| Linux X11 | `x11_api.rs` (`x11rb`) | `x11grab` | `-window_id 0x… -i $DISPLAY` |
| Linux Wayland | `wayland_api.rs` (xdg-desktop-portal) | PipeWire raw frames → FFmpeg `stdin` | `-f rawvideo -i pipe:0` |

Wayland cannot enumerate window titles; the portal returns sources in FIFO order. The UI instructs users to select GBA1, GBA2, GBA3, GBA4 in order.

## Streaming Modes

Four modes. FFmpeg is spawned per slot as a sidecar (Windows/X11) or `std::process::Child` (Wayland).

| Mode | Codec | Key flags | Output |
|---|---|---|---|
| MJPEG | `mjpeg` | `mpdecimate`, `-q:v 5`, `mpjpeg` muxer | TCP `127.0.0.1:9000+slot` → Axum broadcast → HTTP multipart |
| WebRTC | `libx264` | `scale=trunc(iw/2)*2:trunc(ih/2)*2`, `ultrafast`, `yuv420p`, `flv` | RTMP `127.0.0.1:1935/slotN` → MediaMTX → WebRTC `:8889` |
| WebRTC++ | `libx264` | `scale=2*iw:2*ih:flags=neighbor`, `fast`, `crf 18`, `yuv420p`, `flv` | Same RTMP path |
| WebRTC VP9 | `libvpx-vp9` | `scale=2*iw:2*ih:flags=neighbor`, `crf 18`, `yuv444p`, `flv` | Same RTMP path |

Why the complexity: GBA windows are ~240 px tall. `yuv420p` halves chroma resolution at that size, so MJPEG looks better. WebRTC++ 2x-upscales to compensate. VP9 supports `yuv444p` (lossless chroma) but has higher encoding latency. On Wayland a `fps=30,scale=1280:-2:flags=fast_bilinear` pre-filter downscales portal frames before encoding.

## Ports

- `8080` — Axum HTTP (viewer pages + MJPEG streams + gamepad WS)
- `9001–9004` — MJPEG ingest TCP (FFmpeg → Rust backend, per slot)
- `1935` — MediaMTX RTMP ingest
- `8889` — MediaMTX WebRTC player (served in iframe by `/v/:slot` for WebRTC modes)

## Sidecars & Binaries

Declared as `externalBin` in `tauri.conf.json`:
- `src-tauri/binaries/ffmpeg-x86_64-pc-windows-msvc.exe`
- `src-tauri/binaries/ffmpeg-x86_64-unknown-linux-gnu`
- `src-tauri/binaries/mediamtx-x86_64-pc-windows-msvc.exe`
- `src-tauri/binaries/mediamtx-x86_64-unknown-linux-gnu`

MediaMTX config `mediamtx.yml` is a `resources` entry. **MediaMTX v1.18.0 takes its config as a positional argument**, NOT `--config`; passing `--config` crashes it.

## Critical Constraints

- **Never write to `src-tauri/` at runtime.** Tauri's file watcher triggers a rebuild loop. `mediamtx.yml` is static, not generated.
- **Windows Job Object:** `setup_job_object()` in `lib.rs` creates a job with `KILL_ON_JOB_CLOSE` so FFmpeg/MediaMTX child processes are killed by the kernel even on forced exit / Task Manager kill. Requires `windows` crate feature `Win32_Security`.
- **Linux WebView workaround:** `setup_linux_env()` sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` and `WEBKIT_DISABLE_COMPOSITING_MODE=1` before WebView init to avoid blank WebView on many GPU drivers.
- **Lock order:** when both `sessions` and `mediamtx` locks are needed, always acquire `mediamtx` first, then `sessions`. Only the MediaMTX `Terminated` event handler does this; everywhere else only one lock is held at a time.
- **CSP is intentionally `null`** in `tauri.conf.json` for development convenience.
- `restore_window_silent()` uses `SW_SHOWNOACTIVATE` + `SetWindowPos(HWND_BOTTOM)` to unminimize GBA windows without stealing focus.

## See also

- `CLAUDE.md` — deeper architecture, data flows, and module docs.
