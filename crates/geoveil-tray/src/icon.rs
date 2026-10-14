//! Tray icons, drawn at startup (no image files to ship): a filled,
//! anti-aliased disc whose colour shows the state, with a darker rim so it
//! stays visible on light and dark panels.

use crate::model::IconKind;

/// Icon edge length in pixels.
pub const SIZE: u32 = 32;

/// Fill colour for a state.
pub fn color(kind: IconKind) -> [u8; 3] {
    match kind {
        IconKind::Idle => [140, 140, 140],
        IconKind::Engaged => [46, 160, 67],
        IconKind::Paused => [212, 155, 30],
        IconKind::Error => [207, 34, 46],
    }
}

/// RGBA pixels (row-major, `SIZE * SIZE * 4` bytes).
pub fn rgba(kind: IconKind) -> Vec<u8> {
    let [r, g, b] = color(kind);
    let rim = [r / 2, g / 2, b / 2];
    let size = SIZE as f32;
    let center = size / 2.0;
    let radius = size / 2.0 - 1.5;
    let rim_width = 2.0;
    let mut out = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let d = (dx * dx + dy * dy).sqrt();
            // Coverage of the disc edge, anti-aliased over one pixel.
            let alpha = (radius - d + 0.5).clamp(0.0, 1.0);
            let px = if d > radius - rim_width { rim } else { [r, g, b] };
            out.extend_from_slice(&[px[0], px[1], px[2], (alpha * 255.0).round() as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(buf: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * SIZE + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn size_and_shape() {
        for kind in [IconKind::Idle, IconKind::Engaged, IconKind::Paused, IconKind::Error] {
            let buf = rgba(kind);
            assert_eq!(buf.len(), (SIZE * SIZE * 4) as usize);
            let [r, g, b] = color(kind);
            assert_eq!(pixel(&buf, SIZE / 2, SIZE / 2), [r, g, b, 255], "{kind:?} centre");
            assert_eq!(pixel(&buf, 0, 0)[3], 0, "{kind:?} corner is transparent");
        }
    }

    #[test]
    fn states_have_distinct_colours() {
        let all = [IconKind::Idle, IconKind::Engaged, IconKind::Paused, IconKind::Error].map(color);
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(all[i], all[j]);
            }
        }
    }
}
