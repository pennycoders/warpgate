//! Viewer-facing RDP server: runs IronRDP's server state machine (including TLS) over
//! the accepted viewer socket, and bridges everything else to the owning Warpgate session
//! over [`Input`]/[`Event`] channels.
//!
//! Warpgate owns the listening socket, the session, authentication and recording; this
//! module owns only the RDP server protocol and its TLS.

use std::net::SocketAddr;
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use ironrdp::core::decode;
use ironrdp::displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};
use ironrdp::pdu::rdp::capability_sets::{BitmapCodecs, server_codecs_capabilities};
use ironrdp::pdu::rdp::server_error_info::ServerSetErrorInfoPdu;
use ironrdp_server::tokio_rustls::TlsAcceptor;
use ironrdp_server::tokio_rustls::rustls::ServerConfig as TlsServerConfig;
use ironrdp_server::{
    BitmapUpdate, CredentialDecision, CredentialValidationError, CredentialValidator, Credentials,
    DesktopSize, DisplayUpdate, GfxServerFactory, KeyboardEvent, MouseEvent, PixelFormat,
    RGBAPointer, RdpServer, RdpServerDisplay, RdpServerDisplayUpdates, RdpServerInputHandler,
    ServerEvent,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{Instrument as _, Span, debug, warn};
use warpgate_core::{DesktopInput, Scancode};
#[cfg(test)]
use warpgate_desktop_ui::DEFAULT_SIZE;

use super::clipboard::ViewerClipboard;
use super::gfx::{GraphicsFactory, Routed, ViewerGraphics};
use super::protocol::{AuthVerdict, Event, Input};
use super::sound::ViewerSound;
use super::surface::Request;
use crate::desktop_size::bound_desktop_size;
#[cfg(test)]
use crate::desktop_size::{MAX_DESKTOP_DIM, MAX_FRAME_BYTES, MIN_DESKTOP_DIM};

/// Run the RDP server on a dedicated thread, resolving to its result.
///
/// `RdpServer::run_connection` holds `Rc`s across await points, so its future is not
/// `Send` and cannot join the multi-threaded runtime. It gets a current-thread runtime of
/// its own instead; everything it exchanges with the session (the duplex transport and the
/// two channels) is `Send`, so only the state machine itself stays pinned here.
pub fn run_on_thread<S>(
    stream: S,
    cert_pem: String,
    key_pem: String,
    size: (u16, u16),
    graphics_pipeline: bool,
    out_tx: mpsc::UnboundedSender<Event>,
    in_rx: mpsc::Receiver<Input>,
) -> oneshot::Receiver<Result<()>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    let (done_tx, done_rx) = oneshot::channel();
    // The session span is thread-local, so it has to be carried over by hand: without it
    // nothing this server logs is attributed to the session.
    let span = Span::current();
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = done_tx.send(Err(anyhow::Error::new(error).context("RDP server runtime")));
                return;
            }
        };
        let result = runtime.block_on(
            run(
                stream,
                cert_pem,
                key_pem,
                size,
                graphics_pipeline,
                out_tx,
                in_rx,
            )
            .instrument(span),
        );
        let _ = done_tx.send(result);
    });
    done_rx
}

/// Run the RDP server state machine against an accepted viewer connection.
async fn run<S>(
    stream: S,
    cert_pem: String,
    key_pem: String,
    size: (u16, u16),
    graphics_pipeline: bool,
    out_tx: mpsc::UnboundedSender<Event>,
    in_rx: mpsc::Receiver<Input>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    // `ironrdp-server` pulls in tokio-rustls with the aws-lc-rs provider (its default);
    // install it as the process default so `ServerConfig::builder()` works.
    let _ = ironrdp_server::tokio_rustls::rustls::crypto::aws_lc_rs::default_provider()
        .install_default();

    let tls = build_tls_acceptor(&cert_pem, &key_pem)?;

    let (frame_tx, frame_rx) = mpsc::channel::<DisplayUpdate>(256);
    let size = Arc::new(Mutex::new(DesktopSize {
        width: size.0,
        height: size.1,
    }));

    let clipboard = ViewerClipboard::new(out_tx.clone());
    let sound = ViewerSound::new();
    let graphics = ViewerGraphics::new(*size.lock().await);
    let display_route = DisplayRouteState {
        size: Arc::clone(&size),
    };

    let display = DisplayHandler {
        size,
        initial_size_negotiated: false,
        updates: Arc::new(Mutex::new(frame_rx)),
        out: out_tx.clone(),
        graphics: graphics.factory(),
    };
    let input = InputHandler {
        out: out_tx.clone(),
        x: 0,
        y: 0,
        buttons: 0,
        graphics: graphics.factory(),
    };
    let validator: Arc<dyn CredentialValidator> = Arc::new(Validator {
        out: out_tx.clone(),
    });

    let mut server = RdpServer::builder()
        // `addr` is only used by `RdpServer::run()` (which binds a listener); we drive
        // `run_connection()` directly with the accepted stream, so it's a harmless
        // placeholder here.
        .with_addr(SocketAddr::from(([127, 0, 0, 1], 0)))
        .with_tls(tls)
        .with_input_handler(input)
        .with_display_handler(display)
        // Opt in to calling `request_initial_size`; without it the acceptor ignores the
        // viewer's requested resolution and pins the session to `size()`'s seed.
        .with_honor_client_desktop_size(true)
        .with_credential_validator(Some(validator))
        .with_cliprdr_factory(Some(Box::new(clipboard.clone())))
        // Offered to every viewer: the target is not known yet when channels are joined.
        .with_sound_factory(Some(Box::new(sound.clone())))
        .with_gfx_factory(
            graphics_pipeline.then(|| Box::new(graphics.factory()) as Box<dyn GfxServerFactory>),
        )
        .with_bitmap_codecs(bitmap_codecs(graphics_pipeline))
        .build();

    let router = tokio::spawn(
        route_input(
            in_rx,
            frame_tx,
            display_route,
            clipboard,
            sound,
            graphics,
            server.event_sender().clone(),
        )
        .in_current_span(),
    );

    if let Err(error) = server.run_connection(stream).await {
        // A session-level error (e.g. viewer disconnect) is not a failure of the gateway:
        // report it and shut down cleanly.
        warn!(error = %format!("{error:#}"), "RDP session ended with error");
    }

    router.abort();
    Ok(())
}

/// Build a rustls `TlsAcceptor` from PEM cert + key.
fn build_tls_acceptor(cert_pem: &str, key_pem: &str) -> Result<TlsAcceptor> {
    let mut cert_reader = cert_pem.as_bytes();
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .context("parsing TLS certificate PEM")?;
    if certs.is_empty() {
        anyhow::bail!("no certificate found in TLS certificate PEM");
    }

    let mut key_reader = key_pem.as_bytes();
    let key = rustls_pemfile::private_key(&mut key_reader)
        .context("parsing TLS private key PEM")?
        .context("no private key found in TLS private key PEM")?;

    let config = TlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building rustls server config")?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Shared display state used while routing Warpgate framebuffer messages.
struct DisplayRouteState {
    /// The size the bitmap path's encoder is rebuilt at on a reactivation.
    size: Arc<Mutex<DesktopSize>>,
}

impl DisplayRouteState {
    /// Sends bitmap-path updates in order; a resize is committed before it is sent, so the
    /// reactivation it starts rebuilds the encoder at the new size. `false` once the server
    /// is gone.
    async fn send(
        &self,
        frame_tx: &mpsc::Sender<DisplayUpdate>,
        updates: impl IntoIterator<Item = DisplayUpdate>,
    ) -> bool {
        for update in updates {
            if let DisplayUpdate::Resize(size) = &update {
                *self.size.lock().await = *size;
            }
            if frame_tx.send(update).await.is_err() {
                return false;
            }
        }
        true
    }
}

/// Route Warpgate's messages to the display backend (framebuffer updates) and the
/// credential validator (auth decisions).
async fn route_input(
    mut rx: mpsc::Receiver<Input>,
    frame_tx: mpsc::Sender<DisplayUpdate>,
    display: DisplayRouteState,
    clipboard: ViewerClipboard,
    sound: ViewerSound,
    mut graphics: ViewerGraphics,
    server_events: mpsc::UnboundedSender<ServerEvent>,
) {
    loop {
        let msg = tokio::select! {
            // The viewer's requests first: one is always sent before the frames painted for
            // it (the hold screen paints at the size the viewer negotiated).
            biased;
            Some(signal) = graphics.signal() => {
                let updates = graphics.handle(signal);
                if !display.send(&frame_tx, updates).await {
                    return;
                }
                continue;
            }
            msg = rx.recv() => msg,
        };
        let Some(msg) = msg else {
            break;
        };
        match msg {
            Input::Frame {
                x,
                y,
                width,
                height,
                data,
            } => {
                let update = match graphics.frame(x, y, width, height, &data) {
                    Routed::Sent => None,
                    Routed::AsIs => frame_to_update(x, y, width, height, data),
                    Routed::Bitmap(update) => Some(update),
                };
                if let Some(update) = update
                    && frame_tx.send(update).await.is_err()
                {
                    break;
                }
            }
            Input::Resize { width, height } => {
                // The target's desktop (or the hold screen) changed size. The viewer keeps
                // the surface it asked for, and the desktop is scaled into it until the two
                // match, so this never resizes the viewer.
                let updates = graphics.resize(DesktopSize { width, height });
                if !display.send(&frame_tx, updates).await {
                    break;
                }
            }
            Input::Cursor {
                hotspot_x,
                hotspot_y,
                width,
                height,
                data,
            } => {
                let update = cursor_to_update(hotspot_x, hotspot_y, width, height, &data);
                if frame_tx.send(update).await.is_err() {
                    break;
                }
            }
            Input::CursorDefault => {
                if frame_tx.send(DisplayUpdate::DefaultPointer).await.is_err() {
                    break;
                }
            }
            Input::CursorHidden => {
                if frame_tx.send(DisplayUpdate::HidePointer).await.is_err() {
                    break;
                }
            }
            Input::Clipboard(text) => clipboard.offer(text),
            Input::Audio(data) => sound.play(&data),
            Input::Shutdown => break,
            Input::Disconnect(code) => {
                match decode::<ServerSetErrorInfoPdu>(&code.to_le_bytes()) {
                    Ok(pdu) if server_events.send(ServerEvent::Disconnect(pdu.0)).is_ok() => {
                        // Ending the display before the server has sent the reason would
                        // close the session without it.
                        frame_tx.closed().await;
                    }
                    _ => warn!(code, "Could not pass the disconnect reason to the viewer"),
                }
                break;
            }
        }
    }
}

/// Convert a BGRA framebuffer rectangle into an IronRDP bitmap update. Returns `None` for
/// degenerate or short buffers rather than panicking downstream.
fn frame_to_update(x: u16, y: u16, width: u16, height: u16, data: Bytes) -> Option<DisplayUpdate> {
    let nz_width = NonZeroU16::new(width)?;
    let nz_height = NonZeroU16::new(height)?;
    let stride = NonZeroUsize::new(usize::from(width) * 4)?;
    if data.len() < usize::from(width) * usize::from(height) * 4 {
        warn!(width, height, len = data.len(), "ignoring undersized frame");
        return None;
    }
    Some(DisplayUpdate::Bitmap(BitmapUpdate {
        x,
        y,
        width: nz_width,
        height: nz_height,
        // The client emits bytes in B,G,R,A memory order (see its raw-image encoder),
        // which is exactly `PixelFormat::BgrA32`.
        format: PixelFormat::BgrA32,
        data,
        stride,
    }))
}

/// Largest cursor, in pixels per side, sent to the viewer. Bigger ones need the large-pointer
/// capability, which this server doesn't advertise, so a viewer can't be assumed to take them.
const MAX_CURSOR_DIM: u16 = 32;

/// Convert a BGRA cursor image (top row first) into an IronRDP pointer update. A cursor that
/// is malformed or too large for the viewer becomes the viewer's default arrow, so the viewer
/// never keeps showing a shape the target has already left.
fn cursor_to_update(
    hotspot_x: u16,
    hotspot_y: u16,
    width: u16,
    height: u16,
    data: &[u8],
) -> DisplayUpdate {
    let row_len = usize::from(width) * 4;
    if width == 0
        || height == 0
        || width > MAX_CURSOR_DIM
        || height > MAX_CURSOR_DIM
        || data.len() != row_len * usize::from(height)
    {
        warn!(
            width,
            height,
            len = data.len(),
            "unusable cursor, using the default"
        );
        return DisplayUpdate::DefaultPointer;
    }
    // The pointer PDU's 32bpp XOR mask is a bottom-up DIB, hence the reversed rows.
    // `RGBAPointer` is named for its channels, but the mask goes on the wire as given, and
    // the wire order is BGRA.
    let data = data
        .chunks_exact(row_len)
        .rev()
        .flatten()
        .copied()
        .collect();
    DisplayUpdate::RGBAPointer(RGBAPointer {
        // Every update replaces the one slot; the target's own cache isn't mirrored.
        cache_index: 0,
        width,
        height,
        hot_x: hotspot_x,
        hot_y: hotspot_y,
        data,
    })
}

/// The codecs offered for the bitmap path. With the Graphics Pipeline on, RemoteFX is left
/// out: a viewer encoding with it SHOULD NOT request display changes (MS-RDPEDISP 1.5), and
/// Windows App then sends no layout when the device rotates.
fn bitmap_codecs(graphics_pipeline: bool) -> BitmapCodecs {
    let config: &[&str] = if graphics_pipeline {
        &["remotefx:off"]
    } else {
        &[]
    };
    server_codecs_capabilities(config).unwrap_or_default()
}

/// Bound a viewer-supplied desktop size with [`bound_desktop_size`]. On this path it arrives
/// during the capability exchange, before the credential validator has run.
fn clamp_desktop_size(requested: DesktopSize) -> DesktopSize {
    let (width, height) = bound_desktop_size(requested.width, requested.height);
    DesktopSize { width, height }
}

fn primary_monitor_size(layout: &DisplayControlMonitorLayout) -> Option<DesktopSize> {
    let monitor = layout
        .monitors()
        .iter()
        .find(|monitor| monitor.is_primary())?;
    let (width, height) = monitor.dimensions();
    let (width, height) = MonitorLayoutEntry::adjust_display_size(width, height);
    Some(clamp_desktop_size(DesktopSize {
        width: u16::try_from(width).ok()?,
        height: u16::try_from(height).ok()?,
    }))
}

/// Display backend: hands the IronRDP server a receiver of framebuffer updates fed by
/// Warpgate.
struct DisplayHandler {
    /// `size()` is re-queried on every deactivation-reactivation to build the new
    /// `UpdateEncoder`, so this has to track the size last renegotiated with the client,
    /// not the one the session started at.
    size: Arc<Mutex<DesktopSize>>,
    /// Whether the initial viewer-driven size has been adopted. Gates
    /// [`Self::request_initial_size`] so reactivations keep the committed size instead of
    /// re-adopting a stale one the viewer echoes.
    initial_size_negotiated: bool,
    // Shared, not `take()`n: ironrdp-server calls `updates()` again after every
    // deactivation-reactivation (e.g. an mstsc / MS Remote Desktop resize), so a
    // one-shot receiver would fail the second connection with "already taken".
    updates: Arc<Mutex<mpsc::Receiver<DisplayUpdate>>>,
    out: mpsc::UnboundedSender<Event>,
    /// Told every size the viewer asks for, which its surface is then given.
    graphics: GraphicsFactory,
}

#[async_trait::async_trait]
impl RdpServerDisplay for DisplayHandler {
    async fn size(&mut self) -> DesktopSize {
        *self.size.lock().await
    }

    /// Whatever this returns is what the session runs at; RDP lets the server dictate, and
    /// the viewer resizes to match.
    ///
    /// ironrdp-server also calls this on every deactivation-reactivation, passing the size
    /// the viewer echoes in its Confirm Active. That echo can lag the resolution Warpgate
    /// just negotiated — mstsc, growing the window, still reports the smaller previous size —
    /// and adopting it would rebuild the encoder below the frames Warpgate is about to send,
    /// so every larger tile hits the "bitmap exceeds desktop size" drop (a grow paints black,
    /// while a shrink still fits and looks fine). So the viewer's size is honoured only for
    /// the initial negotiation; from then on the committed size — the one the viewer last
    /// asked for over Display Control — is authoritative and the viewer conforms to it.
    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        if self.initial_size_negotiated {
            let committed = *self.size.lock().await;
            debug!(
                ?client_size,
                ?committed,
                "reactivation: keeping committed size, viewer echo ignored"
            );
            return committed;
        }
        self.initial_size_negotiated = true;
        let size = clamp_desktop_size(client_size);
        debug!(?client_size, adopted = ?size, "initial viewer size negotiated");
        *self.size.lock().await = size;
        self.graphics.request(size, Request::Initial);
        let _ = self.out.send(Event::Size {
            width: size.width,
            height: size.height,
        });
        size
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        // Hand out a fresh view over the shared receiver. Only one is active at a time
        // (the previous is dropped when its client loop ends before reactivation).
        Ok(Box::new(DisplayUpdatesReceiver {
            rx: self.updates.clone(),
        }))
    }

    /// Every layout is answered at its size on the viewer's side, and passed on to the
    /// target, which follows if it can (the target client drops a size it was already asked
    /// for).
    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let Some(size) = primary_monitor_size(&layout) else {
            debug!(
                ?layout,
                "viewer sent a layout without a usable primary monitor"
            );
            return;
        };
        debug!(
            monitors = ?layout.monitors(),
            requested = ?size,
            "viewer requested a dynamic resize"
        );
        self.graphics.request(size, Request::Layout);
        let _ = self.out.send(Event::ResizeRequest {
            width: size.width,
            height: size.height,
        });
    }
}

struct DisplayUpdatesReceiver {
    rx: Arc<Mutex<mpsc::Receiver<DisplayUpdate>>>,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for DisplayUpdatesReceiver {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        // `mpsc::Receiver::recv` is cancellation-safe, as this trait requires; the guard
        // is released on cancel and no message is lost. `Resize` and the frames that follow
        // it share this one ordered channel, so the encoder is always resized before it is
        // handed a frame at the new size.
        Ok(self.rx.lock().await.recv().await)
    }
}

/// Credential validator: forwards the viewer's credentials to Warpgate and awaits its
/// verdict on a per-request reply channel.
struct Validator {
    out: mpsc::UnboundedSender<Event>,
}

#[async_trait::async_trait]
impl CredentialValidator for Validator {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> Result<CredentialDecision, CredentialValidationError> {
        let (reply, verdict) = oneshot::channel();
        if self
            .out
            .send(Event::AuthRequest {
                username: credentials.username.clone(),
                password: credentials.password.clone(),
                reply,
            })
            .is_err()
        {
            // Warpgate hung up before we could ask.
            return Ok(CredentialDecision::Reject);
        }
        match verdict.await {
            Ok(AuthVerdict::StartSession) => Ok(CredentialDecision::Accept),
            // Explicit denial, or the control loop dropped the reply without answering.
            Ok(AuthVerdict::Deny) | Err(_) => Ok(CredentialDecision::Reject),
        }
    }
}

/// Input handler: normalises IronRDP keyboard/mouse events onto the shared
/// `DesktopInput` stream.
struct InputHandler {
    out: mpsc::UnboundedSender<Event>,
    x: u16,
    y: u16,
    buttons: u8,
    /// Maps the viewer's pointer from its surface to the desktop.
    graphics: GraphicsFactory,
}

impl InputHandler {
    fn emit_pointer(&self) {
        let _ = self.out.send(Event::Input(DesktopInput::Pointer {
            x: self.x,
            y: self.y,
            buttons: self.buttons,
        }));
    }

    fn send_key(&self, keysym: Option<u32>, scancode: Option<Scancode>, down: bool) {
        let _ = self.out.send(Event::Input(DesktopInput::Key {
            keysym,
            scancode,
            down,
        }));
    }

    fn set_button(&mut self, bit: u8, down: bool) {
        if down {
            self.buttons |= 1 << bit;
        } else {
            self.buttons &= !(1 << bit);
        }
        self.emit_pointer();
    }
}

impl RdpServerInputHandler for InputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                self.send_key(None, Some(Scancode { code, extended }), true);
            }
            KeyboardEvent::Released { code, extended } => {
                self.send_key(None, Some(Scancode { code, extended }), false);
            }
            KeyboardEvent::UnicodePressed(unit) => {
                self.send_key(Some(u32::from(unit)), None, true);
            }
            KeyboardEvent::UnicodeReleased(unit) => {
                self.send_key(Some(u32::from(unit)), None, false);
            }
            // Lock-key (caps/num/scroll) sync state; not propagated to the target.
            KeyboardEvent::Synchronize(_) => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                (self.x, self.y) = self.graphics.to_desktop(x, y);
                self.emit_pointer();
            }
            MouseEvent::LeftPressed => self.set_button(0, true),
            MouseEvent::LeftReleased => self.set_button(0, false),
            MouseEvent::MiddlePressed => self.set_button(1, true),
            MouseEvent::MiddleReleased => self.set_button(1, false),
            MouseEvent::RightPressed => self.set_button(2, true),
            MouseEvent::RightReleased => self.set_button(2, false),
            MouseEvent::Button4Pressed => self.set_button(3, true),
            MouseEvent::Button4Released => self.set_button(3, false),
            MouseEvent::Button5Pressed => self.set_button(4, true),
            MouseEvent::Button5Released => self.set_button(4, false),
            MouseEvent::VerticalScroll { value } => {
                let _ = self.out.send(Event::Input(DesktopInput::Wheel {
                    x: self.x,
                    y: self.y,
                    vertical: true,
                    delta: wheel_notches(value),
                }));
            }
            MouseEvent::Scroll { x, y } => {
                // High-resolution (ainput) 2D scroll: prefer the vertical axis.
                let (vertical, magnitude) = if y != 0 { (true, y) } else { (false, x) };
                if magnitude != 0 {
                    let _ = self.out.send(Event::Input(DesktopInput::Wheel {
                        x: self.x,
                        y: self.y,
                        vertical,
                        delta: i16::try_from(magnitude.signum()).unwrap_or(0),
                    }));
                }
            }
            MouseEvent::RelMove { x, y } => {
                self.x = self.x.saturating_add_signed(clamp_to_i16(x));
                self.y = self.y.saturating_add_signed(clamp_to_i16(y));
                self.emit_pointer();
            }
        }
    }
}

/// RDP carries wheel rotation in units of 120 per detent; Warpgate's shared `Wheel` input
/// is a notch count (re-multiplied by 120 downstream), so divide, preserving sign for
/// sub-detent high-resolution wheels.
const fn wheel_notches(value: i16) -> i16 {
    let notches = value / 120;
    if notches == 0 {
        value.signum()
    } else {
        notches
    }
}

fn clamp_to_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod desktop_size_tests {
    use std::sync::Arc;

    use ironrdp::displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};
    use tokio::sync::{Mutex, mpsc};

    use super::{
        DEFAULT_SIZE, DesktopSize, DisplayHandler, MAX_DESKTOP_DIM, MAX_FRAME_BYTES,
        MIN_DESKTOP_DIM, RdpServerDisplay, ViewerGraphics, clamp_desktop_size,
        primary_monitor_size,
    };
    use crate::server::protocol::Event;

    fn size(width: u16, height: u16) -> DesktopSize {
        DesktopSize { width, height }
    }

    fn single_primary_layout(width: u32, height: u32) -> DisplayControlMonitorLayout {
        DisplayControlMonitorLayout::new(&[MonitorLayoutEntry::new_primary(width, height).unwrap()])
            .unwrap()
    }

    #[test]
    fn honours_a_reasonable_request() {
        assert_eq!(clamp_desktop_size(size(2560, 1440)), size(2560, 1440));
    }

    #[test]
    fn clamps_degenerate_and_oversized_dimensions() {
        assert_eq!(
            clamp_desktop_size(size(0, 0)),
            size(MIN_DESKTOP_DIM, MIN_DESKTOP_DIM)
        );
        // Clamped per dimension, and the resulting area still has to fit one frame.
        let clamped = clamp_desktop_size(size(u16::MAX, 240));
        assert_eq!(clamped, size(MAX_DESKTOP_DIM, 240));
    }

    #[test]
    fn falls_back_when_a_full_frame_would_not_fit() {
        // 8192x8192 BGRA is 256 MB, past MAX_FRAME_BYTES, so neither dimension is trusted.
        let fallback = clamp_desktop_size(size(MAX_DESKTOP_DIM, MAX_DESKTOP_DIM));
        assert_eq!(fallback, size(DEFAULT_SIZE.0, DEFAULT_SIZE.1));
        let frame_len = usize::from(fallback.width) * usize::from(fallback.height) * 4;
        assert!(frame_len <= MAX_FRAME_BYTES);
    }

    #[test]
    fn uses_primary_monitor_for_dynamic_layout() {
        let layout = DisplayControlMonitorLayout::new(&[
            MonitorLayoutEntry::new_primary(1920, 1080).unwrap(),
            MonitorLayoutEntry::new_secondary(1280, 720)
                .unwrap()
                .with_position(1920, 0)
                .unwrap(),
        ])
        .unwrap();

        assert_eq!(primary_monitor_size(&layout), Some(size(1920, 1080)));
    }

    #[test]
    fn normalizes_dynamic_layout_size() {
        let layout = single_primary_layout(1921, 1080);

        assert_eq!(primary_monitor_size(&layout), Some(size(1920, 1080)));
    }

    fn display_handler(
        committed: DesktopSize,
        initial_size_negotiated: bool,
    ) -> (DisplayHandler, mpsc::UnboundedReceiver<Event>) {
        let (_tx, rx) = mpsc::channel(1);
        let (out, events) = mpsc::unbounded_channel();
        let display = DisplayHandler {
            size: Arc::new(Mutex::new(committed)),
            initial_size_negotiated,
            updates: Arc::new(Mutex::new(rx)),
            out,
            graphics: ViewerGraphics::new(committed).factory(),
        };
        (display, events)
    }

    /// Every layout goes to the target, even one at the surface's size: the viewer's side has
    /// answered it already, and the target may still be at another size.
    #[test]
    fn dynamic_layout_request_is_forwarded_to_the_target() {
        let (mut display, mut events) = display_handler(size(800, 600), true);

        display.request_layout(single_primary_layout(800, 600));
        assert!(matches!(
            events.try_recv(),
            Ok(Event::ResizeRequest {
                width: 800,
                height: 600,
            })
        ));

        display.request_layout(single_primary_layout(1024, 768));
        assert!(matches!(
            events.try_recv(),
            Ok(Event::ResizeRequest {
                width: 1024,
                height: 768,
            })
        ));
    }

    #[tokio::test]
    async fn initial_size_adopts_the_viewer_request() {
        let (mut display, mut events) =
            display_handler(size(DEFAULT_SIZE.0, DEFAULT_SIZE.1), false);

        assert_eq!(
            display.request_initial_size(size(1024, 768)).await,
            size(1024, 768)
        );
        assert_eq!(display.size().await, size(1024, 768));
        assert!(matches!(
            events.try_recv(),
            Ok(Event::Size {
                width: 1024,
                height: 768,
            })
        ));
    }

    /// On reactivation the viewer can echo a size that lags the freshly negotiated one; the
    /// committed (target-driven) size must win, or a grow would rebuild the encoder too small
    /// and drop every oversized tile.
    #[tokio::test]
    async fn reactivation_keeps_the_committed_size() {
        let (mut display, mut events) = display_handler(size(1920, 1080), true);

        assert_eq!(
            display.request_initial_size(size(1024, 768)).await,
            size(1920, 1080)
        );
        assert_eq!(display.size().await, size(1920, 1080));
        assert!(events.try_recv().is_err());
    }
}

#[cfg(test)]
mod disconnect_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ironrdp::pdu::rdp::server_error_info::{ErrorInfo, ProtocolIndependentCode};
    use ironrdp_server::{DesktopSize, DisplayUpdate, ServerEvent};
    use tokio::sync::{Mutex, mpsc};

    use super::{
        DisplayRouteState, Input, ViewerClipboard, ViewerGraphics, ViewerSound, route_input,
    };

    struct Router {
        input: mpsc::Sender<Input>,
        events: mpsc::UnboundedReceiver<ServerEvent>,
        frames: mpsc::Receiver<DisplayUpdate>,
        task: tokio::task::JoinHandle<()>,
    }

    fn router() -> Router {
        let (input, rx) = mpsc::channel(4);
        let (frame_tx, frames) = mpsc::channel(4);
        let (out, _) = mpsc::unbounded_channel();
        let (server_events, events) = mpsc::unbounded_channel();
        let size = DesktopSize {
            width: 800,
            height: 600,
        };
        let display = DisplayRouteState {
            size: Arc::new(Mutex::new(size)),
        };
        let task = tokio::spawn(route_input(
            rx,
            frame_tx,
            display,
            ViewerClipboard::new(out),
            ViewerSound::new(),
            ViewerGraphics::new(size),
            server_events,
        ));
        Router {
            input,
            events,
            frames,
            task,
        }
    }

    #[tokio::test]
    async fn the_viewer_is_told_why_and_the_display_outlives_the_reason() {
        let mut router = router();
        router.input.send(Input::Disconnect(5)).await.unwrap();

        assert!(matches!(
            router.events.recv().await,
            Some(ServerEvent::Disconnect(ErrorInfo::ProtocolIndependentCode(
                ProtocolIndependentCode::DisconnectedByOtherconnection
            )))
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !router.task.is_finished(),
            "the display must stay open until the server has ended the session"
        );

        drop(router.frames);
        tokio::time::timeout(Duration::from_secs(5), router.task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn an_unknown_code_still_ends_the_session() {
        let mut router = router();
        router
            .input
            .send(Input::Disconnect(0xDEAD_BEEF))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), router.task)
            .await
            .unwrap()
            .unwrap();
        assert!(router.events.try_recv().is_err());
        drop(router.frames);
    }

    #[tokio::test]
    async fn a_plain_shutdown_sends_no_reason() {
        let mut router = router();
        router.input.send(Input::Shutdown).await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), router.task)
            .await
            .unwrap()
            .unwrap();
        assert!(router.events.try_recv().is_err());
    }
}

#[cfg(test)]
mod graphics_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use ironrdp::core::encode_vec;
    use ironrdp::dvc::DvcProcessor as _;
    use ironrdp::pdu::rdp::capability_sets::CodecProperty;
    use ironrdp_egfx::client::{GraphicsPipelineClient, GraphicsPipelineHandler};
    use ironrdp_server::{DesktopSize, DisplayUpdate, ServerEvent, ServerEventSender as _};
    use tokio::sync::{Mutex, mpsc};

    use super::{
        DisplayRouteState, Input, ViewerClipboard, ViewerGraphics, ViewerSound, bitmap_codecs,
        route_input,
    };

    struct Painter;

    impl GraphicsPipelineHandler for Painter {}

    #[test]
    fn remotefx_is_offered_only_without_the_pipeline() {
        let remotefx = |graphics_pipeline| {
            bitmap_codecs(graphics_pipeline).0.iter().any(|codec| {
                matches!(
                    codec.property,
                    CodecProperty::RemoteFx(_) | CodecProperty::ImageRemoteFx(_)
                )
            })
        };
        assert!(remotefx(false));
        // oracle: spec MS-RDPEDISP §1.5
        assert!(!remotefx(true));
    }

    fn frame() -> Input {
        Input::Frame {
            x: 0,
            y: 0,
            width: 8,
            height: 12,
            data: Bytes::from(vec![0x40; 8 * 12 * 4]),
        }
    }

    #[tokio::test]
    async fn an_open_pipeline_takes_frames_and_resizes_from_the_bitmap_path() {
        let graphics = ViewerGraphics::new(DesktopSize {
            width: 16,
            height: 8,
        });
        let mut factory = graphics.factory();
        let (server_events, mut events) = mpsc::unbounded_channel();
        factory.set_sender(server_events.clone());
        let (mut bridge, _) =
            ironrdp_server::GfxServerFactory::build_server_with_handle(&factory).expect("a bridge");
        bridge.start(7).unwrap();

        let (input, rx) = mpsc::channel(4);
        let (frame_tx, mut frames) = mpsc::channel(4);
        let (out, _) = mpsc::unbounded_channel();
        let display = DisplayRouteState {
            size: Arc::new(Mutex::new(DesktopSize {
                width: 16,
                height: 8,
            })),
        };
        tokio::spawn(route_input(
            rx,
            frame_tx,
            display,
            ViewerClipboard::new(out),
            ViewerSound::new(),
            graphics,
            server_events,
        ));

        input.send(frame()).await.unwrap();
        assert!(matches!(
            frames.recv().await,
            Some(DisplayUpdate::Bitmap(_))
        ));

        let mut client = GraphicsPipelineClient::new(Box::new(Painter), None);
        for advertise in client.start(7).unwrap() {
            bridge
                .process(7, &encode_vec(advertise.as_ref()).unwrap())
                .unwrap();
        }
        assert!(matches!(events.recv().await, Some(ServerEvent::Egfx(_))));

        input
            .send(Input::Resize {
                width: 8,
                height: 12,
            })
            .await
            .unwrap();
        input.send(frame()).await.unwrap();
        assert!(matches!(events.recv().await, Some(ServerEvent::Egfx(_))));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            frames.try_recv().is_err(),
            "the bitmap path must not reactivate or paint under the pipeline"
        );
    }
}

#[cfg(test)]
mod cursor_tests {
    use ironrdp::graphics::pointer::{DecodedPointer, PointerBitmapTarget};
    use ironrdp::pdu::pointer::{ColorPointerAttribute, Point16, PointerAttribute};
    use ironrdp_server::DisplayUpdate;

    use super::{MAX_CURSOR_DIM, cursor_to_update};

    /// What a viewer decodes from the update: the target's RGBA pixels come back out.
    #[test]
    fn viewer_decodes_the_cursor_the_target_sent() {
        // 2x2, BGRA, top row first: red, green / blue, translucent white.
        let bgra = [
            [0, 0, 255, 255],
            [0, 255, 0, 255],
            [255, 0, 0, 255],
            [255, 255, 255, 128],
        ]
        .concat();
        let DisplayUpdate::RGBAPointer(pointer) = cursor_to_update(1, 0, 2, 2, &bgra) else {
            panic!("expected a pointer update");
        };
        assert_eq!((pointer.hot_x, pointer.hot_y), (1, 0));

        let decoded = DecodedPointer::decode_pointer_attribute(
            &PointerAttribute {
                xor_bpp: 32,
                color_pointer: ColorPointerAttribute {
                    cache_index: pointer.cache_index,
                    hot_spot: Point16 {
                        x: pointer.hot_x,
                        y: pointer.hot_y,
                    },
                    width: pointer.width,
                    height: pointer.height,
                    xor_mask: &pointer.data,
                    and_mask: &[],
                },
            },
            PointerBitmapTarget::Accelerated,
        )
        .unwrap();
        let rgba = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 128],
        ]
        .concat();
        assert_eq!(decoded.bitmap_data, rgba);
    }

    #[test]
    fn unusable_cursors_fall_back_to_the_default() {
        let pixels =
            |width: u16, height: u16| vec![0; usize::from(width) * usize::from(height) * 4];

        assert!(matches!(
            cursor_to_update(0, 0, 0, 0, &[]),
            DisplayUpdate::DefaultPointer
        ));
        assert!(matches!(
            cursor_to_update(0, 0, 2, 2, &pixels(2, 1)),
            DisplayUpdate::DefaultPointer
        ));
        let too_big = MAX_CURSOR_DIM + 1;
        assert!(matches!(
            cursor_to_update(0, 0, too_big, 1, &pixels(too_big, 1)),
            DisplayUpdate::DefaultPointer
        ));
        assert!(matches!(
            cursor_to_update(
                0,
                0,
                MAX_CURSOR_DIM,
                MAX_CURSOR_DIM,
                &pixels(MAX_CURSOR_DIM, MAX_CURSOR_DIM)
            ),
            DisplayUpdate::RGBAPointer(_)
        ));
    }
}

#[cfg(test)]
mod framed_tests {
    use std::time::Duration;

    use ironrdp::pdu::X224_HINT;
    use ironrdp_tokio::TokioFramed;
    use tokio::io::AsyncWriteExt as _;

    /// A fast-path header whose length field is zero is a frame `X224_HINT` neither
    /// matches nor can skip past. The reader has to fail it; looping on it would pin a
    /// core for as long as the process lives, long after the sender has gone away.
    #[tokio::test]
    async fn zero_length_unmatched_frame_fails_instead_of_spinning() {
        let (server_side, mut client_side) = tokio::io::duplex(64);
        client_side.write_all(&[0x00, 0x00]).await.unwrap();
        let mut framed = TokioFramed::new(server_side);
        let result = tokio::time::timeout(Duration::from_secs(5), framed.read_by_hint(&X224_HINT))
            .await
            .expect("read_by_hint spun instead of returning");
        assert!(result.is_err());
    }
}
