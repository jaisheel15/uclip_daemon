pub mod clipboard;
pub mod config;
pub mod display;
pub mod error;
pub mod mime;

pub use clipboard::history::{
    ClipState, ClipboardContent, ClipboardEntry, ImageData, MixedData, TextData,
};
pub use clipboard::io::{ReadOutcome, drain_pending_reads};
pub use clipboard::state::{AppState, OfferData, PendingRead, SourceData};
pub use config::{
    MAX_CONTENT_BYTES, MAX_HISTORY, MAX_MIMES_PER_SELECTION, PREFERRED_TEXT_MIMES,
    SUPPORTED_BINARY_MIMES, TEXT_ALIASES,
};
pub use error::{ClipboardError, Result};
pub use mime::{is_text_mime, pick_mimes};
