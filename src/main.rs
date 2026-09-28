use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, poll};
use tracing::{debug, info, warn};
use uclip_daemon::{
    AppState, ClipboardError, DaemonResponse, ReadOutcome,
    daemon::{
        server,
        snapshot::{Snapshot, push_summary},
        types::{EntrySummary, PendingRestore, ServerEvent},
    },
    drain_pending_reads,
};
use wayland_client::{Connection, EventQueue, QueueHandle};

/// Bound on `nix::poll` in the Wayland loop: restore requests stay
/// responsive within ~this many milliseconds while idle.
const RESTORE_POLL_MS: u16 = 100;

/// How long after a restore its compositor echo is still suppressed.
/// Covers normal dispatch latency without swallowing later real copies.
const ECHO_SUPPRESS_SECS: u64 = 5;

fn setup_connection() -> anyhow::Result<(Connection, EventQueue<AppState>, AppState)> {
    let conn = Connection::connect_to_env().context("connect to Wayland compositor")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut state: AppState = AppState::new();

    // Two roundtrips guarantee the initial registry globals (seat, manager)
    // have all arrived before we bind. A single blocking_dispatch may return
    // after just the first event.
    event_queue
        .roundtrip(&mut state)
        .context("initial registry roundtrip")?;
    event_queue
        .roundtrip(&mut state)
        .context("initial registry roundtrip")?;

    if state.seat().is_none() {
        return Err(ClipboardError::NoSeat.into());
    }
    if state.manager().is_none() {
        return Err(ClipboardError::NoManager.into());
    }

    let seat = state.seat().cloned().expect("seat checked above");
    let manager = state.manager().cloned().expect("manager checked above");
    let device = manager.get_data_device(&seat, &qh, ());
    state.set_device(device);

    Ok((conn, event_queue, state))
}

/// True when a freshly stored entry is the echo of our own restore.
///
/// Compares one-line previews (identical bytes in, identical preview out)
/// and requires the flag to be fresh, so a real copy arriving later is never
/// swallowed.
fn is_echo(expect_echo: &Option<(Instant, String)>, preview: &str) -> bool {
    match expect_echo {
        Some((at, expected)) => {
            at.elapsed() < Duration::from_secs(ECHO_SUPPRESS_SECS) && expected == preview
        }
        None => false,
    }
}

/// Execute one restore on the Wayland thread and answer the requesting UI.
///
/// Never propagates storage errors: a bad `entry_id` is a typed `Error`
/// reply, not a daemon crash. A dead client (`send` fails) is ignored — the
/// Wayland thread must never die over a disconnected UI.
fn handle_restore(
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
    pending: PendingRestore,
    expect_echo: &mut Option<(Instant, String)>,
) {
    // Capture before `restore_entry` mutably borrows state for the re-offer.
    let preview = state.clipboard().get(pending.entry_id).map(|e| e.preview());
    let entry_id = pending.entry_id;
    let resp = match state.restore_entry(entry_id, qh) {
        Ok(()) => {
            if let Some(p) = preview {
                *expect_echo = Some((Instant::now(), p));
            }
            DaemonResponse::Restored { entry_id }
        }
        Err(e) => {
            warn!(entry_id, error = %e, "restore failed");
            DaemonResponse::from(e)
        }
    };
    let _ = pending.reply.send(resp);
}

fn run_event_loop(
    conn: Connection,
    mut event_queue: EventQueue<AppState>,
    mut state: AppState,
    snapshot: Snapshot,
    bcast_tx: tokio::sync::broadcast::Sender<ServerEvent>,
    mut req_rx: tokio::sync::mpsc::Receiver<PendingRestore>,
) -> anyhow::Result<()> {
    info!("clipboard monitor ready");
    let qh = event_queue.handle();
    // Raw fd for the blocking wait below. `backend` (and `conn`) must
    // outlive the loop; the borrow ends when they drop at function exit.
    let backend = conn.backend();
    let wl_fd = backend.poll_fd();
    // Set after a successful restore: the compositor echoes our own
    // selection back, and the alias top-up makes that echo look like a new
    // entry. Suppress that single echo (preview match + expiry below).
    let mut expect_echo: Option<(Instant, String)> = None;

    loop {
        event_queue
            .dispatch_pending(&mut state)
            .context("dispatch Wayland events")?;
        for outcome in drain_pending_reads(&mut state) {
            if let ReadOutcome::Stored { entry_id, .. } = outcome {
                // Stale flag? Prune so it can never match a later real copy.
                if let Some((at, _)) = &expect_echo
                    && at.elapsed() >= Duration::from_secs(ECHO_SUPPRESS_SECS)
                {
                    expect_echo = None;
                }
                // Borrow dance: compute the preview (owned) first, then
                // mutate via remove() without fighting the lookup borrow.
                let preview = state.clipboard().get(entry_id).map(|e| e.preview());
                if let Some(p) = &preview
                    && is_echo(&expect_echo, p)
                {
                    state.clipboard_mut().remove(entry_id);
                    expect_echo = None;
                    debug!(entry_id, "suppressed own restore echo");
                    continue;
                }
                if let Some(entry) = state.clipboard().get(entry_id) {
                    let summary = EntrySummary::from(entry);
                    {
                        let mut snap = snapshot.write().expect("snapshot lock poisoned");
                        push_summary(&mut snap, summary.clone());
                    }
                    let _ = bcast_tx.send(ServerEvent::EntryAdded { entry: summary });
                }
            }
        }
        // Restore requests are user-initiated and latency-sensitive; drain
        // without blocking (`try_recv` is sync-safe, no runtime needed).
        while let Ok(pending) = req_rx.try_recv() {
            handle_restore(&mut state, &qh, pending, &mut expect_echo);
        }
        // Push outbound Wayland requests (e.g. a restore's set_selection)
        // before sleeping, or the compositor never sees them.
        event_queue.flush().context("flush Wayland requests")?;
        // Bound the block so restores stay responsive within ~RESTORE_POLL_MS
        // even when the compositor is otherwise silent.
        let mut fds = [PollFd::new(wl_fd, PollFlags::POLLIN)];
        match poll(&mut fds, RESTORE_POLL_MS) {
            Ok(_) => {}
            Err(Errno::EINTR) => {}
            Err(e) => return Err(e).context("poll Wayland fd"),
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Manual runtime (not #[tokio::main]): the Wayland loop below owns the
    // main thread and sleeps in `nix::poll`, waking for compositor events or
    // every RESTORE_POLL_MS to serve restore requests. Runtime worker threads
    // serve IPC; nothing Wayland-related runs on them, so no Send-bound
    // surprises from EventQueue/Connection either.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    let snapshot: Snapshot = Arc::new(std::sync::RwLock::new(Vec::new()));
    let (bcast_tx, _) = tokio::sync::broadcast::channel::<ServerEvent>(64);
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<PendingRestore>(32);

    // Bind first: a UI connecting during a missing-compositor failure gets a
    // typed error path later instead of connection-refused. Stale sockets
    // from kill -9 are unlinked inside `bind`. `block_on` enters the reactor
    // context tokio's `UnixListener::bind` requires; the listener is usable
    // from any runtime thread afterwards.
    let listener = rt
        .block_on(async { server::bind() })
        .context("bind ipc socket")?;
    rt.spawn(server::serve(
        listener,
        snapshot.clone(),
        req_tx,
        bcast_tx.clone(),
    ));

    let (conn, queue, state) = setup_connection()?;
    run_event_loop(conn, queue, state, snapshot, bcast_tx.clone(), req_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_matches_fresh_same_preview() {
        let flag = Some((Instant::now(), "hello".to_string()));
        assert!(is_echo(&flag, "hello"));
    }

    #[test]
    fn echo_rejects_different_preview() {
        let flag = Some((Instant::now(), "hello".to_string()));
        assert!(!is_echo(&flag, "other"));
    }

    #[test]
    fn echo_rejects_missing_flag() {
        assert!(!is_echo(&None, "hello"));
    }

    #[test]
    fn echo_expires_after_window() {
        let stale = Some((
            Instant::now() - Duration::from_secs(ECHO_SUPPRESS_SECS + 1),
            "hello".to_string(),
        ));
        assert!(!is_echo(&stale, "hello"));
    }
}
