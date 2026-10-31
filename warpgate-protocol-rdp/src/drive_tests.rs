//! Drive redirection: the IRP policy, the completion ledger, and the native relay over
//! real RDPDR PDUs between a viewer's RDPDR client and Warpgate's RDPDR server.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ironrdp_core::impl_as_any;
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_rdpdr::pdu::efs::{
    CreateDisposition, CreateOptions, DesiredAccess, DeviceCloseRequest, DeviceCloseResponse,
    DeviceControlRequest, DeviceCreateRequest, DeviceCreateResponse, DeviceIoRequest,
    DeviceIoResponse, DeviceReadRequest, DeviceReadResponse, DeviceWriteRequest, FileAttributes,
    FileEndOfFileInformation, FileFsAttributeInformation, FileInformationClass,
    FileSystemAttributes, FileSystemInformationClass, Information, MajorFunction, MinorFunction,
    NtStatus, ServerDeviceAnnounceResponse, ServerDriveIoRequest, ServerDriveSetInformationRequest,
    SharedAccess,
};
use ironrdp_rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp_rdpdr::{Rdpdr, RdpdrBackend};
use ironrdp_server::{RdpdrServerFactory, RdpdrServerMessage, ServerEvent, ServerEventSender};
use ironrdp_svc::{SvcMessage, SvcProcessor};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use warpgate_core::{FileTransferDirection, FileTransferRecord};

use crate::drive::{
    DriveLedger, DrivePolicy, DriveService, DriveUpdate, completion_header, failure,
};
use crate::server::ViewerDrives;

fn header(
    device_id: u32,
    file_id: u32,
    completion_id: u32,
    major: MajorFunction,
) -> DeviceIoRequest {
    DeviceIoRequest {
        device_id,
        file_id,
        completion_id,
        major_function: major,
        minor_function: MinorFunction::from(0),
    }
}

fn create(
    completion_id: u32,
    path: &str,
    access: DesiredAccess,
    disposition: CreateDisposition,
    options: CreateOptions,
) -> ServerDriveIoRequest {
    ServerDriveIoRequest::ServerCreateDriveRequest(DeviceCreateRequest {
        device_io_request: header(7, 0, completion_id, MajorFunction::Create),
        desired_access: access,
        allocation_size: 0,
        file_attributes: FileAttributes::empty(),
        shared_access: SharedAccess::all(),
        create_disposition: disposition,
        create_options: options,
        path: path.into(),
    })
}

fn open_for_read(completion_id: u32, path: &str) -> ServerDriveIoRequest {
    create(
        completion_id,
        path,
        DesiredAccess::GENERIC_READ,
        CreateDisposition::FILE_OPEN,
        CreateOptions::FILE_NON_DIRECTORY_FILE,
    )
}

fn read(completion_id: u32, file_id: u32, offset: u64, length: u32) -> ServerDriveIoRequest {
    ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
        device_io_request: header(7, file_id, completion_id, MajorFunction::Read),
        length,
        offset,
    })
}

fn write(completion_id: u32, file_id: u32) -> ServerDriveIoRequest {
    ServerDriveIoRequest::DeviceWriteRequest(DeviceWriteRequest {
        device_io_request: header(7, file_id, completion_id, MajorFunction::Write),
        offset: 0,
        write_data: vec![1, 2, 3],
    })
}

fn close(completion_id: u32, file_id: u32) -> ServerDriveIoRequest {
    ServerDriveIoRequest::DeviceCloseRequest(DeviceCloseRequest {
        device_io_request: header(7, file_id, completion_id, MajorFunction::Close),
    })
}

fn truncate(completion_id: u32, file_id: u32) -> ServerDriveIoRequest {
    ServerDriveIoRequest::ServerDriveSetInformationRequest(ServerDriveSetInformationRequest {
        device_io_request: header(7, file_id, completion_id, MajorFunction::SetInformation),
        set_buffer: FileInformationClass::EndOfFile(FileEndOfFileInformation { end_of_file: 0 }),
    })
}

fn device_control(completion_id: u32) -> ServerDriveIoRequest {
    ServerDriveIoRequest::DeviceControlRequest(DeviceControlRequest {
        header: header(7, 1, completion_id, MajorFunction::DeviceControl),
        output_buffer_length: 0,
        input_buffer_length: 0,
        io_control_code: ironrdp_rdpdr::pdu::efs::AnyIoCtlCode(0x0009_0028),
    })
}

const READ_ONLY: DrivePolicy = DrivePolicy { read_only: true };
const READ_WRITE: DrivePolicy = DrivePolicy { read_only: false };

#[test]
fn a_read_only_drive_only_lets_reads_and_queries_through() {
    assert_eq!(READ_ONLY.check(&open_for_read(1, r"\a.txt")), Ok(()));
    assert_eq!(READ_ONLY.check(&read(1, 1, 0, 10)), Ok(()));
    assert_eq!(READ_ONLY.check(&close(1, 1)), Ok(()));

    let denied = Err(NtStatus::MEDIA_WRITE_PROTECTED);
    for request in [
        // Every disposition but plain open can create, replace or truncate.
        create(
            1,
            r"\a",
            DesiredAccess::GENERIC_READ,
            CreateDisposition::FILE_OPEN_IF,
            CreateOptions::empty(),
        ),
        create(
            1,
            r"\a",
            DesiredAccess::GENERIC_READ,
            CreateDisposition::FILE_CREATE,
            CreateOptions::empty(),
        ),
        create(
            1,
            r"\a",
            DesiredAccess::GENERIC_READ,
            CreateDisposition::FILE_OVERWRITE_IF,
            CreateOptions::empty(),
        ),
        create(
            1,
            r"\a",
            DesiredAccess::GENERIC_READ,
            CreateDisposition::FILE_SUPERSEDE,
            CreateOptions::empty(),
        ),
        // An open asking for write or delete access.
        create(
            1,
            r"\a",
            DesiredAccess::GENERIC_WRITE,
            CreateDisposition::FILE_OPEN,
            CreateOptions::empty(),
        ),
        create(
            1,
            r"\a",
            DesiredAccess::DELETE,
            CreateDisposition::FILE_OPEN,
            CreateOptions::empty(),
        ),
        create(
            1,
            r"\a",
            DesiredAccess::FILE_WRITE_ATTRIBUTES,
            CreateDisposition::FILE_OPEN,
            CreateOptions::empty(),
        ),
        // Deleting by closing.
        create(
            1,
            r"\a",
            DesiredAccess::GENERIC_READ,
            CreateDisposition::FILE_OPEN,
            CreateOptions::FILE_DELETE_ON_CLOSE,
        ),
        write(1, 1),
        truncate(1, 1),
    ] {
        assert_eq!(READ_ONLY.check(&request), denied, "{request:?}");
        // The same requests go through when the drive is writable.
        assert_eq!(READ_WRITE.check(&request), Ok(()), "{request:?}");
    }
}

#[test]
fn device_control_never_reaches_the_drive() {
    assert_eq!(
        READ_WRITE.check(&device_control(1)),
        Err(NtStatus::NOT_SUPPORTED)
    );
    assert_eq!(
        READ_ONLY.check(&device_control(1)),
        Err(NtStatus::NOT_SUPPORTED)
    );
}

#[test]
fn every_refusal_completes_the_request_it_refuses() {
    for request in [
        open_for_read(11, r"\a"),
        read(12, 1, 0, 1),
        write(13, 1),
        close(14, 1),
        truncate(15, 1),
        device_control(16),
    ] {
        let pdu = failure(&request, NtStatus::ACCESS_DENIED);
        let reply = completion_header(&pdu).expect("a completion");
        let header = crate::drive::io_header(&request);
        assert_eq!(reply.completion_id, header.completion_id);
        assert_eq!(reply.device_id, header.device_id);
        assert_eq!(reply.io_status, NtStatus::ACCESS_DENIED);
        // And it encodes, so it can actually be sent.
        SvcMessage::from(pdu).encode_unframed_pdu().unwrap();
    }
}

#[test]
fn a_read_only_drive_reports_a_read_only_volume() {
    let mut pdu = RdpdrPdu::ClientDriveQueryVolumeInformationResponse(
        ironrdp_rdpdr::pdu::efs::ClientDriveQueryVolumeInformationResponse::new(
            header(7, 1, 1, MajorFunction::QueryVolumeInformation),
            NtStatus::SUCCESS,
            Some(FileSystemInformationClass::FileFsAttributeInformation(
                FileFsAttributeInformation {
                    file_system_attributes: FileSystemAttributes::FILE_CASE_PRESERVED_NAMES,
                    max_component_name_len: 255,
                    file_system_name: "NTFS".into(),
                },
            )),
        ),
    );
    READ_WRITE.completion(&mut pdu);
    let attributes = |pdu: &RdpdrPdu| match pdu {
        RdpdrPdu::ClientDriveQueryVolumeInformationResponse(r) => match &r.buffer {
            Some(FileSystemInformationClass::FileFsAttributeInformation(a)) => {
                a.file_system_attributes.clone()
            }
            _ => panic!(),
        },
        _ => panic!(),
    };
    assert!(!attributes(&pdu).contains(FileSystemAttributes::FILE_READ_ONLY_VOLUME));
    READ_ONLY.completion(&mut pdu);
    assert!(attributes(&pdu).contains(FileSystemAttributes::FILE_READ_ONLY_VOLUME));
}

fn created(completion_id: u32, file_id: u32) -> RdpdrPdu {
    RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
        device_io_reply: DeviceIoResponse::new(
            header(7, 0, completion_id, MajorFunction::Create),
            NtStatus::SUCCESS,
        ),
        file_id,
        information: Information::FILE_OPENED,
    })
}

fn read_back(completion_id: u32, data: &[u8]) -> RdpdrPdu {
    RdpdrPdu::DeviceReadResponse(DeviceReadResponse {
        device_io_reply: DeviceIoResponse::new(
            header(7, 0, completion_id, MajorFunction::Read),
            NtStatus::SUCCESS,
        ),
        read_data: data.to_vec(),
    })
}

fn closed(completion_id: u32) -> RdpdrPdu {
    RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
        device_io_response: DeviceIoResponse::new(
            header(7, 0, completion_id, MajorFunction::Close),
            NtStatus::SUCCESS,
        ),
    })
}

/// The ledger pairs completions with requests: a file's bytes are counted from the reads
/// that completed, and reported once, when it is closed.
#[test]
fn a_file_read_through_the_drive_is_logged_on_close() {
    let mut ledger = DriveLedger::default();
    let mut records = Vec::new();
    ledger.sent(&open_for_read(1, r"\dir\report.pdf")).unwrap();
    assert!(ledger.completed(&created(1, 5), &mut records));
    ledger.sent(&read(2, 5, 0, 4096)).unwrap();
    assert!(ledger.completed(&read_back(2, &[0; 1000]), &mut records));
    ledger.sent(&read(3, 5, 1000, 4096)).unwrap();
    assert!(ledger.completed(&read_back(3, &[0; 24]), &mut records));
    assert!(records.is_empty());
    ledger.sent(&close(4, 5)).unwrap();
    assert!(ledger.completed(&closed(4), &mut records));
    assert_eq!(
        records,
        [FileTransferRecord {
            direction: FileTransferDirection::Upload,
            path: "dir/report.pdf".into(),
            size: 1024,
        }]
    );
}

#[test]
fn completions_nothing_waits_on_are_dropped() {
    let mut ledger = DriveLedger::default();
    let mut records = Vec::new();
    assert!(!ledger.completed(&created(9, 1), &mut records));
    ledger.sent(&open_for_read(1, r"\a")).unwrap();
    assert!(ledger.completed(&created(1, 1), &mut records));
    // A second answer to the same request.
    assert!(!ledger.completed(&created(1, 1), &mut records));
    // A request reusing a completion id still in flight is refused.
    ledger.sent(&read(2, 1, 0, 1)).unwrap();
    assert_eq!(
        ledger.sent(&read(2, 1, 0, 1)),
        Err(NtStatus::INSUFFICIENT_RESOURCES)
    );
}

#[test]
fn removing_a_drive_fails_what_still_waits_on_it() {
    let mut ledger = DriveLedger::default();
    let mut records = Vec::new();
    ledger.sent(&open_for_read(1, r"\a")).unwrap();
    ledger.completed(&created(1, 3), &mut records);
    ledger.sent(&read(2, 3, 0, 10)).unwrap();
    ledger.completed(&read_back(2, b"0123456789"), &mut records);
    ledger.sent(&read(3, 3, 10, 10)).unwrap();

    let failed = ledger.removed(&[7], &mut records);
    assert_eq!(failed.len(), 1);
    let reply = completion_header(&failed[0]).unwrap();
    assert_eq!(
        (reply.completion_id, reply.io_status),
        (3, NtStatus::CANCELLED)
    );
    assert_eq!(
        records.len(),
        1,
        "what was read before the removal is still logged"
    );
    assert_eq!(records[0].size, 10);
    assert!(!ledger.completed(&read_back(3, b"x"), &mut records));
}

#[test]
fn pending_irps_are_bounded() {
    let mut ledger = DriveLedger::default();
    for id in 0..crate::drive::MAX_PENDING_IRPS as u32 {
        ledger.sent(&read(id, 1, 0, 1)).unwrap();
    }
    assert_eq!(
        ledger.sent(&read(u32::MAX, 1, 0, 1)),
        Err(NtStatus::INSUFFICIENT_RESOURCES)
    );
}

// --- the native relay over real PDUs ---------------------------------------------------

/// A viewer's drive: one file, `\a.txt`, served by its RDPDR client.
#[derive(Debug, Default)]
struct ViewerDisk {
    seen: Arc<Mutex<Vec<ServerDriveIoRequest>>>,
}

impl_as_any!(ViewerDisk);

const FILE: &[u8] = b"the quick brown fox";

impl RdpdrBackend for ViewerDisk {
    fn handle_server_device_announce_response(
        &mut self,
        _: ServerDeviceAnnounceResponse,
    ) -> PduResult<()> {
        Ok(())
    }

    fn handle_scard_call(
        &mut self,
        _: DeviceControlRequest<ScardIoCtlCode>,
        _: ScardCall,
    ) -> PduResult<Vec<SvcMessage>> {
        Ok(Vec::new())
    }

    fn handle_drive_io_request(&mut self, req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.seen.lock().unwrap().push(req.clone());
        let pdu = match &req {
            ServerDriveIoRequest::ServerCreateDriveRequest(create) => {
                RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
                    device_io_reply: DeviceIoResponse::new(
                        create.device_io_request.clone(),
                        NtStatus::SUCCESS,
                    ),
                    file_id: 42,
                    information: Information::FILE_OPENED,
                })
            }
            ServerDriveIoRequest::DeviceReadRequest(read) => {
                let start = (read.offset as usize).min(FILE.len());
                let end = (start + read.length as usize).min(FILE.len());
                RdpdrPdu::DeviceReadResponse(DeviceReadResponse {
                    device_io_reply: DeviceIoResponse::new(
                        read.device_io_request.clone(),
                        NtStatus::SUCCESS,
                    ),
                    read_data: FILE[start..end].to_vec(),
                })
            }
            ServerDriveIoRequest::DeviceCloseRequest(close) => {
                RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
                    device_io_response: DeviceIoResponse::new(
                        close.device_io_request.clone(),
                        NtStatus::SUCCESS,
                    ),
                })
            }
            other => failure(other, NtStatus::NOT_SUPPORTED),
        };
        Ok(vec![SvcMessage::from(pdu)])
    }

    fn add_drive(&mut self, _: u32) -> PduResult<()> {
        Ok(())
    }
}

struct Relay {
    viewer: Rdpdr,
    seen: Arc<Mutex<Vec<ServerDriveIoRequest>>>,
    server: ironrdp_rdpdr::server::RdpdrServer,
    drives: ViewerDrives,
    server_events: UnboundedReceiver<ServerEvent>,
    to_target: UnboundedReceiver<DriveUpdate>,
}

fn encode(messages: Vec<SvcMessage>) -> VecDeque<Vec<u8>> {
    messages
        .into_iter()
        .map(|m| m.encode_unframed_pdu().unwrap())
        .collect()
}

impl Relay {
    fn new() -> Self {
        let seen = Arc::default();
        let viewer = Rdpdr::new(
            Box::new(ViewerDisk {
                seen: Arc::clone(&seen),
            }),
            "viewer".into(),
        )
        .with_drives(Some(vec![(7, "Data".into())]));
        let mut drives = ViewerDrives::default();
        let (server_tx, server_events) = unbounded_channel();
        drives.set_sender(server_tx);
        let server = ironrdp_rdpdr::server::RdpdrServer::new(drives.build_backend());
        let (target_tx, to_target) = unbounded_channel();
        drives.attach(target_tx);
        let mut relay = Self {
            viewer,
            seen,
            server,
            drives,
            server_events,
            to_target,
        };
        let start = relay.server.start().unwrap();
        relay.pump(encode(start));
        relay
    }

    /// Deliver PDUs (server → viewer first) and forwarded IRPs until both sides go quiet.
    fn pump(&mut self, mut to_viewer: VecDeque<Vec<u8>>) {
        let mut to_server = VecDeque::new();
        for _ in 0..1000 {
            while let Ok(ServerEvent::Rdpdr(message)) = self.server_events.try_recv() {
                let RdpdrServerMessage::Forward(request) = message else {
                    panic!("only forwards are expected");
                };
                to_viewer.extend(encode(
                    self.server.forward_drive_io_request(request).unwrap(),
                ));
            }
            if to_viewer.is_empty() && to_server.is_empty() {
                return;
            }
            while let Some(pdu) = to_viewer.pop_front() {
                to_server.extend(encode(self.viewer.process(&pdu).unwrap()));
            }
            while let Some(pdu) = to_server.pop_front() {
                to_viewer.extend(encode(self.server.process(&pdu).unwrap()));
            }
        }
        panic!("RDPDR endpoints never went quiet");
    }

    fn target_request(&mut self, request: ServerDriveIoRequest) -> RdpdrPdu {
        self.drives.request(request);
        self.pump(VecDeque::new());
        match self.to_target.try_recv() {
            Ok(DriveUpdate::Completion(pdu)) => pdu,
            other => panic!("expected a completion, got {other:?}"),
        }
    }
}

/// A viewer's drive announced to Warpgate reaches the target side, and the target's
/// IRPs reach the viewer with their ids intact, as do the viewer's answers coming back.
#[test]
fn viewer_drives_are_relayed_irp_by_irp() {
    let mut relay = Relay::new();
    match relay.to_target.try_recv() {
        Ok(DriveUpdate::Announce(drives)) => assert_eq!(drives, [(7, "Data".to_owned())]),
        other => panic!("expected the viewer's drive, got {other:?}"),
    }

    let RdpdrPdu::DeviceCreateResponse(created) =
        relay.target_request(open_for_read(1000, r"\a.txt"))
    else {
        panic!("expected a create completion");
    };
    assert_eq!(created.device_io_reply.completion_id, 1000);
    assert_eq!(created.device_io_reply.device_id, 7);
    assert_eq!(created.file_id, 42);
    // The viewer saw the target's request as sent, attributes and share mode included.
    let seen = relay.seen.lock().unwrap().clone();
    let ServerDriveIoRequest::ServerCreateDriveRequest(seen) = &seen[0] else {
        panic!("{seen:?}");
    };
    assert_eq!(seen.path, r"\a.txt");
    assert_eq!(seen.device_io_request.completion_id, 1000);
    assert_eq!(seen.shared_access, SharedAccess::all());

    let RdpdrPdu::DeviceReadResponse(data) = relay.target_request(read(1001, 42, 4, 5)) else {
        panic!("expected a read completion");
    };
    assert_eq!(data.device_io_reply.completion_id, 1001);
    assert_eq!(data.read_data, b"quick");
}

/// The relay re-encodes rather than passes bytes through, so a request the RDPDR server
/// cannot forward — here, a device the viewer never announced — comes back as a failure
/// for the target instead of hanging it or ending the channel.
#[test]
fn an_irp_for_an_unannounced_device_fails_back_to_the_target() {
    let mut relay = Relay::new();
    let _ = relay.to_target.try_recv();
    let mut request = open_for_read(5, r"\a.txt");
    if let ServerDriveIoRequest::ServerCreateDriveRequest(create) = &mut request {
        create.device_io_request.device_id = 99;
    }
    let pdu = relay.target_request(request);
    let reply = completion_header(&pdu).unwrap();
    assert_eq!(
        (reply.completion_id, reply.io_status),
        (5, NtStatus::UNSUCCESSFUL)
    );
    assert!(relay.seen.lock().unwrap().is_empty());
}

/// Two in-flight requests with the same completion id would make the second answer
/// ambiguous; the server refuses the duplicate.
#[test]
fn a_duplicate_completion_id_is_refused() {
    let mut relay = Relay::new();
    let _ = relay.to_target.try_recv();
    relay
        .server
        .forward_drive_io_request(open_for_read(7, r"\a.txt"))
        .unwrap();
    // Not delivered to the viewer yet, so 7 is still pending.
    let pdu = relay.target_request(open_for_read(7, r"\b.txt"));
    let reply = completion_header(&pdu).unwrap();
    assert_eq!(reply.io_status, NtStatus::UNSUCCESSFUL);
}

// --- the target side over real PDUs ----------------------------------------------------

/// What a target (standing in for Windows) got back for the IRPs it sent.
#[derive(Debug, Default)]
struct TargetSeen {
    created: Vec<(u32, NtStatus)>,
    read: Vec<Vec<u8>>,
    devices: Vec<u32>,
}

#[derive(Debug, Default)]
struct TargetBackend(Arc<Mutex<TargetSeen>>);

impl_as_any!(TargetBackend);

macro_rules! ignore_completions {
    ($($name:ident: $ty:ty),+ $(,)?) => {
        $(fn $name(&mut self, _: &$ty) -> PduResult<()> {
            Ok(())
        })+
    };
}

impl ironrdp_server::RdpdrServerBackend for TargetBackend {
    fn on_device_announce(
        &mut self,
        devices: &[ironrdp_rdpdr::pdu::efs::DeviceAnnounceHeader],
    ) -> Vec<(u32, bool)> {
        let ids: Vec<u32> = devices.iter().map(|d| d.device_id()).collect();
        self.0.lock().unwrap().devices.extend(&ids);
        ids.into_iter().map(|id| (id, true)).collect()
    }

    fn on_device_remove(&mut self, _: &[u32]) {}

    fn on_client_name(&mut self, _: &str) {}

    fn on_create_complete(&mut self, response: &DeviceCreateResponse) -> PduResult<()> {
        self.0
            .lock()
            .unwrap()
            .created
            .push((response.file_id, response.device_io_reply.io_status));
        Ok(())
    }

    fn on_read_complete(&mut self, response: &DeviceReadResponse) -> PduResult<()> {
        self.0.lock().unwrap().read.push(response.read_data.clone());
        Ok(())
    }

    ignore_completions!(
        on_close_complete: DeviceCloseResponse,
        on_write_complete: ironrdp_rdpdr::pdu::efs::DeviceWriteResponse,
        on_flush_buffers_complete: ironrdp_rdpdr::pdu::efs::DeviceFlushBuffersResponse,
        on_device_control_complete: ironrdp_rdpdr::pdu::efs::DeviceControlResponse,
        on_query_information_complete: ironrdp_rdpdr::pdu::efs::ClientDriveQueryInformationResponse,
        on_set_information_complete: ironrdp_rdpdr::pdu::efs::ClientDriveSetInformationResponse,
        on_query_directory_complete: ironrdp_rdpdr::pdu::efs::ClientDriveQueryDirectoryResponse,
        on_notify_change_directory_complete: ironrdp_rdpdr::pdu::efs::ClientDriveNotifyChangeDirectoryResponse,
        on_query_volume_information_complete: ironrdp_rdpdr::pdu::efs::ClientDriveQueryVolumeInformationResponse,
        on_lock_control_complete: ironrdp_rdpdr::pdu::efs::ClientDriveLockControlResponse,
        on_query_security_complete: ironrdp_rdpdr::pdu::efs::ClientDriveQuerySecurityResponse,
        on_set_security_complete: ironrdp_rdpdr::pdu::efs::ClientDriveSetSecurityResponse,
    );
}

/// The browser's session drive, end to end on the target side: Warpgate's RDPDR client
/// announces it, and the target's opens and reads are answered from the uploads, with
/// writes refused before the drive is consulted.
#[test]
fn the_target_reads_the_session_drive_over_rdpdr() {
    use crate::drive::{SharedLedger, TargetDrives, ledger};
    use crate::drive_uploads::{UPLOADS_DEVICE_ID, UploadDrive};

    let uploads = UploadDrive::new(1024);
    uploads.upload(warpgate_core::DriveUpload::Start {
        id: 1,
        name: "notes.txt".into(),
        size: 5,
    });
    uploads.upload(warpgate_core::DriveUpload::Chunk {
        id: 1,
        data: bytes::Bytes::from_static(b"hello"),
    });
    let ledger_state = SharedLedger::default();
    let (updates_tx, mut updates) = unbounded_channel();
    uploads.attach(updates_tx);
    let mut client = Rdpdr::new(
        Box::new(TargetDrives::new(
            Arc::new(uploads.clone()),
            READ_ONLY,
            Arc::clone(&ledger_state),
        )),
        "Warpgate".into(),
    )
    .with_drives(None);
    let seen = Arc::new(Mutex::new(TargetSeen::default()));
    let mut target =
        ironrdp_rdpdr::server::RdpdrServer::new(Box::new(TargetBackend(Arc::clone(&seen))));

    // What `DriveLink::apply` does with each update, minus the transport.
    let mut pump = |client: &mut Rdpdr,
                    target: &mut ironrdp_rdpdr::server::RdpdrServer,
                    mut to_client: VecDeque<Vec<u8>>| {
        for _ in 0..1000 {
            let mut to_target = VecDeque::new();
            while let Some(pdu) = to_client.pop_front() {
                to_target.extend(encode(client.process(&pdu).unwrap()));
            }
            while let Ok(update) = updates.try_recv() {
                let messages = match update {
                    DriveUpdate::Announce(drives) => drives
                        .into_iter()
                        .flat_map(|(id, name)| client.add_dynamic_drive(id, name).unwrap())
                        .collect(),
                    DriveUpdate::Completion(mut pdu) => {
                        let mut records = Vec::new();
                        assert!(ledger(&ledger_state).completed(&pdu, &mut records));
                        READ_ONLY.completion(&mut pdu);
                        vec![SvcMessage::from(pdu)]
                    }
                    DriveUpdate::Remove(_) => Vec::new(),
                };
                to_target.extend(encode(messages));
            }
            if to_target.is_empty() {
                return;
            }
            while let Some(pdu) = to_target.pop_front() {
                to_client.extend(encode(target.process(&pdu).unwrap()));
            }
        }
        panic!("never went quiet");
    };

    let start = encode(target.start().unwrap());
    pump(&mut client, &mut target, start);
    assert_eq!(seen.lock().unwrap().devices, [UPLOADS_DEVICE_ID]);

    let open = encode(
        target
            .drive_create(
                UPLOADS_DEVICE_ID,
                r"\notes.txt",
                DesiredAccess::GENERIC_READ,
                CreateDisposition::FILE_OPEN,
                CreateOptions::FILE_NON_DIRECTORY_FILE,
            )
            .unwrap(),
    );
    pump(&mut client, &mut target, open);
    let (file_id, status) = seen.lock().unwrap().created[0];
    assert_eq!(status, NtStatus::SUCCESS);

    let read = encode(
        target
            .drive_read(UPLOADS_DEVICE_ID, file_id, 100, 1)
            .unwrap(),
    );
    pump(&mut client, &mut target, read);
    assert_eq!(seen.lock().unwrap().read, [b"ello".to_vec()]);

    // Refused by the policy on the way in: the drive never sees it.
    let overwrite = encode(
        target
            .drive_create(
                UPLOADS_DEVICE_ID,
                r"\notes.txt",
                DesiredAccess::GENERIC_WRITE,
                CreateDisposition::FILE_OVERWRITE_IF,
                CreateOptions::empty(),
            )
            .unwrap(),
    );
    pump(&mut client, &mut target, overwrite);
    assert_eq!(
        seen.lock().unwrap().created[1].1,
        NtStatus::MEDIA_WRITE_PROTECTED
    );
}
