//! The static poster: one frame of the vt100 grid drawn as vector text
//! (`--at <seconds>` selects it). A single-terminal score becomes real `<text>`
//! runs; a staged score embeds ONE rasterized frame as a base64 PNG.

use std::path::Path;

use super::font::{collect_poster_glyphs, embed_for};
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
        Kept::Cells(tf) => poster_document(&tf, w, h)?,
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

/// The whole `<svg>` for a cell-grid frame: the embedded subset font, the
/// rounded canvas, the background runs of every row, then each row's braille
/// dots and text runs. Geometry is all integers (the one `f32` — the font
/// size — is rounded once here), so the document is byte-stable.
pub(crate) fn poster_document(tf: &TextFrame, w: usize, h: usize) -> Result<String> {
    let face = embed_for(&collect_poster_glyphs(tf), &tf.font_family)?.1;
    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n"
    ));
    out.push_str(&format!("<style>\n{face}\n</style>\n"));
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
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::raster::TextCell;

    /// Poster geometry: 10×40 cells, 16px type, the standard canvas colour.
    fn frame(cols: usize, rows: usize, cells: Vec<TextCell>) -> TextFrame {
        TextFrame {
            cols,
            rows,
            cell_w: 10,
            cell_h: 40,
            px: 16.0,
            font_family: "IBM Plex Mono".into(),
            default_bg: [11, 15, 20],
            cells,
        }
    }

    fn cell(ch: char, fg: [u8; 3]) -> TextCell {
        TextCell {
            ch,
            fg,
            bg: [11, 15, 20],
            bold: false,
        }
    }

    fn styled(ch: char, fg: [u8; 3], bg: [u8; 3], bold: bool) -> TextCell {
        TextCell { ch, fg, bg, bold }
    }

    /// A unique scratch directory per test, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "demostage_poster_{}_{}",
                tag,
                std::process::id()
            ));
            let _ = std::fs::create_dir_all(&dir);
            Scratch(dir)
        }

        fn file(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The 4×2 grid: "a" over bold "cd", with "b" on a coloured background.
    fn poster_grid() -> TextFrame {
        frame(
            4,
            2,
            vec![
                cell('a', [255, 0, 0]),
                styled('b', [255, 0, 0], [0, 80, 160], false),
                cell(' ', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
                styled('c', [0, 255, 0], [11, 15, 20], true),
                styled('d', [0, 255, 0], [11, 15, 20], true),
                cell(' ', [0, 255, 0]),
                cell(' ', [0, 255, 0]),
            ],
        )
    }

    /// The exact `<svg>` for [`poster_grid`] at 40×80. The `@font-face` rule
    /// is recomputed from the fixture (its base64 is kilobytes long), so the
    /// snapshot pins the structure while the face-shape tests pin the font.
    fn poster_grid_expected() -> String {
        let face = super::super::font::embed_for(
            &super::super::font::collect_poster_glyphs(&poster_grid()),
            "IBM Plex Mono",
        )
        .unwrap()
        .1;
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
             width=\"40\" height=\"80\" viewBox=\"0 0 40 80\">\n\
             <style>\n{face}\n</style>\n\
             <rect x=\"0\" y=\"0\" width=\"40\" height=\"80\" rx=\"8\" fill=\"#0b0f14\"/>\n\
             <rect x=\"10\" y=\"0\" width=\"10\" height=\"40\" fill=\"#0050a0\"/>\n\
             <text x=\"0\" y=\"25\" font-family=\"'ds-term', 'IBM Plex Mono', monospace\" \
             font-size=\"16\" fill=\"#ff0000\" textLength=\"10\" lengthAdjust=\"spacing\" \
             xml:space=\"preserve\">a</text>\n\
             <text x=\"10\" y=\"25\" font-family=\"'ds-term', 'IBM Plex Mono', monospace\" \
             font-size=\"16\" fill=\"#ff0000\" textLength=\"10\" lengthAdjust=\"spacing\" \
             xml:space=\"preserve\">b</text>\n\
             <text x=\"0\" y=\"65\" font-family=\"'ds-term', 'IBM Plex Mono', monospace\" \
             font-size=\"16\" font-weight=\"bold\" fill=\"#00ff00\" textLength=\"20\" \
             lengthAdjust=\"spacing\" xml:space=\"preserve\">cd</text>\n\
             </svg>\n",
        )
    }

    #[test]
    fn kept_captures_both_frame_shapes() {
        let tf = frame(2, 1, vec![cell('q', [7, 8, 9]), cell('r', [7, 8, 9])]);
        match Kept::capture(&PosterFrame::Cells(&tf)) {
            Kept::Cells(copied) => {
                assert_eq!(copied.cols, 2);
                assert_eq!(copied.cells, tf.cells);
            }
            Kept::Rgba(_) => panic!("the cell frame must be copied as cells"),
        }

        let rgba = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        match Kept::capture(&PosterFrame::Rgba(&rgba)) {
            Kept::Rgba(copied) => assert_eq!(copied, rgba),
            Kept::Cells(_) => panic!("the rgba frame must be copied as rgba"),
        }
    }

    #[test]
    fn encode_keeps_the_frame_whose_index_matches_keep() {
        let scratch = Scratch::new("keep0");
        let path = scratch.file("poster.svg");
        let a = frame(1, 1, vec![cell('a', [200, 200, 200])]);
        let b = frame(1, 1, vec![cell('b', [200, 200, 200])]);
        encode(&path, 10, 40, 0, |emit| {
            emit(&PosterFrame::Cells(&a));
            emit(&PosterFrame::Cells(&b));
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            poster_document(&a, 10, 40).unwrap(),
            "frame #0 must be the one kept"
        );
        assert!(written.contains(">a</text>"), "wrong frame:\n{written}");
        assert!(!written.contains(">b</text>"), "wrong frame:\n{written}");
    }

    #[test]
    fn keep_past_the_end_falls_back_to_the_last_emitted_frame() {
        let scratch = Scratch::new("keeplast");
        let path = scratch.file("poster.svg");
        let a = frame(1, 1, vec![cell('a', [200, 200, 200])]);
        let b = frame(1, 1, vec![cell('b', [200, 200, 200])]);
        encode(&path, 10, 40, 99, |emit| {
            emit(&PosterFrame::Cells(&a));
            emit(&PosterFrame::Cells(&b));
            Ok(())
        })
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            poster_document(&b, 10, 40).unwrap(),
            "the last emitted frame must be the fallback"
        );
    }

    #[test]
    fn an_encoder_with_no_frames_writes_nothing_and_errors() {
        let scratch = Scratch::new("empty");
        let path = scratch.file("poster.svg");
        let err = encode(&path, 10, 40, 0, |_: &mut dyn FnMut(&PosterFrame<'_>)| {
            Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("no frames"), "unhelpful: {err}");
        assert!(!path.exists(), "nothing must be written on failure");
    }

    #[test]
    fn encode_embeds_an_rgba_frame_as_a_png_data_uri() {
        let scratch = Scratch::new("rgba");
        let path = scratch.file("poster.svg");
        let rgba = vec![128u8; 2 * 2 * 4];
        encode(&path, 2, 2, 0, |emit| {
            emit(&PosterFrame::Rgba(&rgba));
            Ok(())
        })
        .unwrap();
        let doc = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            doc.matches("<image").count(),
            1,
            "exactly one embedded frame:\n{doc}"
        );
        // `iVBORw0KGgo` is base64 of the 8-byte PNG signature: a real PNG.
        assert!(
            doc.contains("href=\"data:image/png;base64,iVBORw0KGgo"),
            "not a PNG data URI:\n{doc}"
        );
        assert!(
            doc.contains("xlink:href=\"data:image/png;base64,"),
            "older renderers need the xlink spelling:\n{doc}"
        );
        assert!(!doc.contains("<text"), "no fake vector text");
    }

    #[test]
    fn frame_index_selects_by_seconds_and_clamps_both_ends() {
        // The arithmetic: (t.max(0.0) × fps.max(1)).floor(), min(last).
        assert_eq!(frame_index(None, 15, 10), 9, "no time = last frame");
        assert_eq!(frame_index(None, 15, 1), 0, "a single frame is the last");
        assert_eq!(frame_index(Some(0.0), 15, 10), 0);
        // 0.2 × 15 = 3 exactly.
        assert_eq!(frame_index(Some(0.2), 15, 10), 3);
        // Past the end clamps to n_frames - 1, never beyond.
        assert_eq!(frame_index(Some(99.0), 15, 10), 9);
        assert_eq!(frame_index(Some(0.61), 15, 10), 9);
        // Negative seconds clamp to frame 0 rather than wrapping.
        assert_eq!(frame_index(Some(-1.0), 15, 10), 0);
        // fps 0 is raised to 1: 0.6s × 1fps = 0.
        assert_eq!(frame_index(Some(0.6), 0, 10), 0);
    }

    #[test]
    fn a_braille_row_paints_circles_and_the_text_row_ignores_it() {
        let tf = frame(
            2,
            1,
            vec![
                cell('x', [255, 255, 255]),
                TextCell {
                    ch: '⣿',
                    fg: [1, 2, 3],
                    bg: [11, 15, 20],
                    bold: false,
                },
            ],
        );
        // Same hand-computed layout as paint::braille_dots at row 0, col 1,
        // cell 10×40: cx 12/17, cy 5/15/25/35, r = 2 — one line per dot cell.
        let expected_dots = "<circle cx=\"12\" cy=\"5\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"12\" cy=\"15\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"12\" cy=\"25\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"12\" cy=\"35\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"17\" cy=\"5\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"17\" cy=\"15\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"17\" cy=\"25\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"17\" cy=\"35\" r=\"2\" fill=\"#010203\"/>\n";
        assert_eq!(paint_braille_row(&tf, 0), expected_dots);
        // A row past the grid is empty, not an error.
        assert_eq!(paint_braille_row(&tf, 5), "");

        // A row with no braille at all (text + blank braille) paints nothing.
        let plain = frame(
            2,
            1,
            vec![
                cell('x', [255, 255, 255]),
                TextCell {
                    ch: '\u{2800}',
                    fg: [255, 255, 255],
                    bg: [11, 15, 20],
                    bold: false,
                },
            ],
        );
        assert_eq!(paint_braille_row(&plain, 0), "");
    }

    #[test]
    fn a_text_row_emits_one_text_line_per_run() {
        let tf = frame(
            2,
            1,
            vec![
                cell('x', [255, 255, 255]),
                TextCell {
                    ch: '⣿',
                    fg: [1, 2, 3],
                    bg: [11, 15, 20],
                    bold: false,
                },
            ],
        );
        assert_eq!(
            paint_text_row(&tf, 0),
            "<text x=\"0\" y=\"25\" font-family=\"'ds-term', 'IBM Plex Mono', monospace\" \
             font-size=\"16\" fill=\"#ffffff\" textLength=\"10\" \
             lengthAdjust=\"spacing\" xml:space=\"preserve\">x</text>\n"
        );
        assert_eq!(paint_text_row(&tf, 5), "", "an empty row emits nothing");
    }

    #[test]
    fn poster_document_is_the_exact_poster() {
        let doc = poster_document(&poster_grid(), 40, 80).unwrap();
        assert_eq!(doc, poster_grid_expected());
        // Reproducible: same frame in, same bytes out.
        assert_eq!(
            poster_document(&poster_grid(), 40, 80).unwrap(),
            poster_grid_expected(),
            "must be byte-stable"
        );
    }

    #[test]
    fn raster_document_embeds_the_png_the_crate_encodes() {
        let rgba = vec![128u8; 2 * 2 * 4];
        let payload = base64(&png_bytes(&rgba, 2, 2).unwrap());
        let expected = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
             width=\"2\" height=\"2\" viewBox=\"0 0 2 2\">\n\
             <image x=\"0\" y=\"0\" width=\"2\" height=\"2\" rx=\"8\" \
             href=\"data:image/png;base64,{payload}\" xlink:href=\"data:image/png;base64,{payload}\"/>\n\
             </svg>\n"
        );
        assert_eq!(raster_document(&rgba, 2, 2).unwrap(), expected);
        // The payload itself is a PNG, not a stub.
        assert!(
            payload.starts_with("iVBORw0KGgo"),
            "not PNG bytes: {payload}"
        );
    }

    #[test]
    fn png_bytes_emits_the_png_signature_and_full_image() {
        let bytes = png_bytes(&[128u8; 2 * 2 * 4], 2, 2).unwrap();
        assert!(
            bytes.len() > 8,
            "a 2×2 RGBA PNG is much longer than {} bytes",
            bytes.len()
        );
        assert_eq!(
            &bytes[..8],
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            "PNG signature missing"
        );
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// The score the replay snapshots render: a 10×2 terminal, 10 fps.
    fn replay_score() -> Score {
        toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 40
fps = 10
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 40
"#,
        )
        .unwrap()
    }

    /// Two frames of output: `ab`, then `cd` on the second row.
    fn replay_rec() -> Recording {
        Recording {
            cols: 10,
            rows: 2,
            title: "t".into(),
            events: vec![(0.0, "ab".into()), (0.3, "\r\ncd".into())],
            captions: vec![],
            focuses: vec![],
            duration: 0.5,
        }
    }

    /// The last frame of [`replay_rec`]: "ab" on row 0, "cd" on row 1.
    fn replay_last_frame() -> TextFrame {
        let mut cells = Vec::new();
        for ch in "ab".chars().chain(std::iter::repeat_n(' ', 8)) {
            cells.push(cell(ch, [200, 200, 200]));
        }
        for ch in "cd".chars().chain(std::iter::repeat_n(' ', 8)) {
            cells.push(cell(ch, [200, 200, 200]));
        }
        frame(10, 2, cells)
    }

    #[test]
    fn write_svg_renders_the_selected_frame_to_the_byte() {
        let scratch = Scratch::new("write_svg");
        let path = scratch.file("poster.svg");
        // No `--at`: the LAST frame of the replay, both rows on screen.
        write_svg(&path, &replay_rec(), &replay_score(), None).unwrap();
        let doc = std::fs::read_to_string(&path).unwrap();
        let face = super::super::font::embed_for(
            &super::super::font::collect_poster_glyphs(&replay_last_frame()),
            "DejaVu Sans Mono",
        )
        .unwrap()
        .1;
        let expected = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
             width=\"100\" height=\"38\" viewBox=\"0 0 100 38\">\n\
             <style>\n{face}\n</style>\n\
             <rect x=\"0\" y=\"0\" width=\"100\" height=\"38\" rx=\"8\" fill=\"#0b0f14\"/>\n\
             <text x=\"0\" y=\"14\" font-family=\"'ds-term', 'DejaVu Sans Mono', monospace\" \
             font-size=\"16\" fill=\"#c8c8c8\" textLength=\"20\" lengthAdjust=\"spacing\" \
             xml:space=\"preserve\">ab</text>\n\
             <text x=\"0\" y=\"33\" font-family=\"'ds-term', 'DejaVu Sans Mono', monospace\" \
             font-size=\"16\" fill=\"#c8c8c8\" textLength=\"20\" lengthAdjust=\"spacing\" \
             xml:space=\"preserve\">cd</text>\n\
             </svg>\n",
        );
        assert_eq!(doc, expected, "deterministic poster drifted");
    }
}
