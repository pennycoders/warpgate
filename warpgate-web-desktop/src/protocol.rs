use bytes::Bytes;
use serde::{Deserialize, Serialize};
use warpgate_core::{
    DESKTOP_AUDIO_FORMAT, DesktopEvent, DesktopInput, DesktopRect, DesktopState,
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
    },
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
            DesktopEvent::Bell => Self::Bell,
            DesktopEvent::Audio(data) => Self::Audio { data },
            DesktopEvent::Error(message) => Self::Error { message },
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use warpgate_core::{DesktopEndReason, DesktopEvent};

    use super::{ClientMessage, ServerMessage, WsPayload};

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
        };
        assert!(!features.is_incremental());
    }

    #[test]
    fn features_are_sent_as_json() {
        let WsPayload::Text(json) = (ServerMessage::Features {
            audio_playback: true,
        })
        .ws_payload() else {
            panic!("features are text");
        };
        assert_eq!(json, r#"{"type":"features","audio_playback":true}"#);
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
}
