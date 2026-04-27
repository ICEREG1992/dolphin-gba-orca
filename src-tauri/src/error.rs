//! Application-wide error type. All Tauri commands return `Result<_, AppError>`
//! so the frontend gets a consistent, serializable error shape. Internal
//! helpers can still return `Result<_, String>`; the `From<String>`
//! conversion below makes `?` propagate them transparently.
//!
//! Display strings are in Italian to match the existing user-facing copy.

use serde::ser::Serializer;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    Msg(String),

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("Slot non valido: {0}")]
    InvalidSlot(u8),

    #[error("Slot {0} già in stream")]
    SlotInUse(u8),

    #[error("Nessuno stream attivo per slot {0}")]
    NoStream(u8),

    #[error("FFmpeg: {0}")]
    Ffmpeg(String),

    #[error("MediaMTX: {0}")]
    MediaMtx(String),

    #[error("Portal Wayland: {0}")]
    Portal(String),

    #[error("PipeWire: {0}")]
    Pipewire(String),
}

impl From<String> for AppError {
    fn from(s: String) -> Self { AppError::Msg(s) }
}

impl From<&str> for AppError {
    fn from(s: &str) -> Self { AppError::Msg(s.to_string()) }
}

// Tauri serializes command errors as JSON; flatten to a string so the
// frontend can keep doing `String(e)` without parsing a tagged enum.
impl serde::Serialize for AppError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
