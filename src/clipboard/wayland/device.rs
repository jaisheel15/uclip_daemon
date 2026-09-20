use std::os::fd::AsFd;
use std::sync::Arc;

use nix::unistd::pipe;
use tracing::{debug, info, warn};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_offer_v1,
};

use crate::clipboard::state::{AppState, PendingRead};
use crate::mime::pick_mimes;

impl Dispatch<ext_data_control_device_v1::ExtDataControlDeviceV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ext_data_control_device_v1::ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                debug!("new offer");
                state.insert_offer(id.id().protocol_id());
            }

            ext_data_control_device_v1::Event::Selection { id } => {
                info!("clipboard changed");

                let Some(offer) = id else {
                    // Selection was cleared (NULL offer).
                    state.clear_selection();
                    info!("selection cleared");
                    return;
                };

                let offer_id = offer.id().protocol_id();

                // Per protocol we must destroy the previous selection offer.
                // Only destroy if it is a *different* object.
                if let Some(old) = state.current_selection().cloned()
                    && old != offer
                {
                    state.clear_selection();
                }
                state.set_selection(offer.clone());

                let Some(mimes) = state.offer_mimes(offer_id) else {
                    warn!(offer_id, "selection for unknown offer");
                    return;
                };
                let mimes = mimes.to_vec();

                debug!(offer_id, ?mimes, "offer mimes");

                let mimes = pick_mimes(&mimes);
                if mimes.is_empty() {
                    debug!("no supported mime offered");
                    return;
                }

                for mime in mimes {
                    debug!(%mime, "requesting");
                    if let Err(err) = request_mime(state, &offer, conn, offer_id, mime.clone()) {
                        warn!(error = %err, %mime, "failed to request mime");
                    }
                }
            }

            ext_data_control_device_v1::Event::Finished => {
                warn!("data device finished: compositor revoked access");
                state.clear_device();
                state.clear_selection();
            }

            ext_data_control_device_v1::Event::PrimarySelection { .. } => {
                // Ignored: this monitor only tracks the regular clipboard.
            }

            _ => {}
        }
    }

    /// Child-object factory: opcode 0 on the device creates an offer object.
    ///
    /// Must stay in sync with the protocol XML if `ext-data-control`
    /// ever versions beyond v1.
    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<Self>,
    ) -> Arc<dyn wayland_client::backend::ObjectData> {
        match opcode {
            0 => qh.make_data::<ext_data_control_offer_v1::ExtDataControlOfferV1, ()>(()),
            _ => {
                warn!(opcode, "unexpected child opcode on data device");
                qh.make_data::<ext_data_control_offer_v1::ExtDataControlOfferV1, ()>(())
            }
        }
    }
}

fn request_mime(
    state: &mut AppState,
    offer: &ext_data_control_offer_v1::ExtDataControlOfferV1,
    conn: &Connection,
    offer_id: u32,
    mime: String,
) -> std::result::Result<(), String> {
    let (read_fd, write_fd) = pipe().map_err(|e| format!("pipe failed: {e}"))?;

    offer.receive(mime.clone(), write_fd.as_fd());

    conn.flush().map_err(|e| format!("flush failed: {e}"))?;

    // Close our copy of the write end so the reader sees EOF
    // once the compositor closes its copy.
    drop(write_fd);

    state.push_pending_read(PendingRead::new(read_fd, mime, offer_id));
    Ok(())
}
