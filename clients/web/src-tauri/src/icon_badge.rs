//! The picture Windows paints on a taskbar button.
//!
//! `Window::set_badge_count` is a no-op on Windows. The taskbar takes an
//! overlay image instead, and this builds that image: a red disc with the
//! count in white. Other platforms badge with a number and never call this.
//! The disc is drawn here, rather than shipped as an asset, because the
//! count is part of the picture.

/// Edge length of the overlay. Windows scales a taskbar overlay down from this.
#[cfg(any(windows, test))]
pub const OVERLAY_PX: u32 = 32;

/// A count the icon should show. Zero and negative clear, matching the page,
/// which clears when the unread total is zero.
pub fn visible_count(count: Option<i64>) -> Option<i64> {
    count.filter(|count| *count > 0)
}

/// RGBA overlay for `count`, or a fully transparent image when `count` would
/// not be shown. Counts above 99 draw as 99: a taskbar overlay has no room
/// for a third digit.
#[cfg(any(windows, test))]
pub fn overlay(count: i64) -> Vec<u8> {
    let mut rgba = vec![0u8; (OVERLAY_PX * OVERLAY_PX * 4) as usize];
    let Some(shown) = visible_count(Some(count)) else {
        return rgba;
    };
    fill_disc(&mut rgba);
    paint_count(&mut rgba, shown.min(99));
    rgba
}

#[cfg(any(windows, test))]
const DISC_RED: [u8; 4] = [196, 30, 45, 255];
#[cfg(any(windows, test))]
const WHITE: [u8; 4] = [255, 255, 255, 255];

/// Radius, in pixels from the center of the 32px image. 15² is 225, so the
/// corners (distance² 450) stay transparent and the disc stays inside the square.
#[cfg(any(windows, test))]
const DISC_RADIUS_SQ: i32 = 15 * 15;

#[cfg(any(windows, test))]
fn fill_disc(rgba: &mut [u8]) {
    let center = (OVERLAY_PX as i32 - 1) / 2;
    for y in 0..OVERLAY_PX as i32 {
        for x in 0..OVERLAY_PX as i32 {
            let dx = x - center;
            let dy = y - center;
            if dx * dx + dy * dy <= DISC_RADIUS_SQ {
                put(rgba, x, y, DISC_RED);
            }
        }
    }
}

/// 5×7 digits, one bit per column, the high bit on the left.
#[cfg(any(windows, test))]
const GLYPH_W: i32 = 5;
#[cfg(any(windows, test))]
const GLYPH_H: i32 = 7;
#[cfg(any(windows, test))]
const SCALE: i32 = 2;
#[cfg(any(windows, test))]
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

#[cfg(any(windows, test))]
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

#[cfg(any(windows, test))]
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

#[cfg(any(windows, test))]
fn put(rgba: &mut [u8], x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= OVERLAY_PX as i32 || y >= OVERLAY_PX as i32 {
        return;
    }
    let index = ((y as u32 * OVERLAY_PX + x as u32) * 4) as usize;
    rgba[index..index + 4].copy_from_slice(&color);
}

#[cfg(test)]
mod tests {
    use super::{overlay, visible_count, OVERLAY_PX, WHITE};

    #[test]
    fn zero_and_negative_counts_clear() {
        assert_eq!(visible_count(None), None);
        assert_eq!(visible_count(Some(0)), None);
        assert_eq!(visible_count(Some(-3)), None);
        assert_eq!(visible_count(Some(7)), Some(7));
    }

    #[test]
    fn a_clear_overlay_is_transparent_and_a_count_is_not() {
        let clear = overlay(0);
        assert!(clear.iter().all(|byte| *byte == 0));
        assert_eq!(clear.len(), (OVERLAY_PX * OVERLAY_PX * 4) as usize);

        let seven = overlay(7);
        assert_eq!(pixel(&seven, 0, 0), [0, 0, 0, 0]);
        assert!(seven.contains(&255));
        // The top row of "7" is a solid bar, so the first scaled pixel is white.
        assert_eq!(pixel(&seven, 11, 9), WHITE);
    }

    #[test]
    fn different_counts_draw_different_pictures_and_past_99_does_not() {
        assert_ne!(overlay(7), overlay(8));
        assert_ne!(overlay(1), overlay(10));
        assert_eq!(overlay(99), overlay(100));
        assert_eq!(overlay(99), overlay(10_000));
    }

    fn pixel(rgba: &[u8], x: u32, y: u32) -> [u8; 4] {
        let index = ((y * OVERLAY_PX + x) * 4) as usize;
        rgba[index..index + 4].try_into().expect("pixel")
    }
}
