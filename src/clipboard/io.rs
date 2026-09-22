//! Reading clipboard payloads off pipe fds and storing them in history.
//!
//! The Wayland thread queues one [`PendingRead`] per requested MIME; this
//! module groups reads by offer id so one `Selection` == one history entry
//! (typed `Text` / `Image` / `Mixed`), then stores with a bounded
//! `take(limit + 1)` read so a hostile or buggy client cannot force
//! unbounded buffering.

use std::{collections::HashMap, fs::File, io::Read};

use tracing::{debug, info, warn};

use crate::clipboard::state::AppState;
use crate::config::MAX_CONTENT_BYTES;
use crate::display::format_new_entry;

/// Outcome of a drained copy (one offer id) or a per-MIME transfer issue,
/// useful for tests and future UI hooks.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    Stored {
        entry_id: u64,
        primary_mime: String,
    },
    IgnoredDuplicate {
        primary_mime: String,
    },
    Truncated {
        mime_type: String,
        original_bytes: usize,
    },
    ReadError {
        mime_type: String,
        error: String,
    },
}

/// Read at most `MAX_CONTENT_BYTES + 1` bytes so oversized pastes are
/// detected without buffering the full payload.
fn read_bounded(mut file: File) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let n = file
        .by_ref()
        .take((MAX_CONTENT_BYTES + 1) as u64)
        .read_to_end(&mut buf)?;
    let truncated = n > MAX_CONTENT_BYTES;
    if truncated {
        buf.truncate(MAX_CONTENT_BYTES);
    }
    Ok((buf, truncated))
}

/// Drain every queued pipe read, grouping by offer id into one history entry
/// per copy.
///
/// Offer MIME caches are dropped once fully processed so stale entries
/// cannot accumulate. All diagnostics go through `tracing`.
pub fn drain_pending_reads(state: &mut AppState) -> Vec<ReadOutcome> {
    let mut outcomes = Vec::new();
    // Preserve first-seen offer order so entry ids stay deterministic.
    let mut order: Vec<u32> = Vec::new();
    let mut grouped: HashMap<u32, HashMap<String, Vec<u8>>> = HashMap::new();

    while let Some(read) = state.pop_pending_read() {
        let offer_id = read.offer_id;
        let mime_type = read.mime_type.clone();
        let file = File::from(read.fd);

        match read_bounded(file) {
            Ok((buf, was_truncated)) => {
                if was_truncated {
                    warn!(
                        mime_type = %mime_type,
                        max_bytes = MAX_CONTENT_BYTES,
                        "clipboard content truncated"
                    );
                    outcomes.push(ReadOutcome::Truncated {
                        mime_type: mime_type.clone(),
                        original_bytes: MAX_CONTENT_BYTES + 1,
                    });
                }
                if buf.is_empty() {
                    debug!(mime_type = %mime_type, offer_id, "empty mime payload skipped");
                    continue;
                }
                if !grouped.contains_key(&offer_id) {
                    order.push(offer_id);
                }
                grouped.entry(offer_id).or_default().insert(mime_type, buf);
            }
            Err(err) => {
                warn!(error = %err, mime_type = %mime_type, "clipboard read error");
                outcomes.push(ReadOutcome::ReadError {
                    mime_type,
                    error: err.to_string(),
                });
            }
        }
    }

    for offer_id in order {
        // Offer fully processed: drop its cached MIME types so stale
        // entries cannot accumulate.
        state.remove_offer(offer_id);
        let blobs = grouped.remove(&offer_id).unwrap_or_default();
        if blobs.is_empty() {
            continue;
        }
        // Best-effort label for the duplicate path (pre-storage).
        let label = blobs
            .keys()
            .find(|m| *m == "text/plain;charset=utf-8")
            .or_else(|| blobs.keys().find(|m| *m == "text/plain"))
            .cloned()
            .or_else(|| blobs.keys().min().cloned())
            .unwrap_or_default();

        match state.clipboard_mut().add_grouped(blobs) {
            Some(entry_id) => {
                if let Some(entry) = state.clipboard().get(entry_id) {
                    info!("{}", format_new_entry(entry));
                    let primary_mime = entry.content.primary_mime().to_string();
                    state.clipboard().print_history();
                    outcomes.push(ReadOutcome::Stored {
                        entry_id,
                        primary_mime,
                    });
                }
            }
            None => {
                debug!(offer_id, %label, "duplicate or empty copy ignored");
                outcomes.push(ReadOutcome::IgnoredDuplicate {
                    primary_mime: label,
                });
            }
        }
    }

    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;

    use nix::unistd::pipe;

    fn pipe_with(data: &[u8]) -> OwnedFd {
        let (read_fd, write_fd) = pipe().unwrap();
        let mut f = File::from(write_fd);
        use std::io::Write as _;
        f.write_all(data).unwrap();
        drop(f);
        read_fd
    }

    #[test]
    fn groups_aliases_into_single_entry() {
        let mut state = AppState::new();
        for mime in ["text/plain;charset=utf-8", "text/plain", "TEXT", "STRING"] {
            state.push_pending_read(crate::clipboard::state::PendingRead::new(
                pipe_with(b"hello"),
                mime.into(),
                1,
            ));
        }

        let outcomes = drain_pending_reads(&mut state);
        let stored: Vec<_> = outcomes
            .iter()
            .filter(|o| matches!(o, ReadOutcome::Stored { .. }))
            .collect();
        assert_eq!(stored.len(), 1, "one copy == one entry, got {outcomes:?}");
        assert_eq!(state.clipboard().len(), 1);
        assert_eq!(state.clipboard().latest().unwrap().preview(), "hello");
    }

    #[test]
    fn ignores_consecutive_duplicate_copy() {
        let mut state = AppState::new();
        state.push_pending_read(crate::clipboard::state::PendingRead::new(
            pipe_with(b"hello"),
            "text/plain".into(),
            1,
        ));
        let first = drain_pending_reads(&mut state);
        assert!(matches!(first[0], ReadOutcome::Stored { .. }));

        // Same text arrives as a new Selection (new offer id).
        state.push_pending_read(crate::clipboard::state::PendingRead::new(
            pipe_with(b"hello"),
            "text/plain".into(),
            2,
        ));
        let second = drain_pending_reads(&mut state);
        assert!(
            matches!(second[0], ReadOutcome::IgnoredDuplicate { .. }),
            "got {second:?}"
        );
        assert_eq!(state.clipboard().len(), 1);
    }

    #[test]
    fn truncates_oversized_payload() {
        let big = vec![b'x'; MAX_CONTENT_BYTES + 16];
        // Offer id must exist for removal path; insert a dummy first.
        let mut state = AppState::new();
        state.insert_offer(7);
        // Write via a background-ish chunked approach: the pipe buffer is
        // smaller than MAX+16, so spawn a writer thread to avoid deadlock.
        let (read_fd, write_fd) = pipe().unwrap();
        let writer = std::thread::spawn(move || {
            let mut f = File::from(write_fd);
            use std::io::Write as _;
            f.write_all(&big).unwrap();
        });
        state.push_pending_read(crate::clipboard::state::PendingRead::new(
            read_fd,
            "text/plain".into(),
            7,
        ));
        let outcomes = drain_pending_reads(&mut state);
        writer.join().unwrap();

        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, ReadOutcome::Truncated { .. })),
            "expected truncation, got {outcomes:?}"
        );
        let entry = state.clipboard().latest().unwrap();
        assert_eq!(entry.preview().len(), MAX_CONTENT_BYTES);
    }
}
