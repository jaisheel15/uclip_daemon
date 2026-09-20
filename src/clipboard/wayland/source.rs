use std::{fs::File, io::Write};

use tracing::{debug, warn};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1;

use crate::clipboard::state::{AppState, SourceData};

impl Dispatch<ext_data_control_source_v1::ExtDataControlSourceV1, SourceData> for AppState {
    fn event(
        state: &mut Self,
        source: &ext_data_control_source_v1::ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        data: &SourceData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                let mut file = File::from(fd);
                debug!(%mime_type, "clipboard requested");
                match data.content_for(&mime_type) {
                    Some(bytes) => {
                        if let Err(err) = file.write_all(bytes) {
                            warn!(error = %err, "failed to send clipboard");
                        }
                    }
                    None => {
                        warn!(%mime_type, "unsupported mime request");
                        // Dropping `file` without writing signals failure (EOF) to the reader.
                    }
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                debug!("source cancelled");
                // The compositor revoked the source so the proxy is inert:
                // drop our reference, but only if this event is for the
                // active source (a stale event must not clobber its replacement).
                if let Some(old) = state.take_source_if_active(source) {
                    old.destroy();
                }
            }
            _ => {}
        }
    }
}
