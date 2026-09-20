//! MIME-type helpers: the single source of truth for "is this text?"
//! and "which offered types should we request?".

use crate::config::{MAX_MIMES_PER_SELECTION, PREFERRED_TEXT_MIMES, SUPPORTED_BINARY_MIMES};

/// True for MIME types we can preview as text.
///
/// `TEXT`, `STRING`, `UTF8_STRING` are legacy X11-style text atoms.
pub fn is_text_mime(mime: &str) -> bool {
    mime == "TEXT"
        || mime == "STRING"
        || mime == "UTF8_STRING"
        || mime == "text/plain"
        || mime.starts_with("text/plain;")
        || mime.starts_with("text/")
}

/// Pick which offered MIME types to actually request, in priority order:
///
/// 1. Preferred text types, exact match, in order.
/// 2. Any other `text/*` type (e.g. `text/csv`, unusual charset casing).
/// 3. Supported binary types.
///
/// The result is truncated to [`MAX_MIMES_PER_SELECTION`] to bound pipe usage.
pub fn pick_mimes(mime_types: &[String]) -> Vec<String> {
    let mut picked: Vec<String> = Vec::new();

    for preferred in PREFERRED_TEXT_MIMES {
        if let Some(m) = mime_types.iter().find(|m| m.as_str() == *preferred)
            && !picked.contains(m)
        {
            picked.push(m.clone());
        }
    }

    for m in mime_types {
        if is_text_mime(m) && !picked.contains(m) {
            picked.push(m.clone());
        }
    }

    for supported in SUPPORTED_BINARY_MIMES {
        if let Some(m) = mime_types.iter().find(|m| m.as_str() == *supported)
            && !picked.contains(m)
        {
            picked.push(m.clone());
        }
    }

    picked.truncate(MAX_MIMES_PER_SELECTION);
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn text_atoms_count_as_text() {
        for m in [
            "TEXT",
            "STRING",
            "UTF8_STRING",
            "text/plain",
            "text/plain;charset=utf-8",
            "text/csv",
            "text/html",
        ] {
            assert!(is_text_mime(m), "{m} should be text");
        }
    }

    #[test]
    fn binary_and_app_types_are_not_text() {
        for m in ["image/png", "application/x-foo", "APPLICATION/JSON"] {
            assert!(!is_text_mime(m), "{m} should not be text");
        }
    }

    #[test]
    fn prefers_ordered_text_mimes_first() {
        let offered = strings(&["text/html", "text/plain", "text/plain;charset=utf-8"]);
        assert_eq!(
            pick_mimes(&offered),
            strings(&["text/plain;charset=utf-8", "text/plain", "text/html"])
        );
    }

    #[test]
    fn picks_up_unlisted_text_types() {
        let offered = strings(&["text/csv", "image/png"]);
        assert_eq!(pick_mimes(&offered), strings(&["text/csv", "image/png"]));
    }

    #[test]
    fn ignores_unsupported_application_types() {
        let offered = strings(&["application/x-foo"]);
        assert!(pick_mimes(&offered).is_empty());
    }

    #[test]
    fn truncates_to_max_mimes() {
        let offered = strings(&[
            "text/plain",
            "text/markdown",
            "text/html",
            "text/uri-list",
            "text/csv",
            "image/png",
        ]);
        let picked = pick_mimes(&offered);
        assert_eq!(picked.len(), MAX_MIMES_PER_SELECTION);
    }

    #[test]
    fn dedupes_repeated_offers() {
        let offered = strings(&["text/plain", "text/plain"]);
        assert_eq!(pick_mimes(&offered), strings(&["text/plain"]));
    }
}
