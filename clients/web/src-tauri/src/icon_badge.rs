//! The picture Windows paints on a taskbar button, and the name Narrator reads.
//!
//! `Window::set_badge_count` is a no-op on Windows. The taskbar takes an
//! overlay image instead, and this builds that image: a red disc with the
//! count in white. Other platforms badge with a number and never call this.
//! The disc is drawn here, rather than shipped as an asset, because the
//! count is part of the picture.
//!
//! The picture stops at 99, because a third digit does not fit. The name says
//! the real count. A screen reader has room for it.

/// A count the icon should show. Zero and negative clear, matching the page,
/// which clears when the unread total is zero.
///
/// This is the only place that decides a clear. The Windows picture takes a
/// count that has already passed through here.
pub fn visible_count(count: Option<i64>) -> Option<i64> {
    count.filter(|count| *count > 0)
}

/// The name Narrator reads for a count the overlay is showing.
///
/// English, like the rest of the shell. Callers pass a positive count.
/// Zero clears the overlay and has no name.
#[cfg(any(windows, test))]
pub fn accessible_name(count: i64) -> String {
    if count == 1 {
        "1 unread message".to_string()
    } else {
        format!("{count} unread messages")
    }
}

/// The Windows overlay. Compiled for tests on every host, so a non-Windows
/// clippy lane still type-checks the drawing. One attribute covers the whole
/// module: a helper added inside it cannot become a dead-code error on Linux.
#[cfg(any(windows, test))]
mod drawing {
    /// Edge length of the overlay. Windows scales a taskbar overlay down from this.
    pub const OVERLAY_PX: u32 = 32;

    const DISC_RED: [u8; 4] = [196, 30, 45, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    /// Radius, in pixels. 15² is 225, so the corners stay transparent.
    const DISC_RADIUS_SQ: i32 = 15 * 15;

    /// RGBA overlay for a count the caller has already decided to show.
    ///
    /// Counts above 99 draw as 99: a taskbar overlay has no room for a third
    /// digit. Clearing is not this function's decision.
    pub fn overlay(count: i64) -> Vec<u8> {
        let mut rgba = vec![0u8; (OVERLAY_PX * OVERLAY_PX * 4) as usize];
        fill_disc(&mut rgba);
        paint_count(&mut rgba, count.min(99));
        rgba
    }

    /// BGRA xor bits and the monochrome AND mask `CreateIcon` reads.
    ///
    /// The color bytes swap red and blue. Alpha stays, and it is what makes
    /// the disc transparent. The mask is one bit per pixel, high bit on the
    /// left. `CreateIcon` reads it as a monochrome bitmap, so each row is
    /// padded to a 16-bit boundary. A set bit is transparent, and only a
    /// fully transparent pixel is set. A partial alpha stays opaque in the
    /// mask: a transparent bit on a pixel that still has color paints black
    /// where the mask is honored. `overlay` draws only alpha 0 and 255, so
    /// its mask is the same silhouette as that alpha.
    ///
    /// `None` means a side is zero, or `rgba` is not four bytes per pixel.
    pub fn icon_bits(rgba: &[u8], width: u32, height: u32) -> Option<(Vec<u8>, Vec<u8>)> {
        let width = usize::try_from(width).ok()?;
        let height = usize::try_from(height).ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        let bytes = width.checked_mul(height)?.checked_mul(4)?;
        if rgba.len() != bytes {
            return None;
        }
        let mut bgra = rgba.to_vec();
        for pixel in bgra.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Some((bgra, and_mask(rgba, width, height)?))
    }

    fn and_mask(rgba: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
        let stride = mask_stride(width)?;
        let mut mask = vec![0u8; stride.checked_mul(height)?];
        for y in 0..height {
            let row = y * width * 4;
            for x in 0..width {
                if rgba[row + x * 4 + 3] != 0 {
                    continue;
                }
                mask[y * stride + x / 8] |= 1 << (7 - (x % 8));
            }
        }
        Some(mask)
    }

    /// Bytes in one AND-mask row. Monochrome rows are WORD-aligned.
    fn mask_stride(width: usize) -> Option<usize> {
        let padded = width.checked_add(15)?;
        Some(padded / 16 * 2)
    }

    fn fill_disc(rgba: &mut [u8]) {
        // Pixel centers sit on the half-pixel. Comparing those to the middle
        // of the image (16) keeps the disc on the same center as the glyphs.
        // An integer center of 15 leaves the disc half a pixel up and left,
        // and leaves the last row and column empty.
        for y in 0..OVERLAY_PX as i32 {
            for x in 0..OVERLAY_PX as i32 {
                let dx = x * 2 + 1 - OVERLAY_PX as i32;
                let dy = y * 2 + 1 - OVERLAY_PX as i32;
                if dx * dx + dy * dy <= DISC_RADIUS_SQ * 4 {
                    put(rgba, x, y, DISC_RED);
                }
            }
        }
    }

    /// 5×7 digits, one bit per column, the high bit on the left.
    const GLYPH_W: i32 = 5;
    const GLYPH_H: i32 = 7;
    const SCALE: i32 = 2;
    const DIGITS: [[u8; 7]; 10] = [
        [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
    ];

    fn paint_count(rgba: &mut [u8], count: i64) {
        let text = count.to_string();
        let digits: Vec<u8> = text.bytes().map(|byte| byte.saturating_sub(b'0')).collect();
        let glyph = GLYPH_W * SCALE;
        let gap = SCALE;
        let width = digits.len() as i32 * glyph + (digits.len() as i32 - 1) * gap;
        let mut x = (OVERLAY_PX as i32 - width) / 2;
        let y = (OVERLAY_PX as i32 - GLYPH_H * SCALE) / 2;
        for digit in digits {
            paint_glyph(rgba, x, y, digit);
            x += glyph + gap;
        }
    }

    fn paint_glyph(rgba: &mut [u8], origin_x: i32, origin_y: i32, digit: u8) {
        let rows = DIGITS[usize::from(digit)];
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..GLYPH_W {
                let on = bits & (1 << (GLYPH_W - 1 - col)) != 0;
                if !on {
                    continue;
                }
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        put(
                            rgba,
                            origin_x + col * SCALE + dx,
                            origin_y + row as i32 * SCALE + dy,
                            WHITE,
                        );
                    }
                }
            }
        }
    }

    fn put(rgba: &mut [u8], x: i32, y: i32, color: [u8; 4]) {
        if x < 0 || y < 0 || x >= OVERLAY_PX as i32 || y >= OVERLAY_PX as i32 {
            return;
        }
        let index = ((y as u32 * OVERLAY_PX + x as u32) * 4) as usize;
        rgba[index..index + 4].copy_from_slice(&color);
    }

    #[cfg(test)]
    mod tests {
        use super::{overlay, OVERLAY_PX, WHITE};

        #[test]
        fn a_count_paints_a_disc_and_its_digit() {
            let seven = overlay(7);
            assert_eq!(seven.len(), (OVERLAY_PX * OVERLAY_PX * 4) as usize);
            assert_eq!(pixel(&seven, 0, 0), [0, 0, 0, 0]);
            // The disc is centered, so the middle row's first and last
            // columns stay clear and the columns just inside them do not.
            assert_eq!(pixel(&seven, 0, 16), [0, 0, 0, 0]);
            assert_eq!(pixel(&seven, 31, 16), [0, 0, 0, 0]);
            assert_ne!(pixel(&seven, 1, 16)[3], 0);
            assert_ne!(pixel(&seven, 30, 16)[3], 0);
            assert!(seven.contains(&255));
            // The top row of "7" is a solid bar, so the first scaled pixel is white.
            assert_eq!(pixel(&seven, 11, 9), WHITE);
        }

        #[test]
        fn the_overlay_is_fully_opaque_or_fully_clear() {
            // No antialiased edge. A mask-only draw sees the same silhouette
            // as the alpha channel.
            for count in [1, 7, 10, 99] {
                assert!(
                    overlay(count)
                        .chunks_exact(4)
                        .all(|pixel| pixel[3] == 0 || pixel[3] == 255),
                    "{count}"
                );
            }
        }

        #[test]
        fn different_counts_draw_different_pictures_and_past_99_does_not() {
            assert_ne!(overlay(7), overlay(8));
            assert_ne!(overlay(1), overlay(10));
            assert_eq!(overlay(99), overlay(100));
            assert_eq!(overlay(99), overlay(10_000));
        }

        #[test]
        fn icon_bits_are_bgra_with_a_monochrome_mask() {
            let (bgra, mask) = super::icon_bits(&[196, 30, 45, 255], 1, 1).unwrap();
            assert_eq!(bgra, vec![45, 30, 196, 255]);
            // One pixel, padded to a 16-bit row.
            assert_eq!(mask, vec![0, 0]);

            let (_, mask) = super::icon_bits(&[0, 0, 0, 0], 1, 1).unwrap();
            // The high bit is the leftmost pixel, and a set bit is transparent.
            assert_eq!(mask, vec![0x80, 0]);

            // Partial alpha stays opaque in the mask.
            let (_, mask) = super::icon_bits(&[1, 2, 3, 128], 1, 1).unwrap();
            assert_eq!(mask, vec![0, 0]);

            // 16px is the small-icon width. A WORD row is 2 bytes. A 32-bit
            // pad would be 4, and the second row would start four bytes in.
            let mut wide = vec![0u8; 16 * 2 * 4];
            wide[4..8].copy_from_slice(&[1, 2, 3, 255]);
            let (_, mask) = super::icon_bits(&wide, 16, 2).unwrap();
            assert_eq!(mask, vec![0xBF, 0xFF, 0xFF, 0xFF]);

            assert!(super::icon_bits(&[0, 0, 0, 0], 2, 2).is_none());
            assert!(super::icon_bits(&[], 0, 1).is_none());

            let rgba = overlay(7);
            let (bgra, mask) = super::icon_bits(&rgba, OVERLAY_PX, OVERLAY_PX).unwrap();
            assert_eq!(bgra.len(), rgba.len());
            // 32px rows are 4 bytes under WORD alignment too.
            assert_eq!(mask.len(), 128);
            assert_eq!(pixel(&rgba, 0, 0), [0, 0, 0, 0]);
            assert_ne!(mask[0] & 0x80, 0);
            assert_eq!(pixel(&rgba, 1, 16), super::DISC_RED);
            assert_eq!(
                pixel(&bgra, 1, 16),
                [
                    super::DISC_RED[2],
                    super::DISC_RED[1],
                    super::DISC_RED[0],
                    super::DISC_RED[3],
                ]
            );
            // (16, 16) is inside the disc, so its mask bit is clear.
            let center = 16 * 4 + 16 / 8;
            assert_eq!(mask[center] & 0x80, 0);
        }

        fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
            let index = ((y * OVERLAY_PX + x) * 4) as usize;
            rgba[index..index + 4].try_into().expect("pixel")
        }
    }
}

// Linux tests compile `drawing` so the picture is checked, but only Windows
// calls it. Re-exporting there as well is an unused import (`-D warnings`).
#[cfg(windows)]
pub use drawing::{icon_bits, overlay, OVERLAY_PX};

#[cfg(test)]
mod tests {
    use super::{accessible_name, visible_count};

    #[test]
    fn zero_and_negative_counts_clear() {
        assert_eq!(visible_count(None), None);
        assert_eq!(visible_count(Some(0)), None);
        assert_eq!(visible_count(Some(-3)), None);
        assert_eq!(visible_count(Some(7)), Some(7));
    }

    #[test]
    fn the_name_says_the_real_count() {
        assert_eq!(accessible_name(1), "1 unread message");
        assert_eq!(accessible_name(2), "2 unread messages");
        // The picture draws 100 as 99. The name does not.
        assert_eq!(accessible_name(99), "99 unread messages");
        assert_eq!(accessible_name(100), "100 unread messages");
    }
}
