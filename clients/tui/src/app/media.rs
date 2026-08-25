//! Bounded background work for Matrix image events.
//!
//! Downloads, decoding, EXIF correction, resizing, and terminal-protocol
//! encoding all happen outside the TUI event/render loop.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::{io::Cursor, num::NonZeroU64};

use image::{ImageReader, Limits};
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::Resize;
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

use crate::api::AxonClient;

pub(crate) const IMAGE_CACHE_LIMIT: usize = 16;
pub(crate) const PROTOCOL_CACHE_LIMIT: usize = 32;
pub(crate) const MEDIA_WORKERS: usize = 4;
const MAX_DECODED_PIXELS: u64 = 40_000_000;
const MAX_DECODE_ALLOC_BYTES: u64 = 200 * 1024 * 1024;
const MAX_CACHED_IMAGE_DIMENSION: u32 = 1600;

/// Byte budget for a Sixel payload sent through tmux passthrough.  tmux collects
/// each passthrough DCS sequence into a single buffer and, when it would exceed
/// `INPUT_BUF_DEFAULT_SIZE` (1,048,576 bytes), sets `INPUT_DISCARD` and drops the
/// *entire* sequence — leaving an empty pane.  Sixel size depends on image entropy
/// (icy_sixel emits anywhere from ~0.3 to ~5 bytes/pixel), so a pixel-based cap
/// can't reliably bound it; we measure the encoded bytes instead and re-encode
/// smaller when over budget.  The margin below 1 MiB covers the passthrough
/// wrapper and tmux's escape doubling.
const MAX_TMUX_SIXEL_BYTES: usize = 900_000;

/// Largest number of re-encode attempts when shrinking an over-budget Sixel.  Each
/// attempt scales the cell box toward the budget, so this converges quickly; the
/// bound just guarantees termination for pathological images.
const MAX_SIXEL_SHRINK_ATTEMPTS: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MediaKey {
    pub(crate) account_id: Uuid,
    pub(crate) mxc_url: String,
}

impl MediaKey {
    pub(crate) fn new(account_id: Uuid, mxc_url: String) -> Self {
        Self {
            account_id,
            mxc_url,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProtocolKey {
    pub(crate) media: MediaKey,
    pub(crate) size: Size,
}

pub(crate) enum ImageState {
    Fetching,
    Ready(Arc<image::DynamicImage>),
    Failed(String),
}

pub(crate) enum ProtocolState {
    Encoding,
    Ready(CachedProtocol),
    Failed(String),
}

pub(crate) struct CachedProtocol {
    inline: Protocol,
    sixel_alternate: Option<Protocol>,
}

impl CachedProtocol {
    fn new(protocol: Protocol) -> Self {
        let Protocol::Sixel(sixel) = &protocol else {
            return Self {
                inline: protocol,
                sixel_alternate: None,
            };
        };

        let mut primary = sixel.clone();
        primary.data.push_str("\x1b[0m");
        let mut alternate = sixel.clone();
        alternate.data.push_str("\x1b[00m");
        Self {
            inline: Protocol::Sixel(primary),
            sixel_alternate: Some(Protocol::Sixel(alternate)),
        }
    }

    pub(crate) fn inline(&self, generation: u64) -> &Protocol {
        self.generated(generation)
    }

    pub(crate) fn preview(&self, generation: u64) -> &Protocol {
        self.generated(generation)
    }

    fn generated(&self, generation: u64) -> &Protocol {
        if generation % 2 == 0 {
            &self.inline
        } else {
            self.sixel_alternate.as_ref().unwrap_or(&self.inline)
        }
    }
}

pub(crate) enum MediaResult {
    Image {
        key: MediaKey,
        outcome: Result<Arc<image::DynamicImage>, String>,
    },
    Protocol {
        key: ProtocolKey,
        outcome: Result<CachedProtocol, String>,
    },
}

/// Encode `image` into a protocol that fits the `size` cell box, re-encoding at a
/// smaller box when the result is a tmux-passthrough Sixel over
/// [`MAX_TMUX_SIXEL_BYTES`] (which tmux would otherwise drop, see that constant).
///
/// Only tmux Sixel is shrunk: every other protocol — and Sixel outside tmux, which
/// has no passthrough buffer — is returned from the first encode at full
/// resolution.  Each attempt scales the box toward the budget by `sqrt(budget /
/// bytes)` (bytes track pixel area), so it converges in one or two passes; the
/// attempt cap guarantees termination, and a still-oversized result is returned as
/// the best effort rather than failing the preview.
fn encode_within_tmux_budget(
    picker: &Picker,
    image: &image::DynamicImage,
    size: Size,
) -> Result<Protocol, String> {
    let mut box_size = size;
    for _ in 0..=MAX_SIXEL_SHRINK_ATTEMPTS {
        let protocol = picker
            .new_protocol(image.clone(), box_size, Resize::Fit(None))
            .map_err(|err| err.to_string())?;
        let Protocol::Sixel(sixel) = &protocol else {
            return Ok(protocol);
        };
        if !sixel.is_tmux {
            return Ok(protocol);
        }
        // Can't shrink further (within budget, or already minimal): best effort.
        let Some(next) = shrink_box_for_bytes(box_size, sixel.data.len()) else {
            return Ok(protocol);
        };
        box_size = next;
    }
    // Attempts exhausted: encode once more at the smallest box and accept it.
    picker
        .new_protocol(image.clone(), box_size, Resize::Fit(None))
        .map_err(|err| err.to_string())
}

/// Next cell box to try when a Sixel encoded at `box_size` produced `encoded_len`
/// bytes.  Returns `None` when `encoded_len` is within [`MAX_TMUX_SIXEL_BYTES`] or
/// the box is already minimal (so no smaller box is worth trying).  Bytes scale
/// with pixel area, so the linear box scales by `sqrt(budget / bytes)`, with a 5%
/// undershoot to land under budget despite per-image variation.
fn shrink_box_for_bytes(box_size: Size, encoded_len: usize) -> Option<Size> {
    if encoded_len <= MAX_TMUX_SIXEL_BYTES {
        return None;
    }
    let scale = (MAX_TMUX_SIXEL_BYTES as f64 / encoded_len as f64).sqrt() * 0.95;
    let next = Size::new(
        ((f64::from(box_size.width) * scale) as u16).max(1),
        ((f64::from(box_size.height) * scale) as u16).max(1),
    );
    if next.width >= box_size.width && next.height >= box_size.height {
        return None;
    }
    Some(next)
}

pub(crate) fn spawn_image_fetch(
    client: AxonClient,
    key: MediaKey,
    is_encrypted: bool,
    workers: Arc<Semaphore>,
    tx: mpsc::Sender<MediaResult>,
) {
    tokio::spawn(async move {
        let Ok(_permit) = workers.acquire_owned().await else {
            return;
        };
        let outcome = match client.get_media(key.account_id, &key.mxc_url).await {
            Ok(bytes) => {
                let url = key.mxc_url.clone();
                tokio::task::spawn_blocking(move || decode_image(bytes, is_encrypted, &url))
                    .await
                    .unwrap_or_else(|err| Err(format!("image decode task failed: {err}")))
            }
            Err(err) => Err(err.to_string()),
        };
        let _ = tx.send(MediaResult::Image { key, outcome }).await;
    });
}

pub(crate) fn spawn_protocol_encode(
    picker: Picker,
    image: Arc<image::DynamicImage>,
    key: ProtocolKey,
    workers: Arc<Semaphore>,
    tx: mpsc::Sender<MediaResult>,
) {
    tokio::spawn(async move {
        let Ok(_permit) = workers.acquire_owned().await else {
            return;
        };
        let encode_key = key.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            encode_within_tmux_budget(&picker, &image, encode_key.size).map(CachedProtocol::new)
        })
        .await
        .unwrap_or_else(|err| Err(format!("image encode task failed: {err}")));
        let _ = tx.send(MediaResult::Protocol { key, outcome }).await;
    });
}

fn decode_image(
    bytes: Vec<u8>,
    is_encrypted: bool,
    mxc_url: &str,
) -> Result<Arc<image::DynamicImage>, String> {
    match decode_image_with_limits(&bytes) {
        Ok(image) => Ok(Arc::new(image)),
        Err(err) => {
            let format = sniff_format(&bytes);
            if is_encrypted && format.starts_with("unknown") {
                Err(format!(
                    "encrypted media - server could not decrypt ({mxc_url})"
                ))
            } else {
                Err(format!("{err} ({format}) - {mxc_url}"))
            }
        }
    }
}

fn decode_image_with_limits(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| err.to_string())?;
    let format = reader
        .format()
        .ok_or_else(|| "unsupported image format".to_owned())?;
    let (width, height) = reader.into_dimensions().map_err(|err| err.to_string())?;
    validate_image_dimensions(width, height)?;

    let mut limits = Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let image = reader.decode().map_err(|err| err.to_string())?;
    let image = apply_exif_orientation(image, bytes);
    // thumbnail() upscales small images; only downscale when the image exceeds the cap.
    Ok(
        if image.width() > MAX_CACHED_IMAGE_DIMENSION || image.height() > MAX_CACHED_IMAGE_DIMENSION
        {
            image.thumbnail(MAX_CACHED_IMAGE_DIMENSION, MAX_CACHED_IMAGE_DIMENSION)
        } else {
            image
        },
    )
}

fn validate_image_dimensions(width: u32, height: u32) -> Result<(), String> {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if NonZeroU64::new(pixels).is_none() || pixels > MAX_DECODED_PIXELS {
        return Err(format!(
            "image dimensions {width}x{height} exceed the decode limit"
        ));
    }
    Ok(())
}

pub(super) fn touch_lru<K: Clone + Eq>(order: &mut VecDeque<K>, key: &K) {
    if let Some(index) = order.iter().position(|candidate| candidate == key) {
        order.remove(index);
    }
    order.push_back(key.clone());
}

pub(super) fn evict_lru_where<K, V>(
    cache: &mut HashMap<K, V>,
    order: &mut VecDeque<K>,
    limit: usize,
    can_evict: impl Fn(&V) -> bool,
) -> bool
where
    K: Clone + Eq + std::hash::Hash,
{
    if cache.len() < limit {
        return true;
    }
    let Some(index) = order
        .iter()
        .position(|key| cache.get(key).is_some_and(&can_evict))
    else {
        return false;
    };
    let Some(oldest) = order.remove(index) else {
        return false;
    };
    cache.remove(&oldest);
    true
}

/// Apply the EXIF orientation tag to `img` so it displays upright, matching
/// what other Matrix clients show. `load_from_memory` decodes raw pixels but
/// ignores EXIF, so without this correction rotated JPEGs appear sideways.
/// Returns `img` unchanged if EXIF is absent, unreadable, or already upright.
fn apply_exif_orientation(img: image::DynamicImage, bytes: &[u8]) -> image::DynamicImage {
    use std::io::Cursor;
    let orientation = exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
        .and_then(|exif| {
            exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        })
        .unwrap_or(1);

    // EXIF orientation values 1–8; 1 = already upright.
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// Identify an image format from raw magic bytes without relying on the
/// compiled-in feature set of the `image` crate. Returns a short description
/// including a hex dump of the first bytes for truly unrecognized content.
fn sniff_format(bytes: &[u8]) -> String {
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        return "JPEG".into();
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        return "PNG".into();
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "GIF".into();
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return "WebP".into();
    }
    if bytes.starts_with(b"BM") {
        return "BMP".into();
    }
    if bytes.starts_with(b"II\x2A\x00") || bytes.starts_with(b"MM\x00\x2A") {
        return "TIFF".into();
    }
    // ISO Base Media File Format container: AVIF, HEIC, HEIF, MP4, …
    if bytes.get(4..8) == Some(b"ftyp") {
        return match bytes.get(8..12) {
            Some(b"avif") | Some(b"avis") => "AVIF".into(),
            Some(b"heic") | Some(b"heis") | Some(b"heim") | Some(b"heix") => "HEIC".into(),
            Some(b"mif1") | Some(b"msf1") => "HEIF".into(),
            _ => "ISO BMFF (AVIF/HEIC/MP4/…)".into(),
        };
    }
    if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") || bytes.starts_with(b"<SVG") {
        return "SVG (not supported)".into();
    }
    if bytes.starts_with(b"\x00\x00\x01\x00") {
        return "ICO".into();
    }
    // Not a recognized image format — could be a JSON/HTML error body served
    // with a 2xx status. Show the first bytes so the cause is obvious.
    let prefix: String = bytes
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let printable: String = bytes
        .iter()
        .take(16)
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("unknown — first bytes: {prefix}  ({printable})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui_image::protocol::sixel::Sixel;

    fn sixel_data(protocol: &Protocol) -> &str {
        let Protocol::Sixel(sixel) = protocol else {
            panic!("expected sixel protocol");
        };
        &sixel.data
    }

    #[test]
    fn rejects_images_with_excessive_decoded_dimensions() {
        assert!(validate_image_dimensions(6000, 6000).is_ok()); // 36 MP — under new limit
        assert!(validate_image_dimensions(7000, 6000).is_err()); // 42 MP — over limit
        assert!(validate_image_dimensions(0, 100).is_err());
    }

    #[test]
    fn within_budget_sixel_is_not_shrunk() {
        assert_eq!(
            shrink_box_for_bytes(Size::new(120, 36), MAX_TMUX_SIXEL_BYTES),
            None
        );
        assert_eq!(shrink_box_for_bytes(Size::new(120, 36), 1_000), None);
    }

    #[test]
    fn over_budget_sixel_shrinks_toward_budget_keeping_aspect() {
        // ~4x over budget should roughly halve each dimension (sqrt(1/4)).
        let box_size = Size::new(120, 36);
        let next = shrink_box_for_bytes(box_size, MAX_TMUX_SIXEL_BYTES * 4)
            .expect("over-budget box must shrink");
        assert!(next.width < box_size.width && next.height < box_size.height);
        // Aspect ratio (10:3) preserved within rounding.
        let ratio = f64::from(next.width) / f64::from(next.height);
        assert!(
            (ratio - 120.0 / 36.0).abs() < 0.25,
            "ratio drifted: {ratio}"
        );
        // One shrink step lands within ~sqrt of the overage; a second pass (the
        // encode loop) closes the rest.  Here we only assert it moved meaningfully.
        assert!(f64::from(next.width) < f64::from(box_size.width) * 0.6);
    }

    #[test]
    fn shrink_converges_and_never_returns_zero() {
        // Repeatedly applying the step to a wildly over-budget box terminates and
        // never produces a zero dimension.
        let mut size = Size::new(200, 60);
        for _ in 0..MAX_SIXEL_SHRINK_ATTEMPTS {
            match shrink_box_for_bytes(size, MAX_TMUX_SIXEL_BYTES * 50) {
                Some(next) => {
                    assert!(next.width >= 1 && next.height >= 1);
                    assert!(next.width <= size.width && next.height <= size.height);
                    size = next;
                }
                None => break,
            }
        }
    }

    #[test]
    fn sixel_inline_payload_alternates_by_generation() {
        let protocol = CachedProtocol::new(Protocol::Sixel(Sixel {
            data: "sixel-data".to_owned(),
            size: Size::new(1, 1),
            is_tmux: false,
        }));

        assert_eq!(
            sixel_data(protocol.inline(0)),
            sixel_data(protocol.inline(2))
        );
        assert_ne!(
            sixel_data(protocol.inline(0)),
            sixel_data(protocol.inline(1))
        );
        assert_eq!(
            sixel_data(protocol.inline(1)),
            sixel_data(protocol.preview(1))
        );
    }
}
