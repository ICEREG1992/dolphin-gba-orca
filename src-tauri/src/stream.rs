use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes, BytesMut};
use serde::Serialize;
use tauri::State;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::mediamtx;
use crate::network::{HTTP_PORT, MEDIAMTX_RTMP_PORT, MEDIAMTX_WEBRTC_PORT};
use crate::platform::{is_window_alive, is_window_minimized, restore_window_silent};
use crate::SharedState;

const INGEST_BUFFER_SIZE: usize = 64 * 1024;
const PARSER_CAPACITY: usize = 256 * 1024;
const STATS_LOG_EVERY_N_FRAMES: u64 = 60;

pub type FrameSender = Arc<watch::Sender<Option<Bytes>>>;

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
    /// Latest-frame channel for MJPEG viewers. Each frame published here
    /// replaces the previous one — slow viewers always jump to the newest
    /// frame instead of accumulating a backlog. None for WebRTC sessions.
    pub frame_tx: Option<FrameSender>,
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

/// Incremental parser for FFmpeg's `mpjpeg` muxer output.
///
/// Each frame is emitted as `--ffmpeg\r\nContent-type: image/jpeg\r\n
/// Content-length: N\r\n\r\n<N bytes>\r\n`. We don't care about the boundary
/// or trailers — we just look for the next blank line that ends a header
/// block, parse Content-length, and emit the next N bytes as one JPEG frame.
struct MjpegParser {
    buf: BytesMut,
    state: ParserState,
}

#[derive(Clone, Copy)]
enum ParserState {
    Headers,
    Body { remaining: usize },
}

impl MjpegParser {
    fn new() -> Self {
        Self { buf: BytesMut::with_capacity(PARSER_CAPACITY), state: ParserState::Headers }
    }

    fn feed<F: FnMut(Bytes)>(&mut self, data: &[u8], mut on_frame: F) -> Result<(), &'static str> {
        self.buf.extend_from_slice(data);
        loop {
            match self.state {
                ParserState::Headers => {
                    let Some(end) = find_double_crlf(&self.buf) else { return Ok(()); };
                    let len = parse_content_length(&self.buf[..end])
                        .ok_or("missing or unparseable Content-length")?;
                    self.buf.advance(end + 4);
                    self.state = ParserState::Body { remaining: len };
                }
                ParserState::Body { remaining } => {
                    if self.buf.len() < remaining { return Ok(()); }
                    let frame = self.buf.split_to(remaining).freeze();
                    self.state = ParserState::Headers;
                    on_frame(frame);
                }
            }
        }
    }
}

/// Wrap a single JPEG frame in the multipart envelope expected by browsers
/// reading `multipart/x-mixed-replace; boundary=ffmpeg`. One allocation per
/// frame; the resulting `Bytes` is then Arc-shared across all viewers.
fn wrap_multipart(jpeg: Bytes) -> Bytes {
    let len_str = jpeg.len().to_string();
    let mut out = BytesMut::with_capacity(jpeg.len() + 64 + len_str.len());
    out.extend_from_slice(b"--ffmpeg\r\nContent-Type: image/jpeg\r\nContent-Length: ");
    out.extend_from_slice(len_str.as_bytes());
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(&jpeg);
    out.extend_from_slice(b"\r\n");
    out.freeze()
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 { return None; }
    (0..=buf.len() - 4).find(|&i| &buf[i..i + 4] == b"\r\n\r\n")
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    const NEEDLE: &[u8] = b"content-length:";
    let n = NEEDLE.len();
    let mut i = 0;
    while i + n <= headers.len() {
        if headers[i..i + n].eq_ignore_ascii_case(NEEDLE) {
            let mut j = i + n;
            while j < headers.len() && (headers[j] == b' ' || headers[j] == b'\t') { j += 1; }
            let start = j;
            while j < headers.len() && headers[j].is_ascii_digit() { j += 1; }
            if j == start { return None; }
            return std::str::from_utf8(&headers[start..j]).ok()?.parse().ok();
        }
        i += 1;
    }
    None
}

/// Per-slot timing tracker. Logs a one-line summary every N frames so jitter
/// (avg/max gap) and viewer count are visible without per-frame spam.
struct IngestStats {
    frames: u64,
    bytes: u64,
    window_start: Instant,
    last_frame: Option<Instant>,
    max_gap_ms: u64,
}

impl IngestStats {
    fn new() -> Self {
        Self {
            frames: 0,
            bytes: 0,
            window_start: Instant::now(),
            last_frame: None,
            max_gap_ms: 0,
        }
    }

    fn record(&mut self, slot: u8, viewers: usize, size: usize) {
        let now = Instant::now();
        if let Some(prev) = self.last_frame {
            let gap = now.duration_since(prev).as_millis() as u64;
            if gap > self.max_gap_ms { self.max_gap_ms = gap; }
        }
        self.last_frame = Some(now);
        self.frames += 1;
        self.bytes += size as u64;

        if self.frames >= STATS_LOG_EVERY_N_FRAMES {
            let elapsed = now.duration_since(self.window_start).as_millis().max(1) as u64;
            let avg_gap = elapsed / self.frames;
            let kbps = self.bytes * 8 / elapsed;
            eprintln!(
                "[ingest slot {}] frames={} elapsed={}ms avg_gap={}ms max_gap={}ms ~{}kbps viewers={}",
                slot, self.frames, elapsed, avg_gap, self.max_gap_ms, kbps, viewers
            );
            self.frames = 0;
            self.bytes = 0;
            self.window_start = now;
            self.max_gap_ms = 0;
        }
    }
}

async fn ingest_loop(slot: u8, listener: TcpListener, sender: FrameSender) {
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
        // Local TCP, but disable Nagle so FFmpeg's small writes (the trailing
        // boundary) reach us promptly without a 40ms ack-delay penalty.
        let _ = socket.set_nodelay(true);
        eprintln!("[ingest slot {}] ffmpeg connesso da {}", slot, addr);

        let mut parser = MjpegParser::new();
        let mut stats = IngestStats::new();

        loop {
            match socket.read(&mut buf).await {
                Ok(0) => {
                    eprintln!("[ingest slot {}] ffmpeg disconnesso", slot);
                    break;
                }
                Ok(n) => {
                    let result = parser.feed(&buf[..n], |jpeg| {
                        let size = jpeg.len();
                        let viewers = sender.receiver_count();
                        // Wrap each JPEG in its multipart envelope once, here,
                        // so the HTTP handler is a straight pass-through and
                        // every viewer shares the same Arc-backed Bytes.
                        let payload = wrap_multipart(jpeg);
                        // send_replace stores the latest frame even when no
                        // viewers are subscribed, so the first viewer to
                        // connect immediately sees the freshest frame instead
                        // of waiting for the next FFmpeg packet. Plain send()
                        // would return Err and drop the frame.
                        sender.send_replace(Some(payload));
                        stats.record(slot, viewers, size);
                    });
                    if let Err(e) = result {
                        eprintln!("[ingest slot {}] parse error: {} - dropping connection", slot, e);
                        break;
                    }
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

/// Platform-specific FFmpeg input args: which capture device, how the
/// target window is identified. On Windows that's `gdigrab` + `-i title=`;
/// on Linux X11 that's `x11grab` + `-window_id` (XComposite-tracked) with
/// the X display as the input URL. Everything else (codec, filter, muxer)
/// is shared across platforms in `build_ffmpeg_args`.
#[cfg(windows)]
fn capture_input_args(_hwnd: isize, title: &str) -> Vec<String> {
    vec![
        "-f".into(), "gdigrab".into(),
        "-framerate".into(), "30".into(),
        "-i".into(), format!("title={}", title),
    ]
}

#[cfg(target_os = "linux")]
fn capture_input_args(hwnd: isize, _title: &str) -> Vec<String> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
    vec![
        "-f".into(), "x11grab".into(),
        "-framerate".into(), "30".into(),
        "-window_id".into(), format!("0x{:x}", hwnd as u32),
        "-i".into(), display,
    ]
}

/// Build the full FFmpeg arg list for a given mode. Capture input args are
/// platform-specific (gdigrab/x11grab); per-mode args cover the video filter,
/// codec, pixel format, and output muxer.
fn build_ffmpeg_args(mode: &StreamMode, hwnd: isize, window_title: &str, output_url: &str) -> Vec<String> {
    let prelude: &[&str] = &[
        "-hide_banner",
        "-loglevel", "info",
        "-nostats",
        "-probesize", "32",
        "-analyzeduration", "0",
    ];
    let capture = capture_input_args(hwnd, window_title);
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
    prelude
        .iter()
        .map(|s| (*s).to_string())
        .chain(capture.into_iter())
        .chain(specific.iter().map(|s| (*s).to_string()))
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

    // For MJPEG, bind the local TCP listener and create a watch channel
    // (latest-frame, no backlog) before spawning FFmpeg.
    let (frame_tx, listener) = if matches!(stream_mode, StreamMode::Mjpeg) {
        let port = ingest_port_for_slot(slot);
        let listener = TcpListener::bind(("127.0.0.1", port)).await
            .map_err(|e| format!("Bind ingest port {}: {}", port, e))?;
        let (tx, _) = watch::channel::<Option<Bytes>>(None);
        (Some(Arc::new(tx)), Some(listener))
    } else {
        (None, None)
    };

    let output_url = if stream_mode.is_webrtc() {
        format!("rtmp://127.0.0.1:{}/slot{}", MEDIAMTX_RTMP_PORT, slot)
    } else {
        format!("tcp://127.0.0.1:{}", ingest_port_for_slot(slot))
    };

    let sidecar = app.shell().sidecar("ffmpeg")
        .map_err(|e| format!("ffmpeg sidecar non trovato: {}", e))?;
    let args = build_ffmpeg_args(&stream_mode, hwnd, &window_title, &output_url);
    let (mut rx, child) = sidecar.args(args).spawn()
        .map_err(|e| format!("spawn fallito: {}", e))?;

    // Spawn the MJPEG ingest task (no-op for WebRTC modes).
    let ingest_task = match (listener, frame_tx.clone()) {
        (Some(l), Some(tx)) => Some(tauri::async_runtime::spawn(async move {
            ingest_loop(slot, l, tx).await;
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
        frame_tx,
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
