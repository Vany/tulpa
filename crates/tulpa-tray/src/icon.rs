//! RGBA icon renderer for the menu-bar disc.
//!
//! The disc tint encodes [`LinkState`]; a purple center mark is a dot normally and
//! a pause glyph while paused. Pure pixel math: no platform calls, no I/O, safe to
//! unit-test on any host.

use crate::state::LinkState;

/// Purple accent of the tulpa mark.
pub const MARK_PURPLE: [u8; 4] = [0x9B, 0x59, 0xF5, 0xFF];

/// Default edge length in pixels of the rendered square icon.
pub const ICON_SIZE: u32 = 44;

/// Renders the icon as tightly packed RGBA rows, `size * size * 4` bytes.
///
/// Layout: a soft-edged disc filled with the link-state tint, carrying a purple
/// center dot; while paused the dot becomes a pause glyph (two vertical bars).
pub fn render_icon(link: LinkState, paused: bool, size: u32) -> Vec<u8> {
    let size = size.max(1);
    let dim = u64::from(size);
    let byte_len = usize::try_from(dim.saturating_mul(dim).saturating_mul(4)).unwrap_or(0);
    let mut pixels = vec![0_u8; byte_len];
    let row_len = usize::try_from(dim.saturating_mul(4)).unwrap_or(0);

    let center = f64::from(size.saturating_sub(1)) * 0.5;
    let disc_radius = center * 0.86;
    let mark_radius = center * 0.34;
    let bar_half_width = center * 0.20;
    let bar_half_height = center * 0.30;
    let bar_offset = center * 0.26;
    let tint = link.tint();

    for (y, row) in pixels.chunks_exact_mut(row_len).enumerate() {
        let y_pos = f64::from(u32::try_from(y).unwrap_or(u32::MAX));
        let dy = y_pos - center;
        for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
            let x_pos = f64::from(u32::try_from(x).unwrap_or(u32::MAX));
            let dx = x_pos - center;
            let distance = dx.hypot(dy);
            if distance > disc_radius {
                continue;
            }
            let coverage = edge_coverage(distance, disc_radius);
            pixel.copy_from_slice(&blend(tint, coverage));
            if distance <= mark_radius {
                let mark = if paused {
                    let in_bar = dx.abs() >= bar_offset - bar_half_width
                        && dx.abs() <= bar_offset + bar_half_width
                        && dy.abs() <= bar_half_height;
                    if in_bar {
                        MARK_PURPLE
                    } else {
                        blend(tint, coverage)
                    }
                } else {
                    MARK_PURPLE
                };
                pixel.copy_from_slice(&mark);
            }
        }
    }
    pixels
}

/// Alpha-blends the tint over transparency: opaque core, feathered edge.
const fn blend(tint: [u8; 4], coverage: u8) -> [u8; 4] {
    [tint[0], tint[1], tint[2], coverage]
}

/// Anti-aliasing coverage for a pixel at `distance` from the disc center.
///
/// Pixels well inside the radius are opaque; the outermost `SOFT_EDGE` units
/// feather linearly to transparent. The float-to-byte step is a threshold
/// ladder (ceil semantics) so no lossy cast is needed.
fn edge_coverage(distance: f64, radius: f64) -> u8 {
    const SOFT_EDGE: f64 = 1.5;
    if distance <= radius - SOFT_EDGE {
        return 0xFF;
    }
    let remaining = (radius - distance).clamp(0.0, SOFT_EDGE);
    let fraction = remaining / SOFT_EDGE;
    (0..=u8::MAX)
        .rev()
        .find(|step| f64::from(*step) >= fraction * f64::from(u8::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(buffer: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let offset = u64::from(y)
            .saturating_mul(u64::from(size))
            .saturating_add(u64::from(x))
            .saturating_mul(4);
        let start = usize::try_from(offset).unwrap_or(0);
        buffer
            .get(start..start.saturating_add(4))
            .and_then(|slice| <[u8; 4]>::try_from(slice).ok())
            .unwrap_or([0; 4])
    }

    #[test]
    fn output_has_expected_shape() {
        let buffer = render_icon(LinkState::Off, false, ICON_SIZE);
        let expected = u64::from(ICON_SIZE)
            .saturating_mul(u64::from(ICON_SIZE))
            .saturating_mul(4);
        assert_eq!(buffer.len(), usize::try_from(expected).unwrap_or(0));
    }

    #[test]
    fn disc_is_tinted_and_corners_transparent() {
        let size = 44;
        let buffer = render_icon(LinkState::Connected, false, size);
        let center = size.div_euclid(2);
        assert_eq!(pixel(&buffer, size, center, center), MARK_PURPLE);
        let crown = pixel(&buffer, size, center, 6);
        assert_eq!(crown[3], 0xFF, "inside the disc must be opaque");
        assert_eq!(crown[0], 0x4E);
        assert_eq!(crown[1], 0xC9);
        assert_eq!(crown[2], 0x6B);
        assert_eq!(pixel(&buffer, size, 0, 0)[3], 0, "corners stay transparent");
    }

    #[test]
    fn state_tint_changes_the_disc_not_the_mark() {
        let size = 44;
        let off = render_icon(LinkState::Off, false, size);
        let on = render_icon(LinkState::Connected, false, size);
        let center = size.div_euclid(2);
        assert_ne!(pixel(&off, size, 5, center), pixel(&on, size, 5, center));
        assert_eq!(pixel(&off, size, center, center), MARK_PURPLE);
        assert_eq!(pixel(&on, size, center, center), MARK_PURPLE);
    }

    #[test]
    fn paused_replaces_dot_with_two_bars() {
        let size = 44;
        let running = render_icon(LinkState::Connected, false, size);
        let paused = render_icon(LinkState::Connected, true, size);
        let center = size.div_euclid(2);
        let gap = pixel(&paused, size, center, center);
        assert_eq!(
            gap,
            [0x4E, 0xC9, 0x6B, 0xFF],
            "gap between bars shows the disc"
        );
        let bar_column = center.saturating_add(5);
        assert_eq!(pixel(&paused, size, bar_column, center), MARK_PURPLE);
        assert_ne!(paused, running);
        let bars = (0..size)
            .filter(|y| pixel(&paused, size, bar_column, *y) == MARK_PURPLE)
            .count();
        assert!(bars >= 8, "pause glyph needs a visible vertical extent");
    }
}
