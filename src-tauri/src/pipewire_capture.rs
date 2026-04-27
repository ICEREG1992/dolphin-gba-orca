//! Minimal PipeWire helper for Wayland. Probes the negotiated video format
//! and pumps raw frames to a channel. All heavy lifting (colour conversion,
//! scaling, encoding) is left to FFmpeg.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Condvar, Mutex, Once};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pipewire as pw;
use pw::properties::properties;
use pw::spa;

/// `pw::init()` is idempotent per docs but we still gate it behind a `Once`
/// so the cost (and any future side-effects) only happen once per process.
fn pw_init_once() {
    static INIT: Once = Once::new();
    INIT.call_once(|| pw::init());
}

#[derive(Debug, Clone)]
pub struct VideoFormatInfo {
    pub pix_fmt: &'static str,
    pub width: u32,
    pub height: u32,
}

fn spa_fmt_to_ffmpeg(fmt: spa::param::video::VideoFormat) -> Option<&'static str> {
    use spa::param::video::VideoFormat as F;
    Some(match fmt {
        F::BGRx => "bgr0",
        F::RGBx => "rgb0",
        F::BGRA => "bgra",
        F::RGBA => "rgba",
        _ => return None,
    })
}

fn build_format_params() -> Vec<u8> {
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1280, height: 720 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 144, denom: 1 }
        ),
    );

    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("SPA pod serialize")
    .0
    .into_inner()
}

fn parse_video_format(param: &spa::pod::Pod) -> Result<VideoFormatInfo, String> {
    let (media_type, media_subtype) = spa::param::format_utils::parse_format(param)
        .map_err(|e| format!("format parse: {:?}", e))?;

    if media_type != spa::param::format::MediaType::Video
        || media_subtype != spa::param::format::MediaSubtype::Raw
    {
        return Err("unexpected media type".into());
    }

    let mut info = spa::param::video::VideoInfoRaw::new();
    info.parse(param)
        .map_err(|e| format!("VideoInfoRaw parse: {:?}", e))?;

    let pix_fmt = spa_fmt_to_ffmpeg(info.format())
        .ok_or_else(|| format!("unsupported pixel format {:?}", info.format()))?;

    Ok(VideoFormatInfo {
        pix_fmt,
        width: info.size().width,
        height: info.size().height,
    })
}

/// Open a short-lived PipeWire stream, read the negotiated format, then stop.
/// Consumes the fd.
pub fn probe_format(fd: OwnedFd, node_id: u32) -> Result<VideoFormatInfo, String> {
    pw_init_once();

    let mainloop = pw::main_loop::MainLoopBox::new(None)
        .map_err(|e| format!("mainloop: {}", e))?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)
        .map_err(|e| format!("context: {}", e))?;
    let core = context.connect_fd(fd, None)
        .map_err(|e| format!("connect_fd: {}", e))?;

    let result: Arc<Mutex<Option<Result<VideoFormatInfo, String>>>> = Arc::new(Mutex::new(None));
    let res_clone = result.clone();
    let ml_ptr = MainLoopPtr(mainloop.as_raw_ptr() as usize);

    let stream = pw::stream::StreamBox::new(
        &core,
        "gba-orca-probe",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("stream: {}", e))?;

    // Listener must outlive `mainloop.run()` to keep callbacks active. The
    // leading `_` only suppresses the unused-warning; the binding is load-bearing.
    let _keep_listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(|_, _, _old, _new| {
            tracing::debug!("[pw-probe] state changed");
        })
        .param_changed(move |_, _, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            let mut g = res_clone.lock().unwrap();
            if g.is_none() {
                tracing::info!("[pw-probe] format param received");
                *g = Some(parse_video_format(param));
                unsafe { pw::sys::pw_main_loop_quit(ml_ptr.0 as *mut pw::sys::pw_main_loop) };
            }
        })
        .register();

    let param_bytes = build_format_params();
    let param_pod = spa::pod::Pod::from_bytes(&param_bytes).unwrap();
    let mut params = [param_pod];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("connect: {}", e))?;

    // Force sync with the server so the param request is actually sent.
    core.sync(0).map_err(|e| format!("core sync: {}", e))?;

    // Safety timeout so we never hang forever.
    let res_timeout = result.clone();
    let ml_ptr2 = MainLoopPtr(mainloop.as_raw_ptr() as usize);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(10));
        let mut g = res_timeout.lock().unwrap();
        if g.is_none() {
            tracing::warn!("[pw-probe] timeout waiting for format");
            *g = Some(Err("probe timeout".into()));
            unsafe { pw::sys::pw_main_loop_quit(ml_ptr2.0 as *mut pw::sys::pw_main_loop) };
        }
    });

    mainloop.run();

    let g = result.lock().unwrap();
    g.clone()
        .unwrap_or_else(|| Err("probe failed without error".into()))
}

// ---------------------------------------------------------------------------
// Raw frame pump
// ---------------------------------------------------------------------------

// Wrapper so the raw pointer can cross threads.
struct MainLoopPtr(usize);
unsafe impl Send for MainLoopPtr {}
unsafe impl Sync for MainLoopPtr {}

pub struct PipewirePumpHandle {
    main_loop_ptr: usize,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PipewirePumpHandle {
    pub fn stop(&mut self) {
        if self.main_loop_ptr != 0 {
            unsafe { pw::sys::pw_main_loop_quit(self.main_loop_ptr as *mut pw::sys::pw_main_loop) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct PumpUserData {
    alive: Arc<AtomicBool>,
    main_loop_ptr: usize,
    writer: Box<dyn FnMut(&[u8]) -> bool + Send>,
}

/// Start a thread that captures raw frames from PipeWire and pushes them
/// directly into the supplied writer closure. The closure returns `false` to
/// signal a broken pipe / write failure; the pump then quits the mainloop.
pub fn start_raw_pump(
    fd: OwnedFd,
    node_id: u32,
    writer: Box<dyn FnMut(&[u8]) -> bool + Send>,
) -> Result<PipewirePumpHandle, String> {
    // Pair lock+condvar so the pump thread can publish its mainloop pointer
    // and we can wait for it without polling. The pump signals on success
    // (Some) or on early failure (still None — we'll time out).
    let ptr_slot: Arc<(Mutex<Option<MainLoopPtr>>, Condvar)> =
        Arc::new((Mutex::new(None), Condvar::new()));
    let ptr_for_thread = ptr_slot.clone();

    let thread = std::thread::Builder::new()
        .name(format!("pw-pump-{}", node_id))
        .spawn(move || run_pump(fd, node_id, writer, ptr_for_thread))
        .map_err(|e| format!("thread spawn: {}", e))?;

    let main_loop_ptr = {
        let (lock, cvar) = &*ptr_slot;
        let guard = lock.lock().unwrap();
        let (guard, wait_res) = cvar
            .wait_timeout_while(guard, Duration::from_secs(2), |g| g.is_none())
            .unwrap();
        if wait_res.timed_out() {
            tracing::warn!("[pw] timed out waiting for mainloop pointer");
            0
        } else {
            guard.as_ref().map(|p| p.0).unwrap_or(0)
        }
    };

    Ok(PipewirePumpHandle {
        main_loop_ptr,
        thread: Some(thread),
    })
}

fn run_pump(
    fd: OwnedFd,
    node_id: u32,
    writer: Box<dyn FnMut(&[u8]) -> bool + Send>,
    ptr_slot: Arc<(Mutex<Option<MainLoopPtr>>, Condvar)>,
) {
    // Helper: wake the caller fast on early failure. Publishes a sentinel
    // pointer of 0 so the caller observes failure instead of waiting the
    // full 2s timeout. PipewirePumpHandle::stop already treats ptr==0 as
    // "nothing to quit".
    let signal_failure = |ptr_slot: &Arc<(Mutex<Option<MainLoopPtr>>, Condvar)>| {
        let (lock, cvar) = &**ptr_slot;
        let mut g = lock.lock().unwrap();
        if g.is_none() {
            *g = Some(MainLoopPtr(0));
        }
        cvar.notify_all();
    };

    pw_init_once();

    let mainloop = match pw::main_loop::MainLoopBox::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            tracing::error!("[pw] mainloop failed: {}", e);
            signal_failure(&ptr_slot);
            return;
        }
    };

    let context = match pw::context::ContextBox::new(mainloop.loop_(), None) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("[pw] context failed: {}", e);
            signal_failure(&ptr_slot);
            return;
        }
    };

    let core = match context.connect_fd(fd, None) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("[pw] connect_fd failed: {}", e);
            signal_failure(&ptr_slot);
            return;
        }
    };

    let stream = match pw::stream::StreamBox::new(
        &core,
        "gba-orca-pump",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("[pw] stream failed: {}", e);
            signal_failure(&ptr_slot);
            return;
        }
    };

    let main_loop_ptr = mainloop.as_raw_ptr() as usize;
    let alive = Arc::new(AtomicBool::new(true));
    let user_data = PumpUserData {
        alive: alive.clone(),
        main_loop_ptr,
        writer,
    };

    // Listener must outlive `mainloop.run()` to keep the process callback
    // alive. The leading `_` only suppresses the unused warning.
    let _keep_listener = stream
        .add_local_listener_with_user_data(user_data)
        .process(|stream, ud| {
            if !ud.alive.load(Ordering::Relaxed) {
                return;
            }
            if let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let chunk = data.chunk();
                let offset = chunk.offset() as usize;
                let size = chunk.size() as usize;
                if let Some(slice) = data.data() {
                    let end = (offset + size).min(slice.len());
                    let frame_data = &slice[offset..end];
                    if !(ud.writer)(frame_data) {
                        ud.alive.store(false, Ordering::Relaxed);
                        unsafe {
                            pw::sys::pw_main_loop_quit(ud.main_loop_ptr as *mut pw::sys::pw_main_loop);
                        }
                    }
                }
            }
        })
        .register();

    let param_bytes = build_format_params();
    let param_pod = spa::pod::Pod::from_bytes(&param_bytes).unwrap();
    let mut params = [param_pod];
    if let Err(e) = stream.connect(
        spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    ) {
        tracing::error!("[pw] connect failed: {}", e);
        signal_failure(&ptr_slot);
        return;
    }

    // Publish the mainloop pointer only after every PipeWire object is
    // successfully set up. This guarantees the caller's pointer is valid
    // for the full lifetime of `mainloop.run()`.
    {
        let (lock, cvar) = &*ptr_slot;
        let mut g = lock.lock().unwrap();
        *g = Some(MainLoopPtr(main_loop_ptr));
        cvar.notify_all();
    }

    tracing::info!("[pw] pump started for node {}", node_id);
    mainloop.run();
    tracing::info!("[pw] pump exited for node {}", node_id);
}
