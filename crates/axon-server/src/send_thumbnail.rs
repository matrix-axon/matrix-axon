//! ADR 0142 section 2's thumbnail standard, applied to an already-decoded
//! image: fit inside 800 × 600 without upscaling, turn upright, and encode as
//! JPEG at quality 78, or as PNG when a pixel is not fully opaque.
//!
//! Section 4 runs a client's thumbnail through [`normalise`] so every
//! published thumbnail meets the standard whoever made it; issue 1208's server
//! generation (section 5) reuses it for the images the server decodes itself.

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, GenericImageView, ImageError};

use axon_core::MediaThumbnail;

/// The bounding box every thumbnail fits inside (ADR 0142 section 2).
pub(crate) const MAX_WIDTH: u32 = 800;
pub(crate) const MAX_HEIGHT: u32 = 600;
/// JPEG quality, as Element X iOS and Android use.
pub(crate) const JPEG_QUALITY: u8 = 78;

/// Whether `orientation` turns the image a quarter turn, swapping its sides.
fn swaps_sides(orientation: Orientation) -> bool {
    matches!(
        orientation,
        Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH
    )
}

/// Scale `image` into the box, turn it upright and encode it.
///
/// The image is scaled first, in its own pixel format, and only the scaled
/// copy is rotated and converted to 8 bits: rotating or converting the full
/// image would allocate another full-size buffer (ADR 0142 section 5). The box
/// is swapped for a quarter turn, so the upright result fits 800 × 600.
pub(crate) fn normalise(
    image: &DynamicImage,
    orientation: Orientation,
) -> Result<MediaThumbnail, ImageError> {
    let (box_w, box_h) = if swaps_sides(orientation) {
        (MAX_HEIGHT, MAX_WIDTH)
    } else {
        (MAX_WIDTH, MAX_HEIGHT)
    };
    let (width, height) = image.dimensions();
    let mut scaled = if width <= box_w && height <= box_h {
        // Never upscale. The copy is no bigger than the box's 480,000 pixels.
        image.clone()
    } else {
        image.thumbnail(box_w, box_h)
    };
    scaled.apply_orientation(orientation);
    encode(&scaled)
}

/// Encode an upright, fitted image as section 2's JPEG, or PNG for real
/// transparency. Neither encoder writes EXIF, so no metadata leaks.
fn encode(image: &DynamicImage) -> Result<MediaThumbnail, ImageError> {
    let (width, height) = image.dimensions();
    let mut bytes = Vec::new();
    let rgba = image.color().has_alpha().then(|| image.to_rgba8());
    let mimetype = match rgba {
        Some(rgba) if rgba.pixels().any(|p| p[3] != u8::MAX) => {
            DynamicImage::ImageRgba8(rgba).write_with_encoder(PngEncoder::new(&mut bytes))?;
            "image/png"
        }
        _ => {
            DynamicImage::ImageRgb8(image.to_rgb8())
                .write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY))?;
            "image/jpeg"
        }
    };
    Ok(MediaThumbnail {
        bytes,
        mimetype: mimetype.to_owned(),
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, ImageFormat, Rgb, Rgba};

    fn decode(thumbnail: &MediaThumbnail) -> DynamicImage {
        image::load_from_memory(&thumbnail.bytes).unwrap()
    }

    #[test]
    fn a_large_opaque_image_fits_the_box_as_jpeg() {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(1600, 1600, Rgb([9, 9, 9])));
        let thumbnail = normalise(&image, Orientation::NoTransforms).unwrap();
        assert_eq!(thumbnail.mimetype, "image/jpeg");
        assert_eq!((thumbnail.width, thumbnail.height), (600, 600));
        assert_eq!(
            image::guess_format(&thumbnail.bytes).unwrap(),
            ImageFormat::Jpeg
        );
        assert_eq!(decode(&thumbnail).dimensions(), (600, 600));
    }

    #[test]
    fn a_small_image_is_not_upscaled() {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(40, 30, Rgb([1, 2, 3])));
        let thumbnail = normalise(&image, Orientation::NoTransforms).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (40, 30));
    }

    #[test]
    fn only_real_transparency_keeps_png() {
        let opaque =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(10, 10, Rgba([1, 2, 3, 255])));
        assert_eq!(
            normalise(&opaque, Orientation::NoTransforms)
                .unwrap()
                .mimetype,
            "image/jpeg",
            "an alpha channel that is fully opaque is not a reason for PNG"
        );
        let mut clear = ImageBuffer::from_pixel(10, 10, Rgba([1, 2, 3, 255]));
        clear.put_pixel(3, 3, Rgba([0, 0, 0, 0]));
        let thumbnail =
            normalise(&DynamicImage::ImageRgba8(clear), Orientation::NoTransforms).unwrap();
        assert_eq!(thumbnail.mimetype, "image/png");
        assert_eq!(
            image::guess_format(&thumbnail.bytes).unwrap(),
            ImageFormat::Png
        );
    }

    #[test]
    fn a_quarter_turn_swaps_the_box_and_the_result_is_upright() {
        // A tall sensor image stored sideways: 1200 × 1600 stored, shown
        // 1600 × 1200 after a 90° turn, so it fits 800 × 600.
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_pixel(1200, 1600, Rgb([5, 5, 5])));
        let thumbnail = normalise(&image, Orientation::Rotate90).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (800, 600));
        assert_eq!(decode(&thumbnail).dimensions(), (800, 600));
    }

    #[test]
    fn a_sixteen_bit_image_is_encoded_as_eight_bit() {
        let image = DynamicImage::ImageRgba16(ImageBuffer::from_pixel(
            2000,
            100,
            Rgba([65535, 0, 0, 65535]),
        ));
        let thumbnail = normalise(&image, Orientation::NoTransforms).unwrap();
        assert_eq!(thumbnail.mimetype, "image/jpeg");
        assert_eq!((thumbnail.width, thumbnail.height), (800, 40));
    }
}
