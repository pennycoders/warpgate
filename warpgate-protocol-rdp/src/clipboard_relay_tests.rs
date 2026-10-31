//! End-to-end tests of the clipboard file relay over real cliprdr PDUs.
//!
//! Four channel endpoints are wired up as in a native session: a viewer (standing in for
//! mstsc) and Warpgate's viewer-facing server channel, Warpgate's target-facing client
//! channel and a target (standing in for Windows). Every PDU between an endpoint pair is
//! encoded and decoded, and the two Warpgate channels are joined the way the session joins
//! them — the steps one emits are fed to the other.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatName, ClipboardGeneralCapabilityFlags, FileContentsFlags,
    FileContentsRequest, FileContentsResponse, FileDescriptor, FormatDataRequest,
    FormatDataResponse, LockDataId,
};
use ironrdp_cliprdr::{Cliprdr, CliprdrClient, CliprdrServer, Role};
use ironrdp_core::AsAny;
use ironrdp_svc::{SvcMessage, SvcProcessor};
use warpgate_core::{ClipboardTransfer, FileTransferDirection, FileTransferRecord};

use crate::clipboard::{Clipboard, ClipboardSink};

#[derive(Debug)]
enum Out {
    Request(ClipboardMessage),
    Transfer(ClipboardTransfer),
    Audit(FileTransferRecord),
}

#[derive(Clone, Debug)]
struct Sink(Sender<Out>);

impl ClipboardSink for Sink {
    fn request(&self, message: ClipboardMessage) {
        let _ = self.0.send(Out::Request(message));
    }

    fn text_received(&self, _text: String) {}

    fn transfer(&self, transfer: ClipboardTransfer) {
        let _ = self.0.send(Out::Transfer(transfer));
    }

    fn transferred(&self, record: FileTransferRecord) {
        let _ = self.0.send(Out::Audit(record));
    }
}

/// What a stand-in peer (mstsc or Windows) has seen and holds.
#[derive(Debug, Default)]
struct PeerState {
    /// Messages its backend queued, applied by the pump.
    queue: VecDeque<ClipboardMessage>,
    /// Its own files, served to whoever asks.
    files: Vec<Vec<u8>>,
    remote_formats: Vec<ClipboardFormat>,
    remote_files: Option<(Vec<FileDescriptor>, Option<u32>)>,
    contents: HashMap<u32, Option<Vec<u8>>>,
    /// Ranges it was asked for: (index, position, size).
    served: Vec<(i32, u64, u32)>,
}

#[derive(Debug, Clone)]
struct Peer {
    state: Arc<Mutex<PeerState>>,
    files: bool,
}

impl AsAny for Peer {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl CliprdrBackend for Peer {
    fn temporary_directory(&self) -> &str {
        "."
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        if self.files {
            ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
                | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
                | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
        } else {
            ClipboardGeneralCapabilityFlags::empty()
        }
    }

    fn on_ready(&mut self) {}

    fn on_request_format_list(&mut self) {
        self.state
            .lock()
            .unwrap()
            .queue
            .push_back(ClipboardMessage::SendInitiateCopy(vec![]));
    }

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        self.state.lock().unwrap().remote_formats = formats.to_vec();
    }

    fn on_format_data_request(&mut self, _: FormatDataRequest) {
        self.state
            .lock()
            .unwrap()
            .queue
            .push_back(ClipboardMessage::SendFormatData(
                FormatDataResponse::new_error(),
            ));
    }

    fn on_format_data_response(&mut self, _: FormatDataResponse<'_>) {}

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        let mut state = self.state.lock().unwrap();
        state
            .served
            .push((request.index, request.position, request.requested_size));
        let file = state.files.get(request.index as usize).cloned();
        let response = match file {
            Some(file) if request.flags.contains(FileContentsFlags::SIZE) => {
                FileContentsResponse::new_size_response(request.stream_id, file.len() as u64)
            }
            Some(file) => {
                let start = (request.position as usize).min(file.len());
                let end = (start + request.requested_size as usize).min(file.len());
                FileContentsResponse::new_data_response(
                    request.stream_id,
                    file[start..end].to_vec(),
                )
            }
            None => FileContentsResponse::new_error(request.stream_id),
        };
        state
            .queue
            .push_back(ClipboardMessage::SendFileContentsResponse(response));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let data = (!response.is_error()).then(|| response.data().to_vec());
        self.state
            .lock()
            .unwrap()
            .contents
            .insert(response.stream_id(), data);
    }

    fn on_lock(&mut self, _: LockDataId) {}

    fn on_unlock(&mut self, _: LockDataId) {}

    fn on_remote_file_list(&mut self, files: &[FileDescriptor], clip_data_id: Option<u32>) {
        self.state.lock().unwrap().remote_files = Some((files.to_vec(), clip_data_id));
    }
}

fn apply<R: Role>(cliprdr: &mut Cliprdr<R>, message: ClipboardMessage) -> Vec<SvcMessage> {
    let result = match message {
        ClipboardMessage::SendInitiateCopy(formats) => cliprdr.initiate_copy(&formats),
        ClipboardMessage::SendInitiatePaste(format) => cliprdr.initiate_paste(format),
        ClipboardMessage::SendFormatData(data) => cliprdr.submit_format_data(data),
        ClipboardMessage::SendInitiateFileCopy(files) => cliprdr.initiate_file_copy(files),
        ClipboardMessage::SendFileContentsRequest(request) => {
            cliprdr.request_file_contents(request)
        }
        ClipboardMessage::SendFileContentsResponse(response) => {
            cliprdr.submit_file_contents(response)
        }
        ClipboardMessage::Error(error) => panic!("{error}"),
    };
    result.map(Vec::from).unwrap_or_default()
}

fn encode(messages: Vec<SvcMessage>) -> Vec<Vec<u8>> {
    messages
        .into_iter()
        .map(|message| message.encode_unframed_pdu().unwrap())
        .collect()
}

struct Harness {
    viewer: CliprdrClient,
    viewer_peer: Arc<Mutex<PeerState>>,
    server: CliprdrServer,
    server_clipboard: Clipboard<Sink>,
    server_out: Receiver<Out>,
    client: CliprdrClient,
    client_clipboard: Clipboard<Sink>,
    client_out: Receiver<Out>,
    target: CliprdrServer,
    target_peer: Arc<Mutex<PeerState>>,
    /// PDUs in flight to each endpoint.
    to_viewer: VecDeque<Vec<u8>>,
    to_server: VecDeque<Vec<u8>>,
    to_client: VecDeque<Vec<u8>>,
    to_target: VecDeque<Vec<u8>>,
    audits: Vec<FileTransferRecord>,
}

impl Harness {
    /// `cap` is the target's clipboard file cap; `None` turns the feature off for it.
    fn new(cap: Option<u64>) -> Self {
        let viewer_peer = Arc::new(Mutex::new(PeerState::default()));
        let target_peer = Arc::new(Mutex::new(PeerState::default()));
        let (server_tx, server_out) = channel();
        let (client_tx, client_out) = channel();
        let server_clipboard = Clipboard::new(Sink(server_tx)).with_files(None, false);
        let mut client_clipboard = Clipboard::deferred(Sink(client_tx));
        if cap.is_some() {
            client_clipboard = client_clipboard.with_files(cap, true);
        }
        let mut harness = Self {
            viewer: CliprdrClient::new(Box::new(Peer {
                state: Arc::clone(&viewer_peer),
                files: true,
            })),
            viewer_peer,
            server: CliprdrServer::new(Box::new(server_clipboard.backend())),
            server_clipboard,
            server_out,
            client: CliprdrClient::new(Box::new(client_clipboard.backend())),
            client_clipboard,
            client_out,
            target: CliprdrServer::new(Box::new(Peer {
                state: Arc::clone(&target_peer),
                files: true,
            })),
            target_peer,
            to_viewer: VecDeque::new(),
            to_server: VecDeque::new(),
            to_client: VecDeque::new(),
            to_target: VecDeque::new(),
            audits: Vec::new(),
        };
        let start = harness.server.start().unwrap();
        harness.to_viewer.extend(encode(start));
        let start = harness.target.start().unwrap();
        harness.to_client.extend(encode(start));
        harness.pump();
        // The target is up: the session tells the viewer side what it allows.
        harness.server_clipboard.set_file_limit(cap);
        harness.pump();
        harness
    }

    /// Deliver everything in flight until the four endpoints go quiet.
    fn pump(&mut self) {
        for _ in 0..10_000 {
            let mut busy = false;
            while let Some(pdu) = self.to_viewer.pop_front() {
                busy = true;
                let out = self.viewer.process(&pdu).unwrap();
                self.to_server.extend(encode(out));
            }
            while let Some(pdu) = self.to_server.pop_front() {
                busy = true;
                let out = self.server.process(&pdu).unwrap();
                self.to_viewer.extend(encode(out));
            }
            while let Some(pdu) = self.to_client.pop_front() {
                busy = true;
                let out = self.client.process(&pdu).unwrap();
                self.to_target.extend(encode(out));
            }
            while let Some(pdu) = self.to_target.pop_front() {
                busy = true;
                let out = self.target.process(&pdu).unwrap();
                self.to_client.extend(encode(out));
            }
            loop {
                let message = self.viewer_peer.lock().unwrap().queue.pop_front();
                let Some(message) = message else { break };
                busy = true;
                let out = apply(&mut self.viewer, message);
                self.to_server.extend(encode(out));
            }
            loop {
                let message = self.target_peer.lock().unwrap().queue.pop_front();
                let Some(message) = message else { break };
                busy = true;
                let out = apply(&mut self.target, message);
                self.to_client.extend(encode(out));
            }
            while let Ok(out) = self.server_out.try_recv() {
                busy = true;
                match out {
                    Out::Request(message) => {
                        let out = apply(&mut self.server, message);
                        self.to_viewer.extend(encode(out));
                    }
                    Out::Transfer(transfer) => self.client_clipboard.transfer(transfer),
                    Out::Audit(record) => self.audits.push(record),
                }
            }
            while let Ok(out) = self.client_out.try_recv() {
                busy = true;
                match out {
                    Out::Request(message) => {
                        let out = apply(&mut self.client, message);
                        self.to_target.extend(encode(out));
                    }
                    Out::Transfer(transfer) => self.server_clipboard.transfer(transfer),
                    Out::Audit(record) => self.audits.push(record),
                }
            }
            if !busy {
                return;
            }
        }
        panic!("clipboard endpoints never went quiet");
    }

    fn copy_files(peer: &Arc<Mutex<PeerState>>, files: &[(&str, &[u8])]) {
        let mut state = peer.lock().unwrap();
        state.files = files.iter().map(|(_, data)| data.to_vec()).collect();
        let descriptors = files
            .iter()
            .map(|(name, data)| FileDescriptor::new(*name).with_file_size(data.len() as u64))
            .collect();
        state
            .queue
            .push_back(ClipboardMessage::SendInitiateFileCopy(descriptors));
    }

    /// Paste on `peer`: fetch the file list it was offered, then read every file whole in
    /// `chunk`-sized ranges. Returns each file's name and bytes, or `None` if nothing with
    /// files was offered.
    fn paste_files<R: Role>(
        peer: &Arc<Mutex<PeerState>>,
        endpoint: &mut Cliprdr<R>,
        harness_pump: &mut dyn FnMut(&mut Cliprdr<R>, Vec<SvcMessage>),
        chunk: u32,
    ) -> Option<Vec<(String, Vec<u8>)>> {
        let format = {
            let state = peer.lock().unwrap();
            state
                .remote_formats
                .iter()
                .find(|format| {
                    format
                        .name()
                        .is_some_and(|name| name.value() == ClipboardFormatName::FILE_LIST.value())
                })
                .map(ClipboardFormat::id)?
        };
        let out = apply(endpoint, ClipboardMessage::SendInitiatePaste(format));
        harness_pump(endpoint, out);
        let (files, clip_data_id) = peer.lock().unwrap().remote_files.clone()?;
        let mut stream_id = 1000;
        let mut pasted = Vec::new();
        for (index, file) in files.iter().enumerate() {
            let size = file.file_size.unwrap();
            let mut data = Vec::new();
            while (data.len() as u64) < size {
                stream_id += 1;
                let request = FileContentsRequest {
                    stream_id,
                    index: index as i32,
                    flags: FileContentsFlags::RANGE,
                    position: data.len() as u64,
                    requested_size: chunk.min((size - data.len() as u64) as u32),
                    data_id: clip_data_id,
                };
                let out = apply(endpoint, ClipboardMessage::SendFileContentsRequest(request));
                harness_pump(endpoint, out);
                let reply = peer.lock().unwrap().contents.remove(&stream_id);
                match reply {
                    Some(Some(bytes)) if !bytes.is_empty() => data.extend(bytes),
                    other => panic!("range at {} of {}: {other:?}", data.len(), file.name),
                }
            }
            pasted.push((file.name.clone(), data));
        }
        Some(pasted)
    }

    fn paste_on_target(&mut self, chunk: u32) -> Option<Vec<(String, Vec<u8>)>> {
        let peer = Arc::clone(&self.target_peer);
        let mut target = std::mem::replace(
            &mut self.target,
            CliprdrServer::new(Box::new(Peer {
                state: Arc::default(),
                files: false,
            })),
        );
        let result = Self::paste_files(
            &peer,
            &mut target,
            &mut |target, out| {
                self.to_client.extend(encode(out));
                std::mem::swap(&mut self.target, target);
                self.pump();
                std::mem::swap(&mut self.target, target);
            },
            chunk,
        );
        self.target = target;
        result
    }

    fn paste_on_viewer(&mut self, chunk: u32) -> Option<Vec<(String, Vec<u8>)>> {
        let peer = Arc::clone(&self.viewer_peer);
        let mut viewer = std::mem::replace(
            &mut self.viewer,
            CliprdrClient::new(Box::new(Peer {
                state: Arc::default(),
                files: false,
            })),
        );
        let result = Self::paste_files(
            &peer,
            &mut viewer,
            &mut |viewer, out| {
                self.to_server.extend(encode(out));
                std::mem::swap(&mut self.viewer, viewer);
                self.pump();
                std::mem::swap(&mut self.viewer, viewer);
            },
            chunk,
        );
        self.viewer = viewer;
        result
    }
}

const CAP: u64 = 1024 * 1024;

#[test]
fn files_copied_on_the_viewer_paste_on_the_target() {
    let mut harness = Harness::new(Some(CAP));
    let big: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
    Harness::copy_files(
        &harness.viewer_peer,
        &[("notes.txt", b"hello world"), ("data.bin", &big)],
    );
    harness.pump();

    let pasted = harness
        .paste_on_target(4096)
        .expect("files offered to the target");
    assert_eq!(pasted[0], ("notes.txt".to_owned(), b"hello world".to_vec()));
    assert_eq!(pasted[1].0, "data.bin");
    assert_eq!(pasted[1].1, big);
    assert_eq!(
        harness.audits,
        [
            FileTransferRecord {
                direction: FileTransferDirection::Upload,
                path: "notes.txt".into(),
                size: 11,
            },
            FileTransferRecord {
                direction: FileTransferDirection::Upload,
                path: "data.bin".into(),
                size: 100_000,
            },
        ]
    );
}

#[test]
fn files_copied_on_the_target_paste_on_the_viewer() {
    let mut harness = Harness::new(Some(CAP));
    Harness::copy_files(&harness.target_peer, &[("report.pdf", b"%PDF-1.7 ...")]);
    harness.pump();

    let pasted = harness
        .paste_on_viewer(5)
        .expect("files offered to the viewer");
    assert_eq!(
        pasted,
        [("report.pdf".to_owned(), b"%PDF-1.7 ...".to_vec())]
    );
    // Every range the viewer asked for was re-issued to the target, and nothing more.
    let served = harness.target_peer.lock().unwrap().served.clone();
    assert_eq!(served, [(0, 0, 5), (0, 5, 5), (0, 10, 2)]);
    assert_eq!(
        harness.audits,
        [FileTransferRecord {
            direction: FileTransferDirection::Download,
            path: "report.pdf".into(),
            size: 12,
        }]
    );
}

#[test]
fn a_copy_over_the_cap_leaves_the_big_file_out() {
    let mut harness = Harness::new(Some(16));
    Harness::copy_files(
        &harness.target_peer,
        &[("small.txt", b"0123456789"), ("large.txt", b"0123456789")],
    );
    harness.pump();

    let pasted = harness
        .paste_on_viewer(64)
        .expect("the file that fits is offered");
    assert_eq!(pasted, [("small.txt".to_owned(), b"0123456789".to_vec())]);
}

#[test]
fn nothing_crosses_when_the_target_does_not_allow_files() {
    let mut harness = Harness::new(None);
    Harness::copy_files(&harness.viewer_peer, &[("a.txt", b"a")]);
    Harness::copy_files(&harness.target_peer, &[("b.txt", b"b")]);
    harness.pump();

    assert!(harness.paste_on_target(64).is_none());
    assert!(harness.paste_on_viewer(64).is_none());
    assert!(harness.target_peer.lock().unwrap().served.is_empty());
    assert!(harness.viewer_peer.lock().unwrap().served.is_empty());
    assert!(harness.audits.is_empty());
}

/// The target asks for more than the viewer's file holds past the declared size: the
/// relay only ever asks the viewer for bytes inside the declared size.
#[test]
fn ranges_never_reach_past_the_declared_size() {
    let mut harness = Harness::new(Some(CAP));
    Harness::copy_files(&harness.viewer_peer, &[("a.txt", b"abc")]);
    harness.pump();
    let pasted = harness.paste_on_target(1 << 20).unwrap();
    assert_eq!(pasted, [("a.txt".to_owned(), b"abc".to_vec())]);
    let served = harness.viewer_peer.lock().unwrap().served.clone();
    assert_eq!(served, [(0, 0, 3)]);
}
