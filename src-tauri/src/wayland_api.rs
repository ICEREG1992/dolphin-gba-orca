//! Wayland capture path. Uses xdg-desktop-portal ScreenCast to obtain user
//! consent and a PipeWire node id per selected window. Unlike the X11/Win32
//! paths we cannot enumerate windows ourselves — Wayland strictly requires
//! the portal flow, so the user picks the GBA windows in the system dialog
//! and then assigns each captured source to a slot inside the app.
//!
//! Streaming reuses the FFmpeg+MediaMTX pipeline; the only difference is the
//! capture-input args (`-f libpipewire -i <node_id>`).
//!
//! Requires a recent FFmpeg built with libpipewire support.
use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex};

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use ashpd::enumflags2::BitFlags;
use serde::Serialize;
use tauri::State;

/// Detect a Wayland session. True when WAYLAND_DISPLAY is set or
/// XDG_SESSION_TYPE indicates wayland — covers GNOME/KDE/sway logins.
pub fn is_wayland_session() -> bool {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return true;
    }
    std::env::var("XDG_SESSION_TYPE")
        .map(|v| v.eq_ignore_ascii_case("wayland"))
        .unwrap_or(false)
}

#[derive(Serialize, Clone, Debug)]
pub struct WaylandSource {
    pub id: u64,
    pub label: String,
    pub node_id: u32,
    pub gba_slot: Option<u8>,
}

#[derive(Default)]
struct Inner {
    sources: Vec<WaylandSource>,
    /// PipeWire FD opened on the portal session. Held for the process
    /// lifetime so the granted streams remain usable. Replaced on each
    /// `wayland_select_sources` call.
    _pipewire_fd: Option<OwnedFd>,
    next_id: u64,
}

/// Tauri-managed state with the captured PipeWire sources and their
/// slot assignments.
#[derive(Default, Clone)]
pub struct WaylandData(Arc<Mutex<Inner>>);

impl WaylandData {
    pub fn list(&self) -> Vec<WaylandSource> {
        self.0.lock().unwrap().sources.clone()
    }

    /// Look up a captured source by id, returning (node_id, label).
    pub fn lookup(&self, id: u64) -> Option<(u32, String)> {
        let g = self.0.lock().unwrap();
        g.sources
            .iter()
            .find(|s| s.id == id)
            .map(|s| (s.node_id, s.label.clone()))
    }
}

#[tauri::command]
pub fn wayland_list_sources(state: State<'_, WaylandData>) -> Vec<WaylandSource> {
    state.list()
}

/// Open the xdg-desktop-portal ScreenCast dialog so the user can pick the
/// GBA windows. Returns the captured sources. Replaces any previous
/// selection — closes the prior PipeWire FD on drop.
#[tauri::command]
pub async fn wayland_select_sources(
    state: State<'_, WaylandData>,
) -> Result<Vec<WaylandSource>, String> {
    let proxy = Screencast::new()
        .await
        .map_err(|e| format!("portal connect: {}", e))?;
    let session = proxy
        .create_session()
        .await
        .map_err(|e| format!("create session: {}", e))?;

    proxy
        .select_sources(
            &session,
            CursorMode::Hidden,
            BitFlags::from(SourceType::Window),
            true,
            None,
            PersistMode::DoNot,
        )
        .await
        .map_err(|e| format!("select sources: {}", e))?;

    let response = proxy
        .start(&session, None)
        .await
        .map_err(|e| format!("portal start: {}", e))?
        .response()
        .map_err(|e| format!("portal response: {}", e))?;

    let fd = proxy
        .open_pipe_wire_remote(&session)
        .await
        .map_err(|e| format!("open pipewire fd: {}", e))?;

    let mut g = state.0.lock().unwrap();
    g.sources.clear();
    let mut out = Vec::new();
    for s in response.streams() {
        g.next_id += 1;
        let id = g.next_id;
        let node_id = s.pipe_wire_node_id();
        let label = format!("Source #{}", node_id);
        let src = WaylandSource {
            id,
            label,
            node_id,
            gba_slot: None,
        };
        g.sources.push(src.clone());
        out.push(src);
    }
    g._pipewire_fd = Some(fd);
    eprintln!("[wayland] portal returned {} source(s)", out.len());
    Ok(out)
}

/// Assign a captured source to a GBA slot (or clear it). If another source
/// already holds that slot, it's cleared first. Returns the updated source
/// list so the frontend can refresh in one round-trip.
#[tauri::command]
pub fn wayland_assign_slot(
    state: State<'_, WaylandData>,
    source_id: u64,
    slot: Option<u8>,
) -> Result<Vec<WaylandSource>, String> {
    if let Some(s) = slot {
        if !(1..=4).contains(&s) {
            return Err(format!("Slot non valido: {}", s));
        }
    }
    let mut g = state.0.lock().unwrap();
    if let Some(s) = slot {
        for src in g.sources.iter_mut() {
            if src.gba_slot == Some(s) {
                src.gba_slot = None;
            }
        }
    }
    let src = g
        .sources
        .iter_mut()
        .find(|x| x.id == source_id)
        .ok_or_else(|| format!("Source {} non trovato", source_id))?;
    src.gba_slot = slot;
    Ok(g.sources.clone())
}
