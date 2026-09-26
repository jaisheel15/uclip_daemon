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

## P2 — Socket server (tokio, List/Ping/Subscribe live; Restore stubbed)

Why: real IPC. `List/Ping/Subscribe` work end-to-end. `Restore` channel is defined but execution lands in P3 (Wayland-thread call).

### Tasks

- [ ] 1. Add `src/daemon/server.rs`:
  - `pub fn socket_path() -> PathBuf`: `$XDG_RUNTIME_DIR/uclip.sock`, fallback `/tmp/uclip-$UID.sock`.
  - `pub async fn serve(listener: UnixListener, snapshot: Snapshot, req_tx: mpsc::Sender<PendingRestore>, bcast_tx: broadcast::Sender<ServerEvent>)`.
  - Boot: unlink stale path, bind, `chmod 0700` (dir + sock). Bind fail = already running → exit with message.
- [ ] 2. Per-client loop (`tokio::spawn` per accept):
  - `BufReader` + `read_line` with `MAX_LINE_BYTES` cap (over → `Error{message: "line too long"}` + drop line).
  - Parse `RequestEnvelope` via `serde_json::from_str`. Malformed → `ResponseEnvelope{id: <id or "">, resp: Error{…}}`, keep connection open.
  - Dispatch:
    - `List{offset,limit}` → `list_snapshot(&snapshot.read(), …)` → `Entries{total, entries}`.
    - `Ping` → `Pong`.
    - `Subscribe` → `Subscribed` ack, then spawn forward task: `bcast_tx.subscribe()` → each `ServerEvent` → `EventEnvelope{v:1,event}` + `\n`. Handle `Lagged` by resync hint (`Error` + client re-`List`s).
    - `Restore{entry_id}` → **stub**: reply `Error{message: "restore not wired yet (P3)"}` for now, but send through `req_tx` channel shape so P3 is drop-in. Define `PendingRestore { entry_id, reply: oneshot::Sender<DaemonResponse> }` in `types.rs` or `server.rs`.
  - Write: `serde_json::to_string(&envelope) + "\n"`, `write_all + flush`. One request per line, replies echo `id`.
- [ ] 3. Channel defs (put in `types.rs` now so P3 needs no wire change):
  ```rust
  pub struct PendingRestore { pub entry_id: u64, pub reply: tokio::sync::oneshot::Sender<DaemonResponse> }
  ```
- [ ] 4. `src/main.rs`: `#[tokio::main]`, build snapshot + channels (P1), bind `socket_path()`, spawn `serve()` task, then run Wayland loop on current thread (or `spawn_blocking`). Keep `setup_connection()` unchanged.
- [ ] 5. Tests:
  - Unit: `socket_path()` respects `XDG_RUNTIME_DIR`, falls back correctly.
  - Integration (with `tempfile` sock dir): start `serve` with empty snapshot → `List` → `total=0`; push summary → subscribed client gets `EntryAdded`; `List{0,50}` shows it; malformed line → `Error` + connection stays open; 2 concurrent subscribers both get pushes; oversized line rejected.
- [ ] 6. Manual check:
  ```sh
  cargo run &
  echo '{"v":1,"id":"r1","req":{"List":{"offset":0,"limit":50}}}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock
  echo '{"v":1,"id":"r2","req":"Subscribe"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/uclip.sock
  # copy something in Wayland session → expect EntryAdded push
  ```
- [ ] 7. Verify: `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` all clean. Kill/restart with stale sock works; perms `srwx------`.

Acceptance: UI can `List` history, `Ping`, `Subscribe` to live pushes. `Restore` returns a clean unimplemented error until P3 wires it to `state.restore_entry(id, qh)` on the Wayland thread.

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
