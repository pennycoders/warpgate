use core::iter;

use ironrdp_core::{Encode as _, WriteCursor, cast_int, cast_length};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_graphics::rdp6::{
    ABgrChannels, ARgbChannels, BgrAChannels, BitmapEncodeError, BitmapStreamEncoder, RgbAChannels,
};
use ironrdp_pdu::bitmap::{self, BitmapData, BitmapUpdateData, Compression};
use ironrdp_pdu::geometry::InclusiveRectangle;

use crate::BitmapUpdate;

// PERF: we could also remove the need for this buffer
#[derive(Clone)]
pub(crate) struct BitmapEncoder {
    buffer: Vec<u8>,
}

impl BitmapEncoder {
    pub(crate) fn new() -> Self {
        Self {
            buffer: vec![0; usize::from(u16::MAX)],
        }
    }

    pub(crate) fn encode(&mut self, bitmap: &BitmapUpdate, output: &mut [u8]) -> Result<usize, BitmapEncodeError> {
        // Each row is padded to a multiple of 4 pixels with copies of its last pixel.
        // The destination rectangle keeps the real width, so the client clips the padding.
        let width = bitmap.width.get();
        let padded_width = width.next_multiple_of(4);
        let bytes_per_pixel = u16::from(bitmap.format.bytes_per_pixel());
        let row_len = usize::from(width * bytes_per_pixel);
        let padding = usize::from(padded_width - width);
        let padded_row_len = padded_width * bytes_per_pixel;
        let chunk_height = u16::MAX / padded_row_len;

        let mut cursor = WriteCursor::new(output);
        let stride = bitmap.stride.get();
        let chunks = bitmap.data.chunks(stride * usize::from(chunk_height));

        let total = cast_int!("chunks length lower bound", chunks.size_hint().0).map_err(BitmapEncodeError::Encode)?;
        BitmapUpdateData::encode_header(total, &mut cursor).map_err(BitmapEncodeError::Encode)?;

        for (i, chunk) in chunks.enumerate() {
            // The last row of a bitmap narrower than its stride ends at the row's pixels.
            let height = cast_int!("bitmap height", chunk.len().div_ceil(stride)).map_err(BitmapEncodeError::Encode)?;
            let i: u16 = cast_int!("chunk idx", i).map_err(BitmapEncodeError::Encode)?;
            let top = bitmap.y + i * chunk_height;

            let encoder = BitmapStreamEncoder::new(usize::from(padded_width), usize::from(height));

            let len = {
                let pixels = chunk.chunks(stride).map(|row| &row[..row_len]).rev().flat_map(|row| {
                    let last = &row[row_len - usize::from(bytes_per_pixel)..];
                    row.chunks(usize::from(bytes_per_pixel))
                        .chain(iter::repeat_n(last, padding))
                });

                Self::encode_iter(encoder, bitmap.format, pixels, self.buffer.as_mut_slice())?
            };

            let data = BitmapData {
                rectangle: InclusiveRectangle {
                    left: bitmap.x,
                    top,
                    right: bitmap.x + width - 1,
                    bottom: top + height - 1,
                },
                width: padded_width,
                height,
                bits_per_pixel: u16::from(bitmap.format.bytes_per_pixel()) * 8,
                compression_flags: Compression::BITMAP_COMPRESSION,
                compressed_data_header: Some(bitmap::CompressedDataHeader {
                    main_body_size: cast_length!("main body size", len).map_err(BitmapEncodeError::Encode)?,
                    // In bytes, as the FreeRDP server sends it and clients read it, although
                    // MS-RDPBCGR 2.2.9.1.1.3.1.2.3 words it as pixels.
                    scan_width: padded_row_len,
                    uncompressed_size: height * padded_row_len,
                }),
                bitmap_data: &self.buffer[..len],
            };

            data.encode(&mut cursor).map_err(BitmapEncodeError::Encode)?;
        }

        Ok(cursor.pos())
    }

    fn encode_iter<'a, P>(
        mut encoder: BitmapStreamEncoder,
        format: PixelFormat,
        src: P,
        dst: &mut [u8],
    ) -> Result<usize, BitmapEncodeError>
    where
        P: Iterator<Item = &'a [u8]> + Clone,
    {
        let written = match format {
            PixelFormat::ARgb32 | PixelFormat::XRgb32 => {
                encoder.encode_pixels_stream::<_, ARgbChannels>(src, dst, true)?
            }
            PixelFormat::RgbA32 | PixelFormat::RgbX32 => {
                encoder.encode_pixels_stream::<_, RgbAChannels>(src, dst, true)?
            }
            PixelFormat::ABgr32 | PixelFormat::XBgr32 => {
                encoder.encode_pixels_stream::<_, ABgrChannels>(src, dst, true)?
            }
            PixelFormat::BgrA32 | PixelFormat::BgrX32 => {
                encoder.encode_pixels_stream::<_, BgrAChannels>(src, dst, true)?
            }
        };

        Ok(written)
    }
}
