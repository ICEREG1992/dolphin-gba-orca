use axum::{
    body::Body,
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, Path, State as AxumState},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::StreamExt;
use tokio::net::TcpListener;
use tokio_stream::wrappers::WatchStream;

use crate::input::{GamepadInput, SlotKeyState};
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
{gp_css}
</style>
</head><body>
<div class="wrap"><img src="/stream/{slot}" alt="GBA{slot}"></div>
<button class="rot-btn" onclick="document.body.classList.toggle('rot')" title="Ruota">⟳</button>
{gp_html}
<script>{gp_js}</script>
</body></html>"#;

const WEBRTC_VIEWER_HTML: &str = r#"<!DOCTYPE html>
<html><head>
<title>GBA{slot}</title>
<meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no">
<style>
*{margin:0;padding:0;box-sizing:border-box}
html,body{width:100%;height:100%;background:#000;overflow:hidden}
iframe{width:100%;height:100%;border:none}
{gp_css}
</style>
</head><body>
<iframe id="player"></iframe>
{gp_html}
<script>
document.getElementById('player').src = 'http://' + window.location.hostname + ':{webrtc_port}/slot{slot}';
</script>
<script>{gp_js}</script>
</body></html>"#;

const GAMEPAD_CSS: &str = r#"
.gp-btn{
  position:fixed;
  top:14px;
  right:14px;
  padding:8px 14px;
  border:none;
  border-radius:18px;
  background:rgba(255,255,255,.18);
  color:#fff;
  font:500 13px system-ui,sans-serif;
  cursor:pointer;
  -webkit-tap-highlight-color:transparent;
  z-index:10;
  backdrop-filter:blur(4px);
}
.gp-btn:active{background:rgba(255,255,255,.35)}
.gp-btn.on{background:rgba(60,200,90,.45)}
"#;

const GAMEPAD_HTML: &str =
    r#"<button id="gp-status" class="gp-btn" onclick="connectGamepad()">🎮 Connetti controller</button>"#;

// Tiny client: open WS once user taps the button, find a gamepad (browsers
// only expose one after a button press on the page), poll at rAF rate, send
// only when the serialized state changes. WS URL uses location.host so the
// port matches whatever served the viewer (8080 today).
const GAMEPAD_JS: &str = r#"
const SLOT={slot};
let ws=null,gpIndex=null,prev=null;
function setStatus(t,on){
  const e=document.getElementById('gp-status');
  if(!e)return;
  e.textContent=t;
  e.classList.toggle('on',!!on);
}
function refresh(){
  const open=ws&&ws.readyState===1;
  if(gpIndex!==null&&open)setStatus('🎮 Connesso',true);
  else if(open)setStatus('🎮 Premi un tasto del controller');
  else setStatus('🎮 Connetti controller');
}
function connectGamepad(){
  if(ws&&(ws.readyState===0||ws.readyState===1))return;
  try{ws=new WebSocket('ws://'+location.host+'/ws/'+SLOT);}
  catch(e){setStatus('🎮 Errore connessione');return;}
  ws.onopen=()=>{refresh();poll();};
  ws.onclose=()=>{ws=null;prev=null;refresh();};
  ws.onerror=()=>{try{ws&&ws.close();}catch(e){}};
}
window.addEventListener('gamepadconnected',e=>{gpIndex=e.gamepad.index;refresh();});
window.addEventListener('gamepaddisconnected',e=>{
  if(e.gamepad.index===gpIndex){gpIndex=null;prev=null;refresh();}
});
function poll(){
  if(!ws||ws.readyState!==1)return;
  if(gpIndex===null){
    const pads=navigator.getGamepads();
    for(let i=0;i<pads.length;i++){if(pads[i]){gpIndex=i;refresh();break;}}
  }
  const gp=gpIndex!==null?navigator.getGamepads()[gpIndex]:null;
  if(gp){
    const payload=JSON.stringify({
      axes:Array.from(gp.axes).slice(0,4).map(a=>Math.round(a*1000)/1000),
      buttons:gp.buttons.map(b=>b.pressed?1:0)
    });
    if(payload!==prev){ws.send(payload);prev=payload;}
  }
  requestAnimationFrame(poll);
}
"#;

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

    tracing::info!("[http slot {}] viewer connected", slot);

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

    // Inject the gamepad fragments first so any `{slot}` they reference still
    // gets substituted by the next .replace() pass.
    let template = match mode {
        Some(m) if m.is_webrtc() => WEBRTC_VIEWER_HTML,
        _ => MJPEG_VIEWER_HTML,
    };
    let html = template
        .replace("{gp_css}", GAMEPAD_CSS)
        .replace("{gp_html}", GAMEPAD_HTML)
        .replace("{gp_js}", GAMEPAD_JS)
        .replace("{slot}", &slot_str)
        .replace("{webrtc_port}", &MEDIAMTX_WEBRTC_PORT.to_string());
    Html(html).into_response()
}

async fn ws_handler(Path(slot): Path<u8>, ws: WebSocketUpgrade) -> Response {
    if !(1..=4).contains(&slot) {
        return (StatusCode::NOT_FOUND, "Invalid slot").into_response();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, slot))
}

async fn handle_ws(mut socket: WebSocket, slot: u8) {
    tracing::info!("[ws slot {}] gamepad client connected", slot);
    // SlotKeyState's Drop releases every still-pressed key, so a client
    // disconnecting mid-press can't leave a key stuck on the host.
    let mut state = SlotKeyState::new(slot);
    while let Some(msg) = socket.recv().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("[ws slot {}] recv error: {}", slot, e);
                break;
            }
        };
        match msg {
            Message::Text(text) => match serde_json::from_str::<GamepadInput>(&text) {
                Ok(input) => state.apply(&input),
                Err(e) => tracing::debug!("[ws slot {}] bad json: {}", slot, e),
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
    tracing::info!("[ws slot {}] gamepad client disconnected", slot);
}

pub async fn run_http_server(state: SharedState) {
    let router = Router::new()
        .route("/stream/:slot", get(stream_handler))
        .route("/v/:slot", get(viewer_handler))
        .route("/ws/:slot", get(ws_handler))
        .with_state(state);

    let bind = format!("0.0.0.0:{}", HTTP_PORT);
    match TcpListener::bind(&bind).await {
        Ok(listener) => {
            tracing::info!("[http] listening on {}", bind);
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!("[http] error: {}", e);
            }
        }
        Err(e) => tracing::error!("[http] bind failed on {}: {}", bind, e),
    }
}
