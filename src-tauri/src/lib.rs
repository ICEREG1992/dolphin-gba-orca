use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tauri::Manager;

mod http;
mod mediamtx;
mod network;
mod stream;

#[cfg(windows)]
mod windows_api;
#[cfg(target_os = "linux")]
mod x11_api;
#[cfg(target_os = "linux")]
mod wayland_api;
#[cfg(target_os = "linux")]
mod pipewire_capture;

mod platform {
    #[cfg(windows)]
    pub use crate::windows_api::*;
    #[cfg(target_os = "linux")]
    pub use crate::x11_api::*;
}

use mediamtx::MediamtxState;
use stream::StreamSession;

#[derive(Clone, Default)]
pub(crate) struct SharedState {
    pub sessions: Arc<Mutex<HashMap<u8, StreamSession>>>,
    pub mediamtx: Arc<Mutex<MediamtxState>>,
}

/// Create a Windows Job Object with KILL_ON_JOB_CLOSE and assign the current
/// process to it. All child processes (FFmpeg, MediaMTX) automatically join,
/// so when this process exits — even forcefully — the kernel terminates them.
#[cfg(windows)]
fn setup_job_object() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicLimitInformation,
        SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
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

        // Wrap in a non-Copy newtype so the handle isn't dropped (which would
        // close it). It must stay open for the process lifetime.
        #[allow(dead_code)]
        struct JobHandle(HANDLE);
        std::mem::forget(JobHandle(job));
        eprintln!("[job] KILL_ON_JOB_CLOSE job object created");
    }
}

/// Disable WebKitGTK's DMA-BUF renderer (and compositing mode) before WebView
/// initialization. The DMA-BUF renderer breaks on many Linux GPU drivers,
/// causing a black/blank WebView; this is the workaround recommended upstream.
#[cfg(target_os = "linux")]
fn setup_linux_env() {
    unsafe {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
    }
}

/// Always-available command so the frontend can branch on session type
/// without per-platform compile-time gating in JS.
#[tauri::command]
fn is_wayland() -> bool {
    #[cfg(target_os = "linux")]
    {
        wayland_api::is_wayland_session()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    setup_job_object();
    #[cfg(target_os = "linux")]
    setup_linux_env();
    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .manage(SharedState::default());

    #[cfg(target_os = "linux")]
    let builder = builder.manage(wayland_api::WaylandData::default());

    builder
        .setup(|app| {
            let state = app.state::<SharedState>().inner().clone();
            tauri::async_runtime::spawn(async move {
                http::run_http_server(state).await;
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let state = window.state::<SharedState>().inner().clone();
                stream::shutdown_all(&state);
                state.mediamtx.lock().unwrap().shutdown();
            }
        })
        .invoke_handler(tauri::generate_handler![
            is_wayland,
            platform::list_windows,
            platform::list_gba_windows,
            stream::start_stream,
            stream::stop_stream,
            stream::list_streams,
            network::get_server_info,
            #[cfg(target_os = "linux")]
            wayland_api::wayland_list_sources,
            #[cfg(target_os = "linux")]
            wayland_api::wayland_select_sources,
            #[cfg(target_os = "linux")]
            wayland_api::wayland_assign_slot,
            #[cfg(target_os = "linux")]
            stream::start_wayland_stream,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
