//! The text-grid painting primitives: procedural cells (blocks, box drawing,
//! braille), colour resolution, and the per-cell painter [`render_cells`]
//! drives.

use std::collections::HashMap;

use fontdue::{Font, Metrics};
use vt100::{Color, Parser};

use super::glyph::{rasterize_with_fallback, GlyphBlit};
use super::{FallbackReport, DEFAULT_FG};

/// Standard xterm 16-colour ANSI palette.
pub(super) const ANSI16: [[u8; 3]; 16] = [
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

/// Coverage (0–255 per pixel, row-major) for a block-element, box-drawing, or
/// braille glyph drawn procedurally to fill a `w`×`h` cell, or `None` for an
/// ordinary glyph (use the font).
pub(super) fn solid_cell(ch: char, w: usize, h: usize) -> Option<Vec<u8>> {
    block_cell(ch, w, h)
        .or_else(|| box_cell(ch, w, h))
        .or_else(|| braille_cell(ch, w, h))
}

/// Braille patterns (U+2800–U+28FF): a 2-column × 4-row dot matrix encoded in the
/// low 8 bits of the codepoint. The bundled monospace fonts ship **no** braille
/// glyphs (they'd rasterize to `.notdef` tofu), yet braille is exactly how
/// `mapscii` and similar tools draw — so paint the dots procedurally instead.
pub(super) fn braille_cell(ch: char, w: usize, h: usize) -> Option<Vec<u8>> {
    let cp = ch as u32;
    if !(0x2800..=0x28ff).contains(&cp) {
        return None;
    }
    // Low byte of `cp - 0x2800` — which is just the low byte of `cp`,
    // since 0x2800 is a multiple of 256 and `as u8` keeps only that byte.
    let bits = cp as u8;
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
    for col in 0..2 {
        for row in 0..4 {
            if bits & dot_bit(col, row) == 0 {
                continue;
            }
            let cx = col as f32 * sub_w + sub_w / 2.0;
            let cy = row as f32 * sub_h + sub_h / 2.0;
            disc_coverage(&mut v, w, h, cx, cy, r);
        }
    }
    Some(v)
}

/// Paint one round dot of radius `r` centred at `(cx, cy)` into the `w`×`h`
/// coverage buffer `v`, leaving every other pixel of the cell untouched.
fn disc_coverage(v: &mut [u8], w: usize, h: usize, cx: f32, cy: f32, r: f32) {
    let r2 = r * r;
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

/// Block elements (U+2580–U+259F): full block, shades, halves, eighths, quadrants.
pub(super) fn block_cell(ch: char, w: usize, h: usize) -> Option<Vec<u8>> {
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
        '\u{2588}' => Some(vec![255; n]),         // █ full block
        '\u{2591}' => Some(vec![64; n]),          // ░ light shade
        '\u{2592}' => Some(vec![128; n]),         // ▒ medium shade
        '\u{2593}' => Some(vec![192; n]),         // ▓ dark shade
        '\u{2580}' => region(&|_, y| y < h / 2),  // ▀ upper half
        '\u{2584}' => region(&|_, y| y >= h / 2), // ▄ lower half
        // (No ▌ arm: 0x258C is the midpoint of the left-eighths range
        // below — fill w * 4 / 8 = w / 2, the same left half.)
        '\u{2590}' => region(&|x, _| x >= w / 2), // ▐ right half
        '\u{2594}' => region(&|_, y| y < h / 8),  // ▔ upper one-eighth
        '\u{2595}' => region(&|x, _| x >= w - w / 8), // ▕ right one-eighth
        // Lower 1–7 eighths (▁▂▃▄▅▆▇).
        '\u{2581}'..='\u{2587}' => {
            let fill = h * (ch as usize - 0x2580) / 8;
            region(&move |_, y| y >= h - fill)
        }
        // Left 8–1 eighths (▉▊▋▌▍▎▏ — ▌ is the w/2 midpoint).
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
pub(super) fn box_cell(ch: char, w: usize, h: usize) -> Option<Vec<u8>> {
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
pub(super) fn box_arms(ch: char) -> Option<(bool, bool, bool, bool)> {
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

/// Borrowed font faces and size used to rasterize glyphs the cache lacks.
pub(super) struct FontSet<'a> {
    pub(super) primary: &'a Font,
    pub(super) emoji: &'a Font,
    pub(super) last_resort: &'a Font,
    pub(super) px: f32,
}

/// Pixel geometry of one render.
pub(super) struct GridLayout {
    pub(super) cols: usize,
    pub(super) rows: usize,
    pub(super) cell_w: usize,
    pub(super) cell_h: usize,
    pub(super) ascent: f32,
    pub(super) default_bg: [u8; 3],
}

pub(super) fn render_cells(
    parser: &Parser,
    glyphs: &mut HashMap<char, (Metrics, Vec<u8>)>,
    fonts: &FontSet<'_>,
    grid: &GridLayout,
    fallback_report: &mut FallbackReport,
) -> Vec<u8> {
    let (w, h) = (grid.cols * grid.cell_w, grid.rows * grid.cell_h);
    let mut img = vec![0u8; w * h * 4];
    let screen = parser.screen();

    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let cell = screen.cell(row as u16, col as u16);
            let bg = cell
                .map(|c| resolve(c.bgcolor(), grid.default_bg))
                .unwrap_or(grid.default_bg);
            let fg = cell
                .map(|c| resolve(c.fgcolor(), DEFAULT_FG))
                .unwrap_or(DEFAULT_FG);
            let ctx = CellPaintCtx {
                w,
                h,
                x0: col * grid.cell_w,
                y0: row * grid.cell_h,
                cell_w: grid.cell_w,
                cell_h: grid.cell_h,
                ascent: grid.ascent,
                fg,
            };
            fill_cell_background(&ctx, &mut img, bg);

            let Some(chr) = cell.and_then(|c| c.contents().chars().next()) else {
                continue;
            };
            // Block & box-drawing glyphs are drawn procedurally to FILL the cell,
            // so banners and TUI frames render as solid, continuous shapes — the
            // font glyph leaves gaps between cells.
            if let Some(cov) = solid_cell(chr, grid.cell_w, grid.cell_h) {
                paint_solid_cell(&ctx, &mut img, &cov);
                continue;
            }
            // On-demand rasterization: if the glyph isn't cached yet, rasterize
            // it now so any Unicode character the font supports renders correctly.
            let (m, cov) = glyphs.entry(chr).or_insert_with(|| {
                rasterize_with_fallback(
                    fonts.primary,
                    fonts.emoji,
                    fonts.last_resort,
                    chr,
                    fonts.px,
                    fallback_report,
                )
            });
            if is_empty_glyph(m) {
                continue;
            }
            paint_glyph_cell(&ctx, &mut img, m, cov);
        }
    }
    img
}

/// Where one cell's contents land on the canvas: the cell's origin and size,
/// the canvas geometry that clips it, and the colour its glyph is drawn in.
#[derive(Clone, Copy)]
struct CellPaintCtx {
    /// Canvas width in pixels — the row stride and the right clip bound.
    w: usize,
    /// Canvas height in pixels — the bottom clip bound.
    h: usize,
    /// Cell origin, in canvas pixels.
    x0: usize,
    y0: usize,
    cell_w: usize,
    cell_h: usize,
    /// Distance from the cell top to the baseline, in pixels.
    ascent: f32,
    fg: [u8; 3],
}

/// Whether a rasterized glyph has nothing to paint: either dimension zero
/// means `paint_glyph_cell`'s loops are empty. Pure so the `||` is directly
/// testable (a zero width with a nonzero height — or the reverse — still
/// paints nothing and must be skipped).
pub(super) fn is_empty_glyph(m: &fontdue::Metrics) -> bool {
    m.width == 0 || m.height == 0
}

/// Fill one cell's rectangle with an opaque `bg`.
fn fill_cell_background(ctx: &CellPaintCtx, img: &mut [u8], bg: [u8; 3]) {
    for y in 0..ctx.cell_h {
        let base = ((ctx.y0 + y) * ctx.w + ctx.x0) * 4;
        for x in 0..ctx.cell_w {
            let p = base + x * 4;
            img[p] = bg[0];
            img[p + 1] = bg[1];
            img[p + 2] = bg[2];
            img[p + 3] = 255;
        }
    }
}

/// Alpha-blend a procedural cell fill (block, box-drawing or braille
/// coverage) over the cell.
fn paint_solid_cell(ctx: &CellPaintCtx, img: &mut [u8], cov: &[u8]) {
    for y in 0..ctx.cell_h {
        for x in 0..ctx.cell_w {
            let a = cov[y * ctx.cell_w + x] as u32;
            if a == 0 {
                continue;
            }
            let p = ((ctx.y0 + y) * ctx.w + ctx.x0 + x) * 4;
            blend_pixel(img, p, ctx.fg, a);
        }
    }
}

/// Alpha-blend a font glyph over the cell, centred horizontally and
/// sitting on the cell's baseline.
fn paint_glyph_cell(ctx: &CellPaintCtx, img: &mut [u8], m: &Metrics, cov: &[u8]) {
    let ox = ctx.x0 as i32 + ((ctx.cell_w as i32 - m.width as i32) / 2).max(0);
    let top = ctx.y0 as i32 + (ctx.ascent.round() as i32) - (m.height as i32 + m.ymin);
    GlyphBlit {
        m,
        cov,
        ox,
        top,
        fg: ctx.fg,
    }
    .draw(img, ctx.w, ctx.h);
}

/// Alpha-blend one 0–255 coverage sample of `fg` into the RGBA pixel starting
/// at byte `p`.
pub(super) fn blend_pixel(img: &mut [u8], p: usize, fg: [u8; 3], a: u32) {
    for k in 0..3 {
        let dst = img[p + k] as u32;
        let src = fg[k] as u32;
        img[p + k] = ((src * a + dst * (255 - a)) / 255) as u8;
    }
}

/// Map a vt100 colour to RGB. Indices 0..15 resolve through [`xterm256`],
/// whose first line returns the ANSI16 entry for them — byte-identical to a
/// dedicated fast-path arm, so there is deliberately only one route.
pub(super) fn resolve(c: Color, default: [u8; 3]) -> [u8; 3] {
    match c {
        Color::Default => default,
        Color::Idx(i) => xterm256(i),
        Color::Rgb(r, g, b) => [r, g, b],
    }
}

/// xterm 256-colour cube + grayscale ramp (indices 16..=255).
pub(super) fn xterm256(i: u8) -> [u8; 3] {
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
