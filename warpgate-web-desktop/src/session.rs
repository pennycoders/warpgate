use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use uuid::Uuid;
use warpgate_common::UserSessionId;
use warpgate_core::recordings::DesktopRecorder;
use warpgate_core::{DesktopEvent, DesktopInput, DesktopRect, Framebuffer, Rect};
use warpgate_web_clients_common::{ManagedSession, Sheddable, WebSession};

use crate::protocol::{ServerMessage, WsRect};

/// Initial output-buffer allocation. Not a hard cap: incremental frames are bounded by
/// [`MAX_BUFFERED_INCREMENTAL`] and structural messages are rare, so the buffer stays small.
const OUTPUT_BUFFER_CAPACITY: usize = 128;

/// How many incremental (droppable) framebuffer frames to keep buffered for a slow or
/// briefly-disconnected client. Small on purpose: a client that falls behind should get
/// the recent live edge, not a huge stale backlog to grind through (which is what froze
/// the browser on high-frequency updates). Structural messages are never counted or dropped.
const MAX_BUFFERED_INCREMENTAL: usize = 64;

impl Sheddable for ServerMessage {
    fn is_droppable(&self) -> bool {
        self.is_incremental()
    }
}

pub struct DesktopBackend {
    pub input_tx: Sender<DesktopInput>,
    pub recorder: Option<Arc<DesktopRecorder>>,
}

pub struct WebDesktopSession {
    core: WebSession<ServerMessage>,
    backend: OnceLock<DesktopBackend>,
    /// Composited surface, kept so a viewer attaching mid-session gets a base image.
    /// Separate from the recorder's: that one must only see events it actually wrote.
    /// Holds the refinement scratch too, so encoding one region needs a single lock.
    framebuffer: Mutex<(Framebuffer, RefineScratch)>,
    /// What the target offers, replayed to every viewer that attaches.
    features: OnceLock<ServerMessage>,
    /// A viewer turned sound on. Sound is not sent before, as nothing would play it.
    audio_enabled: AtomicBool,
    /// The target's recording, replayed to every viewer that attaches while it lasts.
    microphone: std::sync::Mutex<Option<ServerMessage>>,
    /// The target's last capabilities announcement, replayed to every viewer that attaches
    /// so a reloaded page still knows which features work.
    capabilities: std::sync::Mutex<Option<ServerMessage>>,
}

/// Buffers reused across refinement encodes. A settling screen refines many regions back to
/// back, and `rgba` is a full copy of each region's pixels — a fresh allocation per region
/// would churn megabytes for a once-per-settle job.
#[derive(Default)]
struct RefineScratch {
    rgba: Vec<u8>,
    png: Vec<u8>,
}

impl WebDesktopSession {
    pub fn new(
        id: UserSessionId,
        user_id: Uuid,
        target_name: String,
        target_kind: warpgate_db_entities::Target::TargetKind,
        server_handle: Arc<tokio::sync::Mutex<warpgate_core::WarpgateServerHandle>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            core: WebSession::new(
                id,
                user_id,
                target_name,
                target_kind,
                server_handle,
                cancel,
                OUTPUT_BUFFER_CAPACITY,
                MAX_BUFFERED_INCREMENTAL,
            ),
            backend: OnceLock::new(),
            framebuffer: Mutex::new((Framebuffer::default(), RefineScratch::default())),
            features: OnceLock::new(),
            audio_enabled: AtomicBool::new(false),
            microphone: std::sync::Mutex::new(None),
            capabilities: std::sync::Mutex::new(None),
        }
    }

    pub fn remember_capabilities(&self, message: &ServerMessage) {
        *self
            .capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.clone());
    }

    pub fn capabilities(&self) -> Option<ServerMessage> {
        self.capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn bind_backend(&self, backend: DesktopBackend) {
        let _ = self.backend.set(backend);
    }

    /// Tell viewers what the target offers; see [`ServerMessage::Features`].
    pub async fn announce_features(&self, audio_playback: bool, microphone: bool) {
        let features = ServerMessage::Features {
            audio_playback,
            microphone,
        };
        if self.features.set(features.clone()).is_ok() {
            self.push(features).await;
        }
    }

    /// The features for a viewer that just attached, once the backend has announced them.
    pub fn features(&self) -> Option<ServerMessage> {
        self.features.get().cloned()
    }

    /// Remember whether the target records, for viewers that attach later.
    pub fn track_microphone(&self, message: &ServerMessage) {
        let state = match message {
            ServerMessage::MicrophoneOpen { .. } => Some(message.clone()),
            ServerMessage::MicrophoneClose => None,
            _ => return,
        };
        *self
            .microphone
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    }

    /// Whether the target records, for a viewer that just attached.
    pub fn microphone(&self) -> Option<ServerMessage> {
        self.microphone
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_audio_enabled(&self, enabled: bool) {
        self.audio_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn audio_enabled(&self) -> bool {
        self.audio_enabled.load(Ordering::Relaxed)
    }

    /// Composite an event into the session's surface. Driven from the manager's event loop
    /// on the *pre-JPEG* stream, so this stays a plain blit with no decode round-trip.
    pub async fn composite(&self, event: &DesktopEvent) {
        self.framebuffer.lock().await.0.apply(event);
    }

    /// A full-canvas snapshot for a viewer that just attached. `None` until the backend has
    /// painted something, so a viewer is never handed a black surface to reveal.
    ///
    /// Encodes under the lock: it runs once per attach, and copying the surface out to
    /// offload it would cost more than the encode saves.
    pub async fn keyframe(&self) -> Option<ServerMessage> {
        let (width, height, data) = self.framebuffer.lock().await.0.snapshot_png()?;
        Some(ServerMessage::Keyframe {
            rect: WsRect {
                x: 0,
                y: 0,
                width,
                height,
            },
            data: data.into(),
        })
    }

    /// The current pixels of a region, as a lossless PNG event. Used to refine a region
    /// that was sent lossily and has since gone quiet; `None` once it falls outside the
    /// surface (a resize since it was marked dirty).
    pub async fn refinement(&self, rect: DesktopRect) -> Option<DesktopEvent> {
        let mut guard = self.framebuffer.lock().await;
        let (framebuffer, scratch) = &mut *guard;
        let clipped = framebuffer.region_png(
            Rect {
                x: u32::from(rect.x),
                y: u32::from(rect.y),
                width: u32::from(rect.width),
                height: u32::from(rect.height),
            },
            &mut scratch.rgba,
            &mut scratch.png,
        )?;
        Some(DesktopEvent::PngImage {
            rect: DesktopRect {
                x: u16::try_from(clipped.x).ok()?,
                y: u16::try_from(clipped.y).ok()?,
                width: u16::try_from(clipped.width).ok()?,
                height: u16::try_from(clipped.height).ok()?,
            },
            // Copied out so the scratch keeps its capacity for the next region.
            data: Bytes::copy_from_slice(&scratch.png),
        })
    }

    pub async fn send_input(&self, input: DesktopInput) {
        let Some(backend) = self.backend.get() else {
            return;
        };
        // Record the viewer's input for audit before forwarding (like native RDP/VNC).
        if let Some(recorder) = &backend.recorder
            && let Err(error) = recorder.write_input(&input).await
        {
            warn!(%error, "Failed to record web-desktop viewer input");
        }
        if matches!(input, DesktopInput::ClipboardTransfer(_)) {
            // A lost file step would stall a transfer until it times out; these are paced
            // by the pull model, so waiting for room is cheap.
            let _ = backend.input_tx.send(input).await;
            return;
        }
        // let inputs drop under backpressure
        let _ = backend.input_tx.try_send(input);
    }
}

impl std::ops::Deref for WebDesktopSession {
    type Target = WebSession<ServerMessage>;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl ManagedSession for WebDesktopSession {
    fn id(&self) -> UserSessionId {
        self.core.id()
    }

    fn user_id(&self) -> Uuid {
        self.core.user_id()
    }

    fn on_removed(&self) {
        self.core.abort();
    }
}
