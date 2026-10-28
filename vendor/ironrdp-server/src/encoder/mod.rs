use core::fmt;
use core::num::NonZeroU16;

use anyhow::{Context as _, Result, anyhow};
use ironrdp_acceptor::DesktopSize;
use ironrdp_graphics::diff::{Rect, find_different_rects_sub};
use ironrdp_pdu::encode_vec;
use ironrdp_pdu::fast_path::UpdateCode;
use ironrdp_pdu::geometry::ExclusiveRectangle;
use ironrdp_pdu::pointer::{
    CachedPointerAttribute, ColorPointerAttribute, LargePointerAttribute, Point16, PointerAttribute,
    PointerPositionAttribute,
};
use ironrdp_pdu::rdp::capability_sets::{CmdFlags, EntropyBits, LargePointerSupportFlags};
use ironrdp_pdu::surface_commands::{ExtendedBitmapDataPdu, SurfaceBitsPdu, SurfaceCommand};
use tracing::{debug, warn};

use self::bitmap::BitmapEncoder;
use self::rfx::RfxEncoder;
use super::BitmapUpdate;
use crate::macros::time_warn;
use crate::{ColorPointer, DisplayUpdate, Framebuffer, LargePointer, RGBAPointer};

mod bitmap;
mod fast_path;
pub(crate) mod rfx;

pub(crate) use fast_path::*;
use ironrdp_graphics::rdp6::BitmapEncodeError;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
enum CodecId {
    None = 0x0,
}

impl CodecId {
    #[expect(
        clippy::as_conversions,
        reason = "guarantees discriminant layout, and as is the only way to cast enum -> primitive"
    )]
    fn as_u8(self) -> u8 {
        self as u8
    }
}

#[cfg_attr(feature = "__bench", visibility::make(pub))]
#[derive(Debug)]
pub(crate) struct UpdateEncoderCodecs {
    remotefx: Option<(EntropyBits, u8)>,
    #[cfg(feature = "qoi")]
    qoi: Option<u8>,
    #[cfg(feature = "qoiz")]
    qoiz: Option<u8>,
    /// `(codec_id, color_loss_level)` from the negotiated NsCodec capability.
    #[cfg(feature = "nscodec")]
    nscodec: Option<(u8, u8)>,
}

impl UpdateEncoderCodecs {
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn new() -> Self {
        Self {
            remotefx: None,
            #[cfg(feature = "qoi")]
            qoi: None,
            #[cfg(feature = "qoiz")]
            qoiz: None,
            #[cfg(feature = "nscodec")]
            nscodec: None,
        }
    }

    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn set_remotefx(&mut self, remotefx: Option<(EntropyBits, u8)>) {
        self.remotefx = remotefx
    }

    #[cfg(feature = "qoi")]
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn set_qoi(&mut self, qoi: Option<u8>) {
        self.qoi = qoi
    }

    #[cfg(feature = "qoiz")]
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn set_qoiz(&mut self, qoiz: Option<u8>) {
        self.qoiz = qoiz
    }

    /// Record the negotiated NsCodec codec id and color-loss level so the
    /// encoder selection path can build an `NsCodecHandler` for this session.
    #[cfg(feature = "nscodec")]
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn set_nscodec(&mut self, nscodec: Option<(u8, u8)>) {
        self.nscodec = nscodec
    }
}

impl Default for UpdateEncoderCodecs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(feature = "__bench", visibility::make(pub))]
pub(crate) struct UpdateEncoder {
    desktop_size: DesktopSize,
    framebuffer: Option<Framebuffer>,
    bitmap_updater: Option<BitmapUpdater>,
    /// Negotiated MultifragmentUpdate reassembly buffer size. Used to split
    /// oversized bitmaps into strips that fit within the limit when sent as
    /// uncompressed surface commands.
    max_request_size: usize,
    /// Client's advertised New Pointer Update cache size (MS-RDPBCGR 2.2.7.1.5
    /// `pointerCacheSize`). Zero means the client did not advertise support for the
    /// New Pointer Update; `RGBAPointer`/`LargePointer` then go out as the default pointer
    /// and `CachedPointer` is skipped, rather than sending a PDU the client did not agree to
    /// accept.
    pointer_cache_size: u16,
    /// Client's advertised Large Pointer Capability Set flags (MS-RDPBCGR 2.2.7.2.7).
    /// Empty when the client didn't send this capability set at all. Governs both the
    /// pointer size ceiling for `RGBAPointer` (32x32 with no flags, 96x96 with
    /// `UP_TO_96X96_PIXELS`) and whether the dedicated `LargePointer` update, up to
    /// 384x384, is usable at all (`UP_TO_384X384_PIXELS`).
    large_pointer_flags: LargePointerSupportFlags,
}

impl fmt::Debug for UpdateEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpdateEncoder")
            .field("bitmap_update", &self.bitmap_updater)
            .finish()
    }
}

impl UpdateEncoder {
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn new(
        desktop_size: DesktopSize,
        surface_flags: CmdFlags,
        codecs: UpdateEncoderCodecs,
        max_request_size: u32,
    ) -> Result<Self> {
        let bitmap_updater = if surface_flags.contains(CmdFlags::SET_SURFACE_BITS) {
            match codecs {
                #[cfg(feature = "qoiz")]
                UpdateEncoderCodecs { qoiz: Some(id), .. } => {
                    BitmapUpdater::Qoiz(QoizHandler::new(id).context("failed to initialize qoiz handler")?)
                }
                #[cfg(feature = "qoi")]
                UpdateEncoderCodecs { qoi: Some(id), .. } => BitmapUpdater::Qoi(QoiHandler::new(id)),
                UpdateEncoderCodecs {
                    remotefx: Some((algo, id)),
                    ..
                } => BitmapUpdater::RemoteFx(RemoteFxHandler::new(algo, id, desktop_size)),
                // NSCodec is the lowest-priority codec because it predates
                // RemoteFX and produces larger output. It's relevant mainly
                // for clients (notably macOS Microsoft Remote Desktop /
                // Windows App) whose legacy bitmap-codec list advertises
                // only NSCodec — those clients would otherwise fall through
                // to raw/RLE BitmapUpdate at much higher bandwidth.
                #[cfg(feature = "nscodec")]
                UpdateEncoderCodecs {
                    nscodec: Some((id, cll)),
                    ..
                } => BitmapUpdater::NsCodec(NsCodecHandler::new(id, cll)),
                _ => BitmapUpdater::None(NoneHandler),
            }
        } else {
            BitmapUpdater::Bitmap(BitmapHandler::new())
        };

        Ok(Self {
            desktop_size,
            framebuffer: None,
            bitmap_updater: Some(bitmap_updater),
            max_request_size: usize::try_from(max_request_size).context("max_request_size")?,
            pointer_cache_size: 0,
            large_pointer_flags: LargePointerSupportFlags::empty(),
        })
    }

    /// The client's negotiated New Pointer Update cache size (MS-RDPBCGR 2.2.7.1.5) and
    /// Large Pointer Capability Set flags (2.2.7.2.7). Without them, the client gets only
    /// its default pointer.
    pub(crate) fn with_pointer_capabilities(
        mut self,
        pointer_cache_size: u16,
        large_pointer_flags: LargePointerSupportFlags,
    ) -> Self {
        self.pointer_cache_size = pointer_cache_size;
        self.large_pointer_flags = large_pointer_flags;
        self
    }

    /// Whether a `width`x`height` pointer shape fits the Color/New Pointer Update
    /// ceiling: 96x96 if the client's Large Pointer Capability Set includes
    /// `UP_TO_96X96_PIXELS` (MS-RDPBCGR 2.2.9.1.1.4.4's width/height field docs; New
    /// Pointer Update carries the same `ColorPointerAttribute` internally, so the same
    /// ceiling applies to both), 32x32 otherwise. Shapes above this ceiling need the
    /// dedicated Large Pointer Update instead (see `large_pointer_update_supported`).
    fn color_or_new_pointer_size_ok(&self, width: u16, height: u16) -> bool {
        let max = if self
            .large_pointer_flags
            .contains(LargePointerSupportFlags::UP_TO_96X96_PIXELS)
        {
            96
        } else {
            32
        };
        width <= max && height <= max
    }

    /// Whether the client negotiated the dedicated Fast-Path Large Pointer Update
    /// itself. Per MS-RDPBCGR 2.2.7.2.7, only `UP_TO_384X384_PIXELS` unlocks this PDU;
    /// `UP_TO_96X96_PIXELS` alone only raises the Color/New Pointer Update ceiling and
    /// does not enable it.
    fn large_pointer_update_supported(&self) -> bool {
        self.large_pointer_flags
            .contains(LargePointerSupportFlags::UP_TO_384X384_PIXELS)
    }

    /// Warpgate fork: the update a pointer shape goes out as. Within the client's
    /// Color/New Pointer ceiling it is a New Pointer Update; above it, a Large Pointer
    /// Update when the client takes those (up to 384x384). A shape the client can't take
    /// either way becomes its default pointer rather than being dropped, so the client
    /// never keeps showing a shape the server has already left.
    fn pointer_shape(&self, ptr: RGBAPointer) -> Result<UpdateFragmenter> {
        if self.pointer_cache_size == 0 {
            debug!("Sending the default pointer: client did not advertise New Pointer Update support");
            return UpdateEncoder::default_pointer();
        }
        if self.color_or_new_pointer_size_ok(ptr.width, ptr.height) {
            return UpdateEncoder::rgba_pointer(ptr);
        }
        if self.large_pointer_update_supported() && ptr.width <= 384 && ptr.height <= 384 {
            return UpdateEncoder::large_pointer(LargePointer {
                cache_index: ptr.cache_index,
                width: ptr.width,
                height: ptr.height,
                hot_x: ptr.hot_x,
                hot_y: ptr.hot_y,
                data: ptr.data,
            });
        }
        debug!(
            "Sending the default pointer: {}x{} exceeds the client's negotiated pointer size ceiling",
            ptr.width, ptr.height,
        );
        UpdateEncoder::default_pointer()
    }

    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) fn update(&mut self, update: DisplayUpdate) -> EncoderIter<'_> {
        EncoderIter {
            encoder: self,
            state: State::Start(update),
        }
    }

    pub(crate) fn set_desktop_size(&mut self, size: DesktopSize) {
        self.desktop_size = size;
        self.bitmap_updater
            .as_mut()
            .expect("bitmap updater always Some")
            .set_desktop_size(size);
    }

    fn rgba_pointer(ptr: RGBAPointer) -> Result<UpdateFragmenter> {
        let xor_mask = ptr.data;

        let hot_spot = Point16 {
            x: ptr.hot_x,
            y: ptr.hot_y,
        };
        let color_pointer = ColorPointerAttribute {
            cache_index: ptr.cache_index,
            hot_spot,
            width: ptr.width,
            height: ptr.height,
            xor_mask: &xor_mask,
            and_mask: &[],
        };
        let ptr = PointerAttribute {
            xor_bpp: 32,
            color_pointer,
        };
        Ok(UpdateFragmenter::new(UpdateCode::NewPointer, encode_vec(&ptr)?))
    }

    fn large_pointer(ptr: LargePointer) -> Result<UpdateFragmenter> {
        let xor_mask = ptr.data;
        // Warpgate fork: the AND mask is not optional (MS-RDPBCGR 2.2.9.1.2.1.11): 1 bpp, rows
        // in the XOR mask's bottom-up order, each padded to 2 bytes. A set bit keeps the screen
        // pixel, so it is set where the pointer is fully transparent.
        let and_stride = usize::from(ptr.width).div_ceil(16) * 2;
        let mut and_mask = vec![0u8; and_stride * usize::from(ptr.height)];
        for (row, pixels) in xor_mask.chunks_exact(usize::from(ptr.width) * 4).enumerate() {
            for (col, pixel) in pixels.chunks_exact(4).enumerate() {
                if pixel[3] == 0 {
                    and_mask[row * and_stride + col / 8] |= 0x80 >> (col % 8);
                }
            }
        }

        let hot_spot = Point16 {
            x: ptr.hot_x,
            y: ptr.hot_y,
        };
        let large_pointer = LargePointerAttribute {
            xor_bpp: 32,
            cache_index: ptr.cache_index,
            hot_spot,
            width: ptr.width,
            height: ptr.height,
            xor_mask: &xor_mask,
            and_mask: &and_mask,
        };
        Ok(UpdateFragmenter::new(UpdateCode::LargePointer, encode_vec(&large_pointer)?))
    }

    fn color_pointer(ptr: ColorPointer) -> Result<UpdateFragmenter> {
        let hot_spot = Point16 {
            x: ptr.hot_x,
            y: ptr.hot_y,
        };
        let ptr = ColorPointerAttribute {
            cache_index: ptr.cache_index,
            hot_spot,
            width: ptr.width,
            height: ptr.height,
            xor_mask: &ptr.xor_mask,
            and_mask: &ptr.and_mask,
        };
        Ok(UpdateFragmenter::new(UpdateCode::ColorPointer, encode_vec(&ptr)?))
    }

    fn cached_pointer(cache_index: u16) -> Result<UpdateFragmenter> {
        let ptr = CachedPointerAttribute { cache_index };
        Ok(UpdateFragmenter::new(UpdateCode::CachedPointer, encode_vec(&ptr)?))
    }

    fn default_pointer() -> Result<UpdateFragmenter> {
        Ok(UpdateFragmenter::new(UpdateCode::DefaultPointer, vec![]))
    }

    fn hide_pointer() -> Result<UpdateFragmenter> {
        Ok(UpdateFragmenter::new(UpdateCode::HiddenPointer, vec![]))
    }

    fn pointer_position(pos: PointerPositionAttribute) -> Result<UpdateFragmenter> {
        Ok(UpdateFragmenter::new(UpdateCode::PositionPointer, encode_vec(&pos)?))
    }

    fn bitmap_diffs(&mut self, bitmap: &BitmapUpdate) -> Vec<Rect> {
        // TODO: we may want to make it optional for servers that already provide damaged regions
        const USE_DIFFS: bool = true;

        let diffs = if let Some(Framebuffer {
            data,
            stride,
            width,
            height,
            ..
        }) = USE_DIFFS.then_some(self.framebuffer.as_ref()).flatten()
        {
            find_different_rects_sub::<4>(
                data,
                *stride,
                width.get().into(),
                height.get().into(),
                &bitmap.data,
                bitmap.stride.get(),
                bitmap.width.get().into(),
                bitmap.height.get().into(),
                bitmap.x.into(),
                bitmap.y.into(),
            )
        } else {
            vec![Rect {
                x: 0,
                y: 0,
                width: bitmap.width.get().into(),
                height: bitmap.height.get().into(),
            }]
        };

        // Subdivide diff rects whose uncompressed size would exceed the
        // MultifragmentUpdate reassembly buffer.
        let mut tiled = Vec::with_capacity(diffs.len());
        for rect in diffs {
            if rect.width * rect.height * 4 <= self.max_request_size {
                tiled.push(rect);
            } else {
                let rects = self.split_diff(rect);
                tiled.extend(rects);
            }
        }
        tiled
    }

    /// Split a rect into tiles that fit within `max_request_size`.
    /// Splits by height first, then by width within each horizontal strip.
    fn split_diff(&self, rect: Rect) -> Vec<Rect> {
        let mut rects = Vec::new();

        let max_height = (self.max_request_size / (rect.width * 4)).max(1);
        let mut y = rect.y;
        let y_end = rect.y + rect.height;
        while y < y_end {
            let h = (y_end - y).min(max_height);
            // Width splitting is unlikely in practice (would require
            // max_request_size < ~256 KB), but ensures correctness.
            let max_width = (self.max_request_size / (h * 4)).max(1);
            let mut x = rect.x;
            let x_end = rect.x + rect.width;
            while x < x_end {
                let w = (x_end - x).min(max_width);
                rects.push(Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                });
                x += max_width;
            }
            y += max_height;
        }

        rects
    }

    fn bitmap_update_framebuffer(&mut self, bitmap: BitmapUpdate, diffs: &[Rect]) {
        if bitmap.x == 0
            && bitmap.y == 0
            && bitmap.width.get() == self.desktop_size.width
            && bitmap.height.get() == self.desktop_size.height
        {
            match bitmap.try_into() {
                Ok(framebuffer) => self.framebuffer = Some(framebuffer),
                Err(err) => warn!("Failed to convert bitmap to framebuffer: {}", err),
            }
        } else if let Some(fb) = self.framebuffer.as_mut() {
            fb.update_diffs(&bitmap, diffs);
        }
    }

    async fn bitmap(&mut self, bitmap: BitmapUpdate) -> Result<UpdateFragmenter> {
        // Move the bitmap updater to satisfy spawn_blocking 'static requirement.
        // It is restored after the blocking operation completes.
        let mut updater = self.bitmap_updater.take().expect("bitmap updater always Some");

        let (result, updater) = tokio::task::spawn_blocking(move || {
            let result = time_warn!("Encoding bitmap", 10, updater.handle(&bitmap));
            (result, updater)
        })
        .await?;

        self.bitmap_updater = Some(updater);

        result
    }
}

#[derive(Debug, Default)]
enum State {
    Start(DisplayUpdate),
    BitmapDiffs {
        diffs: Vec<Rect>,
        bitmap: BitmapUpdate,
        pos: usize,
    },
    #[default]
    Ended,
}

#[cfg_attr(feature = "__bench", visibility::make(pub))]
pub(crate) struct EncoderIter<'a> {
    encoder: &'a mut UpdateEncoder,
    state: State,
}

impl EncoderIter<'_> {
    #[cfg_attr(feature = "__bench", visibility::make(pub))]
    pub(crate) async fn next(&mut self) -> Option<Result<UpdateFragmenter>> {
        loop {
            let state = core::mem::take(&mut self.state);
            let encoder = &mut self.encoder;

            let res = match state {
                State::Start(update) => match update {
                    DisplayUpdate::Bitmap(bitmap) => {
                        let ds = encoder.desktop_size;
                        if bitmap.x + bitmap.width.get() > ds.width || bitmap.y + bitmap.height.get() > ds.height {
                            debug!(
                                "Dropping bitmap update that exceeds desktop size: \
                                 bitmap ({}, {}) {}x{} vs desktop {}x{}",
                                bitmap.x, bitmap.y, bitmap.width, bitmap.height, ds.width, ds.height,
                            );
                            continue;
                        }
                        let diffs = encoder.bitmap_diffs(&bitmap);
                        self.state = State::BitmapDiffs { diffs, bitmap, pos: 0 };
                        continue;
                    }
                    DisplayUpdate::PointerPosition(pos) => UpdateEncoder::pointer_position(pos),
                    DisplayUpdate::RGBAPointer(ptr) => encoder.pointer_shape(ptr),
                    DisplayUpdate::LargePointer(ptr) => encoder.pointer_shape(RGBAPointer {
                        cache_index: ptr.cache_index,
                        width: ptr.width,
                        height: ptr.height,
                        hot_x: ptr.hot_x,
                        hot_y: ptr.hot_y,
                        data: ptr.data,
                    }),
                    DisplayUpdate::ColorPointer(ptr) => UpdateEncoder::color_pointer(ptr),
                    DisplayUpdate::HidePointer => UpdateEncoder::hide_pointer(),
                    DisplayUpdate::DefaultPointer => UpdateEncoder::default_pointer(),
                    DisplayUpdate::CachedPointer(idx) => {
                        if encoder.pointer_cache_size == 0 {
                            debug!(
                                "Dropping CachedPointer update: client did not advertise New Pointer Update support"
                            );
                            continue;
                        }
                        UpdateEncoder::cached_pointer(idx)
                    }
                    DisplayUpdate::Resize(_) => return None,
                },
                State::BitmapDiffs { diffs, bitmap, pos } => {
                    let Some(rect) = diffs.get(pos) else {
                        encoder.bitmap_update_framebuffer(bitmap, &diffs);
                        self.state = State::Ended;
                        return None;
                    };
                    let Rect { x, y, width, height } = *rect;

                    let x = match u16::try_from(x) {
                        Ok(x) => x,
                        Err(_) => return Some(Err(anyhow!("invalid `x`: out of range integral conversion"))),
                    };
                    let y = match u16::try_from(y) {
                        Ok(y) => y,
                        Err(_) => return Some(Err(anyhow!("invalid `y`: out of range integral conversion"))),
                    };
                    let width = match u16::try_from(width) {
                        Ok(width) => match NonZeroU16::new(width) {
                            Some(width) => width,
                            None => return Some(Err(anyhow!("rectangle width cannot be zero"))),
                        },
                        Err(_) => return Some(Err(anyhow!("invalid `width`: out of range integral conversion"))),
                    };
                    let height = match u16::try_from(height) {
                        Ok(height) => match NonZeroU16::new(height) {
                            Some(height) => height,
                            None => return Some(Err(anyhow!("rectangle height cannot be zero"))),
                        },
                        Err(_) => return Some(Err(anyhow!("invalid `height`: out of range integral conversion"))),
                    };

                    let Some(sub) = bitmap.sub(x, y, width, height) else {
                        warn!("Failed to extract bitmap subregion");
                        return None;
                    };
                    self.state = State::BitmapDiffs {
                        diffs,
                        bitmap,
                        pos: pos + 1,
                    };
                    encoder.bitmap(sub).await
                }
                State::Ended => return None,
            };

            return Some(res);
        }
    }
}

#[derive(Debug)]
enum BitmapUpdater {
    None(NoneHandler),
    Bitmap(BitmapHandler),
    RemoteFx(RemoteFxHandler),
    #[cfg(feature = "qoi")]
    Qoi(QoiHandler),
    #[cfg(feature = "qoiz")]
    Qoiz(QoizHandler),
    #[cfg(feature = "nscodec")]
    NsCodec(NsCodecHandler),
}

impl BitmapUpdater {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        match self {
            Self::None(up) => up.handle(bitmap),
            Self::Bitmap(up) => up.handle(bitmap),
            Self::RemoteFx(up) => up.handle(bitmap),
            #[cfg(feature = "qoi")]
            Self::Qoi(up) => up.handle(bitmap),
            #[cfg(feature = "qoiz")]
            Self::Qoiz(up) => up.handle(bitmap),
            #[cfg(feature = "nscodec")]
            Self::NsCodec(up) => up.handle(bitmap),
        }
    }

    fn set_desktop_size(&mut self, size: DesktopSize) {
        if let Self::RemoteFx(up) = self {
            up.set_desktop_size(size)
        }
    }
}

trait BitmapUpdateHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter>;
}

#[derive(Clone, Debug)]
struct NoneHandler;

impl BitmapUpdateHandler for NoneHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let stride = usize::from(bitmap.format.bytes_per_pixel()) * usize::from(bitmap.width.get());
        let mut data = Vec::with_capacity(stride * usize::from(bitmap.height.get()));
        for row in bitmap.data.chunks(bitmap.stride.get()).rev() {
            data.extend_from_slice(&row[..stride]);
        }
        set_surface(bitmap, CodecId::None.as_u8(), &data)
    }
}

#[derive(Clone)]
struct BitmapHandler {
    bitmap: BitmapEncoder,
}

impl fmt::Debug for BitmapHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BitmapHandler").finish()
    }
}

impl BitmapHandler {
    fn new() -> Self {
        Self {
            bitmap: BitmapEncoder::new(),
        }
    }
}

impl BitmapUpdateHandler for BitmapHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let mut buffer = vec![0; bitmap.data.len() * 2]; // TODO: estimate bitmap encoded size
        let len = loop {
            match self.bitmap.encode(bitmap, buffer.as_mut_slice()) {
                Err(err) => match err {
                    BitmapEncodeError::Encode(e) => match e.kind() {
                        ironrdp_core::EncodeErrorKind::NotEnoughBytes { .. } => {
                            buffer.resize(buffer.len() * 2, 0);
                            debug!("encoder buffer resized to: {}", buffer.len() * 2);
                        }
                        _ => Err(e).context("bitmap encode error")?,
                    },
                    BitmapEncodeError::Rle(e) => Err(e).context("bitmap RLE encode error")?,
                },
                Ok(len) => break len,
            }
        };

        buffer.truncate(len);
        Ok(UpdateFragmenter::new(UpdateCode::Bitmap, buffer))
    }
}

#[derive(Debug, Clone)]
struct RemoteFxHandler {
    remotefx: RfxEncoder,
    codec_id: u8,
    desktop_size: Option<DesktopSize>,
}

impl RemoteFxHandler {
    fn new(algo: EntropyBits, codec_id: u8, desktop_size: DesktopSize) -> Self {
        Self {
            remotefx: RfxEncoder::new(algo),
            desktop_size: Some(desktop_size),
            codec_id,
        }
    }

    fn set_desktop_size(&mut self, size: DesktopSize) {
        self.desktop_size = Some(size);
    }
}

impl BitmapUpdateHandler for RemoteFxHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let mut buffer = vec![0; bitmap.data.len()];
        let len = loop {
            match self
                .remotefx
                .encode(bitmap, buffer.as_mut_slice(), self.desktop_size)
            {
                Err(e) => match e.kind() {
                    ironrdp_core::EncodeErrorKind::NotEnoughBytes { .. } => {
                        buffer.resize(buffer.len() * 2, 0);
                        debug!("encoder buffer resized to: {}", buffer.len());
                    }
                    _ => Err(e).context("RemoteFX encode error")?,
                },
                Ok(len) => break len,
            }
        };

        let fragmenter = set_surface(bitmap, self.codec_id, &buffer[..len])?;
        // The RFX header blocks ride the first message only, and `desktop_size` being `Some`
        // is what emits them. It may only be cleared once a message carrying them is built:
        // an encode that overruns `buffer` (any tile-padded rect that compresses larger than
        // its raw size) is retried, and a retry that no longer sees them leaves the stream
        // without a header it can never send again — every later frame is then rejected by
        // the client for the life of the connection.
        self.desktop_size = None;
        Ok(fragmenter)
    }
}

#[cfg(feature = "qoi")]
#[derive(Clone, Debug)]
struct QoiHandler {
    codec_id: u8,
}

#[cfg(feature = "qoi")]
impl QoiHandler {
    fn new(codec_id: u8) -> Self {
        Self { codec_id }
    }
}

#[cfg(feature = "qoi")]
impl BitmapUpdateHandler for QoiHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let data = qoi_encode(bitmap)?;
        set_surface(bitmap, self.codec_id, &data)
    }
}

#[cfg(feature = "qoiz")]
struct QoizHandler {
    codec_id: u8,
    zctxt: zstd_safe::CCtx<'static>,
}

#[cfg(feature = "qoiz")]
impl fmt::Debug for QoizHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QoizHandler").field("codec_id", &self.codec_id).finish()
    }
}

#[cfg(feature = "qoiz")]
impl QoizHandler {
    fn new(codec_id: u8) -> Result<Self> {
        let mut zctxt = zstd_safe::CCtx::default();

        zctxt
            .set_parameter(zstd_safe::CParameter::CompressionLevel(3))
            .map_err(|code| {
                anyhow!(
                    "failed to set zstd compression level: {}",
                    zstd_safe::get_error_name(code)
                )
            })?;
        zctxt
            .set_parameter(zstd_safe::CParameter::EnableLongDistanceMatching(true))
            .map_err(|code| {
                anyhow!(
                    "failed to set zstd enable long distance matching: {}",
                    zstd_safe::get_error_name(code)
                )
            })?;

        Ok(Self { codec_id, zctxt })
    }
}

#[cfg(feature = "qoiz")]
impl BitmapUpdateHandler for QoizHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let qoi = qoi_encode(bitmap)?;
        let mut inb = zstd_safe::InBuffer::around(&qoi);
        let mut data = vec![0; qoi.len()];
        let mut outb;
        let mut pos = 0;

        loop {
            outb = zstd_safe::OutBuffer::around_pos(data.as_mut_slice(), pos);
            let res = self
                .zctxt
                .compress_stream2(
                    &mut outb,
                    &mut inb,
                    zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_flush,
                )
                .map_err(|code| anyhow!("failed to Zstd compress: {}", zstd_safe::get_error_name(code)))?;
            if res == 0 {
                break;
            }
            pos = outb.pos();
            data.resize(data.len() + res, 0);
        }

        set_surface(bitmap, self.codec_id, outb.as_slice())
    }
}

#[cfg(feature = "nscodec")]
#[derive(Clone, Debug)]
struct NsCodecHandler {
    codec_id: u8,
    color_loss_level: u8,
}

#[cfg(feature = "nscodec")]
impl NsCodecHandler {
    fn new(codec_id: u8, color_loss_level: u8) -> Self {
        Self {
            codec_id,
            color_loss_level,
        }
    }
}

#[cfg(feature = "nscodec")]
impl BitmapUpdateHandler for NsCodecHandler {
    fn handle(&mut self, bitmap: &BitmapUpdate) -> Result<UpdateFragmenter> {
        let data = ironrdp_nscodec::encoder::encode(
            &bitmap.data,
            bitmap.width.get(),
            bitmap.height.get(),
            bitmap.stride.get(),
            bitmap.format,
            self.color_loss_level,
        );
        set_surface(bitmap, self.codec_id, &data)
    }
}

#[cfg(feature = "qoi")]
fn qoi_encode(bitmap: &BitmapUpdate) -> Result<Vec<u8>> {
    use ironrdp_graphics::image_processing::PixelFormat::*;
    // Map every 4-byte input — whether it nominally has an alpha byte or
    // an "X" filler — to the 3-channel-output `*x` variant of
    // `RawChannels`. The qoi crate selects `Channels::Rgb` vs
    // `Channels::Rgba` for the QOI header from this enum: `*x` and `*r/g/b`
    // produce `Rgb`; `*a` produces `Rgba`. The `ironrdp-session` NSCodec-
    // free decode path in `fast_path.rs::qoi_apply` only supports
    // `Channels::Rgb` and explicitly drops `Channels::Rgba` frames with
    // `WARN: Unsupported RGBA QOI data`, so the previous "honest" mapping
    // (`BgrA32 -> Bgra`, etc.) produced output that no IronRDP client
    // could decode — every QOI session rendered a blank screen.
    //
    // Server-side bitmap captures are functionally opaque (the alpha byte
    // is either always 0xFF or treated as filler), so discarding it is
    // safe and matches what every successful legacy bitmap path
    // already does.
    let raw_channels = match bitmap.format {
        ARgb32 | XRgb32 => qoi::RawChannels::Xrgb,
        ABgr32 | XBgr32 => qoi::RawChannels::Xbgr,
        BgrA32 | BgrX32 => qoi::RawChannels::Bgrx,
        RgbA32 | RgbX32 => qoi::RawChannels::Rgbx,
    };
    let enc = qoi::EncoderBuilder::new(&bitmap.data, bitmap.width.get().into(), bitmap.height.get().into())
        .stride(bitmap.stride.get())
        .raw_channels(raw_channels)
        .build()?;
    Ok(enc.encode_to_vec()?)
}

fn set_surface(bitmap: &BitmapUpdate, codec_id: u8, data: &[u8]) -> Result<UpdateFragmenter> {
    let destination = ExclusiveRectangle {
        left: bitmap.x,
        top: bitmap.y,
        right: bitmap.x + bitmap.width.get(),
        bottom: bitmap.y + bitmap.height.get(),
    };
    let extended_bitmap_data = ExtendedBitmapDataPdu {
        bpp: bitmap.format.bytes_per_pixel() * 8,
        width: bitmap.width.get(),
        height: bitmap.height.get(),
        codec_id,
        header: None,
        data,
    };
    let pdu = SurfaceBitsPdu {
        destination,
        extended_bitmap_data,
    };
    let cmd = SurfaceCommand::SetSurfaceBits(pdu);
    Ok(UpdateFragmenter::new(UpdateCode::SurfaceCommands, encode_vec(&cmd)?))
}

#[cfg(test)]
mod pointer_tests {
    use ironrdp_core::{ReadCursor, decode, decode_cursor};
    use ironrdp_graphics::pointer::{DecodedPointer, PointerBitmapTarget};
    use ironrdp_pdu::fast_path::{FastPathHeader, FastPathUpdatePdu, UpdateCode};
    use ironrdp_pdu::pointer::{LargePointerAttribute, PointerAttribute};
    use ironrdp_pdu::rdp::capability_sets::{CmdFlags, LargePointerSupportFlags};

    use super::{UpdateEncoder, UpdateEncoderCodecs};
    use crate::{DesktopSize, RGBAPointer};

    const BOTH: LargePointerSupportFlags =
        LargePointerSupportFlags::UP_TO_96X96_PIXELS.union(LargePointerSupportFlags::UP_TO_384X384_PIXELS);

    fn encoder(pointer_cache_size: u16, large_pointer_flags: LargePointerSupportFlags) -> UpdateEncoder {
        UpdateEncoder::new(
            DesktopSize {
                width: 64,
                height: 64,
            },
            CmdFlags::empty(),
            UpdateEncoderCodecs::new(),
            8 * 1024 * 1024,
        )
        .unwrap()
        .with_pointer_capabilities(pointer_cache_size, large_pointer_flags)
    }

    /// A `side`x`side` pointer, BGRA bottom-up as on the wire, every pixel its own row's value.
    fn pointer(side: u16) -> RGBAPointer {
        let data = (0..side)
            .rev()
            .flat_map(|row| {
                let b = u8::try_from(row % 251).unwrap();
                core::iter::repeat_n([b, 1, 2, 255], usize::from(side)).flatten()
            })
            .collect();
        RGBAPointer {
            cache_index: 0,
            width: side,
            height: side,
            hot_x: 3,
            hot_y: 5,
            data,
        }
    }

    /// The update as the client reads it off the wire: its code and reassembled data.
    fn sent(encoder: &UpdateEncoder, pointer: RGBAPointer) -> (UpdateCode, Vec<u8>) {
        let mut fragmenter = encoder.pointer_shape(pointer).unwrap();
        let mut buffer = vec![0; 32 * 1024];
        let (mut code, mut data) = (None, Vec::new());
        while let Some(written) = fragmenter.next(&mut buffer) {
            let mut cursor = ReadCursor::new(&buffer[..written]);
            let _: FastPathHeader = decode_cursor(&mut cursor).unwrap();
            let update: FastPathUpdatePdu<'_> = decode_cursor(&mut cursor).unwrap();
            code = Some(update.update_code);
            data.extend_from_slice(update.data);
        }
        (code.unwrap(), data)
    }

    fn code(encoder: &UpdateEncoder, side: u16) -> UpdateCode {
        sent(encoder, pointer(side)).0
    }

    #[test]
    fn a_pointer_goes_out_as_the_largest_update_the_client_negotiated() {
        // oracle: spec MS-RDPBCGR 2.2.7.2.7
        let none = encoder(25, LargePointerSupportFlags::empty());
        assert_eq!(code(&none, 32), UpdateCode::NewPointer);
        assert_eq!(code(&none, 48), UpdateCode::DefaultPointer);

        let up_to_96 = encoder(25, LargePointerSupportFlags::UP_TO_96X96_PIXELS);
        assert_eq!(code(&up_to_96, 96), UpdateCode::NewPointer);
        assert_eq!(code(&up_to_96, 97), UpdateCode::DefaultPointer);

        let both = encoder(25, BOTH);
        assert_eq!(code(&both, 96), UpdateCode::NewPointer);
        assert_eq!(code(&both, 97), UpdateCode::LargePointer);
        assert_eq!(code(&both, 384), UpdateCode::LargePointer);
        assert_eq!(code(&both, 385), UpdateCode::DefaultPointer);
    }

    #[test]
    fn without_new_pointer_support_the_client_gets_its_default_pointer() {
        // oracle: spec MS-RDPBCGR 2.2.7.1.5
        assert_eq!(code(&encoder(0, BOTH), 16), UpdateCode::DefaultPointer);
    }

    /// What the client decodes from a Large Pointer Update: the shape it was given.
    #[test]
    fn the_client_decodes_a_large_pointer_as_sent() {
        let (code, data) = sent(&encoder(25, BOTH), pointer(200));
        assert_eq!(code, UpdateCode::LargePointer);

        let attribute: LargePointerAttribute<'_> = decode(&data).unwrap();
        assert_eq!((attribute.width, attribute.height), (200, 200));
        // 200 px is 25 bytes of mask, padded to 26; the pointer is opaque, so no bit is set.
        assert_eq!(attribute.and_mask, vec![0; 26 * 200]);
        assert_eq!((attribute.hot_spot.x, attribute.hot_spot.y), (3, 5));
        let decoded = DecodedPointer::decode_large_pointer_attribute(&attribute, PointerBitmapTarget::Accelerated).unwrap();
        // The client's bitmap is RGBA, top row first; the wire's XOR mask is bottom-up.
        assert_eq!(decoded.bitmap_data[..4], [2, 1, 0, 255]);
        assert_eq!(decoded.bitmap_data[decoded.bitmap_data.len() - 4..], [2, 1, 199, 255]);
    }

    #[test]
    fn a_pointer_within_the_ceiling_is_a_new_pointer_update() {
        let (code, data) = sent(&encoder(25, BOTH), pointer(48));
        assert_eq!(code, UpdateCode::NewPointer);
        let attribute: PointerAttribute<'_> = decode(&data).unwrap();
        assert_eq!((attribute.color_pointer.width, attribute.xor_bpp), (48, 32));
    }
}
