//! The file half of the clipboard bridge ([MS-RDPECLIP] 1.3.1.2, 2.2.5).
//!
//! One [`FileRelay`] sits on each cliprdr channel Warpgate terminates. Toward its RDP peer
//! (the *remote*) it speaks `FileGroupDescriptorW` and File Contents PDUs; toward the other
//! side of the session it speaks the neutral [`ClipboardTransfer`] steps. It holds two
//! mirror-image ledgers:
//!
//! - **remote offers** — files the remote copied. Ranges the other side asks for are
//!   checked against the offer, re-issued to the remote under a stream id minted here and
//!   the clipboard lock that offer was fetched under, and the answer handed back.
//! - **local offers** — files the other side copied, which this channel advertised to the
//!   remote. The remote's ranges are checked the same way and forwarded the other way.
//!
//! The relay never stores file contents: each range is in flight exactly once.

use std::collections::{HashMap, HashSet, VecDeque};

use bytes::Bytes;
use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormatId, FileContentsFlags,
    FileContentsRequest as PduFileContentsRequest, FileContentsResponse, FileDescriptor,
};
use tracing::{debug, info, warn};
use warpgate_core::{
    ClipboardFile, ClipboardFileOffer, ClipboardTransfer, FileContentsReply, FileContentsRequest,
    FileTransferDirection, FileTransferRecord, clamp_file_range, limit_file_offer,
};

/// Offers kept per direction. The newest is the clipboard; older ones stay readable while
/// a peer may still hold a lock on them, which is what lets a paste finish after the
/// source clipboard changed.
const KEPT_OFFERS: usize = 8;

/// Unanswered ranges per direction. Explorer reads one range at a time per file, so this
/// is generous; a peer that stops answering is failed past it instead of growing a map.
pub(crate) const MAX_PENDING_RANGES: usize = 64;

/// Matches `ironrdp_cliprdr::MAX_LOCKED_FILE_LISTS`.
const MAX_LOCKS: usize = 100;

/// What a relay step asks of its surroundings.
#[derive(Debug)]
pub(crate) enum RelayOutput {
    /// Send to the remote over cliprdr.
    Remote(ClipboardMessage),
    /// Hand to the other side of the session.
    Peer(ClipboardTransfer),
    /// A file is being read across the session.
    Audit(FileTransferRecord),
}

#[derive(Debug)]
struct Offer {
    id: u32,
    files: Vec<ClipboardFile>,
    /// The lock this channel holds on the remote's clipboard for a remote offer.
    clip_data_id: Option<u32>,
    /// Entries already reported by [`RelayOutput::Audit`].
    audited: HashSet<u32>,
}

#[derive(Debug, Default)]
pub(crate) struct FileRelay {
    /// The per-copy byte cap; `None` while file transfer is off.
    max_bytes: Option<u64>,
    /// Whether to report transfers: set on the target-facing channel only, so every
    /// transfer is logged once whichever viewer moves it.
    audit: bool,
    /// Whether the remote agreed to stream files on this channel.
    negotiated: bool,
    /// The format id of a file list the remote holds but we have not fetched, because
    /// file transfer was off when it was copied.
    unfetched_file_list: Option<ClipboardFormatId>,

    remote_offers: VecDeque<Offer>,
    next_offer: u32,
    /// Our stream id -> (the other side's request id, length asked of the remote).
    to_remote: HashMap<u32, (u32, u32)>,
    next_stream: u32,

    local_offers: VecDeque<Offer>,
    /// The remote's clipDataId -> the local offer it locked.
    locks: HashMap<u32, u32>,
    /// Our request id -> (the remote's stream id, length it may be answered with).
    from_remote: HashMap<u32, (u32, u32)>,
    next_request: u32,
}

impl FileRelay {
    pub(crate) fn new(max_bytes: Option<u64>, audit: bool) -> Self {
        Self {
            max_bytes,
            audit,
            ..Self::default()
        }
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.max_bytes.is_some() && self.negotiated
    }

    pub(crate) fn set_limit(&mut self, max_bytes: Option<u64>) {
        self.max_bytes = max_bytes;
        if max_bytes.is_none() {
            self.remote_offers.clear();
            self.local_offers.clear();
        }
    }

    pub(crate) const fn set_negotiated(&mut self, negotiated: bool) {
        self.negotiated = negotiated;
    }

    /// The remote's clipboard changed; `file_list` is its file list format, if it holds
    /// files. Returns the format to fetch the list in, if files may move now.
    pub(crate) fn remote_copied(
        &mut self,
        file_list: Option<ClipboardFormatId>,
    ) -> Option<ClipboardFormatId> {
        if self.negotiated && self.max_bytes.is_some() {
            self.unfetched_file_list = None;
            file_list
        } else {
            self.unfetched_file_list = file_list.filter(|_| self.negotiated);
            None
        }
    }

    /// A file list copied while file transfer was off, to fetch now that it is on.
    pub(crate) const fn take_unfetched_file_list(&mut self) -> Option<ClipboardFormatId> {
        if self.enabled() {
            self.unfetched_file_list.take()
        } else {
            None
        }
    }

    /// A new cliprdr channel: stream ids and locks belong to the old one.
    pub(crate) fn reset_channel(&mut self) -> Vec<RelayOutput> {
        self.negotiated = false;
        self.unfetched_file_list = None;
        self.remote_offers.clear();
        self.local_offers.clear();
        self.locks.clear();
        self.from_remote.clear();
        self.to_remote
            .drain()
            .map(|(_, (id, _))| RelayOutput::Peer(failed(id)))
            .collect()
    }

    // --- the remote copied files -------------------------------------------------------

    /// The remote's file list arrived (fetched under `clip_data_id`, if locking is on).
    pub(crate) fn remote_file_list(
        &mut self,
        descriptors: &[FileDescriptor],
        clip_data_id: Option<u32>,
    ) -> Vec<RelayOutput> {
        let Some(max_bytes) = self.max_bytes.filter(|_| self.negotiated) else {
            return Vec::new();
        };
        let files = descriptors.iter().filter_map(from_descriptor).collect();
        let (files, dropped) = limit_file_offer(files, max_bytes);
        if dropped > 0 {
            warn!(
                dropped,
                max_bytes,
                "Left files out of a clipboard copy: unknown size, unsafe name or over the cap"
            );
        }
        if files.is_empty() {
            return Vec::new();
        }
        self.next_offer = self.next_offer.wrapping_add(1);
        let offer = self.next_offer;
        push_bounded(
            &mut self.remote_offers,
            Offer {
                id: offer,
                files: files.clone(),
                clip_data_id,
                audited: HashSet::new(),
            },
        );
        vec![RelayOutput::Peer(ClipboardTransfer::Offer(
            ClipboardFileOffer { offer, files },
        ))]
    }

    /// The other side wants a range of a file the remote offered.
    fn request_from_remote(&mut self, request: FileContentsRequest) -> Vec<RelayOutput> {
        let mut out = Vec::new();
        let Some(offer) = self
            .remote_offers
            .iter_mut()
            .find(|offer| offer.id == request.offer)
        else {
            return vec![RelayOutput::Peer(failed(request.id))];
        };
        let Some(file) = offer.files.get(request.index as usize) else {
            return vec![RelayOutput::Peer(failed(request.id))];
        };
        let Some(length) = clamp_file_range(file, request.offset, request.length) else {
            return vec![RelayOutput::Peer(failed(request.id))];
        };
        if length == 0 {
            return vec![RelayOutput::Peer(ClipboardTransfer::Reply(
                FileContentsReply {
                    id: request.id,
                    data: Some(Bytes::new()),
                },
            ))];
        }
        let Ok(index) = i32::try_from(request.index) else {
            return vec![RelayOutput::Peer(failed(request.id))];
        };
        if self.to_remote.len() >= MAX_PENDING_RANGES {
            return vec![RelayOutput::Peer(failed(request.id))];
        }
        if self.audit && offer.audited.insert(request.index) {
            out.push(audit(FileTransferDirection::Download, file));
        }
        self.next_stream = self.next_stream.wrapping_add(1);
        let stream_id = self.next_stream;
        self.to_remote.insert(stream_id, (request.id, length));
        out.push(RelayOutput::Remote(
            ClipboardMessage::SendFileContentsRequest(PduFileContentsRequest {
                stream_id,
                index,
                flags: FileContentsFlags::RANGE,
                position: request.offset,
                requested_size: length,
                data_id: offer.clip_data_id,
            }),
        ));
        out
    }

    /// The remote answered (or the channel gave up on) one of our ranges.
    pub(crate) fn remote_contents(
        &mut self,
        response: &FileContentsResponse<'_>,
    ) -> Vec<RelayOutput> {
        let Some((id, length)) = self.to_remote.remove(&response.stream_id()) else {
            debug!(
                stream_id = response.stream_id(),
                "File contents for no pending range"
            );
            return Vec::new();
        };
        let data = (!response.is_error()).then(|| {
            let data = response.data();
            Bytes::copy_from_slice(data.get(..data.len().min(length as usize)).unwrap_or(data))
        });
        vec![RelayOutput::Peer(ClipboardTransfer::Reply(
            FileContentsReply { id, data },
        ))]
    }

    // --- the other side copied files ---------------------------------------------------

    /// Advertise the other side's files to the remote.
    fn offer_to_remote(&mut self, offer: ClipboardFileOffer) -> Vec<RelayOutput> {
        let Some(max_bytes) = self.max_bytes.filter(|_| self.negotiated) else {
            return Vec::new();
        };
        let (files, dropped) = limit_file_offer(offer.files, max_bytes);
        if dropped > 0 {
            warn!(
                dropped,
                max_bytes, "Left files out of a clipboard copy: unsafe name or over the cap"
            );
        }
        if files.is_empty() {
            return Vec::new();
        }
        let descriptors = files.iter().map(to_descriptor).collect();
        push_bounded(
            &mut self.local_offers,
            Offer {
                id: offer.offer,
                files,
                clip_data_id: None,
                audited: HashSet::new(),
            },
        );
        vec![RelayOutput::Remote(ClipboardMessage::SendInitiateFileCopy(
            descriptors,
        ))]
    }

    /// The remote locked our clipboard: requests carrying `clip_data_id` read the offer
    /// that is current now, even after the other side copies something else.
    pub(crate) fn lock(&mut self, clip_data_id: u32) {
        if let Some(offer) = self.local_offers.back()
            && (self.locks.len() < MAX_LOCKS || self.locks.contains_key(&clip_data_id))
        {
            self.locks.insert(clip_data_id, offer.id);
        }
    }

    pub(crate) fn unlock(&mut self, clip_data_id: u32) {
        self.locks.remove(&clip_data_id);
    }

    /// The remote wants a range (or the size) of a file we advertised.
    pub(crate) fn remote_request(&mut self, request: &PduFileContentsRequest) -> Vec<RelayOutput> {
        let error = || {
            vec![RelayOutput::Remote(
                ClipboardMessage::SendFileContentsResponse(FileContentsResponse::new_error(
                    request.stream_id,
                )),
            )]
        };
        let offer_id = request
            .data_id
            .and_then(|clip_data_id| self.locks.get(&clip_data_id).copied());
        let offer = match offer_id {
            Some(offer_id) => self.local_offers.iter_mut().find(|o| o.id == offer_id),
            None => self.local_offers.back_mut(),
        };
        let Some(offer) = offer else {
            return error();
        };
        let Some(file) = usize::try_from(request.index)
            .ok()
            .and_then(|index| offer.files.get(index))
        else {
            return error();
        };
        if request.flags.contains(FileContentsFlags::SIZE) {
            if file.directory {
                return error();
            }
            // The declared size is the one the cap was checked against, so it is the one
            // every range is held to; the source is never asked.
            return vec![RelayOutput::Remote(
                ClipboardMessage::SendFileContentsResponse(
                    FileContentsResponse::new_size_response(request.stream_id, file.size),
                ),
            )];
        }
        let Some(length) = clamp_file_range(file, request.position, request.requested_size) else {
            return error();
        };
        if length == 0 {
            return vec![RelayOutput::Remote(
                ClipboardMessage::SendFileContentsResponse(
                    FileContentsResponse::new_data_response(request.stream_id, Vec::new()),
                ),
            )];
        }
        if self.from_remote.len() >= MAX_PENDING_RANGES {
            return error();
        }
        let mut out = Vec::new();
        let index = request.index as u32;
        if self.audit && offer.audited.insert(index) {
            out.push(audit(FileTransferDirection::Upload, file));
        }
        self.next_request = self.next_request.wrapping_add(1);
        let id = self.next_request;
        self.from_remote.insert(id, (request.stream_id, length));
        out.push(RelayOutput::Peer(ClipboardTransfer::Request(
            FileContentsRequest {
                id,
                offer: offer.id,
                index,
                offset: request.position,
                length,
            },
        )));
        out
    }

    /// The other side answered a range the remote asked for.
    fn reply_to_remote(&mut self, reply: FileContentsReply) -> Vec<RelayOutput> {
        let Some((stream_id, length)) = self.from_remote.remove(&reply.id) else {
            return Vec::new();
        };
        let response = match reply.data {
            Some(data) => {
                let data = data.slice(..data.len().min(length as usize));
                FileContentsResponse::new_data_response(stream_id, data.to_vec())
            }
            None => FileContentsResponse::new_error(stream_id),
        };
        vec![RelayOutput::Remote(
            ClipboardMessage::SendFileContentsResponse(response),
        )]
    }

    /// A step from the other side of the session.
    pub(crate) fn peer_step(&mut self, transfer: ClipboardTransfer) -> Vec<RelayOutput> {
        if !self.enabled() {
            return match transfer {
                ClipboardTransfer::Request(request) => vec![RelayOutput::Peer(failed(request.id))],
                _ => Vec::new(),
            };
        }
        match transfer {
            ClipboardTransfer::Offer(offer) => self.offer_to_remote(offer),
            ClipboardTransfer::Request(request) => self.request_from_remote(request),
            ClipboardTransfer::Reply(reply) => self.reply_to_remote(reply),
        }
    }
}

const fn failed(id: u32) -> ClipboardTransfer {
    ClipboardTransfer::Reply(FileContentsReply { id, data: None })
}

fn audit(direction: FileTransferDirection, file: &ClipboardFile) -> RelayOutput {
    info!(
        ?direction,
        path = %file.path,
        size = file.size,
        "Clipboard file transfer"
    );
    RelayOutput::Audit(FileTransferRecord {
        direction,
        path: file.path.clone(),
        size: file.size,
    })
}

fn push_bounded(offers: &mut VecDeque<Offer>, offer: Offer) {
    if offers.len() >= KEPT_OFFERS {
        offers.pop_front();
    }
    offers.push_back(offer);
}

/// A remote descriptor as an offer entry. A file without a declared size is dropped: its
/// size is what the cap and every range are checked against.
fn from_descriptor(descriptor: &FileDescriptor) -> Option<ClipboardFile> {
    let directory = descriptor
        .attributes
        .is_some_and(|attributes| attributes.contains(ClipboardFileAttributes::DIRECTORY));
    let path = match &descriptor.relative_path {
        Some(dir) if !dir.is_empty() => format!("{dir}\\{}", descriptor.name),
        _ => descriptor.name.clone(),
    };
    let size = if directory { 0 } else { descriptor.file_size? };
    Some(ClipboardFile {
        path,
        size,
        directory,
    })
}

fn to_descriptor(file: &ClipboardFile) -> FileDescriptor {
    let (dir, name) = match file.path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir.replace('/', "\\")), name),
        None => (None, file.path.as_str()),
    };
    let attributes = if file.directory {
        ClipboardFileAttributes::DIRECTORY
    } else {
        ClipboardFileAttributes::NORMAL
    };
    let mut descriptor = FileDescriptor::new(name).with_attributes(attributes);
    if !file.directory {
        descriptor = descriptor.with_file_size(file.size);
    }
    if let Some(dir) = dir {
        descriptor = descriptor.with_relative_path(dir);
    }
    descriptor
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 1000;

    fn relay(audit: bool) -> FileRelay {
        let mut relay = FileRelay::new(Some(CAP), audit);
        relay.set_negotiated(true);
        relay
    }

    fn descriptor(name: &str, size: u64) -> FileDescriptor {
        FileDescriptor::new(name).with_file_size(size)
    }

    fn offered(out: Vec<RelayOutput>) -> ClipboardFileOffer {
        match out.as_slice() {
            [RelayOutput::Peer(ClipboardTransfer::Offer(offer))] => offer.clone(),
            other => panic!("expected an offer, got {other:?}"),
        }
    }

    fn sent_request(out: &[RelayOutput]) -> PduFileContentsRequest {
        out.iter()
            .find_map(|o| match o {
                RelayOutput::Remote(ClipboardMessage::SendFileContentsRequest(r)) => {
                    Some(r.clone())
                }
                _ => None,
            })
            .expect("a file contents request")
    }

    fn reply(out: &[RelayOutput]) -> FileContentsReply {
        out.iter()
            .find_map(|o| match o {
                RelayOutput::Peer(ClipboardTransfer::Reply(r)) => Some(r.clone()),
                _ => None,
            })
            .expect("a reply")
    }

    fn response(out: &[RelayOutput]) -> (u32, bool, Vec<u8>) {
        out.iter()
            .find_map(|o| match o {
                RelayOutput::Remote(ClipboardMessage::SendFileContentsResponse(r)) => {
                    Some((r.stream_id(), r.is_error(), r.data().to_vec()))
                }
                _ => None,
            })
            .expect("a file contents response")
    }

    fn audits(out: &[RelayOutput]) -> Vec<FileTransferRecord> {
        out.iter()
            .filter_map(|o| match o {
                RelayOutput::Audit(record) => Some(record.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn descriptors_round_trip_through_offer_entries() {
        let file = ClipboardFile {
            path: "dir/sub/a.txt".into(),
            size: 42,
            directory: false,
        };
        let descriptor = to_descriptor(&file);
        assert_eq!(descriptor.name, "a.txt");
        assert_eq!(descriptor.relative_path.as_deref(), Some(r"dir\sub"));
        assert_eq!(descriptor.file_size, Some(42));
        let back = from_descriptor(&descriptor).unwrap();
        let (kept, _) = limit_file_offer(vec![back], u64::MAX);
        assert_eq!(kept, [file]);

        let dir = ClipboardFile {
            path: "dir".into(),
            size: 0,
            directory: true,
        };
        let descriptor = to_descriptor(&dir);
        assert_eq!(descriptor.file_size, None);
        assert_eq!(from_descriptor(&descriptor), Some(dir));
    }

    #[test]
    fn a_remote_file_without_a_size_is_not_offered() {
        let mut relay = relay(false);
        let out = relay.remote_file_list(&[FileDescriptor::new("a")], None);
        assert!(out.is_empty());
    }

    #[test]
    fn nothing_moves_while_disabled_or_not_negotiated() {
        let mut off = FileRelay::new(None, true);
        off.set_negotiated(true);
        assert!(off.remote_file_list(&[descriptor("a", 1)], None).is_empty());

        let mut unnegotiated = FileRelay::new(Some(CAP), true);
        assert!(
            unnegotiated
                .remote_file_list(&[descriptor("a", 1)], None)
                .is_empty()
        );
        let out = unnegotiated.peer_step(ClipboardTransfer::Offer(ClipboardFileOffer {
            offer: 1,
            files: vec![ClipboardFile {
                path: "a".into(),
                size: 1,
                directory: false,
            }],
        }));
        assert!(out.is_empty());
        // A request is still answered, so the other side never waits on it.
        let out = unnegotiated.peer_step(ClipboardTransfer::Request(FileContentsRequest {
            id: 7,
            offer: 1,
            index: 0,
            offset: 0,
            length: 1,
        }));
        assert_eq!(reply(&out), FileContentsReply { id: 7, data: None });
    }

    /// The full download path: the remote's list becomes an offer, a range against it is
    /// re-issued under our own stream id and the remote's lock, and the remote's answer
    /// goes back under the requester's id — trimmed to what was asked.
    #[test]
    fn a_remote_file_is_relayed_range_by_range() {
        let mut relay = relay(true);
        let offer = offered(relay.remote_file_list(
            &[descriptor("a.bin", 10), descriptor("big.bin", CAP + 1)],
            Some(5),
        ));
        assert_eq!(offer.files.len(), 1, "over-cap file is left out");

        let out = relay.peer_step(ClipboardTransfer::Request(FileContentsRequest {
            id: 99,
            offer: offer.offer,
            index: 0,
            offset: 8,
            length: 100,
        }));
        assert_eq!(
            audits(&out),
            [FileTransferRecord {
                direction: FileTransferDirection::Download,
                path: "a.bin".into(),
                size: 10,
            }]
        );
        let sent = sent_request(&out);
        assert_eq!(sent.position, 8);
        assert_eq!(sent.requested_size, 2, "clamped to the declared size");
        assert_eq!(sent.data_id, Some(5));
        assert!(sent.flags.contains(FileContentsFlags::RANGE));

        // A remote overrunning the range is cut back to it.
        let out = relay.remote_contents(&FileContentsResponse::new_data_response(
            sent.stream_id,
            vec![1, 2, 3, 4],
        ));
        assert_eq!(
            reply(&out),
            FileContentsReply {
                id: 99,
                data: Some(Bytes::from_static(&[1, 2])),
            }
        );
        // Answered once only.
        assert!(
            relay
                .remote_contents(&FileContentsResponse::new_data_response(
                    sent.stream_id,
                    vec![1]
                ))
                .is_empty()
        );

        // The same file read again is not audited again.
        let out = relay.peer_step(ClipboardTransfer::Request(FileContentsRequest {
            id: 100,
            offer: offer.offer,
            index: 0,
            offset: 0,
            length: 1,
        }));
        assert!(audits(&out).is_empty());
        let failed_stream = sent_request(&out).stream_id;
        let out = relay.remote_contents(&FileContentsResponse::new_error(failed_stream));
        assert_eq!(
            reply(&out),
            FileContentsReply {
                id: 100,
                data: None
            }
        );
    }

    #[test]
    fn requests_outside_an_offer_are_failed_without_asking_the_remote() {
        let mut relay = relay(false);
        let offer = offered(relay.remote_file_list(&[descriptor("a", 10)], None));
        for (offer_id, index, offset) in [
            (offer.offer + 1, 0, 0),
            (offer.offer, 1, 0),
            (offer.offer, 0, 11),
        ] {
            let out = relay.peer_step(ClipboardTransfer::Request(FileContentsRequest {
                id: 1,
                offer: offer_id,
                index,
                offset,
                length: 1,
            }));
            assert_eq!(out.len(), 1);
            assert_eq!(reply(&out).data, None);
        }
        // End of file is an empty read, not a failure.
        let out = relay.peer_step(ClipboardTransfer::Request(FileContentsRequest {
            id: 2,
            offer: offer.offer,
            index: 0,
            offset: 10,
            length: 1,
        }));
        assert_eq!(reply(&out).data, Some(Bytes::new()));
    }

    #[test]
    fn pending_ranges_to_the_remote_are_bounded() {
        let mut relay = relay(false);
        let offer = offered(relay.remote_file_list(&[descriptor("a", 10)], None));
        for id in 0..MAX_PENDING_RANGES as u32 {
            let out = relay.peer_step(ClipboardTransfer::Request(FileContentsRequest {
                id,
                offer: offer.offer,
                index: 0,
                offset: 0,
                length: 1,
            }));
            sent_request(&out);
        }
        let out = relay.peer_step(ClipboardTransfer::Request(FileContentsRequest {
            id: 1000,
            offer: offer.offer,
            index: 0,
            offset: 0,
            length: 1,
        }));
        assert_eq!(
            reply(&out),
            FileContentsReply {
                id: 1000,
                data: None
            }
        );
    }

    fn local_offer(
        relay: &mut FileRelay,
        offer: u32,
        files: &[(&str, u64)],
    ) -> Vec<FileDescriptor> {
        let out = relay.peer_step(ClipboardTransfer::Offer(ClipboardFileOffer {
            offer,
            files: files
                .iter()
                .map(|(path, size)| ClipboardFile {
                    path: (*path).into(),
                    size: *size,
                    directory: false,
                })
                .collect(),
        }));
        match out.as_slice() {
            [RelayOutput::Remote(ClipboardMessage::SendInitiateFileCopy(descriptors))] => {
                descriptors.clone()
            }
            [] => Vec::new(),
            other => panic!("expected a file copy, got {other:?}"),
        }
    }

    fn remote_range(
        stream_id: u32,
        index: i32,
        position: u64,
        size: u32,
        data_id: Option<u32>,
    ) -> PduFileContentsRequest {
        PduFileContentsRequest {
            stream_id,
            index,
            flags: FileContentsFlags::RANGE,
            position,
            requested_size: size,
            data_id,
        }
    }

    /// The upload path, mirror of the download one: the other side's files are advertised
    /// within the cap, the remote's size requests are answered from the declared size, and
    /// its ranges are forwarded and answered under its own stream id.
    #[test]
    fn a_local_file_is_served_to_the_remote_range_by_range() {
        let mut relay = relay(true);
        let descriptors = local_offer(
            &mut relay,
            3,
            &[("../x", 1), ("dir/a.txt", 6), ("huge", CAP)],
        );
        assert_eq!(
            descriptors.len(),
            1,
            "unsafe and over-cap entries are dropped"
        );

        let size = PduFileContentsRequest {
            stream_id: 1,
            index: 0,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        };
        assert_eq!(
            response(&relay.remote_request(&size)),
            (1, false, 6u64.to_le_bytes().to_vec())
        );

        let out = relay.remote_request(&remote_range(2, 0, 4, 1000, None));
        assert_eq!(audits(&out)[0].direction, FileTransferDirection::Upload);
        assert_eq!(audits(&out)[0].path, "dir/a.txt");
        let RelayOutput::Peer(ClipboardTransfer::Request(request)) = out.last().unwrap() else {
            panic!("expected a request to the other side");
        };
        assert_eq!(
            (request.offer, request.index, request.offset, request.length),
            (3, 0, 4, 2)
        );

        let out = relay.peer_step(ClipboardTransfer::Reply(FileContentsReply {
            id: request.id,
            data: Some(Bytes::from_static(b"abcdef")),
        }));
        assert_eq!(response(&out), (2, false, b"ab".to_vec()));
        // An unknown or repeated reply is dropped.
        assert!(
            relay
                .peer_step(ClipboardTransfer::Reply(FileContentsReply {
                    id: request.id,
                    data: None,
                }))
                .is_empty()
        );
    }

    #[test]
    fn invalid_remote_requests_are_failed_locally() {
        let mut relay = relay(false);
        // Nothing offered yet.
        assert!(response(&relay.remote_request(&remote_range(1, 0, 0, 1, None))).1);
        local_offer(&mut relay, 1, &[("a", 4)]);
        assert!(response(&relay.remote_request(&remote_range(2, 1, 0, 1, None))).1);
        assert!(response(&relay.remote_request(&remote_range(3, -1, 0, 1, None))).1);
        assert!(response(&relay.remote_request(&remote_range(4, 0, 5, 1, None))).1);
        assert_eq!(
            response(&relay.remote_request(&remote_range(5, 0, 4, 1, None))),
            (5, false, vec![])
        );
    }

    /// A lock pins the offer current when it was taken: a paste still running when the
    /// viewer copies something else keeps reading the files it started with.
    #[test]
    fn a_locked_offer_outlives_the_next_copy() {
        let mut relay = relay(false);
        local_offer(&mut relay, 1, &[("old", 4)]);
        relay.lock(77);
        local_offer(&mut relay, 2, &[("new", 4)]);

        let request = |relay: &mut FileRelay, data_id| match relay
            .remote_request(&remote_range(1, 0, 0, 1, data_id))
            .pop()
        {
            Some(RelayOutput::Peer(ClipboardTransfer::Request(r))) => r.offer,
            other => panic!("{other:?}"),
        };
        assert_eq!(request(&mut relay, Some(77)), 1);
        assert_eq!(request(&mut relay, None), 2);
        relay.unlock(77);
        assert_eq!(request(&mut relay, Some(77)), 2);
    }

    /// The viewer-facing channel comes up before the target says whether files may move;
    /// a file copy made in that window is fetched once they may.
    #[test]
    fn a_copy_made_before_files_were_allowed_is_fetched_once_they_are() {
        let format = ClipboardFormatId::new(0xC0FE);
        let mut relay = FileRelay::new(None, false);
        relay.set_negotiated(true);
        assert_eq!(relay.remote_copied(Some(format)), None);
        assert_eq!(relay.take_unfetched_file_list(), None, "still off");
        relay.set_limit(Some(CAP));
        assert_eq!(relay.take_unfetched_file_list(), Some(format));
        assert_eq!(relay.take_unfetched_file_list(), None);
        assert_eq!(relay.remote_copied(Some(format)), Some(format));
        assert_eq!(relay.remote_copied(None), None);
    }

    #[test]
    fn a_new_channel_fails_ranges_still_waiting_on_the_old_one() {
        let mut relay = relay(false);
        let offer = offered(relay.remote_file_list(&[descriptor("a", 10)], None));
        relay.peer_step(ClipboardTransfer::Request(FileContentsRequest {
            id: 4,
            offer: offer.offer,
            index: 0,
            offset: 0,
            length: 1,
        }));
        let out = relay.reset_channel();
        assert_eq!(reply(&out), FileContentsReply { id: 4, data: None });
        assert!(!relay.enabled());
    }
}
