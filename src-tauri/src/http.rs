use axum::{
    body::Body,
    extract::{Path, State as AxumState},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_stream::wrappers::WatchStream;

use crate::network::{HTTP_PORT, MEDIAMTX_WEBRTC_PORT};
use crate::SharedState;

const MJPEG_VIEWER_HTML: &str = r#"<!DOCTYPE html>
<html><head>
<title>GBA{slot}</title>
<meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no">
<meta name="apple-mobile-web-app-capable" content="yes">
<style>
*{margin:0;padding:0;box-sizing:border-box}
html,body{width:100%;height:100%;background:#000;overflow:hidden}
.wrap{position:fixed;inset:0;display:flex;align-items:center;justify-content:center}
img{
  width:100%;
  height:100%;
  object-fit:contain;
  image-rendering:pixelated;
  transition:transform .15s ease;
}
body.rot img{
  width:100vh;
  height:100vw;
  transform:rotate(90deg);
}
.rot-btn{
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
}
.rot-btn:active{background:rgba(255,255,255,.35)}
</style>
</head><body>
<div class="wrap"><img src="/stream/{slot}" alt="GBA{slot}"></div>
<button class="rot-btn" onclick="document.body.classList.toggle('rot')" title="Ruota">⟳</button>
</body></html>"#;

const WEBRTC_VIEWER_HTML: &str = r#"<!DOCTYPE html>
<html><head>
<title>GBA{slot}</title>
<meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no">
<style>
*{margin:0;padding:0;box-sizing:border-box}
html,body{width:100%;height:100%;background:#000;overflow:hidden}
iframe{width:100%;height:100%;border:none}
</style>
</head><body>
<iframe id="player"></iframe>
<script>
document.getElementById('player').src = 'http://' + window.location.hostname + ':{webrtc_port}/slot{slot}';
</script>
</body></html>"#;

async fn stream_handler(
    Path(slot): Path<u8>,
    AxumState(state): AxumState<SharedState>,
) -> Response {
    let receiver = {
        let sessions = state.sessions.lock().unwrap();
        match sessions.get(&slot) {
            Some(session) => match &session.frame_tx {
                Some(tx) => tx.subscribe(),
                None => return (StatusCode::NOT_FOUND, "Stream is WebRTC-only").into_response(),
            },
            None => return (StatusCode::NOT_FOUND, "No active stream").into_response(),
        }
    };

    eprintln!("[http slot {}] viewer connected", slot);

    // Each item is a single complete multipart frame already wrapped by the
    // ingest loop. WatchStream coalesces missed frames to the latest, so
    // slow viewers drop whole frames cleanly instead of getting torn ones.
    let stream = WatchStream::new(receiver).filter_map(|opt| async move {
        opt.map(Ok::<_, std::io::Error>)
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
    let mode = state.sessions.lock().unwrap().get(&slot).map(|s| s.mode.clone());
    let slot_str = slot.to_string();

    let html = match mode {
        Some(m) if m.is_webrtc() => WEBRTC_VIEWER_HTML
            .replace("{slot}", &slot_str)
            .replace("{webrtc_port}", &MEDIAMTX_WEBRTC_PORT.to_string()),
        _ => MJPEG_VIEWER_HTML.replace("{slot}", &slot_str),
    };
    Html(html).into_response()
}

pub async fn run_http_server(state: SharedState) {
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
