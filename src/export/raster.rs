//! Shared frame rasterizer for the pixel targets (gif, mp4, and the multi-scene
//! stage). Replays a recording through a vt100 parser at the score's fps and
//! renders each frame to RGBA with the embedded monospace font. Pure Rust;
//! covers printable ASCII, ANSI colours, and every other glyph the capture
//! actually prints (banners, box-drawing, arrows) as long as the font has it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use fontdue::{Font, Metrics};
use vt100::{Color, Parser};

use super::run::Recording;
use crate::error::Result;
use crate::fonts;
use crate::model::Score;

const DEFAULT_FG: [u8; 3] = [200, 200, 200];

/// Standard xterm 16-colour ANSI palette.
const ANSI16: [[u8; 3]; 16] = [
    [0, 0, 0],
    [205, 0, 0],
    [0, 205, 0],
    [205, 205, 0],
    [0, 0, 238],
    [205, 0, 205],
    [0, 205, 205],
    [229, 229, 229],
    [127, 127, 127],
    [255, 0, 0],
    [0, 255, 0],
    [255, 255, 0],
    [92, 92, 255],
    [255, 0, 255],
    [0, 255, 255],
    [255, 255, 255],
];

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

/// One cell of the text-grid snapshot a poster is drawn from. `fg`/`bg` are
/// already resolved through the same palette logic the raster uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextCell {
    pub ch: char,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub bold: bool,
}

/// The vt100 grid at one point in the replay, with the geometry and colors
/// needed to redraw it as vector text. `cells` is row-major, `cols * rows`.
#[derive(Debug, Clone)]
pub struct TextFrame {
    pub cols: usize,
    pub rows: usize,
    pub cell_w: usize,
    pub cell_h: usize,
    pub px: f32,
    pub font_family: String,
    pub default_bg: [u8; 3],
    pub cells: Vec<TextCell>,
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
        // Cache every non-ASCII glyph the capture actually prints (box-drawing,
        // block art like a banner, arrows, accents, …) so it renders here too —
        // not just the printable-ASCII range. Without this, e.g. a banner drawn
        // with `█`/`░` is invisible on the pixel targets while fine in html.
        for (_, chunk) in &rec.events {
            for ch in chunk.chars() {
                if !ch.is_control() && !ch.is_whitespace() {
                    glyphs.entry(ch).or_insert_with(|| {
                        rasterize_with_fallback(
                            &font,
                            &emoji_font,
                            &last_resort_font,
                            ch,
                            px,
                            &mut fallback_report,
                        )
                    });
                }
            }
        }

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

    /// Render the next frame, or `None` once exhausted.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if self.frame >= self.n_frames {
            return None;
        }
        let t = self.frame as f64 * self.dt;
        while self.ev_idx < self.rec.events.len() && self.rec.events[self.ev_idx].0 <= t {
            self.parser
                .process(self.rec.events[self.ev_idx].1.as_bytes());
            self.ev_idx += 1;
        }
        self.frame += 1;
        let mut img = render_cells(
            &self.parser,
            &mut self.glyphs,
            &self.font,
            &self.emoji_font,
            &self.last_resort_font,
            self.px,
            self.cols,
            self.rows,
            self.cell_w,
            self.cell_h,
            self.ascent,
            self.default_bg,
            &mut self.fallback_report,
        );
        // For a single-terminal score the pane frame is the whole canvas, so the
        // caption is drawn here. (The stage clears captions from its terminal
        // source and draws them on the composited canvas instead.)
        if let Some(caption) = &mut self.caption {
            caption.draw(
                &mut img,
                self.cols * self.cell_w,
                self.rows * self.cell_h,
                t,
                &mut self.fallback_report,
            );
        }
        Some(img)
    }

    /// Take the fallback report, leaving an empty one in its place.
    pub fn take_fallback_report(&mut self) -> FallbackReport {
        std::mem::take(&mut self.fallback_report)
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
        while self.ev_idx < self.rec.events.len() && self.rec.events[self.ev_idx].0 <= t {
            self.parser
                .process(self.rec.events[self.ev_idx].1.as_bytes());
            self.ev_idx += 1;
        }
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
    while let Some(frame) = source.next_frame() {
        on_frame(&frame);
    }
    let report = source.take_fallback_report();
    Ok((plan(rec, score), report))
}

/// Coverage (0–255 per pixel, row-major) for a block-element, box-drawing, or
/// braille glyph drawn procedurally to fill a `w`×`h` cell, or `None` for an
/// ordinary glyph (use the font).
fn solid_cell(ch: char, w: usize, h: usize) -> Option<Vec<u8>> {
    block_cell(ch, w, h)
        .or_else(|| box_cell(ch, w, h))
        .or_else(|| braille_cell(ch, w, h))
}

/// Rasterize `ch` using `primary`, falling back to `emoji` when the primary
/// font lacks the glyph, then to `last_resort` when both before it lack it.
/// Records the outcome in `report`.
fn rasterize_with_fallback(
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

/// Braille patterns (U+2800–U+28FF): a 2-column × 4-row dot matrix encoded in the
/// low 8 bits of the codepoint. The bundled monospace fonts ship **no** braille
/// glyphs (they'd rasterize to `.notdef` tofu), yet braille is exactly how
/// `mapscii` and similar tools draw — so paint the dots procedurally instead.
fn braille_cell(ch: char, w: usize, h: usize) -> Option<Vec<u8>> {
    let cp = ch as u32;
    if !(0x2800..=0x28ff).contains(&cp) {
        return None;
    }
    let bits = (cp - 0x2800) as u8;
    let mut v = vec![0u8; w * h];
    // (col, row) → Unicode dot bit. Left column = dots 1,2,3,7; right = 4,5,6,8.
    let dot_bit = |col: usize, row: usize| -> u8 {
        match (col, row) {
            (0, 0) => 0x01,
            (0, 1) => 0x02,
            (0, 2) => 0x04,
            (0, 3) => 0x40,
            (1, 0) => 0x08,
            (1, 1) => 0x10,
            (1, 2) => 0x20,
            (1, 3) => 0x80,
            _ => 0,
        }
    };
    let sub_w = w as f32 / 2.0;
    let sub_h = h as f32 / 4.0;
    // Dot radius: a fraction of the smaller sub-cell axis, so adjacent dots read
    // as distinct but the pattern still fills densely (min 1px so it never vanishes).
    let r = (sub_w.min(sub_h) * 0.42).max(1.0);
    let r2 = r * r;
    for col in 0..2 {
        for row in 0..4 {
            if bits & dot_bit(col, row) == 0 {
                continue;
            }
            let cx = col as f32 * sub_w + sub_w / 2.0;
            let cy = row as f32 * sub_h + sub_h / 2.0;
            let x0 = (cx - r).floor().max(0.0) as usize;
            let x1 = ((cx + r).ceil() as usize).min(w);
            let y0 = (cy - r).floor().max(0.0) as usize;
            let y1 = ((cy + r).ceil() as usize).min(h);
            for y in y0..y1 {
                for x in x0..x1 {
                    let dx = x as f32 + 0.5 - cx;
                    let dy = y as f32 + 0.5 - cy;
                    if dx * dx + dy * dy <= r2 {
                        v[y * w + x] = 255;
                    }
                }
            }
        }
    }
    Some(v)
}

/// Block elements (U+2580–U+259F): full block, shades, halves, eighths, quadrants.
fn block_cell(ch: char, w: usize, h: usize) -> Option<Vec<u8>> {
    let n = w * h;
    let region = |pred: &dyn Fn(usize, usize) -> bool| {
        let mut v = vec![0u8; n];
        for y in 0..h {
            for x in 0..w {
                if pred(x, y) {
                    v[y * w + x] = 255;
                }
            }
        }
        Some(v)
    };
    match ch {
        '\u{2588}' => Some(vec![255; n]),             // █ full block
        '\u{2591}' => Some(vec![64; n]),              // ░ light shade
        '\u{2592}' => Some(vec![128; n]),             // ▒ medium shade
        '\u{2593}' => Some(vec![192; n]),             // ▓ dark shade
        '\u{2580}' => region(&|_, y| y < h / 2),      // ▀ upper half
        '\u{2584}' => region(&|_, y| y >= h / 2),     // ▄ lower half
        '\u{258C}' => region(&|x, _| x < w / 2),      // ▌ left half
        '\u{2590}' => region(&|x, _| x >= w / 2),     // ▐ right half
        '\u{2594}' => region(&|_, y| y < h / 8),      // ▔ upper one-eighth
        '\u{2595}' => region(&|x, _| x >= w - w / 8), // ▕ right one-eighth
        // Lower 1–7 eighths (▁▂▃▄▅▆▇).
        '\u{2581}'..='\u{2587}' => {
            let fill = h * (ch as usize - 0x2580) / 8;
            region(&move |_, y| y >= h - fill)
        }
        // Left 7–1 eighths (▉▊▋▍▎▏ — ▌ left-half is handled above).
        '\u{2589}'..='\u{258F}' => {
            let fill = w * (0x2590 - ch as usize) / 8;
            region(&move |x, _| x < fill)
        }
        // Quadrant combinations (▖▗▘▙▚▛▜▝▞▟).
        '\u{2596}'..='\u{259F}' => {
            let q = QUADRANTS[ch as usize - 0x2596];
            region(&move |x, y| {
                let (l, t) = (x < w / 2, y < h / 2);
                let bit = match (l, t) {
                    (true, true) => 0b0001,   // top-left
                    (false, true) => 0b0010,  // top-right
                    (true, false) => 0b0100,  // bottom-left
                    (false, false) => 0b1000, // bottom-right
                };
                q & bit != 0
            })
        }
        _ => None,
    }
}

/// Quadrant bitmasks (TL, TR, BL, BR) for U+2596..=U+259F.
const QUADRANTS: [u8; 10] = [
    0b0100, // ▖ BL
    0b1000, // ▗ BR
    0b0001, // ▘ TL
    0b1101, // ▙ TL+BL+BR
    0b1001, // ▚ TL+BR
    0b0111, // ▛ TL+TR+BL
    0b1011, // ▜ TL+TR+BR
    0b0010, // ▝ TR
    0b0110, // ▞ TR+BL
    0b1110, // ▟ TR+BL+BR
];

/// Box-drawing glyphs (U+2500…): draw the present arms from the cell centre.
fn box_cell(ch: char, w: usize, h: usize) -> Option<Vec<u8>> {
    let (up, down, left, right) = box_arms(ch)?;
    let mut v = vec![0u8; w * h];
    let (cx, cy) = (w / 2, h / 2);
    let vt = (w / 6).max(1); // vertical-stroke half-width
    let ht = (h / 10).max(1); // horizontal-stroke half-width
    let (xl, xr) = (cx.saturating_sub(vt), (cx + vt + 1).min(w));
    let (yt, yb) = (cy.saturating_sub(ht), (cy + ht + 1).min(h));
    let mut fill = |x0: usize, x1: usize, y0: usize, y1: usize| {
        for y in y0..y1 {
            for x in x0..x1 {
                v[y * w + x] = 255;
            }
        }
    };
    if up {
        fill(xl, xr, 0, yb);
    }
    if down {
        fill(xl, xr, yt, h);
    }
    if left {
        fill(0, xr, yt, yb);
    }
    if right {
        fill(xl, w, yt, yb);
    }
    Some(v)
}

/// (up, down, left, right) arms for the common light/heavy/rounded box glyphs.
fn box_arms(ch: char) -> Option<(bool, bool, bool, bool)> {
    Some(match ch {
        '─' | '━' => (false, false, true, true),
        '│' | '┃' => (true, true, false, false),
        '┌' | '┏' | '╭' => (false, true, false, true),
        '┐' | '┓' | '╮' => (false, true, true, false),
        '└' | '┗' | '╰' => (true, false, false, true),
        '┘' | '┛' | '╯' => (true, false, true, false),
        '├' | '┣' => (true, true, false, true),
        '┤' | '┫' => (true, true, true, false),
        '┬' | '┳' => (false, true, true, true),
        '┴' | '┻' => (true, false, true, true),
        '┼' | '╋' => (true, true, true, true),
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn render_cells(
    parser: &Parser,
    glyphs: &mut HashMap<char, (Metrics, Vec<u8>)>,
    font: &Font,
    emoji_font: &Font,
    last_resort_font: &Font,
    px: f32,
    cols: usize,
    rows: usize,
    cell_w: usize,
    cell_h: usize,
    ascent: f32,
    default_bg: [u8; 3],
    fallback_report: &mut FallbackReport,
) -> Vec<u8> {
    let (w, h) = (cols * cell_w, rows * cell_h);
    let mut img = vec![0u8; w * h * 4];
    let screen = parser.screen();

    for row in 0..rows {
        for col in 0..cols {
            let cell = screen.cell(row as u16, col as u16);
            let bg = cell
                .map(|c| resolve(c.bgcolor(), default_bg))
                .unwrap_or(default_bg);
            let fg = cell
                .map(|c| resolve(c.fgcolor(), DEFAULT_FG))
                .unwrap_or(DEFAULT_FG);

            let (x0, y0) = (col * cell_w, row * cell_h);
            for y in 0..cell_h {
                let base = ((y0 + y) * w + x0) * 4;
                for x in 0..cell_w {
                    let p = base + x * 4;
                    img[p] = bg[0];
                    img[p + 1] = bg[1];
                    img[p + 2] = bg[2];
                    img[p + 3] = 255;
                }
            }

            let Some(chr) = cell.and_then(|c| c.contents().chars().next()) else {
                continue;
            };
            // Block & box-drawing glyphs are drawn procedurally to FILL the cell,
            // so banners and TUI frames render as solid, continuous shapes — the
            // font glyph leaves gaps between cells.
            if let Some(cov) = solid_cell(chr, cell_w, cell_h) {
                for y in 0..cell_h {
                    for x in 0..cell_w {
                        let a = cov[y * cell_w + x] as u32;
                        if a == 0 {
                            continue;
                        }
                        let p = ((y0 + y) * w + x0 + x) * 4;
                        for k in 0..3 {
                            let dst = img[p + k] as u32;
                            let src = fg[k] as u32;
                            img[p + k] = ((src * a + dst * (255 - a)) / 255) as u8;
                        }
                    }
                }
                continue;
            }
            // On-demand rasterization: if the glyph isn't cached yet, rasterize
            // it now so any Unicode character the font supports renders correctly.
            let (m, cov) = glyphs.entry(chr).or_insert_with(|| {
                rasterize_with_fallback(
                    font,
                    emoji_font,
                    last_resort_font,
                    chr,
                    px,
                    fallback_report,
                )
            });
            if m.width == 0 || m.height == 0 {
                continue;
            }
            let ox = x0 as i32 + ((cell_w as i32 - m.width as i32) / 2).max(0);
            let top = y0 as i32 + (ascent.round() as i32) - (m.height as i32 + m.ymin);
            for gy in 0..m.height {
                let py = top + gy as i32;
                if py < 0 || py as usize >= h {
                    continue;
                }
                for gx in 0..m.width {
                    let pxc = ox + gx as i32;
                    if pxc < 0 || pxc as usize >= w {
                        continue;
                    }
                    let a = cov[gy * m.width + gx] as u32;
                    if a == 0 {
                        continue;
                    }
                    let p = (py as usize * w + pxc as usize) * 4;
                    for k in 0..3 {
                        let dst = img[p + k] as u32;
                        let src = fg[k] as u32;
                        img[p + k] = ((src * a + dst * (255 - a)) / 255) as u8;
                    }
                }
            }
        }
    }
    img
}

/// Map a vt100 colour to RGB.
fn resolve(c: Color, default: [u8; 3]) -> [u8; 3] {
    match c {
        Color::Default => default,
        Color::Idx(i) if (i as usize) < 16 => ANSI16[i as usize],
        Color::Idx(i) => xterm256(i),
        Color::Rgb(r, g, b) => [r, g, b],
    }
}

/// xterm 256-colour cube + grayscale ramp (indices 16..=255).
fn xterm256(i: u8) -> [u8; 3] {
    if i < 16 {
        return ANSI16[i as usize];
    }
    if i >= 232 {
        let v = 8 + (i as u16 - 232) * 10;
        return [v as u8, v as u8, v as u8];
    }
    let i = i as u16 - 16;
    let lvl = |c: u16| if c == 0 { 0u8 } else { (55 + c * 40) as u8 };
    [lvl(i / 36), lvl((i % 36) / 6), lvl(i % 6)]
}

/// Parse a `#rrggbb` colour.
pub fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(s, 16).ok()?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

/// Alpha-blend one rasterized glyph (`cov` coverage, `m` metrics) in `fg` onto
/// the RGBA `img` at `(ox, top)`, clipped to the canvas.
#[allow(clippy::too_many_arguments)]
fn blit_glyph(
    img: &mut [u8],
    w: usize,
    h: usize,
    ox: i32,
    top: i32,
    m: &Metrics,
    cov: &[u8],
    fg: [u8; 3],
) {
    for gy in 0..m.height {
        let py = top + gy as i32;
        if py < 0 || py as usize >= h {
            continue;
        }
        for gx in 0..m.width {
            let pxc = ox + gx as i32;
            if pxc < 0 || pxc as usize >= w {
                continue;
            }
            let a = cov[gy * m.width + gx] as u32;
            if a == 0 {
                continue;
            }
            let p = (py as usize * w + pxc as usize) * 4;
            for k in 0..3 {
                let dst = img[p + k] as u32;
                img[p + k] = ((fg[k] as u32 * a + dst * (255 - a)) / 255) as u8;
            }
        }
    }
}

/// An on-canvas caption track: a bottom bar with centered text, switching to the
/// latest caption that is active at the current time.
pub struct CaptionOverlay {
    captions: Vec<(f64, String)>,
    font: Font,
    emoji_font: Font,
    last_resort_font: Font,
    glyphs: HashMap<char, (Metrics, Vec<u8>)>,
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
            blit_glyph(img, w, h, ox, top, m, cov, fg);
            cx += self.cell_w;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse_hex("#0b0f14"), Some([11, 15, 20]));
        assert_eq!(parse_hex("ffffff"), Some([255, 255, 255]));
        assert_eq!(parse_hex("bad"), None);
    }

    #[test]
    fn resolves_ansi_and_rgb() {
        assert_eq!(resolve(Color::Idx(1), DEFAULT_FG), ANSI16[1]);
        assert_eq!(resolve(Color::Rgb(10, 20, 30), DEFAULT_FG), [10, 20, 30]);
        assert_eq!(resolve(Color::Default, [1, 2, 3]), [1, 2, 3]);
    }

    #[test]
    fn xterm_cube_endpoints() {
        assert_eq!(xterm256(16), [0, 0, 0]);
        assert_eq!(xterm256(231), [255, 255, 255]);
        assert_eq!(xterm256(232), [8, 8, 8]);
    }

    #[test]
    fn braille_dots_render_procedurally() {
        // The bundled fonts have no braille glyphs, so these must come from
        // solid_cell (procedural), not the font — and each pattern must differ.
        let cov = |ch: char| solid_cell(ch, 10, 20).map(|v| v.iter().filter(|&&p| p > 0).count());
        // Blank braille → an empty (space-like) cell, but still handled here.
        assert_eq!(cov('\u{2800}'), Some(0));
        // A single dot has far less ink than the full 8-dot pattern.
        let one = cov('⠁').expect("single dot handled");
        let full = cov('⣿').expect("full pattern handled");
        assert!(one > 0, "single braille dot must draw ink");
        assert!(
            full > one * 4,
            "8-dot braille must be much denser than 1-dot: {full} vs {one}"
        );
        // A non-braille char falls through to the font (None here).
        assert_eq!(solid_cell('a', 10, 20), None);
    }

    #[test]
    fn caption_overlay_picks_latest_nonempty() {
        let c = CaptionOverlay::new(
            vec![
                (0.0, "step 1".into()),
                (1.0, "step 2".into()),
                (2.0, String::new()),
            ],
            18.0,
            fonts::DEFAULT_FONT,
            fonts::load_emoji(),
            fonts::load_last_resort(),
        )
        .unwrap();
        assert_eq!(c.active(0.0), Some("step 1"));
        assert_eq!(c.active(0.9), Some("step 1"));
        assert_eq!(c.active(1.5), Some("step 2"));
        assert_eq!(c.active(2.5), None); // cleared by the empty caption
    }

    #[test]
    fn box_arms_returns_none_for_non_box() {
        assert_eq!(box_arms('a'), None);
        assert_eq!(box_arms(' '), None);
    }

    #[test]
    fn box_arms_known_glyphs() {
        assert_eq!(box_arms('─'), Some((false, false, true, true)));
        assert_eq!(box_arms('│'), Some((true, true, false, false)));
        assert_eq!(box_arms('┌'), Some((false, true, false, true)));
        assert_eq!(box_arms('┐'), Some((false, true, true, false)));
        assert_eq!(box_arms('└'), Some((true, false, false, true)));
        assert_eq!(box_arms('┘'), Some((true, false, true, false)));
        assert_eq!(box_arms('┼'), Some((true, true, true, true)));
        assert_eq!(box_arms('├'), Some((true, true, false, true)));
        assert_eq!(box_arms('┤'), Some((true, true, true, false)));
        assert_eq!(box_arms('┬'), Some((false, true, true, true)));
        assert_eq!(box_arms('┴'), Some((true, false, true, true)));
    }

    #[test]
    fn box_cell_produces_nonempty_for_line_glyphs() {
        let v = box_cell('─', 20, 10).unwrap();
        assert!(v.iter().any(|&p| p > 0));
        let v = box_cell('│', 20, 10).unwrap();
        assert!(v.iter().any(|&p| p > 0));
        assert!(box_cell('a', 10, 10).is_none());
    }

    #[test]
    fn block_cell_covers_all_special_chars() {
        // Full block
        let v = block_cell('█', 4, 4).unwrap();
        assert!(v.iter().all(|&p| p == 255));
        // Light shade
        let v = block_cell('░', 4, 4).unwrap();
        assert!(v.iter().all(|&p| p == 64));
        // Medium shade
        let v = block_cell('▒', 4, 4).unwrap();
        assert!(v.iter().all(|&p| p == 128));
        // Dark shade
        let v = block_cell('▓', 4, 4).unwrap();
        assert!(v.iter().all(|&p| p == 192));
        // Upper half
        let v = block_cell('▀', 4, 4).unwrap();
        assert!(v[0] == 255);
        assert!(v[4 * 4 - 1] == 0);
        // Lower half
        let v = block_cell('▄', 4, 4).unwrap();
        assert!(v[0] == 0);
        assert!(v[4 * 4 - 1] == 255);
        // Non-block char
        assert!(block_cell('z', 4, 4).is_none());
    }

    #[test]
    fn solid_cell_delegates_to_correct_handler() {
        assert!(solid_cell('█', 4, 4).is_some());
        assert!(solid_cell('─', 4, 4).is_some());
        assert!(solid_cell('⠁', 4, 8).is_some());
        assert!(solid_cell('a', 4, 4).is_none());
    }

    #[test]
    fn xterm256_greyscale_ramp() {
        // Greyscale: 232..=255 → v = 8 + (i - 232) * 10
        assert_eq!(xterm256(232), [8, 8, 8]);
        assert_eq!(xterm256(255), [238, 238, 238]);
    }

    #[test]
    fn xterm256_index_below_16_uses_ansi16() {
        assert_eq!(xterm256(0), ANSI16[0]);
        assert_eq!(xterm256(7), ANSI16[7]);
    }

    #[test]
    fn resolve_ansi_high_index() {
        // Index >= 16 goes through xterm256
        let c = resolve(Color::Idx(240), DEFAULT_FG);
        assert_eq!(c[0], c[1]); // greyscale = equal channels
    }

    #[test]
    fn caption_active_empty_before_first() {
        let c = CaptionOverlay::new(
            vec![(1.0, "hello".into())],
            18.0,
            fonts::DEFAULT_FONT,
            fonts::load_emoji(),
            fonts::load_last_resort(),
        )
        .unwrap();
        assert_eq!(c.active(0.5), None);
        assert_eq!(c.active(1.0), Some("hello"));
    }

    #[test]
    fn caption_active_empty_list() {
        let c = CaptionOverlay::new(
            vec![],
            18.0,
            fonts::DEFAULT_FONT,
            fonts::load_emoji(),
            fonts::load_last_resort(),
        )
        .unwrap();
        assert_eq!(c.active(0.0), None);
    }

    #[test]
    fn parse_hex_with_hash_prefix() {
        assert_eq!(parse_hex("#ff0000"), Some([255, 0, 0]));
    }

    #[test]
    fn parse_hex_with_whitespace() {
        assert_eq!(parse_hex("  00ff00  "), Some([0, 255, 0]));
    }

    #[test]
    fn parse_hex_too_short() {
        assert_eq!(parse_hex("abc"), None);
    }

    #[test]
    fn parse_hex_too_long() {
        assert_eq!(parse_hex("aabbccdd"), None);
    }

    #[test]
    fn parse_hex_invalid_hex() {
        assert_eq!(parse_hex("zzzzzz"), None);
    }

    #[test]
    fn cell_size_from_font_size() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  font_size = 20
"#,
        )
        .unwrap();
        let (cw, ch) = cell_size(&score);
        assert_eq!(cw, 12); // 20 * 0.6 = 12
        assert_eq!(ch, 24); // 20 * 1.2 = 24
    }

    #[test]
    fn cell_size_default_font() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
"#,
        )
        .unwrap();
        let (cw, ch) = cell_size(&score);
        // Default px=16: cw=16*0.6=10, ch=16*1.2=19
        assert_eq!(cw, 10);
        assert_eq!(ch, 19);
    }

    #[test]
    fn cell_size_minimum_one() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
line_height = 0.3
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  font_size = 1
"#,
        )
        .unwrap();
        let (cw, ch) = cell_size(&score);
        assert!(cw >= 1);
        assert!(ch >= 1);
    }

    #[test]
    fn plan_computes_pixel_dimensions() {
        use crate::export::run::Recording;
        let rec = Recording {
            cols: 80,
            rows: 24,
            title: "t".into(),
            events: vec![],
            captions: vec![],
            focuses: vec![],
            duration: 0.0,
        };
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 800
height = 480
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 480
  font_size = 20
"#,
        )
        .unwrap();
        let p = plan(&rec, &score);
        assert_eq!(p.width, 80 * 12); // 80 cols * 12px cell_w
        assert_eq!(p.height, 24 * 24); // 24 rows * 24px cell_h
        assert_eq!(p.fps, 15);
    }

    #[test]
    fn plan_minimum_fps_is_one() {
        use crate::export::run::Recording;
        let rec = Recording {
            cols: 80,
            rows: 24,
            title: "t".into(),
            events: vec![],
            captions: vec![],
            focuses: vec![],
            duration: 0.0,
        };
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
fps = 0
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
"#,
        )
        .unwrap();
        let p = plan(&rec, &score);
        assert!(p.fps >= 1);
    }

    /// A caption drawn with a font that lacks the character must still render it
    /// through the fallback chain AND report it. This is the caption-side twin of
    /// the terminal path: a fixed symbol table pre-cached in `new` used to fill the
    /// map behind `entry().or_insert_with()`, so `✗` came out blank and silent.
    #[test]
    fn caption_falls_back_and_reports_a_char_the_primary_font_lacks() {
        assert!(
            !fonts::load("IBM Plex Mono").has_glyph('\u{2717}'),
            "premise: IBM Plex Mono has no U+2717"
        );
        let mut c = CaptionOverlay::new(
            vec![(0.0, "\u{2717}".into())],
            18.0,
            "IBM Plex Mono",
            fonts::load_emoji(),
            fonts::load_last_resort(),
        )
        .unwrap();
        let (w, h) = (200, 100);
        let mut img = vec![0u8; w * h * 4];
        let mut report = FallbackReport::with_primary_name("IBM Plex Mono");
        c.draw(&mut img, w, h, 0.0, &mut report);

        assert!(!report.is_empty(), "the fallback went unreported");
        let lines = report.format("demo");
        assert!(
            lines.iter().any(|l| l.contains('\u{2717}')),
            "report does not name the character: {lines:?}"
        );
        let (m, cov) = c.glyphs.get(&'\u{2717}').expect("glyph was never cached");
        assert!(m.width > 0 && !cov.is_empty(), "rendered blank");
    }

    #[test]
    fn caption_draw_does_not_panic_on_small_image() {
        let mut c = CaptionOverlay::new(
            vec![(0.0, "test".into())],
            18.0,
            fonts::DEFAULT_FONT,
            fonts::load_emoji(),
            fonts::load_last_resort(),
        )
        .unwrap();
        // Very small image — bar_h > h, so draw is a no-op
        let mut img = vec![0u8; 10 * 10 * 4];
        let mut report = FallbackReport::new();
        c.draw(&mut img, 10, 10, 0.5, &mut report);
    }

    #[test]
    fn caption_draw_renders_text() {
        let mut c = CaptionOverlay::new(
            vec![(0.0, "A".into())],
            18.0,
            fonts::DEFAULT_FONT,
            fonts::load_emoji(),
            fonts::load_last_resort(),
        )
        .unwrap();
        let w = 200;
        let h = 100;
        let mut img = vec![0u8; w * h * 4];
        let mut report = FallbackReport::new();
        c.draw(&mut img, w, h, 0.5, &mut report);
        // Some pixels should have been modified (the caption bar darkens existing pixels)
        assert!(img.iter().any(|&p| p != 0));
    }

    #[test]
    fn caption_draw_empty_text_is_noop() {
        let mut c = CaptionOverlay::new(
            vec![(0.0, String::new())],
            18.0,
            fonts::DEFAULT_FONT,
            fonts::load_emoji(),
            fonts::load_last_resort(),
        )
        .unwrap();
        let mut img = vec![128u8; 100 * 50 * 4];
        let before = img.clone();
        let mut report = FallbackReport::new();
        c.draw(&mut img, 100, 50, 0.5, &mut report);
        assert_eq!(img, before);
    }

    #[test]
    fn block_cell_eighths() {
        // Upper one-eighth
        let v = block_cell('▔', 8, 8).unwrap();
        assert!(v[0] == 255); // top row filled
        assert!(v[7 * 8] == 0); // bottom row empty
                                // Lower one-eighth
        let v = block_cell('▁', 8, 8).unwrap();
        assert!(v[0] == 0);
        assert!(v[7 * 8] == 255);
    }

    #[test]
    fn block_cell_left_right_eighths() {
        // Left half
        let v = block_cell('▌', 8, 8).unwrap();
        assert!(v[0] == 255);
        assert!(v[4] == 0);
        // Right half
        let v = block_cell('▐', 8, 8).unwrap();
        assert!(v[0] == 0);
        assert!(v[4] == 255);
    }

    #[test]
    fn block_cell_quadrants() {
        // ▖ (BL quadrant): rows 2-3, cols 0-1 filled
        let v = block_cell('▖', 4, 4).unwrap();
        assert!(v[0] == 0); // row0 col0: empty
        assert!(v[2 * 4] == 255); // row2 col0: filled
        assert!(v[2 * 4 + 3] == 0); // row2 col3: empty
    }

    // ── resolve / xterm256 ───────────────────────────────────────────

    #[test]
    fn resolve_default_color() {
        assert_eq!(resolve(Color::Default, [10, 20, 30]), [10, 20, 30]);
    }

    #[test]
    fn resolve_ansi16_colors() {
        let c = resolve(Color::Idx(0), [0, 0, 0]);
        assert_eq!(c, ANSI16[0]);
        let c = resolve(Color::Idx(1), [0, 0, 0]);
        assert_eq!(c, ANSI16[1]);
    }

    #[test]
    fn resolve_rgb_color() {
        assert_eq!(
            resolve(Color::Rgb(100, 150, 200), [0, 0, 0]),
            [100, 150, 200]
        );
    }

    #[test]
    fn xterm256_grayscale_ramp() {
        let c = xterm256(232);
        assert_eq!(c[0], c[1]);
        assert_eq!(c[1], c[2]);
        assert!(c[0] > 0);
        // Max grayscale is 8 + (255-232)*10 = 238
        let c = xterm256(255);
        assert_eq!(c[0], 238);
    }

    #[test]
    fn xterm256_color_cube() {
        // Index 16 = R=0 G=0 B=0 (lvl(0)=0)
        let c = xterm256(16);
        assert_eq!(c, [0, 0, 0]);
        // Index 19 = R=0 G=0 B=lvl(3)=55+120=175
        let c = xterm256(19);
        assert_eq!(c, [0, 0, 175]);
        // Index 51 = i=35: R=lvl(0)=0 G=lvl(5)=255 B=lvl(5)=255
        let c = xterm256(51);
        assert_eq!(c, [0, 255, 255]);
    }

    #[test]
    fn parse_hex_valid() {
        assert_eq!(parse_hex("#ff0000"), Some([255, 0, 0]));
        assert_eq!(parse_hex("00ff00"), Some([0, 255, 0]));
        assert_eq!(parse_hex("  #0000ff  "), Some([0, 0, 255]));
    }

    #[test]
    fn parse_hex_invalid() {
        assert_eq!(parse_hex("xyz"), None);
        assert_eq!(parse_hex("12345"), None);
        assert_eq!(parse_hex("#1234567"), None);
    }

    #[test]
    fn braille_cell_basic() {
        // Braille character U+2800 (empty) → all dots empty
        let v = braille_cell('\u{2800}', 6, 8).unwrap();
        assert!(v.iter().all(|&p| p == 0));
    }

    #[test]
    fn braille_cell_dot1_fills_some_pixels() {
        // U+2801 = dot 1 (col 0, row 0)
        let v = braille_cell('\u{2801}', 6, 8).unwrap();
        assert!(v.iter().any(|&p| p > 0), "dot 1 should fill some pixels");
    }

    #[test]
    fn braille_cell_all_dots_fills_many() {
        // U+28FF = all 8 dots
        let v = braille_cell('\u{28FF}', 6, 8).unwrap();
        let filled = v.iter().filter(|&&p| p > 0).count();
        assert!(filled > 10, "all dots should fill many pixels");
    }

    #[test]
    fn braille_cell_outside_range() {
        assert!(braille_cell('A', 4, 6).is_none());
        assert!(braille_cell('\u{2900}', 4, 6).is_none());
    }

    #[test]
    fn solid_cell_fills_all() {
        let v = solid_cell('█', 4, 4).unwrap();
        assert!(v.iter().all(|&p| p == 255));
    }

    #[test]
    fn solid_cell_none_for_printable_text() {
        // Regular characters are not block/box/braille
        assert!(solid_cell('A', 4, 4).is_none());
    }

    // ── blit_glyph ────────────────────────────────────────────────

    #[test]
    fn blit_glyph_draws_within_bounds() {
        let mut img = vec![0u8; 20 * 20 * 4];
        let m = fontdue::Metrics {
            width: 8,
            height: 12,
            xmin: 0,
            ymin: -2,
            advance_width: 8.0,
            advance_height: 0.0,
            bounds: fontdue::OutlineBounds {
                xmin: 0.0,
                ymin: -2.0,
                width: 8.0,
                height: 12.0,
            },
        };
        let cov = vec![255u8; 8 * 12];
        blit_glyph(&mut img, 20, 20, 2, 4, &m, &cov, [255, 0, 0]);
        // Some pixels should be red now
        let mut found = false;
        for y in 0..20 {
            for x in 0..20 {
                let p = (y * 20 + x) * 4;
                if img[p] == 255 && img[p + 1] == 0 && img[p + 2] == 0 {
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
        }
        assert!(found, "blit_glyph should draw red pixels");
    }

    #[test]
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn blit_glyph_clips_at_top() {
        let mut img_clipped = vec![0u8; 20 * 20 * 4];
        let mut img_full = vec![0u8; 20 * 20 * 4];
        let m = fontdue::Metrics {
            width: 4,
            height: 8,
            xmin: 0,
            ymin: 0,
            advance_width: 4.0,
            advance_height: 0.0,
            bounds: fontdue::OutlineBounds {
                xmin: 0.0,
                ymin: 0.0,
                width: 4.0,
                height: 8.0,
            },
        };
        let cov = vec![255u8; 4 * 8];
        // top = -4 clips the top half of the glyph
        blit_glyph(&mut img_clipped, 20, 20, 0, -4, &m, &cov, [0, 255, 0]);
        // top = 0 draws the full glyph
        blit_glyph(&mut img_full, 20, 20, 0, 0, &m, &cov, [0, 255, 0]);
        let clipped_pixels: usize = img_clipped
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[1] == 255)
            .count();
        let full_pixels: usize = img_full
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[1] == 255)
            .count();
        assert!(
            clipped_pixels < full_pixels,
            "clipped glyph should have fewer green pixels"
        );
    }

    #[test]
    fn blit_glyph_clips_at_right() {
        let mut img = vec![0u8; 10 * 10 * 4];
        let m = fontdue::Metrics {
            width: 8,
            height: 4,
            xmin: 0,
            ymin: 0,
            advance_width: 8.0,
            advance_height: 0.0,
            bounds: fontdue::OutlineBounds {
                xmin: 0.0,
                ymin: 0.0,
                width: 8.0,
                height: 4.0,
            },
        };
        let cov = vec![255u8; 8 * 4];
        // ox = 6, so pixels 6..14 would go out of bounds (w=10)
        blit_glyph(&mut img, 10, 10, 6, 0, &m, &cov, [0, 0, 255]);
        // Column 9 should have blue, columns 10+ should be untouched
        for y in 0..4 {
            let p = (y * 10 + 9) * 4;
            assert_eq!(img[p + 2], 255, "rightmost column should be blue");
        }
    }

    #[test]
    fn blit_glyph_zero_coverage_does_nothing() {
        let mut img = vec![128u8; 10 * 10 * 4];
        let m = fontdue::Metrics {
            width: 4,
            height: 4,
            xmin: 0,
            ymin: 0,
            advance_width: 4.0,
            advance_height: 0.0,
            bounds: fontdue::OutlineBounds {
                xmin: 0.0,
                ymin: 0.0,
                width: 4.0,
                height: 4.0,
            },
        };
        let cov = vec![0u8; 4 * 4]; // all zeros
        blit_glyph(&mut img, 10, 10, 0, 0, &m, &cov, [255, 255, 255]);
        // Nothing should change
        assert!(img.iter().all(|&p| p == 128));
    }

    // ── block_cell additional branches ─────────────────────────────

    #[test]
    fn block_cell_left_eighths() {
        // ▉ (left 7 eighths)
        let v = block_cell('▉', 8, 8).unwrap();
        let filled_cols: usize = (0..8).filter(|&x| v[x] == 255).count();
        assert!(filled_cols >= 6, "▉ should fill most of the width");
    }

    #[test]
    fn block_cell_right_eighths() {
        // ▏ (U+258F, left one eighth): fill = w * (0x2590 - 0x258F) / 8 = w * 1 / 8
        let v = block_cell('▏', 8, 8).unwrap();
        assert!(v[0] == 255, "leftmost col should be filled");
        assert!(v[7] == 0, "rightmost col should be empty");
    }

    // ── render_cells ───────────────────────────────────────────────

    #[test]
    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn render_cells_empty_screen() {
        let rec = Recording {
            cols: 4,
            rows: 2,
            title: "t".into(),
            events: vec![],
            captions: vec![],
            focuses: vec![],
            duration: 0.0,
        };
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
"#,
        )
        .unwrap();
        let mut source = FrameSource::new(&rec, &score).unwrap();
        let frame = source.next_frame().unwrap();
        // Should produce a valid RGBA buffer
        assert_eq!(frame.len(), 4 * 10 * 2 * 19 * 4);
        // All pixels should be the default background color
        for px in frame.as_chunks::<4>().0 {
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn render_cells_with_text() {
        let rec = Recording {
            cols: 10,
            rows: 2,
            title: "t".into(),
            events: vec![(0.0, "Hello".into())],
            captions: vec![],
            focuses: vec![],
            duration: 0.1,
        };
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
"#,
        )
        .unwrap();
        let mut source = FrameSource::new(&rec, &score).unwrap();
        let frame = source.next_frame().unwrap();
        // Should produce a frame
        assert!(!frame.is_empty());
    }

    // ── box_cell additional variants ───────────────────────────────

    #[test]
    fn box_cell_all_variants() {
        let glyphs = [
            '─', '━', '│', '┃', '┌', '┏', '┐', '┓', '└', '┗', '┘', '┛', '├', '┣', '┤', '┫', '┬',
            '┳', '┴', '┻', '┼', '╋',
        ];
        for ch in glyphs {
            let v = box_cell(ch, 16, 10).unwrap();
            assert!(
                v.iter().any(|&p| p > 0),
                "box glyph {ch} should draw something"
            );
        }
    }

    // ── block_cell quadrant variants ───────────────────────────────

    #[test]
    fn block_cell_all_quadrants() {
        let quadrants = ['▖', '▗', '▘', '▙', '▚', '▛', '▜', '▝', '▞', '▟'];
        for ch in quadrants {
            let v = block_cell(ch, 4, 4).unwrap();
            assert!(
                v.iter().any(|&p| p > 0),
                "quadrant {ch} should have filled pixels"
            );
        }
    }

    // ── braille_cell specific dot patterns ─────────────────────────

    #[test]
    fn braille_cell_dot4_fills_right_column() {
        // U+2808 = dot 4 (col 1, row 0)
        let v = braille_cell('\u{2808}', 6, 8).unwrap();
        // Right side should have some filled pixels
        let right_filled: usize = (0..8).filter(|&y| (3..6).any(|x| v[y * 6 + x] > 0)).count();
        assert!(right_filled > 0, "dot 4 should fill right column");
    }

    #[test]
    fn braille_cell_all_dots_pattern() {
        // U+28FF = all 8 dots
        let v = braille_cell('\u{28FF}', 10, 16).unwrap();
        let filled = v.iter().filter(|&&p| p > 0).count();
        assert!(filled > 30, "all dots should fill many pixels");
    }

    // ── Fallback chain tests ───────────────────────────────────────

    #[test]
    fn fallback_chain_three_level_coverage() {
        let ibm_plex = fonts::load("IBM Plex Mono");
        let emoji = fonts::load_emoji();
        let dejavu = fonts::load_last_resort();
        let px = 16.0;

        assert!(!ibm_plex.has_glyph('✗'), "✗ must not be in IBM Plex Mono");

        let mut report = FallbackReport::new();
        let (m, _) = rasterize_with_fallback(&ibm_plex, &emoji, &dejavu, '✗', px, &mut report);
        assert!(m.width > 0, "✗ must be non-zero after the change");
        assert_eq!(
            report.fallen_back.get(&'✗'),
            Some(&"DejaVu Sans Mono"),
            "✗ must fall back to DejaVu Sans Mono"
        );

        let mut report = FallbackReport::new();
        let (m, _) = rasterize_with_fallback(&ibm_plex, &emoji, &dejavu, '😀', px, &mut report);
        assert!(m.width > 0, "😀 must be non-zero");
        assert_eq!(
            report.fallen_back.get(&'😀'),
            Some(&"Noto Emoji"),
            "😀 must come from the emoji font"
        );

        let mut report = FallbackReport::new();
        let (m, _) = rasterize_with_fallback(&ibm_plex, &emoji, &dejavu, 'A', px, &mut report);
        assert!(m.width > 0, "A must be non-zero");
        assert!(
            !report.fallen_back.contains_key(&'A'),
            "A must never appear in fallen_back"
        );
        assert!(
            !report.unresolved.contains(&'A'),
            "A must not be unresolved"
        );
    }

    #[test]
    fn fallback_chain_primary_wins_over_dejavu() {
        // A character present in both IBM Plex Mono and DejaVu should be drawn
        // by the primary font (IBM Plex Mono), not DejaVu.
        let ibm_plex = fonts::load("IBM Plex Mono");
        let emoji = fonts::load_emoji();
        let dejavu = fonts::load_last_resort();
        let px = 16.0;

        // '→' (U+2192) is present in both IBM Plex Mono and DejaVu
        let (m_primary, _) = ibm_plex.rasterize('→', px);
        assert!(m_primary.width > 0, "→ should be in IBM Plex Mono");

        let (m_dejavu, _) = dejavu.rasterize('→', px);
        assert!(m_dejavu.width > 0, "→ should be in DejaVu");

        // The fallback chain should use the primary font
        let mut report = FallbackReport::new();
        let (m, _) = rasterize_with_fallback(&ibm_plex, &emoji, &dejavu, '→', px, &mut report);
        assert!(m.width > 0, "→ should be non-zero");
        assert!(
            !report.fallen_back.contains_key(&'→'),
            "→ should not fall back (primary has it)"
        );
    }

    #[test]
    fn fallback_chain_unresolved_character() {
        let ibm_plex = fonts::load("IBM Plex Mono");
        let emoji = fonts::load_emoji();
        let dejavu = fonts::load_last_resort();
        let px = 16.0;

        let ch = '\u{1D518}';

        assert!(
            !ibm_plex.has_glyph(ch),
            "precondition: primary must lack this glyph"
        );
        assert!(
            !emoji.has_glyph(ch),
            "precondition: emoji must lack this glyph"
        );
        assert!(
            !dejavu.has_glyph(ch),
            "precondition: dejavu must lack this glyph"
        );

        let mut report = FallbackReport::new();
        let (m, _) = rasterize_with_fallback(&ibm_plex, &emoji, &dejavu, ch, px, &mut report);
        assert_eq!(
            m.width, 0,
            "all three fonts lack this glyph, so output must be zero-width"
        );
        assert!(
            report.unresolved.contains(&ch),
            "character absent from all three must land in unresolved"
        );
        assert!(
            !report.fallen_back.contains_key(&ch),
            "unresolved character must not appear in fallen_back"
        );
    }

    #[test]
    fn fallback_report_excludes_braille() {
        // Braille characters are handled procedurally and should never appear
        // in the fallback report.
        let ibm_plex = fonts::load("IBM Plex Mono");
        let emoji = fonts::load_emoji();
        let dejavu = fonts::load_last_resort();
        let px = 16.0;

        let mut report = FallbackReport::new();
        // Braille patterns are procedural, but if they went through the font path
        // they should not be recorded (the report filters them out).
        let _ = rasterize_with_fallback(&ibm_plex, &emoji, &dejavu, '⠁', px, &mut report);
        assert!(
            !report.fallen_back.contains_key(&'⠁'),
            "braille should not be in fallen_back"
        );
        assert!(
            !report.unresolved.contains(&'⠁'),
            "braille should not be in unresolved"
        );
    }

    #[test]
    fn fallback_report_format_names_actual_primary_font() {
        let mut report = FallbackReport::with_primary_name("IBM Plex Mono");
        report.record_fallback('✗', "DejaVu Sans Mono");
        report.record_fallback('▸', "DejaVu Sans Mono");
        let lines = report.format("demo");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("not in IBM Plex Mono"),
            "format must name the actual primary font, got: {}",
            lines[0]
        );
        assert!(
            !lines[0].contains("primary font,"),
            "format must not contain the literal 'primary font', got: {}",
            lines[0]
        );
    }

    // ── seekable text frames (the vector/poster path) ──────────────

    /// A single full-canvas terminal score at the given fps, for the text-frame
    /// tests below.
    fn text_score(fps: u32) -> Score {
        toml::from_str(&format!(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
fps = {fps}
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
"#
        ))
        .unwrap()
    }

    fn text_rec(events: Vec<(f64, String)>) -> Recording {
        Recording {
            cols: 8,
            rows: 2,
            title: "t".into(),
            events,
            captions: vec![],
            focuses: vec![],
            duration: 1.0,
        }
    }

    #[test]
    fn seeking_forward_renders_the_same_frame_as_playing_through() {
        let rec = text_rec(vec![
            (0.0, "one".into()),
            (0.2, " two".into()),
            (0.5, "\r\nnext".into()),
        ]);
        let score = text_score(10);

        // A: jump straight to frame 3, then render it.
        let mut seeked = FrameSource::new(&rec, &score).unwrap();
        seeked.seek_frame(3);
        let sought = seeked.next_frame().unwrap();

        // B: play the first four frames and keep the last.
        let mut played = FrameSource::new(&rec, &score).unwrap();
        let mut played_rgba = Vec::new();
        for _ in 0..4 {
            played_rgba = played.next_frame().unwrap();
        }

        assert_eq!(sought, played_rgba, "seeking must replay the same frame");
    }

    #[test]
    fn text_frame_exposes_colors_and_bold_per_cell() {
        let rec = text_rec(vec![(0.0, "\x1b[1mH\x1b[0m\x1b[31mi".into())]);
        let score = text_score(10);
        let mut source = FrameSource::new(&rec, &score).unwrap();
        source.seek_frame(0);
        let tf = source.text_frame();

        assert_eq!(tf.cols, 8);
        assert_eq!(tf.cells.len(), 8 * 2);
        assert_eq!(
            tf.cells[0],
            TextCell {
                ch: 'H',
                fg: DEFAULT_FG,
                bg: tf.default_bg,
                bold: true,
            }
        );
        assert_eq!(
            tf.cells[1],
            TextCell {
                ch: 'i',
                fg: ANSI16[1],
                bg: tf.default_bg,
                bold: false,
            }
        );
        // Untouched cells are blanks in the default colours.
        assert_eq!(
            tf.cells[2],
            TextCell {
                ch: ' ',
                fg: DEFAULT_FG,
                bg: tf.default_bg,
                bold: false,
            }
        );
    }

    #[test]
    fn text_frame_resolves_colors_like_the_raster() {
        let rec = text_rec(vec![(0.0, "\x1b[48;5;196m\x1b[38;5;21mZ".into())]);
        let score = text_score(10);
        let mut source = FrameSource::new(&rec, &score).unwrap();
        source.seek_frame(0);
        let tf = source.text_frame();
        let z = tf.cells[0];

        assert_eq!(z.ch, 'Z');
        assert_eq!(z.bg, resolve(Color::Idx(196), tf.default_bg));
        assert_eq!(z.fg, resolve(Color::Idx(21), DEFAULT_FG));
        assert_eq!(z.bg, xterm256(196), "the vector path must not drift");
    }
}
