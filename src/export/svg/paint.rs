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
            let end = col + 1 + cells[col + 1..].iter().take_while(|c| c.bg == bg).count();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::raster::{TextCell, TextFrame};

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

    /// A cell with `ch`, the given fg, and the frame's default background.
    fn cell(ch: char, fg: [u8; 3]) -> TextCell {
        TextCell {
            ch,
            fg,
            bg: [11, 15, 20],
            bold: false,
        }
    }

    /// A cell with an explicit background and boldness.
    fn styled(ch: char, fg: [u8; 3], bg: [u8; 3], bold: bool) -> TextCell {
        TextCell { ch, fg, bg, bold }
    }

    #[test]
    fn row_cells_slices_the_grid_by_column_count() {
        let tf = frame(
            3,
            2,
            vec![
                cell('a', [1, 1, 1]),
                cell('b', [2, 2, 2]),
                cell('c', [3, 3, 3]),
                cell('d', [4, 4, 4]),
                cell('e', [5, 5, 5]),
                cell('f', [6, 6, 6]),
            ],
        );
        assert_eq!(row_cells(&tf, 0), Some(&tf.cells[0..3]));
        assert_eq!(row_cells(&tf, 1), Some(&tf.cells[3..6]));
        // Past the end of the grid: the frame is shorter than its row count.
        assert_eq!(row_cells(&tf, 2), None);
        assert_eq!(row_cells(&tf, 99), None);
    }

    #[test]
    fn a_columnless_frame_has_no_rows_at_all() {
        // cols == 0 must return None *before* chunks(0), which panics.
        let tf = frame(0, 1, vec![]);
        assert_eq!(row_cells(&tf, 0), None);
        assert_eq!(row_cells(&tf, 1), None);
    }

    #[test]
    fn braille_is_exactly_the_u2800_to_u28ff_block() {
        assert!(is_braille('\u{2800}'));
        assert!(is_braille('⣿'));
        assert!(is_braille('\u{28ff}'));
        assert!(!is_braille('\u{27ff}'));
        assert!(!is_braille('\u{2900}'));
        assert!(!is_braille('a'));
        assert!(!is_braille(' '));
    }

    #[test]
    fn breaks_run_covers_controls_and_braille() {
        assert!(breaks_run('\n'));
        assert!(breaks_run('\t'));
        assert!(breaks_run('\u{7}'));
        assert!(breaks_run('⣿'));
        assert!(!breaks_run('a'));
        assert!(!breaks_run(' '));
        assert!(!breaks_run('é'));
    }

    #[test]
    fn a_full_row_of_matching_cells_is_exactly_one_run() {
        let tf = frame(
            3,
            1,
            vec![
                cell('a', [255, 0, 0]),
                cell('b', [255, 0, 0]),
                cell('c', [255, 0, 0]),
            ],
        );
        assert_eq!(
            merge_runs(&tf, 0),
            vec![Run {
                start: 0,
                text: "abc".to_string(),
                fg: [255, 0, 0],
                bold: false,
            }]
        );
        // Row past the grid merges to nothing at all.
        assert!(merge_runs(&tf, 1).is_empty());
    }

    #[test]
    fn a_run_splits_on_fg_bg_and_bold_changes() {
        let tf = frame(
            4,
            1,
            vec![
                cell('a', [255, 0, 0]),
                cell('b', [0, 255, 0]),
                styled('c', [0, 255, 0], [0, 80, 160], false),
                styled('d', [0, 255, 0], [0, 80, 160], true),
            ],
        );
        assert_eq!(
            merge_runs(&tf, 0),
            vec![
                Run {
                    start: 0,
                    text: "a".to_string(),
                    fg: [255, 0, 0],
                    bold: false,
                },
                Run {
                    start: 1,
                    text: "b".to_string(),
                    fg: [0, 255, 0],
                    bold: false,
                },
                Run {
                    start: 2,
                    text: "c".to_string(),
                    fg: [0, 255, 0],
                    bold: false,
                },
                Run {
                    start: 3,
                    text: "d".to_string(),
                    fg: [0, 255, 0],
                    bold: true,
                },
            ]
        );
    }

    #[test]
    fn control_and_braille_cells_break_the_run() {
        // a · \n · b · ⣿ · c — the control char and the braille cell cannot ride
        // in a <text> run, so three runs come out with their exact starts.
        let tf = frame(
            5,
            1,
            vec![
                cell('a', [255, 0, 0]),
                cell('\n', [255, 0, 0]),
                cell('b', [255, 0, 0]),
                cell('⣿', [255, 0, 0]),
                cell('c', [255, 0, 0]),
            ],
        );
        assert_eq!(
            merge_runs(&tf, 0),
            vec![
                Run {
                    start: 0,
                    text: "a".to_string(),
                    fg: [255, 0, 0],
                    bold: false,
                },
                Run {
                    start: 2,
                    text: "b".to_string(),
                    fg: [255, 0, 0],
                    bold: false,
                },
                Run {
                    start: 4,
                    text: "c".to_string(),
                    fg: [255, 0, 0],
                    bold: false,
                },
            ]
        );
    }

    #[test]
    fn leading_and_trailing_spaces_trim_but_interior_ones_stay() {
        let tf = frame(
            7,
            1,
            vec![
                cell(' ', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
                cell('a', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
                cell('b', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
                cell(' ', [255, 0, 0]),
            ],
        );
        assert_eq!(
            merge_runs(&tf, 0),
            vec![Run {
                start: 2,
                text: "a b".to_string(),
                fg: [255, 0, 0],
                bold: false,
            }]
        );

        // Nothing but spaces draws no text at all.
        let blanks = frame(3, 1, vec![cell(' ', [1, 2, 3]); 3]);
        assert!(merge_runs(&blanks, 0).is_empty());
    }

    #[test]
    fn bg_spans_group_consecutive_differing_backgrounds() {
        let tf = frame(
            3,
            2,
            vec![
                // row 0: default, then a two-cell blue run reaching the row end
                cell('a', [255, 255, 255]),
                styled('B', [255, 255, 255], [0, 80, 160], false),
                styled('C', [255, 255, 255], [0, 80, 160], false),
                // row 1: a one-cell red run starting at column 0
                styled('x', [0, 0, 0], [255, 0, 0], false),
                cell('y', [0, 0, 0]),
                cell('z', [0, 0, 0]),
            ],
        );
        assert_eq!(
            bg_spans(&tf),
            vec![
                BgSpan {
                    row: 0,
                    start: 1,
                    cells: 2,
                    color: [0, 80, 160],
                },
                BgSpan {
                    row: 1,
                    start: 0,
                    cells: 1,
                    color: [255, 0, 0],
                },
            ]
        );

        // A frame painted entirely in the canvas colour has no spans.
        let plain = frame(2, 1, vec![cell('a', [1, 2, 3]); 2]);
        assert!(bg_spans(&plain).is_empty());
    }

    #[test]
    fn bg_rects_multiply_the_span_out_to_pixels() {
        let tf = frame(
            3,
            2,
            vec![
                cell('a', [255, 255, 255]),
                styled('B', [255, 255, 255], [0, 80, 160], false),
                styled('C', [255, 255, 255], [0, 80, 160], false),
                styled('x', [0, 0, 0], [255, 0, 0], false),
                cell('y', [0, 0, 0]),
                cell('z', [0, 0, 0]),
            ],
        );
        assert_eq!(
            bg_rects(&tf),
            vec![
                // start 1 × cell_w 10 = 10, row 0 × cell_h 20 = 0, 2 × 10 = 20
                "<rect x=\"10\" y=\"0\" width=\"20\" height=\"20\" fill=\"#0050a0\"/>".to_string(),
                // start 0, row 1 × 20 = 20, one cell wide
                "<rect x=\"0\" y=\"20\" width=\"10\" height=\"20\" fill=\"#ff0000\"/>".to_string(),
            ]
        );

        let plain = frame(2, 1, vec![cell('a', [1, 2, 3]); 2]);
        assert_eq!(bg_rects(&plain), Vec::<String>::new());
    }

    #[test]
    fn run_xml_pins_x_baseline_font_size_and_text_length() {
        let tf = frame(4, 2, vec![]);
        let run = Run {
            start: 2,
            text: "ab".to_string(),
            fg: [1, 2, 3],
            bold: true,
        };
        // x = 2 × 10 = 20
        // y = 1 × 20 + 20 / 2 + (16.0 × 0.35 as usize) = 20 + 10 + 5 = 35
        // font-size = 16.0.round() = 16, textLength = 2 chars × 10 = 20
        assert_eq!(
            run_xml(&run, 1, &tf, "FAM", "LA"),
            "<text x=\"20\" y=\"35\" font-family=\"FAM\" font-size=\"16\" \
             font-weight=\"bold\" fill=\"#010203\" textLength=\"20\" \
             lengthAdjust=\"LA\" xml:space=\"preserve\">ab</text>"
        );

        // The non-bold spelling drops the font-weight attribute entirely.
        let plain = Run {
            start: 2,
            text: "ab".to_string(),
            fg: [1, 2, 3],
            bold: false,
        };
        assert_eq!(
            run_xml(&plain, 1, &tf, "FAM", "LA"),
            "<text x=\"20\" y=\"35\" font-family=\"FAM\" font-size=\"16\" \
             fill=\"#010203\" textLength=\"20\" lengthAdjust=\"LA\" \
             xml:space=\"preserve\">ab</text>"
        );

        // The text node is escaped inside the run.
        let meta = Run {
            start: 0,
            text: "a<b".to_string(),
            fg: [1, 2, 3],
            bold: false,
        };
        assert_eq!(
            run_xml(&meta, 0, &tf, "FAM", "LA"),
            "<text x=\"0\" y=\"15\" font-family=\"FAM\" font-size=\"16\" \
             fill=\"#010203\" textLength=\"30\" lengthAdjust=\"LA\" \
             xml:space=\"preserve\">a&lt;b</text>"
        );
    }

    #[test]
    fn run_text_uses_the_frame_font_family_and_spacing() {
        let tf = frame(4, 2, vec![]);
        let run = Run {
            start: 2,
            text: "ab".to_string(),
            fg: [1, 2, 3],
            bold: true,
        };
        assert_eq!(
            run_text(&run, 1, &tf),
            "<text x=\"20\" y=\"35\" font-family=\"'IBM Plex Mono', monospace\" \
             font-size=\"16\" font-weight=\"bold\" fill=\"#010203\" \
             textLength=\"20\" lengthAdjust=\"spacing\" \
             xml:space=\"preserve\">ab</text>"
        );
    }

    #[test]
    fn braille_dots_paint_the_full_eight_dot_cell() {
        // row 1, col 1, cell 10×40: x0 = 10, y0 = 40,
        // r = ((10/2).min(40/4) × 42 + 50) / 100 = (5 × 42 + 50) / 100 = 2.
        // cx: col 0 → 10 + 1×10/4 = 12, col 1 → 10 + 3×10/4 = 17
        // cy: k 0..3 → 40 + {1,3,5,7}×40/8 = 45, 55, 65, 75
        let expected = "<circle cx=\"12\" cy=\"45\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"12\" cy=\"55\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"12\" cy=\"65\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"12\" cy=\"75\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"17\" cy=\"45\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"17\" cy=\"55\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"17\" cy=\"65\" r=\"2\" fill=\"#010203\"/> \
         <circle cx=\"17\" cy=\"75\" r=\"2\" fill=\"#010203\"/>";
        assert_eq!(braille_dots(1, 1, '⣿', [1, 2, 3], 10, 40), expected);
    }

    #[test]
    fn a_single_braille_dot_in_a_wide_short_cell_lands_exactly() {
        // cell 40×20 at the origin: r = ((40/2).min(20/4) × 42 + 50) / 100 = 2,
        // cx = 0 + 1×40/4 = 10, cy = 0 + 1×20/8 = 2.
        assert_eq!(
            braille_dots(0, 0, '⠁', [0, 0, 0], 40, 20),
            "<circle cx=\"10\" cy=\"2\" r=\"2\" fill=\"#000000\"/>"
        );
    }

    #[test]
    fn the_blank_braille_cell_draws_nothing() {
        assert_eq!(braille_dots(0, 0, '\u{2800}', [9, 9, 9], 10, 40), "");
    }

    #[test]
    fn hex_escapes_and_entities_are_exact() {
        assert_eq!(hex([1, 2, 3]), "#010203");
        assert_eq!(hex([0, 0, 0]), "#000000");
        assert_eq!(hex([255, 255, 255]), "#ffffff");
        assert_eq!(hex([200, 200, 200]), "#c8c8c8");

        assert_eq!(escape_xml("plain"), "plain");
        assert_eq!(escape_xml("&<>"), "&amp;&lt;&gt;");
        assert_eq!(escape_xml("&amp;"), "&amp;amp;");
        assert_eq!(escape_xml("\"'"), "\"'");

        assert_eq!(escape_attr("plain"), "plain");
        assert_eq!(escape_attr("a\"b'c&<"), "a&quot;b'c&amp;&lt;");
    }
}
