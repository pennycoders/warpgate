//! The browser session drive: uploads within its capacity, and the IRPs a target sends
//! to browse and read it.

use bytes::Bytes;
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_rdpdr::pdu::efs::{
    CreateDisposition, CreateOptions, DesiredAccess, DeviceCreateRequest, DeviceIoRequest,
    DeviceReadRequest, FileAttributes, FileInformationClass, FileInformationClassLevel,
    FileSystemAttributes, FileSystemInformationClass, FileSystemInformationClassLevel,
    MajorFunction, MinorFunction, NtStatus, ServerDriveIoRequest, ServerDriveQueryDirectoryRequest,
    ServerDriveQueryVolumeInformationRequest, SharedAccess,
};
use warpgate_core::{DriveUpload, DriveUploadStatus};

use super::{UPLOADS_DEVICE_ID, UploadDrive};
use crate::drive::completion_header;

fn header(file_id: u32, completion_id: u32, major: MajorFunction) -> DeviceIoRequest {
    DeviceIoRequest {
        device_id: UPLOADS_DEVICE_ID,
        file_id,
        completion_id,
        major_function: major,
        minor_function: MinorFunction::from(0),
    }
}

fn upload(drive: &UploadDrive, id: u32, name: &str, data: &[u8]) -> Option<DriveUploadStatus> {
    let started = drive.upload(DriveUpload::Start {
        id,
        name: name.into(),
        size: data.len() as u64,
    });
    if started.is_some() {
        return started;
    }
    let mut status = None;
    for chunk in data.chunks(3) {
        status = drive.upload(DriveUpload::Chunk {
            id,
            data: Bytes::copy_from_slice(chunk),
        });
    }
    status
}

fn open(
    drive: &UploadDrive,
    path: &str,
    disposition: CreateDisposition,
    options: CreateOptions,
) -> RdpdrPdu {
    drive.respond(ServerDriveIoRequest::ServerCreateDriveRequest(
        DeviceCreateRequest {
            device_io_request: header(0, 1, MajorFunction::Create),
            desired_access: DesiredAccess::GENERIC_READ,
            allocation_size: 0,
            file_attributes: FileAttributes::empty(),
            shared_access: SharedAccess::all(),
            create_disposition: disposition,
            create_options: options,
            path: path.into(),
        },
    ))
}

fn opened(pdu: &RdpdrPdu) -> u32 {
    match pdu {
        RdpdrPdu::DeviceCreateResponse(r) if r.device_io_reply.io_status == NtStatus::SUCCESS => {
            r.file_id
        }
        other => panic!("expected a successful open, got {other:?}"),
    }
}

fn status(pdu: &RdpdrPdu) -> NtStatus {
    completion_header(pdu).unwrap().io_status
}

#[test]
fn uploads_are_stored_within_capacity() {
    let drive = UploadDrive::new(10);
    assert_eq!(
        upload(&drive, 1, "a.txt", b"hello"),
        Some(DriveUploadStatus {
            id: 1,
            stored: true
        })
    );
    assert_eq!(
        upload(&drive, 2, "b.txt", b"123456"),
        Some(DriveUploadStatus {
            id: 2,
            stored: false
        }),
        "over capacity"
    );
    // Replacing a file frees its space first.
    assert_eq!(
        upload(&drive, 3, "A.TXT", b"0123456789"),
        Some(DriveUploadStatus {
            id: 3,
            stored: true
        })
    );
    assert_eq!(
        upload(&drive, 4, "more", b"x"),
        Some(DriveUploadStatus {
            id: 4,
            stored: false
        }),
        "drive is full"
    );
}

#[test]
fn uploads_with_unusable_names_are_refused() {
    let drive = UploadDrive::new(100);
    for name in ["../x", "dir/x", r"dir\x", "con", "a:b", ""] {
        assert_eq!(
            upload(&drive, 1, name, b"x"),
            Some(DriveUploadStatus {
                id: 1,
                stored: false
            }),
            "{name:?}"
        );
    }
}

#[test]
fn an_upload_cannot_overrun_its_declared_size() {
    let drive = UploadDrive::new(100);
    assert_eq!(
        drive.upload(DriveUpload::Start {
            id: 1,
            name: "a".into(),
            size: 2
        }),
        None
    );
    assert_eq!(
        drive.upload(DriveUpload::Chunk {
            id: 1,
            data: Bytes::from_static(b"abc")
        }),
        Some(DriveUploadStatus {
            id: 1,
            stored: false
        })
    );
    // Chunks for an upload that isn't running are refused too.
    assert_eq!(
        drive.upload(DriveUpload::Chunk {
            id: 9,
            data: Bytes::from_static(b"a")
        }),
        Some(DriveUploadStatus {
            id: 9,
            stored: false
        })
    );
}

#[test]
fn the_target_can_list_and_read_uploads() {
    let drive = UploadDrive::new(100);
    upload(&drive, 1, "notes.txt", b"hello world");
    upload(&drive, 2, "photo.jpg", b"\xff\xd8");

    let root = opened(&open(
        &drive,
        r"\",
        CreateDisposition::FILE_OPEN,
        CreateOptions::FILE_DIRECTORY_FILE,
    ));
    let list = |initial: bool, path: &str| {
        drive.respond(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
            ServerDriveQueryDirectoryRequest {
                device_io_request: header(root, 2, MajorFunction::DirectoryControl),
                file_info_class_lvl: FileInformationClassLevel::FILE_BOTH_DIRECTORY_INFORMATION,
                initial_query: u8::from(initial),
                path: path.into(),
            },
        ))
    };
    let name = |pdu: RdpdrPdu| match pdu {
        RdpdrPdu::ClientDriveQueryDirectoryResponse(r) => match r.buffer {
            Some(FileInformationClass::BothDirectory(entry)) => Some(entry.file_name),
            _ => None,
        },
        _ => None,
    };
    assert_eq!(name(list(true, r"\*")).as_deref(), Some("notes.txt"));
    assert_eq!(name(list(false, "")).as_deref(), Some("photo.jpg"));
    assert_eq!(status(&list(false, "")), NtStatus::NO_MORE_FILES);
    assert_eq!(name(list(true, r"\*.jpg")).as_deref(), Some("photo.jpg"));
    assert_eq!(status(&list(true, r"\missing")), NtStatus::NO_SUCH_FILE);

    let file = opened(&open(
        &drive,
        r"\NOTES.TXT",
        CreateDisposition::FILE_OPEN,
        CreateOptions::FILE_NON_DIRECTORY_FILE,
    ));
    let read = drive.respond(ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
        device_io_request: header(file, 3, MajorFunction::Read),
        length: 5,
        offset: 6,
    }));
    match read {
        RdpdrPdu::DeviceReadResponse(r) => assert_eq!(r.read_data, b"world"),
        other => panic!("{other:?}"),
    }

    // An open handle keeps reading what it opened even after the file is replaced.
    upload(&drive, 3, "notes.txt", b"replaced");
    let read = drive.respond(ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
        device_io_request: header(file, 4, MajorFunction::Read),
        length: 100,
        offset: 0,
    }));
    match read {
        RdpdrPdu::DeviceReadResponse(r) => assert_eq!(r.read_data, b"hello world"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_session_drive_cannot_be_written() {
    let drive = UploadDrive::new(100);
    upload(&drive, 1, "a.txt", b"a");
    for disposition in [
        CreateDisposition::FILE_CREATE,
        CreateDisposition::FILE_OVERWRITE,
        CreateDisposition::FILE_OVERWRITE_IF,
        CreateDisposition::FILE_SUPERSEDE,
    ] {
        assert_eq!(
            status(&open(
                &drive,
                r"\a.txt",
                disposition,
                CreateOptions::empty()
            )),
            NtStatus::MEDIA_WRITE_PROTECTED,
            "{disposition:?}"
        );
    }
    assert_eq!(
        status(&open(
            &drive,
            r"\new.txt",
            CreateDisposition::FILE_CREATE,
            CreateOptions::empty()
        )),
        NtStatus::NO_SUCH_FILE
    );
    assert_eq!(
        status(&open(
            &drive,
            r"\dir\a.txt",
            CreateDisposition::FILE_OPEN,
            CreateOptions::empty()
        )),
        NtStatus::OBJECT_PATH_NOT_FOUND
    );
    let file = opened(&open(
        &drive,
        r"\a.txt",
        CreateDisposition::FILE_OPEN,
        CreateOptions::empty(),
    ));
    let write = drive.respond(ServerDriveIoRequest::DeviceWriteRequest(
        ironrdp_rdpdr::pdu::efs::DeviceWriteRequest {
            device_io_request: header(file, 5, MajorFunction::Write),
            offset: 0,
            write_data: b"x".to_vec(),
        },
    ));
    assert_eq!(status(&write), NtStatus::MEDIA_WRITE_PROTECTED);

    let volume = drive.respond(
        ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(
            ServerDriveQueryVolumeInformationRequest {
                device_io_request: header(file, 6, MajorFunction::QueryVolumeInformation),
                fs_info_class_lvl: FileSystemInformationClassLevel::FILE_FS_ATTRIBUTE_INFORMATION,
            },
        ),
    );
    match volume {
        RdpdrPdu::ClientDriveQueryVolumeInformationResponse(r) => match r.buffer {
            Some(FileSystemInformationClass::FileFsAttributeInformation(a)) => {
                assert!(
                    a.file_system_attributes
                        .contains(FileSystemAttributes::FILE_READ_ONLY_VOLUME)
                );
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
