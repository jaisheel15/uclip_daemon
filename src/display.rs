//! Human-readable rendering for history entries.
//!
//! Keeps text decoding summaries and layout in one place so the
//! Wayland I/O layer and any future TUI/GUI share the same preview logic.

use crate::clipboard::history::{ClipState, ClipboardContent, ClipboardEntry};

/// One-line summary used in history listings.
pub fn format_entry(entry: &ClipboardEntry) -> String {
    format!(
        "[{}] {:?} {}: {}",
        entry.id,
        entry.timestamp,
        entry.content.primary_mime(),
        entry.preview()
    )
}

/// Human label for the entry variant (Text / Image / Mixed).
pub fn format_kind(entry: &ClipboardEntry) -> &'static str {
    match &entry.content {
        ClipboardContent::Text(_) => "Text",
        ClipboardContent::Image(_) => "Image",
        ClipboardContent::Mixed(_) => "Mixed",
    }
}

/// Multi-line detail view logged when a new clipboard entry lands.
pub fn format_new_entry(entry: &ClipboardEntry) -> String {
    match &entry.content {
        ClipboardContent::Text(t) => format!(
            "\n==== CLIPBOARD [id={} @{:?} {} Text] ====\n{}\n",
            entry.id, entry.timestamp, t.primary_mime, t.text
        ),
        ClipboardContent::Image(i) => format!(
            "\n==== CLIPBOARD [id={} @{:?}] ====\n<{} bytes of {}>\n",
            entry.id,
            entry.timestamp,
            i.bytes.len(),
            i.mime_type
        ),
        ClipboardContent::Mixed(m) => {
            let mut out = format!(
                "\n==== CLIPBOARD [id={} @{:?} Mixed] ====\n",
                entry.id, entry.timestamp
            );
            if let Some(t) = &m.text {
                out.push_str(&t.text);
                out.push('\n');
            }
            for (mime, len) in entry.content.image_summaries() {
                out.push_str(&format!("<{len} bytes of {mime}>\n"));
            }
            out
        }
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
