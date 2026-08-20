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
html,body{width:100%;height:100%;background:rgb(30, 30, 30);overflow:hidden}
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
html,body{width:100%;height:100%;background:rgb(30, 30, 30);overflow:hidden}
iframe{width:100%;height:66%;border:none;z-index:1;position:relative;pointer-events:none;}
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

const VIRTUAL_CSS: &str =
    r#"
#virtual-controller {
  position: fixed;
  left: 20px;
  right: 20px;
  bottom: 40px;
  height: 220px;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: flex-end;
  pointer-events: none;
  z-index: 10000;
}

#virtual-controller .vc-btn {
  pointer-events: auto;
  touch-action: none;
  user-select: none;
  -webkit-user-select: none;
  -webkit-touch-callout: none;
  font-family: sans-serif;
  font-weight: 700;
  border: 2px solid rgba(255,255,255,0.45);
  background: rgba(30,30,30,0.75);
  color: white;
  box-shadow: 0 2px 6px rgba(0,0,0,0.4);
}

#virtual-controller .vc-btn:active,
#virtual-controller .vc-btn.pressed {
  background: rgba(100,100,100,0.9);
  transform: scale(0.94);
}

#virtual-controller .vc-shoulders {
  width: 100%;
  display: flex;
  justify-content: space-between;
  margin-bottom: 12px;
}

#virtual-controller .vc-l,
#virtual-controller .vc-r {
  width: 64px;
  height: 38px;
  border-radius: 10px;
}

#virtual-controller .vc-main {
  width: 90%;
  display: flex;
  align-items: center;
  justify-content: space-between;
}

#virtual-controller .vc-dpad {
  position: relative;
  width: 120px;
  height: 120px;
  /* Hit-area divs (see .vc-hit below) extend past the visible 40x40 arrow
     buttons to cover the whole 120x120 box with no dead zone.
     #virtual-controller above has pointer-events:none, so without this the
     enlarged areas outside the actual button elements couldn't receive
     pointer/touch events at all. */
  pointer-events: auto;
  touch-action: none;
}

#virtual-controller .vc-dpad .vc-btn {
  position: absolute;
  width: 40px;
  height: 40px;
  border-radius: 6px;
  /* The hit-area divs (.vc-hit) sit underneath and do the actual pointer
     handling for the d-pad, so the arrow buttons don't need to catch
     events themselves — pointer-events:none excludes them from
     hit-testing entirely (elementFromPoint sees straight through them),
     so the higher z-index here is purely so they still *paint* on top
     and look unchanged, rather than getting visually covered by the
     .vc-hit debug overlays. */
  pointer-events: none;
  z-index: 3;
}

/* Real, positioned elements for each d-pad hit region — built this way
   (rather than computed in JS) so the boundaries can be seen directly in
   devtools, or visually by uncommenting the `background`/`border` below.
   Layout:
     - up/down: full width, top/bottom 40px band (original 40px height,
       extended 40px to either side)
     - left/right: middle 40px band, split into the original 40x40 square
       plus a 20x120 vertical bar reaching full height — together a
       "sideways T". Where a bar overlaps the up/down bands, up/down wins
       (higher z-index) since that's the arrow's own visual footprint;
       flip the z-index below to give the bar priority instead.
   All six are transparent to touch outside .vc-dpad's own 120x120 box. */
#virtual-controller .vc-hit {
  position: absolute;
  pointer-events: auto;
  box-sizing: border-box;
}

#virtual-controller .vc-hit-up {
  left: 0;
  top: 0;
  width: 120px;
  height: 40px;
  z-index: 2;
}

#virtual-controller .vc-hit-down {
  left: 0;
  bottom: 0;
  width: 120px;
  height: 40px;
  z-index: 2;
}

#virtual-controller .vc-hit-left-main {
  left: 0;
  top: 40px;
  width: 60px;
  height: 40px;
  z-index: 1;
}

#virtual-controller .vc-hit-left-bar {
  left: -20px;
  top: 0;
  width: 20px;
  height: 120px;
  z-index: 1;
}

#virtual-controller .vc-hit-right-main {
  right: 0;
  top: 40px;
  width: 60px;
  height: 40px;
  z-index: 1;
}

#virtual-controller .vc-hit-right-bar {
  left: 120px;
  top: 0;
  width: 20px;
  height: 120px;
  z-index: 1;
}

#virtual-controller .vc-up {
  left: 40px;
  top: 0;
}

#virtual-controller .vc-down {
  left: 40px;
  bottom: 0;
}

#virtual-controller .vc-left {
  left: 0;
  top: 40px;
}

#virtual-controller .vc-right {
  right: 0;
  top: 40px;
}

#virtual-controller .vc-center {
  display: flex;
  gap: 8px;
  align-items: center;
}

#virtual-controller .vc-select,
#virtual-controller .vc-start {
  width: 62px;
  height: 28px;
  border-radius: 14px;
  font-size: 9px;
}

#virtual-controller .vc-face {
  position: relative;
  width: 120px;
  height: 120px;
}

#virtual-controller .vc-face .vc-btn {
  position: absolute;
  width: 52px;
  height: 52px;
  border-radius: 50%;
  font-size: 18px;
}

#virtual-controller .vc-a {
  right: 0;
  top: 18px;
}

#virtual-controller .vc-b {
  left: 0;
  bottom: 18px;
}
"#;

const VIRTUAL_HTML: &str =
    r#"
<div id="virtual-controller">
  <div class="vc-shoulders">
    <button class="vc-btn vc-l" data-button="l">L</button>
    <div class="vc-center">
      <button class="vc-btn vc-select" data-button="select">SELECT</button>
      <button class="vc-btn vc-start" data-button="start">START</button>
    </div>
    <button class="vc-btn vc-r" data-button="r">R</button>
  </div>

  <div class="vc-main">
    <div class="vc-dpad">
      <button class="vc-btn vc-up" data-button="up">▲</button>
      <button class="vc-btn vc-left" data-button="left">◀</button>
      <button class="vc-btn vc-right" data-button="right">▶</button>
      <button class="vc-btn vc-down" data-button="down">▼</button>

      <div class="vc-hit vc-hit-up" data-hit="up"></div>
      <div class="vc-hit vc-hit-down" data-hit="down"></div>
      <div class="vc-hit vc-hit-left-main" data-hit="left"></div>
      <div class="vc-hit vc-hit-left-bar" data-hit="left"></div>
      <div class="vc-hit vc-hit-right-main" data-hit="right"></div>
      <div class="vc-hit vc-hit-right-bar" data-hit="right"></div>
    </div>

    <div class="vc-face">
      <button class="vc-btn vc-b" data-button="b">B</button>
      <button class="vc-btn vc-a" data-button="a">A</button>
    </div>
  </div>
</div>
"#;

const VIRTUAL_JS: &str =
    r#"
const SLOT={slot};

let ws=null;
let prevBuf=null;

const buttonIndices={
  a:      0,
  b:      1,
  l:      4,
  r:      5,
  select: 8,
  start:  9,
  up:     12,
  down:   13,
  left:   14,
  right:  15
};

function connectVirtualController(){
  if(ws&&(ws.readyState===0||ws.readyState===1)) return;

  try{
    ws=new WebSocket('ws://'+location.host+'/ws/'+SLOT);
  }catch(e){
    ws=null;
    return;
  }

  ws.onopen=()=>{
    prevBuf=null;
    sendState();
  };

  ws.onclose=()=>{
    ws=null;
    prevBuf=null;
  };

  ws.onerror=()=>{
    try{
      if(ws) ws.close();
    }catch(e){}
  };
}

function bufEqual(a,b){
  if(!a||!b||a.length!==b.length) return false;
  for(let i=0;i<a.length;i++){
    if(a[i]!==b[i]) return false;
  }
  return true;
}

function sendState(){
  if(!ws||ws.readyState!==1) return;

  const buf=new Uint8Array(32);

  // Axes remain zero for the virtual controller.
  // Buttons occupy bytes 16..31, matching Web Gamepad API indices.
  
  if(!bufEqual(buf,prevBuf)){
    ws.send(buf);
    prevBuf=buf;
  }
}

function setButton(name,pressed){
  if(!ws||ws.readyState!==1) return;

  const index=buttonIndices[name];
  if(index===undefined) return;

  // Web Gamepad buttons begin at byte 16.
  const buf=prevBuf
    ? new Uint8Array(prevBuf)
    : new Uint8Array(32);

  buf[16+index]=pressed?1:0;

  if(!bufEqual(buf,prevBuf)){
    ws.send(buf);
    prevBuf=buf;
  }
}

function bindButton(button){
  const name=button.dataset.button;
  if(!name) return;

  const press=e=>{
    e.preventDefault();

    if(!ws||ws.readyState!==1){
      connectVirtualController();
      return;
    }

    button.classList.add('pressed');
    setButton(name,true);
  };

  const release=e=>{
    e.preventDefault();

    button.classList.remove('pressed');

    if(ws&&ws.readyState===1){
      setButton(name,false);
    }
  };

  button.addEventListener('pointerdown',press);
  button.addEventListener('pointerup',release);
  button.addEventListener('pointercancel',release);
  button.addEventListener('pointerleave',release);

  button.addEventListener('contextmenu',e=>e.preventDefault());
}

// Grouped buttons (d-pad, A/B pair): a single delegated listener on the
// group container tracks one active pointer and hit-tests with
// elementFromPoint on every move. This is what makes press-and-drag
// between buttons in the group (e.g. tap left, slide to right, or tap B,
// slide to A) work. Plain mouseenter/mouseleave can't do this: touch
// drags never fire enter/leave on other elements (there's no hover
// concept for touch), and once a pointer goes down on an element the
// browser implicitly captures it there, so even mouse enter/leave on
// *other* buttons wouldn't reliably fire either. Explicit pointer
// capture + elementFromPoint hit-testing handles mouse and touch the
// same way.
function bindButtonGroup(group){
  let activePointerId=null;
  let current=null; // {name, visual}

  // Resolves the real DOM element under a point to a {name, visual} pair:
  // - if it's one of the d-pad's .vc-hit overlay divs, `name` comes from
  //   data-hit and `visual` is the matching arrow button (for .pressed
  //   styling) — this is what lets the hitboxes be actual, inspectable
  //   elements instead of hand-rolled coordinate math.
  // - otherwise (A/B pair) it's a plain [data-button] element, used as
  //   both the name source and the visual to style.
  function resolveAt(x,y){
    const el=document.elementFromPoint(x,y);
    if(!el) return null;

    const hitEl=el.closest && el.closest('[data-hit]');
    if(hitEl && group.contains(hitEl)){
      const name=hitEl.dataset.hit;
      const visual=group.querySelector('[data-button="'+name+'"]') || hitEl;
      return {name, visual};
    }

    const btnEl=el.closest && el.closest('[data-button]');
    if(btnEl && group.contains(btnEl)){
      return {name:btnEl.dataset.button, visual:btnEl};
    }

    return null;
  }

  function press(control){
    if(!control || (current && current.name===control.name)) return;
    release();
    current=control;
    control.visual.classList.add('pressed');
    setButton(control.name,true);
  }

  // Visually deselects whatever button in this group is currently held
  // and tells the host to release it — called whenever the pointer moves
  // off that button (onto another one, into a dead zone, or off the
  // group entirely) as well as on pointerup/cancel.
  function release(){
    if(!current) return;
    current.visual.classList.remove('pressed');
    setButton(current.name,false);
    current=null;
  }

  group.addEventListener('pointerdown',e=>{
    e.preventDefault();
    const control=resolveAt(e.clientX,e.clientY);
    if(!control) return;

    activePointerId=e.pointerId;
    try{ group.setPointerCapture(e.pointerId); }catch(err){}

    if(!ws||ws.readyState!==1){
      connectVirtualController();
    }
    press(control);
  });

  group.addEventListener('pointermove',e=>{
    if(e.pointerId!==activePointerId) return;
    const control=resolveAt(e.clientX,e.clientY);
    if(control){
      press(control);
    }else{
      // Pointer is still down but has slid off every hit region in the
      // group — release so nothing stays stuck on, and so it visually
      // deselects.
      release();
    }
  });

  function endPointer(e){
    if(e.pointerId!==activePointerId) return;
    release();
    try{ group.releasePointerCapture(e.pointerId); }catch(err){}
    activePointerId=null;
  }

  group.addEventListener('pointerup',endPointer);
  group.addEventListener('pointercancel',endPointer);
  group.addEventListener('pointerleave',e=>{
    // Only fires if capture wasn't established (e.g. capture unsupported);
    // with setPointerCapture in place, drags outside the container's
    // bounding box still deliver move/up events here as normal.
    if(e.pointerId!==activePointerId) return;
    endPointer(e);
  });

  group.addEventListener('contextmenu',e=>e.preventDefault());
}

const dpadEl=document.querySelector('#virtual-controller .vc-dpad');
const faceEl=document.querySelector('#virtual-controller .vc-face'); // A/B pair
const groupedContainers=[dpadEl,faceEl];

document
  .querySelectorAll('#virtual-controller [data-button]')
  .forEach(button=>{
    if(groupedContainers.some(g=>g && g.contains(button))) return; // handled by bindButtonGroup instead
    bindButton(button);
  });

if(dpadEl) bindButtonGroup(dpadEl); // enlarged, gap-free hitboxes via .vc-hit divs
if(faceEl) bindButtonGroup(faceEl); // A/B: hit-tests the real button shapes directly

connectVirtualController();
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
    let (css, html_frag, js) = if rc == 1 {
        (GAMEPAD_CSS, GAMEPAD_HTML, GAMEPAD_JS)
    } else if rc == 2 {
        (VIRTUAL_CSS, VIRTUAL_HTML, VIRTUAL_JS)
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