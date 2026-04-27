//! Wayland capture path. Uses xdg-desktop-portal ScreenCast to obtain user
//! consent and a PipeWire node id per selected window. Unlike the X11/Win32
//! paths we cannot enumerate windows ourselves — Wayland strictly requires
//! the portal flow, so the user picks the GBA windows in the system dialog
//! and then assigns each captured source to a slot inside the app.
//!
//! Streaming reuses the FFmpeg+MediaMTX pipeline; the only difference is the
//! capture-input args (`-f libpipewire -i <node_id>`) **when** the bundled
//! FFmpeg is built with libpipewire support.  If it is not, the caller must
//! read raw frames via `pipewire_capture` and feed them to FFmpeg on stdin.
//!
//! To keep the portal grant alive for the whole streaming lifetime we store
//! the portal proxy + session inside `WaylandData` instead of dropping them
//! after `wayland_select_sources` returns.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::WindowIdentifier;
use ashpd::desktop::PersistMode;
use ashpd::enumflags2::BitFlags;
use serde::Serialize;
use tauri::State;

// We keep the proxy + session alive for the lifetime of the selection so the
// portal grant doesn't disappear.  They are boxed to avoid naming the exact
// private/generic types in our state struct.
type PortalBox = Box<dyn std::any::Any + Send + Sync>;

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
    /// Opaque handle that keeps the portal session (proxy + Session) alive
    /// for the lifetime of this selection. Boxed because the concrete types
    /// are private/generic; we only need it to outlive the FD.
    portal_handle: Option<PortalBox>,
    /// PipeWire FD opened on the portal session. Duplicated on demand via
    /// `dup_fd()` so each capture thread has its own connection.
    pipewire_fd: Option<OwnedFd>,
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

    /// Duplicate the PipeWire FD so the capture thread can open its own
    /// connection while the portal session (and its FD) stays alive here.
    /// Returns Err on dup failure (e.g. EMFILE) so the caller can surface
    /// a real error instead of crashing the whole app.
    pub fn dup_fd(&self) -> Result<Option<OwnedFd>, String> {
        let g = self.0.lock().unwrap();
        let Some(fd) = g.pipewire_fd.as_ref() else { return Ok(None) };
        let raw = fd.as_raw_fd();
        let new_raw = unsafe { libc::dup(raw) };
        if new_raw < 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("dup(pipewire_fd) failed: {}", err));
        }
        Ok(Some(unsafe { OwnedFd::from_raw_fd(new_raw) }))
    }
}

#[tauri::command]
pub fn wayland_list_sources(state: State<'_, WaylandData>) -> Vec<WaylandSource> {
    state.list()
}

/// Open the xdg-desktop-portal ScreenCast dialog so the user can pick the
/// GBA windows. Returns the captured sources. Replaces any previous
/// selection — dropping the prior portal session on the way out.
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
        .start(&session, &WindowIdentifier::None)
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
    // Portals often return streams in reverse selection order (LIFO).
    // Reverse so the first window the user clicked becomes GBA1.
    let streams: Vec<_> = response.streams().to_vec();
    for (idx, s) in streams.into_iter().rev().enumerate() {
        g.next_id += 1;
        let id = g.next_id;
        let node_id = s.pipe_wire_node_id();
        // Auto-assign slot 1..4 in selection order so the user doesn't have
        // to manually map each source.
        let auto_slot = if idx < 4 { Some((idx + 1) as u8) } else { None };
        let label = format!("Source #{} (GBA{})", node_id, idx + 1);
        let src = WaylandSource {
            id,
            label,
            node_id,
            gba_slot: auto_slot,
        };
        g.sources.push(src.clone());
        out.push(src);
    }
    // Box the proxy + session together so they stay alive (and the portal
    // grant remains valid) for the lifetime of this selection.
    g.portal_handle = Some(Box::new((proxy, session)) as PortalBox);
    g.pipewire_fd = Some(fd);
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
