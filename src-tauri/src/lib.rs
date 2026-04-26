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
    for slot in 1u8..=4 {
        if title.contains(&format!("GBA{}", slot)) {
            return Some(slot);
        }
    }
    None
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

    // Salta finestre minimizzate (dimensione 0x0)

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
const BROADCAST_CAPACITY: usize = 32;

fn ingest_port_for_slot(slot: u8) -> u16 {
    9000 + slot as u16
}

struct StreamSession {
    slot: u8,
    title: String,
    child: CommandChild,
    sender: broadcast::Sender<Bytes>,
    ingest_task: tauri::async_runtime::JoinHandle<()>,
    keepalive_task: tauri::async_runtime::JoinHandle<()>,
}

#[derive(Clone, Default)]
struct SharedState {
    sessions: Arc<Mutex<HashMap<u8, StreamSession>>>,
}

#[derive(Serialize, Clone)]
struct StreamInfo {
    slot: u8,
    title: String,
    url: String,
}

fn stream_url(slot: u8) -> String {
    format!("http://127.0.0.1:{}/v/{}", HTTP_PORT, slot)
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

#[tauri::command]
async fn start_stream(
    app: tauri::AppHandle,
    state: State<'_, SharedState>,
    slot: u8,
    hwnd: isize,
    window_title: String,
) -> Result<StreamInfo, String> {
    if !(1..=4).contains(&slot) {
        return Err(format!("Slot non valido: {}", slot));
    }

    {
        let sessions = state.sessions.lock().unwrap();
        if sessions.contains_key(&slot) {
            return Err(format!("Slot {} già in stream", slot));
        }
    }

    // Se la finestra è minimizzata, ripristinala senza rubare il focus
    // e mandala in fondo allo z-order così non copre il gioco principale.
    let initially_minimized = unsafe {
        IsIconic(HWND(hwnd as *mut _)).as_bool()
    };
    if initially_minimized {
        restore_window_silent(hwnd);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    let ingest_port = ingest_port_for_slot(slot);
    let listener = TcpListener::bind(("127.0.0.1", ingest_port))
        .await
        .map_err(|e| format!("Bind ingest port {}: {}", ingest_port, e))?;

    let (sender, _) = broadcast::channel::<Bytes>(BROADCAST_CAPACITY);

    let sender_for_ingest = sender.clone();
    let ingest_task = tauri::async_runtime::spawn(async move {
        ingest_loop(slot, listener, sender_for_ingest).await;
    });

// Keepalive: se l'utente o il sistema riminimizzano la finestra
    // mentre lo stream è attivo, la rimettiamo come la vogliamo noi.
    // Attivo solo se l'utente l'aveva avviata da minimizzata.
    let hwnd_for_keepalive = hwnd;
    let slot_for_keepalive = slot;
    let sessions_for_keepalive = state.sessions.clone();
    let keepalive_task = tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            let win = unsafe { HWND(hwnd_for_keepalive as *mut _) };

            // 1) La finestra esiste ancora?
            let alive = unsafe { IsWindow(win).as_bool() };
            if !alive {
                eprintln!("[keepalive slot {}] finestra chiusa dall'utente, fermo stream", slot_for_keepalive);
                // Killiamo FFmpeg e rimuoviamo la sessione. Il task logger
                // rileverà comunque la terminazione ma intanto puliamo.
                if let Ok(mut sessions) = sessions_for_keepalive.lock() {
                    if let Some(session) = sessions.remove(&slot_for_keepalive) {
                        let _ = session.child.kill();
                        session.ingest_task.abort();
                    }
                }
                break; // esce dal loop, il task finisce
            }

            // 2) È stata minimizzata? Ripristina.
            let is_min = unsafe { IsIconic(win).as_bool() };
            if is_min {
                eprintln!("[keepalive slot {}] finestra minimizzata, ripristino", slot_for_keepalive);
                restore_window_silent(hwnd_for_keepalive);
            }
        }
    });

    let title_arg = format!("title={}", window_title);
    let output_url = format!("tcp://127.0.0.1:{}", ingest_port);

    let sidecar = match app.shell().sidecar("ffmpeg") {
        Ok(s) => s,
        Err(e) => {
            ingest_task.abort();
            keepalive_task.abort();
            return Err(format!("ffmpeg sidecar non trovato: {}", e));
        }
    };

    let command = sidecar.args([
            "-hide_banner",
            "-loglevel", "info",
            "-nostats",
            "-probesize", "32",
            "-analyzeduration", "0",
            "-f", "gdigrab",
            "-framerate", "30",
            "-i", &title_arg,
            "-vf", "mpdecimate",
            "-fps_mode", "vfr",
            "-c:v", "mjpeg",
            "-q:v", "5",
            "-flush_packets", "1",
            "-f", "mpjpeg",
            &output_url,
        ]);

    let (mut rx, child) = match command.spawn() {
        Ok(v) => v,
        Err(e) => {
            ingest_task.abort();
            keepalive_task.abort();
            return Err(format!("spawn fallito: {}", e));
        }
    };

        let slot_log = slot;
        let sessions_for_cleanup = state.sessions.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    CommandEvent::Stderr(line) => {
                        eprintln!("[ffmpeg slot {}] {}", slot_log, String::from_utf8_lossy(&line));
                    }
                    CommandEvent::Terminated(payload) => {
                    eprintln!("[ffmpeg slot {}] terminato: code={:?}", slot_log, payload.code);
                    if let Ok(mut sessions) = sessions_for_cleanup.lock() {
                        if let Some(session) = sessions.remove(&slot_log) {
                            session.ingest_task.abort();
                            session.keepalive_task.abort();
                        }
                    }
                }
                    _ => {}
                }
            }
        });

    let info = StreamInfo {
        slot,
        title: window_title.clone(),
        url: stream_url(slot),
    };

    state.sessions.lock().unwrap().insert(slot, StreamSession {
        slot,
        title: window_title,
        child,
        sender,
        ingest_task,
        keepalive_task,
    });

    Ok(info)
}

#[tauri::command]
fn stop_stream(state: State<'_, SharedState>, slot: u8) -> Result<(), String> {
    let session = {
        let mut sessions = state.sessions.lock().unwrap();
        sessions.remove(&slot)
            .ok_or_else(|| format!("Nessuno stream attivo per slot {}", slot))?
    };
    let _ = session.child.kill();
    session.ingest_task.abort();
    session.keepalive_task.abort();
    Ok(())
}

#[tauri::command]
fn list_streams(state: State<'_, SharedState>) -> Vec<StreamInfo> {
    let sessions = state.sessions.lock().unwrap();
    sessions.values().map(|s| StreamInfo {
        slot: s.slot,
        title: s.title.clone(),
        url: stream_url(s.slot),
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

    Ok(ServerInfo { interfaces, port: HTTP_PORT })
}

/// Ripristina una finestra minimizzata senza rubare il focus
/// e la manda in fondo allo z-order. Operazione idempotente.
fn restore_window_silent(hwnd_val: isize) {
    unsafe {
        let win = HWND(hwnd_val as *mut _);
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
            Some(session) => session.sender.subscribe(),
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

async fn viewer_handler(Path(slot): Path<u8>) -> Html<String> {
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
    ))
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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