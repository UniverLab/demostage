//! The animated SVG: the whole timeline of a terminal-only demo as vector
//! text. Every distinct screen state becomes a `<g>` of merged text runs,
//! shown and hidden with CSS `@keyframes` using `steps()` timing so each state
//! holds for its real duration; the animation loops forever. Consecutive
//! states share a persistent layer (runs drawn once, spanning their lifetime)
//! and recurring states reuse one group via `<use href>`. No scripts, no
//! external references, no `<foreignObject>` — it renders in a README `<img>`.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;

use super::paint::{bg_spans, braille_dots, escape_attr, escape_xml, hex, is_braille, merge_runs};
use super::paint::{row_cells, Run};
use super::timing::{duration_secs, item_rule, style_for, KeyGen};
use crate::error::{Error, Result};
use crate::export::raster::{FrameSource, TextFrame};
use crate::export::run::Recording;
use crate::model::Score;

/// Monospace stack for the animated target: no embedded fonts, so the grid
/// holds with whatever monospace the viewer has (`textLength` pins the width).
pub(crate) const ANIM_FAMILY: &str =
    "ui-monospace, SFMono-Regular, Menlo, Consolas, \"DejaVu Sans Mono\", monospace";

/// One merged text run placed on the grid.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TextItem {
    pub(crate) row: usize,
    pub(crate) start: usize,
    pub(crate) text: String,
    pub(crate) fg: [u8; 3],
    pub(crate) bold: bool,
}

/// One background span placed on the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BgItem {
    pub(crate) row: usize,
    pub(crate) start: usize,
    pub(crate) cells: usize,
    pub(crate) color: [u8; 3],
}

/// One braille cell: `bits` is the codepoint minus U+2800.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct DotItem {
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) bits: u8,
    pub(crate) fg: [u8; 3],
}

/// One distinct screen state, in deterministic (row) order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub(crate) struct Scene {
    pub(crate) bg: Vec<BgItem>,
    pub(crate) text: Vec<TextItem>,
    pub(crate) dots: Vec<DotItem>,
}

impl Scene {
    fn is_empty(&self) -> bool {
        self.bg.is_empty() && self.text.is_empty() && self.dots.is_empty()
    }
}

/// One maximal run of consecutive frames showing the same scene: frames
/// `[start, end)`, i.e. `end` is exclusive.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hold {
    pub(crate) scene: usize,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Pixel geometry shared by every frame of one demo.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Geom {
    pub(crate) cw: usize,
    pub(crate) ch: usize,
    pub(crate) px: f32,
}

/// Everything [`render_document`] needs, bundled so the assembler takes one
/// argument.
pub(crate) struct DocInput<'a> {
    pub(crate) unique: &'a [Scene],
    pub(crate) holds: &'a [Hold],
    pub(crate) n_frames: usize,
    pub(crate) fps: u32,
    pub(crate) w: usize,
    pub(crate) h: usize,
    pub(crate) geom: Geom,
    pub(crate) default_bg: [u8; 3],
    pub(crate) final_tf: &'a TextFrame,
}

/// The items that survive consecutive holds, drawn once in a persistent layer.
struct Persistent {
    bg: HashSet<BgItem>,
    text: HashSet<TextItem>,
    dots: HashSet<DotItem>,
}

/// The scene of one grid frame: background spans, merged runs, braille cells.
pub(crate) fn scene_of(tf: &TextFrame) -> Scene {
    let mut scene = Scene::default();
    for span in bg_spans(tf) {
        scene.bg.push(BgItem {
            row: span.row,
            start: span.start,
            cells: span.cells,
            color: span.color,
        });
    }
    for row in 0..tf.rows {
        for run in merge_runs(tf, row) {
            let Run {
                start, text, fg, ..
            } = &run;
            scene.text.push(TextItem {
                row,
                start: *start,
                text: text.clone(),
                fg: *fg,
                bold: run.bold,
            });
        }
    }
    collect_dots(tf, &mut scene);
    scene
}

/// Braille cells become dot items (never text): viewer fonts lack U+2800–U+28FF.
fn collect_dots(tf: &TextFrame, scene: &mut Scene) {
    for row in 0..tf.rows {
        let Some(cells) = row_cells(tf, row) else {
            break;
        };
        for (col, cell) in cells.iter().enumerate() {
            if is_braille(cell.ch) && cell.ch != '\u{2800}' {
                scene.dots.push(DotItem {
                    row,
                    col,
                    bits: (cell.ch as u32 - 0x2800) as u8,
                    fg: cell.fg,
                });
            }
        }
    }
}

/// Fold grid frames into distinct scenes plus consecutive holds.
pub(crate) fn fold_scenes(scenes: Vec<Scene>) -> (Vec<Scene>, Vec<Hold>) {
    let mut unique: Vec<Scene> = Vec::new();
    let mut index: HashMap<Scene, usize> = HashMap::new();
    let mut holds: Vec<Hold> = Vec::new();
    for (f, scene) in scenes.into_iter().enumerate() {
        let id = match index.get(&scene) {
            Some(&id) => id,
            None => {
                let id = unique.len();
                index.insert(scene.clone(), id);
                unique.push(scene);
                id
            }
        };
        match holds.last_mut() {
            Some(hold) if hold.scene == id => hold.end = f + 1,
            _ => holds.push(Hold {
                scene: id,
                start: f,
                end: f + 1,
            }),
        }
    }
    (unique, holds)
}

/// Maximal consecutive hold windows per item, in first-seen order:
/// `[(item, [(first_hold, end_hold), …]), …]` with ends exclusive.
pub(crate) fn windows_for<T: Clone + Eq + Hash>(
    per_hold: &[Vec<T>],
) -> Vec<(T, Vec<(usize, usize)>)> {
    let mut windows: HashMap<T, Vec<(usize, usize)>> = HashMap::new();
    let mut active: HashMap<T, usize> = HashMap::new();
    let mut order: Vec<T> = Vec::new();
    let mut seen: HashSet<T> = HashSet::new();
    for (h, items) in per_hold.iter().enumerate() {
        close_missing(items, h, &mut active, &mut windows);
        for item in items {
            if active.contains_key(item) {
                continue;
            }
            if seen.insert(item.clone()) {
                order.push(item.clone());
            }
            active.insert(item.clone(), h);
        }
    }
    for (key, start) in std::mem::take(&mut active) {
        windows
            .entry(key)
            .or_default()
            .push((start, per_hold.len()));
    }
    windows_for_ordered(order, &mut windows)
}

/// Close the windows of items absent from this hold.
fn close_missing<T: Clone + Eq + Hash>(
    items: &[T],
    hold: usize,
    active: &mut HashMap<T, usize>,
    windows: &mut HashMap<T, Vec<(usize, usize)>>,
) {
    let live: HashSet<&T> = items.iter().collect();
    let stale: Vec<T> = active
        .keys()
        .filter(|k| !live.contains(*k))
        .cloned()
        .collect();
    for key in stale {
        if let Some(start) = active.remove(&key) {
            windows.entry(key).or_default().push((start, hold));
        }
    }
}

/// Drain the ordered keys against their collected windows.
fn windows_for_ordered<T: Clone + Eq + Hash>(
    order: Vec<T>,
    windows: &mut HashMap<T, Vec<(usize, usize)>>,
) -> Vec<(T, Vec<(usize, usize)>)> {
    order
        .into_iter()
        .map(|key| {
            let spans = windows.remove(&key).unwrap_or_default();
            (key, spans)
        })
        .collect()
}

/// A persistent item spans at least two consecutive holds in one window.
fn is_persistent(windows: &[(usize, usize)]) -> bool {
    windows.iter().any(|(s, e)| e - s >= 2)
}

/// Hold windows → frame spans via the holds table.
fn hold_windows_to_frames(windows: &[(usize, usize)], holds: &[Hold]) -> Vec<(usize, usize)> {
    windows
        .iter()
        .map(|(s, e)| (holds[*s].start, holds[e - 1].end))
        .collect()
}

/// Split the timeline's items into the persistent layer.
fn split_persistent(unique: &[Scene], holds: &[Hold]) -> Persistent {
    let bg = persistent_set(holds, unique, |s| &s.bg);
    let text = persistent_set(holds, unique, |s| &s.text);
    let dots = persistent_set(holds, unique, |s| &s.dots);
    Persistent { bg, text, dots }
}

/// The items of one lane that span consecutive holds.
fn persistent_set<T: Clone + Eq + Hash>(
    holds: &[Hold],
    unique: &[Scene],
    lane: impl Fn(&Scene) -> &[T],
) -> HashSet<T> {
    let per_hold: Vec<Vec<T>> = holds
        .iter()
        .map(|h| lane(&unique[h.scene]).to_vec())
        .collect();
    windows_for(&per_hold)
        .into_iter()
        .filter(|(_, w)| is_persistent(w))
        .map(|(item, _)| item)
        .collect()
}

/// The scene minus its persistent items: what the state group still must draw.
fn residual_of(scene: &Scene, persistent: &Persistent) -> Scene {
    Scene {
        bg: scene
            .bg
            .iter()
            .filter(|i| !persistent.bg.contains(*i))
            .cloned()
            .collect(),
        text: scene
            .text
            .iter()
            .filter(|i| !persistent.text.contains(*i))
            .cloned()
            .collect(),
        dots: scene
            .dots
            .iter()
            .filter(|i| !persistent.dots.contains(*i))
            .cloned()
            .collect(),
    }
}

/// One background span as a `<rect>`.
fn anim_bg_xml(item: &BgItem, geom: &Geom, extra: &str) -> String {
    format!(
        "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\"{extra}/>",
        item.start * geom.cw,
        item.row * geom.ch,
        item.cells * geom.cw,
        geom.ch,
        hex(item.color)
    )
}

/// One merged run as a `<text>` on the fixed monospace stack.
fn anim_text_xml(item: &TextItem, geom: &Geom, extra: &str) -> String {
    let chars = item.text.chars().count();
    let x = item.start * geom.cw;
    let y = item.row * geom.ch + geom.ch / 2 + (geom.px * 0.35) as usize;
    let bold = if item.bold {
        " font-weight=\"bold\""
    } else {
        ""
    };
    format!(
        "<text x=\"{x}\" y=\"{y}\" font-family=\"{}\" font-size=\"{}\"{bold} \
         fill=\"{}\" textLength=\"{}\" lengthAdjust=\"spacingAndGlyphs\" \
         xml:space=\"preserve\"{extra}>{}</text>",
        escape_attr(ANIM_FAMILY),
        geom.px.round() as usize,
        hex(item.fg),
        chars * geom.cw,
        escape_xml(&item.text)
    )
}

/// One braille cell as `<circle>` dots, wrapped in `<g>` when animated.
fn anim_dots_xml(item: &DotItem, geom: &Geom, extra: &str) -> String {
    let ch = char::from_u32(0x2800 + u32::from(item.bits)).unwrap_or('\u{2800}');
    let dots = braille_dots(item.row, item.col, ch, item.fg, geom.cw, geom.ch);
    if dots.is_empty() || extra.is_empty() {
        return dots;
    }
    format!("<g{extra}>{dots}</g>")
}

/// A scene's items as static markup: backgrounds, then dots, then text.
fn residual_markup(scene: &Scene, geom: &Geom) -> String {
    let mut out = String::new();
    for item in &scene.bg {
        out.push_str(&anim_bg_xml(item, geom, ""));
        out.push('\n');
    }
    for item in &scene.dots {
        let dots = anim_dots_xml(item, geom, "");
        if dots.is_empty() {
            continue;
        }
        out.push_str(&dots);
        out.push('\n');
    }
    for item in &scene.text {
        out.push_str(&anim_text_xml(item, geom, ""));
        out.push('\n');
    }
    out
}

/// The persistent background layer (`#pb`).
fn persistent_bg(
    input: &DocInput,
    keys: &mut KeyGen,
    total_secs: f64,
    rules: &mut Vec<String>,
) -> String {
    let per_hold: Vec<Vec<BgItem>> = input
        .holds
        .iter()
        .map(|h| input.unique[h.scene].bg.clone())
        .collect();
    let mut out = String::new();
    for (item, windows) in windows_for(&per_hold) {
        if !is_persistent(&windows) {
            continue;
        }
        let frames = hold_windows_to_frames(&windows, input.holds);
        let key = item_rule(keys, rules, &frames, input.n_frames);
        out.push_str(&anim_bg_xml(
            &item,
            &input.geom,
            &style_for(&key, total_secs),
        ));
        out.push('\n');
    }
    out
}

/// The persistent text + braille layer (`#pt`).
fn persistent_fg(
    input: &DocInput,
    keys: &mut KeyGen,
    total_secs: f64,
    rules: &mut Vec<String>,
) -> String {
    let mut out = String::new();
    out.push_str(&persistent_text(input, keys, total_secs, rules));
    out.push_str(&persistent_dots(input, keys, total_secs, rules));
    out
}

/// Persistent text runs, each drawn once across its consecutive holds.
fn persistent_text(
    input: &DocInput,
    keys: &mut KeyGen,
    total_secs: f64,
    rules: &mut Vec<String>,
) -> String {
    let per_hold: Vec<Vec<TextItem>> = input
        .holds
        .iter()
        .map(|h| input.unique[h.scene].text.clone())
        .collect();
    let mut out = String::new();
    for (item, windows) in windows_for(&per_hold) {
        if !is_persistent(&windows) {
            continue;
        }
        let frames = hold_windows_to_frames(&windows, input.holds);
        let key = item_rule(keys, rules, &frames, input.n_frames);
        out.push_str(&anim_text_xml(
            &item,
            &input.geom,
            &style_for(&key, total_secs),
        ));
        out.push('\n');
    }
    out
}

/// Persistent braille cells, each drawn once across its consecutive holds.
fn persistent_dots(
    input: &DocInput,
    keys: &mut KeyGen,
    total_secs: f64,
    rules: &mut Vec<String>,
) -> String {
    let per_hold: Vec<Vec<DotItem>> = input
        .holds
        .iter()
        .map(|h| input.unique[h.scene].dots.clone())
        .collect();
    let mut out = String::new();
    for (item, windows) in windows_for(&per_hold) {
        if !is_persistent(&windows) {
            continue;
        }
        let frames = hold_windows_to_frames(&windows, input.holds);
        let key = item_rule(keys, rules, &frames, input.n_frames);
        let dots = anim_dots_xml(&item, &input.geom, &style_for(&key, total_secs));
        if dots.is_empty() {
            continue;
        }
        out.push_str(&dots);
        out.push('\n');
    }
    out
}

/// The state groups (`<defs>`) plus one `<use>` per scene id (`#states`).
struct StateLayers {
    defs: String,
    body: String,
    rules: Vec<String>,
}

fn state_layers(
    input: &DocInput,
    persistent: &Persistent,
    keys: &mut KeyGen,
    total_secs: f64,
) -> StateLayers {
    let residuals: Vec<Scene> = input
        .unique
        .iter()
        .map(|s| residual_of(s, persistent))
        .collect();
    let mut gid_of: HashMap<Scene, usize> = HashMap::new();
    let mut order: Vec<Scene> = Vec::new();
    for scene in &residuals {
        if scene.is_empty() || gid_of.contains_key(scene) {
            continue;
        }
        gid_of.insert(scene.clone(), order.len());
        order.push(scene.clone());
    }
    let mut defs = String::new();
    for (gid, scene) in order.iter().enumerate() {
        defs.push_str(&format!("<g id=\"r{gid}\">\n"));
        defs.push_str(&residual_markup(scene, &input.geom));
        defs.push_str("</g>\n");
    }
    let mut body = String::new();
    let mut rules = Vec::new();
    for (sid, scene) in residuals.iter().enumerate() {
        if scene.is_empty() {
            continue;
        }
        let gid = gid_of[scene];
        let held: Vec<(usize, usize)> = scene_hold_windows(input.holds, sid);
        let frames = hold_windows_to_frames(&held, input.holds);
        let key = item_rule(keys, &mut rules, &frames, input.n_frames);
        let style = style_for(&key, total_secs);
        body.push_str(&format!(
            "<use href=\"#r{gid}\" xlink:href=\"#r{gid}\"{style}/>\n"
        ));
    }
    StateLayers { defs, body, rules }
}

/// The consecutive hold runs showing scene `sid`.
fn scene_hold_windows(holds: &[Hold], sid: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut hold = 0;
    while hold < holds.len() {
        if holds[hold].scene != sid {
            hold += 1;
            continue;
        }
        let mut end = hold + 1;
        while end < holds.len() && holds[end].scene == sid {
            end += 1;
        }
        out.push((hold, end));
        hold = end;
    }
    out
}

/// The `<svg>` header plus the `<style>` (keyframes, then the reduced-motion
/// rule showing the final state statically).
fn doc_header(w: usize, h: usize) -> String {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\" \
         width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\n"
    )
}

/// Rounded canvas background.
fn canvas_rect(w: usize, h: usize, bg: [u8; 3]) -> String {
    format!(
        "<rect x=\"0\" y=\"0\" width=\"{w}\" height=\"{h}\" rx=\"8\" fill=\"{}\"/>\n",
        hex(bg)
    )
}

fn style_block(rules: &[String]) -> String {
    let mut out = String::from("<style>\n");
    for rule in rules {
        out.push_str(rule);
        out.push('\n');
    }
    out.push_str("#final{display:none}\n");
    out.push_str(
        "@media (prefers-reduced-motion: reduce){#anim{display:none}#final{display:inline}}\n",
    );
    out.push_str("</style>\n");
    out
}

/// The static final state, shown only under `prefers-reduced-motion`.
///
/// It is emitted as a SIBLING of `#anim`, never inside `<defs>`: defs children
/// are not rendered however their `display` is set, so a `#final` parked there
/// would leave a reduced-motion viewer with a blank canvas.
fn final_group(input: &DocInput) -> String {
    let mut out = String::from("<g id=\"final\">\n");
    out.push_str(&canvas_rect(input.w, input.h, input.default_bg));
    out.push_str(&residual_markup(&scene_of(input.final_tf), &input.geom));
    out.push_str("</g>\n");
    out
}

/// Degenerate timeline (no change, or one frame): the final state, static.
fn static_document(tf: &TextFrame, w: usize, h: usize) -> String {
    let geom = Geom {
        cw: tf.cell_w,
        ch: tf.cell_h,
        px: tf.px,
    };
    let mut out = doc_header(w, h);
    out.push_str(&canvas_rect(w, h, tf.default_bg));
    out.push_str(&residual_markup(&scene_of(tf), &geom));
    out.push_str("</svg>\n");
    out
}

/// The whole animated document for one walked timeline.
pub(crate) fn render_document(input: &DocInput) -> String {
    if input.holds.len() <= 1 {
        return static_document(input.final_tf, input.w, input.h);
    }
    let total_secs = duration_secs(input.n_frames, input.fps);
    let persistent = split_persistent(input.unique, input.holds);
    let mut keys = KeyGen::new();
    let mut rules: Vec<String> = Vec::new();
    let bg_layer = persistent_bg(input, &mut keys, total_secs, &mut rules);
    let fg_layer = persistent_fg(input, &mut keys, total_secs, &mut rules);
    let states = state_layers(input, &persistent, &mut keys, total_secs);
    rules.extend(states.rules);
    let mut out = doc_header(input.w, input.h);
    out.push_str(&style_block(&rules));
    out.push_str("<defs>\n");
    out.push_str(&states.defs);
    out.push_str("</defs>\n");
    out.push_str(&canvas_rect(input.w, input.h, input.default_bg));
    out.push_str("<g id=\"anim\">\n<g id=\"pb\">\n");
    out.push_str(&bg_layer);
    out.push_str("</g>\n<g id=\"states\">\n");
    out.push_str(&states.body);
    out.push_str("</g>\n<g id=\"pt\">\n");
    out.push_str(&fg_layer);
    out.push_str("</g>\n</g>\n");
    // Last in document order so the static frame covers the whole canvas.
    out.push_str(&final_group(input));
    out.push_str("</svg>\n");
    out
}

/// Walk the whole replay once (allocation only — nothing rasterized), fold it
/// into scenes + holds, and write the animated SVG.
pub fn write_animated(path: &Path, rec: &Recording, score: &Score) -> Result<()> {
    let mut source = FrameSource::new(rec, score)?;
    let (w, h) = source.dims();
    let fps = score.layout.fps.max(1);
    let mut scenes: Vec<Scene> = Vec::new();
    let mut geom = Geom {
        cw: 10,
        ch: 20,
        px: 16.0,
    };
    let mut default_bg = [11, 15, 20];
    let mut seen_first = false;
    // Bounded: the source yields exactly `n_frames` frames, so walk them by
    // count — the walk ends on its own whatever the per-frame result is.
    let n = source.n_frames();
    for _ in 0..n {
        let Some(tf) = source.next_text_frame() else {
            break;
        };
        if !seen_first {
            geom = Geom {
                cw: tf.cell_w,
                ch: tf.cell_h,
                px: tf.px,
            };
            default_bg = tf.default_bg;
            seen_first = true;
        }
        scenes.push(scene_of(&tf));
    }
    if scenes.is_empty() {
        return Err(Error::Export("svg: no frames were emitted".to_string()));
    }
    let final_tf = source.text_frame();
    let (unique, holds) = fold_scenes(scenes);
    let input = DocInput {
        unique: &unique,
        holds: &holds,
        // The frames actually walked, not `n_frames()`: the holds index real
        // frames, so the percentages that place every state on the timeline
        // must be scaled by however many the walk produced.
        n_frames: holds.last().map_or(0, |h| h.end),
        fps,
        w,
        h,
        geom,
        default_bg,
        final_tf: &final_tf,
    };
    let doc = render_document(&input);
    std::fs::write(path, &doc).map_err(|e| Error::io(path, e))?;
    eprintln!(
        "demo: animated svg: {} states, {:.1} KB",
        unique.len(),
        doc.len() as f64 / 1024.0
    );
    Ok(())
}
