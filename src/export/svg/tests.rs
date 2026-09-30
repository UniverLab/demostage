use super::animated::{
    fold_scenes, render_document, scene_of, windows_for, write_animated, BgItem, DocInput, DotItem,
    Geom, Scene, TextItem, ANIM_FAMILY,
};
use super::animated_refusal;
use super::paint::{escape_attr, escape_xml, merge_runs};
use super::poster::{base64, encode, frame_index, poster_document, write_svg, PosterFrame};
use super::timing::keyframes_rule;
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
    };
    render_document(&input)
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
fn animated_runs_use_the_monospace_stack_and_spacing_and_glyphs() {
    let a = frame(2, 1, text_row("ab", 2));
    let b = frame(2, 1, text_row("cd", 2));
    let doc = animated_doc(&[a, b], 20, 20, 2);
    assert!(doc.contains("ui-monospace"), "fixed stack missing:\n{doc}");
    assert!(
        doc.contains("DejaVu Sans Mono"),
        "fixed stack missing:\n{doc}"
    );
    assert!(
        doc.contains(&escape_attr(ANIM_FAMILY)),
        "the stack must ride escaped in font-family:\n{doc}"
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
    // over 50%..100% of the 0.6s loop.
    let expected = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="38" viewBox="0 0 100 38">
<style>
@keyframes k0{0%{opacity:0}50%{opacity:1}100%{opacity:0}}
#final{display:none}
@media (prefers-reduced-motion: reduce){#anim{display:none}#final{display:inline}}
</style>
<defs>
<g id="r0">
<text x="0" y="33" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="20" lengthAdjust="spacingAndGlyphs" xml:space="preserve">cd</text>
</g>
</defs>
<rect x="0" y="0" width="100" height="38" rx="8" fill="#0b0f14"/>
<g id="anim">
<g id="pb">
</g>
<g id="states">
<use href="#r0" xlink:href="#r0" style="animation:k0 0.6s steps(1,end) infinite;opacity:0"/>
</g>
<g id="pt">
<text x="0" y="14" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="20" lengthAdjust="spacingAndGlyphs" xml:space="preserve">ab</text>
</g>
</g>
<g id="final">
<rect x="0" y="0" width="100" height="38" rx="8" fill="#0b0f14"/>
<text x="0" y="14" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="20" lengthAdjust="spacingAndGlyphs" xml:space="preserve">ab</text>
<text x="0" y="33" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="20" lengthAdjust="spacingAndGlyphs" xml:space="preserve">cd</text>
</g>
</svg>
"##;
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
    let expected = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="38" viewBox="0 0 100 38">
<rect x="0" y="0" width="100" height="38" rx="8" fill="#0b0f14"/>
<text x="0" y="14" font-family="'DejaVu Sans Mono', monospace" font-size="16" fill="#c8c8c8" textLength="20" lengthAdjust="spacing" xml:space="preserve">ab</text>
<text x="0" y="33" font-family="'DejaVu Sans Mono', monospace" font-size="16" fill="#c8c8c8" textLength="20" lengthAdjust="spacing" xml:space="preserve">cd</text>
</svg>
"##;
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
    let doc = animated_doc(&[f0, f1, f2], 40, 40, 1);
    let expected = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="40" height="40" viewBox="0 0 40 40">
<style>
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
<text x="10" y="15" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">aaa</text>
</g>
<g id="r1">
<text x="10" y="15" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">bbb</text>
</g>
<g id="r2">
<rect x="10" y="0" width="30" height="20" fill="#50a000"/>
<circle cx="32" cy="22" r="2" fill="#28323c"/>
<text x="0" y="15" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#ff0000" textLength="10" lengthAdjust="spacingAndGlyphs" xml:space="preserve">x</text>
<text x="10" y="15" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">yyy</text>
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
<text x="0" y="15" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#ffffff" textLength="10" lengthAdjust="spacingAndGlyphs" xml:space="preserve" style="animation:k1 3s steps(1,end) infinite;opacity:0">K</text>
<g style="animation:k2 3s steps(1,end) infinite;opacity:0"><circle cx="2" cy="22" r="2" fill="#0a141e"/> <circle cx="2" cy="27" r="2" fill="#0a141e"/> <circle cx="2" cy="32" r="2" fill="#0a141e"/> <circle cx="2" cy="37" r="2" fill="#0a141e"/> <circle cx="7" cy="22" r="2" fill="#0a141e"/> <circle cx="7" cy="27" r="2" fill="#0a141e"/> <circle cx="7" cy="32" r="2" fill="#0a141e"/> <circle cx="7" cy="37" r="2" fill="#0a141e"/></g>
</g>
</g>
<g id="final">
<rect x="0" y="0" width="40" height="40" rx="8" fill="#0b0f14"/>
<rect x="10" y="0" width="30" height="20" fill="#50a000"/>
<circle cx="32" cy="22" r="2" fill="#28323c"/>
<text x="0" y="15" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#ff0000" textLength="10" lengthAdjust="spacingAndGlyphs" xml:space="preserve">x</text>
<text x="10" y="15" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">yyy</text>
</g>
</svg>
"##;
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
/// no `<style>`, no `<defs>`, no `<g id="anim">` — header, canvas, markup.
#[test]
fn a_single_hold_timeline_renders_the_static_document() {
    let f = frame(3, 1, text_row("hi!", 3));
    let doc = animated_doc(&[f.clone(), f], 30, 20, 5);
    let expected = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="30" height="20" viewBox="0 0 30 20">
<rect x="0" y="0" width="30" height="20" rx="8" fill="#0b0f14"/>
<text x="0" y="15" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="30" lengthAdjust="spacingAndGlyphs" xml:space="preserve">hi!</text>
</svg>
"##;
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
    let expected = r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="38" viewBox="0 0 100 38">
<rect x="0" y="0" width="100" height="38" rx="8" fill="#0b0f14"/>
<text x="0" y="14" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, &quot;DejaVu Sans Mono&quot;, monospace" font-size="16" fill="#c8c8c8" textLength="20" lengthAdjust="spacingAndGlyphs" xml:space="preserve">ab</text>
</svg>
"##;
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
