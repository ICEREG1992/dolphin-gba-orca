# Future Plan: Direct Phantom Scancodes for Controller over Stream

## Context
Currently `Controller over Stream` uses real keyboard keys (arrows, WASD, numpad) via `SendInput` with `KEYEVENTF_SCANCODE`. This works but means the host PC keyboard can accidentally trigger GBA inputs if the user types while Dolphin is focused.

## Goal
Switch to **phantom scancodes** — scancode values that do not exist on any physical keyboard. This makes the input invisible to normal typing and removes the need for users to manually map specific keyboard keys.

## Implementation Plan

### 1. `src-tauri/src/input.rs`

**Remove:**
- `SLOT_VK` array (Virtual Key codes)
- `MapVirtualKeyW` call (VK → scancode conversion)
- `is_extended_vk` helper

**Add:**
```rust
const SLOT_SCANCODE: [[u16; GBA_KEYS]; 5] = [
    [0; GBA_KEYS],
    // Slot 1: phantom scancodes 0x50..0x59
    [0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59],
    // Slot 2: phantom scancodes 0x60..0x69
    [0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69],
    // Slot 3: phantom scancodes 0x70..0x79
    [0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79],
    // Slot 4: phantom scancodes 0x80..0x89
    [0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89],
];
```

**Modify `send_diff`:**
- `wVk = VIRTUAL_KEY(0)` (ignored with `KEYEVENTF_SCANCODE`)
- `wScan = table[i]` directly from `SLOT_SCANCODE`
- No `KEYEVENTF_EXTENDEDKEY` flag needed

### 2. User Workflow
The binding procedure remains the same as today:
1. In Dolphin, go to **Controllers → GBA (Integrated)** and pick the slot.
2. Set device to **Keyboard**.
3. Click each GBA button field and press the corresponding button on the remote controller.
4. Dolphin registers the phantom scancode via DirectInput.

After this, the host can type normally without ever triggering GBA inputs, because the phantom scancodes do not exist on physical keyboards.

### 3. README Update
Remove the large per-slot key table (arrows/WASD/numpad). Keep only the simplified binding instructions above.

## Notes
- These scancodes (0x50-0x89) are in the standard range (no E0 prefix) and are not mapped to any common keyboard keys.
- DirectInput `GetDeviceState` (used by Dolphin) reads raw scancodes, so it will see these values without issues.
- This change is **Windows-only** since Linux input injection is still a stub.
