use std::sync::Arc;

use anyhow::Context;
use tracing::info;
use uclip_daemon::{
    AppState, ClipboardError, ReadOutcome,
    daemon::{
        server,
        snapshot::{Snapshot, push_summary},
        types::{EntrySummary, PendingRestore, ServerEvent},
    },
    drain_pending_reads,
};
use wayland_client::{Connection, EventQueue};

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

fn run_event_loop(
    _conn: Connection,
    mut event_queue: EventQueue<AppState>,
    mut state: AppState,
    snapshot: Snapshot,
    bcast_tx: tokio::sync::broadcast::Sender<ServerEvent>,
    // P3 will poll this for UiRequest::Restore and call
    // `state.restore_entry(id, &qh)` on this thread. Kept alive (not read)
    // in P2 so sends never fail with a closed channel.
    _req_rx: tokio::sync::mpsc::Receiver<PendingRestore>,
) -> anyhow::Result<()> {
    info!("clipboard monitor ready");
    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .context("dispatch Wayland events")?;
        for outcome in drain_pending_reads(&mut state) {
            if let ReadOutcome::Stored { entry_id, .. } = outcome
                && let Some(entry) = state.clipboard().get(entry_id)
            {
                let summary = EntrySummary::from(entry);
                {
                    let mut snap = snapshot.write().expect("snapshot lock poisoned");
                    push_summary(&mut snap, summary.clone());
                }
                let _ = bcast_tx.send(ServerEvent::EntryAdded { entry: summary });
            }
        }
        // TODO(P1): broadcast ServerEvent::SelectionCleared on NULL selection /
        // device Finished. drain_pending_reads doesn't surface it, so this needs
        // an explicit hook from the device handler (P2/P3).
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Manual runtime (not #[tokio::main]): the Wayland loop below blocks
    // forever on `blocking_dispatch` and must own the main thread. Runtime
    // worker threads serve IPC; nothing Wayland-related runs on them, so no
    // Send-bound surprises from EventQueue/Connection either.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    let snapshot: Snapshot = Arc::new(std::sync::RwLock::new(Vec::new()));
    let (bcast_tx, _) = tokio::sync::broadcast::channel::<ServerEvent>(64);
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<PendingRestore>(32);

    // Bind first: a UI connecting during a missing-compositor failure gets a
    // typed error path later instead of connection-refused. Stale sockets
    // from kill -9 are unlinked inside `bind`.
    let listener = server::bind().context("bind ipc socket")?;
    rt.spawn(server::serve(
        listener,
        snapshot.clone(),
        req_tx,
        bcast_tx.clone(),
    ));

    let (conn, queue, state) = setup_connection()?;
    run_event_loop(conn, queue, state, snapshot, bcast_tx, req_rx)
}
