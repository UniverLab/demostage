//! Multi-scene stage (SPEC §4): run the terminal pane in a PTY, capture each
//! browser pane via headless Chromium, and composite all panes onto the canvas
//! frame by frame. Terminal capture + compositing are verified; the browser
//! capture needs Chromium and is verified outside the sandbox.

use super::run::Recording;
use super::{browser, composite, raster};
use crate::error::{Error, Result};
use crate::model::{Pane, PaneKind, Score, ScrollDirection, Step, Velocity};

/// Scroll parameters extracted from the first scroll step aimed at a pane.
/// The first scroll step wins when several conflict; the rest are ignored
/// and a diagnostic line is printed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollParams {
    pub direction: ScrollDirection,
    pub velocity: Velocity,
    pub ignored_count: usize,
    /// How long the pan should last, from the winning `scroll` step's
    /// `duration_ms`. Clamped to the pane's on-screen window by the caller: a
    /// pane cannot pan for longer than it is visible.
    pub seconds: f64,
}

/// Pure easing function: maps a normalized position in `[0, 1]` to a
/// normalized offset in `[0, 1]`. Shared by both scroll paths (PDF and browser)
/// so they cannot drift apart.
///
/// - `Constant`: identity (linear).
/// - `EaseInOut`: smoothstep — accelerates from rest, decelerates to rest,
///   passes through 0.5 at the midpoint, with `f(0) == 0` and `f(1) == 1`.
pub fn ease(position: f64, velocity: Velocity) -> f64 {
    let t = position.clamp(0.0, 1.0);
    match velocity {
        Velocity::Constant => t,
        Velocity::EaseInOut => t * t * (3.0 - 2.0 * t),
    }
}

/// Compute the absolute scroll offsets for `frames` output frames, given a
/// maximum offset (`max_offset`), a direction, and a velocity curve.
/// Returns a single-element vector `[0]` when the page is not scrollable or
/// only one frame is requested.
pub fn scroll_offsets_with_params(
    max_offset: usize,
    frames: usize,
    direction: ScrollDirection,
    velocity: Velocity,
) -> Vec<usize> {
    if max_offset == 0 || frames <= 1 {
        return vec![0];
    }
    (0..frames)
        .map(|i| {
            let t = i as f64 / (frames - 1) as f64;
            let eased = ease(t, velocity);
            let offset = (max_offset as f64 * eased).round() as usize;
            match direction {
                ScrollDirection::Down => offset,
                ScrollDirection::Up => max_offset.saturating_sub(offset),
            }
        })
        .collect()
}

/// True when a score has more than one pane, any browser pane, or a terminal
/// pane that doesn't fill the canvas (e.g. a capture with an explicit export
/// resolution, where the terminal sits centered on a larger canvas) — all cases
/// that need the compositing stage rather than the single-terminal fast path.
pub fn needs_stage(score: &Score) -> bool {
    let has_browser = score
        .layout
        .panes
        .iter()
        .any(|p| p.kind == PaneKind::Browser);
    let terminals = score
        .layout
        .panes
        .iter()
        .filter(|p| p.kind == PaneKind::Terminal)
        .count();
    let offset_terminal = score.layout.panes.iter().any(|p| {
        p.kind == PaneKind::Terminal
            && (p.x != 0
                || p.y != 0
                || p.width != score.layout.width
                || p.height != score.layout.height)
    });
    has_browser || terminals > 1 || offset_terminal
}

/// The browser panes of a stage once captured: their scenes (each with the
/// pane's on-screen window), the temp-dir guards keeping their frame
/// directories alive, and the cost reports to print.
struct BrowserScenes<'a> {
    scenes: Vec<(&'a Pane, browser::AnyScene, f64, Option<f64>)>,
    _guards: Vec<browser::TempDirGuard>,
    reports: Vec<browser::BrowserCaptureReport>,
}

/// Capture every browser pane up front (Chromium). Each scene reveals at the
/// moment it is first focused (recorded during the terminal run) — so it
/// "opens" exactly when the demo focuses it, e.g. once a server is up or a PDF
/// has compiled.
fn capture_browser_scenes<'a>(
    score: &'a Score,
    rec: &Recording,
    total: f64,
    fps: f64,
    speed: f64,
) -> Result<BrowserScenes<'a>> {
    let mut captured = BrowserScenes {
        scenes: Vec::new(),
        _guards: Vec::new(),
        reports: Vec::new(),
    };
    // Browser panes captured up front (Chromium). Each reveals at the moment it
    // is first focused (recorded during the terminal run) — so it "opens" exactly
    // when the demo focuses it, e.g. once a server is up or a PDF has compiled.
    for pane in score
        .layout
        .panes
        .iter()
        .filter(|p| p.kind == PaneKind::Browser)
    {
        let scrolls = scroll_keyframes_for(score, &pane.id);
        let (reveal_at, hide_at) = pane_window(pane, &rec.focuses);
        let window_end = hide_at.unwrap_or(total);
        let window_dur = (window_end - reveal_at).max(0.0);
        let output_frames = (window_dur * fps).round() as usize;
        // One value decides everything about the pan: whether it happens at all,
        // which way, with what curve, and for how long. Keeping "does it scroll"
        // apart from "how does it scroll" is what let the Chrome path scroll a
        // pane nobody asked to scroll.
        let pan = scroll_params_for(score, &pane.id).map(|mut p| {
            // A pane cannot pan for longer than it is on screen.
            if p.seconds <= 0.0 || p.seconds > window_dur {
                p.seconds = window_dur;
            }
            p
        });
        let effective_speed = if pane.ignore_speed { 1.0 } else { speed };
        let result = browser::capture(
            pane,
            scrolls,
            output_frames.max(1),
            fps,
            pan,
            effective_speed,
        )?;
        if let Some(report) = result.report {
            captured.reports.push(report);
        }
        captured._guards.push(result._guard);
        captured
            .scenes
            .push((pane, result.scene, reveal_at, hide_at));
    }
    Ok(captured)
}

/// A PDF pane exists to show its document, so it may need more time on screen
/// than the recording gives it. When such a pane runs to the end of the demo,
/// the demo waits for it: the terminal underneath holds its last frame while
/// the pan finishes. A pane that hides mid-demo cannot be extended without
/// shifting everything after it, so that one is reported instead of silently
/// truncating the document. Returns the grown `(frame_count, total_secs)`.
fn extend_for_pdf_panes(
    scenes: &[(&Pane, browser::AnyScene, f64, Option<f64>)],
    mut n: usize,
    mut total: f64,
    fps: f64,
) -> (usize, f64) {
    for (pane, scene, reveal_at, hide_at) in scenes {
        let needed = scene.needed_seconds();
        if needed <= 0.0 {
            continue;
        }
        let have = hide_at.unwrap_or(total) - reveal_at;
        if needed <= have + 1e-6 {
            continue;
        }
        match hide_at {
            None => {
                let end = reveal_at + needed;
                n = (end * fps).ceil() as usize + 1;
                total = n as f64 / fps;
                eprintln!(
                    "demo: pane '{}' — held {:.1}s longer so the whole document is shown",
                    pane.id,
                    needed - have
                );
            }
            Some(_) => eprintln!(
                "demo: pane '{}' — the document needs {:.1}s and the pane is on screen {:.1}s; \
                 the rest of it is not shown. Give the pane more time before it switches away.",
                pane.id, needed, have
            ),
        }
    }
    (n, total)
}

/// Owned terminal input for the stage: a captions-free copy of the recording
/// plus the canvas-level caption overlay drawn over the composite.
struct TerminalSource {
    recording: Recording,
    caption: Option<raster::CaptionOverlay>,
}

/// Build the captions-free terminal recording and its canvas caption overlay.
/// Captions are drawn on the composited canvas, so the terminal sub-frames
/// render from a captions-free copy.
fn prepare_terminal_source(rec: &Recording, score: &Score) -> Result<TerminalSource> {
    let font_name = score
        .layout
        .font_family
        .as_deref()
        .unwrap_or(crate::fonts::DEFAULT_FONT);
    let caption = if rec.captions.is_empty() {
        None
    } else {
        Some(raster::CaptionOverlay::new(
            rec.captions.clone(),
            20.0,
            font_name,
            crate::fonts::load_emoji(),
            crate::fonts::load_last_resort(),
        )?)
    };
    let mut recording = rec.clone();
    recording.captions.clear();
    Ok(TerminalSource { recording, caption })
}

/// Geometry and pacing a composite run renders at.
struct CompositeParams {
    canvas_w: usize,
    canvas_h: usize,
    background: [u8; 3],
    fps: f64,
    total: f64,
    frames: usize,
}

/// Emit every composited canvas frame: the terminal holds its last frame past
/// the end of the recording so a PDF pane can finish panning, browser scenes
/// play over their on-screen window, and captions draw on the canvas.
fn composite_frames(
    term_src: &mut raster::FrameSource<'_>,
    captured: &mut BrowserScenes<'_>,
    term_pane: &Pane,
    params: &CompositeParams,
    caption: &mut Option<raster::CaptionOverlay>,
    fallback_report: &mut raster::FallbackReport,
    on_frame: &mut impl FnMut(&[u8]),
) {
    let (tw, th) = term_src.dims();
    let mut held_term_frame: Vec<u8> = Vec::new();
    for i in 0..params.frames {
        let t = i as f64 / params.fps;
        let term_frame = match term_src.next_frame() {
            Some(f) => {
                held_term_frame = f;
                &held_term_frame
            }
            None => &held_term_frame,
        }
        .clone();

        let mut layers = vec![composite::Layer {
            x: term_pane.x as usize,
            y: term_pane.y as usize,
            w: tw,
            h: th,
            rgba: &term_frame,
        }];
        for (pane, scene, reveal_at, hide_at) in &mut captured.scenes {
            if t < *reveal_at {
                continue;
            }
            if (*hide_at).is_some_and(|h| t >= h) {
                continue;
            }
            let window_end = (*hide_at).unwrap_or(params.total);
            let span = (window_end - *reveal_at).max(1e-6);
            let progress = ((t - *reveal_at) / span).clamp(0.0, 1.0);
            layers.push(composite::Layer {
                x: pane.x as usize,
                y: pane.y as usize,
                w: scene.width(),
                h: scene.height(),
                rgba: scene.frame_at(progress),
            });
        }
        let mut canvas =
            composite::composite(params.canvas_w, params.canvas_h, params.background, &layers);
        if let Some(caption) = caption {
            caption.draw(
                &mut canvas,
                params.canvas_w,
                params.canvas_h,
                i as f64 / params.fps,
                fallback_report,
            );
        }
        on_frame(&canvas);
    }
}
/// Composite a multi-pane score from an already-captured terminal `rec`,
/// emitting each composited canvas frame. Pure playback — the terminal pane comes
/// from `rec` (never re-run); browser panes are captured here via Chromium.
/// Returns the fallback report and any browser capture reports for cost printing.
/// `speed` is the resolved export speed multiplier, threaded through to the PDF
/// pan path.
pub fn render_stage(
    rec: &Recording,
    score: &Score,
    speed: f64,
    mut on_frame: impl FnMut(&[u8]),
) -> Result<(raster::FallbackReport, Vec<browser::BrowserCaptureReport>)> {
    let canvas_w = score.layout.width as usize;
    let canvas_h = score.layout.height as usize;
    let bg = score
        .layout
        .background
        .as_deref()
        .and_then(raster::parse_hex)
        .unwrap_or([11, 15, 20]);

    let term_pane = score
        .layout
        .panes
        .iter()
        .find(|p| p.kind == PaneKind::Terminal)
        .ok_or_else(|| Error::Export("a multi-scene stage needs a terminal pane".to_string()))?;

    let TerminalSource {
        recording: term_rec,
        mut caption,
    } = prepare_terminal_source(rec, score)?;
    let mut term_src = raster::FrameSource::new(&term_rec, score)?;
    let n = term_src.n_frames();
    let fps = score.layout.fps.max(1) as f64;
    let total = n as f64 / fps;
    let mut fallback_report = term_src.take_fallback_report();

    let mut captured = capture_browser_scenes(score, rec, total, fps, speed)?;
    let (n, total) = extend_for_pdf_panes(&captured.scenes, n, total, fps);

    // The window may have grown above; every scene maps progress over the window
    // it is actually given, so tell them the final one.
    for (_, scene, reveal_at, hide_at) in captured.scenes.iter_mut() {
        let window = (hide_at.unwrap_or(total) - *reveal_at).max(0.0);
        scene.set_window_frames((window * fps).round() as usize);
    }

    let params = CompositeParams {
        canvas_w,
        canvas_h,
        background: bg,
        fps,
        total,
        frames: n,
    };
    composite_frames(
        &mut term_src,
        &mut captured,
        term_pane,
        &params,
        &mut caption,
        &mut fallback_report,
        &mut on_frame,
    );
    Ok((fallback_report, captured.reports))
}

/// A browser pane's on-screen window `[reveal, hide)`. The recording's focus
/// events are the source of truth when they mention the pane: they sit on the
/// playback clock (and are speed-scaled with the rest of the recording), while
/// the score's `reveal_at`/`hide_at` may still carry times from an earlier
/// capture. The pane reveals at its first focus and hides when focus moves to
/// another pane; the score's window is the fallback for what the events don't
/// say (e.g. a hide recorded as a back-to-terminal reveal with no focus event,
/// or a recording that carries no focus for the pane at all).
fn pane_window(pane: &Pane, focuses: &[(f64, String)]) -> (f64, Option<f64>) {
    match focuses.iter().position(|(_, id)| *id == pane.id) {
        Some(i) => {
            let reveal = focuses[i].0;
            let hide = focuses[i + 1..]
                .iter()
                .find(|(_, id)| *id != pane.id)
                .map(|(t, _)| *t)
                .or(pane.hide_at.filter(|h| *h > reveal));
            (reveal, hide)
        }
        None => (pane.reveal_at.unwrap_or(0.0), pane.hide_at),
    }
}

/// How many scroll keyframes to capture for a browser pane — roughly one per
/// 700 ms of scrolling directed at it (explicitly or via focus), capped.
fn scroll_keyframes_for(score: &Score, pane_id: &str) -> usize {
    let mut focused: Option<&str> = None;
    let mut ms = 0u64;
    for step in &score.timeline {
        match step {
            Step::Focus { pane } => {
                focused = pane.as_deref();
            }
            Step::Scroll {
                duration_ms, pane, ..
            } if pane.as_deref().or(focused) == Some(pane_id) => {
                ms += duration_ms;
            }
            _ => {}
        }
    }
    ((ms / 700) as usize).clamp(0, 16)
}

/// Extract the scroll parameters (direction, velocity) from the first scroll
/// step aimed at a pane. When several scroll steps target the same pane with
/// conflicting directions, the first wins and the rest are ignored (a
/// diagnostic line is printed).
pub fn scroll_params_for(score: &Score, pane_id: &str) -> Option<ScrollParams> {
    let mut focused: Option<&str> = None;
    let mut first: Option<ScrollParams> = None;
    let mut ignored_count = 0usize;
    for step in &score.timeline {
        match step {
            Step::Focus { pane } => {
                focused = pane.as_deref();
            }
            Step::Scroll {
                direction,
                velocity,
                duration_ms,
                pane,
            } if pane.as_deref().or(focused) == Some(pane_id) => {
                if first.is_none() {
                    first = Some(ScrollParams {
                        direction: *direction,
                        velocity: *velocity,
                        ignored_count: 0,
                        seconds: *duration_ms as f64 / 1000.0,
                    });
                } else {
                    ignored_count += 1;
                }
            }
            _ => {}
        }
    }
    if ignored_count > 0 {
        eprintln!(
            "demo: pane '{}': {} scroll step(s) ignored (first scroll step wins)",
            pane_id, ignored_count
        );
    }
    first.map(|mut p| {
        p.ignored_count = ignored_count;
        p
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(toml_str: &str) -> Score {
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn detects_when_a_stage_is_needed() {
        let single = score(
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
        );
        assert!(!needs_stage(&single));

        let multi = score(
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
"#,
        );
        assert!(needs_stage(&multi));

        // A terminal centered on a larger canvas (explicit export resolution)
        // needs compositing even without browser panes.
        let centered = score(
            r#"
[demo]
name = "t"
[layout]
width = 1920
height = 1080
  [[layout.panes]]
  id = "main"
  type = "terminal"
  x = 560
  y = 300
  width = 800
  height = 480
"#,
        );
        assert!(needs_stage(&centered));
    }

    #[test]
    fn scroll_params_found_for_the_focused_browser() {
        let s = score(
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
[[timeline]]
action = "focus"
pane = "p"
[[timeline]]
action = "scroll"
direction = "down"
duration_ms = 2100
"#,
        );
        assert!(scroll_params_for(&s, "p").is_some());
        assert!(scroll_params_for(&s, "c").is_none());
    }

    #[test]
    fn counts_scroll_keyframes_for_the_focused_browser() {
        let s = score(
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
[[timeline]]
action = "focus"
pane = "p"
[[timeline]]
action = "scroll"
direction = "down"
duration_ms = 2100
"#,
        );
        // 2100ms / 700 = 3 keyframes for "p", none for "c".
        assert_eq!(scroll_keyframes_for(&s, "p"), 3);
        assert_eq!(scroll_keyframes_for(&s, "c"), 0);
    }

    fn browser_pane(reveal_at: Option<f64>, hide_at: Option<f64>) -> Pane {
        Pane {
            id: "b".into(),
            kind: PaneKind::Browser,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            font_family: None,
            font_size: None,
            url: Some("https://x".into()),
            theme: None,
            reveal_at,
            hide_at,
            ignore_speed: false,
        }
    }

    #[test]
    fn focus_events_override_a_stale_pane_window() {
        // The score says 82s (capture clock) but this recording focused the
        // pane at 21.4s, then went back to the terminal at 28s.
        let pane = browser_pane(Some(82.1), None);
        let focuses = vec![(21.4, "b".to_string()), (28.0, "main".to_string())];
        assert_eq!(pane_window(&pane, &focuses), (21.4, Some(28.0)));
    }

    #[test]
    fn score_hide_backs_up_missing_focus_events() {
        // Faithful capture: the hide was recorded as a back-to-terminal reveal
        // (no focus event), so the pane's own hide_at closes the window…
        let pane = browser_pane(Some(1.0), Some(2.0));
        assert_eq!(
            pane_window(&pane, &[(1.0, "b".to_string())]),
            (1.0, Some(2.0))
        );
        // …but a hide that predates the observed reveal is stale — ignored.
        let stale = browser_pane(Some(82.0), Some(15.0));
        assert_eq!(
            pane_window(&stale, &[(21.4, "b".to_string())]),
            (21.4, None)
        );
    }

    #[test]
    fn without_focus_events_the_scores_window_stands() {
        let pane = browser_pane(Some(3.0), Some(9.0));
        assert_eq!(pane_window(&pane, &[]), (3.0, Some(9.0)));
        let bare = browser_pane(None, None);
        assert_eq!(pane_window(&bare, &[]), (0.0, None));
    }

    #[test]
    fn needs_stage_false_for_single_fullscreen_terminal() {
        let s = score(
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
        );
        assert!(!needs_stage(&s));
    }

    #[test]
    fn needs_stage_true_for_browser_pane() {
        let s = score(
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
  id = "b"
  type = "browser"
  x = 0
  y = 0
  width = 200
  height = 100
  url = "https://x.com"
"#,
        );
        assert!(needs_stage(&s));
    }

    #[test]
    fn needs_stage_true_for_offset_terminal() {
        let s = score(
            r#"
[demo]
name = "t"
[layout]
width = 200
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 50
  y = 0
  width = 100
  height = 100
"#,
        );
        assert!(needs_stage(&s));
    }

    #[test]
    fn needs_stage_true_for_multiple_terminals() {
        let s = score(
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
        );
        assert!(needs_stage(&s));
    }

    #[test]
    fn scroll_params_none_when_no_scroll() {
        let s = score(
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
        );
        assert!(scroll_params_for(&s, "c").is_none());
    }

    #[test]
    fn scroll_keyframes_for_zero_when_no_scroll() {
        let s = score(
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
        );
        assert_eq!(scroll_keyframes_for(&s, "c"), 0);
    }

    #[test]
    fn scroll_params_some_when_scroll_steps_present() {
        let toml_str = r#"
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
"#;
        let mut s: Score = toml::from_str(toml_str).unwrap();
        s.timeline.push(crate::model::Step::Scroll {
            direction: crate::model::ScrollDirection::Down,
            velocity: crate::model::Velocity::Constant,
            duration_ms: 7000,
            pane: Some("c".into()),
        });
        assert!(scroll_params_for(&s, "c").is_some());
    }

    #[test]
    fn scroll_keyframes_for_capped_at_16() {
        let toml_str = r#"
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
"#;
        let mut s: Score = toml::from_str(toml_str).unwrap();
        for _ in 0..20 {
            s.timeline.push(crate::model::Step::Scroll {
                direction: crate::model::ScrollDirection::Down,
                velocity: crate::model::Velocity::Constant,
                duration_ms: 7000,
                pane: Some("c".into()),
            });
        }
        assert_eq!(scroll_keyframes_for(&s, "c"), 16);
    }

    #[test]
    fn pane_window_no_focuses_uses_score_values() {
        let pane = browser_pane(Some(5.0), Some(10.0));
        assert_eq!(pane_window(&pane, &[]), (5.0, Some(10.0)));
    }

    #[test]
    fn pane_window_focus_hides_at_next_different_focus() {
        let pane = browser_pane(None, None);
        let focuses = vec![(1.0, "b".to_string()), (3.0, "main".to_string())];
        assert_eq!(pane_window(&pane, &focuses), (1.0, Some(3.0)));
    }

    #[test]
    fn ease_constant_is_identity() {
        for i in 0..=100 {
            let t = i as f64 / 100.0;
            let result = ease(t, Velocity::Constant);
            assert!(
                (result - t).abs() < 1e-10,
                "constant ease should be identity at {t}"
            );
        }
    }

    #[test]
    fn ease_in_out_endpoints_exact() {
        assert_eq!(ease(0.0, Velocity::EaseInOut), 0.0);
        assert_eq!(ease(1.0, Velocity::EaseInOut), 1.0);
    }

    #[test]
    fn ease_in_out_midpoint_at_half() {
        let mid = ease(0.5, Velocity::EaseInOut);
        assert!(
            (mid - 0.5).abs() < 1e-10,
            "ease_in_out should pass through 0.5 at midpoint"
        );
    }

    #[test]
    fn ease_in_out_monotonically_increasing() {
        let mut prev = 0.0;
        for i in 1..=100 {
            let t = i as f64 / 100.0;
            let result = ease(t, Velocity::EaseInOut);
            assert!(
                result >= prev,
                "ease_in_out should be monotonically increasing at {t}"
            );
            prev = result;
        }
    }

    #[test]
    fn scroll_offsets_down_constant() {
        let offsets =
            scroll_offsets_with_params(1000, 11, ScrollDirection::Down, Velocity::Constant);
        assert_eq!(offsets[0], 0);
        assert_eq!(offsets[10], 1000);
        for i in 1..11 {
            assert!(
                offsets[i] > offsets[i - 1],
                "down+constant should be strictly increasing"
            );
        }
    }

    #[test]
    fn scroll_offsets_down_ease_in_out() {
        let offsets =
            scroll_offsets_with_params(1000, 11, ScrollDirection::Down, Velocity::EaseInOut);
        assert_eq!(offsets[0], 0);
        assert_eq!(offsets[10], 1000);
        for i in 1..11 {
            assert!(
                offsets[i] >= offsets[i - 1],
                "down+ease_in_out should be non-decreasing"
            );
        }
    }

    #[test]
    fn scroll_offsets_up_constant() {
        let offsets = scroll_offsets_with_params(1000, 11, ScrollDirection::Up, Velocity::Constant);
        assert_eq!(offsets[0], 1000);
        assert_eq!(offsets[10], 0);
        for i in 1..11 {
            assert!(
                offsets[i] < offsets[i - 1],
                "up+constant should be strictly decreasing"
            );
        }
    }

    #[test]
    fn scroll_offsets_up_ease_in_out() {
        let offsets =
            scroll_offsets_with_params(1000, 11, ScrollDirection::Up, Velocity::EaseInOut);
        assert_eq!(offsets[0], 1000);
        assert_eq!(offsets[10], 0);
        for i in 1..11 {
            assert!(
                offsets[i] <= offsets[i - 1],
                "up+ease_in_out should be non-increasing"
            );
        }
    }

    #[test]
    fn scroll_params_for_picks_first_scroll_step() {
        let toml_str = r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "b"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "file:///x.pdf"
[[timeline]]
action = "scroll"
direction = "up"
velocity = "ease_in_out"
duration_ms = 1000
pane = "b"
[[timeline]]
action = "scroll"
direction = "down"
velocity = "constant"
duration_ms = 1000
pane = "b"
"#;
        let s: Score = toml::from_str(toml_str).unwrap();
        let params = scroll_params_for(&s, "b").expect("should have scroll params");
        assert_eq!(params.direction, ScrollDirection::Up);
        assert_eq!(params.velocity, Velocity::EaseInOut);
        assert_eq!(params.ignored_count, 1);
    }
}
