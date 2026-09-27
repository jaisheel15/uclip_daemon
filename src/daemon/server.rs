use std::path::PathBuf;

use crate::{
    ServerEvent, Snapshot,
    daemon::types::{
        DaemonResponse, EventEnvelope, PendingRestore, RequestEnvelope, ResponseEnvelope,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::broadcast::error::RecvError::Lagged,
};
use tokio::{
    net::UnixListener,
    sync::{broadcast, mpsc},
};
use tracing::{debug, error, info, warn};

/// The path to the Unix socket used for communication with the daemon.
/// Typically `/run/user/1000/uclip.sock` or `/tmp/uclip-1000.sock`.
pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("uclip.sock");
    }

    let uid = nix::unistd::Uid::current();

    PathBuf::from(format!("/tmp/uclip-{}.sock", uid.as_raw()))
}

pub fn bind() -> anyhow::Result<UnixListener, anyhow::Error> {
    use std::os::unix::fs::PermissionsExt;

    let path = socket_path();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    // Stale socket after kill -9: unlink, tolerate NotFound. Any other error
    // (e.g. permission) is real and propagates.
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&path)?;
    // Umask-dependent otherwise: the socket must not be world-accessible.
    // Best effort — a chmod failure shouldn't fail the boot.
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)) {
        warn!("failed to chmod socket {:?}: {e}", path);
    }
    info!("Listening on {:?}", path);
    Ok(listener)
}

/// Serialize one envelope, frame with `\n`, write + flush.
///
/// The trailing newline is the framing: the UI reads with `read_line()` and
/// parses exactly one JSON object per line. Flush is mandatory — without it
/// the bytes sit in `BufWriter` and the client's `read_line` hangs forever.
/// Returns `Err` when the client went away; callers should close the loop.
async fn send_envelope(
    writer: &mut BufWriter<tokio::net::unix::OwnedWriteHalf>,
    envelope: &ResponseEnvelope,
) -> anyhow::Result<()> {
    let mut json = serde_json::to_string(envelope)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

async fn send_event(
    writer: &mut BufWriter<tokio::net::unix::OwnedWriteHalf>,
    event: &ServerEvent,
) -> anyhow::Result<()> {
    let envelope = EventEnvelope {
        v: 1,
        event: event.clone(),
    };
    let mut json = serde_json::to_string(&envelope)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn handle_client(
    stream: tokio::net::UnixStream,
    snapshot: Snapshot,
    _req_tx: mpsc::Sender<PendingRestore>,
    mut bcast_rx: broadcast::Receiver<ServerEvent>,
) -> anyhow::Result<(), anyhow::Error> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);
    let mut line = String::new();
    let mut subscribed = false;
    //this can receive messages from the server and send them to the client
    loop {
        line.clear();
        tokio::select! {

            n = reader.read_line(&mut line) => {
                match n {
                    Ok(0) => {
                        // Clean EOF: client closed its end, not an error.
                        return Ok(());
                    },
                    Ok(_) => {},
                    Err(err) => {
                        error!("read error: {err}");
                        return Err(err.into());
                    }
                }
                debug!("received: {}", line.trim());
                if line.len() > crate::MAX_LINE_BYTES {
                    error!("line too long: {} bytes", line.len());
                    let reply = ResponseEnvelope {
                        v: 1,
                        id: String::new(),
                        resp: DaemonResponse::Error {
                            message: format!(
                                "line too long: {} bytes (max {})",
                                line.len(),
                                crate::MAX_LINE_BYTES
                            ),
                        },
                    };
                    // Best effort: client sent garbage, still tell it why.
                    let _ = send_envelope(&mut writer, &reply).await;
                    continue;
                }

                debug!("received: {}", line.trim());
                let request = match serde_json::from_str::<RequestEnvelope>(&line) {
                    Ok(req) => req,
                    Err(err) => {
                        error!("failed to parse request: {err}");
                        // Try to echo the client's `id` so it can match the
                        // reply; fall back to "" when even that is unparsable.
                        let fallback_id = serde_json::from_str::<serde_json::Value>(&line)
                            .ok()
                            .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_owned))
                            .unwrap_or_default();
                        let reply = ResponseEnvelope {
                            v: 1,
                            id: fallback_id,
                            resp: DaemonResponse::Error {
                                message: format!("invalid request: {err}"),
                            },
                        };
                        if let Err(err) = send_envelope(&mut writer, &reply).await {
                            error!("write error: {err}");
                            return Err(err);
                        }
                        continue;
                    }
                };
                match request.req {
                    crate::UiRequest::List { offset, limit } => {
                        // `std` lock, not async: the Wayland thread owns the
                        // writer side. Scope the guard so it drops before any
                        // `.await` below — `RwLockReadGuard` is not `Send` and
                        // `tokio::spawn` requires `Send` futures.
                        let listed = snapshot
                            .read()
                            .map(|snap| crate::list_snapshot(&snap, offset, limit))
                            // PoisonError carries the guard, which is not
                            // `Send` — drop it here so nothing guard-holding
                            // lives across the `.await`s below.
                            .map_err(|_| ());
                        let (total, entries) = match listed {
                            Ok(v) => v,
                            Err(_) => {
                                let reply = ResponseEnvelope {
                                    v: 1,
                                    id: request.id,
                                    resp: DaemonResponse::Error {
                                        message: "history unavailable".into(),
                                    },
                                };
                                if let Err(err) = send_envelope(&mut writer, &reply).await {
                                    error!("write error: {err}");
                                    return Err(err);
                                }
                                continue;
                            }
                        };
                        let reply = ResponseEnvelope {
                            v: 1,
                            id: request.id,
                            resp: DaemonResponse::Entries { total, entries },
                        };
                        if let Err(err) = send_envelope(&mut writer, &reply).await {
                            error!("write error: {err}");
                            return Err(err);
                        }
                    }

                    crate::UiRequest::Restore { entry_id } => {
                        // P3 will take `_req_tx`, forward a PendingRestore and
                        // await the oneshot. Until then: typed stub error,
                        // same id, no hang.
                        warn!(entry_id, "restore requested before P3 wiring");
                        let reply = ResponseEnvelope {
                            v: 1,
                            id: request.id,
                            resp: DaemonResponse::Error {
                                message: "restore not wired yet (P3)".into(),
                            },
                        };
                        if let Err(err) = send_envelope(&mut writer, &reply).await {
                            error!("write error: {err}");
                            return Err(err);
                        }
                    },
                    crate::UiRequest::Ping => {
                        let ping_response  = ResponseEnvelope {
                            v: 1,
                            id: request.id,
                            resp: DaemonResponse::Pong,
                        };
                        if let Err(err) = send_envelope(&mut writer, &ping_response).await {
                            error!("write error: {err}");
                            return Err(err);
                        }
                    },
                    crate::UiRequest::Subscribe => {
                        debug!("client subscribed to events");
                        subscribed = true;
                        let response = ResponseEnvelope {
                            v: 1,
                            id: request.id,
                            resp: DaemonResponse::Subscribed,
                        };
                        if let Err(err) = send_envelope(&mut writer, &response).await {
                            error!("write error: {err}");
                            return Err(err);
                        }
                    },
                }
            }



             msg = bcast_rx.recv() , if subscribed =>{
                match msg {
                    Ok(event) => {
                        if let Err(err) = send_event(&mut writer, &event).await {
                            error!("write error: {err}");
                            return Err(err);
                        }

                    },
                    Err(Lagged(n)) => {
                        // Client fell behind the channel cap: it has a gap it
                        // cannot detect, so tell it to resync via List.
                        warn!("client lagged, missed {} events; asking to re-list", n);
                        let reply = ResponseEnvelope {
                            v: 1,
                            id: String::new(),
                            resp: DaemonResponse::Error {
                                message: "lagged, re-list".into(),
                            },
                        };
                        if let Err(err) = send_envelope(&mut writer, &reply).await {
                            error!("write error: {err}");
                            return Err(err);
                        }
                    },
                    Err(err) => {
                        // Closed: all senders dropped (daemon shutting down).
                        // Calm close, not an error.
                        info!("broadcast closed: {err}");
                        return Ok(());
                    }
                }
            }
        }
    }
}

//TODO(P2): implement graceful shutdown of the server and all clients, e.g. on SIGINT/SIGTERM.
pub async fn serve(
    listener: UnixListener,
    snapshot: Snapshot,
    req_tx: mpsc::Sender<PendingRestore>,
    bcast_tx: broadcast::Sender<ServerEvent>,
) -> anyhow::Result<(), anyhow::Error> {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let snapshot = snapshot.clone();
                let req_tx = req_tx.clone();
                let event_rx = bcast_tx.subscribe();

                tokio::spawn(async move {
                    if let Err(err) = handle_client(stream, snapshot, req_tx, event_rx).await {
                        warn!("Client disconnected with error: {}", err);
                    }
                });
            }

            Err(err) => {
                warn!("Failed to accept connection: {}", err);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UnixStream;
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

    use crate::daemon::snapshot::push_summary;
    use crate::{
        DaemonResponse, EntryKind, EntrySummary, ServerEvent, UiRequest,
        daemon::types::PendingRestore,
    };

    /// Serializes XDG env mutation: tests share one process, and
    /// `set_var`/`remove_var` are process-global (unsafe in edition 2024).
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Lock env guard, recovering from poison so one failing test does not
    /// cascade into the others.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save_xdg() -> Option<std::ffi::OsString> {
        std::env::var_os("XDG_RUNTIME_DIR")
    }

    fn restore_xdg(orig: Option<std::ffi::OsString>) {
        match orig {
            // `set_var` is unsafe in edition 2024 (process-wide mutation).
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir() {
        let _guard = lock_env();
        let orig = save_xdg();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000") };
        assert_eq!(socket_path(), PathBuf::from("/run/user/1000/uclip.sock"));
        restore_xdg(orig);
    }

    #[test]
    fn socket_path_falls_back_to_tmp() {
        let _guard = lock_env();
        let orig = save_xdg();
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
        let path = socket_path();
        // NOTE: `Path::starts_with` is component-wise, so compare as str.
        let s = path.to_str().expect("tmp socket path is utf-8");
        assert!(s.starts_with("/tmp/uclip-"), "got {s}");
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("sock"));
        restore_xdg(orig);
    }

    // `bind` needs a reactor (tokio UnixListener), hence async test.
    #[tokio::test]
    async fn bind_creates_socket_with_restricted_perms_and_rebinds() {
        let _guard = lock_env();
        let dir = std::env::temp_dir().join(format!("uclip-bind-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let orig = save_xdg();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };

        let result = (|| {
            let l1 = bind()?;
            let path = socket_path();
            assert!(path.exists(), "socket missing at {path:?}");
            let mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "socket too permissive: {mode:o}");
            drop(l1);
            // Stale socket from the first bind must not fail the reboot.
            let _l2 = bind()?;
            assert!(socket_path().exists());
            Ok::<_, anyhow::Error>(())
        })();

        restore_xdg(orig);
        let _ = std::fs::remove_dir_all(&dir);
        result.unwrap();
    }

    fn summary(id: u64) -> EntrySummary {
        EntrySummary {
            id,
            timestamp_millis: id * 1000,
            kind: EntryKind::Text,
            primary_mime: "text/plain".into(),
            preview: format!("item {id}"),
            has_text: true,
        }
    }

    fn seeded_snapshot(n: u64) -> Snapshot {
        let snap: Snapshot = Arc::new(std::sync::RwLock::new(Vec::new()));
        {
            let mut w = snap.write().unwrap();
            for id in 1..=n {
                push_summary(&mut w, summary(id));
            }
        }
        snap
    }

    fn req(id: &str, req: UiRequest) -> RequestEnvelope {
        RequestEnvelope {
            v: 1,
            id: id.into(),
            req,
        }
    }

    struct TestClient {
        reader: BufReader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
        buf: String,
    }

    impl TestClient {
        fn new(stream: UnixStream) -> Self {
            let (r, w) = stream.into_split();
            Self {
                reader: BufReader::new(r),
                writer: w,
                buf: String::new(),
            }
        }

        async fn send(&mut self, env: &RequestEnvelope) {
            let mut s = serde_json::to_string(env).unwrap();
            s.push('\n');
            self.writer.write_all(s.as_bytes()).await.unwrap();
            self.writer.flush().await.unwrap();
        }

        async fn raw_send(&mut self, s: &str) {
            self.writer.write_all(s.as_bytes()).await.unwrap();
            self.writer.flush().await.unwrap();
        }

        async fn next_response(&mut self) -> ResponseEnvelope {
            self.buf.clear();
            tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut self.buf))
                .await
                .expect("timed out waiting for response")
                .unwrap();
            serde_json::from_str(&self.buf).unwrap()
        }

        async fn next_event(&mut self) -> EventEnvelope {
            self.buf.clear();
            tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut self.buf))
                .await
                .expect("timed out waiting for event")
                .unwrap();
            serde_json::from_str(&self.buf).unwrap()
        }
    }

    /// One end of a socket pair driven by the test, the other owned by a
    /// spawned `handle_client`. Callers keep the channel halves alive.
    fn spawn_pair(
        snapshot: Snapshot,
        bcast_tx: broadcast::Sender<ServerEvent>,
    ) -> (TestClient, mpsc::Sender<PendingRestore>) {
        let (req_tx, _rx) = mpsc::channel::<PendingRestore>(8);
        let (a, b) = UnixStream::pair().unwrap();
        let tx = req_tx.clone();
        tokio::spawn(handle_client(b, snapshot, tx, bcast_tx.subscribe()));
        (TestClient::new(a), req_tx)
    }

    #[tokio::test]
    async fn list_returns_page_with_id_echo_and_total() {
        let snapshot = seeded_snapshot(3);
        let (bcast_tx, _) = broadcast::channel::<ServerEvent>(64);
        let (mut client, _req_tx) = spawn_pair(snapshot, bcast_tx);

        client
            .send(&req(
                "r1",
                UiRequest::List {
                    offset: 1,
                    limit: 1,
                },
            ))
            .await;
        let resp = client.next_response().await;
        assert_eq!(resp.id, "r1");
        match resp.resp {
            DaemonResponse::Entries { total, entries } => {
                assert_eq!(total, 3);
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].id, 2);
            }
            other => panic!("expected Entries, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ping_gets_pong() {
        let snapshot = seeded_snapshot(0);
        let (bcast_tx, _) = broadcast::channel::<ServerEvent>(64);
        let (mut client, _req_tx) = spawn_pair(snapshot, bcast_tx);

        client.send(&req("p1", UiRequest::Ping)).await;
        let resp = client.next_response().await;
        assert_eq!(resp.id, "p1");
        assert_eq!(resp.resp, DaemonResponse::Pong);
    }

    #[tokio::test]
    async fn restore_returns_stub_error_without_hang() {
        let snapshot = seeded_snapshot(1);
        let (bcast_tx, _) = broadcast::channel::<ServerEvent>(64);
        let (mut client, _req_tx) = spawn_pair(snapshot, bcast_tx);

        client
            .send(&req("rr", UiRequest::Restore { entry_id: 99 }))
            .await;
        let resp = client.next_response().await;
        assert_eq!(resp.id, "rr");
        match resp.resp {
            DaemonResponse::Error { message } => {
                assert!(message.contains("P3"), "stub should name P3, got {message}")
            }
            other => panic!("expected Error stub, got {other:?}"),
        }
        // Connection still alive afterwards.
        client.send(&req("p2", UiRequest::Ping)).await;
        let resp = client.next_response().await;
        assert_eq!(resp.resp, DaemonResponse::Pong);
    }

    #[tokio::test]
    async fn malformed_line_errors_but_keeps_connection() {
        let snapshot = seeded_snapshot(0);
        let (bcast_tx, _) = broadcast::channel::<ServerEvent>(64);
        let (mut client, _req_tx) = spawn_pair(snapshot, bcast_tx);

        client.raw_send("this is not json\n").await;
        let resp = client.next_response().await;
        match resp.resp {
            DaemonResponse::Error { message } => {
                assert!(message.contains("invalid request"), "got {message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        client.send(&req("after", UiRequest::Ping)).await;
        let resp = client.next_response().await;
        assert_eq!(resp.resp, DaemonResponse::Pong);
    }

    #[tokio::test]
    async fn oversized_line_rejected_and_connection_survives() {
        let snapshot = seeded_snapshot(0);
        let (bcast_tx, _) = broadcast::channel::<ServerEvent>(64);
        let (mut client, _req_tx) = spawn_pair(snapshot, bcast_tx);

        let big = "x".repeat(crate::MAX_LINE_BYTES + 1024);
        client.raw_send(&format!("{big}\n")).await;
        let resp = client.next_response().await;
        match resp.resp {
            DaemonResponse::Error { message } => {
                assert!(message.contains("line too long"), "got {message}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
        client.send(&req("after", UiRequest::Ping)).await;
        let resp = client.next_response().await;
        assert_eq!(resp.resp, DaemonResponse::Pong);
    }

    #[tokio::test]
    async fn subscribe_fanout_reaches_both_clients() {
        let snapshot = seeded_snapshot(1);
        let (bcast_tx, _keep) = broadcast::channel::<ServerEvent>(64);
        let (mut c1, _t1) = spawn_pair(snapshot.clone(), bcast_tx.clone());
        let (mut c2, _t2) = spawn_pair(snapshot, bcast_tx.clone());

        c1.send(&req("s1", UiRequest::Subscribe)).await;
        assert_eq!(c1.next_response().await.resp, DaemonResponse::Subscribed);
        c2.send(&req("s2", UiRequest::Subscribe)).await;
        assert_eq!(c2.next_response().await.resp, DaemonResponse::Subscribed);

        let pushed = ServerEvent::EntryAdded { entry: summary(42) };
        bcast_tx.send(pushed.clone()).unwrap();

        let e1 = c1.next_event().await;
        let e2 = c2.next_event().await;
        assert_eq!(e1.event, pushed);
        assert_eq!(e2.event, pushed);
    }

    #[tokio::test]
    async fn lagged_client_gets_resync_error() {
        let snapshot = seeded_snapshot(0);
        let (bcast_tx, _) = broadcast::channel::<ServerEvent>(1);
        // Overflow the cap-1 buffer before the task can poll it: the receiver
        // is already behind when `handle_client` starts. The `if subscribed`
        // guard keeps it unpolled until Subscribe below, so this is exact.
        let rx = bcast_tx.subscribe();
        bcast_tx
            .send(ServerEvent::EntryAdded { entry: summary(1) })
            .unwrap();
        bcast_tx
            .send(ServerEvent::EntryAdded { entry: summary(2) })
            .unwrap();

        let (req_tx, _rx) = mpsc::channel::<PendingRestore>(8);
        let (a, b) = UnixStream::pair().unwrap();
        tokio::spawn(handle_client(b, snapshot, req_tx, rx));
        let mut client = TestClient::new(a);

        client.send(&req("s1", UiRequest::Subscribe)).await;
        assert_eq!(
            client.next_response().await.resp,
            DaemonResponse::Subscribed
        );
        let resp = client.next_response().await;
        assert_eq!(resp.id, "");
        match resp.resp {
            DaemonResponse::Error { message } => {
                assert!(message.contains("re-list"), "got {message}")
            }
            other => panic!("expected lagged resync Error, got {other:?}"),
        }
    }
}
