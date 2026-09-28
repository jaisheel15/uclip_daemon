# uclip_daemon

Wayland clipboard monitor / history daemon using `ext_data_control_v1`.

Listens for clipboard selections, reads up to `MAX_MIMES_PER_SELECTION` MIME payloads per copy over pipes, groups them into one typed history entry (`Text` / `Image` / `Mixed` in `ClipboardContent`), stores them in a bounded in-memory history (`MAX_HISTORY`), and can publish a history entry back as the active selection via `restore_entry`.

## Requirements

- Wayland compositor exposing `wl_seat` + `ext_data_control_manager_v1` (privileged clipboard-manager protocol — compositor must allow it)
- Rust stable toolchain
- `WAYLAND_DISPLAY` set (uses `Connection::connect_to_env()`)

## Run

```sh
cargo run
# with log filtering (tracing-subscriber env-filter):
RUST_LOG=debug cargo run
```

Flow in `src/main.rs`: bind IPC socket → double `roundtrip` to bind seat/manager → `get_data_device` → `loop { dispatch_pending + drain_pending_reads + try_recv restores + flush + poll(100ms) }`. The Wayland loop owns the main thread; a manually built tokio runtime serves IPC on worker threads. UI protocol: newline-delimited JSON over `$XDG_RUNTIME_DIR/uclip.sock` — see `TESTING.md`.

## Test / lint

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

50 tests: `mime::pick_mimes`, `history::ClipState` (typed Text/Image/Mixed, single-entry grouping, `remove`), `io::drain_pending_reads` (incl. bounded truncation), `daemon::types` (protocol serde), `daemon::snapshot` (paging/clamp/eviction), `daemon::server` (`UnixStream::pair` harness: List/Ping/Restore/Subscribe, fan-out, lagged resync), `main::is_echo`.

## Configuration

All tuning lives in `src/config.rs`:

| Constant | Default | Meaning |
|---|---|---|
| `MAX_HISTORY` | 1000 | entries retained |
| `MAX_CONTENT_BYTES` | 1 MiB | bytes kept per entry; reads use `take(limit+1)` so oversized pastes are truncated without unbounded buffering |
| `PREFERRED_TEXT_MIMES` | see file | request priority order |
| `SUPPORTED_BINARY_MIMES` | png/jpeg/webp/gif | other types (e.g. `application/x-foo`) ignored |
| `MAX_MIMES_PER_SELECTION` | 4 | pipe fan-out bound per copy |
| `TEXT_ALIASES` | see file | extra MIME offers when restoring text |

## Architecture

```
src/lib.rs            public re-exports (ClipState, AppState, pick_mimes, daemon types…)
src/main.rs           bootstrap + run_event_loop (poll loop, handle_restore, echo-suppress) + is_echo tests
src/config.rs         tuning constants
src/error.rs          thiserror ClipboardError (NoSeat/NoManager/NoDevice/EntryNotFound/…)
src/mime.rs           is_text_mime + pick_mimes (single source of truth)
src/display.rs        format_entry / format_new_entry / format_history
src/daemon/
  types.rs            wire protocol: UiRequest/DaemonResponse/ServerEvent envelopes + EntrySummary
  snapshot.rs         UI mirror (Arc<RwLock<Vec<EntrySummary>>>, list_snapshot, push_summary)
  server.rs           UnixListener bind + per-client JSON-lines loop + serve + harness tests
src/clipboard/
  history.rs          ClipboardContent (Text/TextData + Image/ImageData + Mixed/MixedData) + ClipboardEntry + ClipState (add_grouped: one copy == one entry, consecutive-dedup, remove for echo-suppress)
  state.rs            AppState (private fields + accessors, OfferData/SourceData/PendingRead) + restore_entry
  io.rs               drain_pending_reads -> Vec<ReadOutcome>, bounded reads, tracing only
  wayland/
    registry.rs       Dispatch<WlRegistry> (+ seat/manager bind)
    source.rs         Dispatch<Source> (Send/Cancelled)
    offer.rs          Dispatch<Offer> (mime advertisement)
    device.rs         Dispatch<Device> (Selection/DataOffer/Finished) + child factory
```

Key invariants:

- `AppState` fields are private — access via `seat()/set_seat()`, `offers/push_offer_mime/insert_offer/remove_offer`, `pending_reads/push/pop`, `clipboard()/clipboard_mut()`.
- Previous selection offer must be destroyed on new/NULL selection (`clear_selection`).
- Owned `Source` must be destroyed after replacement or `Cancelled` (`replace_source` / `take_source_if_active` — stale events never clobber the replacement).
- Offer MIME cache is dropped after reads so stale entries can't accumulate.

## Restoring history

Live over IPC — no direct API needed. The UI sends one line:

```json
{"v":1,"id":"r1","req":{"Restore":{"entry_id":7}}}
```

`handle_client` forwards a `PendingRestore` over the bounded-32 `mpsc` channel; the Wayland thread picks it up within ~100 ms (`try_recv` in the poll loop) and calls:

```rust
state.restore_entry(entry_id, &qh)?;
```

Looks up the entry, re-offers its exact stored representations (plus `TEXT_ALIASES` top-up for text), offers each MIME, calls `set_selection`, then destroys the previous source (no selection gap). Replies `Restored { entry_id }`, or typed `Error` for `EntryNotFound / NoManager / NoDevice`, restore timeouts (5 s), and shutdown — always echoing the request `id`.

Self-echo suppression: the compositor re-announces our own selection, and the alias top-up makes that echo look like a new entry. After a successful restore the loop arms an expect-echo flag (entry preview + 5 s expiry); the echo is dropped from history via `ClipState::remove` before snapshot/broadcast, so the UI never sees a phantom row.

## Logging

`tracing` + `tracing-subscriber`. Former `println!/eprintln!` are now `info!/debug!/warn!`. History dumps go through `ClipState::print_history` → `format_history`.
