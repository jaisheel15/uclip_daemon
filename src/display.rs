//! Human-readable rendering for history entries.
//!
//! Keeps text decoding (`from_utf8_lossy`) and layout in one place so the
//! Wayland I/O layer and any future TUI/GUI share the same preview logic.

use crate::clipboard::history::{ClipState, ClipboardEntry};

/// One-line summary used in history listings.
pub fn format_entry(entry: &ClipboardEntry) -> String {
    format!(
        "[{}] {:?} {}: {}",
        entry.id,
        entry.timestamp,
        entry.mime_type,
        entry.preview()
    )
}

/// Multi-line detail view logged when a new clipboard entry lands.
pub fn format_new_entry(entry: &ClipboardEntry) -> String {
    if entry.is_text() {
        let text = String::from_utf8_lossy(&entry.content);
        format!(
            "\n==== CLIPBOARD [id={} @{:?} {}] ====\n{}\n",
            entry.id, entry.timestamp, entry.mime_type, text
        )
    } else {
        format!(
            "\n==== CLIPBOARD [id={} @{:?}] ====\n<{} bytes of {}>\n",
            entry.id,
            entry.timestamp,
            entry.content.len(),
            entry.mime_type
        )
    }
}

/// Full history dump (debug helper until a TUI/GUI renders `entries` directly).
pub fn format_history(state: &ClipState) -> String {
    let mut out = format!("History ({} entries):", state.len());
    for entry in state.iter() {
        out.push('\n');
        out.push_str(&format_entry(entry));
    }
    out
}
