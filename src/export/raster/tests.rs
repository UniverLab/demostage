use super::cells::{block_cell, box_arms, box_cell, braille_cell, solid_cell, xterm256, ANSI16};
use super::glyph::{rasterize_with_fallback, GlyphBlit};
use super::*;
use vt100::Color;

#[test]
fn precache_glyphs_caches_what_the_recording_prints() {
    let rec = Recording {
        cols: 16,
        rows: 2,
        title: "t".into(),
        events: vec![(0.0, "\u{2588}\u{2591}\u{2192} caf\u{e9}".into())],
        captions: vec![],
        focuses: vec![],
        duration: 0.1,
    };
    let font = fonts::load(fonts::DEFAULT_FONT);
    let emoji = fonts::load_emoji();
    let last_resort = fonts::load_last_resort();
    let mut glyphs = HashMap::new();
    let mut report = FallbackReport::new();

    precache_glyphs(
        &rec,
        &mut glyphs,
        &font,
        &emoji,
        &last_resort,
        16.0,
        &mut report,
    );

    // Every non-ASCII glyph the capture prints is cached up front …
    assert!(
        glyphs.contains_key(&'\u{2588}'),
        "banner block must be cached"
    );
    assert!(
        glyphs.contains_key(&'\u{2591}'),
        "shade block must be cached"
    );
    assert!(glyphs.contains_key(&'\u{2192}'), "arrow must be cached");
    assert!(
        glyphs.contains_key(&'\u{e9}'),
        "accented letter must be cached"
    );
    // … but control characters and whitespace never are.
    assert!(!glyphs.contains_key(&'\n'));
    assert!(!glyphs.contains_key(&' '));
}

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
    GlyphBlit {
        m: &m,
        cov: &cov,
        ox: 2,
        top: 4,
        fg: [255, 0, 0],
    }
    .draw(&mut img, 20, 20);
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
    GlyphBlit {
        m: &m,
        cov: &cov,
        ox: 0,
        top: -4,
        fg: [0, 255, 0],
    }
    .draw(&mut img_clipped, 20, 20);
    // top = 0 draws the full glyph
    GlyphBlit {
        m: &m,
        cov: &cov,
        ox: 0,
        top: 0,
        fg: [0, 255, 0],
    }
    .draw(&mut img_full, 20, 20);
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
    GlyphBlit {
        m: &m,
        cov: &cov,
        ox: 6,
        top: 0,
        fg: [0, 0, 255],
    }
    .draw(&mut img, 10, 10);
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
    GlyphBlit {
        m: &m,
        cov: &cov,
        ox: 0,
        top: 0,
        fg: [255, 255, 255],
    }
    .draw(&mut img, 10, 10);
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
        '─', '━', '│', '┃', '┌', '┏', '┐', '┓', '└', '┗', '┘', '┛', '├', '┣', '┤', '┫', '┬', '┳',
        '┴', '┻', '┼', '╋',
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
fn next_text_frame_walks_the_same_states_as_seek() {
    let rec = text_rec(vec![
        (0.0, "one".into()),
        (0.2, " two".into()),
        (0.5, "\r\nnext".into()),
    ]);
    let score = text_score(10);

    let mut walked = FrameSource::new(&rec, &score).unwrap();
    let n = walked.n_frames();
    let mut states = Vec::new();
    while let Some(tf) = walked.next_text_frame() {
        states.push(tf.cells.clone());
    }
    assert_eq!(states.len(), n, "the walk must cover every frame");

    let mut seeker = FrameSource::new(&rec, &score).unwrap();
    for (i, cells) in states.iter().enumerate() {
        seeker.seek_frame(i);
        assert_eq!(&seeker.text_frame().cells, cells, "frame {i} drifted");
    }
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
