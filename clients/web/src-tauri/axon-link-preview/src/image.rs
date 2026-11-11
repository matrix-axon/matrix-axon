//! A preview image, made safe to send (ADR 0132 § 3, last paragraph).
//!
//! Every image is decoded and re-encoded, never passed through: scaled to at
//! most 1200 pixels on its longest side, written as JPEG, or as PNG when it
//! has transparency, with any EXIF orientation applied first. So the result
//! always fits the upload contract (2 MiB, 4,000,000 pixels), and no EXIF or
//! other embedded metadata survives. PNG, JPEG, GIF and WebP are read; SVG and
//! everything else are refused. An animated image contributes its first frame.
//!
//! The dimensions are read and checked before any pixel is allocated, and
//! every pass over the bytes, the dimension read included, runs with an
//! allocation limit. A WebP file's chunk lengths are checked against the
//! bytes actually present first, because image-webp allocates a chunk's
//! declared length when it reads metadata, outside `image`'s limits.

use crate::error::{Failure, Result};
use crate::http::THUMBNAIL_BYTES;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits};
use std::io::Cursor;

/// The upload contract's pixel limit.
const MAX_PIXELS: u64 = 4_000_000;
/// The longest side of what is sent.
pub(crate) const MAX_SIDE: u32 = 1200;
/// What the decoder may allocate: one 4,000,000-pixel RGBA16 frame, and room.
const MAX_ALLOC: u64 = 64 * 1024 * 1024;
const JPEG_QUALITY: u8 = 80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Thumbnail {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime: &'static str,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

fn limits(width: u32, height: u32) -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(MAX_ALLOC);
    limits
}

fn reader(data: &[u8], limits: Limits) -> Result<ImageReader<Cursor<&[u8]>>> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| Failure::Invalid)?;
    match reader.format() {
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif) => {}
        Some(ImageFormat::WebP) => webp_chunks_fit(data)?,
        _ => return Err(Failure::Invalid),
    }
    reader.limits(limits);
    Ok(reader)
}

/// Every chunk of a WebP file lies inside the bytes received: the RIFF
/// length, then each chunk's declared length and its padding byte. So no
/// chunk image-webp reads can ask for more than the input's own size, which
/// is at most 2 MiB.
fn webp_chunks_fit(data: &[u8]) -> Result<()> {
    let word = |at: usize| -> Result<usize> {
        let bytes = data.get(at..at + 4).ok_or(Failure::Invalid)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
    };
    if data.get(..4) != Some(b"RIFF") || data.get(8..12) != Some(b"WEBP") {
        return Err(Failure::Invalid);
    }
    let end = word(4)?.checked_add(8).ok_or(Failure::Invalid)?;
    if end > data.len() {
        return Err(Failure::Invalid);
    }
    let mut at = 12;
    while at < end {
        let size = word(at + 4)?;
        let next = at
            .checked_add(8)
            .and_then(|start| start.checked_add(size))
            .ok_or(Failure::Invalid)?;
        if next > end {
            return Err(Failure::Invalid);
        }
        at = next + (size & 1);
    }
    Ok(())
}

pub(crate) fn normalize(data: &[u8]) -> Result<Thumbnail> {
    if data.len() > THUMBNAIL_BYTES {
        return Err(Failure::Quota);
    }
    // The contract's 4,000,000 pixels bound either side before the real
    // dimensions are known.
    let (width, height) = reader(data, limits(MAX_PIXELS as u32, MAX_PIXELS as u32))?
        .into_dimensions()
        .map_err(|_| Failure::Invalid)?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(Failure::Invalid);
    }
    let mut decoder = reader(data, limits(width, height))?
        .into_decoder()
        .map_err(|_| Failure::Invalid)?;
    let orientation = decoder.orientation().map_err(|_| Failure::Invalid)?;
    let mut decoded = DynamicImage::from_decoder(decoder).map_err(|_| Failure::Invalid)?;
    decoded.apply_orientation(orientation);
    if decoded.width().max(decoded.height()) > MAX_SIDE {
        decoded = decoded.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle);
    }
    let (width, height) = (decoded.width(), decoded.height());
    let transparent =
        decoded.color().has_alpha() && decoded.to_rgba8().pixels().any(|pixel| pixel.0[3] < 255);
    if transparent {
        let rgba = decoded.to_rgba8();
        let mut bytes = Vec::new();
        PngEncoder::new(&mut bytes)
            .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
            .map_err(|_| Failure::Invalid)?;
        if bytes.len() <= THUMBNAIL_BYTES {
            return Ok(Thumbnail {
                bytes,
                mime: "image/png",
                width,
                height,
            });
        }
        // A noisy transparent image can exceed the budget as PNG. Flatten it
        // onto white rather than send nothing.
        decoded = DynamicImage::ImageRgb8(flatten(&rgba));
    }
    let rgb = decoded.to_rgb8();
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
        .write_image(&rgb, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|_| Failure::Invalid)?;
    if bytes.len() > THUMBNAIL_BYTES {
        return Err(Failure::Quota);
    }
    Ok(Thumbnail {
        bytes,
        mime: "image/jpeg",
        width,
        height,
    })
}

fn flatten(rgba: &image::RgbaImage) -> image::RgbImage {
    image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let blend = |channel: u8| {
            ((u16::from(channel) * u16::from(a) + 255 * (255 - u16::from(a))) / 255) as u8
        };
        image::Rgb([blend(r), blend(g), blend(b)])
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), format)
            .unwrap();
        bytes
    }

    #[test]
    fn opaque_images_become_jpeg_and_large_ones_shrink() {
        let photo = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2400,
            1200,
            image::Rgb([200, 30, 30]),
        ));
        for format in [ImageFormat::Png, ImageFormat::Jpeg] {
            let thumbnail = normalize(&encoded(&photo, format)).unwrap();
            assert_eq!(thumbnail.mime, "image/jpeg");
            assert_eq!((thumbnail.width, thumbnail.height), (1200, 600));
            assert!(thumbnail.bytes.starts_with(&[0xFF, 0xD8]));
        }
    }

    #[test]
    fn transparency_is_kept_as_png_and_an_opaque_alpha_channel_is_not() {
        let mut clear = image::RgbaImage::from_pixel(10, 10, image::Rgba([0, 0, 255, 255]));
        clear.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        let thumbnail =
            normalize(&encoded(&DynamicImage::ImageRgba8(clear), ImageFormat::Png)).unwrap();
        assert_eq!(thumbnail.mime, "image/png");
        let opaque = image::RgbaImage::from_pixel(10, 10, image::Rgba([0, 0, 255, 255]));
        let thumbnail = normalize(&encoded(
            &DynamicImage::ImageRgba8(opaque),
            ImageFormat::Png,
        ))
        .unwrap();
        assert_eq!(thumbnail.mime, "image/jpeg");
    }

    #[test]
    fn a_gif_contributes_a_still_frame() {
        let gif = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            4,
            4,
            image::Rgba([10, 200, 10, 255]),
        ));
        let thumbnail = normalize(&encoded(&gif, ImageFormat::Gif)).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (4, 4));
    }

    #[test]
    fn svg_text_and_oversized_images_are_refused() {
        assert_eq!(
            normalize(b"<svg xmlns='http://www.w3.org/2000/svg'/>"),
            Err(Failure::Invalid)
        );
        assert_eq!(normalize(b"not an image"), Err(Failure::Invalid));
        assert_eq!(
            normalize(&vec![0u8; THUMBNAIL_BYTES + 1]),
            Err(Failure::Quota)
        );
        // 2001 x 2000 is over 4,000,000 pixels; refused before decoding.
        let wide = DynamicImage::ImageLuma8(image::GrayImage::new(2001, 2000));
        assert_eq!(
            normalize(&encoded(&wide, ImageFormat::Png)),
            Err(Failure::Invalid)
        );
    }

    /// The reviewer's probe: a 1x1 lossless WebP whose EXIF chunk declares
    /// 65 MiB, in 52 bytes.
    fn webp_with_oversized_exif() -> Vec<u8> {
        let mut data = b"RIFF".to_vec();
        data.extend_from_slice(&50u32.to_le_bytes());
        data.extend_from_slice(b"WEBPVP8X");
        data.extend_from_slice(&10u32.to_le_bytes());
        data.extend_from_slice(&[8, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        data.extend_from_slice(b"VP8L");
        data.extend_from_slice(&5u32.to_le_bytes());
        data.extend_from_slice(&[0x2f, 0, 0, 0, 0, 0]);
        data.extend_from_slice(b"EXIF");
        data.extend_from_slice(&(65u32 * 1024 * 1024).to_le_bytes());
        data
    }

    #[test]
    fn a_webp_chunk_cannot_claim_more_than_the_input() {
        let data = webp_with_oversized_exif();
        assert_eq!(data.len(), 52);
        let (result, largest) = crate::tests::largest_allocation(|| normalize(&data));
        assert_eq!(result, Err(Failure::Invalid));
        assert!(largest < 1024 * 1024, "allocated {largest} bytes");
        // A RIFF length past the end, and a chunk past the RIFF length.
        let mut riff = data.clone();
        riff[4..8].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(webp_chunks_fit(&riff), Err(Failure::Invalid));
        let mut chunk = data[..44].to_vec();
        chunk[4..8].copy_from_slice(&36u32.to_le_bytes());
        chunk[32..36].copy_from_slice(&9u32.to_le_bytes());
        assert_eq!(webp_chunks_fit(&chunk), Err(Failure::Invalid));
    }

    #[test]
    fn a_well_formed_webp_still_decodes() {
        let photo = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            6,
            3,
            image::Rgba([50, 60, 70, 255]),
        ));
        let webp = encoded(&photo, ImageFormat::WebP);
        assert_eq!(webp_chunks_fit(&webp), Ok(()));
        let thumbnail = normalize(&webp).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (6, 3));
    }

    #[test]
    fn exif_and_other_metadata_do_not_survive() {
        let photo =
            DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3])));
        let mut jpeg = encoded(&photo, ImageFormat::Jpeg);
        // An APP1 Exif segment straight after SOI.
        let exif: &[u8] = &[
            0xFF, 0xE1, 0x00, 0x10, b'E', b'x', b'i', b'f', 0, 0, b'I', b'I', 0x2A, 0, 8, 0, 0, 0,
        ];
        jpeg.splice(2..2, exif.iter().copied());
        let thumbnail = normalize(&jpeg).unwrap();
        assert!(!thumbnail.bytes.windows(4).any(|w| w == b"Exif"));
    }
}
