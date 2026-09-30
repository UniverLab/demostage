//! The recording artifact (`.rec`) that `demo record` writes and `demo export`
//! plays back.
//!
//! It is a JSON header line followed by `[t,"o",data]` timestamped output lines;
//! the header carries the demo-stage render config (layout, typing, captions,
//! focuses) under a `demostage` key, so `export` reconstructs everything it needs
//! to render **without re-executing** the demo.
//!
//! `read` also accepts a raw capture (`macro.raw.toml`): it renders the live
//! session's recorded output directly, with a default single-terminal layout.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::run::Recording;
use crate::error::{Error, Result};
use crate::model::{
    DemoMeta, Layout, Orientation, Pane, PaneKind, RawEvent, RawMacro, RevealPane, Score,
    ScrollDirection, Step, Typing, Velocity,
};

/// Assumed monospace cell size (px), matching the recorder's geometry.
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;
/// Minimum padding around the terminal pane so it doesn't touch the canvas edges.
const MIN_PAD: u32 = 32;

/// The extra demo-stage render config stashed in the cast header.
#[derive(Serialize, Deserialize)]
struct DemoStageMeta {
    demo: DemoMeta,
    layout: Layout,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    typing: Option<Typing>,
    #[serde(default)]
    captions: Vec<(f64, String)>,
    #[serde(default)]
    focuses: Vec<(f64, String)>,
    /// Browser-scroll steps for staged playback (never executed — they only tell
    /// the stage how to animate a scrolling pane). Without persisting them, a
    /// `--scroll` reveal would lose its scroll on the write→read round trip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    timeline: Vec<Step>,
    /// `true` for a faithful capture (real output, typing/idle as recorded);
    /// `false` for a `demo record` run (normalized typing & spacing).
    #[serde(default)]
    faithful: bool,
    /// Total playback duration in seconds, including any trailing hold after the
    /// last output (e.g. a browser scene revealed at the end). Without this, a
    /// reader would recompute duration from the last output event and cut the hold.
    #[serde(default)]
    duration: f64,
}

/// Serialize a recording plus its render config: a JSON header line (with the
/// `demostage` render config) followed by timestamped output lines. `faithful`
/// marks a real capture (vs a normalized `record` run) so `export` can flag it.
pub fn write(rec: &Recording, score: &Score, faithful: bool) -> Result<String> {
    let meta = DemoStageMeta {
        demo: score.demo.clone(),
        layout: score.layout.clone(),
        typing: score.typing.clone(),
        captions: rec.captions.clone(),
        focuses: rec.focuses.clone(),
        timeline: score.timeline.clone(),
        faithful,
        duration: rec.duration,
    };
    let header = json!({
        "version": 2,
        "width": rec.cols,
        "height": rec.rows,
        "title": rec.title,
        "env": { "TERM": "xterm-256color" },
        "demostage": meta,
    });

    let mut out = serde_json::to_string(&header)?;
    out.push('\n');
    for (t, data) in &rec.events {
        out.push_str(&serde_json::to_string(&json!([t, "o", data]))?);
        out.push('\n');
    }
    Ok(out)
}

/// Load a recording for playback. Accepts either a `.cast` (from `demo record`)
/// or a raw capture (`macro.raw.toml`). Returns the recording plus a score that
/// carries the layout/styling to render it (its timeline is empty — playback
/// replays the recorded events, it never executes the timeline).
pub fn read(path: &Path) -> Result<(Recording, Score, bool)> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    if text.trim_start().starts_with('{') {
        read_cast(&text)
    } else {
        // A raw capture is always faithful (real output, not re-typed).
        let (rec, score) = read_raw(path, &text)?;
        Ok((rec, score, true))
    }
}

fn read_cast(text: &str) -> Result<(Recording, Score, bool)> {
    let mut lines = text.lines();
    let header: Value = lines
        .next()
        .ok_or_else(|| Error::Export("empty cast".to_string()))
        .and_then(|l| {
            serde_json::from_str(l).map_err(|e| Error::Export(format!("cast header: {e}")))
        })?;

    // A rec captured from a size-less terminal can carry 0×0 — rendering that
    // panics in the VT parser, so treat it as absent.
    let cols = match header["width"].as_u64() {
        Some(c) if c > 0 => c as u16,
        _ => 80,
    };
    let rows = match header["height"].as_u64() {
        Some(r) if r > 0 => r as u16,
        _ => 24,
    };
    let title = header["title"].as_str().unwrap_or("demo").to_string();

    let mut events = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let ev: Value =
            serde_json::from_str(line).map_err(|e| Error::Export(format!("cast event: {e}")))?;
        if let (Some(t), Some("o"), Some(data)) = (
            ev.get(0).and_then(Value::as_f64),
            ev.get(1).and_then(Value::as_str),
            ev.get(2).and_then(Value::as_str),
        ) {
            events.push((t, data.to_string()));
        }
    }
    let last_event = events.last().map(|(t, _)| *t).unwrap_or(0.0);

    // Recover the demo-stage render config if this cast came from `demo record`;
    // otherwise fall back to a default single-terminal layout.
    let (score, captions, focuses, faithful, meta_duration) = match header.get("demostage") {
        Some(v) => {
            let meta: DemoStageMeta = serde_json::from_value(v.clone())
                .map_err(|e| Error::Export(format!("cast demostage header: {e}")))?;
            let score = Score {
                demo: meta.demo,
                env: None,
                typing: meta.typing,
                sources: vec![],
                layout: meta.layout,
                // Only browser-scroll steps survive the round trip; playback
                // still never executes a timeline.
                timeline: meta.timeline,
            };
            (
                score,
                meta.captions,
                meta.focuses,
                meta.faithful,
                meta.duration,
            )
        }
        None => (
            default_score("demo", cols, rows),
            Vec::new(),
            Vec::new(),
            false,
            0.0,
        ),
    };
    // Honour the recorded duration (it may extend past the last output to hold a
    // trailing scene); fall back to the last event for older casts.
    let duration = meta_duration.max(last_event);

    Ok((
        Recording {
            cols,
            rows,
            title,
            events,
            captions,
            focuses,
            duration,
        },
        score,
        faithful,
    ))
}

fn read_raw(path: &Path, text: &str) -> Result<(Recording, Score)> {
    let raw: RawMacro =
        toml::from_str(text).map_err(|e| Error::Export(format!("{}: {e}", path.display())))?;
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.strip_suffix(".raw").unwrap_or(s))
        .unwrap_or("demo");
    let (rec, layout, timeline) = from_raw(&raw, name);
    Ok((rec, score_with_layout(name, layout, timeline)))
}

/// Wrap a layout in a playback score. The timeline carries only browser-scene
/// scroll steps (which drive how many scroll keyframes the stage captures);
/// playback never executes it — it replays the recorded events.
fn score_with_layout(name: &str, layout: Layout, timeline: Vec<Step>) -> Score {
    Score {
        demo: DemoMeta {
            name: name.to_string(),
            output_dir: "./dist".into(),
            prompt: None,
            speed: None,
            targets: None,
        },
        env: None,
        typing: None,
        sources: vec![],
        layout,
        timeline,
    }
}

/// Longest pause kept between consecutive output chunks when normalizing a
/// faithful recording — longer waits (thinking, network) are trimmed to this, so
/// playback feels deliberate without dead air.
const MAX_GAP_MS: f64 = 1200.0;
/// How long the final frame is held after the last output, so the result is
/// readable before the demo ends / loops (instead of cutting off abruptly).
const TAIL_HOLD_MS: f64 = 1800.0;
/// Default time a browser scene stays on screen when no explicit `--hold` is
/// given — long enough to actually read the page (a plain reveal near the end of
/// the capture otherwise just flashes by); longer still when scrolling.
const DEFAULT_REVEAL_HOLD_MS: f64 = 6000.0;
const DEFAULT_SCROLL_HOLD_MS: f64 = 8000.0;

/// How long a reveal should keep its scene on screen, in milliseconds: an
/// explicit `--hold`, else a longer default for scrolling scenes, else the
/// standard reveal hold.
fn reveal_hold_ms(hold: Option<u64>, scroll: bool) -> f64 {
    match hold {
        Some(h) => h as f64,
        None if scroll => DEFAULT_SCROLL_HOLD_MS,
        None => DEFAULT_REVEAL_HOLD_MS,
    }
}

/// Build a playback recording from a raw capture's real output stream, with its
/// **timing normalized**: the trailing `demo stop` you typed is dropped, and idle
/// gaps are trimmed. Input events are dropped — the output already echoes them.
///
/// Note: typo-pruning and re-humanized typing can't be applied to a faithful
/// recording (the keystrokes are already baked into the output) — those need
/// re-execution via `demo record`. Timing is what we can clean here.
pub fn from_raw(raw: &RawMacro, name: &str) -> (Recording, Layout, Vec<Step>) {
    let cutoff = stop_cutoff_ms(raw);
    let cap = MAX_GAP_MS / 1000.0;

    // Walk output + `demo open` reveals in order, trimming idle gaps; the reveals
    // ride the same trimmed clock so they fire at the right composited moment.
    let mut events: Vec<(f64, String)> = Vec::new();
    let mut reveals: Vec<Reveal> = Vec::new();
    let mut acc = 0.0;
    let mut prev_ms: Option<u64> = None;
    for e in &raw.events {
        let t_ms = match e {
            RawEvent::Output { t_ms, .. } | RawEvent::Reveal { t_ms, .. } => *t_ms,
            // Input/Secret carry no rendered output — the program already echoed
            // them (a secret as a mask), so faithful playback ignores them.
            RawEvent::Input { .. } | RawEvent::Secret { .. } => continue,
        };
        if cutoff.is_some_and(|c| t_ms >= c) {
            continue;
        }
        // Drop output inside a meta-command span (the echo + wizard of a
        // `demo focus`/`demo open`) without advancing the clock, so the region
        // collapses away. Reveal events sit at the span's end and are kept.
        if matches!(e, RawEvent::Output { .. }) && raw.meta.is_muted(t_ms) {
            continue;
        }
        let gap = match prev_ms {
            Some(p) => t_ms.saturating_sub(p) as f64 / 1000.0,
            None => t_ms as f64 / 1000.0,
        };
        acc += gap.clamp(0.0, cap);
        prev_ms = Some(t_ms);
        match e {
            RawEvent::Output { data, .. } => events.push((acc, data.clone())),
            RawEvent::Reveal {
                panes,
                orientation,
                hold_ms,
                scroll,
                t_ms: _,
            } => reveals.push(Reveal {
                t: acc,
                panes: panes.clone(),
                orientation: *orientation,
                hold_ms: *hold_ms,
                scroll: *scroll,
            }),
            RawEvent::Input { .. } | RawEvent::Secret { .. } => {}
        }
    }

    // Hold the final frame so the last result is readable, and keep any browser
    // reveal on screen for at least its hold window past where it opened.
    let tail = events
        .last()
        .map(|(t, _)| *t + TAIL_HOLD_MS / 1000.0)
        .unwrap_or(0.0);
    let reveal_tail = reveals
        .iter()
        .map(|r| r.t + reveal_hold_ms(r.hold_ms, r.scroll) / 1000.0)
        .fold(0.0_f64, f64::max);
    let duration = tail.max(reveal_tail);

    let (cols, rows) = (raw.meta.cols, raw.meta.rows);
    let (layout, focuses, timeline) =
        build_layout(cols, rows, raw.meta.resolution, raw.meta.fps, &reveals);

    (
        Recording {
            cols,
            rows,
            title: name.to_string(),
            events,
            captions: Vec::new(),
            focuses,
            duration,
        },
        layout,
        timeline,
    )
}

/// A reveal on the trimmed playback clock: the panes to show, how arranged.
struct Reveal {
    t: f64,
    panes: Vec<RevealPane>,
    orientation: Orientation,
    hold_ms: Option<u64>,
    scroll: bool,
}

/// Pane rectangles for a reveal: 1 pane fills the canvas; 2 split it by
/// `orientation` (horizontal → left/right, vertical → top/bottom).
/// Applies minimum padding so panes don't touch canvas edges.
fn pane_rects(
    tw: u32,
    th: u32,
    count: usize,
    orientation: Orientation,
) -> Vec<(u32, u32, u32, u32)> {
    let pad = MIN_PAD;
    let inner_w = tw.saturating_sub(2 * pad);
    let inner_h = th.saturating_sub(2 * pad);
    match count {
        0 | 1 => vec![(pad, pad, inner_w, inner_h)],
        _ => match orientation {
            Orientation::Horizontal => {
                let half = inner_w / 2;
                vec![
                    (pad, pad, half, inner_h),
                    (pad + half, pad, inner_w - half, inner_h),
                ]
            }
            Orientation::Vertical => {
                let half = inner_h / 2;
                vec![
                    (pad, pad, inner_w, half),
                    (pad, pad + half, inner_w, inner_h - half),
                ]
            }
        },
    }
}

/// Build the render layout from a capture's reveals. The canvas is the chosen
/// `resolution` (else the terminal size) and the terminal pane is the always-on
/// background, centered when the canvas is larger. Each reveal overlays its
/// browser panes for a window `[reveal_at, hide_at)` — `hide_at` being the
/// next reveal — so switching sources, or returning to the terminal (a reveal
/// with only the terminal pane), works. A `scroll` reveal adds scroll steps.
fn build_layout(
    cols: u16,
    rows: u16,
    resolution: Option<(u32, u32)>,
    fps: Option<u32>,
    reveals: &[Reveal],
) -> (Layout, Vec<(f64, String)>, Vec<Step>) {
    let tw = (cols as u32 * CELL_W).max(CELL_W);
    let th = (rows as u32 * CELL_H).max(CELL_H);
    // A faithful capture replays the recorded grid, so a smaller resolution
    // can't shrink it — the canvas is at least the terminal's pixel size.
    let (cw, ch) = resolution
        .map(|(w, h)| (w.max(tw), h.max(th)))
        .unwrap_or((tw, th));
    // Center the terminal with minimum padding so it doesn't touch canvas edges
    let tx = MIN_PAD.saturating_add((cw.saturating_sub(tw).saturating_sub(2 * MIN_PAD)) / 2);
    let ty = MIN_PAD.saturating_add((ch.saturating_sub(th).saturating_sub(2 * MIN_PAD)) / 2);
    let mut panes = vec![terminal_pane(tx, ty, tw, th)];
    let mut focuses = Vec::new();
    let mut timeline = Vec::new();

    for (i, r) in reveals.iter().enumerate() {
        let reveal_at = r.t;
        let hide_at = reveals.get(i + 1).map(|n| n.t);
        let rects = pane_rects(cw, ch, r.panes.len(), r.orientation);
        for (idx, p) in r.panes.iter().enumerate() {
            // The terminal is the background; only browser panes become overlays.
            if p.is_terminal() {
                continue;
            }
            let (x, y, w, h) = rects.get(idx).copied().unwrap_or((0, 0, cw, ch));
            // Unique per reveal so re-showing a source captures it afresh.
            let id = format!("{}-r{}", p.id, i + 1);
            let url = p.url.clone().unwrap_or_default();
            let mut pane = browser_pane(&id, &url, x, y, w, h, p.theme.clone());
            pane.reveal_at = Some(reveal_at);
            pane.hide_at = hide_at;
            panes.push(pane);
            focuses.push((reveal_at, id.clone()));
            if r.scroll {
                timeline.push(Step::Focus {
                    pane: Some(id.clone()),
                });
                timeline.push(Step::Scroll {
                    direction: ScrollDirection::Down,
                    velocity: Velocity::Constant,
                    duration_ms: reveal_hold_ms(r.hold_ms, r.scroll) as u64,
                    pane: Some(id),
                });
            }
        }
    }

    (
        Layout {
            width: cw,
            height: ch,
            fps: fps.unwrap_or(15),
            line_height: 1.2,
            background: Some("#0b0f14".to_string()),
            font_family: None,
            font_size: None,
            panes,
        },
        focuses,
        timeline,
    )
}

fn terminal_pane(x: u32, y: u32, width: u32, height: u32) -> Pane {
    Pane {
        id: "main".to_string(),
        kind: PaneKind::Terminal,
        x,
        y,
        width,
        height,
        font_family: Some("monospace".to_string()),
        font_size: Some(16),
        url: None,
        theme: None,
        reveal_at: None,
        hide_at: None,
        ignore_speed: false,
    }
}

fn browser_pane(
    id: &str,
    url: &str,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    theme: Option<String>,
) -> Pane {
    Pane {
        id: id.to_string(),
        kind: PaneKind::Browser,
        x,
        y,
        width,
        height,
        font_family: None,
        font_size: None,
        url: Some(url.to_string()),
        theme,
        reveal_at: None,
        hide_at: None,
        ignore_speed: false,
    }
}

/// Does this completed input line end the capture (`demo stop`)?
fn is_stop_line(line: &str) -> bool {
    line.trim() == crate::STOP_COMMAND
}

/// Fold one recorded input chunk into the stop-line scan: `line`/`line_start`
/// track the command currently being typed, and `cutoff` remembers where that
/// command started when it turns out to be `demo stop`.
fn feed_stop_line(
    line: &mut String,
    line_start: &mut Option<u64>,
    t_ms: u64,
    cutoff: &mut Option<u64>,
    bytes: &str,
) {
    for ch in bytes.chars() {
        match ch {
            '\r' | '\n' => {
                if is_stop_line(line) {
                    *cutoff = *line_start;
                }
                line.clear();
                *line_start = None;
            }
            c if c.is_control() => {}
            c => {
                line_start.get_or_insert(t_ms);
                line.push(c);
            }
        }
    }
}

/// The `t_ms` at which the user started typing the final `demo stop` line, if the
/// capture ended that way — so its echo (and the "stopping" message) is dropped.
pub fn stop_cutoff_ms(raw: &RawMacro) -> Option<u64> {
    let mut line = String::new();
    let mut line_start: Option<u64> = None;
    let mut cutoff: Option<u64> = None;
    for e in &raw.events {
        let RawEvent::Input { t_ms, bytes } = e else {
            continue;
        };
        feed_stop_line(&mut line, &mut line_start, *t_ms, &mut cutoff, bytes);
    }
    if is_stop_line(&line) {
        cutoff = cutoff.or(line_start);
    }
    cutoff
}

/// A plain single-terminal score sized to a `cols`×`rows` capture, used to render
/// a recording that carries no layout of its own.
pub fn default_score(name: &str, cols: u16, rows: u16) -> Score {
    let (layout, _, timeline) = build_layout(cols, rows, None, None, &[]);
    score_with_layout(name, layout, timeline)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Recording, Score) {
        let rec = Recording {
            cols: 80,
            rows: 24,
            title: "t".into(),
            events: vec![(0.1, "hi".into()), (0.5, "\r\n".into())],
            captions: vec![(0.2, "step one".into())],
            focuses: vec![],
            duration: 0.5,
        };
        (rec, default_score("t", 80, 24))
    }

    #[test]
    fn cast_round_trips_through_write_and_read() {
        let (rec, score) = sample();
        let cast = write(&rec, &score, false).unwrap();
        // First line is a valid JSON header carrying our config.
        assert!(cast.lines().next().unwrap().contains("\"demostage\""));

        let tmp = std::env::temp_dir().join(format!("rec-{}.cast", std::process::id()));
        std::fs::write(&tmp, &cast).unwrap();
        let (back, back_score, _) = read(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();

        assert_eq!(back.events, rec.events);
        assert_eq!(back.captions, rec.captions);
        assert_eq!(back.cols, 80);
        assert_eq!(back_score.layout.panes.len(), 1);
    }

    #[test]
    fn write_read_preserves_a_trailing_hold_duration() {
        // A browser scene revealed after the last output: duration extends past the
        // last event, and that must survive the round-trip (else the scene is cut).
        let rec = Recording {
            cols: 80,
            rows: 24,
            title: "t".into(),
            events: vec![(0.1, "hi".into()), (2.0, "\r\n".into())],
            captions: vec![],
            focuses: vec![(3.0, "scene1".into())],
            duration: 11.0, // 8s hold after the last output at 2.0s
        };
        let score = default_score("t", 80, 24);
        let cast = write(&rec, &score, false).unwrap();
        let tmp = std::env::temp_dir().join(format!("hold-{}.cast", std::process::id()));
        std::fs::write(&tmp, &cast).unwrap();
        let (back, _, _) = read(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();
        assert!(
            (back.duration - 11.0).abs() < 1e-9,
            "trailing hold lost: {}",
            back.duration
        );
    }

    #[test]
    fn reads_a_raw_capture_as_a_recording() {
        let raw = r#"
[meta]
shell = "/bin/bash"
cols = 90
rows = 30

[[events]]
kind = "input"
t_ms = 0
bytes = "ls\r"

[[events]]
kind = "output"
t_ms = 250
data = "file.txt\n"
"#;
        let tmp = std::env::temp_dir().join(format!("cap-{}.raw.toml", std::process::id()));
        std::fs::write(&tmp, raw).unwrap();
        let (rec, score, faithful) = read(&tmp).unwrap();
        assert!(faithful, "a raw capture is faithful");
        std::fs::remove_file(&tmp).ok();

        // Only output events become the playback stream; input is ignored.
        assert_eq!(rec.events, vec![(0.25, "file.txt\n".to_string())]);
        assert_eq!(rec.cols, 90);
        assert_eq!(score.layout.width, 90 * CELL_W);
    }

    fn raw(events: Vec<RawEvent>) -> RawMacro {
        RawMacro {
            meta: crate::model::RawMeta {
                shell: "/bin/bash".into(),
                cols: 80,
                rows: 24,
                idle_timeout_ms: 0,
                resolution: None,
                fps: None,
                stage: None,
                mute_spans: Vec::new(),
            },
            events,
        }
    }

    #[test]
    fn from_raw_excises_muted_output() {
        // A `demo open` wizard span (200..900) is dropped from the playback stream,
        // but the reveal that opens at its end survives.
        let mut r = raw(vec![
            RawEvent::Output {
                t_ms: 100,
                data: "real".into(),
            },
            RawEvent::Output {
                t_ms: 300,
                data: "demo open — reveal a browser scene".into(),
            },
            RawEvent::Output {
                t_ms: 500,
                data: "Show as: replace".into(),
            },
            RawEvent::Reveal {
                t_ms: 900,
                panes: vec![RevealPane {
                    id: "browser".into(),
                    url: Some("https://example.com".into()),
                    theme: None,
                }],
                orientation: Orientation::Horizontal,
                hold_ms: None,
                scroll: false,
            },
        ]);
        r.meta.mute_spans = vec![(200, 900)];
        let (rec, layout, _) = from_raw(&r, "t");
        // Only the real output remains; the wizard chatter is gone.
        assert_eq!(rec.events.len(), 1);
        assert_eq!(rec.events[0].1, "real");
        // The browser scene was still built from the reveal.
        assert_eq!(layout.panes.len(), 2);
    }

    #[test]
    fn from_raw_caps_idle_gaps() {
        // A 5s dead-air gap between two chunks is trimmed to the 1.2s cap.
        let r = raw(vec![
            RawEvent::Output {
                t_ms: 0,
                data: "a".into(),
            },
            RawEvent::Output {
                t_ms: 5000,
                data: "b".into(),
            },
        ]);
        let (rec, _, _) = from_raw(&r, "t");
        assert_eq!(rec.events[0].0, 0.0);
        assert!((rec.events[1].0 - 1.2).abs() < 1e-9, "{}", rec.events[1].0);
    }

    #[test]
    fn from_raw_drops_the_trailing_demo_stop() {
        // The `demo stop` the user typed (and its echo/stopping message) is gone.
        let r = raw(vec![
            RawEvent::Output {
                t_ms: 100,
                data: "real output".into(),
            },
            RawEvent::Input {
                t_ms: 2000,
                bytes: "demo stop\r".into(),
            },
            RawEvent::Output {
                t_ms: 2010,
                data: "demo stop".into(),
            },
            RawEvent::Output {
                t_ms: 2100,
                data: "● stopping recording…".into(),
            },
        ]);
        let (rec, _, _) = from_raw(&r, "t");
        assert_eq!(rec.events.len(), 1);
        assert_eq!(rec.events[0].1, "real output");
    }

    #[test]
    fn demo_open_builds_a_browser_scene() {
        // A `demo open` reveal becomes a browser pane + a focus at its (trimmed) time.
        let r = raw(vec![
            RawEvent::Output {
                t_ms: 0,
                data: "building...".into(),
            },
            RawEvent::Reveal {
                t_ms: 500,
                panes: vec![RevealPane {
                    id: "browser".into(),
                    url: Some("https://example.com".into()),
                    theme: None,
                }],
                orientation: Orientation::Horizontal,
                hold_ms: None,
                scroll: false,
            },
        ]);
        let (rec, layout, _) = from_raw(&r, "t");
        // terminal + one browser scene pane.
        assert_eq!(layout.panes.len(), 2);
        let scene = &layout.panes[1];
        assert_eq!(scene.kind, PaneKind::Browser);
        assert_eq!(scene.url.as_deref(), Some("https://example.com"));
        // replace → full canvas (with padding).
        assert_eq!(scene.width, layout.width - 2 * MIN_PAD);
        // reveal focus targets the scene at t=0.5s.
        assert_eq!(rec.focuses.len(), 1);
        assert_eq!(rec.focuses[0].1, scene.id);
        assert!((rec.focuses[0].0 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn two_source_split_places_panes_and_windows_them() {
        // A horizontal terminal|browser split, then a "back to terminal" reveal.
        let r = raw(vec![
            RawEvent::Output {
                t_ms: 0,
                data: "a".into(),
            },
            RawEvent::Reveal {
                t_ms: 1000,
                panes: vec![
                    RevealPane::terminal(),
                    RevealPane {
                        id: "docs".into(),
                        url: Some("https://d".into()),
                        theme: None,
                    },
                ],
                orientation: Orientation::Horizontal,
                hold_ms: None,
                scroll: false,
            },
            RawEvent::Reveal {
                t_ms: 2000,
                panes: vec![RevealPane::terminal()],
                orientation: Orientation::Horizontal,
                hold_ms: None,
                scroll: false,
            },
        ]);
        let (_rec, layout, _) = from_raw(&r, "t");
        // Terminal background + exactly one browser overlay (the terminal reveal
        // adds none — it just closes the browser's window).
        assert_eq!(layout.panes.len(), 2);
        let docs = &layout.panes[1];
        // Horizontal split → right half of the canvas (with padding).
        let inner_w = layout.width - 2 * MIN_PAD;
        assert_eq!(docs.x, MIN_PAD + inner_w / 2);
        assert_eq!(docs.width, inner_w - inner_w / 2);
        assert_eq!(docs.height, layout.height - 2 * MIN_PAD);
        // Revealed at 1s, hidden again at the 2s "back to terminal" reveal.
        assert_eq!(docs.reveal_at, Some(1.0));
        assert_eq!(docs.hide_at, Some(2.0));
    }

    #[test]
    fn explicit_resolution_sizes_the_canvas_and_centers_the_terminal() {
        let mut r = raw(vec![RawEvent::Output {
            t_ms: 0,
            data: "a".into(),
        }]);
        r.meta.resolution = Some((1920, 1080));
        let (_rec, layout, _) = from_raw(&r, "t");
        assert_eq!((layout.width, layout.height), (1920, 1080));
        // The 80×24 grid renders at 800×480 px, centered on the canvas with padding.
        let term = &layout.panes[0];
        assert_eq!((term.width, term.height), (800, 480));
        let expected_x = MIN_PAD + (1920 - 800 - 2 * MIN_PAD) / 2;
        let expected_y = MIN_PAD + (1080 - 480 - 2 * MIN_PAD) / 2;
        assert_eq!((term.x, term.y), (expected_x, expected_y));
    }

    #[test]
    fn resolution_never_crops_the_terminal() {
        // A resolution smaller than the recorded grid is raised to fit it.
        let mut r = raw(vec![RawEvent::Output {
            t_ms: 0,
            data: "a".into(),
        }]);
        r.meta.resolution = Some((100, 100));
        let (_rec, layout, _) = from_raw(&r, "t");
        assert_eq!((layout.width, layout.height), (800, 480));
        // Terminal fills the canvas (resolution raised to fit), but with minimum padding.
        assert_eq!((layout.panes[0].x, layout.panes[0].y), (MIN_PAD, MIN_PAD));
    }

    #[test]
    fn capture_fps_threads_into_the_layout() {
        // A 24 fps choice at capture reaches the score's [layout] fps.
        let mut r = raw(vec![RawEvent::Output {
            t_ms: 0,
            data: "a".into(),
        }]);
        r.meta.fps = Some(24);
        let (_rec, layout, _) = from_raw(&r, "t");
        assert_eq!(layout.fps, 24);
    }

    #[test]
    fn layout_fps_defaults_to_15_when_unset() {
        let r = raw(vec![RawEvent::Output {
            t_ms: 0,
            data: "a".into(),
        }]);
        let (_rec, layout, _) = from_raw(&r, "t");
        assert_eq!(layout.fps, 15);
    }

    #[test]
    fn vertical_split_stacks_the_browser_pane() {
        let r = raw(vec![RawEvent::Reveal {
            t_ms: 0,
            panes: vec![
                RevealPane::terminal(),
                RevealPane {
                    id: "docs".into(),
                    url: Some("https://d".into()),
                    theme: None,
                },
            ],
            orientation: Orientation::Vertical,
            hold_ms: None,
            scroll: false,
        }]);
        let (_rec, layout, _) = from_raw(&r, "t");
        let docs = &layout.panes[1];
        // Vertical split → bottom half (with padding).
        let inner_h = layout.height - 2 * MIN_PAD;
        assert_eq!(docs.x, MIN_PAD);
        assert_eq!(docs.y, MIN_PAD + inner_h / 2);
        assert_eq!(docs.width, layout.width - 2 * MIN_PAD);
    }

    #[test]
    fn reveal_theme_threads_into_the_browser_pane() {
        let r = raw(vec![RawEvent::Reveal {
            t_ms: 100,
            panes: vec![RevealPane {
                id: "browser".into(),
                url: Some("https://github.com/x".into()),
                theme: Some("dark".into()),
            }],
            orientation: Orientation::Horizontal,
            hold_ms: None,
            scroll: false,
        }]);
        let (_rec, layout, _) = from_raw(&r, "t");
        let scene = &layout.panes[1];
        assert_eq!(scene.theme.as_deref(), Some("dark"));
    }

    #[test]
    fn scrolling_reveal_holds_and_emits_a_scroll_step() {
        // A scrolling reveal near the end keeps the scene on screen for its hold
        // window and adds a focus+scroll step so the stage captures keyframes.
        let r = raw(vec![
            RawEvent::Output {
                t_ms: 0,
                data: "done".into(),
            },
            RawEvent::Reveal {
                t_ms: 100,
                panes: vec![RevealPane {
                    id: "browser".into(),
                    url: Some("https://example.com".into()),
                    theme: None,
                }],
                orientation: Orientation::Horizontal,
                hold_ms: Some(5000),
                scroll: true,
            },
        ]);
        let (rec, _layout, timeline) = from_raw(&r, "t");
        // reveal at ~0.1s + 5s hold → duration is at least ~5.1s (vs ~1.9s tail).
        assert!(rec.duration >= 5.0, "duration={}", rec.duration);
        // one focus + one scroll step for the scene.
        assert!(matches!(timeline.first(), Some(Step::Focus { .. })));
        assert!(matches!(
            timeline.get(1),
            Some(Step::Scroll {
                direction: ScrollDirection::Down,
                duration_ms: 5000,
                ..
            })
        ));
    }

    #[test]
    fn reveal_hold_ms_explicit() {
        assert_eq!(reveal_hold_ms(Some(3000), false), 3000.0);
        assert_eq!(reveal_hold_ms(Some(0), false), 0.0);
    }

    #[test]
    fn reveal_hold_ms_scroll_default() {
        assert_eq!(reveal_hold_ms(None, true), DEFAULT_SCROLL_HOLD_MS);
    }

    #[test]
    fn reveal_hold_ms_no_scroll_default() {
        assert_eq!(reveal_hold_ms(None, false), DEFAULT_REVEAL_HOLD_MS);
    }

    #[test]
    fn pane_rects_zero_count() {
        let rects = pane_rects(1000, 500, 0, Orientation::Horizontal);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, MIN_PAD);
        assert_eq!(rects[0].1, MIN_PAD);
    }

    #[test]
    fn pane_rects_one_count() {
        let rects = pane_rects(1000, 500, 1, Orientation::Vertical);
        assert_eq!(rects.len(), 1);
    }

    #[test]
    fn pane_rects_two_horizontal() {
        let rects = pane_rects(1000, 500, 2, Orientation::Horizontal);
        assert_eq!(rects.len(), 2);
        // Left pane starts at MIN_PAD
        assert_eq!(rects[0].0, MIN_PAD);
        // Right pane starts after left
        assert!(rects[1].0 > rects[0].0);
        // Both have same height
        assert_eq!(rects[0].3, rects[1].3);
    }

    #[test]
    fn pane_rects_two_vertical() {
        let rects = pane_rects(1000, 500, 2, Orientation::Vertical);
        assert_eq!(rects.len(), 2);
        // Top pane
        assert_eq!(rects[0].1, MIN_PAD);
        // Bottom pane starts after top
        assert!(rects[1].1 > rects[0].1);
        // Both have same width
        assert_eq!(rects[0].2, rects[1].2);
    }

    #[test]
    fn stop_cutoff_ms_finds_demo_stop() {
        let r = raw(vec![
            RawEvent::Output {
                t_ms: 100,
                data: "hello".into(),
            },
            RawEvent::Input {
                t_ms: 2000,
                bytes: "demo stop\r".into(),
            },
            RawEvent::Output {
                t_ms: 2010,
                data: "stopping".into(),
            },
        ]);
        let cutoff = stop_cutoff_ms(&r);
        assert!(cutoff.is_some());
        assert_eq!(cutoff.unwrap(), 2000);
    }

    #[test]
    fn stop_cutoff_ms_no_stop() {
        let r = raw(vec![RawEvent::Output {
            t_ms: 100,
            data: "hello".into(),
        }]);
        assert!(stop_cutoff_ms(&r).is_none());
    }

    #[test]
    fn default_score_single_terminal() {
        let score = default_score("test", 80, 24);
        assert_eq!(score.demo.name, "test");
        assert_eq!(score.layout.panes.len(), 1);
        assert_eq!(score.layout.panes[0].kind, PaneKind::Terminal);
    }

    #[test]
    fn default_score_canvas_dimensions() {
        let score = default_score("test", 80, 24);
        assert_eq!(score.layout.width, 80 * CELL_W);
        assert_eq!(score.layout.height, 24 * CELL_H);
    }

    #[test]
    fn default_score_fps() {
        let score = default_score("test", 80, 24);
        assert_eq!(score.layout.fps, 15);
    }

    #[test]
    fn stop_cutoff_ms_stop_with_newline() {
        let r = raw(vec![RawEvent::Input {
            t_ms: 500,
            bytes: "demo stop\n".into(),
        }]);
        let cutoff = stop_cutoff_ms(&r);
        assert_eq!(cutoff, Some(500));
    }

    /// Control-only input must not start a line: a later `demo stop` is cut
    /// at its own timestamp, not the earlier control noise. Deleting the
    /// control-skip arm would keep the bytes, plant `line_start` early, and
    /// report the wrong cutoff.
    #[test]
    fn stop_cutoff_ms_ignores_leading_control_noise() {
        let r = raw(vec![
            RawEvent::Input {
                t_ms: 100,
                bytes: "\x1b\x00".into(),
            },
            RawEvent::Input {
                t_ms: 200,
                bytes: "demo stop\r".into(),
            },
        ]);
        assert_eq!(stop_cutoff_ms(&r), Some(200));
        // Control-only input alone creates no line and no cutoff.
        let r2 = raw(vec![RawEvent::Input {
            t_ms: 100,
            bytes: "\x1b\x00".into(),
        }]);
        assert_eq!(stop_cutoff_ms(&r2), None);
    }

    #[test]
    fn stop_cutoff_ms_stop_among_other_commands() {
        let r = raw(vec![
            RawEvent::Input {
                t_ms: 100,
                bytes: "ls\n".into(),
            },
            RawEvent::Input {
                t_ms: 500,
                bytes: "demo stop\r".into(),
            },
        ]);
        let cutoff = stop_cutoff_ms(&r);
        assert_eq!(cutoff, Some(500));
    }

    #[test]
    fn pane_rects_large_canvas() {
        let rects = pane_rects(1920, 1080, 2, Orientation::Horizontal);
        assert_eq!(rects.len(), 2);
        // Inner width = 1920 - 2*32 = 1856, half = 928
        let inner_w = 1920 - 2 * MIN_PAD;
        assert_eq!(rects[0].2, inner_w / 2);
        assert_eq!(rects[1].2, inner_w - inner_w / 2);
    }

    #[test]
    fn reveal_hold_ms_large_explicit() {
        assert_eq!(reveal_hold_ms(Some(60000), false), 60000.0);
    }

    #[test]
    fn default_score_different_sizes() {
        let score = default_score("small", 40, 12);
        assert_eq!(score.layout.width, 40 * CELL_W);
        assert_eq!(score.layout.height, 12 * CELL_H);
    }
}
