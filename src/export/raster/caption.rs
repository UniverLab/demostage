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
