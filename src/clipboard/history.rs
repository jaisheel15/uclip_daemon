use std::{collections::VecDeque, time::SystemTime};

use tracing::info;

use crate::config::MAX_HISTORY;
use crate::display::format_history;
use crate::mime::is_text_mime;

pub struct ClipboardEntry {
    pub id: u64,
    pub timestamp: SystemTime,
    pub mime_type: String,
    pub content: Vec<u8>,
}

impl ClipboardEntry {
    /// True for MIME types we can preview as text.
    pub fn is_text(&self) -> bool {
        is_text_mime(&self.mime_type)
    }

    /// Short one-line preview for history / log output.
    ///
    /// UI list views should call this (or a truncated variant) — text
    /// decoding (`from_utf8_lossy`) stays here, not in the UI layer.
    pub fn preview(&self) -> String {
        if self.is_text() {
            String::from_utf8_lossy(&self.content)
                .lines()
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            format!("<{} bytes of {}>", self.content.len(), self.mime_type)
        }
    }
}

pub struct ClipState {
    entries: VecDeque<ClipboardEntry>,
    next_id: u64,
}

impl ClipState {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 1,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ClipboardEntry> {
        self.entries.iter()
    }

    pub fn latest(&self) -> Option<&ClipboardEntry> {
        self.entries.back()
    }

    pub fn get(&self, id: u64) -> Option<&ClipboardEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Log the full history at `info` level (debug helper until a TUI/GUI
    /// renders `entries` directly).
    pub fn print_history(&self) {
        info!("{}", format_history(self));
    }

    /// Push a new entry unless empty or a consecutive duplicate.
    /// Returns the new entry's id if it was added.
    ///
    /// Dedup is intentionally shallow (consecutive, same mime+bytes only).
    /// A smarter UI may want cross-history content hashing — add there,
    /// keep this as the storage-layer guard.
    pub fn add_entry(&mut self, mime_type: String, content: Vec<u8>) -> Option<u64> {
        if content.is_empty() {
            return None;
        }
        if let Some(last) = self.entries.back()
            && last.content == content
            && last.mime_type == mime_type
        {
            return None;
        }

        let id = self.next_id;
        self.next_id += 1;

        self.entries.push_back(ClipboardEntry {
            id,
            timestamp: SystemTime::now(),
            mime_type,
            content,
        });

        while self.entries.len() > MAX_HISTORY {
            self.entries.pop_front();
        }
        Some(id)
    }
}

impl Default for ClipState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_content() {
        let mut state = ClipState::new();
        assert_eq!(state.add_entry("text/plain".into(), vec![]), None);
        assert!(state.is_empty());
    }

    #[test]
    fn rejects_consecutive_duplicates_but_allows_repeats_after_change() {
        let mut state = ClipState::new();
        let first = state.add_entry("text/plain".into(), b"hi".to_vec());
        assert!(first.is_some());
        assert_eq!(state.add_entry("text/plain".into(), b"hi".to_vec()), None);
        assert_eq!(state.len(), 1);

        state.add_entry("text/plain".into(), b"other".to_vec());
        // Same bytes as the first entry but no longer consecutive → stored.
        assert!(
            state
                .add_entry("text/plain".into(), b"hi".to_vec())
                .is_some()
        );
    }

    #[test]
    fn same_bytes_different_mime_are_distinct() {
        let mut state = ClipState::new();
        state.add_entry("text/plain".into(), b"hi".to_vec());
        assert!(
            state
                .add_entry("text/html".into(), b"hi".to_vec())
                .is_some()
        );
    }

    #[test]
    fn assigns_incrementing_ids_and_lookup() {
        let mut state = ClipState::new();
        let a = state.add_entry("text/plain".into(), b"a".to_vec()).unwrap();
        let b = state.add_entry("text/plain".into(), b"b".to_vec()).unwrap();
        assert_eq!(b, a + 1);
        assert_eq!(state.get(a).unwrap().content, b"a");
        assert_eq!(state.latest().unwrap().id, b);
    }

    #[test]
    fn preview_uses_first_text_line_or_byte_summary() {
        let mut state = ClipState::new();
        let text_id = state
            .add_entry("text/plain".into(), b"first\nsecond".to_vec())
            .unwrap();
        assert_eq!(state.get(text_id).unwrap().preview(), "first");

        let bin_id = state.add_entry("image/png".into(), vec![0u8; 8]).unwrap();
        assert_eq!(
            state.get(bin_id).unwrap().preview(),
            "<8 bytes of image/png>"
        );
    }
}
