//! The browser desktop's redirected drive: files the user uploads during the session,
//! held by Warpgate and served to the target as a flat, read-only drive.
//!
//! Only what Explorer and the common file dialogs need is answered: opening the root and
//! the files in it, reading, and the directory, file and volume queries they issue.
//! Everything that would change the drive is refused, before [`crate::drive::DrivePolicy`]
//! is even consulted.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_rdpdr::pdu::efs::{
    Boolean, Characteristics, ClientDriveQueryDirectoryResponse,
    ClientDriveQueryInformationResponse, ClientDriveQueryVolumeInformationResponse,
    CreateDisposition, CreateOptions, DeviceCloseResponse, DeviceCreateRequest,
    DeviceCreateResponse, DeviceIoRequest, DeviceIoResponse, DeviceReadResponse,
    FileAttributeTagInformation, FileAttributes, FileBasicInformation,
    FileBothDirectoryInformation, FileDirectoryInformation, FileFsAttributeInformation,
    FileFsDeviceInformation, FileFsFullSizeInformation, FileFsSizeInformation,
    FileFsVolumeInformation, FileFullDirectoryInformation, FileInformationClass,
    FileInformationClassLevel, FileNamesInformation, FileStandardInformation, FileSystemAttributes,
    FileSystemInformationClass, FileSystemInformationClassLevel, Information, NtStatus,
    ServerDriveIoRequest, ServerDriveQueryDirectoryRequest,
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};
use warpgate_core::{DriveUpload, DriveUploadStatus, sanitize_file_path};

use crate::drive::{DriveService, DriveUpdate, failure};

/// The device id the uploads drive is announced under.
pub(crate) const UPLOADS_DEVICE_ID: u32 = 1;
const UPLOADS_NAME: &str = "Uploads";

/// The most files the drive holds; uploading more replaces the oldest.
pub(crate) const MAX_UPLOADED_FILES: usize = 256;

/// `FILE_DEVICE_DISK`.
const FILE_DEVICE_DISK: u32 = 0x0000_0007;

#[derive(Debug, Clone)]
struct Uploaded {
    id: u32,
    name: String,
    data: Bytes,
    /// FILETIME of the upload.
    time: i64,
}

#[derive(Debug)]
struct Partial {
    id: u32,
    name: String,
    size: u64,
    data: BytesMut,
}

#[derive(Debug)]
enum Handle {
    Root {
        /// The next entry a directory query continues from, and the pattern it matches.
        cursor: usize,
        pattern: Option<String>,
    },
    /// The file as it was when opened: a later upload of the same name doesn't change
    /// what an open handle reads.
    File(Uploaded),
}

#[derive(Debug, Default)]
struct State {
    max_bytes: u64,
    files: Vec<Uploaded>,
    partial: Option<Partial>,
    /// Open handles by file id.
    open: Vec<(u32, Handle)>,
    next_file_id: u32,
    updates: Option<UnboundedSender<DriveUpdate>>,
}

impl State {
    fn used(&self) -> u64 {
        let stored: u64 = self.files.iter().map(|f| f.data.len() as u64).sum();
        stored + self.partial.as_ref().map_or(0, |p| p.size)
    }
}

/// The uploads drive. Cloning shares it.
#[derive(Debug, Clone, Default)]
pub(crate) struct UploadDrive(Arc<Mutex<State>>);

fn filetime_now() -> i64 {
    // 100 ns intervals since 1601-01-01.
    const EPOCH_DIFFERENCE: i64 = 11_644_473_600;
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    (unix + EPOCH_DIFFERENCE) * 10_000_000
}

impl UploadDrive {
    pub(crate) fn new(max_bytes: u64) -> Self {
        Self(Arc::new(Mutex::new(State {
            max_bytes,
            next_file_id: 1,
            ..State::default()
        })))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A step of a browser upload. Returns the status to report, once the upload is
    /// stored or refused.
    pub(crate) fn upload(&self, step: DriveUpload) -> Option<DriveUploadStatus> {
        let mut state = self.state();
        match step {
            DriveUpload::Start { id, name, size } => {
                // A new upload abandons one left unfinished.
                state.partial = None;
                let name = sanitize_file_path(&name).filter(|name| !name.contains('/'));
                let Some(name) = name else {
                    return Some(DriveUploadStatus { id, stored: false });
                };
                // Replacing a file of the same name frees its space first.
                let replaced: u64 = state
                    .files
                    .iter()
                    .filter(|f| f.name.eq_ignore_ascii_case(&name))
                    .map(|f| f.data.len() as u64)
                    .sum();
                if state.used().saturating_sub(replaced).saturating_add(size) > state.max_bytes {
                    return Some(DriveUploadStatus { id, stored: false });
                }
                let capacity = usize::try_from(size).unwrap_or(0).min(16 * 1024 * 1024);
                state.partial = Some(Partial {
                    id,
                    name,
                    size,
                    data: BytesMut::with_capacity(capacity),
                });
                if size == 0 {
                    return Self::finish(&mut state);
                }
                None
            }
            DriveUpload::Chunk { id, data } => {
                let Some(partial) = state.partial.as_mut().filter(|p| p.id == id) else {
                    return Some(DriveUploadStatus { id, stored: false });
                };
                if partial.data.len() as u64 + data.len() as u64 > partial.size {
                    state.partial = None;
                    return Some(DriveUploadStatus { id, stored: false });
                }
                partial.data.extend_from_slice(&data);
                if partial.data.len() as u64 == partial.size {
                    return Self::finish(&mut state);
                }
                None
            }
            DriveUpload::Delete { id } => {
                state.files.retain(|f| f.id != id);
                None
            }
        }
    }

    fn finish(state: &mut State) -> Option<DriveUploadStatus> {
        let partial = state.partial.take()?;
        state
            .files
            .retain(|f| !f.name.eq_ignore_ascii_case(&partial.name));
        if state.files.len() >= MAX_UPLOADED_FILES {
            state.files.remove(0);
        }
        info!(name = %partial.name, size = partial.size, "File uploaded to the session drive");
        state.files.push(Uploaded {
            id: partial.id,
            name: partial.name,
            data: partial.data.freeze(),
            time: filetime_now(),
        });
        Some(DriveUploadStatus {
            id: partial.id,
            stored: true,
        })
    }

    fn respond(&self, request: ServerDriveIoRequest) -> RdpdrPdu {
        let mut state = self.state();
        match request {
            ServerDriveIoRequest::ServerCreateDriveRequest(create) => {
                create_file(&mut state, create)
            }
            ServerDriveIoRequest::DeviceCloseRequest(close) => {
                let file_id = close.device_io_request.file_id;
                let known = state.open.iter().any(|(id, _)| *id == file_id);
                state.open.retain(|(id, _)| *id != file_id);
                let status = if known {
                    NtStatus::SUCCESS
                } else {
                    NtStatus::INVALID_HANDLE
                };
                RdpdrPdu::DeviceCloseResponse(DeviceCloseResponse {
                    device_io_response: DeviceIoResponse::new(close.device_io_request, status),
                })
            }
            ServerDriveIoRequest::DeviceReadRequest(read) => {
                let header = read.device_io_request.clone();
                let file = handle(&state, header.file_id).and_then(|handle| match handle {
                    Handle::File(file) => Some(file),
                    Handle::Root { .. } => None,
                });
                let Some(file) = file else {
                    return failure(
                        &ServerDriveIoRequest::DeviceReadRequest(read),
                        NtStatus::INVALID_HANDLE,
                    );
                };
                let start = usize::try_from(read.offset)
                    .unwrap_or(usize::MAX)
                    .min(file.data.len());
                let end = start
                    .saturating_add(read.length as usize)
                    .min(file.data.len());
                RdpdrPdu::DeviceReadResponse(DeviceReadResponse {
                    device_io_reply: DeviceIoResponse::new(header, NtStatus::SUCCESS),
                    read_data: file.data.slice(start..end).to_vec(),
                })
            }
            ServerDriveIoRequest::ServerDriveQueryInformationRequest(query) => {
                let header = query.device_io_request.clone();
                let Some(entry) = entry_for(&state, header.file_id) else {
                    return failure(
                        &ServerDriveIoRequest::ServerDriveQueryInformationRequest(query),
                        NtStatus::INVALID_HANDLE,
                    );
                };
                let buffer = if query.file_info_class_lvl
                    == FileInformationClassLevel::FILE_BASIC_INFORMATION
                {
                    FileInformationClass::Basic(FileBasicInformation {
                        creation_time: entry.time,
                        last_access_time: entry.time,
                        last_write_time: entry.time,
                        change_time: entry.time,
                        file_attributes: entry.attributes,
                    })
                } else if query.file_info_class_lvl
                    == FileInformationClassLevel::FILE_STANDARD_INFORMATION
                {
                    FileInformationClass::Standard(FileStandardInformation {
                        allocation_size: entry.size,
                        end_of_file: entry.size,
                        number_of_links: 1,
                        delete_pending: Boolean::False,
                        directory: if entry.directory {
                            Boolean::True
                        } else {
                            Boolean::False
                        },
                    })
                } else if query.file_info_class_lvl
                    == FileInformationClassLevel::FILE_ATTRIBUTE_TAG_INFORMATION
                {
                    FileInformationClass::AttributeTag(FileAttributeTagInformation {
                        file_attributes: entry.attributes,
                        reparse_tag: 0,
                    })
                } else {
                    return failure(
                        &ServerDriveIoRequest::ServerDriveQueryInformationRequest(query),
                        NtStatus::NOT_SUPPORTED,
                    );
                };
                RdpdrPdu::ClientDriveQueryInformationResponse(ClientDriveQueryInformationResponse {
                    device_io_response: DeviceIoResponse::new(header, NtStatus::SUCCESS),
                    buffer: Some(buffer),
                })
            }
            ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(query) => {
                query_directory(&mut state, query)
            }
            ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(query) => {
                let header = query.device_io_request.clone();
                let total = i64::try_from(state.max_bytes / 4096)
                    .unwrap_or(i64::MAX)
                    .max(1);
                let free =
                    i64::try_from(state.max_bytes.saturating_sub(state.used()) / 4096).unwrap_or(0);
                let class = &query.fs_info_class_lvl;
                let buffer = if *class
                    == FileSystemInformationClassLevel::FILE_FS_VOLUME_INFORMATION
                {
                    FileSystemInformationClass::FileFsVolumeInformation(FileFsVolumeInformation {
                        volume_creation_time: 0,
                        volume_serial_number: 0x5741_5250,
                        supports_objects: Boolean::False,
                        volume_label: UPLOADS_NAME.to_owned(),
                    })
                } else if *class == FileSystemInformationClassLevel::FILE_FS_SIZE_INFORMATION {
                    FileSystemInformationClass::FileFsSizeInformation(FileFsSizeInformation {
                        total_alloc_units: total,
                        available_alloc_units: free,
                        sectors_per_alloc_unit: 8,
                        bytes_per_sector: 512,
                    })
                } else if *class == FileSystemInformationClassLevel::FILE_FS_FULL_SIZE_INFORMATION {
                    FileSystemInformationClass::FileFsFullSizeInformation(
                        FileFsFullSizeInformation {
                            total_alloc_units: total,
                            caller_available_alloc_units: free,
                            actual_available_alloc_units: free,
                            sectors_per_alloc_unit: 8,
                            bytes_per_sector: 512,
                        },
                    )
                } else if *class == FileSystemInformationClassLevel::FILE_FS_ATTRIBUTE_INFORMATION {
                    FileSystemInformationClass::FileFsAttributeInformation(
                        FileFsAttributeInformation {
                            file_system_attributes: FileSystemAttributes::FILE_CASE_PRESERVED_NAMES
                                | FileSystemAttributes::FILE_UNICODE_ON_DISK
                                | FileSystemAttributes::FILE_READ_ONLY_VOLUME,
                            max_component_name_len: 255,
                            file_system_name: "FAT32".to_owned(),
                        },
                    )
                } else if *class == FileSystemInformationClassLevel::FILE_FS_DEVICE_INFORMATION {
                    FileSystemInformationClass::FileFsDeviceInformation(FileFsDeviceInformation {
                        device_type: FILE_DEVICE_DISK,
                        characteristics: Characteristics::FILE_REMOTE_DEVICE
                            | Characteristics::FILE_READ_ONLY_DEVICE,
                    })
                } else {
                    return failure(
                        &ServerDriveIoRequest::ServerDriveQueryVolumeInformationRequest(query),
                        NtStatus::NOT_SUPPORTED,
                    );
                };
                RdpdrPdu::ClientDriveQueryVolumeInformationResponse(
                    ClientDriveQueryVolumeInformationResponse::new(
                        header,
                        NtStatus::SUCCESS,
                        Some(buffer),
                    ),
                )
            }
            // A flat drive whose contents change only by upload: change notifications
            // are not offered, so Explorer refreshes on F5.
            request @ ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(_) => {
                failure(&request, NtStatus::NOT_SUPPORTED)
            }
            request @ (ServerDriveIoRequest::DeviceFlushBuffersRequest(_)
            | ServerDriveIoRequest::ServerDriveLockControlRequest(_)) => {
                let status = if handle(&state, io_header_of(&request).file_id).is_some() {
                    NtStatus::SUCCESS
                } else {
                    NtStatus::INVALID_HANDLE
                };
                failure(&request, status)
            }
            request => failure(&request, NtStatus::MEDIA_WRITE_PROTECTED),
        }
    }
}

const fn io_header_of(request: &ServerDriveIoRequest) -> &DeviceIoRequest {
    crate::drive::io_header(request)
}

fn handle(state: &State, file_id: u32) -> Option<&Handle> {
    state
        .open
        .iter()
        .find(|(id, _)| *id == file_id)
        .map(|(_, handle)| handle)
}

struct Entry {
    time: i64,
    size: i64,
    attributes: FileAttributes,
    directory: bool,
}

fn file_entry(file: &Uploaded) -> Entry {
    Entry {
        time: file.time,
        size: i64::try_from(file.data.len()).unwrap_or(i64::MAX),
        attributes: FileAttributes::FILE_ATTRIBUTE_READONLY
            | FileAttributes::FILE_ATTRIBUTE_ARCHIVE,
        directory: false,
    }
}

fn entry_for(state: &State, file_id: u32) -> Option<Entry> {
    match handle(state, file_id)? {
        Handle::Root { .. } => Some(Entry {
            time: 0,
            size: 0,
            attributes: FileAttributes::FILE_ATTRIBUTE_DIRECTORY
                | FileAttributes::FILE_ATTRIBUTE_READONLY,
            directory: true,
        }),
        Handle::File(file) => Some(file_entry(file)),
    }
}

/// `\name` → `name`; `\` or empty → the root (`""`).
fn relative(path: &str) -> &str {
    path.trim_start_matches('\\')
}

fn create_file(state: &mut State, create: DeviceCreateRequest) -> RdpdrPdu {
    let reply = |status, file_id, information| {
        RdpdrPdu::DeviceCreateResponse(DeviceCreateResponse {
            device_io_reply: DeviceIoResponse::new(create.device_io_request.clone(), status),
            file_id,
            information,
        })
    };
    let name = relative(&create.path);
    let handle = if name.is_empty() {
        if create
            .create_options
            .contains(CreateOptions::FILE_NON_DIRECTORY_FILE)
        {
            return reply(NtStatus::FILE_IS_A_DIRECTORY, 0, Information::empty());
        }
        Handle::Root {
            cursor: 0,
            pattern: None,
        }
    } else {
        let file = state
            .files
            .iter()
            .find(|f| f.name.eq_ignore_ascii_case(name));
        match file {
            Some(_)
                if create
                    .create_options
                    .contains(CreateOptions::FILE_DIRECTORY_FILE) =>
            {
                return reply(NtStatus::NOT_A_DIRECTORY, 0, Information::empty());
            }
            Some(file) => Handle::File(file.clone()),
            // The drive is flat: anything with a separator is under a directory that
            // doesn't exist.
            None if name.contains('\\') => {
                return reply(NtStatus::OBJECT_PATH_NOT_FOUND, 0, Information::empty());
            }
            None => return reply(NtStatus::NO_SUCH_FILE, 0, Information::empty()),
        }
    };
    if create.create_disposition != CreateDisposition::FILE_OPEN
        && create.create_disposition != CreateDisposition::FILE_OPEN_IF
    {
        return reply(NtStatus::MEDIA_WRITE_PROTECTED, 0, Information::empty());
    }
    if state.open.len() >= 1024 {
        return reply(NtStatus::INSUFFICIENT_RESOURCES, 0, Information::empty());
    }
    let file_id = state.next_file_id;
    state.next_file_id = state.next_file_id.wrapping_add(1).max(1);
    state.open.push((file_id, handle));
    reply(NtStatus::SUCCESS, file_id, Information::FILE_OPENED)
}

/// Whether `name` matches a directory query pattern: `*`, `*.*`, an exact name, or a
/// `*`-wildcard such as `*.txt`. Case-insensitive, as on Windows.
fn matches(pattern: &str, name: &str) -> bool {
    if pattern == "*" || pattern == "*.*" {
        return true;
    }
    let pattern = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    match pattern.split_once('*') {
        None => pattern == name,
        Some((prefix, suffix)) if !suffix.contains('*') => {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        }
        // More than one wildcard: answer with everything and let the target filter.
        Some(_) => true,
    }
}

fn query_directory(state: &mut State, query: ServerDriveQueryDirectoryRequest) -> RdpdrPdu {
    let header = query.device_io_request.clone();
    let initial = query.initial_query != 0;
    let request = ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(query);
    let ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(query) = &request else {
        return failure(&request, NtStatus::UNSUCCESSFUL);
    };
    let Some(position) = state.open.iter().position(|(id, _)| *id == header.file_id) else {
        return failure(&request, NtStatus::INVALID_HANDLE);
    };
    let Some((_, Handle::Root { cursor, pattern })) = state.open.get_mut(position) else {
        return failure(&request, NtStatus::NOT_A_DIRECTORY);
    };
    if initial {
        // The path names the pattern under the directory: `\*`, `\name`, `\*.txt`.
        let wanted = query.path.rsplit('\\').next().unwrap_or("*");
        *pattern = Some(if wanted.is_empty() {
            "*".to_owned()
        } else {
            wanted.to_owned()
        });
        *cursor = 0;
    }
    let wanted = pattern.clone().unwrap_or_else(|| "*".to_owned());
    let found = state
        .files
        .iter()
        .enumerate()
        .skip(*cursor)
        .find(|(_, f)| matches(&wanted, &f.name));
    let Some((index, file)) = found else {
        let status = if initial {
            NtStatus::NO_SUCH_FILE
        } else {
            NtStatus::NO_MORE_FILES
        };
        return failure(&request, status);
    };
    *cursor = index + 1;
    let entry = file_entry(file);
    let name = file.name.clone();
    let class = &query.file_info_class_lvl;
    let buffer = if *class == FileInformationClassLevel::FILE_BOTH_DIRECTORY_INFORMATION {
        FileInformationClass::BothDirectory(FileBothDirectoryInformation::new(
            entry.time,
            entry.time,
            entry.time,
            entry.time,
            entry.size,
            entry.attributes,
            name,
        ))
    } else if *class == FileInformationClassLevel::FILE_FULL_DIRECTORY_INFORMATION {
        FileInformationClass::FullDirectory(FileFullDirectoryInformation::new(
            entry.time,
            entry.time,
            entry.time,
            entry.time,
            entry.size,
            entry.attributes,
            name,
        ))
    } else if *class == FileInformationClassLevel::FILE_DIRECTORY_INFORMATION {
        FileInformationClass::Directory(FileDirectoryInformation::new(
            entry.time,
            entry.time,
            entry.time,
            entry.time,
            entry.size,
            entry.attributes,
            name,
        ))
    } else if *class == FileInformationClassLevel::FILE_NAMES_INFORMATION {
        FileInformationClass::Names(FileNamesInformation::new(name))
    } else {
        return failure(&request, NtStatus::NOT_SUPPORTED);
    };
    RdpdrPdu::ClientDriveQueryDirectoryResponse(ClientDriveQueryDirectoryResponse {
        device_io_reply: DeviceIoResponse::new(header, NtStatus::SUCCESS),
        buffer: Some(buffer),
    })
}

impl DriveService for UploadDrive {
    fn attach(&self, updates: UnboundedSender<DriveUpdate>) {
        let _ = updates.send(DriveUpdate::Announce(vec![(
            UPLOADS_DEVICE_ID,
            UPLOADS_NAME.to_owned(),
        )]));
        self.state().updates = Some(updates);
    }

    fn request(&self, request: ServerDriveIoRequest) {
        let header = crate::drive::io_header(&request);
        let completion = if header.device_id == UPLOADS_DEVICE_ID {
            self.respond(request)
        } else {
            warn!(device_id = header.device_id, "IRP for an unknown drive");
            failure(&request, NtStatus::INVALID_DEVICE_REQUEST)
        };
        if let Some(updates) = &self.state().updates {
            let _ = updates.send(DriveUpdate::Completion(completion));
        }
    }
}

#[cfg(test)]
#[path = "drive_uploads_tests.rs"]
mod tests;
