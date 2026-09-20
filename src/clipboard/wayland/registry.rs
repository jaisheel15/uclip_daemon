use tracing::{debug, info};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{wl_registry, wl_seat},
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1;

use crate::clipboard::state::AppState;

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                debug!(name, %interface, version, "global advertised");

                match interface.as_str() {
                    "wl_seat" => {
                        // wl_seat is currently at version 9; clamp to what we
                        // understand (8+ has no breaking changes for our use).
                        let seat =
                            registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(9), qh, ());
                        state.set_seat(seat);
                    }

                    "ext_data_control_manager_v1" => {
                        let manager = registry
                            .bind::<ext_data_control_manager_v1::ExtDataControlManagerV1, _, _>(
                                name,
                                version.min(1),
                                qh,
                                (),
                            );
                        state.set_manager(manager);
                    }

                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                // If the seat or manager disappears our proxies are inert.
                // Logging is enough for this monitor; a full client would
                // tear down and re-bind here.
                info!(name, "global removed");
            }
            _ => {}
        }
    }
}

delegate_noop!(AppState: ignore wl_seat::WlSeat);
delegate_noop!(AppState: ignore ext_data_control_manager_v1::ExtDataControlManagerV1);
