//! The on-canvas caption track drawn over a single-terminal recording.

use std::collections::HashMap;

use fontdue::{Font, Metrics};

use super::glyph::{rasterize_with_fallback, GlyphBlit};
use super::FallbackReport;
use crate::error::Result;
use crate::fonts;

/// An on-canvas caption track: a bottom bar with centered text, switching to the
/// latest caption that is active at the current time.
pub struct CaptionOverlay {
    captions: Vec<(f64, String)>,
    font: Font,
    emoji_font: Font,
    last_resort_font: Font,
    pub(super) glyphs: HashMap<char, (Metrics, Vec<u8>)>,
    px: f32,
    cell_w: usize,
}

impl CaptionOverlay {
    pub fn new(
        captions: Vec<(f64, String)>,
        px: f32,
        font_name: &str,
        emoji_font: Font,
        last_resort_font: Font,
    ) -> Result<Self> {
        let font = fonts::load(font_name);
        // Printable ASCII only: every bundled font covers it, so it can be cached
        // straight from the primary face. Everything else is rasterized on demand
        // in `draw`, through the fallback chain — pre-caching a fixed symbol table
        // here would fill the cache behind `entry().or_insert_with()` and silently
        // bypass both the fallback and the report.
        let mut glyphs = HashMap::new();
        for code in 0x20u8..=0x7e {
            let ch = code as char;
            glyphs.insert(ch, font.rasterize(ch, px));
        }
        Ok(CaptionOverlay {
            captions,
            font,
            emoji_font,
            last_resort_font,
            glyphs,
            px,
            cell_w: (px * 0.6).round().max(1.0) as usize,
        })
    }

    /// The caption text active at time `t` (latest with start ≤ t), if non-empty.
    pub fn active(&self, t: f64) -> Option<&str> {
        let mut chosen: Option<&str> = None;
        for (start, text) in &self.captions {
            if *start <= t {
                chosen = Some(text.as_str());
            } else {
                break;
            }
        }
        chosen.filter(|s| !s.is_empty())
    }

    /// Draw the active caption onto `img` (`w`×`h` RGBA) at time `t`.
    pub fn draw(
        &mut self,
        img: &mut [u8],
        w: usize,
        h: usize,
        t: f64,
        fallback_report: &mut FallbackReport,
    ) {
        let Some(text) = self.active(t).map(str::to_owned) else {
            return;
        };
        let bar_h = (self.px * 2.2).round() as usize;
        if bar_h == 0 || bar_h >= h || w == 0 {
            return;
        }
        let y0 = h - bar_h;
        // Darken the bar to ~40% so light text reads on top.
        for y in y0..h {
            for x in 0..w {
                let p = (y * w + x) * 4;
                img[p] = (img[p] as u32 * 2 / 5) as u8;
                img[p + 1] = (img[p + 1] as u32 * 2 / 5) as u8;
                img[p + 2] = (img[p + 2] as u32 * 2 / 5) as u8;
            }
        }
        let text_w = text.chars().count() * self.cell_w;
        let start_x = w.saturating_sub(text_w) / 2;
        let baseline = y0 + bar_h / 2 + (self.px * 0.35) as usize;
        let fg = [235u8, 235, 235];
        let mut cx = start_x;
        for ch in text.chars() {
            // On-demand rasterization for caption characters.
            let (m, cov) = self.glyphs.entry(ch).or_insert_with(|| {
                rasterize_with_fallback(
                    &self.font,
                    &self.emoji_font,
                    &self.last_resort_font,
                    ch,
                    self.px,
                    fallback_report,
                )
            });
            let ox = cx as i32 + ((self.cell_w as i32 - m.width as i32) / 2).max(0);
            let top = baseline as i32 - (m.height as i32 + m.ymin);
            GlyphBlit {
                m,
                cov,
                ox,
                top,
                fg,
            }
            .draw(img, w, h);
            cx += self.cell_w;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- fixtures ---------------------------------------------------------
    //
    // px = 10.0 fixes every constant the mutants live in:
    //   cell_w  = (10.0 * 0.6).round() = 6
    //   bar_h   = (10.0 * 2.2).round() = 22  → on 60×30 the bar is rows 8..30
    //   "Hi.g"  = 4 cells → text_w = 24 → start_x = (60 - 24) / 2 = 18
    //   baseline = y0 + bar_h / 2 + (10.0 * 0.35) as usize = 8 + 11 + 3 = 22
    // Glyph placement then is: 'H' ox 18 top 14, 'i' ox 24 top 14,
    // '.' ox 32 top 20, 'g' ox 36 top 16 ('g' has ymin -3, the only char
    // whose `top` moves when `height + ymin` is computed wrong).

    const W: usize = 60;
    const H: usize = 30;
    const TEXT: &str = "Hi.g";

    fn overlay(text: &str) -> CaptionOverlay {
        CaptionOverlay::new(
            vec![(0.0, text.to_string())],
            10.0,
            fonts::DEFAULT_FONT,
            fonts::load_emoji(),
            fonts::load_last_resort(),
        )
        .unwrap()
    }

    /// Every pixel gets a distinct, non-zero RGB, so a skipped, misplaced or
    /// double-applied byte always shows up as an exact-value mismatch:
    /// `x * 2 / 5` never maps any of these channels back onto themselves.
    fn patterned() -> Vec<u8> {
        let mut img = vec![0u8; W * H * 4];
        for y in 0..H {
            for x in 0..W {
                let p = (y * W + x) * 4;
                img[p] = (x * 3 + 50) as u8;
                img[p + 1] = (y * 5 + 90) as u8;
                img[p + 2] = (x + y + 70) as u8;
                img[p + 3] = 255;
            }
        }
        img
    }

    fn pixel(img: &[u8], x: usize, y: usize) -> [u8; 4] {
        let p = (y * W + x) * 4;
        [img[p], img[p + 1], img[p + 2], img[p + 3]]
    }

    fn draw_into(ov: &mut CaptionOverlay, img: &mut [u8]) {
        let mut report = FallbackReport::new();
        ov.draw(img, W, H, 0.0, &mut report);
        assert!(report.is_empty(), "caption is pure ASCII: no fallback");
    }

    /// Exact bar geometry and colours: the painted rows are precisely
    /// `y0..h`, the rows above stay byte-identical, and darkened pixels are
    /// `[r*2/5, g*2/5, b*2/5, a]` with the alpha channel untouched.
    #[test]
    fn bar_darkening_covers_exactly_rows_8_to_30() {
        let before = patterned();
        let mut img = before.clone();
        let mut ov = overlay(TEXT);
        draw_into(&mut ov, &mut img);

        // The set of changed rows is exactly y0..h (y0 = 30 - 22 = 8):
        // no row above the bar may be touched, no bar row may be skipped.
        let changed: Vec<usize> = (0..H)
            .filter(|&y| img[y * W * 4..(y + 1) * W * 4] != before[y * W * 4..(y + 1) * W * 4])
            .collect();
        assert_eq!(changed, (8..30).collect::<Vec<_>>(), "painted rows");

        // Rows above the bar are byte-for-byte untouched.
        assert_eq!(&img[..8 * W * 4], &before[..8 * W * 4]);

        // Spot pixels, exact RGBA:
        // (0,8)  = [50,130,78,255] → [20,52,31,255]
        assert_eq!(pixel(&img, 0, 8), [20, 52, 31, 255], "bar top-left pixel");
        // (3,8)  = [59,130,81,255] → [23,52,32,255]
        assert_eq!(pixel(&img, 3, 8), [23, 52, 32, 255], "bar top-row pixel");
        // (59,29) = [227,235,158,255] → [90,94,63,255]
        assert_eq!(
            pixel(&img, 59, 29),
            [90, 94, 63, 255],
            "bar bottom-right pixel"
        );
    }

    /// Independent transcription of `CaptionOverlay::draw`'s layout maths for
    /// this fixture, used as a pixel-exact oracle: bar rows, darkening,
    /// centring, baseline and per-cell glyph placement. (The glyph blend
    /// itself is the production `GlyphBlit`, which has no mutants in scope.)
    fn reference_draw(ov: &CaptionOverlay) -> Vec<u8> {
        let text = ov.active(0.0).expect("caption active at t = 0").to_string();

        let bar_h = (ov.px * 2.2).round() as usize;
        assert!(bar_h > 0 && bar_h < H && W > 0, "fixture: bar must fit");
        let y0 = H - bar_h;

        let mut img = patterned();
        for y in y0..H {
            for x in 0..W {
                let p = (y * W + x) * 4;
                for k in 0..3 {
                    img[p + k] = (img[p + k] as u32 * 2 / 5) as u8;
                }
            }
        }

        let text_w = text.chars().count() * ov.cell_w;
        let start_x = W.saturating_sub(text_w) / 2;
        let baseline = y0 + bar_h / 2 + (ov.px * 0.35) as usize;
        let fg = [235u8, 235, 235];
        let mut cx = start_x;
        for ch in text.chars() {
            let (m, cov) = ov.glyphs.get(&ch).expect("ASCII glyph pre-cached");
            let ox = cx as i32 + ((ov.cell_w as i32 - m.width as i32) / 2).max(0);
            let top = baseline as i32 - (m.height as i32 + m.ymin);
            GlyphBlit {
                m,
                cov,
                ox,
                top,
                fg,
            }
            .draw(&mut img, W, H);
            cx += ov.cell_w;
        }
        img
    }

    /// Whole-buffer equality against the reference: every arithmetic mutant
    /// in `draw` changes at least one byte (or panics) for this fixture.
    #[test]
    fn draw_matches_reference_geometry_pixel_for_pixel() {
        let mut ov = overlay(TEXT);
        let mut img = patterned();
        draw_into(&mut ov, &mut img);

        let want = reference_draw(&ov);
        assert_eq!(img.len(), want.len());
        if img != want {
            let i = img
                .iter()
                .zip(&want)
                .position(|(a, b)| a != b)
                .expect("buffers differ, so some byte must differ");
            panic!(
                "first difference at byte {i} (x = {}, y = {}, rgba channel {}): got {:?}, want {:?}",
                i / 4 % W,
                i / 4 / W,
                i % 4,
                &img[i..i + 4],
                &want[i..i + 4],
            );
        }
    }

    /// Exact glyph pixels: hardcoded RGBA around each glyph pin start_x,
    /// ox, top, the cell advance and the `height + ymin` offset.
    #[test]
    fn draw_places_glyphs_on_exact_pixels() {
        let mut ov = overlay(TEXT);
        let mut img = patterned();
        draw_into(&mut ov, &mut img);

        // 'H' at ox 18, top 14: ink in its first column, background one row
        // above and one column left — pins start_x, ox and top for the line.
        assert_eq!(
            pixel(&img, 18, 13),
            [41, 62, 40, 255],
            "background above 'H'"
        );
        assert_eq!(
            pixel(&img, 17, 14),
            [40, 64, 40, 255],
            "background left of 'H'"
        );
        assert_eq!(
            pixel(&img, 18, 14),
            [59, 80, 58, 255],
            "'H' first ink column"
        );
        assert_eq!(pixel(&img, 30, 14), [56, 64, 45, 255], "gap right of 'i'");

        // '.' at ox 32, top 20 (narrow glyph: cell_w - width = 4 > 0).
        assert_eq!(
            pixel(&img, 31, 20),
            [57, 76, 48, 255],
            "background left of '.'"
        );
        assert_eq!(
            pixel(&img, 32, 20),
            [110, 123, 103, 255],
            "'.' first ink column"
        );

        // 'g' at ox 36, top 16 — its ymin of -3 must count as `height + ymin`
        // = 6 below the baseline; background one row above its ink …
        assert_eq!(
            pixel(&img, 36, 15),
            [63, 66, 48, 255],
            "background above 'g'"
        );
        assert_eq!(pixel(&img, 37, 16), [95, 98, 83, 255], "'g' first ink row");
        // … and at row 10, where ink appears if `height - ymin` is used.
        assert_eq!(
            pixel(&img, 37, 10),
            [64, 56, 46, 255],
            "row above 'g' when ymin is dropped"
        );
    }
}
