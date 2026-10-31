use bytes::Bytes;
use serde::{Deserialize, Serialize};
use warpgate_core::{
    ClipboardFile, ClipboardFileOffer, ClipboardTransfer, DESKTOP_AUDIO_FORMAT, DesktopEvent,
    DesktopInput, DesktopRect, DesktopState, DriveUpload, FileContentsReply, FileContentsRequest,
    MAX_CLIPBOARD_BYTES, Scancode, truncate_clipboard_contents_in_place,
};
use warpgate_web_clients_common::{LiveSessionPhase, SessionPhase};

#[derive(Debug, Clone, Copy, Serialize)]
pub struct WsRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl From<DesktopRect> for WsRect {
    fn from(r: DesktopRect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

/// One entry of a clipboard file offer, as the browser sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WsClipboardFile {
    pub path: String,
    pub size: u64,
    #[serde(default)]
    pub directory: bool,
}

impl From<ClipboardFile> for WsClipboardFile {
    fn from(file: ClipboardFile) -> Self {
        Self {
            path: file.path,
            size: file.size,
            directory: file.directory,
        }
    }
}

impl From<WsClipboardFile> for ClipboardFile {
    fn from(file: WsClipboardFile) -> Self {
        Self {
            path: file.path,
            size: file.size,
            directory: file.directory,
        }
    }
}

/// File bytes on the WebSocket, base64 in the JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct WsBytes(#[serde(with = "warpgate_common::helpers::serde_base64")] pub Bytes);

/// Messages sent from the browser to the server.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    PointerEvent {
        x: u16,
        y: u16,
        buttons: u8,
    },
    /// The browser reports both forms it can derive from a `KeyboardEvent`: the keysym
    /// from `event.key` (already mapped through the client's layout) and the scancode
    /// from `event.code` (the physical key). Older clients send only the keysym.
    KeyEvent {
        keysym: Option<u32>,
        #[serde(default)]
        scancode: Option<u8>,
        #[serde(default)]
        extended: bool,
        down: bool,
    },
    WheelEvent {
        x: u16,
        y: u16,
        vertical: bool,
        delta: i16,
    },
    Clipboard {
        text: String,
    },
    Refresh,
    Resize {
        width: u16,
        height: u16,
    },
    /// Whether this viewer plays the target's sound. Sound is sent only while a viewer
    /// asked for it, which it can do only after a user gesture.
    Audio {
        enabled: bool,
    },
    /// Files the user pasted or dropped into the desktop. The browser keeps them and
    /// serves their ranges as the target asks (`file_contents`).
    ClipboardFiles {
        offer: u32,
        files: Vec<WsClipboardFile>,
    },
    /// A range of a file the target copied, for a download.
    FileContentsRequest {
        id: u32,
        offer: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    /// The answer to a server `file_contents_request`; `data` absent on failure.
    FileContents {
        id: u32,
        #[serde(default)]
        data: Option<WsBytes>,
    },
    /// Start uploading a file to the session drive; its chunks follow in order.
    DriveUploadStart {
        id: u32,
        name: String,
        size: u64,
    },
    DriveUploadChunk {
        id: u32,
        data: WsBytes,
    },
    DriveUploadDelete {
        id: u32,
    },
}

impl From<ClientMessage> for Option<DesktopInput> {
    fn from(msg: ClientMessage) -> Self {
        Some(match msg {
            ClientMessage::PointerEvent { x, y, buttons } => {
                DesktopInput::Pointer { x, y, buttons }
            }
            ClientMessage::KeyEvent {
                keysym,
                scancode,
                extended,
                down,
            } => DesktopInput::Key {
                keysym,
                scancode: scancode.map(|code| Scancode { code, extended }),
                down,
            },
            ClientMessage::WheelEvent {
                x,
                y,
                vertical,
                delta,
            } => DesktopInput::Wheel {
                x,
                y,
                vertical,
                delta,
            },
            ClientMessage::Clipboard { mut text } => {
                truncate_clipboard_contents_in_place(&mut text, MAX_CLIPBOARD_BYTES);
                DesktopInput::Clipboard(text)
            }
            ClientMessage::Refresh => DesktopInput::Refresh,
            ClientMessage::Resize { width, height } => DesktopInput::Resize { width, height },
            ClientMessage::Audio { .. } => return None,
            // Limits are applied where the transfer meets the target, as for native viewers.
            ClientMessage::ClipboardFiles { offer, files } => {
                DesktopInput::ClipboardTransfer(ClipboardTransfer::Offer(ClipboardFileOffer {
                    offer,
                    files: files.into_iter().map(Into::into).collect(),
                }))
            }
            ClientMessage::FileContentsRequest {
                id,
                offer,
                index,
                offset,
                length,
            } => DesktopInput::ClipboardTransfer(ClipboardTransfer::Request(FileContentsRequest {
                id,
                offer,
                index,
                offset,
                length,
            })),
            ClientMessage::FileContents { id, data } => {
                DesktopInput::ClipboardTransfer(ClipboardTransfer::Reply(FileContentsReply {
                    id,
                    data: data.map(|data| data.0),
                }))
            }
            // Name, size and capacity are checked by the drive itself.
            ClientMessage::DriveUploadStart { id, name, size } => {
                DesktopInput::DriveUpload(DriveUpload::Start { id, name, size })
            }
            ClientMessage::DriveUploadChunk { id, data } => {
                DesktopInput::DriveUpload(DriveUpload::Chunk { id, data: data.0 })
            }
            ClientMessage::DriveUploadDelete { id } => {
                DesktopInput::DriveUpload(DriveUpload::Delete { id })
            }
        })
    }
}

/// Messages sent from the server to the browser.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    State(SessionPhase),
    Resize {
        width: u16,
        height: u16,
    },
    RawImage {
        rect: WsRect,
        #[serde(with = "warpgate_common::helpers::serde_base64")]
        data: Bytes,
    },
    JpegImage {
        rect: WsRect,
        #[serde(with = "warpgate_common::helpers::serde_base64")]
        data: Bytes,
    },
    PngImage {
        rect: WsRect,
        #[serde(with = "warpgate_common::helpers::serde_base64")]
        data: Bytes,
    },
    /// Full-canvas PNG snapshot handed to a viewer as it attaches, so it has a base image
    /// to paint deltas onto. Structural: a viewer that loses this one paints onto black.
    Keyframe {
        rect: WsRect,
        #[serde(with = "warpgate_common::helpers::serde_base64")]
        data: Bytes,
    },
    CopyRect {
        dst: WsRect,
        src_x: u16,
        src_y: u16,
    },
    Cursor {
        rect: WsRect,
        #[serde(with = "warpgate_common::helpers::serde_base64")]
        data: Bytes,
    },
    Clipboard {
        text: String,
    },
    /// Files the target copied, offered for download.
    ClipboardFiles {
        offer: u32,
        files: Vec<WsClipboardFile>,
    },
    /// A range of a file the browser offered, which the target is reading.
    FileContentsRequest {
        id: u32,
        offer: u32,
        index: u32,
        offset: u64,
        length: u32,
    },
    /// The answer to a browser `file_contents_request`; `data` absent on failure.
    FileContents {
        id: u32,
        data: Option<WsBytes>,
    },
    /// What the target allows; `clipboard_files_max_bytes` is absent when file transfer
    /// is off.
    Capabilities {
        clipboard_files_max_bytes: Option<u64>,
        drive_uploads_max_bytes: Option<u64>,
    },
    /// An upload to the session drive was stored, or refused.
    DriveUpload {
        id: u32,
        stored: bool,
    },
    Bell,
    /// A block of the target's sound, interleaved signed 16-bit little-endian PCM in
    /// [`DESKTOP_AUDIO_FORMAT`].
    Audio {
        #[serde(with = "warpgate_common::helpers::serde_base64")]
        data: Bytes,
    },
    /// What the target offers beyond the screen, so the viewer shows only the controls
    /// that do something. Sent as the backend connects, and again to every viewer that
    /// attaches later.
    Features {
        audio_playback: bool,
        microphone: bool,
    },
    /// The target records the viewer's microphone: the viewer sends interleaved signed
    /// 16-bit little-endian PCM at `sample_rate` with `channels` channels, as binary frames
    /// (see [`decode_client_binary`]), ideally `frames_per_packet` sample frames at a time.
    MicrophoneOpen {
        channels: u16,
        sample_rate: u32,
        frames_per_packet: u32,
    },
    /// The target stopped recording.
    MicrophoneClose,
    /// A non-fatal notice; the session ends through [`ServerMessage::State`] alone.
    Error {
        message: String,
    },
}

impl From<SessionPhase> for ServerMessage {
    fn from(phase: SessionPhase) -> Self {
        Self::State(phase)
    }
}

impl ServerMessage {
    /// Whether this is an incremental framebuffer delta or a block of sound, which may be
    /// dropped under output-buffer pressure, as opposed to a structural message (resize,
    /// clipboard, connection state) whose loss would corrupt or desync the client.
    pub const fn is_incremental(&self) -> bool {
        matches!(
            self,
            Self::RawImage { .. }
                | Self::JpegImage { .. }
                | Self::PngImage { .. }
                | Self::CopyRect { .. }
                | Self::Cursor { .. }
                | Self::Audio { .. }
        )
    }

    /// Encode for the WebSocket. Pixel-carrying frames are sent as compact **binary**
    /// (`[kind: u8][x,y,w,h: u16 LE][pixels…]`) so large buffers avoid the base64 +
    /// JSON-string cost that otherwise pins a core on the hot path; everything else stays
    /// small JSON text.
    pub fn ws_payload(&self) -> WsPayload {
        match self {
            Self::RawImage { rect, data } => WsPayload::Binary(encode_image(1, *rect, data)),
            Self::JpegImage { rect, data } => WsPayload::Binary(encode_image(2, *rect, data)),
            Self::Cursor { rect, data } => WsPayload::Binary(encode_image(3, *rect, data)),
            Self::Keyframe { rect, data } => WsPayload::Binary(encode_image(4, *rect, data)),
            Self::PngImage { rect, data } => WsPayload::Binary(encode_image(5, *rect, data)),
            Self::Audio { data } => WsPayload::Binary(encode_audio(data)),
            other => WsPayload::Text(serde_json::to_string(other).unwrap_or_default()),
        }
    }
}

/// A WebSocket payload: small control messages as JSON text, pixel frames as binary.
pub enum WsPayload {
    Text(String),
    Binary(Vec<u8>),
}

/// `[kind: u8][x: u16 LE][y: u16 LE][width: u16 LE][height: u16 LE][pixels…]`.
fn encode_image(kind: u8, rect: WsRect, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(9 + data.len());
    out.push(kind);
    out.extend_from_slice(&rect.x.to_le_bytes());
    out.extend_from_slice(&rect.y.to_le_bytes());
    out.extend_from_slice(&rect.width.to_le_bytes());
    out.extend_from_slice(&rect.height.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// `[kind = 6: u8][channels: u8][sample rate: u32 LE][interleaved s16le samples…]`.
fn encode_audio(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + data.len());
    out.push(6);
    out.push(u8::try_from(DESKTOP_AUDIO_FORMAT.channels).unwrap_or(u8::MAX));
    out.extend_from_slice(&DESKTOP_AUDIO_FORMAT.samples_per_sec.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// The binary frame kind for the viewer's microphone: `[7][interleaved s16le samples…]`.
const MICROPHONE_FRAME: u8 = 7;

/// The most microphone audio one frame may carry: a second of 48 kHz stereo.
const MAX_MICROPHONE_FRAME_BYTES: usize = 48_000 * 4;

/// A binary frame from the browser. The only kind is the viewer's microphone.
pub fn decode_client_binary(mut frame: Vec<u8>) -> Option<DesktopInput> {
    if frame.first() != Some(&MICROPHONE_FRAME) || frame.len() > 1 + MAX_MICROPHONE_FRAME_BYTES {
        return None;
    }
    frame.remove(0);
    (!frame.is_empty()).then(|| DesktopInput::Microphone(frame.into()))
}

/// A backend state change as a session phase; `None` for the backend's own end, which
/// the lifecycle reports once with a reason.
pub const fn phase_for(state: DesktopState) -> Option<LiveSessionPhase> {
    match state {
        DesktopState::Connecting => Some(LiveSessionPhase::Connecting),
        DesktopState::Connected => Some(LiveSessionPhase::Connected),
        DesktopState::Disconnected => None,
    }
}

impl ServerMessage {
    /// `None` for state changes and the end of the session, which are reported as a session
    /// phase instead.
    pub fn from_event(event: DesktopEvent) -> Option<Self> {
        Some(match event {
            // The browser client doesn't draw the target's cursor shape, so there is nothing
            // to tell it when the cursor reverts or hides; a session end is reported on close.
            DesktopEvent::State(_)
            | DesktopEvent::CursorDefault
            | DesktopEvent::CursorHidden
            | DesktopEvent::Ended(_) => return None,
            DesktopEvent::Resize { width, height } => Self::Resize { width, height },
            DesktopEvent::RawImage { rect, data } => Self::RawImage {
                rect: rect.into(),
                data,
            },
            DesktopEvent::JpegImage { rect, data } => Self::JpegImage {
                rect: rect.into(),
                data,
            },
            DesktopEvent::PngImage { rect, data } => Self::PngImage {
                rect: rect.into(),
                data,
            },
            DesktopEvent::CopyRect { dst, src_x, src_y } => Self::CopyRect {
                dst: dst.into(),
                src_x,
                src_y,
            },
            DesktopEvent::Cursor { rect, data } => Self::Cursor {
                rect: rect.into(),
                data,
            },
            DesktopEvent::Clipboard(text) => Self::Clipboard { text },
            DesktopEvent::ClipboardTransfer(ClipboardTransfer::Offer(offer)) => {
                Self::ClipboardFiles {
                    offer: offer.offer,
                    files: offer.files.into_iter().map(Into::into).collect(),
                }
            }
            DesktopEvent::ClipboardTransfer(ClipboardTransfer::Request(request)) => {
                Self::FileContentsRequest {
                    id: request.id,
                    offer: request.offer,
                    index: request.index,
                    offset: request.offset,
                    length: request.length,
                }
            }
            DesktopEvent::ClipboardTransfer(ClipboardTransfer::Reply(reply)) => {
                Self::FileContents {
                    id: reply.id,
                    data: reply.data.map(WsBytes),
                }
            }
            DesktopEvent::Capabilities(capabilities) => Self::Capabilities {
                clipboard_files_max_bytes: capabilities.clipboard_files_max_bytes,
                drive_uploads_max_bytes: capabilities.drive_uploads_max_bytes,
            },
            DesktopEvent::DriveUpload(status) => Self::DriveUpload {
                id: status.id,
                stored: status.stored,
            },
            // Audit only: the recorder has it.
            DesktopEvent::FileTransferred(_) => return None,
            DesktopEvent::Bell => Self::Bell,
            DesktopEvent::Audio(data) => Self::Audio { data },
            DesktopEvent::MicrophoneOpen {
                format,
                frames_per_packet,
            } => Self::MicrophoneOpen {
                channels: format.channels,
                sample_rate: format.samples_per_sec,
                frames_per_packet,
            },
            DesktopEvent::MicrophoneClose => Self::MicrophoneClose,
            DesktopEvent::Error(message) => Self::Error { message },
        })
    }
}

#[cfg(test)]
mod tests {
    use warpgate_core::DesktopEndReason;

    use super::*;

    #[test]
    fn sound_goes_to_the_browser_as_a_binary_frame_with_its_format() {
        let Some(message) =
            ServerMessage::from_event(DesktopEvent::Audio(Bytes::from_static(&[1, 2, 3, 4])))
        else {
            panic!("sound is relayed");
        };
        let WsPayload::Binary(frame) = message.ws_payload() else {
            panic!("sound is sent as binary");
        };
        assert_eq!(frame, [6, 2, 0x44, 0xAC, 0, 0, 1, 2, 3, 4]);
    }

    #[test]
    fn sound_is_shed_like_frame_deltas() {
        let audio = ServerMessage::Audio {
            data: Bytes::from_static(&[0; 4]),
        };
        assert!(audio.is_incremental());
        let features = ServerMessage::Features {
            audio_playback: true,
            microphone: false,
        };
        assert!(!features.is_incremental());
        assert!(!ServerMessage::MicrophoneClose.is_incremental());
    }

    #[test]
    fn features_are_sent_as_json() {
        let WsPayload::Text(json) = (ServerMessage::Features {
            audio_playback: true,
            microphone: true,
        })
        .ws_payload() else {
            panic!("features are text");
        };
        assert_eq!(
            json,
            r#"{"type":"features","audio_playback":true,"microphone":true}"#
        );
    }

    fn input(json: &str) -> DesktopInput {
        let message: ClientMessage = serde_json::from_str(json).unwrap();
        Option::<DesktopInput>::from(message).unwrap()
    }

    #[test]
    fn browser_file_messages_become_transfer_steps() {
        let DesktopInput::ClipboardTransfer(ClipboardTransfer::Offer(offer)) =
            input(r#"{"type":"clipboard_files","offer":3,"files":[{"path":"a.txt","size":5}]}"#)
        else {
            panic!("expected an offer");
        };
        assert_eq!(offer.offer, 3);
        assert_eq!(
            offer.files,
            [ClipboardFile {
                path: "a.txt".into(),
                size: 5,
                directory: false,
            }]
        );

        let DesktopInput::ClipboardTransfer(ClipboardTransfer::Request(request)) = input(
            r#"{"type":"file_contents_request","id":1,"offer":2,"index":0,"offset":10,"length":20}"#,
        ) else {
            panic!("expected a request");
        };
        assert_eq!(
            (request.id, request.offer, request.offset, request.length),
            (1, 2, 10, 20)
        );

        let DesktopInput::ClipboardTransfer(ClipboardTransfer::Reply(reply)) =
            input(r#"{"type":"file_contents","id":4,"data":"aGVsbG8="}"#)
        else {
            panic!("expected a reply");
        };
        assert_eq!(reply.data.as_deref(), Some(&b"hello"[..]));

        let DesktopInput::ClipboardTransfer(ClipboardTransfer::Reply(reply)) =
            input(r#"{"type":"file_contents","id":4}"#)
        else {
            panic!("expected a reply");
        };
        assert_eq!(reply.data, None);
    }

    fn drive_step(input: DesktopInput) -> Option<DriveUpload> {
        match input {
            DesktopInput::DriveUpload(step) => Some(step),
            _ => None,
        }
    }

    #[test]
    fn browser_uploads_become_drive_steps() {
        assert_eq!(
            drive_step(input(
                r#"{"type":"drive_upload_start","id":1,"name":"a.txt","size":2}"#
            )),
            Some(DriveUpload::Start {
                id: 1,
                name: "a.txt".into(),
                size: 2,
            })
        );
        assert_eq!(
            drive_step(input(
                r#"{"type":"drive_upload_chunk","id":1,"data":"aGk="}"#
            )),
            Some(DriveUpload::Chunk {
                id: 1,
                data: Bytes::from_static(b"hi"),
            })
        );
        assert_eq!(
            drive_step(input(r#"{"type":"drive_upload_delete","id":1}"#)),
            Some(DriveUpload::Delete { id: 1 })
        );
    }

    #[test]
    fn invalid_base64_from_the_browser_is_rejected() {
        assert!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"file_contents","id":4,"data":"%%"}"#)
                .is_err()
        );
    }

    fn json(event: DesktopEvent) -> serde_json::Value {
        let message = ServerMessage::from_event(event).unwrap();
        assert!(
            !message.is_incremental(),
            "file messages must never be shed"
        );
        match message.ws_payload() {
            WsPayload::Text(text) => serde_json::from_str(&text).unwrap(),
            WsPayload::Binary(_) => panic!("expected JSON"),
        }
    }

    #[test]
    fn target_file_steps_reach_the_browser_as_json() {
        assert_eq!(
            json(DesktopEvent::ClipboardTransfer(ClipboardTransfer::Offer(
                ClipboardFileOffer {
                    offer: 1,
                    files: vec![ClipboardFile {
                        path: "dir/b.bin".into(),
                        size: 2,
                        directory: false,
                    }],
                }
            ))),
            serde_json::json!({
                "type": "clipboard_files",
                "offer": 1,
                "files": [{"path": "dir/b.bin", "size": 2, "directory": false}],
            })
        );
        assert_eq!(
            json(DesktopEvent::ClipboardTransfer(ClipboardTransfer::Reply(
                FileContentsReply {
                    id: 9,
                    data: Some(Bytes::from_static(b"hi")),
                }
            ))),
            serde_json::json!({"type": "file_contents", "id": 9, "data": "aGk="})
        );
        assert_eq!(
            json(DesktopEvent::Capabilities(
                warpgate_core::DesktopCapabilities {
                    clipboard_files_max_bytes: None,
                    drive_uploads_max_bytes: Some(5),
                }
            )),
            serde_json::json!({
                "type": "capabilities",
                "clipboard_files_max_bytes": null,
                "drive_uploads_max_bytes": 5,
            })
        );
        assert_eq!(
            json(DesktopEvent::DriveUpload(
                warpgate_core::DriveUploadStatus {
                    id: 3,
                    stored: true,
                }
            )),
            serde_json::json!({"type": "drive_upload", "id": 3, "stored": true})
        );
    }

    #[test]
    fn the_browser_is_told_the_format_the_target_records_in() {
        let message = ServerMessage::from_event(DesktopEvent::MicrophoneOpen {
            format: warpgate_core::DESKTOP_MICROPHONE_FORMATS[6],
            frames_per_packet: 160,
        })
        .unwrap();
        let WsPayload::Text(json) = message.ws_payload() else {
            panic!("microphone control is text");
        };
        assert_eq!(
            json,
            r#"{"type":"microphone_open","channels":1,"sample_rate":16000,"frames_per_packet":160}"#
        );
        let WsPayload::Text(json) = ServerMessage::from_event(DesktopEvent::MicrophoneClose)
            .unwrap()
            .ws_payload()
        else {
            panic!("microphone control is text");
        };
        assert_eq!(json, r#"{"type":"microphone_close"}"#);
    }

    #[test]
    fn microphone_frames_from_the_browser_become_input() {
        let Some(warpgate_core::DesktopInput::Microphone(data)) =
            super::decode_client_binary(vec![7, 1, 2, 3, 4])
        else {
            panic!("expected microphone input");
        };
        assert_eq!(&data[..], &[1, 2, 3, 4]);
        assert!(super::decode_client_binary(vec![7]).is_none());
        assert!(super::decode_client_binary(vec![1, 1, 2]).is_none());
        assert!(super::decode_client_binary(vec![]).is_none());
        let mut oversized = vec![7];
        oversized.resize(2 + 48_000 * 4, 0);
        assert!(super::decode_client_binary(oversized).is_none());
    }

    #[test]
    fn the_sound_switch_is_not_forwarded_to_the_target() {
        let message: ClientMessage =
            serde_json::from_str(r#"{"type":"audio","enabled":true}"#).unwrap();
        assert!(matches!(message, ClientMessage::Audio { enabled: true }));
        assert!(Option::<warpgate_core::DesktopInput>::from(message).is_none());
    }

    /// The end reaches the browser as the session's closing phase, not as a message.
    #[test]
    fn the_end_of_the_session_is_not_a_frame_message() {
        let ended = DesktopEvent::Ended(DesktopEndReason {
            message: "Another user connected to the server.".into(),
            rdp_error_info: Some(5),
        });
        assert!(ServerMessage::from_event(ended).is_none());
    }

    #[test]
    fn audit_records_stay_on_the_server() {
        assert!(
            ServerMessage::from_event(DesktopEvent::FileTransferred(
                warpgate_core::FileTransferRecord {
                    direction: warpgate_core::FileTransferDirection::Upload,
                    path: "a".into(),
                    size: 1,
                }
            ))
            .is_none()
        );
    }
}
