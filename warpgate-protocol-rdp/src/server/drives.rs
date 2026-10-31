//! The native viewer's redirected drives, relayed to the target.
//!
//! Warpgate is the viewer's RDPDR server. It accepts the viewer's filesystem devices as
//! they are announced — before any target is chosen — but nothing touches them until a
//! target with drive redirection enabled attaches ([`DriveService::attach`]). From then
//! on each IRP the target sends, once [`crate::drive::DrivePolicy`] lets it through, is
//! forwarded to the viewer with its ids intact, and the viewer's completion goes back
//! to the target the same way. The RDPDR server only accepts completions for requests it
//! forwarded, so the viewer cannot inject replies.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ironrdp_core::impl_as_any;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_rdpdr::pdu::efs::{
    ClientDriveLockControlResponse, ClientDriveNotifyChangeDirectoryResponse,
    ClientDriveQueryDirectoryResponse, ClientDriveQueryInformationResponse,
    ClientDriveQuerySecurityResponse, ClientDriveQueryVolumeInformationResponse,
    ClientDriveSetInformationResponse, ClientDriveSetSecurityResponse, DeviceAnnounceHeader,
    DeviceCloseResponse, DeviceControlResponse, DeviceCreateResponse, DeviceFlushBuffersResponse,
    DeviceReadResponse, DeviceType, DeviceWriteResponse, NtStatus, ServerDriveIoRequest,
};
use ironrdp_server::{
    RdpdrServerBackend, RdpdrServerFactory, RdpdrServerMessage, ServerEvent, ServerEventSender,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info};

use crate::drive::{DriveService, DriveUpdate, MAX_DRIVES, failure};

#[derive(Debug, Default)]
struct Hub {
    /// Accepted filesystem devices: `(device id, name)`.
    drives: Vec<(u32, String)>,
    /// Into the viewer's RDP server, to send it forwarded IRPs.
    server: Option<UnboundedSender<ServerEvent>>,
    /// To the target side, once a target with drives enabled is up.
    target: Option<UnboundedSender<DriveUpdate>>,
}

/// The viewer's drives. Cloning shares them.
#[derive(Debug, Clone, Default)]
pub(crate) struct ViewerDrives(Arc<Mutex<Hub>>);

impl ViewerDrives {
    fn hub(&self) -> MutexGuard<'_, Hub> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn to_target(&self, update: DriveUpdate) {
        if let Some(target) = &self.hub().target {
            let _ = target.send(update);
        }
    }
}

/// The name a drive is shown under on the target: the viewer's full name for it when it
/// sent one (UTF-16 in the device data), else its DOS name, limited to printable text.
pub(crate) fn drive_name(device: &DeviceAnnounceHeader) -> String {
    let data = device.device_data();
    let full = (data.len().is_multiple_of(2) && !data.is_empty())
        .then(|| {
            let units: Vec<u16> = data
                .chunks_exact(2)
                .filter_map(|pair| Some(u16::from_le_bytes(pair.try_into().ok()?)))
                .take_while(|unit| *unit != 0)
                .collect();
            String::from_utf16(&units).ok()
        })
        .flatten();
    let name = full
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| device.preferred_dos_name().trim_end_matches(':').to_owned());
    let name: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '\\' && *c != '/')
        .take(32)
        .collect();
    if name.trim().is_empty() {
        format!("Drive {}", device.device_id())
    } else {
        name
    }
}

impl ServerEventSender for ViewerDrives {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.hub().server = Some(sender);
    }
}

impl RdpdrServerFactory for ViewerDrives {
    fn build_backend(&self) -> Box<dyn RdpdrServerBackend> {
        // A new channel re-announces its devices.
        let removed: Vec<u32> = {
            let mut hub = self.hub();
            hub.drives.drain(..).map(|(id, _)| id).collect()
        };
        if !removed.is_empty() {
            self.to_target(DriveUpdate::Remove(removed));
        }
        Box::new(ViewerDrivesBackend(self.clone()))
    }
}

impl DriveService for ViewerDrives {
    fn attach(&self, updates: UnboundedSender<DriveUpdate>) {
        let mut hub = self.hub();
        if !hub.drives.is_empty() {
            let _ = updates.send(DriveUpdate::Announce(hub.drives.clone()));
        }
        hub.target = Some(updates);
    }

    fn request(&self, request: ServerDriveIoRequest) {
        let sent = self.hub().server.as_ref().is_some_and(|server| {
            server
                .send(ServerEvent::Rdpdr(RdpdrServerMessage::Forward(
                    request.clone(),
                )))
                .is_ok()
        });
        if !sent {
            self.to_target(DriveUpdate::Completion(failure(
                &request,
                NtStatus::UNSUCCESSFUL,
            )));
        }
    }
}

#[derive(Debug)]
struct ViewerDrivesBackend(ViewerDrives);

impl_as_any!(ViewerDrivesBackend);

impl ViewerDrivesBackend {
    fn complete(&self, pdu: RdpdrPdu) -> PduResult<()> {
        self.0.to_target(DriveUpdate::Completion(pdu));
        Ok(())
    }
}

impl RdpdrServerBackend for ViewerDrivesBackend {
    fn on_device_announce(&mut self, devices: &[DeviceAnnounceHeader]) -> Vec<(u32, bool)> {
        let mut decisions = Vec::with_capacity(devices.len());
        let mut announced = Vec::new();
        {
            let mut hub = self.0.hub();
            for device in devices {
                let id = device.device_id();
                // Only drives: printers, ports and smart cards are not relayed.
                let accept = device.device_type() == DeviceType::Filesystem
                    && hub.drives.len() < MAX_DRIVES
                    && !hub.drives.iter().any(|(known, _)| *known == id);
                if accept {
                    let name = drive_name(device);
                    info!(device_id = id, %name, "Viewer redirected a drive");
                    hub.drives.push((id, name.clone()));
                    announced.push((id, name));
                }
                decisions.push((id, accept));
            }
        }
        if !announced.is_empty() {
            self.0.to_target(DriveUpdate::Announce(announced));
        }
        decisions
    }

    fn on_device_remove(&mut self, device_ids: &[u32]) {
        self.0
            .hub()
            .drives
            .retain(|(id, _)| !device_ids.contains(id));
        self.0.to_target(DriveUpdate::Remove(device_ids.to_vec()));
    }

    fn on_client_name(&mut self, computer_name: &str) {
        debug!(%computer_name, "Viewer RDPDR client name");
    }

    fn on_create_complete(&mut self, response: &DeviceCreateResponse) -> PduResult<()> {
        self.complete(RdpdrPdu::DeviceCreateResponse(response.clone()))
    }

    fn on_close_complete(&mut self, response: &DeviceCloseResponse) -> PduResult<()> {
        self.complete(RdpdrPdu::DeviceCloseResponse(response.clone()))
    }

    fn on_read_complete(&mut self, response: &DeviceReadResponse) -> PduResult<()> {
        self.complete(RdpdrPdu::DeviceReadResponse(DeviceReadResponse {
            device_io_reply: response.device_io_reply.clone(),
            read_data: response.read_data.clone(),
        }))
    }

    fn on_write_complete(&mut self, response: &DeviceWriteResponse) -> PduResult<()> {
        self.complete(RdpdrPdu::DeviceWriteResponse(response.clone()))
    }

    fn on_flush_buffers_complete(
        &mut self,
        response: &DeviceFlushBuffersResponse,
    ) -> PduResult<()> {
        self.complete(RdpdrPdu::DeviceFlushBuffersResponse(response.clone()))
    }

    fn on_device_control_complete(&mut self, response: &DeviceControlResponse) -> PduResult<()> {
        // Device control is never forwarded (see `DrivePolicy`), so there is no request
        // on the target side this could answer.
        debug!(
            completion_id = response.device_io_reply.completion_id,
            "Ignoring device control completion"
        );
        Ok(())
    }

    fn on_query_information_complete(
        &mut self,
        response: &ClientDriveQueryInformationResponse,
    ) -> PduResult<()> {
        self.complete(RdpdrPdu::ClientDriveQueryInformationResponse(
            response.clone(),
        ))
    }

    fn on_set_information_complete(
        &mut self,
        response: &ClientDriveSetInformationResponse,
    ) -> PduResult<()> {
        self.complete(RdpdrPdu::ClientDriveSetInformationResponse(
            response.clone(),
        ))
    }

    fn on_query_directory_complete(
        &mut self,
        response: &ClientDriveQueryDirectoryResponse,
    ) -> PduResult<()> {
        self.complete(RdpdrPdu::ClientDriveQueryDirectoryResponse(
            response.clone(),
        ))
    }

    fn on_notify_change_directory_complete(
        &mut self,
        response: &ClientDriveNotifyChangeDirectoryResponse,
    ) -> PduResult<()> {
        self.complete(RdpdrPdu::ClientDriveNotifyChangeDirectoryResponse(
            response.clone(),
        ))
    }

    fn on_query_volume_information_complete(
        &mut self,
        response: &ClientDriveQueryVolumeInformationResponse,
    ) -> PduResult<()> {
        self.complete(RdpdrPdu::ClientDriveQueryVolumeInformationResponse(
            response.clone(),
        ))
    }

    fn on_lock_control_complete(
        &mut self,
        response: &ClientDriveLockControlResponse,
    ) -> PduResult<()> {
        self.complete(RdpdrPdu::ClientDriveLockControlResponse(response.clone()))
    }

    fn on_query_security_complete(
        &mut self,
        response: &ClientDriveQuerySecurityResponse,
    ) -> PduResult<()> {
        self.complete(RdpdrPdu::ClientDriveQuerySecurityResponse(response.clone()))
    }

    fn on_set_security_complete(
        &mut self,
        response: &ClientDriveSetSecurityResponse,
    ) -> PduResult<()> {
        self.complete(RdpdrPdu::ClientDriveSetSecurityResponse(response.clone()))
    }

    fn on_forward_rejected(&mut self, request: ServerDriveIoRequest) {
        self.0.to_target(DriveUpdate::Completion(failure(
            &request,
            NtStatus::UNSUCCESSFUL,
        )));
    }
}
