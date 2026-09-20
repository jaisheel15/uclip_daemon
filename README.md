# uclip_daemon

Wayland clipboard monitor / history daemon using `ext_data_control_v1`.

Listens for clipboard selections, reads up to `MAX_MIMES_PER_SELECTION` MIME payloads per copy over pipes, stores them in a bounded in-memory history (`MAX_HISTORY`), and can publish a history entry back as the active selection via `restore_entry`.

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

Flow in `src/main.rs`: connect → double `roundtrip` to bind seat/manager → `get_data_device` → `loop { blocking_dispatch + drain_pending_reads }`.

## Test / lint

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

14 unit tests: `mime::pick_mimes`, `history::ClipState`, `io::drain_pending_reads` (incl. bounded truncation).

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
src/lib.rs            public re-exports (ClipState, AppState, pick_mimes, …)
src/main.rs           thin bootstrap: setup_connection + run_event_loop
src/config.rs         tuning constants
src/error.rs          thiserror ClipboardError (NoSeat/NoManager/NoDevice/EntryNotFound/…)
src/mime.rs           is_text_mime + pick_mimes (single source of truth)
src/display.rs        format_entry / format_new_entry / format_history
src/clipboard/
  history.rs          ClipboardEntry + ClipState (private fields, shallow consecutive-dedup)
  state.rs            AppState (private fields + accessors, OfferData/SourceData/PendingRead)
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

```rust
state.restore_entry(entry_id, &qh)?;
```

Looks up the entry, builds `SourceData` with its MIME plus `TEXT_ALIASES` for text, offers each MIME, calls `set_selection`, then destroys the previous source (no selection gap). Errors as `ClipboardError::EntryNotFound / NoManager / NoDevice`. Intended UI message: `UiRequest::Restore { entry_id }` to the Wayland thread.

## Logging

`tracing` + `tracing-subscriber`. Former `println!/eprintln!` are now `info!/debug!/warn!`. History dumps go through `ClipState::print_history` → `format_history`.
