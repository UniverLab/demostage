use std::collections::BTreeSet;

use super::animated::{
    braille_bits, fold_scenes, render_document, scene_hold_windows, scene_of, windows_for, BgItem,
    BlockItem, DocInput, DotItem, Geom, Hold, Scene, TextItem,
};
use super::animated_refusal;
use super::blocks::is_block;
use super::font::{collect_animated_glyphs, embed_for, subset_bytes, EMBED_FAMILY};
use super::paint::{escape_attr, escape_xml, merge_runs};
use super::poster::{base64, encode, frame_index, poster_document, write_svg, PosterFrame};
use super::timing::keyframes_rule;
use super::writer::write_animated;
use crate::export::raster::{TextCell, TextFrame};
use crate::export::run::Recording;
use crate::model::Score;

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

/// A row of plain cells spelling `text` in the default style.
fn text_row(text: &str, cols: usize) -> Vec<TextCell> {
    let mut row: Vec<TextCell> = text.chars().map(|ch| cell(ch, [200, 200, 200])).collect();
    row.resize(cols, cell(' ', [200, 200, 200]));
    row
}

/// Render synthetic frames through the animated document builder.
fn animated_doc(frames: &[TextFrame], w: usize, h: usize, fps: u32) -> String {
    let (font_family, font_face) = animated_face(frames);
    let first = &frames[0];
    let scenes: Vec<Scene> = frames.iter().map(scene_of).collect();
    let (unique, holds) = fold_scenes(scenes);
    let input = DocInput {
        unique: &unique,
        holds: &holds,
        n_frames: frames.len(),
        fps,
        w,
        h,
        geom: Geom {
            cw: first.cell_w,
            ch: first.cell_h,
            px: first.px,
        },
        default_bg: first.default_bg,
        final_tf: &frames[frames.len() - 1],
        font_family,
        font_face,
    };
    render_document(&input)
}

/// The `(family, face_rule)` pair the production writer embeds for `frames`.
fn animated_face(frames: &[TextFrame]) -> (String, String) {
    let scenes: Vec<Scene> = frames.iter().map(scene_of).collect();
    let (unique, _) = fold_scenes(scenes);
    let font_name = frames[frames.len() - 1].font_family.clone();
    embed_for(&collect_animated_glyphs(&unique), &font_name).unwrap()
}

#[test]
fn identical_consecutive_frames_collapse_to_one_hold() {
    let tf = frame(2, 1, vec![cell('a', [1, 2, 3]), cell('b', [1, 2, 3])]);
    let scenes = vec![scene_of(&tf), scene_of(&tf), scene_of(&tf)];
    let (unique, holds) = fold_scenes(scenes);
    assert_eq!(unique.len(), 1, "same screen twice is one state");
    assert_eq!(holds.len(), 1, "consecutive duplicates must collapse");
    assert_eq!((holds[0].start, holds[0].end), (0, 3));
}

#[test]
fn keyframe_percentages_match_frame_durations() {
    // 10 frames, visible over frames 3..6: the stops land on 30% and 60%.
    let rule = keyframes_rule("k0", &[(3, 6)], 10);
    assert!(rule.starts_with("@keyframes k0{"), "bad rule:\n{rule}");
    assert!(rule.contains("30%"), "start stop drifted:\n{rule}");
    assert!(rule.contains("60%"), "end stop drifted:\n{rule}");
}

#[test]
fn a_window_on_the_first_frame_is_visible_at_zero_percent() {
    // Nudging an equal stop would leave the opening state hidden for a
    // rounding step of every loop — at 0% nothing is drawn.
    let rule = keyframes_rule("k0", &[(0, 3)], 10);
    assert!(
        rule.contains("0%{opacity:1}"),
        "opening state blinks:\n{rule}"
    );
    let last = keyframes_rule("k1", &[(7, 10)], 10);
    assert!(
        last.contains("100%{opacity:0}"),
        "closing stop lost:\n{last}"
    );
}

#[test]
fn windows_sharing_a_boundary_stay_visible_across_it() {
    // Two windows back to back must collapse into one continuous visible
    // stretch, not a hidden sliver between the two stops.
    let rule = keyframes_rule("k0", &[(2, 4), (4, 6)], 10);
    assert!(
        rule.contains("20%{opacity:1}"),
        "first window lost:\n{rule}"
    );
    assert!(
        rule.contains("60%{opacity:0}"),
        "second window lost:\n{rule}"
    );
    assert!(
        !rule.contains("40%"),
        "the shared boundary must merge, not blink:\n{rule}"
    );
}

#[test]
fn a_recurring_state_is_defined_once_and_reused_with_use() {
    let a = frame(3, 1, text_row("aaa", 3));
    let b = frame(3, 1, text_row("bbb", 3));
    let doc = animated_doc(&[a.clone(), b, a], 30, 20, 3);
    assert_eq!(
        doc.matches("<g id=\"r0\">").count(),
        1,
        "the recurring screen needs one group:\n{doc}"
    );
    assert_eq!(
        doc.matches("<use").count(),
        2,
        "one <use> per distinct scene id:\n{doc}"
    );
    assert_eq!(
        doc.matches("<use href=\"#r0\"").count(),
        1,
        "the recurring screen reuses its group:\n{doc}"
    );
    assert!(
        doc.contains("66.6667%"),
        "the reuse must span both windows (frames 2..3 of 3):\n{doc}"
    );
    assert!(doc.contains(">aaa</text>"), "content lost:\n{doc}");
}

#[test]
fn animated_runs_name_the_embedded_face_first() {
    let a = frame(2, 1, text_row("ab", 2));
    let b = frame(2, 1, text_row("cd", 2));
    let doc = animated_doc(&[a, b], 20, 20, 2);
    assert!(
        doc.contains("font-family=\"'ds-term', 'IBM Plex Mono', monospace\""),
        "the embedded face must ride first:\n{doc}"
    );
    assert!(
        !doc.contains("ui-monospace"),
        "the viewer-fallback stack is gone:\n{doc}"
    );
    assert!(
        doc.contains("lengthAdjust=\"spacingAndGlyphs\""),
        "grid pinning missing:\n{doc}"
    );
    assert!(
        doc.contains("textLength=\"20\""),
        "run width missing:\n{doc}"
    );
}

#[test]
fn animated_document_is_scriptless_and_self_contained() {
    let a = frame(2, 1, text_row("ab", 2));
    let b = frame(2, 1, text_row("cd", 2));
    let doc = animated_doc(&[a, b], 20, 20, 2);
    assert!(!doc.contains("<script"), "no scripts allowed:\n{doc}");
    assert!(!doc.contains("<foreignObject"), "no foreignObject:\n{doc}");
    assert!(!doc.contains("href=\"http"), "no external refs:\n{doc}");
    assert!(
        !doc.contains("xlink:href=\"http"),
        "no external refs:\n{doc}"
    );
    assert!(
        doc.contains("steps(1,end)"),
        "states must hold with steps() timing:\n{doc}"
    );
}

#[test]
fn reduced_motion_shows_the_final_state() {
    let a = frame(3, 1, text_row("aaa", 3));
    let b = frame(3, 1, text_row("zzz", 3));
    let doc = animated_doc(&[a, b], 30, 20, 2);
    assert!(
        doc.contains("prefers-reduced-motion: reduce"),
        "reduced-motion rule missing:\n{doc}"
    );
    assert!(
        doc.contains("#final{display:none}"),
        "the final state hides by default:\n{doc}"
    );
    assert!(doc.contains(">zzz</text>"), "final state lost:\n{doc}");
}

#[test]
fn the_reduced_motion_frame_is_not_parked_inside_defs() {
    // `<defs>` children are never rendered, whatever `display` says — a
    // `#final` parked there leaves a reduced-motion viewer with a blank
    // canvas. It has to be a sibling of the animated group, after it.
    let a = frame(3, 1, text_row("aaa", 3));
    let b = frame(3, 1, text_row("zzz", 3));
    let doc = animated_doc(&[a, b], 30, 20, 2);
    let anim = doc.find("<g id=\"anim\">").expect("animated group");
    let defs_end = doc.find("</defs>").expect("defs block");
    let final_at = doc.find("<g id=\"final\">").expect("final state");
    assert!(anim < final_at, "the static frame must paint last:\n{doc}");
    assert!(
        defs_end < final_at,
        "the final state must live outside <defs>:\n{doc}"
    );
}

#[test]
fn a_persistent_run_is_drawn_once_across_consecutive_states() {
    // "keep" survives holds 0..2 but is gone from the final frame, so the only
    // copy in the document is the persistent element.
    let mut f0 = text_row("keep", 4);
    f0.extend(text_row("aaa", 4));
    let mut f1 = text_row("keep", 4);
    f1.extend(text_row("bbb", 4));
    let mut f2 = text_row("gone", 4);
    f2.extend(text_row("ccc", 4));
    let doc = animated_doc(
        &[frame(4, 2, f0), frame(4, 2, f1), frame(4, 2, f2)],
        40,
        40,
        3,
    );
    assert_eq!(
        doc.matches(">keep</text>").count(),
        1,
        "a run spanning consecutive holds is drawn once:\n{doc}"
    );
}

#[test]
fn a_staged_score_with_a_browser_pane_refuses_with_kind_and_time() {
    let score: Score = toml::from_str(
        r#"
[demo]
name = "t"
[layout]
width = 200
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "p"
  type = "browser"
  x = 100
  y = 0
  width = 100
  height = 100
  url = "file:///x.pdf"
  reveal_at = 4.0
"#,
    )
    .unwrap();
    let msg = animated_refusal(&score).expect("a PDF pane must refuse");
    assert!(msg.contains("PDF"), "kind missing: {msg}");
    assert!(msg.contains('4'), "time missing: {msg}");
    assert!(
        msg.contains("use gif/mp4, or --at <s> for a poster"),
        "guidance missing: {msg}"
    );
}

#[test]
fn a_staged_score_with_a_second_terminal_still_refuses() {
    let score: Score = toml::from_str(
        r#"
[demo]
name = "t"
[layout]
width = 200
height = 100
  [[layout.panes]]
  id = "c1"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "c2"
  type = "terminal"
  x = 100
  y = 0
  width = 100
  height = 100
"#,
    )
    .unwrap();
    let msg = animated_refusal(&score).expect("two terminals must refuse");
    assert!(msg.contains("terminal"), "kind missing: {msg}");
}

#[test]
fn an_image_pane_uses_an_as_its_article() {
    let score: Score = toml::from_str(
        r#"
[demo]
name = "t"
[layout]
width = 200
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "p"
  type = "browser"
  x = 100
  y = 0
  width = 100
  height = 100
  url = "file:///x.png"
"#,
    )
    .unwrap();
    let msg = animated_refusal(&score).expect("an image pane must refuse");
    assert!(
        msg.contains("an image pane"),
        "article wrong for image: {msg}"
    );
}

#[test]
fn a_single_terminal_score_does_not_refuse() {
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
    assert_eq!(animated_refusal(&score), None);
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
    // Negative seconds never survive `parse_at`, but the selection itself
    // clamps to the first frame rather than wrapping to a huge index.
    assert_eq!(frame_index(Some(-1.0), 15, 10), 0);
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
    let svg = poster_document(&tf, 30, 20).unwrap();
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
    // above it, while the run's `fill` is the foreground. The face rule is
    // recomputed from the fixture (its base64 is kilobytes long).
    let face = embed_for(&BTreeSet::from(['a', 'b', 'c', 'd', ' ']), "IBM Plex Mono")
        .unwrap()
        .1;
    let fam = "'ds-term', 'IBM Plex Mono', monospace";
    let template = concat!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" ",
        "width=\"40\" height=\"40\" viewBox=\"0 0 40 40\">\n",
        "<style>\n{FACE}\n</style>\n",
        "<rect x=\"0\" y=\"0\" width=\"40\" height=\"40\" rx=\"8\" fill=\"#0b0f14\"/>\n",
        "<rect x=\"10\" y=\"0\" width=\"10\" height=\"20\" fill=\"#0050a0\"/>\n",
        "<text x=\"0\" y=\"15\" font-family=\"{FAM}\" font-size=\"16\" ",
        "fill=\"#ff0000\" textLength=\"10\" lengthAdjust=\"spacing\" xml:space=\"preserve\">a</text>\n",
        "<text x=\"10\" y=\"15\" font-family=\"{FAM}\" font-size=\"16\" ",
        "fill=\"#ff0000\" textLength=\"10\" lengthAdjust=\"spacing\" xml:space=\"preserve\">b</text>\n",
        "<text x=\"0\" y=\"35\" font-family=\"{FAM}\" font-size=\"16\" ",
        "font-weight=\"bold\" fill=\"#00ff00\" textLength=\"20\" lengthAdjust=\"spacing\" ",
        "xml:space=\"preserve\">cd</text>\n",
        "</svg>\n",
    );
    let expected = template.replace("{FACE}", &face).replace("{FAM}", fam);
    let doc = poster_document(&tf, 40, 40).unwrap();
    assert_eq!(doc, expected);
    assert_eq!(
        poster_document(&tf, 40, 40).unwrap(),
        doc,
        "must be reproducible"
    );
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

/// The score the replay snapshots below render: a 10×2 terminal, 10 fps.
fn replay_score() -> Score {
    toml::from_str(
        r#"
[demo]
name = "snap"
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

#[test]
fn a_small_replay_exports_a_deterministic_animated_document() {
    let rec = replay_rec();
    let score = replay_score();
    let dir = std::env::temp_dir().join(format!("demostage_svg_animated_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let first = dir.join("first.svg");
    let second = dir.join("second.svg");
    write_animated(&first, &rec, &score).unwrap();
    write_animated(&second, &rec, &score).unwrap();
    let doc = std::fs::read_to_string(&first).unwrap();
    assert_eq!(
        doc,
        std::fs::read_to_string(&second).unwrap(),
        "must be reproducible"
    );
    // Byte-stable snapshot: "ab" is on screen the whole replay, so it is a
    // static persistent run; "cd" arrives at frame 3 of 6, so its group shows
    // over 50%..100% of the 0.6s loop. The face rule is recomputed from the
    // fixture chars (its base64 is kilobytes long); the double write above
    // pins the byte stability.
    let face = embed_for(
        &BTreeSet::from(['a', 'b', 'c', 'd', ' ']),
        "DejaVu Sans Mono",
    )
    .unwrap()
    .1;
    let fam = "'ds-term', 'DejaVu Sans Mono', monospace";
    let expected = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         width=\"100\" height=\"38\" viewBox=\"0 0 100 38\">\n\
         <style>\n{face}\n\
         @keyframes k0{{0%{{opacity:0}}50%{{opacity:1}}100%{{opacity:0}}}}\n\
         #final{{display:none}}\n\
         @media (prefers-reduced-motion: reduce){{#anim{{display:none}}#final{{display:inline}}}}\n\
         </style>\n\
         <defs>\n\
         <g id=\"r0\">\n\
         <text x=\"0\" y=\"33\" font-family=\"{fam}\" font-size=\"16\" fill=\"#c8c8c8\" \
         textLength=\"20\" lengthAdjust=\"spacingAndGlyphs\" \
         xml:space=\"preserve\">cd</text>\n\
         </g>\n\
         </defs>\n\
         <rect x=\"0\" y=\"0\" width=\"100\" height=\"38\" rx=\"8\" fill=\"#0b0f14\"/>\n\
         <g id=\"anim\">\n<g id=\"pb\">\n</g>\n<g id=\"states\">\n\
         <use href=\"#r0\" xlink:href=\"#r0\" \
         style=\"animation:k0 0.6s steps(1,end) infinite;opacity:0\"/>\n\
         </g>\n<g id=\"pt\">\n\
         <text x=\"0\" y=\"14\" font-family=\"{fam}\" font-size=\"16\" fill=\"#c8c8c8\" \
         textLength=\"20\" lengthAdjust=\"spacingAndGlyphs\" \
         xml:space=\"preserve\">ab</text>\n\
         </g>\n</g>\n\
         <g id=\"final\">\n\
         <rect x=\"0\" y=\"0\" width=\"100\" height=\"38\" rx=\"8\" fill=\"#0b0f14\"/>\n\
         <text x=\"0\" y=\"14\" font-family=\"{fam}\" font-size=\"16\" fill=\"#c8c8c8\" \
         textLength=\"20\" lengthAdjust=\"spacingAndGlyphs\" \
         xml:space=\"preserve\">ab</text>\n\
         <text x=\"0\" y=\"33\" font-family=\"{fam}\" font-size=\"16\" fill=\"#c8c8c8\" \
         textLength=\"20\" lengthAdjust=\"spacingAndGlyphs\" \
         xml:space=\"preserve\">cd</text>\n\
         </g>\n\
         </svg>\n",
    );
    assert_eq!(doc, expected, "deterministic snapshot drifted");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_small_replay_exports_a_byte_stable_poster() {
    let rec = replay_rec();
    let score = replay_score();
    let dir = std::env::temp_dir().join(format!("demostage_svg_replay_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("poster.svg");
    // No `--at`: the poster picks the LAST frame of the replay, so this
    // also pins the default frame selection end to end.
    write_svg(&path, &rec, &score, None).unwrap();
    let doc = std::fs::read_to_string(&path).unwrap();
    let face = embed_for(
        &BTreeSet::from(['a', 'b', 'c', 'd', ' ']),
        "DejaVu Sans Mono",
    )
    .unwrap()
    .1;
    let fam = "'ds-term', 'DejaVu Sans Mono', monospace";
    let expected = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         width=\"100\" height=\"38\" viewBox=\"0 0 100 38\">\n\
         <style>\n{face}\n</style>\n\
         <rect x=\"0\" y=\"0\" width=\"100\" height=\"38\" rx=\"8\" fill=\"#0b0f14\"/>\n\
         <text x=\"0\" y=\"14\" font-family=\"{fam}\" font-size=\"16\" fill=\"#c8c8c8\" \
         textLength=\"20\" lengthAdjust=\"spacing\" xml:space=\"preserve\">ab</text>\n\
         <text x=\"0\" y=\"33\" font-family=\"{fam}\" font-size=\"16\" fill=\"#c8c8c8\" \
         textLength=\"20\" lengthAdjust=\"spacing\" xml:space=\"preserve\">cd</text>\n\
         </svg>\n",
    );
    assert_eq!(doc, expected, "deterministic snapshot drifted");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The earliest browser pane wins the refusal message; ties keep the first
/// (`<`, not `<=`: equal times must not dethrone the leader).
#[test]
fn refusal_names_the_earliest_browser_pane() {
    let score: Score = toml::from_str(
        r#"
[demo]
name = "t"
[layout]
width = 300
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "late"
  type = "browser"
  x = 100
  y = 0
  width = 100
  height = 100
  url = "https://late.example"
  reveal_at = 5.0
  [[layout.panes]]
  id = "early"
  type = "browser"
  x = 200
  y = 0
  width = 100
  height = 100
  url = "https://early.example"
  reveal_at = 2.0
"#,
    )
    .unwrap();
    let msg = animated_refusal(&score).expect("staged must refuse");
    assert!(msg.contains('2'), "earliest time missing: {msg}");
    assert!(!msg.contains('5'), "later time must not win: {msg}");
}

/// A single terminal that is offset or resized is not fullscreen: each
/// `is_single_terminal` conjunct refuses on its own (`&&`, not `||`).
#[test]
fn offset_or_resized_terminal_still_refuses() {
    for (label, pane_toml) in [
        ("x offset", "x = 50\ny = 0\nwidth = 100\nheight = 100"),
        ("y offset", "x = 0\ny = 10\nwidth = 100\nheight = 100"),
        ("narrow", "x = 0\ny = 0\nwidth = 80\nheight = 100"),
        ("short", "x = 0\ny = 0\nwidth = 100\nheight = 48"),
    ] {
        let score: Score = toml::from_str(&format!(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  {pane_toml}
"#,
        ))
        .unwrap();
        assert!(
            animated_refusal(&score).is_some(),
            "{label}: a non-fullscreen terminal must refuse"
        );
    }
}

/// A cell with `ch`, the given fg, and an explicit background.
fn bg_cell(ch: char, fg: [u8; 3], bg: [u8; 3]) -> TextCell {
    TextCell {
        ch,
        fg,
        bg,
        bold: false,
    }
}

/// One frame laid out row by row.
fn frame_rows(rows: &[Vec<TextCell>]) -> TextFrame {
    let cols = rows[0].len();
    let cells: Vec<TextCell> = rows.iter().flatten().copied().collect();
    frame(cols, rows.len(), cells)
}

/// `scene_of` maps every lane exactly: background spans, merged runs, and
/// braille cells — but ONLY real braille. The blank U+2800 cell and ordinary
/// characters never become dot items, and the bits are codepoint − U+2800.
#[test]
fn scene_of_builds_the_exact_scene() {
    let tf = frame(
        4,
        2,
        vec![
            cell('a', [255, 0, 0]),
            // a background change splits the run and opens a `<rect>` span
            TextCell {
                ch: 'b',
                fg: [255, 0, 0],
                bg: [0, 80, 160],
                bold: false,
            },
            // ⣿ = U+28FF → dots; the blank cell draws nothing at all
            cell('\u{28ff}', [10, 20, 30]),
            cell('\u{2800}', [10, 20, 30]),
            // row 1: one run whose trailing spaces trim away
            cell('c', [0, 255, 0]),
            cell(' ', [0, 255, 0]),
            cell(' ', [0, 255, 0]),
            cell(' ', [0, 255, 0]),
        ],
    );
    assert_eq!(
        scene_of(&tf),
        Scene {
            bg: vec![BgItem {
                row: 0,
                start: 1,
                cells: 1,
                color: [0, 80, 160],
            }],
            text: vec![
                TextItem {
                    row: 0,
                    start: 0,
                    text: "a".into(),
                    fg: [255, 0, 0],
                    bold: false,
                },
                TextItem {
                    row: 0,
                    start: 1,
                    text: "b".into(),
                    fg: [255, 0, 0],
                    bold: false,
                },
                TextItem {
                    row: 1,
                    start: 0,
                    text: "c".into(),
                    fg: [0, 255, 0],
                    bold: false,
                },
            ],
            dots: vec![DotItem {
                row: 0,
                col: 2,
                bits: 0xff,
                fg: [10, 20, 30],
            }],
            blocks: vec![],
        },
        "scene_of must map every lane exactly"
    );
}

/// Frames fold into first-seen ids and hold windows `[start, end)`: a repeat
/// extends the running hold, a reprise of an earlier scene opens a new one.
#[test]
fn fold_scenes_keeps_first_seen_ids_and_exact_hold_bounds() {
    let a = scene_of(&frame(2, 1, text_row("aa", 2)));
    let b = scene_of(&frame(2, 1, text_row("bb", 2)));
    let (unique, holds) = fold_scenes(vec![a.clone(), a.clone(), b.clone(), a.clone()]);
    assert_eq!(unique, vec![a, b], "ids must be assigned first-seen");
    let spans: Vec<(usize, usize, usize)> =
        holds.iter().map(|h| (h.scene, h.start, h.end)).collect();
    assert_eq!(
        spans,
        vec![(0, 0, 2), (1, 2, 3), (0, 3, 4)],
        "consecutive frames extend the run; a reprise opens a new hold"
    );
}

/// `windows_for` closes a window the moment its item misses a hold, keeps
/// first-seen order, and leaves windows open until the end of the timeline.
#[test]
fn windows_for_reports_exact_first_seen_windows() {
    let per_hold = vec![vec![1, 2], vec![2], vec![1, 3]];
    assert_eq!(
        windows_for(&per_hold),
        vec![
            (1, vec![(0, 1), (2, 3)]),
            (2, vec![(0, 2)]),
            (3, vec![(2, 3)]),
        ],
        "first-seen order, closed on absence, ends exclusive"
    );
    assert_eq!(
        windows_for::<i32>(&[]),
        Vec::<(i32, Vec<(usize, usize)>)>::new(),
        "no holds, no windows"
    );
}

/// The whole three-state timeline as ONE byte-exact document. Every lane is
/// represented: a persistent background span (`#pb`), a persistent text run
/// and a persistent braille cell (`#pt`), transient residuals in the state
/// groups (`<defs>`), one `<use>` per scene, all six keyframe rules in
/// emission order (k0…k5), and the reduced-motion `#final` group.
#[test]
fn the_full_timeline_document_is_byte_exact() {
    // State 0: K (blue background + run) then "aaa"; ⣿ on row 1.
    let f0 = frame_rows(&[
        vec![
            TextCell {
                ch: 'K',
                fg: [255, 255, 255],
                bg: [0, 80, 160],
                bold: false,
            },
            cell('a', [200, 200, 200]),
            cell('a', [200, 200, 200]),
            cell('a', [200, 200, 200]),
        ],
        vec![
            cell('\u{28ff}', [10, 20, 30]),
            cell(' ', [200, 200, 200]),
            cell(' ', [200, 200, 200]),
            cell(' ', [200, 200, 200]),
        ],
    ]);
    // State 1: K survives, the run after it churns; the braille cell survives.
    let f1 = frame_rows(&[
        vec![
            TextCell {
                ch: 'K',
                fg: [255, 255, 255],
                bg: [0, 80, 160],
                bold: false,
            },
            cell('b', [200, 200, 200]),
            cell('b', [200, 200, 200]),
            cell('b', [200, 200, 200]),
        ],
        vec![
            cell('\u{28ff}', [10, 20, 30]),
            cell(' ', [200, 200, 200]),
            cell(' ', [200, 200, 200]),
            cell(' ', [200, 200, 200]),
        ],
    ]);
    // State 2: K and ⣿ are gone; a green run and a lone ⠁ appear instead.
    let f2 = frame_rows(&[
        vec![
            cell('x', [255, 0, 0]),
            bg_cell('y', [200, 200, 200], [80, 160, 0]),
            bg_cell('y', [200, 200, 200], [80, 160, 0]),
            bg_cell('y', [200, 200, 200], [80, 160, 0]),
        ],
        vec![
            cell(' ', [200, 200, 200]),
            cell(' ', [200, 200, 200]),
            cell(' ', [200, 200, 200]),
            cell('\u{2801}', [40, 50, 60]),
        ],
    ]);
    let face = animated_face(&[f0.clone(), f1.clone(), f2.clone()]).1;
    let fam = "'ds-term', 'IBM Plex Mono', monospace";
    let doc = animated_doc(&[f0, f1, f2], 40, 40, 1);
    let template = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="40" height="40" viewBox="0 0 40 40">
<style>
{FACE}
@keyframes k0{0%{opacity:1}66.6667%{opacity:0}100%{opacity:0}}
@keyframes k1{0%{opacity:1}66.6667%{opacity:0}100%{opacity:0}}
@keyframes k2{0%{opacity:1}66.6667%{opacity:0}100%{opacity:0}}
@keyframes k3{0%{opacity:1}33.3333%{opacity:0}100%{opacity:0}}
@keyframes k4{0%{opacity:0}33.3333%{opacity:1}66.6667%{opacity:0}100%{opacity:0}}
@keyframes k5{0%{opacity:0}66.6667%{opacity:1}100%{opacity:0}}
#final{display:none}
@media (prefers-reduced-motion: reduce){#anim{display:none}#final{display:inline}}
</style>
<defs>
<g id="r0">
<text x="10" y="15" font-family="{FAM}" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">aaa</text>
</g>
<g id="r1">
<text x="10" y="15" font-family="{FAM}" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">bbb</text>
</g>
<g id="r2">
<rect x="10" y="0" width="30" height="20" fill="#50a000"/>
<circle cx="32" cy="22" r="2" fill="#28323c"/>
<text x="0" y="15" font-family="{FAM}" font-size="16" fill="#ff0000" textLength="10" lengthAdjust="spacingAndGlyphs" xml:space="preserve">x</text>
<text x="10" y="15" font-family="{FAM}" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">yyy</text>
</g>
</defs>
<rect x="0" y="0" width="40" height="40" rx="8" fill="#0b0f14"/>
<g id="anim">
<g id="pb">
<rect x="0" y="0" width="10" height="20" fill="#0050a0" style="animation:k0 3s steps(1,end) infinite;opacity:0"/>
</g>
<g id="states">
<use href="#r0" xlink:href="#r0" style="animation:k3 3s steps(1,end) infinite;opacity:0"/>
<use href="#r1" xlink:href="#r1" style="animation:k4 3s steps(1,end) infinite;opacity:0"/>
<use href="#r2" xlink:href="#r2" style="animation:k5 3s steps(1,end) infinite;opacity:0"/>
</g>
<g id="pt">
<text x="0" y="15" font-family="{FAM}" font-size="16" fill="#ffffff" textLength="10" lengthAdjust="spacingAndGlyphs" xml:space="preserve" style="animation:k1 3s steps(1,end) infinite;opacity:0">K</text>
<g style="animation:k2 3s steps(1,end) infinite;opacity:0"><circle cx="2" cy="22" r="2" fill="#0a141e"/> <circle cx="2" cy="27" r="2" fill="#0a141e"/> <circle cx="2" cy="32" r="2" fill="#0a141e"/> <circle cx="2" cy="37" r="2" fill="#0a141e"/> <circle cx="7" cy="22" r="2" fill="#0a141e"/> <circle cx="7" cy="27" r="2" fill="#0a141e"/> <circle cx="7" cy="32" r="2" fill="#0a141e"/> <circle cx="7" cy="37" r="2" fill="#0a141e"/></g>
</g>
</g>
<g id="final">
<rect x="0" y="0" width="40" height="40" rx="8" fill="#0b0f14"/>
<rect x="10" y="0" width="30" height="20" fill="#50a000"/>
<circle cx="32" cy="22" r="2" fill="#28323c"/>
<text x="0" y="15" font-family="{FAM}" font-size="16" fill="#ff0000" textLength="10" lengthAdjust="spacingAndGlyphs" xml:space="preserve">x</text>
<text x="10" y="15" font-family="{FAM}" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">yyy</text>
</g>
</svg>
"##;
    let expected = template.replace("{FACE}", &face).replace("{FAM}", fam);
    assert_eq!(
        doc, expected,
        "the three-state timeline must assemble byte for byte"
    );
}

/// A scene that recurs non-consecutively keeps ONE group and ONE `<use>`, but
/// its rule must open over BOTH of its windows (frames 0..1 and 2..3 of 3).
#[test]
fn a_recurring_state_gets_one_rule_with_two_visible_windows() {
    let a = frame(3, 1, text_row("aaa", 3));
    let b = frame(3, 1, text_row("bbb", 3));
    let doc = animated_doc(&[a.clone(), b, a], 30, 20, 3);
    assert!(
        doc.contains(
            "@keyframes k0{0%{opacity:1}33.3333%{opacity:0}66.6667%{opacity:1}100%{opacity:0}}"
        ),
        "the reprise must be its own visible window:\n{doc}"
    );
    assert!(
        doc.contains(
            "@keyframes k1{0%{opacity:0}33.3333%{opacity:1}66.6667%{opacity:0}100%{opacity:0}}"
        ),
        "the middle state's window drifted:\n{doc}"
    );
    assert_eq!(
        doc.matches("<use href=\"#r0\"").count(),
        1,
        "one <use> per scene id, however many windows:\n{doc}"
    );
    assert_eq!(doc.matches("<g id=\"r0\">").count(), 1, ":\n{doc}");
    assert!(
        doc.contains(
            "<use href=\"#r0\" xlink:href=\"#r0\" style=\"animation:k0 1s steps(1,end) infinite;opacity:0\"/>"
        ),
        "exact <use> row with its animation style:\n{doc}"
    );
}

/// One hold (no change over the whole timeline) renders the STATIC document:
/// header, the single `@font-face`, canvas, markup — no `<defs>`, no anim.
#[test]
fn a_single_hold_timeline_renders_the_static_document() {
    let f = frame(3, 1, text_row("hi!", 3));
    let face = animated_face(&[f.clone(), f.clone()]).1;
    let fam = "'ds-term', 'IBM Plex Mono', monospace";
    let doc = animated_doc(&[f.clone(), f], 30, 20, 5);
    let template = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="30" height="20" viewBox="0 0 30 20">
<style>
{FACE}
#final{display:none}
@media (prefers-reduced-motion: reduce){#anim{display:none}#final{display:inline}}
</style>
<rect x="0" y="0" width="30" height="20" rx="8" fill="#0b0f14"/>
<text x="0" y="15" font-family="{FAM}" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">hi!</text>
</svg>
"##;
    let expected = template.replace("{FACE}", &face).replace("{FAM}", fam);
    assert_eq!(
        doc, expected,
        "a degenerate timeline must fall back to the static document"
    );
}

/// End-to-end through the writer: a replay whose screen never changes walks
/// into a single hold, so `write_animated` must persist the STATIC document.
#[test]
fn a_constant_replay_writes_the_static_document() {
    let rec = Recording {
        cols: 10,
        rows: 2,
        title: "t".into(),
        events: vec![(0.0, "ab".into())],
        captions: vec![],
        focuses: vec![],
        duration: 0.5,
    };
    let score = replay_score();
    let dir = std::env::temp_dir().join(format!("demostage_svg_static_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("static.svg");
    write_animated(&path, &rec, &score).unwrap();
    let doc = std::fs::read_to_string(&path).unwrap();
    let face = embed_for(&BTreeSet::from(['a', 'b', ' ']), "DejaVu Sans Mono")
        .unwrap()
        .1;
    let fam = "'ds-term', 'DejaVu Sans Mono', monospace";
    let template = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="38" viewBox="0 0 100 38">
<style>
{FACE}
#final{display:none}
@media (prefers-reduced-motion: reduce){#anim{display:none}#final{display:inline}}
</style>
<rect x="0" y="0" width="100" height="38" rx="8" fill="#0b0f14"/>
<text x="0" y="14" font-family="{FAM}" font-size="16" fill="#c8c8c8" textLength="20" lengthAdjust="spacingAndGlyphs" xml:space="preserve">ab</text>
</svg>
"##;
    let expected = template.replace("{FACE}", &face).replace("{FAM}", fam);
    assert_eq!(
        doc, expected,
        "a one-state replay must be written as the static document"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `write_animated` announces its state count and size on stderr. libtest
/// captures stderr in-process with no way to read it back, so the test re-runs
/// ITSELF in a child process with `--nocapture` and asserts the exact line.
#[test]
fn write_animated_announces_its_state_count_and_size_on_stderr() {
    const CHILD: &str = "DEMOSTAGE_WRITE_ANIMATED_STDERR_CHILD";
    let rec = replay_rec();
    let score = replay_score();
    let dir = std::env::temp_dir().join(format!("demostage_svg_stderr_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    if std::env::var_os(CHILD).is_some() {
        write_animated(&dir.join("child.svg"), &rec, &score).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let path = dir.join("parent.svg");
    write_animated(&path, &rec, &score).unwrap();
    let doc = std::fs::read_to_string(&path).unwrap();
    let expected = format!(
        "demo: animated svg: 2 states, {:.1} KB\n",
        doc.len() as f64 / 1024.0
    );
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("write_animated_announces_its_state_count_and_size_on_stderr")
        .arg("--nocapture")
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "child failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "the child must run exactly this one test:\n{stdout}"
    );
    assert_eq!(
        stderr, expected,
        "the status line must report the real state count and size"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two browser panes revealing at the SAME instant: `<` (not `<=`) keeps the
/// FIRST one, and every field of the message is exact.
#[test]
fn equal_reveal_times_refuse_the_first_browser_pane() {
    let score: Score = toml::from_str(
        r#"
[demo]
name = "t"
[layout]
width = 300
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "first"
  type = "browser"
  x = 100
  y = 0
  width = 100
  height = 100
  url = "file:///first.pdf"
  reveal_at = 4.0
  [[layout.panes]]
  id = "second"
  type = "browser"
  x = 200
  y = 0
  width = 100
  height = 100
  url = "file:///second.png"
  reveal_at = 4.0
"#,
    )
    .unwrap();
    assert_eq!(
        animated_refusal(&score).as_deref(),
        Some(
            "animated svg supports terminal-only demos; this score shows a PDF pane at 4.0s \
             — use gif/mp4, or --at <s> for a poster"
        ),
        "ties must keep the FIRST pane; kind, article, time and guidance must all be exact"
    );
}

/// `pane_kind_label` splits query strings off before sniffing the extension.
#[test]
fn a_browser_url_with_a_query_still_labels_by_extension() {
    let score: Score = toml::from_str(
        r#"
[demo]
name = "t"
[layout]
width = 200
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "p"
  type = "browser"
  x = 100
  y = 0
  width = 100
  height = 100
  url = "file:///x.pdf?start=2"
  reveal_at = 1.5
"#,
    )
    .unwrap();
    assert_eq!(
        animated_refusal(&score).as_deref(),
        Some(
            "animated svg supports terminal-only demos; this score shows a PDF pane at 1.5s \
             — use gif/mp4, or --at <s> for a poster"
        ),
        "the query string must not hide the .pdf extension"
    );
}

/// The single-terminal fast lane needs EXACTLY one pane: a fullsize first
/// pane must not smuggle a second pane past `panes.len() != 1`.
#[test]
fn a_fullsize_first_pane_still_refuses_when_the_layout_holds_two_panes() {
    let score: Score = toml::from_str(
        r#"
[demo]
name = "t"
[layout]
width = 200
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 200
  height = 100
  [[layout.panes]]
  id = "p"
  type = "browser"
  x = 0
  y = 50
  width = 200
  height = 50
  url = "file:///x.pdf"
  reveal_at = 2.0
"#,
    )
    .unwrap();
    assert_eq!(
        animated_refusal(&score).as_deref(),
        Some(
            "animated svg supports terminal-only demos; this score shows a PDF pane at 2.0s \
             — use gif/mp4, or --at <s> for a poster"
        ),
        "the fast lane is exactly one pane; a fullsize first pane must not claim it"
    );
}

/// Braille bits are codepoint minus U+2800: `-`→`+` overflows the `u8` and
/// maps every cell to blank, so each nonzero pattern below kills it.
#[test]
fn braille_bits_subtract_the_base() {
    assert_eq!(braille_bits('\u{2801}'), 0x01);
    assert_eq!(braille_bits('\u{2847}'), 0x47);
    assert_eq!(braille_bits('\u{28ff}'), 0xff);
}

/// `scene_hold_windows` returns each maximal consecutive run of holds showing
/// `sid`, ends exclusive: a split scene yields two windows, a trailing run
/// ends at `holds.len()`, and gaps stay gaps.
#[test]
fn scene_hold_windows_groups_consecutive_runs_with_exact_bounds() {
    let holds = vec![
        Hold {
            scene: 0,
            start: 0,
            end: 1,
        },
        Hold {
            scene: 1,
            start: 1,
            end: 2,
        },
        Hold {
            scene: 0,
            start: 2,
            end: 3,
        },
        Hold {
            scene: 0,
            start: 3,
            end: 4,
        },
        Hold {
            scene: 2,
            start: 4,
            end: 5,
        },
    ];
    // Scene 0 appears at hold 0 alone, then as the consecutive pair 2..4:
    // `end = hold + 1` (not `*`: 2 + 1 = 3, not 2) and the inner scan keeps
    // extending while the next hold matches (not the opposite).
    assert_eq!(scene_hold_windows(&holds, 0), vec![(0, 1), (2, 4)]);
    // A lone middle scene is exactly its own hold.
    assert_eq!(scene_hold_windows(&holds, 1), vec![(1, 2)]);
    // The trailing run ends at `holds.len()` (5), not at its start.
    assert_eq!(scene_hold_windows(&holds, 2), vec![(4, 5)]);
}

/// No hold shows the scene (or there are no holds at all): no windows.
/// Flipping `==` to `!=` would return every other hold here instead.
#[test]
fn scene_hold_windows_with_no_match_is_empty() {
    let holds = vec![
        Hold {
            scene: 0,
            start: 0,
            end: 1,
        },
        Hold {
            scene: 1,
            start: 1,
            end: 2,
        },
    ];
    assert!(scene_hold_windows(&holds, 7).is_empty());
    assert!(scene_hold_windows(&[], 0).is_empty());
}

/// Every hold shows the scene: one window spanning the whole table.
#[test]
fn scene_hold_windows_covering_every_hold_is_one_window() {
    let holds = vec![
        Hold {
            scene: 3,
            start: 0,
            end: 1,
        },
        Hold {
            scene: 3,
            start: 1,
            end: 2,
        },
        Hold {
            scene: 3,
            start: 2,
            end: 3,
        },
    ];
    assert_eq!(scene_hold_windows(&holds, 3), vec![(0, 3)]);
}

/// An exported animated SVG carries exactly one `@font-face` with a TTF data
/// source (spec: the README `<img>` cannot load fonts any other way).
#[test]
fn animated_export_embeds_exactly_one_ttf_font_face() {
    let dir = std::env::temp_dir().join(format!("demostage_svg_face_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("face.svg");
    write_animated(&path, &replay_rec(), &replay_score()).unwrap();
    let doc = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        doc.matches("@font-face").count(),
        1,
        "exactly one embedded face:\n{doc}"
    );
    assert!(
        doc.contains("data:font/ttf;base64,"),
        "the face must be a TTF data URI:\n{doc}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The embedded subset parses and covers exactly the states' glyphs: every
/// character the walks show (that the original font maps) plus space, and
/// nothing outside that set.
#[test]
fn the_embedded_subset_covers_exactly_the_states_glyphs() {
    let dir = std::env::temp_dir().join(format!("demostage_svg_cover_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("cover.svg");
    write_animated(&path, &replay_rec(), &replay_score()).unwrap();
    let doc = std::fs::read_to_string(&path).unwrap();
    let face = doc
        .lines()
        .find(|l| l.contains("@font-face"))
        .expect("one face rule");
    let subset = fontdue::Font::from_bytes(subset_bytes(face), fontdue::FontSettings::default())
        .expect("the embedded font must parse");
    // The replay shows "ab" then "cd": those four plus the always-kept space.
    let expected: BTreeSet<char> = ['a', 'b', 'c', 'd', ' '].into();
    for c in &expected {
        assert!(subset.has_glyph(*c), "subset lost {c:?}");
    }
    for mapped in subset.chars().keys() {
        assert!(
            expected.contains(mapped),
            "subset maps {mapped:?} outside the states plus space"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every `<text>` in both targets names the embedded face first.
#[test]
fn every_text_names_the_embed_first_in_both_targets() {
    let a = frame(3, 1, text_row("aaa", 3));
    let b = frame(3, 1, text_row("zzz", 3));
    let animated = animated_doc(&[a, b.clone()], 30, 20, 2);
    let poster = poster_document(&b, 30, 20).unwrap();
    let lead = format!("'{EMBED_FAMILY}',");
    for (tag, doc) in [("animated", animated), ("poster", poster)] {
        assert!(doc.contains("<text"), "{tag} has no text:\n{doc}");
        for part in doc.split("font-family=\"").skip(1) {
            assert!(
                part.starts_with(&lead),
                "{tag} text does not name the embed first:\n{part}"
            );
        }
    }
}

/// A character the bundled font lacks keeps the fallback behaviour: the
/// export still succeeds, the run still names the embed first (the viewer
/// falls back for that glyph), and the subset does not claim it.
#[test]
fn a_character_the_font_lacks_keeps_the_fallback() {
    let tf = frame(
        2,
        1,
        vec![
            cell('a', [200, 200, 200]),
            cell('\u{1f680}', [200, 200, 200]),
        ],
    );
    let doc = animated_doc(&[tf.clone(), tf], 20, 20, 2);
    assert!(
        doc.contains("font-family=\"'ds-term', 'IBM Plex Mono', monospace\""),
        "the run still names the embed first:\n{doc}"
    );
    let face = doc
        .lines()
        .find(|l| l.contains("@font-face"))
        .expect("one face rule");
    let subset = fontdue::Font::from_bytes(subset_bytes(face), fontdue::FontSettings::default())
        .expect("the embedded font must parse");
    assert!(subset.has_glyph('a'), "used glyph lost");
    assert!(
        !subset.has_glyph('\u{1f680}'),
        "a glyph the font lacks must stay out of the subset"
    );
}

/// The vector poster embeds the same single-face shape; the staged PNG
/// fallback embeds no font at all.
#[test]
fn poster_embeds_one_face_while_the_staged_fallback_embeds_none() {
    let tf = frame(2, 1, text_row("hi", 2));
    let doc = poster_document(&tf, 20, 20).unwrap();
    assert_eq!(
        doc.matches("@font-face").count(),
        1,
        "exactly one embedded face:\n{doc}"
    );
    assert!(
        doc.contains("data:font/ttf;base64,"),
        "the face must be a TTF data URI:\n{doc}"
    );
    let dir = std::env::temp_dir().join(format!("demostage_svg_nofont_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("staged.svg");
    encode(&path, 2, 2, 0, |emit| {
        emit(&PosterFrame::Rgba(&[128u8; 2 * 2 * 4]));
        Ok(())
    })
    .unwrap();
    let staged = std::fs::read_to_string(&path).unwrap();
    assert!(
        !staged.contains("@font-face"),
        "the PNG fallback must not embed a font:\n{staged}"
    );
    assert!(
        !staged.contains("<text"),
        "the PNG fallback must not fake vector text"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every character inside a `<text>…</text>` node — the shape assertion both
/// targets share: no block element may ride as a glyph.
fn text_nodes(doc: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = doc;
    while let Some(start) = rest.find("<text") {
        let after = &rest[start..];
        let Some(open) = after.find('>') else {
            break;
        };
        let Some(close) = after[open..].find("</text>") else {
            break;
        };
        out.push(after[open + 1..open + close].to_string());
        rest = &after[open + close..];
    }
    out
}

/// The poster paints block cells as `<rect>` geometry: the four blocks break
/// the text runs either side of them, no `<text>` node holds a block glyph, and
/// the geometry carries the raster's own coverage (a full-cell shade at
/// 64/255, a solid full block).
#[test]
fn poster_block_cells_break_runs_and_ride_as_rects() {
    let tf = frame(
        7,
        1,
        vec![
            cell('a', [255, 255, 255]),
            cell('░', [255, 255, 255]),
            cell('▒', [255, 255, 255]),
            cell('▓', [255, 255, 255]),
            cell('█', [255, 255, 255]),
            cell(' ', [255, 255, 255]),
            cell('b', [255, 255, 255]),
        ],
    );
    // The blocks break the run: only the two letters survive, and the leading
    // space before "b" trims its start (column 5 → 6).
    let runs: Vec<(usize, String)> = merge_runs(&tf, 0)
        .into_iter()
        .map(|r| (r.start, r.text))
        .collect();
    assert_eq!(
        runs,
        vec![(0, "a".to_string()), (6, "b".to_string())],
        "a block must break the text run"
    );

    let doc = poster_document(&tf, 70, 20).unwrap();
    for node in text_nodes(&doc) {
        assert!(
            !node.chars().any(is_block),
            "block glyph {node:?} rode a <text> node"
        );
    }
    // Cell 10×20, row 0: ░ at col 1 → 64/255 = 0.251; █ at col 4 → opaque.
    assert!(
        doc.contains(
            "<rect x=\"10\" y=\"0\" width=\"10\" height=\"20\" \
                      fill=\"#ffffff\" fill-opacity=\"0.251\"/>"
        ),
        "the light shade must be one full-cell rect:\n{doc}"
    );
    assert!(
        doc.contains("<rect x=\"40\" y=\"0\" width=\"10\" height=\"20\" fill=\"#ffffff\"/>"),
        "the full block must be one opaque full-cell rect:\n{doc}"
    );
    assert!(
        doc.contains("font-family=\"'ds-term',"),
        "the letter runs still name the embed first:\n{doc}"
    );
}

/// The animated target folds block cells through the same lane as the braille
/// dots: `scene_of` collects a `BlockItem`, the document renders its `<rect>`
/// geometry, no `<text>` node holds a block glyph, and the timing/dedup shape
/// is unchanged (one `<use>` per distinct scene, a keyframes rule per item).
#[test]
fn animated_block_cells_are_scene_items_not_text() {
    let f0 = frame(
        5,
        1,
        vec![
            cell('a', [200, 200, 200]),
            cell('░', [255, 0, 0]),
            cell('▒', [255, 0, 0]),
            cell('▓', [255, 0, 0]),
            cell('█', [255, 0, 0]),
        ],
    );
    // The ░▒▓█ run survives into the second frame, so it spans two holds and
    // becomes persistent while the letter churns under it. The third frame
    // drops the blocks, which gives their window an end shorter than the
    // timeline.
    let f1 = frame(
        5,
        1,
        vec![
            cell('b', [200, 200, 200]),
            cell('░', [255, 0, 0]),
            cell('▒', [255, 0, 0]),
            cell('▓', [255, 0, 0]),
            cell('█', [255, 0, 0]),
        ],
    );
    let f2 = frame(5, 1, text_row("ccc", 5));
    assert_eq!(
        scene_of(&f0).blocks,
        vec![
            BlockItem {
                row: 0,
                col: 1,
                ch: '░',
                fg: [255, 0, 0],
            },
            BlockItem {
                row: 0,
                col: 2,
                ch: '▒',
                fg: [255, 0, 0],
            },
            BlockItem {
                row: 0,
                col: 3,
                ch: '▓',
                fg: [255, 0, 0],
            },
            BlockItem {
                row: 0,
                col: 4,
                ch: '█',
                fg: [255, 0, 0],
            },
        ],
        "each block cell must become exactly one BlockItem"
    );
    assert!(
        scene_of(&f2).blocks.is_empty(),
        "a frame without a block has no block item"
    );

    let doc = animated_doc(&[f0, f1, f2], 50, 20, 3);
    for node in text_nodes(&doc) {
        assert!(
            !node.chars().any(is_block),
            "block glyph {node:?} rode a <text> node"
        );
    }
    // Cell 10×20: ░▒▓█ at cols 1..4 → 64/128/192/255, drawn ONCE together in
    // the persistent `#pt` layer over frames 0..2 of 3 (one second at 3 fps) —
    // the shared frame window groups them under one rule, the same fold the
    // braille dots ride.
    assert!(
        doc.contains(
            "<g style=\"animation:k0 1s steps(1,end) infinite;opacity:0\">\
             <rect x=\"10\" y=\"0\" width=\"10\" height=\"20\" \
             fill=\"#ff0000\" fill-opacity=\"0.251\"/> \
             <rect x=\"20\" y=\"0\" width=\"10\" height=\"20\" \
             fill=\"#ff0000\" fill-opacity=\"0.502\"/> \
             <rect x=\"30\" y=\"0\" width=\"10\" height=\"20\" \
             fill=\"#ff0000\" fill-opacity=\"0.753\"/> \
             <rect x=\"40\" y=\"0\" width=\"10\" height=\"20\" \
             fill=\"#ff0000\"/></g>"
        ),
        "the shades and full block must ride as animated rect geometry:\n{doc}"
    );
    assert!(
        doc.contains("@keyframes k0{0%{opacity:1}66.6667%{opacity:0}100%{opacity:0}}"),
        "the persistent block must carry its own keyframes rule:\n{doc}"
    );
    // Timing and dedup are unchanged: three distinct states, one `<use>` each.
    for (gid, key) in [("r0", "k1"), ("r1", "k2"), ("r2", "k3")] {
        let use_row = format!(
            "<use href=\"#{gid}\" xlink:href=\"#{gid}\" \
             style=\"animation:{key} 1s steps(1,end) infinite;opacity:0\"/>"
        );
        assert_eq!(
            doc.matches(&use_row).count(),
            1,
            "one <use> per distinct scene:\n{doc}"
        );
    }
}
