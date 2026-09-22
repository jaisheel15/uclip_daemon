use std::{
    collections::{HashMap, VecDeque},
    time::SystemTime,
};

use tracing::info;

use crate::config::{MAX_HISTORY, PREFERRED_TEXT_MIMES};
use crate::display::format_history;
use crate::mime::is_text_mime;

/// Decoded text copy with its raw per-MIME blobs preserved for restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextData {
    /// Best decoded text (from `primary_mime`).
    pub text: String,
    /// Decoded `text/html` payload when the copy offered distinct HTML.
    pub html: Option<String>,
    /// MIME the preview / restore prefers (first `PREFERRED_TEXT_MIMES` hit).
    pub primary_mime: String,
    /// All text MIMEs seen in this copy, in preference order.
    pub offered_mimes: Vec<String>,
    /// Exact bytes per MIME, used to re-offer the copy faithfully.
    pub blobs: HashMap<String, Vec<u8>>,
}

/// Binary image copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

/// A copy that offered both text and image (or several images).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixedData {
    pub text: Option<TextData>,
    pub images: Vec<ImageData>,
}

/// Typed clipboard payload: one variant per copy, not per MIME.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(TextData),
    Image(ImageData),
    Mixed(MixedData),
}

impl ClipboardContent {
    /// Classify raw `mime -> bytes` blobs from one `Selection` into a variant.
    ///
    /// Empty blobs are dropped by the caller; an empty map yields `None`.
    /// Text is anything `is_text_mime`, image is anything in
    /// `SUPPORTED_BINARY_MIMES` (or `image/*` defensively). Unknown types are
    /// kept as images (opaque bytes) so rich copies are not silently lost.
    pub fn from_blobs(blobs: HashMap<String, Vec<u8>>) -> Option<Self> {
        let mut text_blobs: HashMap<String, Vec<u8>> = HashMap::new();
        let mut image_blobs: Vec<(String, Vec<u8>)> = Vec::new();

        for (mime, bytes) in blobs {
            if bytes.is_empty() {
                continue;
            }
            if is_text_mime(&mime) {
                text_blobs.insert(mime, bytes);
            } else {
                image_blobs.push((mime, bytes));
            }
        }

        if text_blobs.is_empty() && image_blobs.is_empty() {
            return None;
        }

        let text = Self::build_text(text_blobs);
        let mut images: Vec<ImageData> = image_blobs
            .into_iter()
            .map(|(mime_type, bytes)| ImageData { mime_type, bytes })
            .collect();
        // Deterministic order for tests / dedup / display.
        images.sort_by(|a, b| a.mime_type.cmp(&b.mime_type));

        match (text, images.len()) {
            (Some(t), 0) => Some(ClipboardContent::Text(t)),
            (None, 1) => Some(ClipboardContent::Image(images.pop().unwrap())),
            (t, _) => Some(ClipboardContent::Mixed(MixedData { text: t, images })),
        }
    }

    fn build_text(blobs: HashMap<String, Vec<u8>>) -> Option<TextData> {
        if blobs.is_empty() {
            return None;
        }
        // Primary = first PREFERRED_TEXT_MIMES hit, else sorted-first key.
        let primary_mime = PREFERRED_TEXT_MIMES
            .iter()
            .find_map(|p| blobs.contains_key(*p).then(|| p.to_string()))
            .or_else(|| {
                let mut keys: Vec<&String> = blobs.keys().collect();
                keys.sort();
                keys.first().map(|s| (*s).clone())
            })?;
        let text = String::from_utf8_lossy(blobs.get(&primary_mime).expect("primary must exist"))
            .to_string();
        let html = match blobs.get("text/html") {
            Some(bytes) => {
                let decoded = String::from_utf8_lossy(bytes).to_string();
                // Don't duplicate when HTML bytes equal the plain payload.
                if blobs.get(&primary_mime).is_some_and(|p| p == bytes)
                    || primary_mime == "text/html"
                {
                    None
                } else {
                    Some(decoded)
                }
            }
            None => None,
        };
        let mut offered_mimes: Vec<String> = blobs.keys().cloned().collect();
        offered_mimes.sort_by_key(|m| {
            PREFERRED_TEXT_MIMES
                .iter()
                .position(|p| p == m)
                .unwrap_or(usize::MAX)
        });

        // Drop the map entry clone cost? No — blobs are needed for restore.
        Some(TextData {
            text,
            html,
            primary_mime,
            offered_mimes,
            blobs,
        })
    }

    /// MIME shown in history / logs (text primary, else first image).
    pub fn primary_mime(&self) -> &str {
        match self {
            ClipboardContent::Text(t) => &t.primary_mime,
            ClipboardContent::Image(i) => &i.mime_type,
            ClipboardContent::Mixed(m) => m
                .text
                .as_ref()
                .map(|t| t.primary_mime.as_str())
                .or_else(|| m.images.first().map(|i| i.mime_type.as_str()))
                .unwrap_or("application/octet-stream"),
        }
    }

    /// Every MIME this copy offered (for re-offer on restore).
    pub fn offered_mimes(&self) -> Vec<String> {
        match self {
            ClipboardContent::Text(t) => t.offered_mimes.clone(),
            ClipboardContent::Image(i) => vec![i.mime_type.clone()],
            ClipboardContent::Mixed(m) => {
                let mut out = Vec::new();
                if let Some(t) = &m.text {
                    out.extend(t.offered_mimes.iter().cloned());
                }
                out.extend(m.images.iter().map(|i| i.mime_type.clone()));
                out
            }
        }
    }

    /// Flattened `mime -> bytes` for `restore_entry` / `SourceData`.
    pub fn representations(&self) -> HashMap<String, Vec<u8>> {
        match self {
            ClipboardContent::Text(t) => t.blobs.clone(),
            ClipboardContent::Image(i) => [(i.mime_type.clone(), i.bytes.clone())]
                .into_iter()
                .collect(),
            ClipboardContent::Mixed(m) => {
                let mut map = HashMap::new();
                if let Some(t) = &m.text {
                    map.extend(t.blobs.iter().map(|(k, v)| (k.clone(), v.clone())));
                }
                for img in &m.images {
                    map.insert(img.mime_type.clone(), img.bytes.clone());
                }
                map
            }
        }
    }

    /// True when the copy carries decodable text (Text or Mixed-with-text).
    pub fn has_text(&self) -> bool {
        match self {
            ClipboardContent::Text(_) => true,
            ClipboardContent::Image(_) => false,
            ClipboardContent::Mixed(m) => m.text.is_some(),
        }
    }

    /// Short one-line preview for history / log output.
    pub fn preview(&self) -> String {
        match self {
            ClipboardContent::Text(t) => t.text.lines().next().unwrap_or("").to_string(),
            ClipboardContent::Image(i) => {
                format!("<{} bytes of {}>", i.bytes.len(), i.mime_type)
            }
            ClipboardContent::Mixed(m) => {
                let text_part = m
                    .text
                    .as_ref()
                    .map(|t| t.text.lines().next().unwrap_or("").to_string())
                    .unwrap_or_default();
                let image_part = m
                    .images
                    .iter()
                    .map(|i| format!("<{} bytes of {}>", i.bytes.len(), i.mime_type))
                    .collect::<Vec<_>>()
                    .join(", ");
                if text_part.is_empty() {
                    image_part
                } else if image_part.is_empty() {
                    text_part
                } else {
                    format!("{text_part} + {image_part}")
                }
            }
        }
    }

    /// Convenience for image-size summaries without matching on the enum.
    pub fn image_summaries(&self) -> Vec<(String, usize)> {
        match self {
            ClipboardContent::Text(_) => vec![],
            ClipboardContent::Image(i) => vec![(i.mime_type.clone(), i.bytes.len())],
            ClipboardContent::Mixed(m) => m
                .images
                .iter()
                .map(|i| (i.mime_type.clone(), i.bytes.len()))
                .collect(),
        }
    }
}

pub struct ClipboardEntry {
    pub id: u64,
    pub timestamp: SystemTime,
    pub content: ClipboardContent,
}

impl ClipboardEntry {
    /// Backwards-compatible accessor: the entry's preferred MIME.
    pub fn mime_type(&self) -> &str {
        self.content.primary_mime()
    }

    /// True for entries carrying text (Text or Mixed-with-text).
    pub fn is_text(&self) -> bool {
        self.content.has_text()
    }

    /// Short one-line preview for history / log output.
    ///
    /// UI list views should call this (or a truncated variant) — text
    /// decoding (`from_utf8_lossy`) already happened at ingest, not here.
    pub fn preview(&self) -> String {
        self.content.preview()
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

    /// Push one grouped entry per copy from raw `mime -> bytes` blobs.
    /// Returns the new entry's id if it was added.
    ///
    /// Dedup is intentionally shallow (consecutive, full-content equality).
    /// A smarter UI may want cross-history content hashing — add there,
    /// keep this as the storage-layer guard.
    pub fn add_grouped(&mut self, blobs: HashMap<String, Vec<u8>>) -> Option<u64> {
        let content = ClipboardContent::from_blobs(blobs)?;
        if let Some(last) = self.entries.back()
            && last.content == content
        {
            return None;
        }

        let id = self.next_id;
        self.next_id += 1;

        self.entries.push_back(ClipboardEntry {
            id,
            timestamp: SystemTime::now(),
            content,
        });

        while self.entries.len() > MAX_HISTORY {
            self.entries.pop_front();
        }
        Some(id)
    }

    /// Legacy single-mime push, kept for tests / simple producers.
    /// Prefer [`ClipState::add_grouped`] so one copy == one entry.
    pub fn add_entry(&mut self, mime_type: String, content: Vec<u8>) -> Option<u64> {
        if content.is_empty() {
            return None;
        }
        self.add_grouped([(mime_type, content)].into_iter().collect())
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
    use crate::config::SUPPORTED_BINARY_MIMES;

    fn blobs(pairs: &[(&str, &[u8])]) -> HashMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(m, b)| (m.to_string(), b.to_vec()))
            .collect()
    }

    #[test]
    fn rejects_empty_content() {
        let mut state = ClipState::new();
        assert_eq!(state.add_entry("text/plain".into(), vec![]), None);
        assert!(state.is_empty());
        assert_eq!(state.add_grouped(HashMap::new()), None);
        assert!(state.is_empty());
    }

    #[test]
    fn groups_text_aliases_into_single_entry() {
        let mut state = ClipState::new();
        let id = state
            .add_grouped(blobs(&[
                ("text/plain;charset=utf-8", b"hello"),
                ("text/plain", b"hello"),
                ("TEXT", b"hello"),
                ("STRING", b"hello"),
            ]))
            .unwrap();
        assert_eq!(state.len(), 1);
        let entry = state.get(id).unwrap();
        assert!(matches!(entry.content, ClipboardContent::Text(_)));
        assert_eq!(entry.mime_type(), "text/plain;charset=utf-8");
        assert_eq!(entry.preview(), "hello");
        assert_eq!(entry.content.offered_mimes().len(), 4);
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
    fn same_bytes_different_mime_in_one_copy_are_one_entry() {
        let mut state = ClipState::new();
        state
            .add_grouped(blobs(&[("text/plain", b"hi"), ("text/html", b"hi")]))
            .unwrap();
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn distinct_html_is_preserved() {
        let mut state = ClipState::new();
        let id = state
            .add_grouped(blobs(&[("text/plain", b"hi"), ("text/html", b"<b>hi</b>")]))
            .unwrap();
        let entry = state.get(id).unwrap();
        match &entry.content {
            ClipboardContent::Text(t) => {
                assert_eq!(t.text, "hi");
                assert_eq!(t.html.as_deref(), Some("<b>hi</b>"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn stores_image_as_typed_variant() {
        let mut state = ClipState::new();
        let id = state
            .add_grouped(blobs(&[("image/png", &[0u8; 8])]))
            .unwrap();
        let entry = state.get(id).unwrap();
        assert!(matches!(entry.content, ClipboardContent::Image(_)));
        assert!(!entry.is_text());
        assert_eq!(entry.preview(), "<8 bytes of image/png>");
    }

    #[test]
    fn stores_text_plus_image_as_mixed() {
        let mut state = ClipState::new();
        let id = state
            .add_grouped(blobs(&[
                ("text/plain", b"caption"),
                ("image/png", &[1u8; 4]),
            ]))
            .unwrap();
        let entry = state.get(id).unwrap();
        assert!(matches!(entry.content, ClipboardContent::Mixed(_)));
        assert!(entry.is_text());
        assert_eq!(entry.preview(), "caption + <4 bytes of image/png>");
    }

    #[test]
    fn assigns_incrementing_ids_and_lookup() {
        let mut state = ClipState::new();
        let a = state.add_entry("text/plain".into(), b"a".to_vec()).unwrap();
        let b = state.add_entry("text/plain".into(), b"b".to_vec()).unwrap();
        assert_eq!(b, a + 1);
        assert_eq!(state.get(a).unwrap().preview(), "a");
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

    #[test]
    fn unknown_binary_is_kept_not_dropped() {
        // Defensive: pick_mimes filters, but storage must not lose bytes.
        let mut state = ClipState::new();
        let id = state
            .add_grouped(blobs(&[("application/octet-stream", &[9u8; 3])]))
            .unwrap();
        let entry = state.get(id).unwrap();
        assert_eq!(entry.content.primary_mime(), "application/octet-stream");
    }

    #[test]
    fn supported_binary_list_is_image_only() {
        for m in SUPPORTED_BINARY_MIMES {
            assert!(m.starts_with("image/"), "{m} should be image/*");
        }
    }
}
