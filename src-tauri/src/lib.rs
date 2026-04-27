use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::Body,
    extract::{Path, State as AxumState},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Serialize;
use tauri::{Manager, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, IsIconic, ShowWindow, SetWindowPos,
    SW_SHOWNOACTIVATE, HWND_BOTTOM, SWP_NOMOVE, SWP_NOSIZE, SWP_NOACTIVATE,
};

// ============= WINDOW ENUMERATION =============

#[derive(Serialize, Clone, Debug)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub pid: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub gba_slot: Option<u8>,
}

fn detect_gba_slot(title: &str) -> Option<u8> {
    const SLOTS: [(&str, u8); 4] = [("GBA1", 1), ("GBA2", 2), ("GBA3", 3), ("GBA4", 4)];
    SLOTS.iter().find(|(s, _)| title.contains(s)).map(|(_, n)| *n)
}

fn to_hwnd(v: isize) -> HWND {
    HWND(v as *mut _)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<WindowInfo>);
    if !IsWindowVisible(hwnd).as_bool() { return TRUE; }
    let len = GetWindowTextLengthW(hwnd);
    if len == 0 { return TRUE; }
    let mut buf = vec![0u16; (len + 1) as usize];
    let read = GetWindowTextW(hwnd, &mut buf);
    if read == 0 { return TRUE; }
    let title = String::from_utf16_lossy(&buf[..read as usize]);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    out.push(WindowInfo {
        hwnd: hwnd.0 as isize,
        gba_slot: detect_gba_slot(&title),
        title,
        pid,
        x: rect.left,
        y: rect.top,
        width,
        height,
    });
    TRUE
}

#[tauri::command]
fn list_windows() -> Vec<WindowInfo> {
    let mut windows: Vec<WindowInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut windows as *mut _ as isize));
    }
    windows
}

#[tauri::command]
fn list_gba_windows() -> Vec<WindowInfo> {
    list_windows().into_iter().filter(|w| w.gba_slot.is_some()).collect()
}

// ============= STREAMING =============

const HTTP_PORT: u16 = 8080;
const MEDIAMTX_RTMP_PORT: u16 = 1935;
const MEDIAMTX_WEBRTC_PORT: u16 = 8889;
const BROADCAST_CAPACITY: usize = 32;

fn ingest_port_for_slot(slot: u8) -> u16 {
    9000 + slot as u16
}

#[derive(Serialize, Clone, Debug)]
enum StreamMode {
    Mjpeg,
    Webrtc,
    WebrtcPlus,
    WebrtcVp9,
}

struct StreamSession {
    title: String,
    mode: StreamMode,
    child: CommandChild,
    sender: Option<broadcast::Sender<Bytes>>,
    ingest_task: Option<tauri::async_runtime::JoinHandle<()>>,
    keepalive_task: tauri::async_runtime::JoinHandle<()>,
}

fn shutdown_session(session: StreamSession) {
    let _ = session.child.kill();
    if let Some(task) = session.ingest_task {
        task.abort();
    }
    session.keepalive_task.abort();
}

struct MediamtxState {
    child: Option<CommandChild>,
    event_task: Option<tauri::async_runtime::JoinHandle<()>>,
}

impl Default for MediamtxState {
    fn default() -> Self {
        Self { child: None, event_task: None }
    }
}

#[derive(Clone, Default)]
struct SharedState {
    sessions: Arc<Mutex<HashMap<u8, StreamSession>>>,
    mediamtx: Arc<Mutex<MediamtxState>>,
}

#[derive(Serialize, Clone)]
struct StreamInfo {
    slot: u8,
    title: String,
    mode: StreamMode,
    url: String,
}

fn stream_url(slot: u8, mode: &StreamMode) -> String {
    match mode {
        StreamMode::Mjpeg => format!("http://127.0.0.1:{}/v/{}", HTTP_PORT, slot),
        StreamMode::Webrtc | StreamMode::WebrtcPlus | StreamMode::WebrtcVp9 => format!("http://127.0.0.1:{}/slot{}", MEDIAMTX_WEBRTC_PORT, slot),
    }
}

async fn ingest_loop(slot: u8, listener: TcpListener, sender: broadcast::Sender<Bytes>) {
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

        let mut buf = vec![0u8; 64 * 1024];
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

async fn ensure_mediamtx(app: &tauri::AppHandle, state: &SharedState) -> Result<(), String> {
    {
        let mtx = state.mediamtx.lock().unwrap();
        if mtx.child.is_some() {
            return Ok(());
        }
    }

    // Risolvi il percorso della config statica mediamtx.yml
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("current_exe: {}", e))?
        .parent()
        .ok_or("no parent dir")?
        .to_path_buf();

    let config_candidates = [
        exe_dir.join("../../mediamtx.yml"),
        exe_dir.join("mediamtx.yml"),
    ];

    let config_path = config_candidates.iter()
        .find(|p| p.exists())
        .ok_or_else(|| format!("mediamtx.yml non trovato (cercato in {:?})", config_candidates))?
        .clone();

    eprintln!("[mediamtx] config: {}", config_path.display());

    let sidecar = app.shell().sidecar("mediamtx")
        .map_err(|e| format!("mediamtx sidecar non trovato: {}", e))?;

    let config_arg = config_path.to_string_lossy().into_owned();
    let command = sidecar.args([config_arg.as_str()]);

    let (mut rx, child) = command.spawn()
        .map_err(|e| format!("mediamtx spawn fallito: {}", e))?;

    eprintln!("[mediamtx] avviato come sidecar");

    let sessions = state.sessions.clone();
    let mediamtx = state.mediamtx.clone();
    let event_task = tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stderr(line) | CommandEvent::Stdout(line) => {
                    eprintln!("[mediamtx] {}", String::from_utf8_lossy(&line));
                }
                CommandEvent::Terminated(payload) => {
                    eprintln!("[mediamtx] terminato: code={:?}", payload.code);
                    let mut mtx = mediamtx.lock().unwrap();
                    mtx.child = None;
                    let mut sessions = sessions.lock().unwrap();
                    let webrtc_slots: Vec<u8> = sessions.iter()
                        .filter(|(_, s)| matches!(s.mode, StreamMode::Webrtc | StreamMode::WebrtcPlus | StreamMode::WebrtcVp9))
                        .map(|(&slot, _)| slot)
                        .collect();
                    for slot in webrtc_slots {
                        if let Some(session) = sessions.remove(&slot) {
                            shutdown_session(session);
                        }
                    }
                }
                _ => {}
            }
        }
    });

    {
        let mut mtx = state.mediamtx.lock().unwrap();
        mtx.child = Some(child);
        mtx.event_task = Some(event_task);
    }

    // Attendi che MediaMTX sia pronto (poll sulla porta RTMP)
    let mut ready = false;
    for _ in 0..20 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{}", MEDIAMTX_RTMP_PORT)).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if !ready {
        let mut mtx = state.mediamtx.lock().unwrap();
        if let Some(child) = mtx.child.take() {
            let _ = child.kill();
        }
        if let Some(task) = mtx.event_task.take() {
            task.abort();
        }
        return Err("MediaMTX non si è avviato in tempo (porta RTMP non raggiungibile)".into());
    }

    Ok(())
}

fn maybe_stop_mediamtx(
    sessions: &Arc<Mutex<HashMap<u8, StreamSession>>>,
    mediamtx: &Arc<Mutex<MediamtxState>>,
) {
    let has_webrtc = {
        let sess = sessions.lock().unwrap();
        sess.values().any(|s| matches!(s.mode, StreamMode::Webrtc | StreamMode::WebrtcPlus | StreamMode::WebrtcVp9))
    };

    if has_webrtc {
        return;
    }

    let mut mtx = mediamtx.lock().unwrap();
    if let Some(child) = mtx.child.take() {
        eprintln!("[mediamtx] kill sidecar");
        let _ = child.kill();
    }
    if let Some(task) = mtx.event_task.take() {
        task.abort();
    }
}

#[tauri::command]
async fn start_stream(
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

    let stream_mode = match mode.as_str() {
        "mjpeg" => StreamMode::Mjpeg,
        "webrtc" => StreamMode::Webrtc,
        "webrtc++" => StreamMode::WebrtcPlus,
        "webrtc-vp9" => StreamMode::WebrtcVp9,
        _ => return Err(format!("Modalità non valida: {}", mode)),
    };

    {
        let sessions = state.sessions.lock().unwrap();
        if sessions.contains_key(&slot) {
            return Err(format!("Slot {} già in stream", slot));
        }
    }

    // Se WebRTC, assicurati che MediaMTX sia in esecuzione
    if matches!(stream_mode, StreamMode::Webrtc | StreamMode::WebrtcPlus | StreamMode::WebrtcVp9) {
        ensure_mediamtx(&app, &state).await?;
    }

    // Se la finestra è minimizzata, ripristinala senza rubare il focus
    // e mandala in fondo allo z-order così non copre il gioco principale.
    let initially_minimized = unsafe { IsIconic(to_hwnd(hwnd)).as_bool() };
    if initially_minimized {
        restore_window_silent(hwnd);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    let (sender, ingest_task) = match &stream_mode {
        StreamMode::Mjpeg => {
            let ingest_port = ingest_port_for_slot(slot);
            let listener = TcpListener::bind(("127.0.0.1", ingest_port))
                .await
                .map_err(|e| format!("Bind ingest port {}: {}", ingest_port, e))?;

            let (sender, _) = broadcast::channel::<Bytes>(BROADCAST_CAPACITY);
            let sender_clone = sender.clone();
            let ingest_task = tauri::async_runtime::spawn(async move {
                ingest_loop(slot, listener, sender_clone).await;
            });
            (Some(sender), Some(ingest_task))
        }
        StreamMode::Webrtc | StreamMode::WebrtcPlus | StreamMode::WebrtcVp9 => (None, None),
    };

    // Keepalive: se l'utente o il sistema riminimizzano la finestra
    // mentre lo stream è attivo, la rimettiamo come la vogliamo noi.
    let sessions = state.sessions.clone();
    let mediamtx = state.mediamtx.clone();
    let keepalive_task = tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            let win = to_hwnd(hwnd);

            // 1) La finestra esiste ancora?
            let alive = unsafe { IsWindow(win).as_bool() };
            if !alive {
                eprintln!("[keepalive slot {}] finestra chiusa dall'utente, fermo stream", slot);
                if let Ok(mut sess) = sessions.lock() {
                    if let Some(session) = sess.remove(&slot) {
                        let was_webrtc = matches!(session.mode, StreamMode::Webrtc | StreamMode::WebrtcPlus | StreamMode::WebrtcVp9);
                        shutdown_session(session);
                        if was_webrtc {
                            drop(sess);
                            maybe_stop_mediamtx(&sessions, &mediamtx);
                        }
                    }
                }
                break;
            }

            // 2) È stata minimizzata? Ripristina.
            let is_min = unsafe { IsIconic(win).as_bool() };
            if is_min {
                eprintln!("[keepalive slot {}] finestra minimizzata, ripristino", slot);
                restore_window_silent(hwnd);
            }
        }
    });

    let title_arg = format!("title={}", window_title);

    let sidecar = match app.shell().sidecar("ffmpeg") {
        Ok(s) => s,
        Err(e) => {
            if let Some(task) = ingest_task { task.abort(); }
            keepalive_task.abort();
            return Err(format!("ffmpeg sidecar non trovato: {}", e));
        }
    };

    let command = match &stream_mode {
        StreamMode::Mjpeg => {
            let output_url = format!("tcp://127.0.0.1:{}", ingest_port_for_slot(slot));
            sidecar.args([
                "-hide_banner",
                "-loglevel", "info",
                "-nostats",
                "-probesize", "32",
                "-analyzeduration", "0",
                "-f", "gdigrab",
                "-framerate", "30",
                "-i", &title_arg,
                "-vf", "mpdecimate=max=30",
                "-fps_mode", "vfr",
                "-c:v", "mjpeg",
                "-q:v", "5",
                "-flush_packets", "1",
                "-f", "mpjpeg",
                &output_url,
            ])
        }
        StreamMode::Webrtc => {
            let output_url = format!("rtmp://127.0.0.1:{}/slot{}", MEDIAMTX_RTMP_PORT, slot);
            sidecar.args([
                "-hide_banner",
                "-loglevel", "info",
                "-nostats",
                "-probesize", "32",
                "-analyzeduration", "0",
                "-f", "gdigrab",
                "-framerate", "30",
                "-i", &title_arg,
                "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2",
                "-c:v", "libx264",
                "-preset", "ultrafast",
                "-tune", "zerolatency",
                "-g", "30",
                "-b:v", "2M",
                "-pix_fmt", "yuv420p",
                "-f", "flv",
                &output_url,
            ])
        }
        StreamMode::WebrtcPlus => {
            let output_url = format!("rtmp://127.0.0.1:{}/slot{}", MEDIAMTX_RTMP_PORT, slot);
            sidecar.args([
                "-hide_banner",
                "-loglevel", "info",
                "-nostats",
                "-probesize", "32",
                "-analyzeduration", "0",
                "-f", "gdigrab",
                "-framerate", "30",
                "-i", &title_arg,
                "-vf", "scale=2*iw:2*ih:flags=neighbor",
                "-c:v", "libx264",
                "-preset", "fast",
                "-tune", "zerolatency",
                "-crf", "18",
                "-pix_fmt", "yuv420p",
                "-g", "30",
                "-f", "flv",
                &output_url,
            ])
        }
        StreamMode::WebrtcVp9 => {
            let output_url = format!("rtmp://127.0.0.1:{}/slot{}", MEDIAMTX_RTMP_PORT, slot);
            sidecar.args([
                "-hide_banner",
                "-loglevel", "info",
                "-nostats",
                "-probesize", "32",
                "-analyzeduration", "0",
                "-f", "gdigrab",
                "-framerate", "30",
                "-i", &title_arg,
                "-vf", "scale=2*iw:2*ih:flags=neighbor",
                "-c:v", "libvpx-vp9",
                "-crf", "18",
                "-b:v", "0",
                "-pix_fmt", "yuv444p",
                "-g", "30",
                "-tune-content", "screen",
                "-f", "flv",
                &output_url,
            ])
        }
    };

    let (mut rx, child) = match command.spawn() {
        Ok(v) => v,
        Err(e) => {
            if let Some(task) = ingest_task { task.abort(); }
            keepalive_task.abort();
            return Err(format!("spawn fallito: {}", e));
        }
    };

    let sessions_for_ff = state.sessions.clone();
    let mediamtx_for_ff = state.mediamtx.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stderr(line) => {
                    eprintln!("[ffmpeg slot {}] {}", slot, String::from_utf8_lossy(&line));
                }
                CommandEvent::Terminated(payload) => {
                    eprintln!("[ffmpeg slot {}] terminato: code={:?}", slot, payload.code);
                    let was_webrtc = {
                        let mut sess = sessions_for_ff.lock().unwrap();
                        match sess.remove(&slot) {
                            Some(session) => {
                                let webrtc = matches!(session.mode, StreamMode::Webrtc | StreamMode::WebrtcPlus | StreamMode::WebrtcVp9);
                                shutdown_session(session);
                                webrtc
                            }
                            None => false,
                        }
                    };
                    if was_webrtc {
                        maybe_stop_mediamtx(&sessions_for_ff, &mediamtx_for_ff);
                    }
                }
                _ => {}
            }
        }
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

    Ok(info)
}

#[tauri::command]
fn stop_stream(state: State<'_, SharedState>, slot: u8) -> Result<(), String> {
    let session = state.sessions.lock().unwrap().remove(&slot)
        .ok_or_else(|| format!("Nessuno stream attivo per slot {}", slot))?;
    let was_webrtc = matches!(session.mode, StreamMode::Webrtc | StreamMode::WebrtcPlus | StreamMode::WebrtcVp9);
    shutdown_session(session);

    if was_webrtc {
        maybe_stop_mediamtx(&state.sessions, &state.mediamtx);
    }

    Ok(())
}

#[tauri::command]
fn list_streams(state: State<'_, SharedState>) -> Vec<StreamInfo> {
    let sessions = state.sessions.lock().unwrap();
    sessions.iter().map(|(&slot, s)| StreamInfo {
        slot,
        title: s.title.clone(),
        mode: s.mode.clone(),
        url: stream_url(slot, &s.mode),
    }).collect()
}


#[derive(Serialize, Clone)]
struct NetInterface {
    name: String,
    ip: String,
    score: i32,
}

#[derive(Serialize, Clone)]
struct ServerInfo {
    interfaces: Vec<NetInterface>,
    port: u16,
    webrtc_port: u16,
}

fn score_interface(name: &str, ip: &std::net::Ipv4Addr) -> i32 {
    let lower = name.to_lowercase();
    let mut score = 0;

    // Penalizza interfacce virtuali in base al nome
    for kw in ["vethernet", "vmware", "virtualbox", "wsl", "hyper-v",
               "loopback", "bluetooth", "tap", "tun", "docker", "tailscale", "zerotier"] {
        if lower.contains(kw) { score -= 100; }
    }

    // Boost per interfacce fisiche tipiche
    for kw in ["wi-fi", "wifi", "ethernet", "wlan", "eth"] {
        if lower.contains(kw) { score += 50; }
    }

    // Bonus in base al range IP (LAN domestiche più probabili in alto)
    let oct = ip.octets();
    match oct[0] {
        192 if oct[1] == 168 => score += 30,
        10 => score += 20,
        172 if (16..=31).contains(&oct[1]) => score += 5,
        _ => {}
    }

    score
}

#[tauri::command]
fn get_server_info() -> Result<ServerInfo, String> {
    use local_ip_address::list_afinet_netifas;
    use std::net::IpAddr;

    let netifas = list_afinet_netifas().map_err(|e| e.to_string())?;

    let mut interfaces: Vec<NetInterface> = netifas
        .into_iter()
        .filter_map(|(name, ip)| {
            let v4 = match ip {
                IpAddr::V4(v) => v,
                _ => return None,
            };
            if v4.is_loopback() || v4.is_link_local() {
                return None;
            }
            Some(NetInterface {
                score: score_interface(&name, &v4),
                name,
                ip: v4.to_string(),
            })
        })
        .collect();

    interfaces.sort_by(|a, b| b.score.cmp(&a.score));

    Ok(ServerInfo { interfaces, port: HTTP_PORT, webrtc_port: MEDIAMTX_WEBRTC_PORT })
}

/// Ripristina una finestra minimizzata senza rubare il focus
/// e la manda in fondo allo z-order. Operazione idempotente.
fn restore_window_silent(hwnd_val: isize) {
    let win = to_hwnd(hwnd_val);
    unsafe {
        if IsIconic(win).as_bool() {
            let _ = ShowWindow(win, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                win,
                HWND_BOTTOM,
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

// ============= AXUM HTTP SERVER =============

async fn stream_handler(
    Path(slot): Path<u8>,
    AxumState(state): AxumState<SharedState>,
) -> Response {
    let receiver = {
        let sessions = state.sessions.lock().unwrap();
        match sessions.get(&slot) {
            Some(session) => {
                match &session.sender {
                    Some(sender) => sender.subscribe(),
                    None => return (StatusCode::NOT_FOUND, "Stream is WebRTC-only").into_response(),
                }
            }
            None => return (StatusCode::NOT_FOUND, "No active stream").into_response(),
        }
    };

    let stream = BroadcastStream::new(receiver).filter_map(|res| async move {
        match res {
            Ok(bytes) => Some(Ok::<_, std::io::Error>(bytes)),
            Err(_) => None,
        }
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "multipart/x-mixed-replace;boundary=ffmpeg")
        .header(header::CACHE_CONTROL, "no-cache, no-store")
        .header("Access-Control-Allow-Origin", "*")
        .body(Body::from_stream(stream))
        .unwrap()
}

async fn viewer_handler(
    Path(slot): Path<u8>,
    AxumState(state): AxumState<SharedState>,
) -> Response {
    let mode = {
        let sessions = state.sessions.lock().unwrap();
        sessions.get(&slot).map(|s| s.mode.clone())
    };

    match mode {
        Some(StreamMode::Webrtc | StreamMode::WebrtcPlus | StreamMode::WebrtcVp9) => {
            Html(format!(
                r#"<!DOCTYPE html>
<html><head>
<title>GBA{slot}</title>
<meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no">
<style>
*{{margin:0;padding:0;box-sizing:border-box}}
html,body{{width:100%;height:100%;background:#000;overflow:hidden}}
iframe{{width:100%;height:100%;border:none}}
</style>
</head><body>
<iframe id="player"></iframe>
<script>
document.getElementById('player').src = 'http://' + window.location.hostname + ':{webrtc_port}/slot{slot}';
</script>
</body></html>"#,
                webrtc_port = MEDIAMTX_WEBRTC_PORT,
            )).into_response()
        }
        _ => {
            Html(format!(
                r#"<!DOCTYPE html>
<html><head>
<title>GBA{slot}</title>
<meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no">
<meta name="apple-mobile-web-app-capable" content="yes">
<style>
*{{margin:0;padding:0;box-sizing:border-box}}
html,body{{width:100%;height:100%;background:#000;overflow:hidden}}
.wrap{{position:fixed;inset:0;display:flex;align-items:center;justify-content:center}}
img{{
  width:100%;
  height:100%;
  object-fit:contain;
  image-rendering:pixelated;
  transition:transform .15s ease;
}}
body.rot img{{
  width:100vh;
  height:100vw;
  transform:rotate(90deg);
}}
.rot-btn{{
  position:fixed;
  bottom:14px;
  right:14px;
  width:44px;
  height:44px;
  border:none;
  border-radius:50%;
  background:rgba(255,255,255,.18);
  color:#fff;
  font-size:22px;
  cursor:pointer;
  display:flex;
  align-items:center;
  justify-content:center;
  -webkit-tap-highlight-color:transparent;
  z-index:10;
  backdrop-filter:blur(4px);
}}
.rot-btn:active{{background:rgba(255,255,255,.35)}}
</style>
</head><body>
<div class="wrap"><img src="/stream/{slot}" alt="GBA{slot}"></div>
<button class="rot-btn" onclick="document.body.classList.toggle('rot')" title="Ruota">⟳</button>
</body></html>"#
            )).into_response()
        }
    }
}

async fn run_http_server(state: SharedState) {
    let router = Router::new()
        .route("/stream/:slot", get(stream_handler))
        .route("/v/:slot", get(viewer_handler))
        .with_state(state);

    let bind = format!("0.0.0.0:{}", HTTP_PORT);
    match TcpListener::bind(&bind).await {
        Ok(listener) => {
            eprintln!("[http] listening on {}", bind);
            if let Err(e) = axum::serve(listener, router).await {
                eprintln!("[http] error: {}", e);
            }
        }
        Err(e) => eprintln!("[http] bind failed on {}: {}", bind, e),
    }
}



// ============= ENTRY POINT =============

fn setup_job_object() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::*;
    use windows::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let job = match CreateJobObjectW(None, PCWSTR::null()) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[job] CreateJobObjectW failed: {}", e);
                return;
            }
        };

        let mut info = JOBOBJECT_BASIC_LIMIT_INFORMATION::default();
        info.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        if let Err(e) = SetInformationJobObject(
            job,
            JobObjectBasicLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>() as u32,
        ) {
            eprintln!("[job] SetInformationJobObject failed: {}", e);
            return;
        }

        if let Err(e) = AssignProcessToJobObject(job, GetCurrentProcess()) {
            eprintln!("[job] AssignProcessToJobObject failed: {}", e);
            return;
        }

        // Wrap in a non-Copy type so it won't be dropped (closing the handle).
        // The Job Object handle must stay open for the process lifetime — when
        // the process exits, the kernel closes the handle and KILL_ON_JOB_CLOSE
        // terminates all child processes in the job.
        #[allow(dead_code)]
        struct JobHandle(HANDLE);
        std::mem::forget(JobHandle(job));
        eprintln!("[job] KILL_ON_JOB_CLOSE job object created");
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    setup_job_object();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .manage(SharedState::default())
        .setup(|app| {
            let state = app.state::<SharedState>().inner().clone();
            tauri::async_runtime::spawn(async move {
                run_http_server(state).await;
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let state = window.state::<SharedState>().inner().clone();
                let mut sessions = state.sessions.lock().unwrap();
                for (_, session) in sessions.drain() {
                    shutdown_session(session);
                }
                drop(sessions);
                let mut mtx = state.mediamtx.lock().unwrap();
                if let Some(child) = mtx.child.take() {
                    let _ = child.kill();
                }
                if let Some(task) = mtx.event_task.take() {
                    task.abort();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_windows,
            list_gba_windows,
            start_stream,
            stop_stream,
            list_streams,
            get_server_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
