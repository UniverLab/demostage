//! Glyph rasterization: the font-fallback chain and the alpha-blend of one
//! placed glyph.

use fontdue::{Font, Metrics};

use super::cells::blend_pixel;
use super::FallbackReport;

/// Rasterize `ch` using `primary`, falling back to `emoji` when the primary
/// font lacks the glyph, then to `last_resort` when both before it lack it.
/// Records the outcome in `report`.
pub(super) fn rasterize_with_fallback(
    primary: &Font,
    emoji: &Font,
    last_resort: &Font,
    ch: char,
    px: f32,
    report: &mut FallbackReport,
) -> (Metrics, Vec<u8>) {
    if primary.has_glyph(ch) {
        return primary.rasterize(ch, px);
    }
    if emoji.has_glyph(ch) {
        report.record_fallback(ch, "Noto Emoji");
        return emoji.rasterize(ch, px);
    }
    if last_resort.has_glyph(ch) {
        report.record_fallback(ch, "DejaVu Sans Mono");
        return last_resort.rasterize(ch, px);
    }
    report.record_unresolved(ch);
    (
        Metrics {
            xmin: 0,
            ymin: 0,
            width: 0,
            height: 0,
            advance_width: 0.0,
            advance_height: 0.0,
            bounds: fontdue::OutlineBounds {
                xmin: 0.0,
                ymin: 0.0,
                width: 0.0,
                height: 0.0,
            },
        },
        vec![],
    )
}

/// One rasterized glyph placed on the canvas: its metrics and coverage, where
/// it lands (`ox`, `top`), and the colour it is blended in.
pub(super) struct GlyphBlit<'a> {
    pub(super) m: &'a Metrics,
    pub(super) cov: &'a [u8],
    pub(super) ox: i32,
    pub(super) top: i32,
    pub(super) fg: [u8; 3],
}

impl GlyphBlit<'_> {
    /// Alpha-blend the glyph into the RGBA `img` (`w`×`h`), clipped to the
    /// canvas on every side.
    pub(super) fn draw(&self, img: &mut [u8], w: usize, h: usize) {
        for gy in 0..self.m.height {
            let py = self.top + gy as i32;
            if py < 0 || py as usize >= h {
                continue;
            }
            for gx in 0..self.m.width {
                let pxc = self.ox + gx as i32;
                if pxc < 0 || pxc as usize >= w {
                    continue;
                }
                let a = self.cov[gy * self.m.width + gx] as u32;
                if a == 0 {
                    continue;
                }
                let p = (py as usize * w + pxc as usize) * 4;
                blend_pixel(img, p, self.fg, a);
            }
        }
    }
}
