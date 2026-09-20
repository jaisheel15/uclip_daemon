//! Shared tuning constants for the clipboard daemon.
//!
//! Centralising these avoids drift between the Wayland layer
//! (`pick_mimes`, pipe fan-out) and the storage layer (`ClipState`).

/// Maximum history entries retained in [`crate::clipboard::history::ClipState`].
pub const MAX_HISTORY: usize = 1000;

/// Maximum bytes kept per clipboard entry.
///
/// Reads use `take(MAX_CONTENT_BYTES + 1)` so oversized pastes are detected
/// without buffering unbounded input first.
pub const MAX_CONTENT_BYTES: usize = 1024 * 1024;

/// Preferred text MIME types, in preference order.
pub const PREFERRED_TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "text/markdown",
    "text/html",
    "text/uri-list",
    "TEXT",
    "STRING",
    "UTF8_STRING",
];

/// Binary MIME types we store (with a size summary instead of a text preview).
/// Anything else (e.g. `application/x-foo`) is ignored by [`crate::mime::pick_mimes`].
pub const SUPPORTED_BINARY_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Maximum MIME transfers requested per selection (bounds pipe usage).
///
/// One copy == up to N pipe round-trips + N history rows; raise with care
/// if the UI ever groups rows per copy.
pub const MAX_MIMES_PER_SELECTION: usize = 4;

/// Text aliases offered when restoring a text entry via [`crate::clipboard::state::AppState::restore_entry`].
///
/// The entry's own MIME type is always offered first; these aliases point at
/// the same bytes so legacy clients (`TEXT`, `STRING`, …) still paste.
pub const TEXT_ALIASES: &[&str] = &[
    "text/plain",
    "text/plain;charset=utf-8",
    "TEXT",
    "STRING",
    "UTF8_STRING",
];
