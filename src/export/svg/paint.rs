//! Shared cell→paint primitives for the SVG targets: run merging, background
//! spans, braille dots, and escaping. Both the static poster and the animated
//! document build from these, so the two cannot drift on colour or geometry.

use crate::export::raster::{TextCell, TextFrame};

/// A maximal horizontal span of cells sharing `(fg, bg, bold)`, trimmed to the
/// part that actually draws ink: leading spaces shift `x`, trailing ones draw
/// nothing, and a span with no ink at all emits nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Run {
    pub(crate) start: usize,
    pub(crate) text: String,
    pub(crate) fg: [u8; 3],
    pub(crate) bold: bool,
}

/// One maximal run of same-coloured background cells in a row that differ from
/// the canvas background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BgSpan {
    pub(crate) row: usize,
    pub(crate) start: usize,
    pub(crate) cells: usize,
    pub(crate) color: [u8; 3],
}

/// The cells of one row, or `None` when the frame is shorter than its grid.
pub(crate) fn row_cells(tf: &TextFrame, row: usize) -> Option<&[TextCell]> {
    if tf.cols == 0 {
        return None;
    }
    tf.cells.chunks(tf.cols).nth(row)
}

/// Cells whose character can't ride in a `<text>` run: viewer fonts have no
/// braille (raster.rs paints those dots itself) and no control character draws
/// ink — both break the run instead.
fn breaks_run(ch: char) -> bool {
    ch.is_control() || is_braille(ch)
}

pub(crate) fn is_braille(ch: char) -> bool {
    (0x2800..=0x28ff).contains(&(ch as u32))
}

/// The cell-run merging the SVG targets are built from: adjacent cells with
/// the same style become one `<text>` run.
pub(crate) fn merge_runs(tf: &TextFrame, row: usize) -> Vec<Run> {
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

/// The structured background spans of a frame, in row order. [`bg_rects`]
/// formats them; the animated writer consumes them directly.
pub(crate) fn bg_spans(tf: &TextFrame) -> Vec<BgSpan> {
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
            out.push(BgSpan {
                row,
                start: col,
                cells: end - col,
                color: bg,
            });
            col = end;
        }
    }
    out
}

/// One `<rect>` per maximal run of same-coloured cells in a row that differ
/// from the canvas background. Spaces are included on purpose: a selected-text
/// look (and coloured padding) must survive.
pub(crate) fn bg_rects(tf: &TextFrame) -> Vec<String> {
    bg_spans(tf)
        .iter()
        .map(|s| {
            format!(
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"/>",
                s.start * tf.cell_w,
                s.row * tf.cell_h,
                s.cells * tf.cell_w,
                tf.cell_h,
                hex(s.color)
            )
        })
        .collect()
}

/// One `<text>` element for a run, with an explicit font stack and length
/// adjustment. The baseline is the caption overlay's integer centering rule
/// (`cell_h/2 + px*0.35`), never fontdue's ascent: the viewer renders a
/// different font than the embedded one, and integer math keeps the document
/// deterministic. `textLength` locks the run to its grid columns regardless of
/// the viewer's advance ratio.
pub(crate) fn run_xml(
    run: &Run,
    row: usize,
    tf: &TextFrame,
    family: &str,
    length_adjust: &str,
) -> String {
    let n = run.text.chars().count();
    let x = run.start * tf.cell_w;
    let y = row * tf.cell_h + tf.cell_h / 2 + (tf.px * 0.35) as usize;
    let bold = if run.bold {
        " font-weight=\"bold\""
    } else {
        ""
    };
    format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"{family}\" font-size=\"{}\"{bold} \
         fill=\"{}\" textLength=\"{}\" lengthAdjust=\"{length_adjust}\" xml:space=\"preserve\">{}</text>",
        (tf.px.round() as usize),
        hex(run.fg),
        n * tf.cell_w,
        escape_xml(&run.text)
    )
}

/// The poster's spelling: the frame's own font family with `spacing`.
pub(crate) fn run_text(run: &Run, row: usize, tf: &TextFrame) -> String {
    let family = format!("'{}', monospace", escape_attr(&tf.font_family));
    run_xml(run, row, tf, &family, "spacing")
}

/// Procedural dots for a braille cell — bundled AND viewer fonts generally
/// lack U+2800–U+28FF, which is exactly why raster.rs paints them itself. Same
/// 2×4 sub-cell layout and 0.42 radius ratio as the raster, in integer math so
/// the document stays byte-stable. Empty (U+2800) draws nothing.
pub(crate) fn braille_dots(
    row: usize,
    col: usize,
    ch: char,
    fg: [u8; 3],
    cell_w: usize,
    cell_h: usize,
) -> String {
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
    let (w, h) = (cell_w, cell_h);
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

pub(crate) fn hex(c: [u8; 3]) -> String {
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
