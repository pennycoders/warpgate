use bytes::Bytes;
use serde::{Deserialize, Serialize};
use warpgate_core::{
    ClipboardFile, ClipboardFileOffer, ClipboardTransfer, DesktopEvent, DesktopInput, DesktopRect,
    DesktopState, FileContentsReply, FileContentsRequest, MAX_CLIPBOARD_BYTES, Scancode,
    truncate_clipboard_contents_in_place,
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
    },
    Bell,
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
    /// Whether this is an incremental framebuffer delta that may be dropped under
    /// output-buffer pressure, as opposed to a structural message (resize, clipboard,
    /// connection state) whose loss would corrupt or desync the client.
    pub const fn is_incremental(&self) -> bool {
        matches!(
            self,
            Self::RawImage { .. }
                | Self::JpegImage { .. }
                | Self::PngImage { .. }
                | Self::CopyRect { .. }
                | Self::Cursor { .. }
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
    /// `None` for state changes, which are reported as a session phase instead.
    pub fn from_event(event: DesktopEvent) -> Option<Self> {
        Some(match event {
            // The browser client doesn't draw the target's cursor shape, so there is nothing
            // to tell it when the cursor reverts or hides.
            DesktopEvent::State(_) | DesktopEvent::CursorDefault | DesktopEvent::CursorHidden => {
                return None;
            }
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
            },
            // Audit only: the recorder has it.
            DesktopEvent::FileTransferred(_) => return None,
            // The browser has no session drive yet.
            DesktopEvent::DriveUpload(_) => return None,
            DesktopEvent::Bell => Self::Bell,
            DesktopEvent::Error(message) => Self::Error { message },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    drive_uploads_max_bytes: None,
                }
            )),
            serde_json::json!({"type": "capabilities", "clipboard_files_max_bytes": null})
        );
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
