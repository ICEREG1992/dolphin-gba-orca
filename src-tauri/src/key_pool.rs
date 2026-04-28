use std::collections::HashMap;

/// Global pool of 80 safe, real keyboard keys.
/// Each slot can request up to 20 keys from this pool.
/// Keys are assigned sequentially and never reused within a session.
pub struct KeyPool {
    /// All VK codes in the pool, ordered from most desirable (A-Z) to least.
    available: Vec<u16>,
    /// Next index to assign from `available`.
    next_idx: usize,
    /// Per-slot assignment tracking: slot -> ordered list of assigned VKs.
    /// The index in this Vec is the gba_idx (0..19).
    assigned: HashMap<u8, Vec<u16>>,
}

impl Default for KeyPool {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyPool {
    pub fn new() -> Self {
        let mut available = Vec::with_capacity(80);

        // 0-25: Letters A-Z
        for vk in 0x41..=0x5A {
            available.push(vk);
        }
        // 26-35: Numbers 0-9
        for vk in 0x30..=0x39 {
            available.push(vk);
        }
        // 36-46: Function keys F1-F10, F12
        for vk in 0x70..=0x79 {
            available.push(vk);
        }
        available.push(0x7B); // F12
        // 47-57: Numpad 0-9, decimal
        for vk in 0x60..=0x69 {
            available.push(vk);
        }
        available.push(0x6E); // Numpad .
        // 58-61: Arrow keys
        available.push(0x26); // Up
        available.push(0x28); // Down
        available.push(0x25); // Left
        available.push(0x27); // Right
        // 62-67: Navigation (Insert, Delete, Home, End, PgUp, PgDn)
        available.push(0x2D); // Insert
        available.push(0x2E); // Delete
        available.push(0x24); // Home
        available.push(0x23); // End
        available.push(0x21); // PgUp
        available.push(0x22); // PgDn
        // 68-71: Numpad operators (*, /, +, -)
        available.push(0x6A); // *
        available.push(0x6F); // /
        available.push(0x6B); // +
        available.push(0x6D); // -
        assert_eq!(available.len(), 72, "Pool must contain exactly 72 safe keys");

        Self {
            available,
            next_idx: 0,
            assigned: HashMap::new(),
        }
    }

    /// Request the next available key for a slot.
    /// Returns `(gba_idx, vk)` where `gba_idx` is the sequential index
    /// (0..17) for this slot. Returns `None` if the pool is exhausted
    /// or the slot already has 18 keys.
    pub fn request_key(&mut self, slot: u8) -> Option<(usize, u16)> {
        if !(1..=4).contains(&slot) {
            return None;
        }
        let slot_keys = self.assigned.entry(slot).or_insert_with(Vec::new);
        if slot_keys.len() >= 18 {
            return None;
        }
        if self.next_idx >= self.available.len() {
            return None;
        }
        let vk = self.available[self.next_idx];
        self.next_idx += 1;
        let gba_idx = slot_keys.len();
        slot_keys.push(vk);
        tracing::info!("[key_pool] slot {} assigned gba_idx={} vk=0x{:02X}", slot, gba_idx, vk);
        Some((gba_idx, vk))
    }

    /// Get the VK assigned to a specific gba_idx for a slot.
    pub fn get_vk(&self, slot: u8, gba_idx: usize) -> Option<u16> {
        self.assigned.get(&slot)?.get(gba_idx).copied()
    }

    /// Get all assigned VKs for a slot.
    pub fn slot_assigned(&self, slot: u8) -> Option<&Vec<u16>> {
        self.assigned.get(&slot)
    }
}
