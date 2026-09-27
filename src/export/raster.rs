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
mod tests;
