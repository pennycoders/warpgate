//! Drive redirection toward the target ([MS-RDPEFS]).
//!
//! Warpgate is the target's RDPDR client. The drives it announces are not its own: they
//! come from a [`DriveService`] — a native viewer's redirected drives, relayed IRP by IRP
//! (see `server::drives`), or files the browser user uploaded (see `drive_uploads`).
//!
//! Every IRP the target sends passes [`DrivePolicy`] first. Requests the policy refuses are
//! completed here and never reach the drive's owner, so read-only mode holds whatever the
//! owner would have allowed. Completions coming back pass [`DriveLedger`], which pairs them
//! with their requests to keep the per-file byte counts that the transfer log reports.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use ironrdp_core::impl_as_any;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::RdpdrBackend;
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_rdpdr::pdu::efs::{
    ClientDriveLockControlResponse, ClientDriveNotifyChangeDirectoryResponse,
    ClientDriveQueryDirectoryResponse, ClientDriveQueryInformationResponse,
    ClientDriveQuerySecurityResponse, ClientDriveQueryVolumeInformationResponse,
    ClientDriveSetInformationResponse, ClientDriveSetSecurityResponse, CreateDisposition,
    CreateOptions, DesiredAccess, DeviceCloseResponse, DeviceControlRequest, DeviceControlResponse,
    DeviceCreateResponse, DeviceFlushBuffersResponse, DeviceIoRequest, DeviceIoResponse,
    DeviceReadResponse, DeviceWriteResponse, FileSystemAttributes, FileSystemInformationClass,
    Information, NtStatus, ServerDeviceAnnounceResponse, ServerDriveIoRequest,
};
use ironrdp_rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp_svc::SvcMessage;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use warpgate_core::{FileTransferDirection, FileTransferRecord};

/// The most drives a viewer may redirect through Warpgate.
pub(crate) const MAX_DRIVES: usize = 8;

/// Unanswered IRPs toward a drive's owner. A target keeps at most a handful per open file;
/// past this it is failed rather than queued.
pub(crate) const MAX_PENDING_IRPS: usize = 256;

/// Open files tracked for the transfer log; a target holding more is refused new opens.
const MAX_OPEN_FILES: usize = 1024;

/// What a drive's owner tells the target side.
#[derive(Debug)]
pub(crate) enum DriveUpdate {
    /// Drives to announce: `(device id, name)`.
    Announce(Vec<(u32, String)>),
    Remove(Vec<u32>),
    /// The completion of an IRP the target sent.
    Completion(RdpdrPdu),
}

/// The owner of the drives the target sees.
pub(crate) trait DriveService: Send + Sync + fmt::Debug {
    /// Start sending updates — the current drives first — to `updates`.
    fn attach(&self, updates: UnboundedSender<DriveUpdate>);
    /// An IRP from the target that [`DrivePolicy`] let through. Its completion comes back
    /// as [`DriveUpdate::Completion`].
    fn request(&self, request: ServerDriveIoRequest);
}

/// The device, file and completion ids every IRP carries.
pub(crate) const fn io_header(request: &ServerDriveIoRequest) -> &DeviceIoRequest {
    match request {
        ServerDriveIoRequest::ServerCreateDriveRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::ServerDriveQueryInformationRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::DeviceCloseRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::DeviceControlRequest(r) => &r.header,
        ServerDriveIoRequest::DeviceReadRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::DeviceWriteRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::DeviceFlushBuffersRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::ServerDriveSetInformationRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::ServerDriveLockControlRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::ServerDriveQuerySecurityRequest(r) => &r.device_io_request,
        ServerDriveIoRequest::ServerDriveSetSecurityRequest(r) => &r.device_io_request,
    }
}

/// The reply header of a completion; `None` for PDUs that are not completions.
pub(crate) const fn completion_header(pdu: &RdpdrPdu) -> Option<&DeviceIoResponse> {
    Some(match pdu {
        RdpdrPdu::DeviceControlResponse(r) => &r.device_io_reply,
        RdpdrPdu::DeviceCreateResponse(r) => &r.device_io_reply,
        RdpdrPdu::ClientDriveQueryInformationResponse(r) => &r.device_io_response,
        RdpdrPdu::ClientDriveQuerySecurityResponse(r) => &r.device_io_response,
        RdpdrPdu::DeviceCloseResponse(r) => &r.device_io_response,
        RdpdrPdu::ClientDriveQueryDirectoryResponse(r) => &r.device_io_reply,
        RdpdrPdu::ClientDriveQueryVolumeInformationResponse(r) => &r.device_io_reply,
        RdpdrPdu::DeviceReadResponse(r) => &r.device_io_reply,
        RdpdrPdu::DeviceWriteResponse(r) => &r.device_io_reply,
        RdpdrPdu::DeviceFlushBuffersResponse(r) => &r.device_io_response,
        RdpdrPdu::ClientDriveSetInformationResponse(r) => &r.device_io_reply,
        RdpdrPdu::ClientDriveSetSecurityResponse(r) => &r.device_io_response,
        RdpdrPdu::ClientDriveNotifyChangeDirectoryResponse(r) => &r.device_io_reply,
        RdpdrPdu::ClientDriveLockControlResponse(r) => &r.device_io_reply,
        _ => return None,
    })
}

/// A completion of `request` with `status` and no data, of the type that request expects.
pub(crate) fn failure(request: &ServerDriveIoRequest, status: NtStatus) -> RdpdrPdu {
    let reply = || DeviceIoResponse::new(io_header(request).clone(), status);
    match request {
        ServerDriveIoRequest::ServerCreateDriveRequest(_) => {
            RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
                device_io_reply: reply(),
                file_id: 0,
                information: Information::empty(),
            })
        }
        ServerDriveIoRequest::ServerDriveQueryInformationRequest(_) => {
            RdpdrPdu::ClientDriveQueryInformationResponse(ClientDriveQueryInformationResponse {
                device_io_response: reply(),
                buffer: None,
            })
        }
        ServerDriveIoRequest::DeviceCloseRequest(_) => {
            RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
                device_io_response: reply(),
            })
        }
        ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(_) => {
            RdpdrPdu::ClientDriveQueryDirectoryResponse(ClientDriveQueryDirectoryResponse {
                device_io_reply: reply(),
                buffer: None,
            })
        }
        ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(r) => {
            RdpdrPdu::ClientDriveNotifyChangeDirectoryResponse(
                ClientDriveNotifyChangeDirectoryResponse::new(
                    r.device_io_request.clone(),
                    status,
                    Vec::new(),
                ),
            )
        }
        ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(_) => {
            RdpdrPdu::ClientDriveQueryVolumeInformationResponse(
                ClientDriveQueryVolumeInformationResponse {
                    device_io_reply: reply(),
                    buffer: None,
                },
            )
        }
        ServerDriveIoRequest::DeviceControlRequest(_) => {
            RdpdrPdu::DeviceControlResponse(DeviceControlResponse {
                device_io_reply: reply(),
                output_buffer: None,
            })
        }
        ServerDriveIoRequest::DeviceReadRequest(_) => {
            RdpdrPdu::DeviceReadResponse(DeviceReadResponse {
                device_io_reply: reply(),
                read_data: Vec::new(),
            })
        }
        ServerDriveIoRequest::DeviceWriteRequest(_) => {
            RdpdrPdu::DeviceWriteResponse(DeviceWriteResponse {
                device_io_reply: reply(),
                length: 0,
            })
        }
        ServerDriveIoRequest::DeviceFlushBuffersRequest(_) => {
            RdpdrPdu::DeviceFlushBuffersResponse(DeviceFlushBuffersResponse {
                device_io_response: reply(),
            })
        }
        ServerDriveIoRequest::ServerDriveSetInformationRequest(r) => {
            match ClientDriveSetInformationResponse::new(r, status) {
                Ok(response) => RdpdrPdu::ClientDriveSetInformationResponse(response),
                // Only fails for a set buffer too large to describe, which decoding it
                // already ruled out; a zero-length success-free answer still completes it.
                Err(_) => RdpdrPdu::DeviceFlushBuffersResponse(DeviceFlushBuffersResponse {
                    device_io_response: reply(),
                }),
            }
        }
        ServerDriveIoRequest::ServerDriveLockControlRequest(r) => {
            RdpdrPdu::ClientDriveLockControlResponse(ClientDriveLockControlResponse::new(
                r.device_io_request.clone(),
                status,
            ))
        }
        ServerDriveIoRequest::ServerDriveQuerySecurityRequest(_) => {
            RdpdrPdu::ClientDriveQuerySecurityResponse(ClientDriveQuerySecurityResponse {
                device_io_response: reply(),
                security_descriptor: None,
            })
        }
        ServerDriveIoRequest::ServerDriveSetSecurityRequest(r) => {
            match ClientDriveSetSecurityResponse::new(r, status) {
                Ok(response) => RdpdrPdu::ClientDriveSetSecurityResponse(response),
                Err(_) => RdpdrPdu::DeviceFlushBuffersResponse(DeviceFlushBuffersResponse {
                    device_io_response: reply(),
                }),
            }
        }
    }
}

/// Access rights that let an open change the file or its metadata.
const WRITE_ACCESS: DesiredAccess = DesiredAccess::FILE_WRITE_DATA_OR_FILE_ADD_FILE
    .union(DesiredAccess::FILE_APPEND_DATA_OR_FILE_ADD_SUBDIRECTORY)
    .union(DesiredAccess::FILE_WRITE_EA)
    .union(DesiredAccess::FILE_WRITE_ATTRIBUTES)
    .union(DesiredAccess::FILE_DELETE_CHILD)
    .union(DesiredAccess::DELETE)
    .union(DesiredAccess::WRITE_DAC)
    .union(DesiredAccess::WRITE_OWNER)
    .union(DesiredAccess::GENERIC_WRITE)
    .union(DesiredAccess::GENERIC_ALL);

/// Which IRPs may reach a drive's owner.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DrivePolicy {
    pub read_only: bool,
}

impl DrivePolicy {
    /// `Err` with the status to complete the IRP with, if it may not go through.
    pub(crate) fn check(self, request: &ServerDriveIoRequest) -> Result<(), NtStatus> {
        match request {
            // Device control would let the target issue arbitrary FSCTLs against the
            // owner's file system, and security descriptors are not offered (see
            // `supports_drive_security`); neither is needed to browse or copy files.
            ServerDriveIoRequest::DeviceControlRequest(_) => Err(NtStatus::NOT_SUPPORTED),
            ServerDriveIoRequest::ServerDriveQuerySecurityRequest(_)
            | ServerDriveIoRequest::ServerDriveSetSecurityRequest(_) => {
                Err(NtStatus::NOT_SUPPORTED)
            }
            _ if !self.read_only => Ok(()),
            ServerDriveIoRequest::ServerCreateDriveRequest(create) => {
                // Anything but opening what exists can create, truncate or replace a file.
                if create.create_disposition != CreateDisposition::FILE_OPEN
                    || create
                        .create_options
                        .contains(CreateOptions::FILE_DELETE_ON_CLOSE)
                    || create.desired_access.intersects(WRITE_ACCESS)
                {
                    Err(NtStatus::MEDIA_WRITE_PROTECTED)
                } else {
                    Ok(())
                }
            }
            // Writes, renames, deletes, truncation and attribute changes.
            ServerDriveIoRequest::DeviceWriteRequest(_)
            | ServerDriveIoRequest::ServerDriveSetInformationRequest(_) => {
                Err(NtStatus::MEDIA_WRITE_PROTECTED)
            }
            _ => Ok(()),
        }
    }

    /// Adjust a completion on its way to the target: a read-only drive says so in its
    /// volume attributes, so Explorer greys out what would fail anyway.
    pub(crate) fn completion(self, pdu: &mut RdpdrPdu) {
        if !self.read_only {
            return;
        }
        if let RdpdrPdu::ClientDriveQueryVolumeInformationResponse(response) = pdu
            && let Some(FileSystemInformationClass::FileFsAttributeInformation(attributes)) =
                &mut response.buffer
        {
            attributes.file_system_attributes |= FileSystemAttributes::FILE_READ_ONLY_VOLUME;
        }
    }
}

#[derive(Debug)]
struct OpenFile {
    path: String,
    read: u64,
    written: u64,
}

/// IRPs in flight toward a drive's owner, and the files they opened.
#[derive(Debug, Default)]
pub(crate) struct DriveLedger {
    /// By completion id.
    pending: HashMap<u32, ServerDriveIoRequest>,
    /// By (device id, file id).
    open: HashMap<(u32, u32), OpenFile>,
}

impl DriveLedger {
    /// Record an IRP about to go to the owner. `Err` with the status to fail it with if
    /// too much is already outstanding.
    pub(crate) fn sent(&mut self, request: &ServerDriveIoRequest) -> Result<(), NtStatus> {
        let header = io_header(request);
        if self.pending.len() >= MAX_PENDING_IRPS
            || self.pending.contains_key(&header.completion_id)
            || (matches!(request, ServerDriveIoRequest::ServerCreateDriveRequest(_))
                && self.open.len() >= MAX_OPEN_FILES)
        {
            return Err(NtStatus::INSUFFICIENT_RESOURCES);
        }
        self.pending.insert(header.completion_id, request.clone());
        Ok(())
    }

    /// Pair a completion with its request. Returns `false` for a completion nothing is
    /// waiting on, which must not reach the target. A completed close reports the file's
    /// traffic, if it had any.
    pub(crate) fn completed(
        &mut self,
        pdu: &RdpdrPdu,
        records: &mut Vec<FileTransferRecord>,
    ) -> bool {
        let Some(reply) = completion_header(pdu) else {
            return false;
        };
        let Some(request) = self.pending.remove(&reply.completion_id) else {
            return false;
        };
        let header = io_header(&request);
        let key = (header.device_id, header.file_id);
        let ok = reply.io_status == NtStatus::SUCCESS;
        match (&request, pdu) {
            (
                ServerDriveIoRequest::ServerCreateDriveRequest(create),
                RdpdrPdu::DeviceCreateResponse(response),
            ) if ok => {
                self.open.insert(
                    (header.device_id, response.file_id),
                    OpenFile {
                        path: create.path.clone(),
                        read: 0,
                        written: 0,
                    },
                );
            }
            (
                ServerDriveIoRequest::DeviceReadRequest(_),
                RdpdrPdu::DeviceReadResponse(response),
            ) => {
                if let Some(file) = self.open.get_mut(&key) {
                    file.read = file.read.saturating_add(response.read_data.len() as u64);
                }
            }
            (
                ServerDriveIoRequest::DeviceWriteRequest(_),
                RdpdrPdu::DeviceWriteResponse(response),
            ) => {
                if let Some(file) = self.open.get_mut(&key) {
                    file.written = file.written.saturating_add(u64::from(response.length));
                }
            }
            (ServerDriveIoRequest::DeviceCloseRequest(_), RdpdrPdu::DeviceCloseResponse(_)) => {
                if let Some(file) = self.open.remove(&key) {
                    records.extend(file_records(&file));
                }
            }
            _ => {}
        }
        true
    }

    /// Fail every IRP still waiting on `device_ids`, and forget their files.
    pub(crate) fn removed(
        &mut self,
        device_ids: &[u32],
        records: &mut Vec<FileTransferRecord>,
    ) -> Vec<RdpdrPdu> {
        let mut failed = Vec::new();
        self.pending.retain(|_, request| {
            if device_ids.contains(&io_header(request).device_id) {
                failed.push(failure(request, NtStatus::CANCELLED));
                false
            } else {
                true
            }
        });
        self.open.retain(|(device_id, _), file| {
            if device_ids.contains(device_id) {
                records.extend(file_records(file));
                false
            } else {
                true
            }
        });
        failed
    }
}

/// One record per direction a file's bytes moved in. Reading a redirected drive moves
/// data from the viewer to the target.
fn file_records(file: &OpenFile) -> Vec<FileTransferRecord> {
    let path = file.path.trim_start_matches('\\').replace('\\', "/");
    let mut records = Vec::new();
    for (direction, size) in [
        (FileTransferDirection::Upload, file.read),
        (FileTransferDirection::Download, file.written),
    ] {
        if size > 0 {
            info!(?direction, %path, size, "Redirected drive file transfer");
            records.push(FileTransferRecord {
                direction,
                path: path.clone(),
                size,
            });
        }
    }
    records
}

pub(crate) type SharedLedger = Arc<Mutex<DriveLedger>>;

pub(crate) fn ledger(ledger: &SharedLedger) -> std::sync::MutexGuard<'_, DriveLedger> {
    ledger.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The RDPDR client backend facing the target: policy first, then the drive's owner.
#[derive(Debug)]
pub(crate) struct TargetDrives {
    service: Arc<dyn DriveService>,
    policy: DrivePolicy,
    ledger: SharedLedger,
}

impl_as_any!(TargetDrives);

impl TargetDrives {
    pub(crate) fn new(
        service: Arc<dyn DriveService>,
        policy: DrivePolicy,
        ledger: SharedLedger,
    ) -> Self {
        Self {
            service,
            policy,
            ledger,
        }
    }
}

impl RdpdrBackend for TargetDrives {
    fn handle_server_device_announce_response(
        &mut self,
        pdu: ServerDeviceAnnounceResponse,
    ) -> PduResult<()> {
        if pdu.result_code != NtStatus::SUCCESS {
            warn!(device_id = pdu.device_id, status = ?pdu.result_code, "Target refused a redirected drive");
        }
        Ok(())
    }

    fn handle_scard_call(
        &mut self,
        req: DeviceControlRequest<ScardIoCtlCode>,
        _call: ScardCall,
    ) -> PduResult<Vec<SvcMessage>> {
        // No smart card is announced, so this is a confused target.
        Ok(vec![SvcMessage::from(RdpdrPdu::DeviceControlResponse(
            DeviceControlResponse {
                device_io_reply: DeviceIoResponse::new(req.header, NtStatus::NOT_SUPPORTED),
                output_buffer: None,
            },
        ))])
    }

    fn handle_drive_io_request(&mut self, req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        if let Err(status) = self.policy.check(&req) {
            return Ok(vec![SvcMessage::from(failure(&req, status))]);
        }
        if let Err(status) = ledger(&self.ledger).sent(&req) {
            return Ok(vec![SvcMessage::from(failure(&req, status))]);
        }
        if let ServerDriveIoRequest::ServerCreateDriveRequest(create) = &req {
            info!(
                path = %create.path,
                write = create.desired_access.intersects(WRITE_ACCESS),
                "Target opened a redirected file"
            );
        }
        self.service.request(req);
        Ok(Vec::new())
    }

    fn add_drive(&mut self, _device_id: u32) -> PduResult<()> {
        Ok(())
    }

    fn remove_drive(&mut self, device_id: u32) -> PduResult<Vec<SvcMessage>> {
        let mut records = Vec::new();
        let failed = ledger(&self.ledger).removed(&[device_id], &mut records);
        Ok(failed.into_iter().map(SvcMessage::from).collect())
    }
}
