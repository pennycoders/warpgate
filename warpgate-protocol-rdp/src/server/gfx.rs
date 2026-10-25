//! The desktop over the Graphics Pipeline (MS-RDPEGFX), for viewers that open it.
//!
//! A resize then reaches the viewer as a ResetGraphics with a new surface instead of a
//! deactivation-reactivation. Windows App needs that: after a reactivation it keeps
//! drawing at the size it connected with.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use ironrdp::dvc::{DvcMessage, encode_dvc_messages};
use ironrdp::graphics::clearcodec::ClearCodecEncoder;
use ironrdp::graphics::zgfx::CompressionMode;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signal {
    Ready,
    Acknowledged,
    Closed,
}

struct Shared {
    events: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
    pipeline: Mutex<Option<GfxServerHandle>>,
    signals: mpsc::UnboundedSender<Signal>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub(super) struct GraphicsFactory(Arc<Shared>);

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
            }),
            signals,
            size: empty,
            canvas: Vec::new(),
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
            self.dirty.push(rect);
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
        if let Some(pipeline) = self.pipeline() {
            let mut pipeline = lock(&pipeline);
            pipeline.resize(size.width, size.height);
            self.surface = pipeline.create_surface(size.width, size.height);
            if let Some(surface) = self.surface {
                pipeline.map_surface_to_output(surface, 0, 0);
            }
        }
        self.flush();
        true
    }

    fn open(&mut self, size: DesktopSize) {
        self.fit(size);
        self.committed = size;
        // A (re)negotiation starts the viewer's ClearCodec state from scratch.
        self.encoder = ClearCodecEncoder::new();
        let Some(pipeline) = self.pipeline() else {
            return;
        };
        {
            let mut pipeline = lock(&pipeline);
            pipeline.set_output_dimensions(size.width, size.height);
            self.surface = pipeline.create_surface(size.width, size.height);
            if let Some(surface) = self.surface {
                pipeline.map_surface_to_output(surface, 0, 0);
            }
        }
        self.dirty = vec![self.full()];
        self.flush();
    }

    fn close(&mut self, size: DesktopSize) -> Vec<DisplayUpdate> {
        if self.surface.take().is_none() {
            return Vec::new();
        }
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
            right: self.size.width,
            bottom: self.size.height,
        }
    }

    fn offset(&self, x: u16, y: u16) -> usize {
        (usize::from(y) * usize::from(self.size.width) + usize::from(x)) * 4
    }

    fn pixels(&self, rect: &ExclusiveRectangle) -> Vec<u8> {
        let columns = usize::from(rect.right - rect.left) * 4;
        (rect.top..rect.bottom)
            .flat_map(|y| {
                let at = self.offset(rect.left, y);
                self.canvas.get(at..at + columns).unwrap_or_default()
            })
            .copied()
            .collect()
    }
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

    use ironrdp::core::{Decode as _, ReadCursor, encode_vec};
    use ironrdp::dvc::DvcProcessor as _;
    use ironrdp::dvc::pdu::{DrdynvcDataPdu, DrdynvcServerPdu};
    use ironrdp_egfx::client::GraphicsPipelineClient;

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
                    self.client
                        .process(CHANNEL, &encode_vec(message.as_ref()).unwrap())
                        .unwrap();
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
}
