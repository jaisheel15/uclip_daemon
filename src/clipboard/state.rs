pub use crate::clipboard::history::ClipState;
use std::{
    collections::{HashMap, VecDeque},
    os::fd::OwnedFd,
};

use tracing::info;
use wayland_client::{Proxy, QueueHandle, protocol::wl_seat};

use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1,
    ext_data_control_source_v1,
};

use crate::config::TEXT_ALIASES;
use crate::error::{ClipboardError, Result};
use crate::mime::is_text_mime;

#[derive(Default)]
pub struct OfferData {
    pub mime_types: Vec<String>,
}

impl OfferData {
    pub fn push_mime(&mut self, mime_type: String) {
        self.mime_types.push(mime_type);
    }
}

#[derive(Debug)]
pub struct SourceData {
    pub contents: HashMap<String, Vec<u8>>,
}

impl SourceData {
    pub fn new(contents: HashMap<String, Vec<u8>>) -> Self {
        Self { contents }
    }

    /// Look up the exact MIME, falling back to any text payload for text
    /// requests (e.g. charset casing variants we did not explicitly offer).
    pub fn content_for(&self, mime_type: &str) -> Option<&Vec<u8>> {
        self.contents.get(mime_type).or_else(|| {
            if is_text_mime(mime_type) {
                self.contents.values().next()
            } else {
                None
            }
        })
    }
}

pub struct PendingRead {
    pub fd: OwnedFd,
    pub mime_type: String,
    pub offer_id: u32,
}

impl PendingRead {
    pub fn new(fd: OwnedFd, mime_type: String, offer_id: u32) -> Self {
        Self {
            fd,
            mime_type,
            offer_id,
        }
    }
}

pub struct AppState {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<ext_data_control_manager_v1::ExtDataControlManagerV1>,
    device: Option<ext_data_control_device_v1::ExtDataControlDeviceV1>,
    /// Data source created via [`AppState::restore_entry`] and currently owned.
    /// Must be `destroy()`ed when replaced or `Cancelled` — never leak by
    /// overwriting without destroy.
    current_source: Option<ext_data_control_source_v1::ExtDataControlSourceV1>,
    /// Offer ID → advertised mimes. Filled by `DataOffer` + `Offer` events,
    /// consumed on `Selection`, cleaned up after reads / clear.
    offers: HashMap<u32, OfferData>,
    /// Currently selected (active clipboard) offer proxy. Per protocol the
    /// *previous* offer must be destroyed when a new one (or NULL) arrives.
    current_selection: Option<ext_data_control_offer_v1::ExtDataControlOfferV1>,
    /// Pipe read-ends waiting in `drain_pending_reads`, one per requested mime.
    pending_reads: VecDeque<PendingRead>,
    /// Stored history. UI threads must not touch this directly — share via
    /// channel or `Arc<Mutex<ClipState>>` after the split.
    clipboard: ClipState,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            seat: None,
            manager: None,
            device: None,
            current_source: None,
            offers: HashMap::new(),
            current_selection: None,
            pending_reads: VecDeque::new(),
            clipboard: ClipState::new(),
        }
    }

    // -- seat / manager / device -------------------------------------------

    pub fn seat(&self) -> Option<&wl_seat::WlSeat> {
        self.seat.as_ref()
    }

    pub fn set_seat(&mut self, seat: wl_seat::WlSeat) {
        self.seat = Some(seat);
    }

    pub fn manager(&self) -> Option<&ext_data_control_manager_v1::ExtDataControlManagerV1> {
        self.manager.as_ref()
    }

    pub fn set_manager(&mut self, manager: ext_data_control_manager_v1::ExtDataControlManagerV1) {
        self.manager = Some(manager);
    }

    pub fn device(&self) -> Option<&ext_data_control_device_v1::ExtDataControlDeviceV1> {
        self.device.as_ref()
    }

    pub fn set_device(&mut self, device: ext_data_control_device_v1::ExtDataControlDeviceV1) {
        self.device = Some(device);
    }

    /// Drop the device without destroying offers (used when the compositor
    /// sends `Finished` and revokes access).
    pub fn clear_device(&mut self) {
        self.device = None;
    }

    // -- sources ------------------------------------------------------------

    /// Forget the currently owned source without destroying it.
    ///
    /// Used when the compositor reports `Cancelled`: the proxy is already
    /// inert, but only the *active* source is dropped — a stale event for an
    /// older source must not clobber the replacement.
    pub fn take_source_if_active(
        &mut self,
        source: &ext_data_control_source_v1::ExtDataControlSourceV1,
    ) -> Option<ext_data_control_source_v1::ExtDataControlSourceV1> {
        if self.current_source.as_ref() == Some(source) {
            self.current_source.take()
        } else {
            None
        }
    }

    /// Replace the owned source, returning the previous one for the caller
    /// to destroy after the new selection is set (no selection gap).
    pub fn replace_source(
        &mut self,
        source: ext_data_control_source_v1::ExtDataControlSourceV1,
    ) -> Option<ext_data_control_source_v1::ExtDataControlSourceV1> {
        self.current_source.replace(source)
    }

    // -- offers -------------------------------------------------------------

    pub fn offers(&self) -> &HashMap<u32, OfferData> {
        &self.offers
    }

    pub fn offer_mimes(&self, offer_id: u32) -> Option<&[String]> {
        self.offers.get(&offer_id).map(|o| o.mime_types.as_slice())
    }

    pub fn insert_offer(&mut self, offer_id: u32) {
        self.offers.insert(offer_id, OfferData::default());
    }

    pub fn push_offer_mime(&mut self, offer_id: u32, mime_type: String) {
        // DataOffer is supposed to arrive before Offer, but be defensive:
        // create the entry if we missed the DataOffer event.
        self.offers
            .entry(offer_id)
            .or_default()
            .push_mime(mime_type);
    }

    pub fn remove_offer(&mut self, offer_id: u32) {
        self.offers.remove(&offer_id);
    }

    // -- selection ----------------------------------------------------------

    pub fn current_selection(&self) -> Option<&ext_data_control_offer_v1::ExtDataControlOfferV1> {
        self.current_selection.as_ref()
    }

    pub fn set_selection(&mut self, offer: ext_data_control_offer_v1::ExtDataControlOfferV1) {
        self.current_selection = Some(offer);
    }

    /// Destroy the previous selection offer (if any) and drop its cached mimes.
    /// Per the protocol the client must destroy the previous selection offer
    /// once a new selection (or NULL) is received.
    pub fn clear_selection(&mut self) {
        if let Some(old) = self.current_selection.take() {
            let old_id = old.id().protocol_id();
            self.offers.remove(&old_id);
            old.destroy();
        }
    }

    // -- pending reads / history --------------------------------------------

    pub fn pending_reads(&self) -> &VecDeque<PendingRead> {
        &self.pending_reads
    }

    pub fn pop_pending_read(&mut self) -> Option<PendingRead> {
        self.pending_reads.pop_front()
    }

    pub fn push_pending_read(&mut self, read: PendingRead) {
        self.pending_reads.push_back(read);
    }

    pub fn clipboard(&self) -> &ClipState {
        &self.clipboard
    }

    pub fn clipboard_mut(&mut self) -> &mut ClipState {
        &mut self.clipboard
    }

    /// Publish a history entry back to the compositor as the active selection.
    ///
    /// Contract: looks up `entry_id`, builds a [`SourceData`] with the entry's
    /// MIME plus text aliases, offers each MIME, calls `set_selection`, then
    /// destroys the previously owned source (no selection gap).
    /// Errors when the entry/manager/device is missing — callers (future UI
    /// "paste"/"restore" action, e.g. `UiRequest::Restore { entry_id }` sent to
    /// the Wayland thread with the `QueueHandle`) should surface these, not unwrap.
    pub fn restore_entry(&mut self, entry_id: u64, qh: &QueueHandle<Self>) -> Result<()> {
        // Clone out of the history first so later `&mut self` borrows
        // (replace/destroy of `current_source`) don't fight the lookup borrow.
        let (content, mime_type, found_id) = {
            let entry = self
                .clipboard
                .get(entry_id)
                .ok_or(ClipboardError::EntryNotFound(entry_id))?;
            (entry.content.clone(), entry.mime_type.clone(), entry.id)
        };

        let manager = self.manager.as_ref().ok_or(ClipboardError::NoManager)?;
        let device = self.device.as_ref().ok_or(ClipboardError::NoDevice)?;

        // Per-mime contents so `Send` can serve the exact requested type.
        // Text entries get common aliases pointing at the same bytes.
        let mut contents: HashMap<String, Vec<u8>> = HashMap::new();
        contents.insert(mime_type.clone(), content.clone());
        if is_text_mime(&mime_type) {
            for alias in TEXT_ALIASES {
                contents
                    .entry((*alias).to_string())
                    .or_insert_with(|| content.clone());
            }
        }

        let source = manager.create_data_source(qh, SourceData::new(contents.clone()));

        for mime in contents.keys() {
            source.offer(mime.clone());
        }

        device.set_selection(Some(&source));

        // Destroy the previous source we own *after* the new selection is
        // set, so there is no gap where the selection is empty.
        if let Some(old) = self.replace_source(source) {
            old.destroy();
        }

        info!("restored clipboard entry {}", found_id);

        Ok(())
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
