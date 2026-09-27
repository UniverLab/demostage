//! SVG poster target: one static frame of the vt100 grid drawn as vector text
//! (opt-in — animation stays gif/mp4). A single-terminal score becomes real
//! `<text>` runs: crisp, selectable and tiny, with `textLength` pinning every
//! glyph to its grid column whatever monospace the viewer has. A staged
//! (multi-pane) score has no cell grid to draw from, so it falls back to
//! embedding ONE rasterized frame as a base64 PNG — documented, not faked as
//! vector. Deliberately no SMIL/CSS animation: motion belongs to gif/mp4.

use std::path::Path;

use super::raster::{FrameSource, TextCell, TextFrame};
use super::run::Recording;
use crate::error::{Error, Result};
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

/// Encode the poster at `path`. Mirrors [`super::gif::encode`]'s callback
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

/// Single-terminal fast path, mirrors [`super::gif::write_gif`]: build a
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
        if let Some(cells) = row_cells(tf, row) {
            for (col, cell) in cells.iter().enumerate() {
                if is_braille(cell.ch) {
                    let dots = braille_dots(row, col, cell.ch, cell.fg, tf);
                    if !dots.is_empty() {
                        out.push_str(&dots);
                        out.push('\n');
                    }
                }
            }
        }
        for run in merge_runs(tf, row) {
            out.push_str(&run_text(&run, row, tf));
            out.push('\n');
        }
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

/// The cells of one row, or `None` when the frame is shorter than its grid.
fn row_cells(tf: &TextFrame, row: usize) -> Option<&[TextCell]> {
    if tf.cols == 0 {
        return None;
    }
    tf.cells.chunks(tf.cols).nth(row)
}

/// One `<rect>` per maximal run of same-coloured cells in a row that differ
/// from the canvas background. Spaces are included on purpose: a selected-text
/// look (and coloured padding) must survive.
fn bg_rects(tf: &TextFrame) -> Vec<String> {
    let mut out = Vec::new();
    for row in 0..tf.rows {
        let Some(cells) = row_cells(tf, row) else {
            break;
        };
        let mut col = 0;
        while col < cells.len() {
            let bg = cells[col].bg;
            if bg == tf.default_bg {
                col += 1;
                continue;
            }
            let mut end = col + 1;
            while end < cells.len() && cells[end].bg == bg {
                end += 1;
            }
            out.push(format!(
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"/>",
                col * tf.cell_w,
                row * tf.cell_h,
                (end - col) * tf.cell_w,
                tf.cell_h,
                hex(bg)
            ));
            col = end;
        }
    }
    out
}

/// A maximal horizontal span of cells sharing `(fg, bg, bold)`, trimmed to the
/// part that actually draws ink: leading spaces shift `x`, trailing ones draw
/// nothing, and a span with no ink at all emits nothing.
struct Run {
    start: usize,
    text: String,
    fg: [u8; 3],
    bold: bool,
}

/// Cells whose character can't ride in a `<text>` run: viewer fonts have no
/// braille (raster.rs paints those dots itself) and no control character draws
/// ink — both break the run instead.
fn breaks_run(ch: char) -> bool {
    ch.is_control() || is_braille(ch)
}

fn is_braille(ch: char) -> bool {
    (0x2800..=0x28ff).contains(&(ch as u32))
}

/// The cell-run merging the poster is built from: adjacent cells with the same
/// style become one `<text>` run.
fn merge_runs(tf: &TextFrame, row: usize) -> Vec<Run> {
    let mut runs = Vec::new();
    let Some(cells) = row_cells(tf, row) else {
        return runs;
    };
    let mut i = 0;
    while i < cells.len() {
        if breaks_run(cells[i].ch) {
            i += 1;
            continue;
        }
        let head = cells[i];
        let mut end = i + 1;
        while end < cells.len()
            && !breaks_run(cells[end].ch)
            && cells[end].fg == head.fg
            && cells[end].bg == head.bg
            && cells[end].bold == head.bold
        {
            end += 1;
        }
        let (mut first, mut last) = (i, end);
        while first < last && cells[first].ch == ' ' {
            first += 1;
        }
        while last > first && cells[last - 1].ch == ' ' {
            last -= 1;
        }
        if first < last {
            runs.push(Run {
                start: first,
                text: cells[first..last].iter().map(|c| c.ch).collect(),
                fg: head.fg,
                bold: head.bold,
            });
        }
        i = end;
    }
    runs
}

/// One `<text>` element for a run. The baseline is the caption overlay's
/// integer centering rule (`cell_h/2 + px*0.35`), never fontdue's ascent: the
/// viewer renders a different font than the embedded one, and integer math
/// keeps the document deterministic. `textLength` + `lengthAdjust="spacing"`
/// locks the run to its grid columns regardless of the viewer's advance ratio.
fn run_text(run: &Run, row: usize, tf: &TextFrame) -> String {
    let n = run.text.chars().count();
    let x = run.start * tf.cell_w;
    let y = row * tf.cell_h + tf.cell_h / 2 + (tf.px * 0.35) as usize;
    let family = format!("'{}', monospace", escape_attr(&tf.font_family));
    let bold = if run.bold {
        " font-weight=\"bold\""
    } else {
        ""
    };
    format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"{family}\" font-size=\"{}\"{bold} \
         fill=\"{}\" textLength=\"{}\" lengthAdjust=\"spacing\" xml:space=\"preserve\">{}</text>",
        (tf.px.round() as usize),
        hex(run.fg),
        n * tf.cell_w,
        escape_xml(&run.text)
    )
}

/// Procedural dots for a braille cell — bundled AND viewer fonts generally
/// lack U+2800–U+28FF, which is exactly why raster.rs paints them itself. Same
/// 2×4 sub-cell layout and 0.42 radius ratio as the raster, in integer math so
/// the document stays byte-stable. Empty (U+2800) draws nothing.
fn braille_dots(row: usize, col: usize, ch: char, fg: [u8; 3], tf: &TextFrame) -> String {
    // Unicode dot bit → (column, row) in the 2×4 sub-cell matrix. Left column
    // = dots 1,2,3,7; right column = dots 4,5,6,8 — the same mapping
    // raster.rs paints with.
    const DOTS: [(u8, usize, usize); 8] = [
        (0x01, 0, 0),
        (0x02, 0, 1),
        (0x04, 0, 2),
        (0x40, 0, 3),
        (0x08, 1, 0),
        (0x10, 1, 1),
        (0x20, 1, 2),
        (0x80, 1, 3),
    ];
    let bits = (ch as u32 - 0x2800) as u8;
    let (w, h) = (tf.cell_w, tf.cell_h);
    // Origin of the cell on the canvas.
    let (x0, y0) = (col * w, row * h);
    // sub-cell = half the cell wide, a quarter tall; radius = 0.42 of the
    // smaller axis, rounded to a whole pixel (min 1px so a dot never
    // vanishes) — the raster uses the same ratio as a float.
    let r = (((w / 2).min(h / 4) * 42 + 50) / 100).max(1);
    let mut dots: Vec<String> = Vec::new();
    for (bit, c, k) in DOTS {
        if bits & bit == 0 {
            continue;
        }
        let cx = x0 + (2 * c + 1) * w / 4;
        let cy = y0 + (2 * k + 1) * h / 8;
        dots.push(format!(
            "<circle cx=\"{cx}\" cy=\"{cy}\" r=\"{r}\" fill=\"{}\"/>",
            hex(fg)
        ));
    }
    dots.join(" ")
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
fn base64(data: &[u8]) -> String {
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

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// Escape a text node. `&` goes first so the entities the other replacements
/// introduce are never re-escaped.
pub(crate) fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape an attribute value. The attributes are double-quoted, so `"` is the
/// only extra character that must be escaped (`'` is legal there and shows up
/// literally in every font list).
pub(crate) fn escape_attr(s: &str) -> String {
    escape_xml(s).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(cols: usize, rows: usize, cells: Vec<TextCell>) -> TextFrame {
        TextFrame {
            cols,
            rows,
            cell_w: 10,
            cell_h: 20,
            px: 16.0,
            font_family: "IBM Plex Mono".into(),
            default_bg: [11, 15, 20],
            cells,
        }
    }

    /// A cell with `ch`, the given fg and the frame's default background.
    fn cell(ch: char, fg: [u8; 3]) -> TextCell {
        TextCell {
            ch,
            fg,
            bg: [11, 15, 20],
            bold: false,
        }
    }

    #[test]
    fn adjacent_same_style_cells_merge_into_one_text_run() {
        let tf = frame(
            6,
            1,
            vec![
                cell('a', [255, 0, 0]),
                cell('b', [255, 0, 0]),
                cell('c', [255, 0, 0]),
                // an fg change splits the run
                cell('d', [0, 255, 0]),
                // a bg change splits it too
                TextCell {
                    ch: 'e',
                    fg: [0, 255, 0],
                    bg: [1, 2, 3],
                    bold: false,
                },
                // a bold change splits it as well
                TextCell {
                    ch: 'f',
                    fg: [0, 255, 0],
                    bg: [1, 2, 3],
                    bold: true,
                },
            ],
        );
        let runs = merge_runs(&tf, 0);
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0].text, "abc");
        assert_eq!(runs[0].start, 0);
        assert_eq!(runs[1].text, "d");
        assert_eq!(runs[1].start, 3);
        assert_eq!(runs[2].text, "e");
        assert_eq!(runs[3].text, "f");
        assert!(runs[3].bold);
    }

    #[test]
    fn spaces_inside_a_run_survive_but_trailing_ones_are_trimmed() {
        let tf = frame(
            7,
            1,
            vec![
                // leading spaces only shift x
                cell(' ', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
                cell('a', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
                cell('b', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
            ],
        );
        let runs = merge_runs(&tf, 0);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "a b");
        assert_eq!(runs[0].start, 2, "leading spaces must shift the run's x");

        // A run of nothing but spaces draws no text at all.
        let blanks = frame(3, 1, vec![cell(' ', [1, 2, 3]); 3]);
        assert!(merge_runs(&blanks, 0).is_empty());
    }

    #[test]
    fn xml_special_chars_are_escaped() {
        assert_eq!(escape_xml("a<b>&\"'"), "a&lt;b&gt;&amp;\"'");
        assert_eq!(
            escape_attr("a<b>&\"'"),
            "a&lt;b&gt;&amp;&quot;'",
            "attribute values escape the double quote too"
        );
        // `&` goes first: escaping `<` alone would double-escape the `&` it
        // introduces, turning `&lt;` into `&amp;lt;`.
        assert_eq!(escape_xml("<"), "&lt;");
        assert_eq!(escape_xml("&"), "&amp;");
        assert_eq!(escape_xml("&amp;"), "&amp;amp;");
    }

    #[test]
    fn base64_matches_the_known_rfc_vectors() {
        // RFC 4648 §10 vectors (the memo's `"f" → "Zm8="` is a slip: `Zm8=` is
        // `fo`; one byte pads to `Zg==`).
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_time_past_the_end_clamps_to_the_last_frame() {
        assert_eq!(frame_index(Some(99.0), 15, 10), 9);
        assert_eq!(frame_index(Some(0.61), 15, 10), 9);
    }

    #[test]
    fn no_time_selected_means_the_last_frame() {
        assert_eq!(frame_index(None, 15, 10), 9);
        assert_eq!(frame_index(None, 15, 1), 0);
    }

    #[test]
    fn zero_picks_the_first_frame() {
        assert_eq!(frame_index(Some(0.0), 15, 10), 0);
        assert_eq!(frame_index(Some(0.05), 15, 10), 0);
        // 0.2s at 15fps → frame 3 (0.2 * 15 = 3).
        assert_eq!(frame_index(Some(0.2), 15, 10), 3);
    }

    #[test]
    fn braille_cells_become_dot_circles_not_tofu_text() {
        // ⣿ = all 8 dots; the blank braille cell draws nothing at all.
        let tf = frame(
            3,
            1,
            vec![
                cell('x', [255, 255, 255]),
                cell('⣿', [255, 255, 255]),
                cell('\u{2800}', [255, 255, 255]),
            ],
        );
        let svg = poster_document(&tf, 30, 20);
        assert_eq!(
            svg.matches("<circle").count(),
            8,
            "one circle per dot:\n{svg}"
        );
        assert!(
            !svg.contains("⣿"),
            "braille must never ride in a text run:\n{svg}"
        );
        assert!(
            svg.contains(">x</text>"),
            "ordinary text still draws:\n{svg}"
        );
    }

    #[test]
    fn the_small_replay_snapshot_is_byte_stable() {
        // 4×2 grid: "ab" (b on a coloured background) over bold "cd".
        let tf = frame(
            4,
            2,
            vec![
                cell('a', [255, 0, 0]),
                TextCell {
                    ch: 'b',
                    fg: [255, 0, 0],
                    bg: [0, 80, 160],
                    bold: false,
                },
                cell(' ', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
                TextCell {
                    ch: 'c',
                    fg: [0, 255, 0],
                    bg: [11, 15, 20],
                    bold: true,
                },
                TextCell {
                    ch: 'd',
                    fg: [0, 255, 0],
                    bg: [11, 15, 20],
                    bold: true,
                },
                cell(' ', [0, 255, 0]),
                cell(' ', [0, 255, 0]),
            ],
        );
        // 'b' is red text on a blue background: the blue comes from the `<rect>`
        // above it, while the run's `fill` is the foreground.
        let expected = concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" ",
            "width=\"40\" height=\"40\" viewBox=\"0 0 40 40\">\n",
            "<rect x=\"0\" y=\"0\" width=\"40\" height=\"40\" rx=\"8\" fill=\"#0b0f14\"/>\n",
            "<rect x=\"10\" y=\"0\" width=\"10\" height=\"20\" fill=\"#0050a0\"/>\n",
            "<text x=\"0\" y=\"15\" font-family=\"'IBM Plex Mono', monospace\" font-size=\"16\" ",
            "fill=\"#ff0000\" textLength=\"10\" lengthAdjust=\"spacing\" xml:space=\"preserve\">a</text>\n",
            "<text x=\"10\" y=\"15\" font-family=\"'IBM Plex Mono', monospace\" font-size=\"16\" ",
            "fill=\"#ff0000\" textLength=\"10\" lengthAdjust=\"spacing\" xml:space=\"preserve\">b</text>\n",
            "<text x=\"0\" y=\"35\" font-family=\"'IBM Plex Mono', monospace\" font-size=\"16\" ",
            "font-weight=\"bold\" fill=\"#00ff00\" textLength=\"20\" lengthAdjust=\"spacing\" ",
            "xml:space=\"preserve\">cd</text>\n",
            "</svg>\n",
        );
        let doc = poster_document(&tf, 40, 40);
        assert_eq!(doc, expected);
        assert_eq!(poster_document(&tf, 40, 40), doc, "must be reproducible");
    }

    #[test]
    fn a_frame_the_encoder_never_saw_falls_back_to_the_last_one() {
        let tf = frame(1, 1, vec![cell('z', [1, 2, 3])]);
        let dir = std::env::temp_dir().join(format!("demostage_svg_keep_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("poster.svg");
        // `keep` is past every emitted frame → the last emitted one is kept.
        encode(&path, 10, 20, 7, |emit| {
            emit(&PosterFrame::Cells(&tf));
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains(">z</text>"),
            "wrong frame kept:\n{written}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_staged_frame_is_embedded_as_one_base64_png() {
        // Staged (multi-pane) scores have no cell grid, so the poster keeps one
        // composited frame instead — a real PNG, not vector text pretending.
        let rgba = vec![128u8; 2 * 2 * 4];
        let dir = std::env::temp_dir().join(format!("demostage_svg_staged_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("poster.svg");
        encode(&path, 2, 2, 0, |emit| {
            emit(&PosterFrame::Rgba(&rgba));
            Ok(())
        })
        .unwrap();
        let doc = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            doc.matches("<image").count(),
            1,
            "exactly one frame:\n{doc}"
        );
        // `iVBORw0KGgo` is the base64 of the 8-byte PNG signature — so the
        // payload came out of the png encoder, not a stub. (No `=` here: the
        // stream continues, padding only lands at the end of the data URI.)
        assert!(
            doc.contains("href=\"data:image/png;base64,iVBORw0KGgo"),
            "not a PNG data URI:\n{doc}"
        );
        assert!(
            doc.contains("xlink:href=\"data:image/png;base64,"),
            "older renderers need the xlink spelling:\n{doc}"
        );
        assert!(
            !doc.contains("<text"),
            "the fallback must not fake vector text"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_encoder_that_emits_nothing_is_an_error() {
        let dir = std::env::temp_dir().join(format!("demostage_svg_empty_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("poster.svg");
        let err = encode(&path, 10, 20, 0, |_: &mut dyn FnMut(&PosterFrame<'_>)| {
            Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("no frames"), "unhelpful: {err}");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
