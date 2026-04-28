//! Gamepad input pipeline. Mobile clients send Web Gamepad API state over
//! WebSocket; we project that state onto the 10 GBA buttons (Up/Down/Left/
//! Right/A/B/L/R/Start/Select), then synthesize OS keyboard events using a
//! per-slot virtual-key table. Dolphin's main window must be in focus; the
//! user binds each slot's GBA controls in Dolphin to the keys this slot uses.
//!
//! Linux: stub — the WS endpoint accepts data but no keys are injected. A
//! future X11 XTest / libei path can drop into `send_diff` without changing
//! the protocol or call sites.

use serde::Deserialize;

pub const GBA_KEYS: usize = 10;

const KEY_UP: usize = 0;
const KEY_DOWN: usize = 1;
const KEY_LEFT: usize = 2;
const KEY_RIGHT: usize = 3;
const KEY_A: usize = 4;
const KEY_B: usize = 5;
const KEY_L: usize = 6;
const KEY_R: usize = 7;
const KEY_START: usize = 8;
const KEY_SELECT: usize = 9;

// Web Gamepad API "standard" mapping indices.
const GP_A: usize = 0;
const GP_B: usize = 1;
const GP_L1: usize = 4;
const GP_R1: usize = 5;
const GP_SELECT: usize = 8;
const GP_START: usize = 9;
const GP_DPAD_UP: usize = 12;
const GP_DPAD_DOWN: usize = 13;
const GP_DPAD_LEFT: usize = 14;
const GP_DPAD_RIGHT: usize = 15;

const STICK_DEADZONE: f32 = 0.5;

/// JSON payload from the browser. Matches the shape of `Gamepad` in the Web
/// Gamepad API: an array of stick axes (float -1..1) and an array of button
/// states (0/1). Both fields default to empty so a partial / non-standard
/// controller never panics the deserializer.
#[derive(Deserialize, Debug, Default, Clone)]
pub struct GamepadInput {
    #[serde(default)]
    pub axes: Vec<f32>,
    #[serde(default)]
    pub buttons: Vec<u8>,
}

impl GamepadInput {
    /// Parse compact binary payload: 4×f32 LE axes + 16×u8 buttons = 32 bytes.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 32 { return None; }
        let mut axes = Vec::with_capacity(4);
        for i in 0..4 {
            let b = &data[i * 4..i * 4 + 4];
            axes.push(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        }
        let buttons = data[16..32].to_vec();
        Some(Self { axes, buttons })
    }

    fn to_gba_keys(&self) -> [bool; GBA_KEYS] {
        let btn = |i: usize| self.buttons.get(i).copied().unwrap_or(0) != 0;
        let axis = |i: usize| self.axes.get(i).copied().unwrap_or(0.0);

        let lx = axis(0);
        let ly = axis(1);

        let mut k = [false; GBA_KEYS];
        k[KEY_A] = btn(GP_A);
        k[KEY_B] = btn(GP_B);
        k[KEY_L] = btn(GP_L1);
        k[KEY_R] = btn(GP_R1);
        k[KEY_SELECT] = btn(GP_SELECT);
        k[KEY_START] = btn(GP_START);
        k[KEY_UP] = btn(GP_DPAD_UP) || ly < -STICK_DEADZONE;
        k[KEY_DOWN] = btn(GP_DPAD_DOWN) || ly > STICK_DEADZONE;
        k[KEY_LEFT] = btn(GP_DPAD_LEFT) || lx < -STICK_DEADZONE;
        k[KEY_RIGHT] = btn(GP_DPAD_RIGHT) || lx > STICK_DEADZONE;
        k
    }
}

/// Virtual-key code per (slot, gba-button). Slot is 1-indexed; row 0 is unused.
/// Order in each row mirrors `KEY_UP`..`KEY_SELECT`.
#[cfg(windows)]
const SLOT_VK: [[u16; GBA_KEYS]; 5] = [
    [0; GBA_KEYS],
    // Slot 1: Arrows + Z X C V Enter Backspace
    [0x26, 0x28, 0x25, 0x27, 0x5A, 0x58, 0x43, 0x56, 0x0D, 0x08],
    // Slot 2: WASD + E Q R T F G
    [0x57, 0x53, 0x41, 0x44, 0x45, 0x51, 0x52, 0x54, 0x46, 0x47],
    // Slot 3: IJKL + P O U Y H N
    [0x49, 0x4B, 0x4A, 0x4C, 0x50, 0x4F, 0x55, 0x59, 0x48, 0x4E],
    // Slot 4: Numpad 8/5/4/6/1/2/3/0/9/.
    [0x68, 0x65, 0x64, 0x66, 0x61, 0x62, 0x63, 0x60, 0x69, 0x6E],
];

/// Tracks which GBA buttons are currently held down for one WS session.
/// Drop releases everything still pressed so a client disconnecting mid-press
/// can never leave a key stuck down on the host.
pub struct SlotKeyState {
    slot: u8,
    pressed: [bool; GBA_KEYS],
}

impl SlotKeyState {
    pub fn new(slot: u8) -> Self {
        Self { slot, pressed: [false; GBA_KEYS] }
    }

    pub fn apply(&mut self, input: &GamepadInput) {
        let target = input.to_gba_keys();
        if target == self.pressed { return; }
        send_diff(self.slot, &self.pressed, &target);
        self.pressed = target;
    }
}

impl Drop for SlotKeyState {
    fn drop(&mut self) {
        let target = [false; GBA_KEYS];
        if target != self.pressed {
            send_diff(self.slot, &self.pressed, &target);
        }
    }
}

/// VKs tra quelli usati nelle nostre tabelle che richiedono `KEYEVENTF_EXTENDEDKEY`:
/// solo le frecce dello slot 1. Numpad 0-9 e VK_DECIMAL non sono estesi; VK_RETURN
/// qui è l'Enter principale, non quello del numpad.
#[cfg(windows)]
fn is_extended_vk(vk: u16) -> bool {
    matches!(vk, 0x25 | 0x26 | 0x27 | 0x28)
}

#[cfg(windows)]
fn send_diff(slot: u8, prev: &[bool; GBA_KEYS], next: &[bool; GBA_KEYS]) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
        MAPVK_VK_TO_VSC, VIRTUAL_KEY,
    };

    if !(1..=4).contains(&slot) { return; }
    let table = &SLOT_VK[slot as usize];

    let mut inputs: Vec<INPUT> = Vec::with_capacity(GBA_KEYS);
    for i in 0..GBA_KEYS {
        if prev[i] == next[i] { continue; }
        let vk = table[i];
        // Scancode-based input: senza KEYEVENTF_SCANCODE, DirectInput
        // (`GetDeviceState`, indicizzato per scancode) non vede l'evento e
        // dialog come "Premi Ora" di Dolphin non lo registrano.
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16;
        let mut flags = KEYEVENTF_SCANCODE;
        if is_extended_vk(vk) { flags |= KEYEVENTF_EXTENDEDKEY; }
        if !next[i] { flags |= KEYEVENTF_KEYUP; }
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    // Con KEYEVENTF_SCANCODE wVk è ignorato; lo lasciamo per
                    // chiarezza/diagnostica, non disturba.
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
    if inputs.is_empty() { return; }

    let cb = std::mem::size_of::<INPUT>() as i32;
    let sent = unsafe { SendInput(&inputs, cb) };
    if sent as usize != inputs.len() {
        tracing::warn!(
            "[input slot {}] SendInput sent {}/{} events",
            slot, sent, inputs.len()
        );
    }
}

#[cfg(not(windows))]
fn send_diff(_slot: u8, _prev: &[bool; GBA_KEYS], _next: &[bool; GBA_KEYS]) {
    // Linux: not yet implemented. Endpoint stays available so the mobile UI
    // doesn't have to branch on platform; key injection is just a no-op.
}
