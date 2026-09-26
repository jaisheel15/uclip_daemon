use std::path::PathBuf;

use tokio::{net::UnixListener, sync::{broadcast, mpsc}};

use crate::{ServerEvent, Snapshot, daemon::types::PendingRestore};

///run/user/1000/uclip_daemon

pub fn socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir)
            .join("uclip_daemon")
            .join("uclip.sock");
    }

    let uid = nix::unistd::Uid::current();

    PathBuf::from(format!("/tmp/uclip-{}.sock", uid.as_raw()))
}

pub async fn serve(
    listener: UnixListener,
    snapshot: Snapshot,
    req_tx: mpsc::Sender<PendingRestore>,
    bcast_tx: broadcast::Sender<ServerEvent>,
) {
    loop{
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::error!("accept error: {e}");
                continue;
            }
        };
        
    }


}
