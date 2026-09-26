use std::sync::{Arc, RwLock};

use crate::MAX_HISTORY;
use crate::daemon::types::{EntrySummary, clamp_limit};

/// Shared UI mirror of history: cheap `EntrySummary` rows only, never raw blobs.
///
/// Written only by the Wayland thread, read by future socket tasks.
/// `std` lock (not tokio): the writer is sync and holds it for µs.
pub type Snapshot = Arc<RwLock<Vec<EntrySummary>>>;

/// Append one stored summary, evicting oldest while over `MAX_HISTORY`.
///
/// Single source of truth for eviction so the snapshot never diverges
/// from `ClipState` (which truncates the same way).
pub fn push_summary(snap: &mut Vec<EntrySummary>, summary: EntrySummary) {
    snap.push(summary);
    while snap.len() > MAX_HISTORY {
        snap.remove(0);
    }
}

pub fn list_snapshot(
    snap: &[EntrySummary],
    offset: usize,
    limit: usize,
) -> (usize, Vec<EntrySummary>) {
    let total = snap.len();
    let limit = clamp_limit(limit);
    let entries = snap
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    (total, entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::types::EntryKind;

    fn summary(id: u64) -> EntrySummary {
        EntrySummary {
            id,
            timestamp_millis: 0,
            kind: EntryKind::Text,
            primary_mime: "text/plain".into(),
            preview: format!("item {id}"),
            has_text: true,
        }
    }

    fn vec_n(n: u64) -> Vec<EntrySummary> {
        (1..=n).map(summary).collect()
    }

    #[test]
    fn list_pagination_returns_slice_and_total() {
        let snap = vec_n(5);
        let (total, entries) = list_snapshot(&snap, 2, 2);
        assert_eq!(total, 5);
        assert_eq!(entries.iter().map(|e| e.id).collect::<Vec<_>>(), [3, 4]);
    }

    #[test]
    fn limit_is_clamped_to_max() {
        let snap = vec_n(5);
        let (total, entries) = list_snapshot(&snap, 0, 10_000);
        assert_eq!(total, 5);
        assert!(entries.len() <= crate::daemon::types::MAX_LIST_LIMIT);
        assert_eq!(entries.len(), 5);
    }

    #[test]
    fn offset_past_end_returns_empty_with_total() {
        let snap = vec_n(3);
        let (total, entries) = list_snapshot(&snap, 99, 10);
        assert_eq!(total, 3);
        assert!(entries.is_empty());
    }

    #[test]
    fn empty_snapshot_lists_empty() {
        let (total, entries) = list_snapshot(&[], 0, 50);
        assert_eq!(total, 0);
        assert!(entries.is_empty());
    }

    #[test]
    fn push_summary_evicts_oldest_over_max_history() {
        let snap = vec_n(MAX_HISTORY as u64 + 2);
        // Simulate steady-state push through the helper (not raw push).
        let mut steady: Vec<EntrySummary> = vec_n(MAX_HISTORY as u64);
        push_summary(&mut steady, summary(MAX_HISTORY as u64 + 1));
        push_summary(&mut steady, summary(MAX_HISTORY as u64 + 2));
        assert_eq!(steady.len(), MAX_HISTORY);
        // Oldest evicted first: id 1 and 2 are gone.
        assert_eq!(steady.first().unwrap().id, 3);
        assert_eq!(snap.len(), MAX_HISTORY + 2); // sanity: raw vec untouched
    }
}
