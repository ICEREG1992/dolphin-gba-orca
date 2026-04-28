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
iframe{width:100%;height:100%;border:none;z-index:1;position:relative}
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
.gp-toast{position:fixed;top:12px;right:12px;display:flex;align-items:center;gap:8px;z-index:99999;padding:10px 16px;border-radius:4px;background:rgba(0,120,215,0.92);color:#fff;font:600 15px system-ui,sans-serif;box-shadow:0 3px 10px rgba(0,0,0,0.4);backdrop-filter:blur(6px);transform:translateX(120%) translateZ(0);opacity:0;transition:transform .3s ease,opacity .3s ease;will-change:transform}
.gp-toast.visible{transform:translateX(0) translateZ(0);opacity:1}
.gp-toast.connected{background:rgba(60,200,90,0.95)}
.gp-close{width:30px;height:30px;padding:0;display:flex;align-items:center;justify-content:center;border:none;border-radius:4px;background:rgba(255,255,255,0.2);color:#ff0000;font-size:20px;line-height:1;cursor:pointer;-webkit-tap-highlight-color:transparent;font-weight:700}
.gp-close:active{background:rgba(255,255,255,0.35)}
"#;

const GAMEPAD_HTML: &str =
    r#"<div id="gp-toast" class="gp-toast"><span id="gp-text">🎮 Connect Controller</span><button class="gp-close" onclick="dismissGamepad()" title="Close">×</button></div>"#;

// Tiny client: open WS once user taps the button, find a gamepad (browsers
// only expose one after a button press on the page), poll at rAF rate, send
// only when the serialized state changes. WS URL uses location.host so the
// port matches whatever served the viewer (8080 today).
const GAMEPAD_JS: &str = r#"
const SLOT={slot};
let ws=null,gpIndex=null;
let prevBuf=null;
let gpHideTimer=null;
let gpCycleTimer=null;
let gpDismissed=false;
let pollTimer=null;
let detectTimer=null;
const cycleTexts=['🎮 Connect Controller','🎮 Press any button'];
let cycleIdx=0;
function getToast(){ return document.getElementById('gp-toast'); }
function getText(){ return document.getElementById('gp-text'); }
function setToastText(text){
  const span=getText();
  if(span) span.textContent=text;
}
function showToast(autoHide){
  if(gpDismissed) return;
  const t=getToast();
  if(!t) return;
  t.classList.add('visible');
  if(gpHideTimer){ clearTimeout(gpHideTimer); gpHideTimer=null; }
  if(autoHide){ gpHideTimer=setTimeout(()=>{ hideToast(); },3000); }
}
function hideToast(){
  const t=getToast();
  if(t){ t.classList.remove('visible'); t.classList.remove('connected'); }
  if(gpHideTimer){ clearTimeout(gpHideTimer); gpHideTimer=null; }
}
function startCycle(){
  if(gpDismissed) return;
  if(gpCycleTimer){ clearTimeout(gpCycleTimer); }
  setToastText(cycleTexts[cycleIdx]);
  cycleIdx=(cycleIdx+1)%cycleTexts.length;
  showToast(false);
  gpCycleTimer=setTimeout(startCycle,3000);
}
function stopCycle(){ if(gpCycleTimer){ clearTimeout(gpCycleTimer); gpCycleTimer=null; } }
function stopTimers(){
  if(pollTimer){ clearTimeout(pollTimer); pollTimer=null; }
  if(detectTimer){ clearTimeout(detectTimer); detectTimer=null; }
}
function dismissGamepad(){ gpDismissed=true; if(ws){ try{ws.close();}catch(e){} ws=null; } gpIndex=null; prevBuf=null; stopTimers(); stopCycle(); hideToast(); }
function isGpConnected(){ return ws&&ws.readyState===1&&gpIndex!==null; }
function refresh(){
  if(gpDismissed) return;
  const t=getToast();
  const open=ws&&ws.readyState===1;
  if(gpIndex!==null&&open){ stopCycle(); if(t) t.classList.add('connected'); setToastText('🎮 Connected'); showToast(true); }
  else if(open){ stopCycle(); if(t) t.classList.remove('connected'); setToastText('🎮 Press any button'); showToast(false); }
  else { if(t) t.classList.remove('connected'); startCycle(); }
}
function connectGamepad(){
  if(gpDismissed) return;
  if(ws&&(ws.readyState===0||ws.readyState===1))return;
  try{ws=new WebSocket('ws://'+location.host+'/ws/'+SLOT);}
  catch(e){return;}
  ws.onopen=()=>{refresh();stopTimers();poll();};
  ws.onclose=()=>{ws=null;gpIndex=null;prevBuf=null;refresh();autoDetectGamepad();};
  ws.onerror=()=>{try{ws&&ws.close();}catch(e){}};
}
window.addEventListener('gamepadconnected',e=>{
  gpIndex=e.gamepad.index;
  if(!ws||ws.readyState!==1) connectGamepad();
  refresh();
});
window.addEventListener('gamepaddisconnected',e=>{
  if(e.gamepad.index===gpIndex){gpIndex=null;prevBuf=null;refresh();}
});
function bufEqual(a,b){ if(!a||!b||a.length!==b.length) return false; for(let i=0;i<a.length;i++) if(a[i]!==b[i]) return false; return true; }
function poll(){
  if(!ws||ws.readyState!==1)return;
  if(gpIndex===null){
    const pads=navigator.getGamepads();
    for(let i=0;i<pads.length;i++){if(pads[i]){gpIndex=i;refresh();break;}}
  }
  const gp=gpIndex!==null?navigator.getGamepads()[gpIndex]:null;
  if(gp){
    const buf=new Uint8Array(32);
    const dv=new DataView(buf.buffer);
    for(let i=0;i<4;i++){ dv.setFloat32(i*4,gp.axes[i]||0,true); }
    for(let i=0;i<16;i++){ buf[16+i]=gp.buttons[i]&&gp.buttons[i].pressed?1:0; }
    if(!bufEqual(buf,prevBuf)){ ws.send(buf); prevBuf=buf; }
  }
  pollTimer=setTimeout(poll,8);
}
function autoDetectGamepad(){
  if(gpDismissed) return;
  if(!ws||ws.readyState!==1){
    const pads=navigator.getGamepads();
    for(let i=0;i<pads.length;i++){
      if(pads[i]){ gpIndex=i; connectGamepad(); break; }
    }
  }
  detectTimer=setTimeout(autoDetectGamepad,8);
}
autoDetectGamepad();
startCycle();
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
    let rc = state.remote_controller.load(std::sync::atomic::Ordering::Relaxed);

    // Inject the gamepad fragments first so any `{slot}` they reference still
    // gets substituted by the next .replace() pass.
    let template = match mode {
        Some(m) if m.is_webrtc() => WEBRTC_VIEWER_HTML,
        _ => MJPEG_VIEWER_HTML,
    };
    let (css, html_frag, js) = if rc {
        (GAMEPAD_CSS, GAMEPAD_HTML, GAMEPAD_JS)
    } else {
        ("", "", "")
    };
    let html = template
        .replace("{gp_css}", css)
        .replace("{gp_html}", html_frag)
        .replace("{gp_js}", js)
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
            Message::Binary(data) => {
                if let Some(input) = GamepadInput::from_bytes(&data) {
                    state.apply(&input);
                } else {
                    tracing::debug!("[ws slot {}] bad binary payload (len={})", slot, data.len());
                }
            }
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
