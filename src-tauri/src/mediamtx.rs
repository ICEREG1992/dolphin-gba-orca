use std::time::Duration;

use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

use crate::error::{AppError, AppResult};
use crate::network::MEDIAMTX_RTMP_PORT;
use crate::SharedState;

#[derive(Default)]
pub struct MediamtxState {
    pub child: Option<CommandChild>,
    pub event_task: Option<tauri::async_runtime::JoinHandle<()>>,
}

impl MediamtxState {
    /// Kill the child and abort the event task. Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = child.kill();
        }
        if let Some(task) = self.event_task.take() {
            task.abort();
        }
    }
}

/// Start MediaMTX as a sidecar if not already running. Idempotent and safe
/// against concurrent callers — the `mediamtx_starter` async mutex
/// serializes the spawn flow so port 1935 isn't double-bound.
pub async fn ensure(app: &tauri::AppHandle, state: &SharedState) -> AppResult<()> {
    let _starter = state.mediamtx_starter.lock().await;

    if state.mediamtx.lock().unwrap().child.is_some() {
        return Ok(());
    }

    let exe_dir = std::env::current_exe()?
        .parent()
        .ok_or(AppError::MediaMtx("no parent dir for current_exe".into()))?
        .to_path_buf();

    let candidates = [
        exe_dir.join("../../mediamtx.yml"),
        exe_dir.join("mediamtx.yml"),
    ];
    let config_path = candidates
        .iter()
        .find(|p| p.exists())
        .ok_or_else(|| AppError::MediaMtx(
            format!("mediamtx.yml non trovato (cercato in {:?})", candidates)))?
        .clone();

    tracing::info!("[mediamtx] config: {}", config_path.display());

    let sidecar = app.shell().sidecar("mediamtx")
        .map_err(|e| AppError::MediaMtx(format!("sidecar non trovato: {}", e)))?;
    let config_arg = config_path.to_string_lossy().into_owned();

    let (mut rx, child) = sidecar.args([config_arg.as_str()]).spawn()
        .map_err(|e| AppError::MediaMtx(format!("spawn fallito: {}", e)))?;

    tracing::info!("[mediamtx] avviato come sidecar");

    let state_for_event = state.clone();
    let event_task = tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stderr(line) | CommandEvent::Stdout(line) => {
                    tracing::info!("[mediamtx] {}", String::from_utf8_lossy(&line));
                }
                CommandEvent::Terminated(payload) => {
                    tracing::info!("[mediamtx] terminato: code={:?}", payload.code);
                    // Clear the child so future ensure() calls will respawn,
                    // and tear down all dependent WebRTC streams. We hold
                    // mediamtx then sessions — the only place both locks are
                    // held simultaneously, so lock order is consistent.
                    let mut mtx = state_for_event.mediamtx.lock().unwrap();
                    mtx.child = None;
                    crate::stream::shutdown_all_webrtc(&state_for_event);
                    drop(mtx);
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

    // Wait until MediaMTX accepts connections on the RTMP port.
    for _ in 0..20 {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{}", MEDIAMTX_RTMP_PORT)).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    state.mediamtx.lock().unwrap().shutdown();
    Err(AppError::MediaMtx(
        "non si è avviato in tempo (porta RTMP non raggiungibile)".into(),
    ))
}

/// Force-stop MediaMTX. Called by stream cleanup when no WebRTC streams remain.
/// Idempotent.
pub fn stop(state: &SharedState) {
    tracing::info!("[mediamtx] kill sidecar");
    state.mediamtx.lock().unwrap().shutdown();
}
