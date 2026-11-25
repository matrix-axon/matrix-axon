//! Best-effort image inspection for staged sends (ADR 0142, section 5).

use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axon_core::{MediaThumbnail, SendImageMetadata};
use axon_store::{MediaUploadKind, MediaUploadState, Store};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use tokio::sync::Semaphore;
use tokio::time::{timeout_at, Instant};
use uuid::Uuid;

use crate::send_thumbnail::{normalise, swaps_sides, MAX_HEIGHT, MAX_WIDTH};

const BUDGET: Duration = Duration::from_secs(5);
const DECODE_PERMITS: usize = 1;
const MIB: u64 = 1024 * 1024;
const HEADER_BYTES: u64 = MIB;
const JPEG_LONG_HEADER_BYTES: u64 = 4 * MIB;
const MAX_JPEG_BYTES: u64 = 50 * MIB;
const MAX_DECODE_COST: u64 = 192 * MIB;
const MAX_ALLOC: u64 = 256 * MIB;
const MAX_SIDE: u32 = 16_384;
const MAX_THUMBNAIL_BYTES: u64 = axon_core::link_preview::MAX_IMAGE_BYTES;
const SMALL_ORIGINAL_BYTES: u64 = 32 * 1024;
const MIN_SAVINGS_BYTES: u64 = 64 * 1024;

type Inspection<T> = Result<T, &'static str>;

pub(crate) struct ImageSendService {
    store: Store,
    enabled: bool,
    probes: Arc<Semaphore>,
    decodes: Arc<Semaphore>,
}

impl ImageSendService {
    pub(crate) fn new(store: Store, enabled: bool) -> Self {
        Self {
            store,
            enabled,
            probes: Arc::new(Semaphore::new(4)),
            decodes: Arc::new(Semaphore::new(DECODE_PERMITS)),
        }
    }
}

#[async_trait::async_trait]
impl axon_api::SendThumbnailer for ImageSendService {
    async fn inspect(
        &self,
        account_id: Uuid,
        room_id: &str,
        upload_id: Uuid,
        generate: bool,
    ) -> SendImageMetadata {
        let deadline = Instant::now() + BUDGET;
        let mut metadata = SendImageMetadata::default();
        let result = self
            .inspect_claim(
                account_id,
                upload_id,
                generate && self.enabled,
                deadline,
                &mut metadata,
            )
            .await;
        if let Err(error_kind) = result {
            tracing::debug!(%account_id, %room_id, %upload_id, error_kind, "staged image inspection incomplete");
        }
        metadata
    }
}

impl ImageSendService {
    async fn inspect_claim(
        &self,
        account_id: Uuid,
        upload_id: Uuid,
        generate: bool,
        deadline: Instant,
        metadata: &mut SendImageMetadata,
    ) -> Inspection<()> {
        let row = timeout_at(deadline, self.store.get_media_upload(account_id, upload_id))
            .await
            .map_err(|_| "deadline")?
            .map_err(|_| "staged_lookup")?
            .filter(|row| {
                row.state == MediaUploadState::Sending && row.kind == MediaUploadKind::Image
            })
            .ok_or("staged_original_missing")?;
        let size = u64::try_from(row.size_bytes).map_err(|_| "staged_original_size")?;
        let path = PathBuf::from(row.path);
        // A separate, small header pool keeps dimensions available when the
        // full decoder is busy. Each worker retains its permit if abandoned.
        let file = path.clone();
        let (format, header) = blocking_before(&self.probes, deadline, move || {
            probe(open_original(&file, size)?)
        })
        .await?;
        let header = match header {
            Some(header) => header,
            // Retry long JPEG headers under the single decode permit.
            None => {
                let file = path.clone();
                blocking_before(&self.decodes, deadline, move || {
                    jpeg_header(&mut open_original(&file, size)?, JPEG_LONG_HEADER_BYTES)
                })
                .await?
            }
        };
        metadata.dimensions = Some(header.dimensions());
        if generate && header.needs_thumbnail(format, size) {
            metadata.thumbnail = blocking_before(&self.decodes, deadline, move || {
                generate_thumbnail(open_original(&path, size)?, format, header, size, deadline)
            })
            .await?;
        }
        Ok(())
    }
}

async fn blocking_before<T: Send + 'static>(
    permits: &Arc<Semaphore>,
    deadline: Instant,
    work: impl FnOnce() -> Inspection<T> + Send + 'static,
) -> Inspection<T> {
    let permit = timeout_at(deadline, permits.clone().acquire_owned())
        .await
        .map_err(|_| "decoder_capacity")?
        .map_err(|_| "decoder_closed")?;
    let mut task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if Instant::now() >= deadline {
            return Err("deadline");
        }
        work()
    });
    match timeout_at(deadline, &mut task).await {
        Ok(result) if Instant::now() < deadline => result.map_err(|_| "decoder_task")?,
        _ => {
            // Abort queued work. A running decoder retains its permit until
            // it returns, even though this send can continue.
            task.abort();
            Err("deadline")
        }
    }
}

fn open_original(path: &Path, size: u64) -> Inspection<BufReader<File>> {
    let file = File::open(path).map_err(|_| "staged_original_open")?;
    let metadata = file.metadata().map_err(|_| "staged_original_size")?;
    if !metadata.is_file() || metadata.len() != size {
        return Err("staged_original_size");
    }
    Ok(BufReader::new(file))
}

#[derive(Clone, Copy)]
struct Header {
    width: u32,
    height: u32,
    orientation: Orientation,
    progressive: bool,
}

impl Header {
    fn dimensions(self) -> (u32, u32) {
        if swaps_sides(self.orientation) {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    fn needs_thumbnail(self, format: ImageFormat, size: u64) -> bool {
        // WebP gets a compatibility thumbnail even if small.
        let (width, height) = self.dimensions();
        format == ImageFormat::WebP
            || (size > SMALL_ORIGINAL_BYTES && (width > MAX_WIDTH || height > MAX_HEIGHT))
    }

    fn pixels(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    fn padded_pixels(self, block: u32) -> u64 {
        u64::from(self.width.div_ceil(block) * block)
            * u64::from(self.height.div_ceil(block) * block)
    }
}

fn probe(mut file: BufReader<File>) -> Inspection<(ImageFormat, Option<Header>)> {
    let format = image::guess_format(file.fill_buf().map_err(|_| "header_read")?)
        .map_err(|_| "unsupported_image")?;
    let header = match format {
        ImageFormat::Jpeg => match jpeg_header(&mut file, HEADER_BYTES) {
            Ok(header) => Some(header),
            Err("header_limit") => None,
            Err(error) => return Err(error),
        },
        ImageFormat::Png | ImageFormat::Gif => {
            let mut prefix = Vec::new();
            file.take(HEADER_BYTES)
                .read_to_end(&mut prefix)
                .map_err(|_| "header_read")?;
            let mut decoder = reader(Cursor::new(prefix), format)
                .into_decoder()
                .map_err(|_| "invalid_image_header")?;
            Some(decoder_header(&mut decoder, false))
        }
        ImageFormat::WebP => {
            let mut decoder =
                image_webp::WebPDecoder::new(file).map_err(|_| "invalid_image_header")?;
            decoder.set_memory_limit(HEADER_BYTES as usize);
            let (width, height) = decoder.dimensions();
            let orientation = decoder
                .exif_metadata()
                .ok()
                .flatten()
                .as_deref()
                .and_then(Orientation::from_exif_chunk)
                .unwrap_or(Orientation::NoTransforms);
            Some(Header {
                width,
                height,
                orientation,
                progressive: false,
            })
        }
        _ => return Err("unsupported_image"),
    };
    if header.is_some_and(|header| header.width == 0 || header.height == 0) {
        return Err("invalid_image_header");
    }
    Ok((format, header))
}

fn reader<R: BufRead + Seek>(input: R, format: ImageFormat) -> ImageReader<R> {
    let mut reader = ImageReader::with_format(input, format);
    let mut limits = Limits::default();
    // In particular, constrain PNG ancillary chunks during construction.
    // Full pixel buffers get a separate check before decoding.
    limits.max_alloc = Some(HEADER_BYTES);
    reader.limits(limits);
    reader
}

fn decoder_header(decoder: &mut impl ImageDecoder, progressive: bool) -> Header {
    let (width, height) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    Header {
        width,
        height,
        orientation,
        progressive,
    }
}

fn jpeg_header(file: &mut BufReader<File>, limit: u64) -> Inspection<Header> {
    let (progressive, header_bytes) = jpeg_layout(file, limit)?;
    file.rewind().map_err(|_| "header_read")?;
    let mut prefix = Vec::new();
    file.take(header_bytes)
        .read_to_end(&mut prefix)
        .map_err(|_| "header_read")?;
    let mut decoder = reader(Cursor::new(prefix), ImageFormat::Jpeg)
        .into_decoder()
        .map_err(|_| "invalid_image_header")?;
    let header = decoder_header(&mut decoder, progressive);
    file.rewind().map_err(|_| "header_read")?;
    Ok(header)
}

/// JPEG SOF0/SOF1 are sequential, SOF2 is progressive. The image codec does
/// not expose the coding mode. Walk only pre-scan segment headers, seeking
/// over their payloads, and require the first scan header to fit the probe.
fn jpeg_layout(file: &mut (impl Read + Seek), limit: u64) -> Inspection<(bool, u64)> {
    let mut marker = [0; 2];
    file.read_exact(&mut marker)
        .map_err(|_| "invalid_image_header")?;
    if marker != [0xff, 0xd8] {
        return Err("invalid_image_header");
    }
    let mut progressive = None;
    let mut position = 2;
    loop {
        if position + 4 > limit {
            return Err("header_limit");
        }
        file.read_exact(&mut marker)
            .map_err(|_| "invalid_image_header")?;
        if marker[0] != 0xff {
            return Err("invalid_image_header");
        }
        position += 2;
        while marker[1] == 0xff {
            if position + 3 > limit {
                return Err("header_limit");
            }
            file.read_exact(&mut marker[1..])
                .map_err(|_| "invalid_image_header")?;
            position += 1;
        }
        if matches!(marker[1], 0x00 | 0xd0..=0xd9) {
            return Err("invalid_image_header");
        }
        let mut length = [0; 2];
        file.read_exact(&mut length)
            .map_err(|_| "invalid_image_header")?;
        let length = u64::from(u16::from_be_bytes(length));
        if length < 2 {
            return Err("invalid_image_header");
        }
        position += length;
        if position > limit {
            return Err("header_limit");
        }
        match marker[1] {
            0xc0..=0xc2 if progressive.is_none() => progressive = Some(marker[1] == 0xc2),
            0xc0..=0xc2 => return Err("invalid_image_header"),
            0xda => {
                return progressive
                    .map(|mode| (mode, position))
                    .ok_or("invalid_image_header")
            }
            _ => {}
        }
        file.seek(SeekFrom::Start(position))
            .map_err(|_| "header_read")?;
    }
}

fn generate_thumbnail(
    file: BufReader<File>,
    format: ImageFormat,
    header: Header,
    size: u64,
    deadline: Instant,
) -> Inspection<Option<MediaThumbnail>> {
    let image = decode_original(file, format, header, size)?;
    if Instant::now() >= deadline {
        return Err("deadline");
    }
    let thumbnail = normalise(&image, header.orientation).map_err(|_| "encode")?;
    let length = thumbnail.bytes.len() as u64;
    if length > MAX_THUMBNAIL_BYTES {
        return Err("thumbnail_size");
    }
    let savings = size.saturating_sub(length);
    if format != ImageFormat::WebP && savings <= MIN_SAVINGS_BYTES && savings <= size / 10 {
        return Ok(None);
    }
    Ok(Some(thumbnail))
}

fn decode_original(
    file: BufReader<File>,
    format: ImageFormat,
    header: Header,
    size: u64,
) -> Inspection<DynamicImage> {
    if header.width == 0
        || header.height == 0
        || header.width > MAX_SIDE
        || header.height > MAX_SIDE
    {
        return Err("image_dimensions");
    }
    if format == ImageFormat::Jpeg && size > MAX_JPEG_BYTES {
        return Err("compressed_size");
    }
    if format == ImageFormat::WebP {
        decode_webp(file, header, size)
    } else {
        let mut decoder = reader(file, format).into_decoder().map_err(|_| "decode")?;
        let decoded = decoder.total_bytes();
        let scratch = match format {
            // Round up for the largest MCU and reserve coefficients for up
            // to four components, including CMYK.
            ImageFormat::Jpeg if header.progressive => header.padded_pixels(32) * 8,
            ImageFormat::Gif => header.pixels() * 4,
            _ => 0,
        };
        // The GIF decoder retains ICC and XMP application extensions while
        // reading the first frame. Account for the input they can retain.
        let metadata = if format == ImageFormat::Gif {
            size.saturating_mul(2)
        } else {
            0
        };
        check_cost(decoded, scratch.saturating_add(metadata))?;
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_SIDE);
        limits.max_image_height = Some(MAX_SIDE);
        // GIF frames can be larger than their logical screen. Reserve at
        // most one screen-sized scratch buffer even on malformed input.
        limits.max_alloc = Some(if format == ImageFormat::Gif {
            decoded + scratch
        } else {
            MAX_ALLOC
        });
        limits.reserve(decoded).map_err(|_| "decode_cost")?;
        decoder.set_limits(limits).map_err(|_| "decode_cost")?;
        DynamicImage::from_decoder(decoder).map_err(|_| "decode")
    }
}

fn check_cost(decoded: u64, scratch: u64) -> Inspection<()> {
    if decoded.saturating_add(scratch) > MAX_DECODE_COST {
        return Err("decode_cost");
    }
    Ok(())
}

fn decode_webp(mut file: BufReader<File>, header: Header, size: u64) -> Inspection<DynamicImage> {
    // Extended WebP has a canvas and an embedded picture. Check the first
    // picture's dimensions before handing it to the codec, which otherwise
    // checks VP8 dimensions after allocating the planes.
    check_webp_picture(&mut file, header, size).map_err(|_| "invalid_image_header")?;
    file.rewind().map_err(|_| "header_read")?;
    let mut decoder = image_webp::WebPDecoder::new(file).map_err(|_| "decode")?;
    decoder.set_memory_limit(MAX_ALLOC as usize);
    let alpha = decoder.has_alpha();
    let pixels = header.pixels();
    let decoded = pixels * if alpha { 4 } else { 3 };
    let mut scratch = pixels * 4;
    if decoder.is_animated() {
        scratch += pixels * 4; // Compositing canvas in addition to the frame.
    }
    if decoder.is_lossy() {
        // Padded YUV planes, the alpha extraction buffer, and the VP8
        // partitions. The final partition is copied once. A malformed
        // partition length can allocate up to 2^24 bytes before EOF.
        scratch += header.padded_pixels(16) * 3 / 2 + if alpha { pixels } else { 0 };
        scratch = scratch.saturating_add(size.saturating_mul(2).saturating_add(16 * MIB));
    }
    check_cost(decoded, scratch)?;
    let mut bytes = vec![0; decoded as usize];
    decoder.read_image(&mut bytes).map_err(|_| "decode")?;
    if alpha {
        image::RgbaImage::from_raw(header.width, header.height, bytes)
            .map(DynamicImage::ImageRgba8)
            .ok_or("decode")
    } else {
        image::RgbImage::from_raw(header.width, header.height, bytes)
            .map(DynamicImage::ImageRgb8)
            .ok_or("decode")
    }
}

/// Read the canvas's first picture header, descending into its first ANMF
/// when animated. Check both the chunk bounds and the VP8/VP8L dimensions.
fn check_webp_picture(
    file: &mut BufReader<File>,
    header: Header,
    size: u64,
) -> std::io::Result<()> {
    let invalid = || std::io::Error::from(std::io::ErrorKind::InvalidData);
    file.seek(SeekFrom::Start(12))?;
    let mut end = size;
    let mut dimensions = (header.width, header.height);
    let mut animated = false;
    let mut in_frame = false;
    let mut after_alpha = false;
    while file.stream_position()? + 8 <= end {
        let mut chunk = [0; 8];
        file.read_exact(&mut chunk)?;
        let length = u64::from(u32::from_le_bytes(chunk[4..].try_into().unwrap()));
        let start = file.stream_position()?;
        if start + length > end {
            return Err(invalid());
        }
        if after_alpha && &chunk[..4] != b"VP8 " {
            return Err(invalid());
        }
        match &chunk[..4] {
            b"VP8X" if start == 20 && length == 10 => {
                let mut flags = [0];
                file.read_exact(&mut flags)?;
                animated = flags[0] & 0x02 != 0;
                file.seek(SeekFrom::Start(start + length))?;
            }
            b"ANMF" if animated && !in_frame && length >= 16 => {
                let mut frame = [0; 16];
                file.read_exact(&mut frame)?;
                let field = |index: usize| {
                    u32::from_le_bytes([frame[index], frame[index + 1], frame[index + 2], 0])
                };
                dimensions = (field(6) + 1, field(9) + 1);
                if field(0) * 2 + dimensions.0 > header.width
                    || field(3) * 2 + dimensions.1 > header.height
                {
                    return Err(invalid());
                }
                in_frame = true;
                end = start + length;
            }
            b"ALPH" if in_frame => {
                after_alpha = true;
                file.seek(SeekFrom::Start(start + length.next_multiple_of(2)))?;
            }
            b"VP8 " if length >= 10 && (!animated || in_frame) => {
                let mut picture = [0; 10];
                file.read_exact(&mut picture)?;
                let width = u16::from_le_bytes([picture[6], picture[7]]);
                let height = u16::from_le_bytes([picture[8], picture[9]]);
                if (u32::from(width & 0x3fff), u32::from(height & 0x3fff)) != dimensions {
                    return Err(invalid());
                }
                return Ok(());
            }
            b"VP8L" if length >= 5 && (!animated || in_frame) => {
                let mut picture = [0; 5];
                file.read_exact(&mut picture)?;
                let bits = u32::from_le_bytes(picture[1..].try_into().unwrap());
                if ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1) != dimensions {
                    return Err(invalid());
                }
                return Ok(());
            }
            _ => {
                file.seek(SeekFrom::Start(start + length.next_multiple_of(2)))?;
            }
        }
    }
    Err(invalid())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use image::{ImageBuffer, ImageEncoder, Rgb, Rgba};

    use super::*;

    fn fixture(bytes: &[u8]) -> BufReader<File> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        BufReader::new(file)
    }

    fn decode_fixture(bytes: &[u8], format: ImageFormat) -> Inspection<Option<MediaThumbnail>> {
        let (detected, header) = probe(fixture(bytes))?;
        assert_eq!(detected, format);
        let header = header.unwrap();
        let size = bytes.len() as u64;
        if !header.needs_thumbnail(format, size) {
            return Ok(None);
        }
        generate_thumbnail(
            fixture(bytes),
            format,
            header,
            size,
            Instant::now() + BUDGET,
        )
    }

    #[test]
    fn jpeg_orientation_changes_event_dimensions_and_the_thumbnail_pixels() {
        let image = DynamicImage::ImageRgb8(ImageBuffer::from_fn(900, 1200, |_, y| {
            if y < 600 {
                Rgb([240, 0, 0])
            } else {
                Rgb([0, 0, 240])
            }
        }));
        for (orientation, left_channel, right_channel) in [(6, 2, 0), (8, 0, 2)] {
            let mut bytes = Vec::new();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut bytes);
            // Little-endian TIFF with one orientation entry.
            encoder
                .set_exif_metadata(vec![
                    b'I',
                    b'I',
                    42,
                    0,
                    8,
                    0,
                    0,
                    0,
                    1,
                    0,
                    0x12,
                    1,
                    3,
                    0,
                    1,
                    0,
                    0,
                    0,
                    orientation,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                ])
                .unwrap();
            encoder.encode_image(&image).unwrap();
            // Leave room for the savings rule without making fixture encoding expensive.
            bytes.resize(128 * 1024, 0);
            let (_, header) = probe(fixture(&bytes)).unwrap();
            assert_eq!(header.unwrap().dimensions(), (1200, 900));
            let thumb = decode_fixture(&bytes, ImageFormat::Jpeg).unwrap().unwrap();
            assert_eq!((thumb.width, thumb.height), (800, 600));
            let pixels = image::load_from_memory(&thumb.bytes).unwrap().into_rgb8();
            assert!(pixels.get_pixel(50, 300)[left_channel] > 200);
            assert!(pixels.get_pixel(750, 300)[right_channel] > 200);
        }
    }

    #[test]
    fn a_long_jpeg_header_uses_the_second_probe() {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&DynamicImage::new_rgb8(80, 60))
            .unwrap();
        let mut bytes = jpeg[..2].to_vec();
        for _ in 0..17 {
            bytes.extend_from_slice(&[0xff, 0xfe, 0xff, 0xff]);
            bytes.resize(bytes.len() + 65533, 0);
        }
        bytes.extend_from_slice(&jpeg[2..]);
        assert!(probe(fixture(&bytes)).unwrap().1.is_none());
        assert_eq!(
            jpeg_header(&mut fixture(&bytes), JPEG_LONG_HEADER_BYTES)
                .unwrap()
                .dimensions(),
            (80, 60),
        );
    }

    #[test]
    fn small_images_have_dimensions_without_a_thumbnail_and_corrupt_headers_fall_back() {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(80, 60)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let mut png = png.into_inner();
        png.resize(100 * 1024, 0);
        let (format, header) = probe(fixture(&png)).unwrap();
        assert_eq!(format, ImageFormat::Png);
        assert_eq!(header.unwrap().dimensions(), (80, 60));
        assert!(decode_fixture(&png, ImageFormat::Png).unwrap().is_none());
        assert!(probe(fixture(b"\xff\xd8\xff\xe1broken")).is_err());
    }

    #[test]
    fn a_fifty_megapixel_rgba16_png_is_rejected_before_full_decode() {
        // A 10,000 × 5,000 RGBA16 IHDR with valid CRC and an empty IDAT.
        // The test must reach the cost check before the decoder sees no pixels.
        let mut bytes = b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x27\x10\x00\x00\x13\x88\
                          \x10\x06\x00\x00\x00\x47\xc7\xab\x0b\x00\x00\x00\x00\x49\x44\x41\x54\x35\xaf\x06\x1e".to_vec();
        bytes.resize(128 * 1024, 0);
        let (_, header) = probe(fixture(&bytes)).unwrap();
        assert_eq!(header.unwrap().dimensions(), (10_000, 5_000));
        assert_eq!(decode_fixture(&bytes, ImageFormat::Png), Err("decode_cost"));
    }

    #[test]
    fn webp_with_alpha_gets_a_png_even_when_small() {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(ImageBuffer::from_pixel(80, 60, Rgba([1, 20, 200, 100])))
            .write_to(&mut bytes, ImageFormat::WebP)
            .unwrap();
        let thumb = decode_fixture(bytes.get_ref(), ImageFormat::WebP)
            .unwrap()
            .unwrap();
        assert_eq!(thumb.mimetype, "image/png");
        assert_eq!((thumb.width, thumb.height), (80, 60));
    }

    #[test]
    fn animated_gif_uses_the_first_frame() {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            let frames = [[255, 0, 0, 255], [0, 0, 255, 255]]
                .map(|pixel| image::Frame::new(ImageBuffer::from_pixel(1000, 750, Rgba(pixel))));
            encoder.encode_frames(frames).unwrap();
        }
        bytes.resize(128 * 1024, 0);
        let thumb = decode_fixture(&bytes, ImageFormat::Gif).unwrap().unwrap();
        let image = image::load_from_memory(&thumb.bytes).unwrap().into_rgb8();
        assert_eq!(image.dimensions(), (800, 600));
        assert!(image.get_pixel(400, 300)[0] > 200);
        assert!(image.get_pixel(400, 300)[2] < 50);
    }

    #[tokio::test]
    async fn expired_decode_does_not_release_a_running_workers_permit() {
        let permits = Arc::new(Semaphore::new(1));
        let (entered, entry) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let worker = blocking_before(
            &permits,
            Instant::now() + Duration::from_millis(100),
            move || {
                entered.send(()).unwrap();
                wait.recv().unwrap();
                Ok(())
            },
        );
        let (result, ()) = tokio::join!(worker, async { entry.await.unwrap() });
        assert_eq!(result, Err("deadline"));
        assert_eq!(
            blocking_before(&permits, Instant::now() + Duration::from_millis(20), || {
                panic!("another decoder must not enter");
            })
            .await,
            Err::<(), _>("decoder_capacity"),
        );
        release.send(()).unwrap();
        assert!(
            blocking_before(&permits, Instant::now() + BUDGET, || Ok(()))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn busy_send_decoder_preserves_dimensions_and_respects_the_budget() {
        let url = std::env::var("DATABASE_URL").unwrap();
        let store = Store::connect(&url, 5).await.unwrap();
        let owner = store
            .upsert_account(
                &format!("@image-send-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .unwrap()
            .account_id;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.webp");
        DynamicImage::new_rgba8(80, 60).save(&path).unwrap();
        let upload_id = Uuid::new_v4();
        store
            .insert_media_upload(&axon_store::NewMediaUpload {
                upload_id,
                account_id: owner,
                kind: MediaUploadKind::Image,
                filename: "image.webp",
                content_type: Some("image/webp"),
                size_bytes: std::fs::metadata(&path).unwrap().len() as i64,
                path: path.to_str().unwrap(),
                expires_at: chrono::Utc::now() + chrono::TimeDelta::minutes(5),
            })
            .await
            .unwrap();
        store
            .claim_staged_media_upload(owner, upload_id)
            .await
            .unwrap()
            .unwrap();
        let service = ImageSendService::new(store, true);
        let _held = service.decodes.try_acquire().unwrap();

        let mut metadata = SendImageMetadata::default();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            service.inspect_claim(
                owner,
                upload_id,
                true,
                Instant::now() + Duration::from_millis(250),
                &mut metadata,
            ),
        )
        .await
        .expect("inspection does not wait for the occupied permit");
        assert_eq!(result, Err("decoder_capacity"));
        assert_eq!(metadata.dimensions, Some((80, 60)));
        assert!(metadata.thumbnail.is_none());
        assert_eq!(
            service
                .inspect_claim(
                    owner,
                    upload_id,
                    false,
                    Instant::now() + BUDGET,
                    &mut metadata,
                )
                .await,
            Ok(())
        );
        assert_eq!(metadata.dimensions, Some((80, 60)));
        assert!(metadata.thumbnail.is_none());
    }

    /// Run this test in its own process under a peak-RSS sampler. The fixtures
    /// are prepared separately so raster creation does not inflate the result.
    /// Hold each preview immediately after its real decode to force overlap
    /// with the send decoder, even when the individual fixtures decode fast.
    #[test]
    #[ignore = "requires large fixtures and a dedicated process for peak RSS"]
    fn image_decode_peak() {
        use std::sync::Barrier;

        use crate::uploads::decode_preview_image;

        let fixture_dir =
            std::path::PathBuf::from(std::env::var_os("AXON_IMAGE_PEAK_FIXTURES").unwrap());
        let preview = std::fs::read(fixture_dir.join("preview.png")).unwrap();
        let original = fixture_dir.join("original.jpg");
        let original_file = File::open(&original).unwrap();
        let original_size = original_file.metadata().unwrap().len();
        let (format, header) = probe(BufReader::new(original_file)).unwrap();

        let staging = axon_core::MediaConfig::default().max_concurrent_uploads;
        let generation = crate::preview_generation::PREVIEW_DECODE_PERMITS;
        let ready = Barrier::new(staging + generation + 1);
        let finished = Barrier::new(staging + generation + 1);
        let staging_permits = Semaphore::new(staging);
        let generation_permits = Semaphore::new(generation);
        let send_permits = Semaphore::new(DECODE_PERMITS);
        std::thread::scope(|scope| {
            for i in 0..staging + generation {
                let (ready, finished, preview) = (&ready, &finished, &preview);
                let permits = if i < staging {
                    &staging_permits
                } else {
                    &generation_permits
                };
                scope.spawn(move || {
                    let _permit = permits.try_acquire().unwrap();
                    let decoded = decode_preview_image(axon_api::ClaimedUpload {
                        upload_id: Uuid::nil(),
                        kind: axon_api::MediaUploadKindDto::Image,
                        filename: "preview.png".into(),
                        content_type: Some("image/png".into()),
                        size_bytes: preview.len() as u64,
                        bytes: preview.clone(),
                    });
                    let rendered = if i < staging {
                        decoded
                            .as_ref()
                            .ok()
                            .map(|decoded| normalise(&decoded.image, decoded.orientation).unwrap())
                    } else {
                        None
                    };
                    ready.wait();
                    finished.wait();
                    std::hint::black_box((decoded.unwrap(), rendered));
                });
            }
            ready.wait();
            let _permit = send_permits.try_acquire().unwrap();
            let decoded = decode_original(
                BufReader::new(File::open(original).unwrap()),
                format,
                header.unwrap(),
                original_size,
            );
            finished.wait();
            std::hint::black_box(decoded.unwrap());
        });
    }
}
