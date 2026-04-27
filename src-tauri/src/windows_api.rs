use serde::Serialize;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
    SetWindowPos, ShowWindow,
    HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE,
};

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

fn to_hwnd(v: isize) -> HWND { HWND(v as *mut _) }

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<WindowInfo>);
    if !IsWindowVisible(hwnd).as_bool() { return TRUE; }
    let len = GetWindowTextLengthW(hwnd);
    if len == 0 { return TRUE; }
    let mut buf = vec![0u16; (len + 1) as usize];
    let read = GetWindowTextW(hwnd, &mut buf);
    if read == 0 { return TRUE; }
    let title = String::from_utf16_lossy(&buf[..read as usize]);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    out.push(WindowInfo {
        hwnd: hwnd.0 as isize,
        gba_slot: crate::detect_gba_slot(&title),
        title,
        pid,
        x: rect.left,
        y: rect.top,
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
    });
    TRUE
}

#[tauri::command]
pub fn list_windows() -> Vec<WindowInfo> {
    let mut windows: Vec<WindowInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut windows as *mut _ as isize));
    }
    windows
}

#[tauri::command]
pub fn list_gba_windows() -> Vec<WindowInfo> {
    list_windows().into_iter().filter(|w| w.gba_slot.is_some()).collect()
}

pub fn is_window_alive(hwnd_val: isize) -> bool {
    unsafe { IsWindow(to_hwnd(hwnd_val)).as_bool() }
}

pub fn is_window_minimized(hwnd_val: isize) -> bool {
    unsafe { IsIconic(to_hwnd(hwnd_val)).as_bool() }
}

/// Restore a minimized window without stealing focus and send it to the bottom
/// of the z-order. Idempotent — does nothing if the window isn't minimized.
pub fn restore_window_silent(hwnd_val: isize) {
    let win = to_hwnd(hwnd_val);
    unsafe {
        if IsIconic(win).as_bool() {
            let _ = ShowWindow(win, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                win, HWND_BOTTOM,
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}
