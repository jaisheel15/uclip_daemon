# uclip IPC Roadmap — P0 · P1 · P2

Goal: connect UI over Unix IPC sockets.
Locked scope: `tokio async sockets` + `JSON newline-delimited` + `minimal v1 (list + restore + notify)`.

Prevailing invariants (do not break):
- Only the Wayland thread touches `AppState` / `EventQueue` / `QueueHandle` (`src/clipboard/state.rs`, `src/main.rs`).
- `restore_entry()` stays Wayland-thread-only (destroy-after-set, stale `Cancelled` guard).
- `drain_pending_reads() -> Vec<ReadOutcome>` is the sole new-entry source.

---

## P0 — Protocol types ✅ DONE

State: implemented in `src/daemon/types.rs` (29 tests green: 21 old + 8 new).

What exists:
- `src/daemon/types.rs`: `EntryKind` (lowercase serde), `EntrySummary`, `UiRequest::{List,Restore,Ping,Subscribe}`, `DaemonResponse::{Entries,Restored,Pong,Subscribed,Error}`, `ServerEvent::{EntryAdded,SelectionCleared}`, `RequestEnvelope/ResponseEnvelope/EventEnvelope` with `v` + `id`.
- Constants: `PREVIEW_CHARS=200`, `MAX_LIST_LIMIT=100`, `MAX_LINE_BYTES=64KiB`.
- Helpers: `truncate_preview()` (char-safe), `clamp_limit()`, `From<ClipboardError> for DaemonResponse`, `From<&ClipboardEntry> for EntrySummary` (reuses `preview()/primary_mime()/is_text()`, `timestamp_millis` with `unwrap_or(0)`).
- 8 tests: variant summaries, truncation + emoji boundary, request round-trip + wire shape, envelope id echo + `v` default, unknown-field tolerance, error mapping (`EntryNotFound(99)`), kind lowercase, limit clamp.

You still need to do:
- [ ] `cargo test daemon::types` green (expect 8 passed).
- [ ] `cargo clippy --all-targets -- -D warnings` clean.
- [ ] `cargo fmt --check` clean.
- [ ] Freeze the wire format — any change after P2 ships is breaking (bump `v`).

---

## P1 — Snapshot + broadcast hook (no socket yet)

Why: socket tasks must serve `List` and receive pushes without locking `AppState` or seeing raw `Vec<u8>` blobs (`1000 × 1MiB` risk). Build a cheap mirror: `Vec<EntrySummary>` + broadcast.

### Tasks

- [ ] 1. Create `src/daemon/snapshot.rs`:
  ```rust
  pub type Snapshot = Arc<RwLock<Vec<EntrySummary>>>;
  pub fn list_snapshot(snap: &[EntrySummary], offset: usize, limit: usize) -> (usize, Vec<EntrySummary>);
  ```
  - `total = snap.len()`, `limit = clamp_limit(limit)`, slice `snap[offset..min(offset+limit,total)]`, empty when `offset >= total`.
- [ ] 2. Wire `src/main.rs`: create `Snapshot` + `tokio::sync::broadcast::channel::<ServerEvent>(64)` at startup, pass into `run_event_loop`.
- [ ] 3. Hook after `drain_pending_reads()` in the Wayland thread (no changes inside `io.rs`/`history.rs`):
  ```rust
  for outcome in drain_pending_reads(&mut state) {
      if let ReadOutcome::Stored { entry_id, .. } = outcome {
          if let Some(entry) = state.clipboard().get(entry_id) {
              let summary = EntrySummary::from(entry);
              { let mut s = snapshot.write().unwrap(); s.push(summary.clone()); while s.len() > MAX_HISTORY { s.remove(0); } }
              let _ = event_tx.send(ServerEvent::EntryAdded { entry: summary });
          }
      }
  }
  ```
  - `IgnoredDuplicate` / empty → no write, no send.
  - NULL selection / device `Finished` → `event_tx.send(SelectionCleared)`, no snapshot change.
- [ ] 4. Re-export in `src/lib.rs`: `pub use daemon::{snapshot, types}` (or `pub mod daemon` + use paths).
- [ ] 5. Tests in `snapshot.rs` (headless, no compositor — drive via `ClipState::add_entry/add_grouped` + `drain` or direct `push` helper):
  - `Stored` → snapshot +1, receiver gets `EntryAdded` with right `id/kind/preview`.
  - `IgnoredDuplicate` → snapshot unchanged, no event.
  - Eviction at `MAX_HISTORY` → `len == MAX_HISTORY`, `total` correct.
  - Pagination: 5 entries, `list(2,2)` → ids 3,4 + `total=5`; `limit=10000` clamps to 100; `offset >= total` → empty + total.
  - `SelectionCleared` passes through on empty snapshot.
- [ ] 6. Verify: `cargo test daemon::snapshot`, full `cargo test` (expect 29 + new), clippy + fmt clean. Wayland `tracing` logs unchanged.

Acceptance: P2 can serve `List` as pure `list_snapshot(&snapshot.read(), …)` with zero Wayland locking.

---

## P2 — Socket server ✅ DONE

State: `List`/`Ping`/`Subscribe` live end-to-end; `Restore` wired through in P3 (stub replaced).

Shipped (deviations from plan noted):
- `src/daemon/server.rs`: `socket_path()` (`$XDG_RUNTIME_DIR/uclip.sock`, `/tmp` fallback), sync `bind()` (mkdir parent, stale unlink, `chmod 0700`), per-client JSON-lines loop with `send_envelope`/`send_event`, `serve` accept loop (log-and-continue).
- `src/main.rs`: manual multi-thread runtime (not `#[tokio::main]`); Wayland loop owns the main thread, IPC on workers; bind-before-Wayland-connect.
- `PendingRestore { entry_id, reply: oneshot }` in `types.rs`, plumbed `main → serve → handle_client`.
- 10 `server.rs` harness tests (`UnixStream::pair`: List/Ping/Restore/Subscribe, fan-out, lagged resync, malformed/oversized liveness, bind perms + rebind).
- Verified live on Hyprland: `Ping→Pong`, `List` returns real captures, socket `srwx------`, stale rebind works.

## P3 — Restore wiring ✅ DONE

State: `UiRequest::Restore` publishes the entry as the live selection end-to-end.

Shipped:
- `src/main.rs`: poll loop (`dispatch_pending` → drain → non-blocking `try_recv` → `flush` → `nix::poll` 100 ms, `EINTR`-tolerant), `qh` captured once, `handle_restore` (typed `Restored`/`Error` on the oneshot, never propagates — a bad id can't kill the daemon), expect-echo suppression (entry preview + 5 s expiry, stale-flag pruning, `is_echo` + 4 tests).
- `src/clipboard/history.rs`: `ClipState::remove(id)` + test (echo dropped before snapshot/broadcast, so no phantom row or push).
- `src/daemon/server.rs`: `Restore` arm (oneshot + 5 s `RESTORE_TIMEOUT_SECS` + `shutting down`/`timed out` errors, always echoing `id`); tests rewritten from stub-shape to forward-and-reply (fake Wayland consumer) + closed-channel.
- `nix` gained the `poll` feature in `Cargo.toml`.
- Why suppression exists: the compositor echoes our own `set_selection` back, and the `TEXT_ALIASES` top-up makes the echo's MIME set wider than the stored entry, defeating consecutive-dedup. Residual accepted risk: a byte-identical user copy within 5 s of a restore is indistinguishable from the echo (one missed row).

Left for P3 sign-off (needs a compositor, headless tests can't cover `set_selection`): restore → byte-exact paste; fresh copy after restore grows history with no phantom row; bad id → `Error`; rapid-fire restores serialize with correct ids.

> Historical note: the original P2 task checklist (stub-era) is superseded by
> the sections above. The full manual socket checklist now lives in
> `TESTING.md`.

---

## Commands cheat-sheet

```sh
cargo test daemon::types
cargo test daemon::snapshot
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
RUST_LOG=debug cargo run
```
