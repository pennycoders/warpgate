//! The desktop over the Graphics Pipeline (MS-RDPEGFX), for viewers that open it.
//!
//! A resize then reaches the viewer as a ResetGraphics with a new surface instead of a
//! deactivation-reactivation. Windows App needs that: after a reactivation it keeps
//! drawing at the size it connected with.
//!
//! The output keeps the viewer's shape: a desktop of another shape (a target that keeps its
//! own size) is centred in it between black bars, since Windows App stretches the output to
//! its screen.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use ironrdp::dvc::{DvcMessage, encode_dvc_messages};
use ironrdp::graphics::clearcodec::ClearCodecEncoder;
use ironrdp::graphics::zgfx::CompressionMode;
use ironrdp::pdu::gcc::{Monitor, MonitorFlags};
use ironrdp::pdu::geometry::ExclusiveRectangle;
use ironrdp::svc::ChannelFlags;
use ironrdp_egfx::pdu::{CapabilitiesAdvertisePdu, CapabilitySet};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer, MixedTilePayload};
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, EgfxServerMessage, GfxDvcBridge, GfxServerFactory,
    GfxServerHandle, PixelFormat, ServerEvent, ServerEventSender,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Past this many pending rectangles they are sent as their bounding box.
const MAX_TILES: usize = 16;

/// The largest output side a letterbox may grow to; past it the desktop is sent as it is.
const MAX_OUTPUT: u32 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signal {
    Ready,
    Acknowledged,
    Closed,
    /// The viewer's screen changed shape.
    Viewer,
}

struct Shared {
    events: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
    pipeline: Mutex<Option<GfxServerHandle>>,
    signals: mpsc::UnboundedSender<Signal>,
    /// The viewer's screen size.
    viewer: Mutex<Option<DesktopSize>>,
    /// Where the desktop sits in the output, as `x << 16 | y`.
    origin: AtomicU32,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub(super) struct GraphicsFactory(Arc<Shared>);

impl GraphicsFactory {
    /// Records the viewer's screen size, which the output keeps the shape of.
    pub(super) fn set_viewer(&self, size: DesktopSize) {
        let changed = lock(&self.0.viewer).replace(size) != Some(size);
        if changed {
            let _ = self.0.signals.send(Signal::Viewer);
        }
    }

    /// Maps a point on the viewer's output to the desktop.
    pub(super) fn to_desktop(&self, x: u16, y: u16) -> (u16, u16) {
        let origin = self.0.origin.load(Ordering::Relaxed);
        let (left, top) = (
            u16::try_from(origin >> 16).unwrap_or(0),
            u16::try_from(origin & 0xffff).unwrap_or(0),
        );
        (x.saturating_sub(left), y.saturating_sub(top))
    }
}

impl ServerEventSender for GraphicsFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *lock(&self.0.events) = Some(sender);
    }
}

impl GfxServerFactory for GraphicsFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(Handler(self.0.signals.clone()))
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let server = GraphicsPipelineServer::with_compression(
            self.build_gfx_handler(),
            CompressionMode::Auto,
        );
        let handle = Arc::new(Mutex::new(server));
        *lock(&self.0.pipeline) = Some(Arc::clone(&handle));
        Some((GfxDvcBridge::new(Arc::clone(&handle)), handle))
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

/// The router's side: keeps a copy of the desktop and, while the viewer has the pipeline
/// open, sends it the changed rectangles as ClearCodec tiles.
pub(super) struct ViewerGraphics {
    shared: Arc<Shared>,
    signals: mpsc::UnboundedReceiver<Signal>,
    size: DesktopSize,
    /// BGRA rows of `size`.
    canvas: Vec<u8>,
    /// The viewer's output: the desktop letterboxed to the viewer's shape.
    output: DesktopSize,
    /// Where the desktop sits in `output`.
    origin: (u16, u16),
    surface: Option<u16>,
    encoder: ClearCodecEncoder,
    dirty: Vec<ExclusiveRectangle>,
    /// The size the bitmap path was left at when the pipeline opened.
    committed: DesktopSize,
    started: Instant,
}

impl ViewerGraphics {
    pub(super) fn new() -> Self {
        let (signals_tx, signals) = mpsc::unbounded_channel();
        let empty = DesktopSize {
            width: 0,
            height: 0,
        };
        Self {
            shared: Arc::new(Shared {
                events: Mutex::default(),
                pipeline: Mutex::default(),
                signals: signals_tx,
                viewer: Mutex::default(),
                origin: AtomicU32::new(0),
            }),
            signals,
            size: empty,
            canvas: Vec::new(),
            output: empty,
            origin: (0, 0),
            surface: None,
            encoder: ClearCodecEncoder::new(),
            dirty: Vec::new(),
            committed: empty,
            started: Instant::now(),
        }
    }

    pub(super) fn factory(&self) -> GraphicsFactory {
        GraphicsFactory(Arc::clone(&self.shared))
    }

    pub(super) async fn signal(&mut self) -> Option<Signal> {
        self.signals.recv().await
    }

    /// Updates for the bitmap path, which takes the desktop back when the pipeline closes.
    pub(super) fn handle(&mut self, signal: Signal, size: DesktopSize) -> Vec<DisplayUpdate> {
        match signal {
            Signal::Ready => {
                self.open(size);
                Vec::new()
            }
            Signal::Acknowledged => {
                self.flush();
                Vec::new()
            }
            Signal::Closed => self.close(size),
            Signal::Viewer => {
                self.reshape();
                Vec::new()
            }
        }
    }

    /// Whether the frame went through the pipeline; otherwise it is the bitmap path's.
    pub(super) fn frame(
        &mut self,
        size: DesktopSize,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        data: &[u8],
    ) -> bool {
        self.fit(size);
        let rect = self.clip(x, y, width, height);
        let columns = usize::from(rect.right - rect.left) * 4;
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
        if self.surface.is_none() {
            return false;
        }
        if rect.right > rect.left && rect.bottom > rect.top {
            let (left, top) = self.origin;
            self.dirty.push(ExclusiveRectangle {
                left: rect.left + left,
                top: rect.top + top,
                right: rect.right + left,
                bottom: rect.bottom + top,
            });
        }
        self.flush();
        true
    }

    /// Whether the pipeline took the resize; otherwise the bitmap path reactivates.
    pub(super) fn resize(&mut self, size: DesktopSize) -> bool {
        self.fit(size);
        if self.surface.is_none() {
            return false;
        }
        self.reset();
        true
    }

    /// Letterboxes the desktop to the viewer's new shape, if that moves it.
    fn reshape(&mut self) {
        let before = (self.output, self.origin);
        if self.surface.is_some() && self.layout() != before {
            self.reset();
        }
    }

    fn open(&mut self, size: DesktopSize) {
        self.fit(size);
        self.committed = size;
        // A (re)negotiation starts the viewer's ClearCodec state from scratch.
        self.encoder = ClearCodecEncoder::new();
        self.reset();
    }

    /// Resets the viewer's graphics to the letterboxed output and sends all of it.
    fn reset(&mut self) {
        self.layout();
        let Some(pipeline) = self.pipeline() else {
            return;
        };
        self.surface = reset(&mut lock(&pipeline), self.output);
        let (left, top) = self.origin;
        self.shared
            .origin
            .store(u32::from(left) << 16 | u32::from(top), Ordering::Relaxed);
        self.dirty = vec![self.full()];
        self.flush();
    }

    fn layout(&mut self) -> (DesktopSize, (u16, u16)) {
        let viewer = *lock(&self.shared.viewer);
        (self.output, self.origin) = letterbox(self.size, viewer);
        (self.output, self.origin)
    }

    fn close(&mut self, size: DesktopSize) -> Vec<DisplayUpdate> {
        if self.surface.take().is_none() {
            return Vec::new();
        }
        self.shared.origin.store(0, Ordering::Relaxed);
        self.dirty.clear();
        self.fit(size);
        let resize = (size != self.committed).then_some(DisplayUpdate::Resize(size));
        let (Some(width), Some(height), Some(stride)) = (
            NonZeroU16::new(size.width),
            NonZeroU16::new(size.height),
            NonZeroUsize::new(usize::from(size.width) * 4),
        ) else {
            return resize.into_iter().collect();
        };
        let desktop = DisplayUpdate::Bitmap(BitmapUpdate {
            x: 0,
            y: 0,
            width,
            height,
            format: PixelFormat::BgrA32,
            data: Bytes::copy_from_slice(&self.canvas),
            stride,
        });
        resize.into_iter().chain([desktop]).collect()
    }

    /// Sends the pending rectangles unless the viewer is behind on acknowledgements, and
    /// whatever else the pipeline has queued.
    fn flush(&mut self) {
        let (Some(surface), Some(pipeline)) = (self.surface, self.pipeline()) else {
            return;
        };
        let mut pipeline = lock(&pipeline);
        if !self.dirty.is_empty() && !pipeline.should_backpressure() {
            if self.dirty.len() > MAX_TILES {
                self.dirty = self
                    .dirty
                    .drain(..)
                    .reduce(|a, b| union(&a, &b))
                    .into_iter()
                    .collect();
            }
            let tiles = core::mem::take(&mut self.dirty)
                .into_iter()
                .map(|destination| MixedTilePayload::ClearCodec {
                    bitmap_data: self.encoder.encode(
                        &self.pixels(&destination),
                        destination.right - destination.left,
                        destination.bottom - destination.top,
                    ),
                    destination,
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

    fn fit(&mut self, size: DesktopSize) {
        if self.size != size {
            self.size = size;
            self.canvas = vec![0; usize::from(size.width) * usize::from(size.height) * 4];
        }
    }

    fn clip(&self, x: u16, y: u16, width: u16, height: u16) -> ExclusiveRectangle {
        let left = x.min(self.size.width);
        let top = y.min(self.size.height);
        ExclusiveRectangle {
            left,
            top,
            right: x.saturating_add(width).clamp(left, self.size.width),
            bottom: y.saturating_add(height).clamp(top, self.size.height),
        }
    }

    const fn full(&self) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left: 0,
            top: 0,
            right: self.output.width,
            bottom: self.output.height,
        }
    }

    fn offset(&self, x: u16, y: u16) -> usize {
        (usize::from(y) * usize::from(self.size.width) + usize::from(x)) * 4
    }

    /// The output's pixels in `rect`: the desktop's, black around it.
    fn pixels(&self, rect: &ExclusiveRectangle) -> Vec<u8> {
        let (left, top) = self.origin;
        let width = usize::from(rect.right - rect.left);
        let mut out = vec![0; width * usize::from(rect.bottom - rect.top) * 4];
        let from = rect.left.max(left);
        let to = rect.right.min(left + self.size.width);
        if from >= to {
            return out;
        }
        let columns = usize::from(to - from) * 4;
        let into = usize::from(from - rect.left) * 4;
        for (row, y) in (rect.top..rect.bottom).enumerate() {
            let Some(y) = y.checked_sub(top).filter(|y| *y < self.size.height) else {
                continue;
            };
            let at = self.offset(from - left, y);
            let start = row * width * 4 + into;
            if let (Some(src), Some(dst)) = (
                self.canvas.get(at..at + columns),
                out.get_mut(start..start + columns),
            ) {
                dst.copy_from_slice(src);
            }
        }
        out
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

/// The output for `desktop` on a viewer of `viewer`'s shape, and where the desktop sits in it:
/// the smallest size of that shape around the desktop, which is centred in it.
fn letterbox(desktop: DesktopSize, viewer: Option<DesktopSize>) -> (DesktopSize, (u16, u16)) {
    let as_is = (desktop, (0, 0));
    let Some(viewer) = viewer.filter(|v| v.width > 0 && v.height > 0) else {
        return as_is;
    };
    let (width, height) = (u32::from(desktop.width), u32::from(desktop.height));
    let (shape_width, shape_height) = (u32::from(viewer.width), u32::from(viewer.height));
    let (output_width, output_height) = if width * shape_height >= height * shape_width {
        (width, (width * shape_height).div_ceil(shape_width))
    } else {
        ((height * shape_width).div_ceil(shape_height), height)
    };
    let (Ok(output_width), Ok(output_height)) = (
        u16::try_from(output_width.min(MAX_OUTPUT + 1)),
        u16::try_from(output_height.min(MAX_OUTPUT + 1)),
    ) else {
        return as_is;
    };
    if u32::from(output_width.max(output_height)) > MAX_OUTPUT {
        return as_is;
    }
    (
        DesktopSize {
            width: output_width,
            height: output_height,
        },
        (
            (output_width - desktop.width) / 2,
            (output_height - desktop.height) / 2,
        ),
    )
}

fn union(a: &ExclusiveRectangle, b: &ExclusiveRectangle) -> ExclusiveRectangle {
    ExclusiveRectangle {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ironrdp::core::{Decode as _, ReadCursor, decode_cursor, encode_vec};
    use ironrdp::dvc::DvcProcessor as _;
    use ironrdp::dvc::pdu::{DrdynvcDataPdu, DrdynvcServerPdu};
    use ironrdp::graphics::zgfx::Decompressor;
    use ironrdp_egfx::client::GraphicsPipelineClient;
    use ironrdp_egfx::pdu::{GfxPdu, ResetGraphicsPdu};

    use super::*;

    const CHANNEL: u32 = 7;

    /// Counts the tiles the client decodes.
    struct Painter(Arc<AtomicUsize>);

    impl ironrdp_egfx::client::GraphicsPipelineHandler for Painter {
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
        fn new() -> Self {
            let graphics = ViewerGraphics::new();
            let mut factory = graphics.factory();
            let (sender, events) = mpsc::unbounded_channel();
            factory.set_sender(sender);
            let (mut bridge, _) = factory.build_server_with_handle().unwrap();
            bridge.start(CHANNEL).unwrap();
            let tiles = Arc::new(AtomicUsize::new(0));
            Self {
                graphics,
                bridge,
                events,
                client: GraphicsPipelineClient::new(Box::new(Painter(Arc::clone(&tiles))), None),
                screen: Vec::new(),
                width: 0,
                acks: Vec::new(),
                tiles,
                decompressor: Decompressor::new(),
                resets: Vec::new(),
            }
        }

        /// Negotiates the pipeline and hands the router its `Ready`.
        fn open(&mut self, size: DesktopSize) {
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
            assert!(self.graphics.handle(Signal::Ready, size).is_empty());
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

        fn acknowledge(&mut self) {
            for ack in core::mem::take(&mut self.acks) {
                self.bridge.process(CHANNEL, &ack).unwrap();
            }
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

    #[test]
    fn frames_go_to_the_bitmap_path_until_the_pipeline_opens_and_then_reach_it() {
        let mut viewer = Viewer::new();
        let desktop = pixels(16, 8, 0);

        assert!(!viewer.graphics.frame(size(16, 8), 0, 0, 16, 8, &desktop));
        viewer.open(size(16, 8));

        assert_eq!(viewer.client.total_frames_decoded(), 1);
        assert_eq!(viewer.bgr(), bgr(&desktop));
    }

    #[test]
    fn a_frame_on_an_open_pipeline_is_painted_in_place() {
        let mut viewer = Viewer::new();
        let mut desktop = pixels(16, 8, 0);
        assert!(!viewer.graphics.frame(size(16, 8), 0, 0, 16, 8, &desktop));
        viewer.open(size(16, 8));
        viewer.acknowledge();

        let patch = pixels(5, 3, 99);
        assert!(viewer.graphics.frame(size(16, 8), 6, 2, 5, 3, &patch));
        viewer.deliver();

        for (row, src) in patch.chunks_exact(5 * 4).enumerate() {
            let at = ((2 + row) * 16 + 6) * 4;
            desktop[at..at + src.len()].copy_from_slice(src);
        }
        assert_eq!(viewer.bgr(), bgr(&desktop));
    }

    #[test]
    fn a_resize_resets_the_graphics_instead_of_reactivating() {
        let mut viewer = Viewer::new();
        viewer.open(size(16, 8));

        assert!(viewer.graphics.resize(size(8, 12)));
        let rotated = pixels(8, 12, 7);
        assert!(viewer.graphics.frame(size(8, 12), 0, 0, 8, 12, &rotated));
        viewer.acknowledge();
        viewer.graphics.handle(Signal::Acknowledged, size(8, 12));
        viewer.deliver();

        assert_eq!(viewer.width, 8);
        assert_eq!(viewer.bgr(), bgr(&rotated));
    }

    #[test]
    fn every_reset_lays_out_one_primary_monitor_of_the_new_size() {
        let mut viewer = Viewer::new();
        viewer.open(size(16, 8));
        assert!(viewer.graphics.resize(size(8, 12)));
        viewer.deliver();

        let monitor = |right, bottom| Monitor {
            left: 0,
            top: 0,
            right,
            bottom,
            flags: MonitorFlags::PRIMARY,
        };
        // oracle: spec MS-RDPEGFX §2.2.2.14, MS-RDPBCGR §2.2.1.3.6.1 (inclusive edges)
        assert_eq!(
            viewer.resets,
            [
                ResetGraphicsPdu {
                    width: 16,
                    height: 8,
                    monitors: vec![monitor(15, 7)],
                },
                ResetGraphicsPdu {
                    width: 8,
                    height: 12,
                    monitors: vec![monitor(7, 11)],
                },
            ]
        );
    }

    #[test]
    fn without_the_pipeline_a_resize_is_the_bitmap_paths() {
        assert!(!ViewerGraphics::new().resize(size(8, 12)));
    }

    #[test]
    fn frames_wait_for_acknowledgements_and_none_is_lost() {
        let mut viewer = Viewer::new();
        let mut desktop = pixels(16, 8, 0);
        assert!(!viewer.graphics.frame(size(16, 8), 0, 0, 16, 8, &desktop));
        viewer.open(size(16, 8));

        for x in 0..6 {
            let column = pixels(1, 8, 40 + x);
            assert!(
                viewer
                    .graphics
                    .frame(size(16, 8), u16::from(x), 0, 1, 8, &column)
            );
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

        viewer.acknowledge();
        assert!(viewer.signals().contains(&Signal::Acknowledged));
        viewer.graphics.handle(Signal::Acknowledged, size(16, 8));
        viewer.deliver();

        assert_eq!(viewer.bgr(), bgr(&desktop));
    }

    #[test]
    fn a_backlog_of_rectangles_goes_out_as_one_tile() {
        let mut viewer = Viewer::new();
        let mut desktop = pixels(16, 8, 0);
        assert!(!viewer.graphics.frame(size(16, 8), 0, 0, 16, 8, &desktop));
        viewer.open(size(16, 8));
        // Two go out before the client is three frames behind; the rest wait.
        for i in 0..MAX_TILES + 3 {
            let (x, y) = (i % 16, i / 16);
            let pixel = pixels(1, 1, 60 + u8::try_from(i).unwrap());
            let (x, y) = (u16::try_from(x).unwrap(), u16::try_from(y).unwrap());
            assert!(viewer.graphics.frame(size(16, 8), x, y, 1, 1, &pixel));
            let at = (usize::from(y) * 16 + usize::from(x)) * 4;
            desktop[at..at + 4].copy_from_slice(&pixel);
        }
        viewer.deliver();
        viewer.acknowledge();
        let before = viewer.tiles.load(Ordering::Relaxed);

        viewer.graphics.handle(Signal::Acknowledged, size(16, 8));
        viewer.deliver();

        assert_eq!(viewer.tiles.load(Ordering::Relaxed) - before, 1);
        assert_eq!(viewer.bgr(), bgr(&desktop));
    }

    #[test]
    fn a_closed_pipeline_hands_the_desktop_back_to_the_bitmap_path() {
        let mut viewer = Viewer::new();
        viewer.open(size(16, 8));
        assert!(viewer.graphics.resize(size(8, 12)));
        let rotated = pixels(8, 12, 7);
        assert!(viewer.graphics.frame(size(8, 12), 0, 0, 8, 12, &rotated));

        let updates = viewer.graphics.handle(Signal::Closed, size(8, 12));

        let [
            DisplayUpdate::Resize(resized),
            DisplayUpdate::Bitmap(desktop),
        ] = updates.as_slice()
        else {
            panic!("expected a resize and the desktop, got {updates:?}");
        };
        assert_eq!(*resized, size(8, 12));
        assert_eq!((desktop.width.get(), desktop.height.get()), (8, 12));
        assert_eq!(desktop.data.as_ref(), rotated);
        assert!(!viewer.graphics.frame(size(8, 12), 0, 0, 8, 12, &rotated));
    }

    #[test]
    fn a_frame_past_the_desktop_edge_is_clipped() {
        let mut graphics = ViewerGraphics::new();
        let patch = pixels(4, 4, 3);
        assert!(!graphics.frame(size(16, 8), 14, 6, 4, 4, &patch));

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

    /// The viewer, told its screen size, gets the desktop centred in an output of its shape.
    fn letterboxed(screen: DesktopSize, desktop: DesktopSize) -> (Viewer, GraphicsFactory) {
        let mut viewer = Viewer::new();
        let factory = viewer.graphics.factory();
        factory.set_viewer(screen);
        assert_eq!(viewer.signals(), [Signal::Viewer]);
        assert!(viewer.graphics.handle(Signal::Viewer, desktop).is_empty());
        (viewer, factory)
    }

    #[test]
    fn a_desktop_of_another_shape_is_centred_between_black_bars() {
        let (mut viewer, factory) = letterboxed(size(8, 16), size(16, 8));
        let desktop = pixels(16, 8, 0);
        assert!(!viewer.graphics.frame(size(16, 8), 0, 0, 16, 8, &desktop));
        viewer.open(size(16, 8));

        let reset = viewer.resets.last().unwrap();
        assert_eq!((reset.width, reset.height), (16, 32));
        let mut screen = vec![0; 16 * 32 * 4];
        screen[12 * 16 * 4..20 * 16 * 4].copy_from_slice(&desktop);
        assert_eq!(viewer.bgr(), bgr(&screen));
        assert_eq!(factory.to_desktop(5, 13), (5, 1));
        assert_eq!(factory.to_desktop(5, 3), (5, 0));
    }

    #[test]
    fn a_frame_lands_inside_the_bars() {
        let (mut viewer, _) = letterboxed(size(8, 16), size(16, 8));
        let mut desktop = pixels(16, 8, 0);
        assert!(!viewer.graphics.frame(size(16, 8), 0, 0, 16, 8, &desktop));
        viewer.open(size(16, 8));
        viewer.acknowledge();

        let patch = pixels(5, 3, 99);
        assert!(viewer.graphics.frame(size(16, 8), 6, 2, 5, 3, &patch));
        viewer.deliver();

        for (row, src) in patch.chunks_exact(5 * 4).enumerate() {
            let at = ((2 + row) * 16 + 6) * 4;
            desktop[at..at + src.len()].copy_from_slice(src);
        }
        let mut screen = vec![0; 16 * 32 * 4];
        screen[12 * 16 * 4..20 * 16 * 4].copy_from_slice(&desktop);
        assert_eq!(viewer.bgr(), bgr(&screen));
    }

    #[test]
    fn a_viewer_turned_to_the_desktop_shape_gets_it_without_bars() {
        let (mut viewer, factory) = letterboxed(size(8, 16), size(16, 8));
        let desktop = pixels(16, 8, 0);
        assert!(!viewer.graphics.frame(size(16, 8), 0, 0, 16, 8, &desktop));
        viewer.open(size(16, 8));
        viewer.acknowledge();

        factory.set_viewer(size(32, 16));
        assert_eq!(viewer.signals(), [Signal::Acknowledged, Signal::Viewer]);
        assert!(
            viewer
                .graphics
                .handle(Signal::Viewer, size(16, 8))
                .is_empty()
        );
        viewer.deliver();

        let reset = viewer.resets.last().unwrap();
        assert_eq!((reset.width, reset.height), (16, 8));
        assert_eq!(viewer.bgr(), bgr(&desktop));
        assert_eq!(factory.to_desktop(5, 3), (5, 3));
    }

    #[test]
    fn the_output_is_the_smallest_of_the_viewers_shape_around_the_desktop() {
        assert_eq!(
            letterbox(size(1440, 900), Some(size(560, 1058))),
            (size(1440, 2721), (0, 910))
        );
        assert_eq!(
            letterbox(size(560, 1058), Some(size(1058, 560))),
            (size(1999, 1058), (719, 0))
        );
        assert_eq!(
            letterbox(size(560, 1058), Some(size(560, 1058))),
            (size(560, 1058), (0, 0))
        );
        assert_eq!(letterbox(size(560, 1058), None), (size(560, 1058), (0, 0)));
        assert_eq!(
            letterbox(size(8000, 100), Some(size(100, 8000))),
            (size(8000, 100), (0, 0))
        );
    }
}
