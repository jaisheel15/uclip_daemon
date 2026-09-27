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
