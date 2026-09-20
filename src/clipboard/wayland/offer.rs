use tracing::debug;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1;

use crate::clipboard::state::AppState;

impl Dispatch<ext_data_control_offer_v1::ExtDataControlOfferV1, ()> for AppState {
    fn event(
        state: &mut Self,
        offer: &ext_data_control_offer_v1::ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            let id = offer.id().protocol_id();
            state.push_offer_mime(id, mime_type.clone());
            debug!(%mime_type, offer_id = id, "mime offered");
        }
    }
}
