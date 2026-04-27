use serde::Serialize;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask, StackMode, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::x11_utils::Serialize as _;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        _NET_CLIENT_LIST,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_STATE,
        _NET_WM_STATE_HIDDEN,
        WM_STATE,
        UTF8_STRING,
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    pub pid: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub gba_slot: Option<u8>,
}

fn detect_gba_slot(title: &str) -> Option<u8> {
    const SLOTS: [(&str, u8); 4] = [("GBA1", 1), ("GBA2", 2), ("GBA3", 3), ("GBA4", 4)];
    SLOTS.iter().find(|(s, _)| title.contains(s)).map(|(_, n)| *n)
}

/// Open a fresh X11 connection. Returns None when no DISPLAY is set or the
/// X server can't be reached — on a pure Wayland session callers then see
/// an empty window list and the helpers no-op.
fn open() -> Option<(RustConnection, usize, Atoms)> {
    let (conn, screen_num) = x11rb::connect(None)
        .map_err(|e| eprintln!("[x11] connect failed: {}", e))
        .ok()?;
    let cookie = Atoms::new(&conn)
        .map_err(|e| eprintln!("[x11] atom request failed: {}", e))
        .ok()?;
    let atoms = cookie
        .reply()
        .map_err(|e| eprintln!("[x11] atom reply failed: {}", e))
        .ok()?;
    Some((conn, screen_num, atoms))
}

fn list_window_ids(conn: &RustConnection, root: Window, atoms: &Atoms) -> Vec<Window> {
    conn.get_property(false, root, atoms._NET_CLIENT_LIST, AtomEnum::WINDOW, 0, 4096)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|it| it.collect()))
        .unwrap_or_default()
}

fn get_title(conn: &RustConnection, win: Window, atoms: &Atoms) -> Option<String> {
    if let Some(reply) = conn
        .get_property(false, win, atoms._NET_WM_NAME, atoms.UTF8_STRING, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
    {
        if !reply.value.is_empty() {
            return Some(String::from_utf8_lossy(&reply.value).into_owned());
        }
    }
    if let Some(reply) = conn
        .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
    {
        if !reply.value.is_empty() {
            return Some(String::from_utf8_lossy(&reply.value).into_owned());
        }
    }
    None
}

fn get_pid(conn: &RustConnection, win: Window, atoms: &Atoms) -> u32 {
    conn.get_property(false, win, atoms._NET_WM_PID, AtomEnum::CARDINAL, 0, 1)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().and_then(|mut it| it.next()))
        .unwrap_or(0)
}

fn get_geometry(conn: &RustConnection, win: Window, root: Window) -> Option<(i32, i32, i32, i32)> {
    let geom = conn.get_geometry(win).ok()?.reply().ok()?;
    let coords = conn.translate_coordinates(win, root, 0, 0).ok()?.reply().ok()?;
    Some((coords.dst_x as i32, coords.dst_y as i32, geom.width as i32, geom.height as i32))
}

#[tauri::command]
pub fn list_windows() -> Vec<WindowInfo> {
    let Some((conn, screen_num, atoms)) = open() else { return Vec::new(); };
    let root = conn.setup().roots[screen_num].root;
    list_window_ids(&conn, root, &atoms)
        .into_iter()
        .map(|xid| {
            let title = get_title(&conn, xid, &atoms).unwrap_or_default();
            let (x, y, width, height) = get_geometry(&conn, xid, root).unwrap_or((0, 0, 0, 0));
            WindowInfo {
                hwnd: xid as isize,
                gba_slot: detect_gba_slot(&title),
                pid: get_pid(&conn, xid, &atoms),
                title,
                x,
                y,
                width,
                height,
            }
        })
        .collect()
}

#[tauri::command]
pub fn list_gba_windows() -> Vec<WindowInfo> {
    list_windows().into_iter().filter(|w| w.gba_slot.is_some()).collect()
}

pub fn is_window_alive(hwnd: isize) -> bool {
    let Some((conn, _, _)) = open() else { return false; };
    conn.get_window_attributes(hwnd as Window)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some()
}

pub fn is_window_minimized(hwnd: isize) -> bool {
    let Some((conn, _, atoms)) = open() else { return false; };
    let win = hwnd as Window;

    // ICCCM: WM_STATE.state == 3 (IconicState).
    let iconic = conn
        .get_property(false, win, atoms.WM_STATE, atoms.WM_STATE, 0, 2)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().and_then(|mut it| it.next()))
        .map(|s| s == 3)
        .unwrap_or(false);
    if iconic {
        return true;
    }

    // EWMH: _NET_WM_STATE contains _NET_WM_STATE_HIDDEN. Some WMs set this
    // without flipping WM_STATE to Iconic, so we OR the two checks.
    conn.get_property(false, win, atoms._NET_WM_STATE, AtomEnum::ATOM, 0, 64)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|it| it.collect::<Vec<_>>()))
        .map(|states| states.into_iter().any(|a| a == atoms._NET_WM_STATE_HIDDEN))
        .unwrap_or(false)
}

/// Un-iconify a window without focus theft (the EWMH analog of Win32's
/// `SW_SHOWNOACTIVATE`). Sends `_NET_WM_STATE` with action REMOVE for
/// `_NET_WM_STATE_HIDDEN` to the root, then pushes the window to the
/// bottom of the stack to mirror `HWND_BOTTOM`. WMs ignore REMOVE on
/// an absent state, so this is idempotent.
pub fn restore_window_silent(hwnd: isize) {
    let Some((conn, screen_num, atoms)) = open() else { return; };
    let root = conn.setup().roots[screen_num].root;
    let win = hwnd as Window;

    const NET_WM_STATE_REMOVE: u32 = 0;
    const SOURCE_INDICATION_APPLICATION: u32 = 1;

    let event = ClientMessageEvent::new(
        32,
        win,
        atoms._NET_WM_STATE,
        [
            NET_WM_STATE_REMOVE,
            atoms._NET_WM_STATE_HIDDEN,
            0,
            SOURCE_INDICATION_APPLICATION,
            0,
        ],
    );

    let _ = conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        event.serialize(),
    );

    let _ = conn.configure_window(
        win,
        &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
    );

    let _ = conn.flush();
}
