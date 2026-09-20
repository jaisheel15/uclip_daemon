//! Typed errors for the clipboard daemon.

use thiserror::Error;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, ClipboardError>;

/// All recoverable failures of the daemon.
#[derive(Debug, Error)]
pub enum ClipboardError {
    /// No `wl_seat` global was advertised (no compositor / headless env).
    #[error("no wl_seat found: is a Wayland compositor running?")]
    NoSeat,

    /// Compositor does not expose the privileged data-control manager.
    #[error("no ext_data_control_manager_v1 found: compositor does not support clipboard managers")]
    NoManager,

    /// Data device is missing or was revoked (`Finished` event).
    #[error("data device missing: compositor revoked access or setup incomplete")]
    NoDevice,

    /// Requested history entry does not exist.
    #[error("clipboard entry {0} not found")]
    EntryNotFound(u64),

    /// Wayland protocol / event-queue failure.
    #[error("wayland error: {0}")]
    Wayland(String),

    /// Pipe creation or clipboard transfer I/O failure.
    #[error("clipboard I/O error: {0}")]
    Io(String),
}

impl From<std::io::Error> for ClipboardError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<nix::errno::Errno> for ClipboardError {
    fn from(err: nix::errno::Errno) -> Self {
        Self::Io(err.to_string())
    }
}
