//! Block elements (U+2580–U+259F) as `<rect>` geometry — the SVG counterpart
//! of the raster [`block_cell`]: the same integer region math, so the poster
//! and the animated document fill exactly the pixels the GIF fills. Without
//! this the embedded face draws ░▒▓ as its own dotted patterns while the GIF
//! shows a uniform wash.

use super::paint::hex;

/// Whether `ch` is a block element (exactly U+2580–U+259F).
pub(crate) fn is_block(ch: char) -> bool {
    (0x2580..=0x259f).contains(&(ch as u32))
}

/// One filled region of a cell, in cell-local pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn new(x: usize, y: usize, w: usize, h: usize) -> Self {
        Rect { x, y, w, h }
    }

    /// Whether the region paints anything at all.
    fn drawn(&self) -> bool {
        self.w > 0 && self.h > 0
    }
}

/// Quadrant bitmasks (TL, TR, BL, BR) for U+2596..=U+259F — the raster's table.
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

/// The `fill-opacity` for the shades — 64 / 128 / 192 of 255, printed with
/// exactly three decimals so the SVG blends to the raster's coverage — or
/// `None` for every other block, which paints opaque.
fn shade_opacity(ch: char) -> Option<String> {
    let coverage = match ch {
        '\u{2591}' => 64u32,
        '\u{2592}' => 128,
        '\u{2593}' => 192,
        _ => return None,
    };
    Some(format!("{:.3}", f64::from(coverage) / 255.0))
}

/// The regions `block_cell` fills for `ch`, with the raster's exact integer
/// math. Empty for anything it does not handle.
fn block_regions(ch: char, w: usize, h: usize) -> Vec<Rect> {
    match ch {
        '\u{2588}' | '\u{2591}' | '\u{2592}' | '\u{2593}' => vec![Rect::new(0, 0, w, h)],
        '\u{2580}' => vec![Rect::new(0, 0, w, h / 2)],
        '\u{2584}' => vec![Rect::new(0, h / 2, w, h - h / 2)],
        '\u{2590}' => vec![Rect::new(w / 2, 0, w - w / 2, h)],
        '\u{2594}' => vec![Rect::new(0, 0, w, h / 8)],
        '\u{2595}' => vec![Rect::new(w - w / 8, 0, w / 8, h)],
        // Lower 1–7 eighths (▁▂▃▄▅▆▇).
        '\u{2581}'..='\u{2587}' => {
            let fill = h * (ch as usize - 0x2580) / 8;
            vec![Rect::new(0, h - fill, w, fill)]
        }
        // Left 8–1 eighths (▉▊▋▌▍▎▏ — ▌ is the w/2 midpoint).
        '\u{2589}'..='\u{258f}' => {
            let fill = w * (0x2590 - ch as usize) / 8;
            vec![Rect::new(0, 0, fill, h)]
        }
        // Quadrant combinations (▖▗▘▙▚▛▜▝▞▟).
        '\u{2596}'..='\u{259f}' => quadrant_regions(ch, w, h),
        _ => Vec::new(),
    }
}

/// The set quadrants of a quadrant char, in the raster's TL, TR, BL, BR order.
fn quadrant_regions(ch: char, w: usize, h: usize) -> Vec<Rect> {
    let mask = QUADRANTS[ch as usize - 0x2596];
    let (hw, hh) = (w / 2, h / 2);
    [
        (0b0001, Rect::new(0, 0, hw, hh)),
        (0b0010, Rect::new(hw, 0, w - hw, hh)),
        (0b0100, Rect::new(0, hh, hw, h - hh)),
        (0b1000, Rect::new(hw, hh, w - hw, h - hh)),
    ]
    .into_iter()
    .filter(|(bit, _)| mask & bit != 0)
    .map(|(_, r)| r)
    .collect()
}

/// Block `ch` at `(row, col)` as space-joined `<rect>` elements, in canvas
/// pixels. Empty for a character `block_cell` does not fill, so it never rides
/// a `<text>` run and never adds an unhandled codepoint to the geometry.
/// Degenerate regions (a zero-area eighth or quadrant) emit nothing, which is
/// exactly the raster's coverage.
pub(crate) fn block_rects(
    row: usize,
    col: usize,
    ch: char,
    fg: [u8; 3],
    cell_w: usize,
    cell_h: usize,
) -> String {
    let (x0, y0) = (col * cell_w, row * cell_h);
    let fill = hex(fg);
    let opacity = shade_opacity(ch);
    let suffix = opacity
        .as_deref()
        .map(|o| format!(" fill-opacity=\"{o}\""))
        .unwrap_or_default();
    block_regions(ch, cell_w, cell_h)
        .into_iter()
        .filter(Rect::drawn)
        .map(|r| {
            format!(
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{fill}\"{suffix}/>",
                x0 + r.x,
                y0 + r.y,
                r.w,
                r.h
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::raster::block_cell;

    /// The value of one `name="value"` attribute of a `<rect>` the rasteriser
    /// below needs. Matched on the full `name="` so `fill` cannot be confused
    /// with `fill-opacity`.
    fn attr(rect: &str, name: &str) -> Option<String> {
        let key = format!("{name}=\"");
        let start = rect.find(&key)? + key.len();
        let rest = &rect[start..];
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    }

    /// Paint the emitted markup back into a `w`×`h` coverage buffer, so the
    /// SVG geometry can be compared with `block_cell`'s coverage pixel for
    /// pixel. Regions never overlap, so plain assignment is exact.
    fn coverage_of(markup: &str, w: usize, h: usize) -> Vec<u8> {
        let mut buf = vec![0u8; w * h];
        for rect in markup.split("<rect ").skip(1) {
            let num = |name: &str| {
                attr(rect, name)
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or_else(|| panic!("{name} missing from <rect {rect}"))
            };
            let (x, y) = (num("x"), num("y"));
            let (rw, rh) = (num("width"), num("height"));
            let alpha = attr(rect, "fill-opacity")
                .map(|o| (o.parse::<f64>().unwrap() * 255.0).round() as u8)
                .unwrap_or(255);
            for row in 0..rh {
                for col in 0..rw {
                    buf[(y + row) * w + x + col] = alpha;
                }
            }
        }
        buf
    }

    #[test]
    fn is_block_is_exactly_the_u2580_to_u259f_range() {
        assert!(is_block('\u{2580}'));
        assert!(is_block('█'));
        assert!(is_block('░'));
        assert!(is_block('\u{259f}'));
        assert!(!is_block('\u{257f}'));
        assert!(!is_block('\u{25a0}'));
        assert!(!is_block('a'));
        assert!(!is_block(' '));
        assert!(!is_block('⣿'));
    }

    #[test]
    fn a_full_block_is_one_rect_covering_the_cell() {
        assert_eq!(
            block_rects(0, 0, '█', [255, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"0\" width=\"10\" height=\"20\" fill=\"#ff0000\"/>"
        );
    }

    #[test]
    fn the_half_blocks_split_the_cell_in_the_raster_direction() {
        // ▀ = y < h / 2 → the upper half; ▄ = y >= h / 2; ▐ = x >= w / 2.
        assert_eq!(
            block_rects(0, 0, '▀', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"0\" width=\"10\" height=\"10\" fill=\"#000000\"/>"
        );
        assert_eq!(
            block_rects(0, 0, '▄', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"10\" width=\"10\" height=\"10\" fill=\"#000000\"/>"
        );
        assert_eq!(
            block_rects(0, 0, '▐', [0, 0, 0], 10, 20),
            "<rect x=\"5\" y=\"0\" width=\"5\" height=\"20\" fill=\"#000000\"/>"
        );
        // ▔ = y < h / 8 → 20 / 8 = 2 rows; ▕ = x >= w - w / 8 → 1 column.
        assert_eq!(
            block_rects(0, 0, '▔', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"0\" width=\"10\" height=\"2\" fill=\"#000000\"/>"
        );
        assert_eq!(
            block_rects(0, 0, '▕', [0, 0, 0], 10, 20),
            "<rect x=\"9\" y=\"0\" width=\"1\" height=\"20\" fill=\"#000000\"/>"
        );
    }

    #[test]
    fn the_lower_eighths_fill_h_n_eighths_up_from_the_bottom() {
        // fill = h * (cp - 0x2580) / 8, region y >= h - fill.
        // ▂ → 20 × 2 / 8 = 5; ▁ → 2; ▇ → 20 × 7 / 8 = 17.
        assert_eq!(
            block_rects(0, 0, '▂', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"15\" width=\"10\" height=\"5\" fill=\"#000000\"/>"
        );
        assert_eq!(
            block_rects(0, 0, '▁', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"18\" width=\"10\" height=\"2\" fill=\"#000000\"/>"
        );
        assert_eq!(
            block_rects(0, 0, '▇', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"3\" width=\"10\" height=\"17\" fill=\"#000000\"/>"
        );
    }

    #[test]
    fn the_left_eighths_fill_w_n_eighths_in_from_the_left() {
        // fill = w * (0x2590 - cp) / 8, region x < fill.
        // ▊ → 10 × 6 / 8 = 7; ▉ → 10 × 7 / 8 = 8; ▏ → 10 × 1 / 8 = 1.
        assert_eq!(
            block_rects(0, 0, '▊', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"0\" width=\"7\" height=\"20\" fill=\"#000000\"/>"
        );
        assert_eq!(
            block_rects(0, 0, '▉', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"0\" width=\"8\" height=\"20\" fill=\"#000000\"/>"
        );
        assert_eq!(
            block_rects(0, 0, '▏', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"0\" width=\"1\" height=\"20\" fill=\"#000000\"/>"
        );
        // ▌ is the w / 2 midpoint of that range.
        assert_eq!(
            block_rects(0, 0, '▌', [0, 0, 0], 10, 20),
            "<rect x=\"0\" y=\"0\" width=\"5\" height=\"20\" fill=\"#000000\"/>"
        );
    }

    #[test]
    fn quadrant_blocks_are_one_rect_per_set_quadrant() {
        // ▚ = TL + BR: two rects, in the raster's TL-then-BR order.
        assert_eq!(
            block_rects(0, 0, '▚', [1, 2, 3], 10, 20),
            "<rect x=\"0\" y=\"0\" width=\"5\" height=\"10\" fill=\"#010203\"/> \
             <rect x=\"5\" y=\"10\" width=\"5\" height=\"10\" fill=\"#010203\"/>"
        );
        // ▘ = TL alone.
        assert_eq!(
            block_rects(0, 0, '▘', [1, 2, 3], 10, 20),
            "<rect x=\"0\" y=\"0\" width=\"5\" height=\"10\" fill=\"#010203\"/>"
        );
        // ▟ = TR + BL + BR: three rects.
        assert_eq!(
            block_rects(0, 0, '▟', [1, 2, 3], 10, 20)
                .matches("<rect ")
                .count(),
            3
        );
    }

    #[test]
    fn the_shades_are_one_full_cell_rect_with_three_decimal_opacity() {
        // 64 / 128 / 192 of 255: 0.251, 0.502, 0.753.
        for (ch, opacity) in [('░', "0.251"), ('▒', "0.502"), ('▓', "0.753")] {
            assert_eq!(
                block_rects(0, 0, ch, [255, 255, 255], 10, 20),
                format!(
                    "<rect x=\"0\" y=\"0\" width=\"10\" height=\"20\" \
                     fill=\"#ffffff\" fill-opacity=\"{opacity}\"/>"
                ),
                "wrong opacity for {ch}"
            );
        }
    }

    #[test]
    fn every_block_codepoint_matches_the_raster_coverage() {
        for cp in 0x2580..=0x259f_u32 {
            let ch = char::from_u32(cp).unwrap();
            let markup = block_rects(0, 0, ch, [1, 2, 3], 10, 20);
            assert!(!markup.is_empty(), "U+{cp:04X} drew nothing");
            assert_eq!(
                coverage_of(&markup, 10, 20),
                block_cell(ch, 10, 20).unwrap(),
                "U+{cp:04X} coverage drifted from the raster"
            );
        }
    }

    #[test]
    fn characters_outside_the_block_range_emit_nothing() {
        for ch in ['a', ' ', '─', '\u{257f}', '\u{25a0}', '⣿', '\n'] {
            assert_eq!(
                block_rects(0, 0, ch, [0, 0, 0], 10, 20),
                "",
                "{ch:?} must not be treated as a block"
            );
        }
    }

    #[test]
    fn block_rects_offset_by_row_and_col() {
        assert_eq!(
            block_rects(2, 3, '█', [0, 0, 0], 10, 20),
            "<rect x=\"30\" y=\"40\" width=\"10\" height=\"20\" fill=\"#000000\"/>"
        );
        assert_eq!(
            block_rects(1, 2, '▀', [0, 0, 0], 10, 20),
            "<rect x=\"20\" y=\"20\" width=\"10\" height=\"10\" fill=\"#000000\"/>"
        );
    }

    #[test]
    fn degenerate_regions_emit_no_zero_area_rect() {
        // A cell too short for its region (`h / 8 == 0` or `h / 2 == 0`): the
        // raster's predicate matches nothing, so neither may we — and never a
        // `<rect height="0">`.
        assert_eq!(block_rects(0, 0, '▔', [0, 0, 0], 10, 4), "");
        assert_eq!(block_rects(0, 0, '▀', [0, 0, 0], 10, 1), "");
        // ▐ at w / 2 == 0 fills the whole cell — exactly what the raster's
        // `x >= w / 2` does on a one-pixel-wide cell.
        assert_eq!(
            block_rects(0, 0, '▐', [0, 0, 0], 1, 1),
            "<rect x=\"0\" y=\"0\" width=\"1\" height=\"1\" fill=\"#000000\"/>"
        );
        // The one-pixel-wide cases still agree with the reference.
        for (ch, w, h) in [
            ('▀', 1, 1),
            ('▐', 1, 1),
            ('▔', 10, 4),
            ('▀', 10, 1),
            ('█', 1, 1),
        ] {
            assert_eq!(
                coverage_of(&block_rects(0, 0, ch, [1, 2, 3], w, h), w, h),
                block_cell(ch, w, h).unwrap(),
                "U+{:04X} at {w}x{h}",
                ch as u32
            );
        }
    }
}
