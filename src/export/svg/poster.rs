//! The static poster: one frame of the vt100 grid drawn as vector text
//! (`--at <seconds>` selects it). A single-terminal score becomes real `<text>`
//! runs; a staged score embeds ONE rasterized frame as a base64 PNG.

use std::path::Path;

use super::paint::{bg_rects, braille_dots, hex, is_braille, merge_runs, row_cells, run_text};
use crate::error::{Error, Result};
use crate::export::raster::{FrameSource, TextFrame};
use crate::export::run::Recording;
use crate::model::Score;

/// One emitted frame for the poster encoder.
pub enum PosterFrame<'a> {
    /// The vt100 cell grid — drawn as vector `<text>`.
    Cells(&'a TextFrame),
    /// A composited RGBA canvas (staged scores) — embedded as ONE base64 PNG.
    Rgba(&'a [u8]),
}

/// The frame [`encode`] keeps, in a form that outlives the callback.
enum Kept {
    Cells(TextFrame),
    Rgba(Vec<u8>),
}

impl Kept {
    /// An owned copy of an emitted frame: the reference only lives as long as
    /// the callback, so the frame the poster keeps is copied here.
    fn capture(frame: &PosterFrame<'_>) -> Self {
        match frame {
            PosterFrame::Cells(tf) => Kept::Cells((*tf).clone()),
            PosterFrame::Rgba(rgba) => Kept::Rgba(rgba.to_vec()),
        }
    }
}

/// Encode the poster at `path`. Mirrors [`crate::export::gif::encode`]'s callback
/// shape: `render` replays frames through `emit`; the encoder keeps frame
/// `#keep` (0-based among EMITTED frames) and drops the rest, so only one
/// frame is ever held in memory. A source that has already seeked
/// ([`write_svg`]) emits one frame and passes `keep = 0`.
pub fn encode(
    path: &Path,
    w: usize,
    h: usize,
    keep: usize,
    render: impl FnOnce(&mut dyn FnMut(&PosterFrame)) -> Result<()>,
) -> Result<()> {
    let mut count = 0usize;
    let mut at_keep: Option<Kept> = None;
    let mut last: Option<Kept> = None;

    render(&mut |frame| {
        let idx = count;
        count += 1;
        if idx == keep {
            at_keep = Some(Kept::capture(frame));
        } else if at_keep.is_none() {
            // Provisional "last frame", in case `keep` never lands on one: it
            // stops being copied the moment the kept frame appears, so at most
            // one frame is ever held here.
            last = Some(Kept::capture(frame));
        }
    })?;

    // `keep` past the end of the replay falls back to the last emitted frame.
    let kept = at_keep
        .or(last)
        .ok_or_else(|| Error::Export("svg poster: no frames were emitted".to_string()))?;
    let svg = match kept {
        Kept::Cells(tf) => poster_document(&tf, w, h),
        Kept::Rgba(rgba) => raster_document(&rgba, w, h)?,
    };
    std::fs::write(path, svg).map_err(|e| Error::io(path, e))
}

/// Single-terminal fast path, mirrors [`crate::export::gif::write_gif`]: build a
/// [`FrameSource`], seek to the requested frame (no rasterizing of the frames
/// in between) and emit its cell grid. `at_secs: None` means the last frame.
pub fn write_svg(path: &Path, rec: &Recording, score: &Score, at_secs: Option<f64>) -> Result<()> {
    let mut source = FrameSource::new(rec, score)?;
    // Clamp with the source's own count: `n_frames` uses `duration.max(dt)`,
    // which can differ from export::render's `total_frames` for sub-frame
    // recordings.
    let at = frame_index(at_secs, score.layout.fps.max(1), source.n_frames());
    source.seek_frame(at);
    let frame = source.text_frame();
    let (w, h) = source.dims();
    encode(path, w, h, 0, |emit| {
        emit(&PosterFrame::Cells(&frame));
        Ok(())
    })
}

/// Pure frame selection, used by both `write_svg` and the staged arm: `None`
/// (or a time past the end) selects the last frame, `Some(t)` the frame at
/// `t * fps`, clamped into `[0, n_frames - 1]`.
pub fn frame_index(at_secs: Option<f64>, fps: u32, n_frames: usize) -> usize {
    let last = n_frames.max(1) - 1;
    match at_secs {
        None => last,
        Some(t) => ((t.max(0.0) * fps.max(1) as f64).floor() as usize).min(last),
    }
}

/// One row's braille cells as `<circle>` dots, one element per dot.
/// Cells without braille (or the blank U+2800) contribute nothing.
fn paint_braille_row(tf: &TextFrame, row: usize) -> String {
    let mut out = String::new();
    let Some(cells) = row_cells(tf, row) else {
        return out;
    };
    for (col, cell) in cells.iter().enumerate() {
        if !is_braille(cell.ch) {
            continue;
        }
        let dots = braille_dots(row, col, cell.ch, cell.fg, tf.cell_w, tf.cell_h);
        if dots.is_empty() {
            continue;
        }
        out.push_str(&dots);
        out.push('\n');
    }
    out
}

/// One row's merged text runs as `<text>` elements, one per line.
fn paint_text_row(tf: &TextFrame, row: usize) -> String {
    let mut out = String::new();
    for run in merge_runs(tf, row) {
        out.push_str(&run_text(&run, row, tf));
        out.push('\n');
    }
    out
}

/// The whole `<svg>` for a cell-grid frame: rounded canvas, the background runs
/// of every row, then each row's braille dots and text runs. Geometry is all
/// integers (the one `f32` — the font size — is rounded once here), so the
/// document is byte-stable.
pub(crate) fn poster_document(tf: &TextFrame, w: usize, h: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n"
    ));
    out.push_str(&format!(
        "<rect x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\" rx=\"8\" fill=\"{}\"/>\n",
        hex(tf.default_bg)
    ));
    for rect in bg_rects(tf) {
        out.push_str(&rect);
        out.push('\n');
    }
    for row in 0..tf.rows {
        out.push_str(&paint_braille_row(tf, row));
        out.push_str(&paint_text_row(tf, row));
    }
    out.push_str("</svg>\n");
    out
}

/// The staged fallback: ONE composited frame embedded as a base64 PNG, both
/// `href` spellings so older librsvg/Inkscape render it too. Staged scores
/// embed one rasterized frame — multi-pane compositing has no cell-grid
/// equivalent.
fn raster_document(rgba: &[u8], w: usize, h: usize) -> Result<String> {
    let b64 = base64(&png_bytes(rgba, w, h)?);
    Ok(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n\
         <image x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\" rx=\"8\" \
         href=\"data:image/png;base64,{b64}\" xlink:href=\"data:image/png;base64,{b64}\"/>\n\
         </svg>\n"
    ))
}

/// Encode an RGBA canvas as a PNG with the crate's existing encoder (no new
/// dependency).
fn png_bytes(rgba: &[u8], w: usize, h: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, w as u32, h as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::Export(format!("png: {e}")))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| Error::Export(format!("png: {e}")))?;
        writer
            .finish()
            .map_err(|e| Error::Export(format!("png: {e}")))?;
    }
    Ok(buf)
}

/// Standard-alphabet base64 with `=` padding, hand-rolled: the `base64` crate
/// is not a dependency and this is ~20 lines of it.
pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}
