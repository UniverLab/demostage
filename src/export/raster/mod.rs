//! Shared frame rasterizer for the pixel targets (gif, mp4, and the multi-scene
//! stage). Replays a recording through a vt100 parser at the score's fps and
//! renders each frame to RGBA with the embedded monospace font. Pure Rust;
//! covers printable ASCII, ANSI colours, and every other glyph the capture
//! actually prints (banners, box-drawing, arrows) as long as the font has it.
//!
//! Split by responsibility: [`cells`] paints the text grid, [`glyph`] the font
//! rasterization, [`caption`] the caption bar.

mod caption;
mod cells;
mod glyph;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use fontdue::{Font, Metrics};
use vt100::Parser;

use super::run::Recording;
use crate::error::Result;
use crate::fonts;
use crate::model::Score;

pub use caption::CaptionOverlay;
/// The block coverage the SVG targets' geometry is asserted against, for
/// tests only — production code reaches it through [`cells::solid_cell`].
#[cfg(test)]
pub(crate) use cells::block_cell;
pub use cells::{parse_hex, TextCell, TextFrame};
use cells::{render_cells, resolve, FontSet, GridLayout};
use glyph::rasterize_with_fallback;

const DEFAULT_FG: [u8; 3] = [200, 200, 200];

/// The pixel geometry and rate of a render.
pub struct Plan {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
}

/// Records which characters needed fallback during an export, and which
/// characters no bundled font could draw.
#[derive(Default)]
pub struct FallbackReport {
    primary_font_name: String,
    fallen_back: BTreeMap<char, &'static str>,
    unresolved: BTreeSet<char>,
}

impl FallbackReport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_primary_name(name: &str) -> Self {
        Self {
            primary_font_name: name.to_owned(),
            fallen_back: BTreeMap::new(),
            unresolved: BTreeSet::new(),
        }
    }

    fn record_fallback(&mut self, ch: char, font_name: &'static str) {
        if (ch as u32) < 0x2800 || (ch as u32) > 0x28ff {
            self.fallen_back.entry(ch).or_insert(font_name);
        }
    }

    fn record_unresolved(&mut self, ch: char) {
        if (ch as u32) < 0x2800 || (ch as u32) > 0x28ff {
            self.unresolved.insert(ch);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.fallen_back.is_empty() && self.unresolved.is_empty()
    }

    pub fn format(&self, demo_name: &str) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.fallen_back.is_empty() {
            let chars: String = self.fallen_back.keys().collect();
            let fonts: BTreeSet<&str> = self.fallen_back.values().copied().collect();
            let font_list = fonts.into_iter().collect::<Vec<_>>().join(", ");
            let primary = if self.primary_font_name.is_empty() {
                "primary font".to_owned()
            } else {
                self.primary_font_name.clone()
            };
            lines.push(format!(
                "{demo_name}: {} character{} not in {primary}, drawn with {font_list}: {chars}",
                self.fallen_back.len(),
                if self.fallen_back.len() == 1 { "" } else { "s" }
            ));
        }
        if !self.unresolved.is_empty() {
            let chars: String = self.unresolved.iter().collect();
            lines.push(format!(
                "{demo_name}: {} character{} no bundled font can draw, rendered blank: {chars}",
                self.unresolved.len(),
                if self.unresolved.len() == 1 { "" } else { "s" }
            ));
        }
        lines
    }
}

fn cell_size(score: &Score) -> (usize, usize) {
    let px = score
        .layout
        .panes
        .iter()
        .find_map(|p| p.font_size)
        .unwrap_or(16) as f32;
    let line_height = score.layout.line_height.max(0.5);
    (
        (px * 0.6).round().max(1.0) as usize,
        (px * line_height).round().max(1.0) as usize,
    )
}

/// Pixel dimensions + fps for a recording, without rendering.
pub fn plan(rec: &Recording, score: &Score) -> Plan {
    let (cw, ch) = cell_size(score);
    Plan {
        width: rec.cols as usize * cw,
        height: rec.rows as usize * ch,
        fps: score.layout.fps.max(1),
    }
}

/// Pre-fill the glyph cache with everything the recording prints: every
/// non-ASCII character in the output (box-drawing, banner blocks like `█`/`░`,
/// arrows, accents, …), so it renders on the pixel targets too — not just the
/// printable-ASCII range. Cache every non-ASCII glyph the capture actually
/// prints so it renders here too. Characters the primary font lacks go through
/// the fallback chain and are reported.
fn precache_glyphs(
    rec: &Recording,
    glyphs: &mut HashMap<char, (Metrics, Vec<u8>)>,
    font: &Font,
    emoji_font: &Font,
    last_resort_font: &Font,
    px: f32,
    report: &mut FallbackReport,
) {
    for (_, chunk) in &rec.events {
        for ch in chunk.chars() {
            if ch.is_control() || ch.is_whitespace() {
                continue;
            }
            glyphs.entry(ch).or_insert_with(|| {
                rasterize_with_fallback(font, emoji_font, last_resort_font, ch, px, report)
            });
        }
    }
}

/// Canvas size the caption is drawn over: the full frame (`cols * cell_w` by
/// `rows * cell_h`), not a sum or quotient. Pure so the `*` operators are
/// unit-testable without rendering a frame.
fn caption_canvas(cols: usize, cell_w: usize, rows: usize, cell_h: usize) -> (usize, usize) {
    (cols * cell_w, rows * cell_h)
}

/// A stateful, frame-by-frame terminal renderer. Advancing it monotonically
/// replays the recording at the score's fps — used directly by gif/mp4 and, in
/// lockstep with other panes, by the multi-scene stage.
pub struct FrameSource<'a> {
    rec: &'a Recording,
    font: Font,
    emoji_font: Font,
    last_resort_font: Font,
    font_name: String,
    px: f32,
    glyphs: HashMap<char, (Metrics, Vec<u8>)>,
    cols: usize,
    rows: usize,
    cell_w: usize,
    cell_h: usize,
    ascent: f32,
    default_bg: [u8; 3],
    parser: Parser,
    ev_idx: usize,
    dt: f64,
    frame: usize,
    n_frames: usize,
    caption: Option<CaptionOverlay>,
    fallback_report: FallbackReport,
}

impl<'a> FrameSource<'a> {
    pub fn new(rec: &'a Recording, score: &Score) -> Result<Self> {
        let font_name = score
            .layout
            .font_family
            .as_deref()
            .unwrap_or(fonts::DEFAULT_FONT);
        let font = fonts::load(font_name);
        let emoji_font = fonts::load_emoji();
        let last_resort_font = fonts::load_last_resort();
        let px = score
            .layout
            .panes
            .iter()
            .find_map(|p| p.font_size)
            .unwrap_or(16) as f32;
        let (cell_w, cell_h) = cell_size(score);
        let default_bg = score
            .layout
            .background
            .as_deref()
            .and_then(parse_hex)
            .unwrap_or([11, 15, 20]);
        let ascent = font
            .horizontal_line_metrics(px)
            .map(|m| m.ascent)
            .unwrap_or(px * 0.8);

        let mut fallback_report = FallbackReport::with_primary_name(font_name);
        let mut glyphs = HashMap::new();
        for code in 0x21u8..=0x7e {
            let ch = code as char;
            glyphs.insert(ch, font.rasterize(ch, px));
        }
        precache_glyphs(
            rec,
            &mut glyphs,
            &font,
            &emoji_font,
            &last_resort_font,
            px,
            &mut fallback_report,
        );

        let fps = score.layout.fps.max(1) as f64;
        let dt = 1.0 / fps;
        let total = rec.duration.max(dt);
        let n_frames = (total / dt).ceil() as usize + 1;

        let caption = if rec.captions.is_empty() {
            None
        } else {
            Some(CaptionOverlay::new(
                rec.captions.clone(),
                18.0,
                font_name,
                emoji_font.clone(),
                last_resort_font.clone(),
            )?)
        };

        Ok(FrameSource {
            rec,
            font,
            emoji_font,
            last_resort_font,
            font_name: font_name.to_owned(),
            px,
            glyphs,
            cols: rec.cols as usize,
            rows: rec.rows as usize,
            cell_w,
            cell_h,
            ascent,
            default_bg,
            parser: Parser::new(rec.rows, rec.cols, 0),
            ev_idx: 0,
            dt,
            frame: 0,
            n_frames,
            caption,
            fallback_report,
        })
    }

    pub fn n_frames(&self) -> usize {
        self.n_frames
    }

    pub fn dims(&self) -> (usize, usize) {
        (self.cols * self.cell_w, self.rows * self.cell_h)
    }

    /// Feed the events up to the current frame's time and step forward.
    /// Returns the frame time, or `None` once exhausted. [`next_frame`] and
    /// [`next_text_frame`] share it so the raster and vector paths cannot drift.
    fn advance(&mut self) -> Option<f64> {
        if self.frame >= self.n_frames {
            return None;
        }
        let t = self.frame as f64 * self.dt;
        self.feed_events_upto(t);
        self.frame += 1;
        Some(t)
    }

    /// Feed every recorded event at or before `t` into the parser, in order.
    /// The count of events to feed is taken from the remaining slice in one
    /// pass first, so feeding itself never has to re-check the timeline.
    fn feed_events_upto(&mut self, t: f64) {
        let start = self.ev_idx;
        let take = self.rec.events[start..]
            .iter()
            .take_while(|e| e.0 <= t)
            .count();
        let end = start + take;
        for ev in &self.rec.events[start..end] {
            self.parser.process(ev.1.as_bytes());
        }
        self.ev_idx = end;
    }

    /// Render the next frame, or `None` once exhausted.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let t = self.advance()?;
        let fonts = FontSet {
            primary: &self.font,
            emoji: &self.emoji_font,
            last_resort: &self.last_resort_font,
            px: self.px,
        };
        let grid = GridLayout {
            cols: self.cols,
            rows: self.rows,
            cell_w: self.cell_w,
            cell_h: self.cell_h,
            ascent: self.ascent,
            default_bg: self.default_bg,
        };
        let mut img = render_cells(
            &self.parser,
            &mut self.glyphs,
            &fonts,
            &grid,
            &mut self.fallback_report,
        );
        // For a single-terminal score the pane frame is the whole canvas, so the
        // caption is drawn here. (The stage clears captions from its terminal
        // source and draws them on the composited canvas instead.)
        if let Some(caption) = &mut self.caption {
            let (cap_w, cap_h) = caption_canvas(self.cols, self.cell_w, self.rows, self.cell_h);
            caption.draw(&mut img, cap_w, cap_h, t, &mut self.fallback_report);
        }
        Some(img)
    }

    /// Take the fallback report, leaving an empty one in its place.
    pub fn take_fallback_report(&mut self) -> FallbackReport {
        std::mem::take(&mut self.fallback_report)
    }

    /// The next frame's cell grid, without rasterizing. Feeds exactly the same
    /// `<= t` events as [`FrameSource::next_frame`], so the vector (SVG) path
    /// walks the same states as the gif — minus the fontdue work.
    pub fn next_text_frame(&mut self) -> Option<TextFrame> {
        self.advance()?;
        Some(self.text_frame())
    }

    /// Fast-forward the parser to frame `at` WITHOUT rasterizing, so a poster
    /// can replay straight to the one frame it draws. Monotonic: the vt100
    /// parser cannot rewind (the debug_assert guards a backwards seek), and
    /// `at` is clamped to the last frame. It feeds exactly the same `<= t`
    /// events as [`FrameSource::next_frame`], so the grid afterwards is the one
    /// `next_frame` would render at that index — a following `next_frame` is
    /// therefore idempotent.
    pub fn seek_frame(&mut self, at: usize) {
        let at = at.min(self.n_frames.saturating_sub(1));
        debug_assert!(
            at + 1 >= self.frame || self.frame == 0,
            "seek_frame({at}) rewinds past frame {} — the vt100 parser cannot replay backwards",
            self.frame
        );
        let t = at as f64 * self.dt;
        self.feed_events_upto(t);
        self.frame = at;
    }

    /// Snapshot the current grid as styled text — no fontdue rasterization, so
    /// it costs nothing on the vector path. `fg`/`bg` go through the same
    /// `resolve` logic [`render_cells`] uses, so the poster and the gif can't
    /// drift apart on colour.
    pub fn text_frame(&self) -> TextFrame {
        let screen = self.parser.screen();
        let mut cells = Vec::with_capacity(self.cols * self.rows);
        for row in 0..self.rows {
            for col in 0..self.cols {
                let cell = screen.cell(row as u16, col as u16);
                cells.push(TextCell {
                    ch: cell
                        .and_then(|c| c.contents().chars().next())
                        .unwrap_or(' '),
                    fg: cell
                        .map(|c| resolve(c.fgcolor(), DEFAULT_FG))
                        .unwrap_or(DEFAULT_FG),
                    bg: cell
                        .map(|c| resolve(c.bgcolor(), self.default_bg))
                        .unwrap_or(self.default_bg),
                    bold: cell.is_some_and(vt100::Cell::bold),
                });
            }
        }
        TextFrame {
            cols: self.cols,
            rows: self.rows,
            cell_w: self.cell_w,
            cell_h: self.cell_h,
            px: self.px,
            font_family: self.font_name.clone(),
            default_bg: self.default_bg,
            cells,
        }
    }
}

/// Render every frame, invoking `on_frame` with each RGBA buffer in order.
/// Returns the plan and the fallback report.
pub fn render_frames(
    rec: &Recording,
    score: &Score,
    mut on_frame: impl FnMut(&[u8]),
) -> Result<(Plan, FallbackReport)> {
    let mut source = FrameSource::new(rec, score)?;
    // Bounded: the source yields exactly `n_frames` frames, so walk them by
    // count — the walk ends on its own whatever the per-frame result is.
    let n = source.n_frames();
    for _ in 0..n {
        if let Some(frame) = source.next_frame() {
            on_frame(&frame);
        }
    }
    let report = source.take_fallback_report();
    Ok((plan(rec, score), report))
}

#[cfg(test)]
mod tests;
