use std::time::Duration;

use bytes::Bytes;
use serde::Serialize;
use tauri::State;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use crate::mediamtx;
use crate::network::{HTTP_PORT, MEDIAMTX_RTMP_PORT, MEDIAMTX_WEBRTC_PORT};
use crate::windows_api::{is_window_alive, is_window_minimized, restore_window_silent};
use crate::SharedState;

const BROADCAST_CAPACITY: usize = 32;
const INGEST_BUFFER_SIZE: usize = 64 * 1024;

pub fn ingest_port_for_slot(slot: u8) -> u16 { 9000 + slot as u16 }

#[derive(Serialize, Clone, Debug)]
pub enum StreamMode {
    Mjpeg,
    Webrtc,
    WebrtcPlus,
    WebrtcVp9,
}

impl StreamMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "mjpeg" => Ok(Self::Mjpeg),
            "webrtc" => Ok(Self::Webrtc),
            "webrtc++" => Ok(Self::WebrtcPlus),
            "webrtc-vp9" => Ok(Self::WebrtcVp9),
            other => Err(format!("Modalità non valida: {}", other)),
        }
    }

    pub fn is_webrtc(&self) -> bool {
        matches!(self, Self::Webrtc | Self::WebrtcPlus | Self::WebrtcVp9)
    }
}

pub struct StreamSession {
    pub title: String,
    pub mode: StreamMode,
    pub child: CommandChild,
    pub sender: Option<broadcast::Sender<Bytes>>,
    pub ingest_task: Option<tauri::async_runtime::JoinHandle<()>>,
    pub keepalive_task: tauri::async_runtime::JoinHandle<()>,
}

pub fn shutdown_session(session: StreamSession) {
    let _ = session.child.kill();
    if let Some(task) = session.ingest_task { task.abort(); }
    session.keepalive_task.abort();
}

#[derive(Serialize, Clone)]
pub struct StreamInfo {
    pub slot: u8,
    pub title: String,
    pub mode: StreamMode,
    pub url: String,
}

fn stream_url(slot: u8, mode: &StreamMode) -> String {
    if mode.is_webrtc() {
        format!("http://127.0.0.1:{}/slot{}", MEDIAMTX_WEBRTC_PORT, slot)
    } else {
        format!("http://127.0.0.1:{}/v/{}", HTTP_PORT, slot)
    }
}

fn any_webrtc_active(state: &SharedState) -> bool {
    state.sessions.lock().unwrap().values().any(|s| s.mode.is_webrtc())
}

/// Remove a session by slot, kill its FFmpeg + tasks, and stop MediaMTX if no
/// WebRTC streams remain. Returns true if a session was actually removed.
fn remove_and_cleanup(state: &SharedState, slot: u8) -> bool {
    let session = state.sessions.lock().unwrap().remove(&slot);
    let Some(session) = session else { return false };
    let was_webrtc = session.mode.is_webrtc();
    shutdown_session(session);
    if was_webrtc && !any_webrtc_active(state) {
        mediamtx::stop(state);
    }
    true
}

/// Drain all WebRTC sessions. Called when MediaMTX dies unexpectedly — those
/// streams can no longer reach any viewer, so kill their FFmpeg processes too.
/// Caller must NOT hold the sessions lock; may hold the mediamtx lock.
pub fn shutdown_all_webrtc(state: &SharedState) {
    let mut sessions = state.sessions.lock().unwrap();
    let webrtc_slots: Vec<u8> = sessions
        .iter()
        .filter(|(_, s)| s.mode.is_webrtc())
        .map(|(&slot, _)| slot)
        .collect();
    for slot in webrtc_slots {
        if let Some(session) = sessions.remove(&slot) {
            shutdown_session(session);
        }
    }
}

/// Drain all sessions of any kind. Called on app shutdown.
pub fn shutdown_all(state: &SharedState) {
    let drained: Vec<StreamSession> = state.sessions.lock().unwrap()
        .drain().map(|(_, s)| s).collect();
    for session in drained {
        shutdown_session(session);
    }
}

async fn ingest_loop(slot: u8, listener: TcpListener, sender: broadcast::Sender<Bytes>) {
    let mut buf = vec![0u8; INGEST_BUFFER_SIZE];
    loop {
        let (mut socket, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[ingest slot {}] accept error: {}", slot, e);
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        eprintln!("[ingest slot {}] ffmpeg connesso da {}", slot, addr);

        loop {
            match socket.read(&mut buf).await {
                Ok(0) => {
                    eprintln!("[ingest slot {}] ffmpeg disconnesso", slot);
                    break;
                }
                Ok(n) => {
                    let _ = sender.send(Bytes::copy_from_slice(&buf[..n]));
                }
                Err(e) => {
                    eprintln!("[ingest slot {}] read error: {}", slot, e);
                    break;
                }
            }
        }
    }
}

async fn keepalive_loop(slot: u8, hwnd: isize, state: SharedState) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    loop {
        interval.tick().await;

        if !is_window_alive(hwnd) {
            eprintln!("[keepalive slot {}] finestra chiusa dall'utente, fermo stream", slot);
            remove_and_cleanup(&state, slot);
            break;
        }

        if is_window_minimized(hwnd) {
            eprintln!("[keepalive slot {}] finestra minimizzata, ripristino", slot);
            restore_window_silent(hwnd);
        }
    }
}

/// Build the full FFmpeg arg list for a given mode. Common args (capture
/// device, framerate, input title) are listed once; per-mode args cover the
/// video filter, codec, pixel format, and output muxer.
fn build_ffmpeg_args(mode: &StreamMode, title_arg: &str, output_url: &str) -> Vec<String> {
    let common: &[&str] = &[
        "-hide_banner",
        "-loglevel", "info",
        "-nostats",
        "-probesize", "32",
        "-analyzeduration", "0",
        "-f", "gdigrab",
        "-framerate", "30",
        "-i", title_arg,
    ];
    let specific: &[&str] = match mode {
        StreamMode::Mjpeg => &[
            "-vf", "mpdecimate=max=30",
            "-fps_mode", "vfr",
            "-c:v", "mjpeg",
            "-q:v", "5",
            "-flush_packets", "1",
            "-f", "mpjpeg",
        ],
        StreamMode::Webrtc => &[
            "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2",
            "-c:v", "libx264",
            "-preset", "ultrafast",
            "-tune", "zerolatency",
            "-g", "30",
            "-b:v", "2M",
            "-pix_fmt", "yuv420p",
            "-f", "flv",
        ],
        StreamMode::WebrtcPlus => &[
            "-vf", "scale=2*iw:2*ih:flags=neighbor",
            "-c:v", "libx264",
            "-preset", "fast",
            "-tune", "zerolatency",
            "-crf", "18",
            "-pix_fmt", "yuv420p",
            "-g", "30",
            "-f", "flv",
        ],
        StreamMode::WebrtcVp9 => &[
            "-vf", "scale=2*iw:2*ih:flags=neighbor",
            "-c:v", "libvpx-vp9",
            "-crf", "18",
            "-b:v", "0",
            "-pix_fmt", "yuv444p",
            "-g", "30",
            "-tune-content", "screen",
            "-f", "flv",
        ],
    };
    common
        .iter()
        .chain(specific.iter())
        .map(|s| (*s).to_string())
        .chain(std::iter::once(output_url.to_string()))
        .collect()
}

#[tauri::command]
pub async fn start_stream(
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
    slot: u8,
    hwnd: isize,
    window_title: String,
    mode: String,
) -> Result<StreamInfo, String> {
    if !(1..=4).contains(&slot) {
        return Err(format!("Slot non valido: {}", slot));
    }
    let stream_mode = StreamMode::parse(&mode)?;

    if state.sessions.lock().unwrap().contains_key(&slot) {
        return Err(format!("Slot {} già in stream", slot));
    }

    if stream_mode.is_webrtc() {
        mediamtx::ensure(&app, &state).await?;
    }

    // If the window is minimized, unminimize without stealing focus.
    if is_window_minimized(hwnd) {
        restore_window_silent(hwnd);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    // For MJPEG, bind the local TCP listener and create a broadcast channel
    // before spawning FFmpeg, so it can connect immediately.
    let (sender, listener) = if matches!(stream_mode, StreamMode::Mjpeg) {
        let port = ingest_port_for_slot(slot);
        let listener = TcpListener::bind(("127.0.0.1", port)).await
            .map_err(|e| format!("Bind ingest port {}: {}", port, e))?;
        let (s, _) = broadcast::channel::<Bytes>(BROADCAST_CAPACITY);
        (Some(s), Some(listener))
    } else {
        (None, None)
    };

    let title_arg = format!("title={}", window_title);
    let output_url = if stream_mode.is_webrtc() {
        format!("rtmp://127.0.0.1:{}/slot{}", MEDIAMTX_RTMP_PORT, slot)
    } else {
        format!("tcp://127.0.0.1:{}", ingest_port_for_slot(slot))
    };

    let sidecar = app.shell().sidecar("ffmpeg")
        .map_err(|e| format!("ffmpeg sidecar non trovato: {}", e))?;
    let args = build_ffmpeg_args(&stream_mode, &title_arg, &output_url);
    let (mut rx, child) = sidecar.args(args).spawn()
        .map_err(|e| format!("spawn fallito: {}", e))?;

    // Spawn the MJPEG ingest task (no-op for WebRTC modes).
    let ingest_task = match (listener, sender.clone()) {
        (Some(l), Some(s)) => Some(tauri::async_runtime::spawn(async move {
            ingest_loop(slot, l, s).await;
        })),
        _ => None,
    };

    // Spawn the keepalive task: re-restore the window if the user re-minimizes
    // it, and stop the stream if the window is closed.
    let state_for_ka = state.inner().clone();
    let keepalive_task = tauri::async_runtime::spawn(async move {
        keepalive_loop(slot, hwnd, state_for_ka).await;
    });

    let info = StreamInfo {
        slot,
        title: window_title.clone(),
        mode: stream_mode.clone(),
        url: stream_url(slot, &stream_mode),
    };

    state.sessions.lock().unwrap().insert(slot, StreamSession {
        title: window_title,
        mode: stream_mode,
        child,
        sender,
        ingest_task,
        keepalive_task,
    });

    // Spawn the FFmpeg event watcher. Inserted into the sessions map first so
    // a fast-fail FFmpeg termination still finds the session to clean up.
    let state_for_ff = state.inner().clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stderr(line) => {
                    eprintln!("[ffmpeg slot {}] {}", slot, String::from_utf8_lossy(&line));
                }
                CommandEvent::Terminated(payload) => {
                    eprintln!("[ffmpeg slot {}] terminato: code={:?}", slot, payload.code);
                    remove_and_cleanup(&state_for_ff, slot);
                }
                _ => {}
            }
        }
    });

    Ok(info)
}

#[tauri::command]
pub fn stop_stream(state: State<'_, SharedState>, slot: u8) -> Result<(), String> {
    if remove_and_cleanup(state.inner(), slot) {
        Ok(())
    } else {
        Err(format!("Nessuno stream attivo per slot {}", slot))
    }
}

#[tauri::command]
pub fn list_streams(state: State<'_, SharedState>) -> Vec<StreamInfo> {
    state.sessions.lock().unwrap()
        .iter()
        .map(|(&slot, s)| StreamInfo {
            slot,
            title: s.title.clone(),
            mode: s.mode.clone(),
            url: stream_url(slot, &s.mode),
        })
        .collect()
}
