//! The desktop on the viewer's surface, over the Graphics Pipeline (MS-RDPEGFX) for viewers
//! that open it and over the bitmap path for the others.
//!
//! The viewer's surface is always the size the viewer last asked for (see
//! [`super::surface`]): each monitor layout is answered with a ResetGraphics, or on the
//! bitmap path a deactivation-reactivation, at exactly that size, whether or not the target
//! follows. Windows App needs that: it draws the last surface it was given over its whole
//! screen, so a surface of another shape is stretched when the device turns. The target's
//! desktop is scaled into the surface until it has the same size, then drawn 1:1.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use ironrdp_dvc::{DvcMessage, encode_dvc_messages};
use ironrdp_egfx::pdu::{CapabilitiesAdvertisePdu, CapabilitySet};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer, MixedTilePayload};
use ironrdp_graphics::clearcodec::ClearCodecEncoder;
use ironrdp_graphics::zgfx::CompressionMode;
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::geometry::ExclusiveRectangle;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, EgfxServerMessage, GfxContext, GfxDvcBridge,
    GfxServerFactory, GfxServerHandle, PixelFormat, ServerEvent, ServerEventSender,
};
use ironrdp_svc::ChannelFlags;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::surface::{Answer, Painter, Placement, Rect, Request, ViewerSurface};

/// Past this many pending rectangles they are sent as their bounding box.
const MAX_TILES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signal {
    Ready,
    Acknowledged,
    Closed,
    /// The viewer asked for its surface to be this size.
    Viewer(DesktopSize, Request),
}

struct Shared {
    events: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
    pipeline: Mutex<Option<GfxServerHandle>>,
    signals: mpsc::UnboundedSender<Signal>,
    /// Where the desktop is on the viewer's surface, for the pointer.
    placement: Mutex<Placement>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub(super) struct GraphicsFactory(Arc<Shared>);

impl GraphicsFactory {
    /// Passes on the viewer's request for its surface size.
    pub(super) fn request(&self, size: DesktopSize, request: Request) {
        let _ = self.0.signals.send(Signal::Viewer(size, request));
    }

    /// Maps a point on the viewer's surface to the desktop.
    pub(super) fn to_desktop(&self, x: u16, y: u16) -> (u16, u16) {
        lock(&self.0.placement).to_desktop(x, y)
    }
}

impl ServerEventSender for GraphicsFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *lock(&self.0.events) = Some(sender);
    }
}

impl GraphicsFactory {
    fn handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(Handler(self.0.signals.clone()))
    }

    /// The pipeline of one connection, whose handle the router keeps for sending frames.
    pub(super) fn pipeline(&self) -> (GfxDvcBridge, GfxServerHandle) {
        let server =
            GraphicsPipelineServer::with_compression(self.handler(), CompressionMode::Auto);
        let handle = Arc::new(Mutex::new(server));
        *lock(&self.0.pipeline) = Some(Arc::clone(&handle));
        (GfxDvcBridge::new(Arc::clone(&handle)), handle)
    }
}

impl GfxServerFactory for GraphicsFactory {
    fn build_gfx_handler(&self, _ctx: GfxContext) -> Box<dyn GraphicsPipelineHandler> {
        self.handler()
    }

    fn build_server_with_handle(
        &self,
        _ctx: GfxContext,
    ) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        Some(self.pipeline())
    }
}

/// Runs inside the pipeline's lock, so it only tells the router what happened.
struct Handler(mpsc::UnboundedSender<Signal>);

impl GraphicsPipelineHandler for Handler {
    fn capabilities_advertise(&mut self, _pdu: &CapabilitiesAdvertisePdu) {}

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        debug!(?negotiated, "viewer opened the graphics pipeline");
        let _ = self.0.send(Signal::Ready);
    }

    fn on_frame_ack(&mut self, _frame_id: u32, _queue_depth: u32, _total_frames_decoded: u32) {
        let _ = self.0.send(Signal::Acknowledged);
    }

    fn on_close(&mut self) {
        let _ = self.0.send(Signal::Closed);
    }
}

/// Where a frame from the target went.
#[derive(Debug)]
pub(super) enum Routed {
    /// Through the pipeline, or nowhere: nothing more to send.
    Sent,
    /// The bitmap path takes it as it is: the desktop is the viewer's surface.
    AsIs,
    /// The bitmap path takes the surface's pixels it changed.
    Bitmap(DisplayUpdate),
}

/// The router's side: keeps a copy of the desktop and draws it on the viewer's surface,
/// over the pipeline while the viewer has it open (the changed rectangles as ClearCodec
/// tiles) and as bitmap updates otherwise.
pub(super) struct ViewerGraphics {
    shared: Arc<Shared>,
    signals: mpsc::UnboundedReceiver<Signal>,
    /// The target's desktop; until the first frame size is known, the surface's.
    desktop: Option<DesktopSize>,
    /// BGRA rows of the desktop.
    canvas: Vec<u8>,
    surface: ViewerSurface,
    painter: Painter,
    /// The pipeline surface, while the pipeline is open.
    pipeline_surface: Option<u16>,
    encoder: ClearCodecEncoder,
    dirty: Vec<Rect>,
    /// The size the bitmap path's encoder is at.
    bitmap: DesktopSize,
    started: Instant,
}

impl ViewerGraphics {
    /// Starts with the viewer's surface at `size`, the size the server advertises.
    pub(super) fn new(size: DesktopSize) -> Self {
        let (signals_tx, signals) = mpsc::unbounded_channel();
        let placement = Placement::identity(size);
        let mut graphics = Self {
            shared: Arc::new(Shared {
                events: Mutex::default(),
                pipeline: Mutex::default(),
                signals: signals_tx,
                placement: Mutex::new(placement),
            }),
            signals,
            desktop: None,
            canvas: Vec::new(),
            surface: ViewerSurface::new(size),
            painter: Painter::new(placement),
            pipeline_surface: None,
            encoder: ClearCodecEncoder::new(),
            dirty: Vec::new(),
            bitmap: size,
            started: Instant::now(),
        };
        graphics.fit_canvas();
        graphics
    }

    pub(super) fn factory(&self) -> GraphicsFactory {
        GraphicsFactory(Arc::clone(&self.shared))
    }

    pub(super) async fn signal(&mut self) -> Option<Signal> {
        self.signals.recv().await
    }

    /// Updates for the bitmap path.
    pub(super) fn handle(&mut self, signal: Signal) -> Vec<DisplayUpdate> {
        match signal {
            Signal::Ready => {
                self.open();
                Vec::new()
            }
            Signal::Acknowledged => {
                self.flush();
                Vec::new()
            }
            Signal::Closed => self.close(),
            Signal::Viewer(size, request) => self.requested(size, request),
        }
    }

    /// Takes a frame of the target's desktop.
    pub(super) fn frame(&mut self, x: u16, y: u16, width: u16, height: u16, data: &[u8]) -> Routed {
        // From the first frame on the desktop keeps its size until told otherwise, so a later
        // layout scales what is on it rather than clearing it.
        self.desktop.get_or_insert_with(|| self.surface.size());
        let rect = self.clip(x, y, width, height);
        let columns = usize::from(rect.width()) * 4;
        let src_stride = usize::from(width) * 4;
        for (row, top) in (rect.top..rect.bottom).enumerate() {
            let at = self.offset(rect.left, top);
            let (Some(src), Some(dst)) = (
                data.get(row * src_stride..row * src_stride + columns),
                self.canvas.get_mut(at..at + columns),
            ) else {
                break;
            };
            dst.copy_from_slice(src);
        }
        if self.pipeline_surface.is_none() {
            if self.painter.placement().is_identity() && self.bitmap == self.desktop_size() {
                return Routed::AsIs;
            }
            if rect.is_empty() {
                return Routed::Sent;
            }
            let output =
                self.painter
                    .placement()
                    .to_output(rect.left, rect.top, rect.right, rect.bottom);
            return self
                .bitmap_update(output)
                .map_or(Routed::Sent, Routed::Bitmap);
        }
        if !rect.is_empty() {
            let output =
                self.painter
                    .placement()
                    .to_output(rect.left, rect.top, rect.right, rect.bottom);
            self.dirty.push(output);
        }
        self.flush();
        Routed::Sent
    }

    /// The target's desktop is now `size`. The viewer's surface stays as it is.
    pub(super) fn resize(&mut self, size: DesktopSize) -> Vec<DisplayUpdate> {
        if self.desktop == Some(size) {
            return Vec::new();
        }
        self.desktop = Some(size);
        self.fit_canvas();
        let placement = self.relayout();
        let surface = self.surface.size();
        if size == surface {
            debug!(
                ?size,
                "target desktop has the viewer's size; drawing it 1:1"
            );
        } else {
            debug!(
                desktop = ?size,
                ?surface,
                scaled = ?placement.scaled,
                scale = %format_args!("{:.3}", placement.scale()),
                "target desktop differs from the viewer's surface; scaling it in"
            );
        }
        if self.pipeline_surface.is_some() {
            self.dirty = vec![self.full()];
            self.flush();
            return Vec::new();
        }
        if placement.is_identity() {
            // The target's next update covers all of it.
            return Vec::new();
        }
        self.bitmap_update(self.full()).into_iter().collect()
    }

    /// Answers the viewer's request for its surface size.
    fn requested(&mut self, size: DesktopSize, request: Request) -> Vec<DisplayUpdate> {
        let answer = self.surface.request(size, request);
        if request == Request::Initial {
            // The connection is negotiated at this size.
            self.bitmap = size;
            self.fit_canvas();
            self.relayout();
            debug!(?size, "viewer surface negotiated");
            return Vec::new();
        }
        let Answer::Reset { changed } = answer else {
            debug!(?size, "viewer repeated the layout just answered");
            return Vec::new();
        };
        if self.desktop.is_none() {
            self.fit_canvas();
        }
        let placement = self.relayout();
        let pipeline = self.pipeline_surface.is_some();
        debug!(
            requested = ?size,
            applied = ?self.surface.size(),
            desktop = ?placement.desktop,
            scaled = ?placement.scaled,
            scale = %format_args!("{:.3}", placement.scale()),
            path = if pipeline { "graphics pipeline" } else { "bitmap" },
            "answering the viewer's layout at its size"
        );
        if pipeline {
            self.reset();
            return Vec::new();
        }
        if !changed {
            return Vec::new();
        }
        self.bitmap = size;
        let desktop = self.bitmap_update(self.full());
        core::iter::once(DisplayUpdate::Resize(size))
            .chain(desktop)
            .collect()
    }

    fn open(&mut self) {
        // A (re)negotiation starts the viewer's ClearCodec state from scratch.
        self.encoder = ClearCodecEncoder::new();
        self.relayout();
        self.reset();
    }

    /// Resets the viewer's graphics to its surface and sends all of it.
    fn reset(&mut self) {
        let Some(pipeline) = self.pipeline() else {
            return;
        };
        let size = self.surface.size();
        self.pipeline_surface = reset(&mut lock(&pipeline), size);
        debug!(?size, "reset the viewer's graphics");
        self.dirty = vec![self.full()];
        self.flush();
    }

    /// Places the desktop on the surface again.
    fn relayout(&mut self) -> Placement {
        let placement = Placement::fit(self.desktop_size(), self.surface.size());
        if *self.painter.placement() != placement {
            self.painter = Painter::new(placement);
            *lock(&self.shared.placement) = placement;
        }
        placement
    }

    fn close(&mut self) -> Vec<DisplayUpdate> {
        if self.pipeline_surface.take().is_none() {
            return Vec::new();
        }
        self.dirty.clear();
        let surface = self.surface.size();
        let resize = (surface != self.bitmap).then_some(DisplayUpdate::Resize(surface));
        self.bitmap = surface;
        let desktop = self.bitmap_update(self.full());
        resize.into_iter().chain(desktop).collect()
    }

    /// The surface's pixels in `rect` as a bitmap update.
    fn bitmap_update(&self, rect: Rect) -> Option<DisplayUpdate> {
        let (Some(width), Some(height)) = (
            NonZeroU16::new(rect.width()),
            NonZeroU16::new(rect.height()),
        ) else {
            return None;
        };
        let stride = NonZeroUsize::new(usize::from(rect.width()) * 4)?;
        Some(DisplayUpdate::Bitmap(BitmapUpdate {
            x: rect.left,
            y: rect.top,
            width,
            height,
            format: PixelFormat::BgrA32,
            data: Bytes::from(self.painter.paint(&self.canvas, rect)),
            stride,
        }))
    }

    /// Sends the pending rectangles unless the viewer is behind on acknowledgements, and
    /// whatever else the pipeline has queued.
    fn flush(&mut self) {
        let (Some(surface), Some(pipeline)) = (self.pipeline_surface, self.pipeline()) else {
            return;
        };
        let mut pipeline = lock(&pipeline);
        if !self.dirty.is_empty() && !pipeline.should_backpressure() {
            if self.dirty.len() > MAX_TILES {
                self.dirty = self.dirty.drain(..).reduce(union).into_iter().collect();
            }
            let tiles = core::mem::take(&mut self.dirty)
                .into_iter()
                .filter(|rect| !rect.is_empty())
                .map(|rect| MixedTilePayload::ClearCodec {
                    bitmap_data: self.encoder.encode(
                        &self.painter.paint(&self.canvas, rect),
                        rect.width(),
                        rect.height(),
                    ),
                    destination: ExclusiveRectangle {
                        left: rect.left,
                        top: rect.top,
                        right: rect.right,
                        bottom: rect.bottom,
                    },
                })
                .collect();
            let timestamp = u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX);
            pipeline.send_mixed_frame(surface, tiles, timestamp);
        }
        let messages = pipeline.drain_output();
        let channel_id = pipeline.channel_id();
        drop(pipeline);
        self.send(channel_id, messages);
    }

    fn send(&self, channel_id: Option<u32>, messages: Vec<DvcMessage>) {
        let Some(channel_id) = channel_id.filter(|_| !messages.is_empty()) else {
            return;
        };
        match encode_dvc_messages(channel_id, messages, ChannelFlags::SHOW_PROTOCOL) {
            Ok(messages) => {
                if let Some(events) = lock(&self.shared.events).as_ref() {
                    let _ = events.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                        messages,
                    }));
                }
            }
            Err(error) => warn!(%error, "Could not encode graphics pipeline output"),
        }
    }

    fn pipeline(&self) -> Option<GfxServerHandle> {
        lock(&self.shared.pipeline).clone()
    }

    fn desktop_size(&self) -> DesktopSize {
        self.desktop.unwrap_or_else(|| self.surface.size())
    }

    fn fit_canvas(&mut self) {
        let size = self.desktop_size();
        let len = usize::from(size.width) * usize::from(size.height) * 4;
        if self.canvas.len() != len {
            self.canvas = vec![0; len];
        }
    }

    fn clip(&self, x: u16, y: u16, width: u16, height: u16) -> Rect {
        let size = self.desktop_size();
        let left = x.min(size.width);
        let top = y.min(size.height);
        Rect {
            left,
            top,
            right: x.saturating_add(width).clamp(left, size.width),
            bottom: y.saturating_add(height).clamp(top, size.height),
        }
    }

    const fn full(&self) -> Rect {
        let size = self.surface.size();
        Rect {
            left: 0,
            top: 0,
            right: size.width,
            bottom: size.height,
        }
    }

    fn offset(&self, x: u16, y: u16) -> usize {
        (usize::from(y) * usize::from(self.desktop_size().width) + usize::from(x)) * 4
    }
}

/// Resets the viewer's graphics to `size` with one primary monitor covering it: Windows App
/// lays the output out by the monitors (MS-RDPEGFX 2.2.2.14), and with none it keeps the
/// geometry it connected with. Returns the new surface, mapped to the output.
fn reset(pipeline: &mut GraphicsPipelineServer, size: DesktopSize) -> Option<u16> {
    let monitor = Monitor {
        left: 0,
        top: 0,
        right: i32::from(size.width) - 1,
        bottom: i32::from(size.height) - 1,
        flags: MonitorFlags::PRIMARY,
    };
    pipeline.resize_with_monitors(size.width, size.height, vec![monitor]);
    let surface = pipeline.create_surface(size.width, size.height)?;
    pipeline.map_surface_to_output(surface, 0, 0);
    Some(surface)
}

fn union(a: Rect, b: Rect) -> Rect {
    Rect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ironrdp_core::{Decode as _, ReadCursor, decode_cursor, encode_vec};
    use ironrdp_dvc::DvcProcessor as _;
    use ironrdp_dvc::pdu::{DrdynvcDataPdu, DrdynvcServerPdu};
    use ironrdp_egfx::client::GraphicsPipelineClient;
    use ironrdp_egfx::pdu::{GfxPdu, ResetGraphicsPdu};
    use ironrdp_graphics::zgfx::Decompressor;

    use super::*;

    const CHANNEL: u32 = 7;

    /// Counts the tiles the client decodes.
    struct Counter(Arc<AtomicUsize>);

    impl ironrdp_egfx::client::GraphicsPipelineHandler for Counter {
        fn on_bitmap_updated(&mut self, _update: &ironrdp_egfx::client::BitmapUpdate) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The router's graphics on one side of the pipeline and IronRDP's client on the other.
    struct Viewer {
        graphics: ViewerGraphics,
        bridge: GfxDvcBridge,
        events: mpsc::UnboundedReceiver<ServerEvent>,
        client: GraphicsPipelineClient,
        /// The client's RGBA output, as of the last [`Self::deliver`].
        screen: Vec<u8>,
        width: u16,
        /// Acknowledgements the client sent and the server has not been given.
        acks: Vec<Vec<u8>>,
        tiles: Arc<AtomicUsize>,
        /// A second decoder of the server's output, for the ResetGraphics PDUs themselves.
        decompressor: Decompressor,
        resets: Vec<ResetGraphicsPdu>,
    }

    impl Viewer {
        /// A viewer that connected at `surface`.
        fn new(surface: DesktopSize) -> Self {
            let mut graphics = ViewerGraphics::new(size(1024, 768));
            assert!(
                graphics
                    .handle(Signal::Viewer(surface, Request::Initial))
                    .is_empty()
            );
            let mut factory = graphics.factory();
            let (sender, events) = mpsc::unbounded_channel();
            factory.set_sender(sender);
            let (mut bridge, _) = factory.pipeline();
            bridge.start(CHANNEL).unwrap();
            let tiles = Arc::new(AtomicUsize::new(0));
            Self {
                graphics,
                bridge,
                events,
                client: GraphicsPipelineClient::new(Box::new(Counter(Arc::clone(&tiles))), None),
                screen: Vec::new(),
                width: 0,
                acks: Vec::new(),
                tiles,
                decompressor: Decompressor::new(),
                resets: Vec::new(),
            }
        }

        /// Negotiates the pipeline and hands the router its `Ready`.
        fn open(&mut self) {
            for advertise in self.client.start(CHANNEL).unwrap() {
                let confirm = self
                    .bridge
                    .process(CHANNEL, &encode_vec(advertise.as_ref()).unwrap())
                    .unwrap();
                for message in confirm {
                    let data = encode_vec(message.as_ref()).unwrap();
                    self.record_resets(&data);
                    self.client.process(CHANNEL, &data).unwrap();
                }
            }
            assert_eq!(self.signals(), [Signal::Ready]);
            assert!(self.graphics.handle(Signal::Ready).is_empty());
            self.deliver();
        }

        /// The viewer sends a monitor layout of `size`.
        fn layout(&mut self, size: DesktopSize) {
            assert!(
                self.graphics
                    .handle(Signal::Viewer(size, Request::Layout))
                    .is_empty()
            );
            self.deliver();
        }

        fn signals(&mut self) -> Vec<Signal> {
            core::iter::from_fn(|| self.graphics.signals.try_recv().ok()).collect()
        }

        /// Hands the client everything the router sent and paints its output.
        fn deliver(&mut self) {
            while let Ok(ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages })) =
                self.events.try_recv()
            {
                for message in messages {
                    let bytes = message.encode_unframed_pdu().unwrap();
                    let pdu = DrdynvcServerPdu::decode(&mut ReadCursor::new(&bytes)).unwrap();
                    let DrdynvcServerPdu::Data(DrdynvcDataPdu::Data(data)) = pdu else {
                        panic!("expected one DVC data PDU, got {pdu:?}");
                    };
                    self.record_resets(data.data());
                    for reply in self.client.process(CHANNEL, data.data()).unwrap() {
                        self.acks.push(encode_vec(reply.as_ref()).unwrap());
                    }
                }
            }
            if let Some((width, height)) = self.client.take_output_reset() {
                self.width = width;
                self.screen = vec![0; usize::from(width) * usize::from(height) * 4];
            }
            for update in self.client.drain_output() {
                let columns = usize::from(update.region.right - update.region.left) * 4;
                for (row, y) in (update.region.top..update.region.bottom).enumerate() {
                    let at = (usize::from(y) * usize::from(self.width)
                        + usize::from(update.region.left))
                        * 4;
                    self.screen[at..at + columns]
                        .copy_from_slice(&update.data[row * columns..(row + 1) * columns]);
                }
            }
        }

        fn record_resets(&mut self, data: &[u8]) {
            let mut pdus = Vec::new();
            self.decompressor.decompress(data, &mut pdus).unwrap();
            let mut cursor = ReadCursor::new(&pdus);
            while !cursor.is_empty() {
                if let GfxPdu::ResetGraphics(reset) = decode_cursor(&mut cursor).unwrap() {
                    self.resets.push(reset);
                }
            }
        }

        fn reset_sizes(&self) -> Vec<(u32, u32)> {
            self.resets.iter().map(|r| (r.width, r.height)).collect()
        }

        fn acknowledge(&mut self) {
            for ack in core::mem::take(&mut self.acks) {
                self.bridge.process(CHANNEL, &ack).unwrap();
            }
            self.graphics.handle(Signal::Acknowledged);
            self.deliver();
        }

        /// The client's screen as BGR, to compare with what the target drew.
        fn bgr(&self) -> Vec<[u8; 3]> {
            self.screen
                .chunks_exact(4)
                .map(|rgba| [rgba[2], rgba[1], rgba[0]])
                .collect()
        }
    }

    fn size(width: u16, height: u16) -> DesktopSize {
        DesktopSize { width, height }
    }

    /// BGRA pixels whose neighbours all differ.
    fn pixels(width: u16, height: u16, seed: u8) -> Vec<u8> {
        (0..usize::from(width) * usize::from(height))
            .flat_map(|i| {
                let i = u8::try_from(i % 251).unwrap().wrapping_add(seed);
                [i, i.wrapping_mul(3), i.wrapping_mul(7), 0xff]
            })
            .collect()
    }

    fn bgr(bgra: &[u8]) -> Vec<[u8; 3]> {
        bgra.chunks_exact(4).map(|p| [p[0], p[1], p[2]]).collect()
    }

    /// What the viewer should see: `desktop` (BGRA rows of `desktop_size`) on `surface`.
    fn expected(desktop_size: DesktopSize, desktop: &[u8], surface: DesktopSize) -> Vec<[u8; 3]> {
        let painter = Painter::new(Placement::fit(desktop_size, surface));
        bgr(&painter.paint(
            desktop,
            Rect {
                left: 0,
                top: 0,
                right: surface.width,
                bottom: surface.height,
            },
        ))
    }

    #[test]
    fn frames_go_to_the_bitmap_path_until_the_pipeline_opens_and_then_reach_it() {
        let mut viewer = Viewer::new(size(16, 8));
        let desktop = pixels(16, 8, 0);

        assert!(matches!(
            viewer.graphics.frame(0, 0, 16, 8, &desktop),
            Routed::AsIs
        ));
        viewer.open();

        assert_eq!(viewer.client.total_frames_decoded(), 1);
        assert_eq!(viewer.bgr(), bgr(&desktop));
    }

    #[test]
    fn a_frame_on_an_open_pipeline_is_painted_in_place() {
        let mut viewer = Viewer::new(size(16, 8));
        let mut desktop = pixels(16, 8, 0);
        viewer.graphics.frame(0, 0, 16, 8, &desktop);
        viewer.open();
        viewer.acknowledge();

        let patch = pixels(5, 3, 99);
        assert!(matches!(
            viewer.graphics.frame(6, 2, 5, 3, &patch),
            Routed::Sent
        ));
        viewer.deliver();

        for (row, src) in patch.chunks_exact(5 * 4).enumerate() {
            let at = ((2 + row) * 16 + 6) * 4;
            desktop[at..at + src.len()].copy_from_slice(src);
        }
        assert_eq!(viewer.bgr(), bgr(&desktop));
    }

    /// Windows App on a phone: it connects in portrait, the target keeps its landscape
    /// desktop, and the viewer gets its own size with the desktop shrunk into it.
    #[test]
    fn the_pipeline_opens_at_the_viewers_size_with_the_desktop_scaled_into_it() {
        let mut viewer = Viewer::new(size(14, 26));
        // Until the pipeline opens the bitmap path shows it.
        assert_eq!(viewer.graphics.resize(size(36, 22)).len(), 1);
        let desktop = pixels(36, 22, 3);
        viewer.graphics.frame(0, 0, 36, 22, &desktop);
        viewer.open();

        assert_eq!(viewer.reset_sizes(), [(14, 26)]);
        assert_eq!(viewer.bgr(), expected(size(36, 22), &desktop, size(14, 26)));
        let placement = Placement::fit(size(36, 22), size(14, 26));
        assert_eq!(placement.scaled, size(14, 9));
        assert_eq!(placement.origin, (0, 8));
        assert_eq!(viewer.graphics.factory().to_desktop(13, 16), (34, 20));
    }

    #[test]
    fn every_layout_is_answered_with_a_reset_of_exactly_its_size() {
        let mut viewer = Viewer::new(size(14, 26));
        viewer.graphics.resize(size(36, 22));
        let desktop = pixels(36, 22, 3);
        viewer.graphics.frame(0, 0, 36, 22, &desktop);
        viewer.open();
        viewer.acknowledge();

        // The layout Windows App sends once it has the pipeline repeats its size.
        viewer.layout(size(14, 26));
        // Turned to landscape, then back.
        viewer.layout(size(26, 14));
        viewer.acknowledge();
        assert_eq!(viewer.bgr(), expected(size(36, 22), &desktop, size(26, 14)));
        viewer.layout(size(14, 26));
        // A repeat of the layout just answered.
        viewer.layout(size(14, 26));

        assert_eq!(
            viewer.reset_sizes(),
            [(14, 26), (14, 26), (26, 14), (14, 26)]
        );
        let monitor = |right, bottom| Monitor {
            left: 0,
            top: 0,
            right,
            bottom,
            flags: MonitorFlags::PRIMARY,
        };
        // oracle: spec MS-RDPEGFX §2.2.2.14, MS-RDPBCGR §2.2.1.3.6.1 (inclusive edges)
        assert_eq!(viewer.resets[2].monitors, [monitor(25, 13)]);
        assert_eq!(viewer.resets[3].monitors, [monitor(13, 25)]);
    }

    /// Once the target follows the viewer's layout the desktop is drawn 1:1, with no further
    /// reset of the viewer's graphics.
    #[test]
    fn a_target_that_follows_is_drawn_one_to_one() {
        let mut viewer = Viewer::new(size(16, 8));
        viewer.graphics.frame(0, 0, 16, 8, &pixels(16, 8, 0));
        viewer.open();
        viewer.acknowledge();
        viewer.layout(size(8, 12));
        viewer.acknowledge();

        assert!(viewer.graphics.resize(size(8, 12)).is_empty());
        let rotated = pixels(8, 12, 7);
        viewer.graphics.frame(0, 0, 8, 12, &rotated);
        viewer.acknowledge();

        assert_eq!(viewer.reset_sizes(), [(16, 8), (8, 12)]);
        assert_eq!(viewer.width, 8);
        assert_eq!(viewer.bgr(), bgr(&rotated));
        assert_eq!(viewer.graphics.factory().to_desktop(5, 9), (5, 9));
    }

    #[test]
    fn without_the_pipeline_a_layout_reactivates_the_bitmap_path_at_its_size() {
        let mut graphics = ViewerGraphics::new(size(16, 8));
        graphics.resize(size(16, 8));
        let desktop = pixels(16, 8, 0);
        assert!(matches!(
            graphics.frame(0, 0, 16, 8, &desktop),
            Routed::AsIs
        ));

        let updates = graphics.handle(Signal::Viewer(size(8, 12), Request::Layout));
        let [
            DisplayUpdate::Resize(resized),
            DisplayUpdate::Bitmap(surface),
        ] = updates.as_slice()
        else {
            panic!("expected a resize and the surface, got {updates:?}");
        };
        assert_eq!(*resized, size(8, 12));
        assert_eq!((surface.width.get(), surface.height.get()), (8, 12));
        assert_eq!(
            bgr(&surface.data),
            expected(size(16, 8), &desktop, size(8, 12))
        );
        // Frames now come out scaled, as surface pixels.
        let Routed::Bitmap(DisplayUpdate::Bitmap(update)) =
            graphics.frame(0, 0, 4, 4, &pixels(4, 4, 9))
        else {
            panic!("expected a scaled bitmap");
        };
        assert!(update.x + update.width.get() <= 8 && update.y + update.height.get() <= 12);

        // The same size again needs no reactivation.
        assert!(
            graphics
                .handle(Signal::Viewer(size(8, 12), Request::Layout))
                .is_empty()
        );
        // The target follows: frames pass through again.
        assert!(graphics.resize(size(8, 12)).is_empty());
        assert!(matches!(
            graphics.frame(0, 0, 8, 12, &pixels(8, 12, 1)),
            Routed::AsIs
        ));
    }

    #[test]
    fn a_bitmap_viewer_keeps_its_size_when_the_target_does_not_follow() {
        let mut graphics = ViewerGraphics::new(size(1024, 768));
        graphics.handle(Signal::Viewer(size(28, 53), Request::Initial));
        let updates = graphics.resize(size(72, 45));
        let [DisplayUpdate::Bitmap(surface)] = updates.as_slice() else {
            panic!("expected the surface repainted, got {updates:?}");
        };
        assert_eq!((surface.width.get(), surface.height.get()), (28, 53));
        assert_eq!(graphics.surface.size(), size(28, 53));
    }

    #[test]
    fn frames_wait_for_acknowledgements_and_none_is_lost() {
        let mut viewer = Viewer::new(size(16, 8));
        let mut desktop = pixels(16, 8, 0);
        viewer.graphics.frame(0, 0, 16, 8, &desktop);
        viewer.open();

        for x in 0..6 {
            let column = pixels(1, 8, 40 + x);
            viewer.graphics.frame(u16::from(x), 0, 1, 8, &column);
            for (y, src) in column.chunks_exact(4).enumerate() {
                let at = (y * 16 + usize::from(x)) * 4;
                desktop[at..at + 4].copy_from_slice(src);
            }
        }
        viewer.deliver();
        assert_ne!(
            viewer.bgr(),
            bgr(&desktop),
            "every frame went out unacknowledged"
        );

        for ack in core::mem::take(&mut viewer.acks) {
            viewer.bridge.process(CHANNEL, &ack).unwrap();
        }
        assert!(viewer.signals().contains(&Signal::Acknowledged));
        viewer.graphics.handle(Signal::Acknowledged);
        viewer.deliver();

        assert_eq!(viewer.bgr(), bgr(&desktop));
    }

    #[test]
    fn a_backlog_of_rectangles_goes_out_as_one_tile() {
        let mut viewer = Viewer::new(size(16, 8));
        let mut desktop = pixels(16, 8, 0);
        viewer.graphics.frame(0, 0, 16, 8, &desktop);
        viewer.open();
        // Two go out before the client is three frames behind; the rest wait.
        for i in 0..MAX_TILES + 3 {
            let (x, y) = (i % 16, i / 16);
            let pixel = pixels(1, 1, 60 + u8::try_from(i).unwrap());
            let (x, y) = (u16::try_from(x).unwrap(), u16::try_from(y).unwrap());
            viewer.graphics.frame(x, y, 1, 1, &pixel);
            let at = (usize::from(y) * 16 + usize::from(x)) * 4;
            desktop[at..at + 4].copy_from_slice(&pixel);
        }
        viewer.deliver();
        for ack in core::mem::take(&mut viewer.acks) {
            viewer.bridge.process(CHANNEL, &ack).unwrap();
        }
        let before = viewer.tiles.load(Ordering::Relaxed);

        viewer.graphics.handle(Signal::Acknowledged);
        viewer.deliver();

        assert_eq!(viewer.tiles.load(Ordering::Relaxed) - before, 1);
        assert_eq!(viewer.bgr(), bgr(&desktop));
    }

    #[test]
    fn a_closed_pipeline_hands_the_surface_back_to_the_bitmap_path() {
        let mut viewer = Viewer::new(size(16, 8));
        let desktop = pixels(16, 8, 0);
        viewer.graphics.frame(0, 0, 16, 8, &desktop);
        viewer.open();
        viewer.layout(size(8, 12));

        let updates = viewer.graphics.handle(Signal::Closed);

        let [
            DisplayUpdate::Resize(resized),
            DisplayUpdate::Bitmap(surface),
        ] = updates.as_slice()
        else {
            panic!("expected a resize and the surface, got {updates:?}");
        };
        assert_eq!(*resized, size(8, 12));
        assert_eq!((surface.width.get(), surface.height.get()), (8, 12));
        assert_eq!(
            bgr(&surface.data),
            expected(size(16, 8), &desktop, size(8, 12))
        );
        assert!(matches!(
            viewer.graphics.frame(0, 0, 4, 4, &pixels(4, 4, 1)),
            Routed::Bitmap(_)
        ));
    }

    #[test]
    fn a_frame_past_the_desktop_edge_is_clipped() {
        let mut graphics = ViewerGraphics::new(size(16, 8));
        let patch = pixels(4, 4, 3);
        graphics.frame(14, 6, 4, 4, &patch);

        assert_eq!(graphics.clip(14, 6, 4, 4).right, 16);
        assert_eq!(
            &graphics.canvas[(6 * 16 + 14) * 4..(6 * 16 + 16) * 4],
            &patch[..8]
        );
        assert_eq!(
            &graphics.canvas[(7 * 16 + 14) * 4..(7 * 16 + 16) * 4],
            &patch[16..24]
        );
    }
}
