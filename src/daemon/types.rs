use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::{ClipboardEntry, ClipboardError};

/// Chars kept in `EntrySummary::preview`. Full bytes stay in history only.
pub const PREVIEW_CHARS: usize = 200;

/// Upper bound for `UiRequest::List { limit }`. The server clamps above this.
pub const MAX_LIST_LIMIT: usize = 100;

/// Max bytes per JSON line on the socket (P2 enforces on read).
pub const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Text,
    Image,
    Mixed,
}

pub struct PendingRestore {
    pub entry_id: u64,
    pub reply: tokio::sync::oneshot::Sender<DaemonResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntrySummary {
    pub id: u64,
    pub timestamp_millis: u64,
    pub kind: EntryKind,
    pub primary_mime: String,
    pub preview: String,
    pub has_text: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum UiRequest {
    List { offset: usize, limit: usize },

    Restore { entry_id: u64 },

    Ping,

    Subscribe,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DaemonResponse {
    Entries {
        total: usize,
        entries: Vec<EntrySummary>,
    },

    Restored {
        entry_id: u64,
    },

    Pong,

    Subscribed,

    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ServerEvent {
    EntryAdded { entry: EntrySummary },
    SelectionCleared,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestEnvelope {
    #[serde(default = "default_version")]
    pub v: u8,

    pub id: String,

    pub req: UiRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseEnvelope {
    #[serde(default = "default_version")]
    pub v: u8,

    pub id: String,

    pub resp: DaemonResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventEnvelope {
    #[serde(default = "default_version")]
    pub v: u8,

    pub event: ServerEvent,
}

fn default_version() -> u8 {
    1
}

pub fn truncate_preview(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// Clamp a client-supplied `List.limit` to `[0, MAX_LIST_LIMIT]`.
pub fn clamp_limit(limit: usize) -> usize {
    limit.min(MAX_LIST_LIMIT)
}

impl From<ClipboardError> for DaemonResponse {
    fn from(err: ClipboardError) -> Self {
        DaemonResponse::Error {
            message: err.to_string(),
        }
    }
}

impl From<&ClipboardEntry> for EntrySummary {
    fn from(value: &ClipboardEntry) -> Self {
        let kind = match &value.content {
            crate::ClipboardContent::Text(_) => EntryKind::Text,
            crate::ClipboardContent::Image(_) => EntryKind::Image,
            crate::ClipboardContent::Mixed(_) => EntryKind::Mixed,
        };

        let primary_mime = value.mime_type().to_string();
        let preview = truncate_preview(&value.preview(), PREVIEW_CHARS);
        let has_text = value.is_text();
        let timestamp_millis = value
            .timestamp
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        EntrySummary {
            id: value.id,
            timestamp_millis,
            kind,
            primary_mime,
            preview,
            has_text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClipState;
    use serde_json::json;

    #[test]
    fn summary_from_text_image_mixed_variants() {
        let mut state = ClipState::new();

        let text_id = state
            .add_entry("text/plain".into(), b"hello".to_vec())
            .unwrap();
        let text = EntrySummary::from(state.get(text_id).unwrap());
        assert_eq!(text.kind, EntryKind::Text);
        assert_eq!(text.primary_mime, "text/plain");
        assert_eq!(text.preview, "hello");
        assert!(text.has_text);

        let img_id = state.add_entry("image/png".into(), vec![0u8; 8]).unwrap();
        let img = EntrySummary::from(state.get(img_id).unwrap());
        assert_eq!(img.kind, EntryKind::Image);
        assert_eq!(img.preview, "<8 bytes of image/png>");
        assert!(!img.has_text);

        let mut mixed_state = ClipState::new();
        let mixed_id = mixed_state
            .add_grouped(
                [
                    ("text/plain".to_string(), b"caption".to_vec()),
                    ("image/png".to_string(), vec![1u8; 4]),
                ]
                .into_iter()
                .collect(),
            )
            .unwrap();
        let mixed = EntrySummary::from(mixed_state.get(mixed_id).unwrap());
        assert_eq!(mixed.kind, EntryKind::Mixed);
        assert!(mixed.has_text);
        assert!(mixed.preview.contains("caption"));
    }

    #[test]
    fn preview_truncation_is_char_safe() {
        // 10k-char paste collapses to PREVIEW_CHARS, history untouched.
        let mut state = ClipState::new();
        let big = "x".repeat(10_000);
        let id = state
            .add_entry("text/plain".into(), big.as_bytes().to_vec())
            .unwrap();
        let summary = EntrySummary::from(state.get(id).unwrap());
        assert_eq!(summary.preview.chars().count(), PREVIEW_CHARS);
        assert!(summary.has_text);
        // Full bytes stay in storage; only the summary is cut.
        assert_eq!(state.get(id).unwrap().preview().len(), 10_000);

        // Multibyte boundary: must not split UTF-8.
        assert_eq!(truncate_preview("héllo🌍world", 5), "héllo");
        assert_eq!(truncate_preview("abc", 10), "abc");
    }

    #[test]
    fn ui_request_json_round_trip() {
        let cases = [
            UiRequest::List {
                offset: 0,
                limit: 50,
            },
            UiRequest::Restore { entry_id: 12 },
            UiRequest::Ping,
            UiRequest::Subscribe,
        ];
        for req in cases {
            let s = serde_json::to_string(&req).unwrap();
            let back: UiRequest = serde_json::from_str(&s).unwrap();
            assert_eq!(req, back);
        }
        // Wire shape is externally tagged, as documented.
        let v: serde_json::Value =
            serde_json::from_str(r#"{"List":{"offset":0,"limit":50}}"#).unwrap();
        assert_eq!(v, json!({"List": {"offset": 0, "limit": 50}}));
    }

    #[test]
    fn envelope_id_echo_and_version_default() {
        let req = RequestEnvelope {
            v: 1,
            id: "r1".into(),
            req: UiRequest::Ping,
        };
        let s = serde_json::to_string(&req).unwrap();
        let back: RequestEnvelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back, req);
        assert_eq!(back.id, "r1");

        let resp = ResponseEnvelope {
            v: 1,
            id: "r2".into(),
            resp: DaemonResponse::Pong,
        };
        let s = serde_json::to_string(&resp).unwrap();
        let back: ResponseEnvelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "r2");

        // Missing `v` defaults to 1 (old clients / hand-written JSON).
        let legacy: RequestEnvelope = serde_json::from_str(r#"{"id":"r9","req":"Ping"}"#).unwrap();
        assert_eq!(legacy.v, 1);
        assert_eq!(legacy.req, UiRequest::Ping);
    }

    #[test]
    fn unknown_fields_are_ignored_for_forward_compat() {
        let req: RequestEnvelope =
            serde_json::from_str(r#"{"v":1,"id":"r1","req":"Ping","extra":"future"}"#).unwrap();
        assert_eq!(req.req, UiRequest::Ping);

        let resp: ResponseEnvelope =
            serde_json::from_str(r#"{"v":1,"id":"r2","resp":"Pong","extra":123}"#).unwrap();
        assert_eq!(resp.resp, DaemonResponse::Pong);

        let ev: EventEnvelope =
            serde_json::from_str(r#"{"v":1,"event":"SelectionCleared","extra":true}"#).unwrap();
        assert_eq!(ev.event, ServerEvent::SelectionCleared);
    }

    #[test]
    fn clipboard_error_maps_to_error_response() {
        let resp = DaemonResponse::from(ClipboardError::EntryNotFound(99));
        match resp {
            DaemonResponse::Error { message } => assert!(
                message.contains("99"),
                "message should name the id, got {message}"
            ),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn entry_kind_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(EntryKind::Text).unwrap(),
            json!("text")
        );
        assert_eq!(
            serde_json::to_value(EntryKind::Image).unwrap(),
            json!("image")
        );
        assert_eq!(
            serde_json::to_value(EntryKind::Mixed).unwrap(),
            json!("mixed")
        );
    }

    #[test]
    fn clamp_limit_bounds_list_paging() {
        assert_eq!(clamp_limit(50), 50);
        assert_eq!(clamp_limit(10_000), MAX_LIST_LIMIT);
    }
}
