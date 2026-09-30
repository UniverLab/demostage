//! `demo export` — render a recording to one or more formats. Pure playback:
//! it replays a recording (a `.cast` from `demo record`, or a raw `capture`)
//! and never executes the demo.

use crate::cli::{all_targets, parse_speed, ExportArgs, Target};
use crate::error::{Error, Result};
use crate::export::{
    ensure_local_server, recording, render, rewrite_local_urls, scale_pane_windows, scale_recording,
};
use crate::model::Score;

pub fn run(args: ExportArgs) -> Result<()> {
    let (mut rec, mut score, faithful) = recording::read(&args.input)?;

    // A faithful capture renders the real session as-is — typing and spacing are
    // exactly as recorded, NOT humanized. Require `--force` to render one, so the
    // clean path (`demo record`) is the default; but keep it possible, since
    // interactive / side-effecting demos (ghScaff, secrets) can't be re-executed.
    if faithful && !args.force {
        return Err(Error::Export(format!(
            "{} is a faithful capture (typing/idle as recorded, not re-humanized).\n  \
             • To render it as-is, add `--force`. This is the right path for \
             interactive or side-effecting demos — a wizard, anything that needs \
             secrets or creates real resources (e.g. ghScaff) — which `demo record` \
             would RE-RUN and break.\n  \
             • Only for a deterministic demo with no side effects: `demo record` \
             re-executes `demo.toml` for a humanized take, then export its `.rec`.",
            args.input.display()
        )));
    }

    // The command line wins; the score is what the demo says about itself; 1x is
    // the fallback. Without the middle term the multiplier a demo is published at
    // survives only in whoever typed the command.
    let speed = resolve_speed(args.speed, score.demo.speed.as_deref())?;
    scale_recording(&mut rec, speed);
    scale_pane_windows(&mut score, speed);

    // Apply resolution override if specified
    if let Some((new_w, new_h)) =
        resolve_export_resolution(&args, score.layout.width, score.layout.height)?
    {
        let (old_w, old_h) = (score.layout.width, score.layout.height);
        rescale_layout(&mut score, new_w, new_h);
        eprintln!("{}", resolution_override_note(new_w, new_h, old_w, old_h));
    }

    if faithful {
        eprintln!(
            "note: rendering a faithful capture as-is (--force) — typing/idle are \
             as recorded; `demo record` would re-humanize them."
        );
    }

    // Same precedence as the speed: the command line, then the score, then all.
    let targets = match args.targets.map(|t| t.0) {
        Some(t) => t,
        None => resolve_targets(score.demo.targets.as_deref())?,
    };

    // The animated SVG has no cell grid to walk on a staged score: refuse
    // before starting the local server or any render work. (Mixed targets
    // still start the server — gif/mp4 need it — and refuse at the svg step.)
    if args.at.is_none() && targets == [Target::Svg] && crate::export::stage::needs_stage(&score) {
        return Err(Error::Export(
            crate::export::svg::animated_refusal(&score)
                .expect("staged score without --at must refuse"),
        ));
    }

    // Start the local file server once (if needed) and keep it alive for all targets.
    let _server = ensure_local_server(&score)?;
    let score = if let Some(server) = _server.as_ref() {
        rewrite_local_urls(&score, server.port())
    } else {
        score
    };

    for target in targets {
        let path = render(&rec, &score, target, speed, args.at)?;
        println!("exported {} → {}", args.input.display(), path.display());
    }
    Ok(())
}

/// Resolve the export speed: `--speed`, else the score's `[demo] speed`, else 1x.
fn resolve_speed(flag: Option<f64>, from_score: Option<&str>) -> Result<f64> {
    if let Some(v) = flag {
        return Ok(v);
    }
    match from_score {
        Some(raw) => {
            parse_speed(raw).map_err(|e| Error::Export(format!("[demo] speed in the score: {e}")))
        }
        None => Ok(1.0),
    }
}

/// Resolve the export targets: the score's `[demo] targets`, else the defaults
/// (gif + mp4; `svg` is opt-in).
fn resolve_targets(from_score: Option<&[String]>) -> Result<Vec<Target>> {
    let Some(names) = from_score else {
        return Ok(all_targets());
    };
    if names.is_empty() {
        return Ok(all_targets());
    }
    names
        .iter()
        .map(|n| match n.trim().to_ascii_lowercase().as_str() {
            "gif" => Ok(Target::Gif),
            "mp4" => Ok(Target::Mp4),
            "svg" => Ok(Target::Svg),
            other => Err(Error::Export(format!(
                "[demo] targets in the score: unknown format '{other}' (expected gif, mp4 or svg)"
            ))),
        })
        .collect()
}

/// Compute the export resolution from flags, or `None` to keep the capture-time resolution.
fn resolve_export_resolution(
    args: &ExportArgs,
    _default_w: u32,
    _default_h: u32,
) -> Result<Option<(u32, u32)>> {
    if let Some(r) = &args.resolution {
        return parse_resolution_override(r).map(Some);
    }
    if let Some(a) = &args.aspect {
        let q = args.quality.as_deref().unwrap_or("fullhd");
        return canvas_from_aspect_quality(a, q).map(Some);
    }
    if let Some(q) = &args.quality {
        return canvas_from_aspect_quality("16:9", q).map(Some);
    }
    Ok(None)
}

/// Parse a `--resolution` value for export: `WxH` or a preset name.
fn parse_resolution_override(s: &str) -> Result<(u32, u32)> {
    let v = s.trim().to_ascii_lowercase();
    // Try preset names
    const PRESETS: [(&str, u32, u32); 5] = [
        ("landscape", 1920, 1080),
        ("portrait", 1080, 1920),
        ("square", 1080, 1080),
        ("standard", 1280, 720),
        ("fullhd", 1920, 1080),
    ];
    if let Some(&(_, w, h)) = PRESETS.iter().find(|(name, ..)| *name == v) {
        return Ok((w, h));
    }
    // Try WxH
    if let Some((w, h)) = v.split_once(['x', '×']) {
        if let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
            if w > 0 && h > 0 {
                return Ok((w, h));
            }
        }
    }
    Err(Error::Export(format!(
        "invalid resolution '{s}' — try a WxH pair (e.g. 1920x1080) or a preset (landscape, portrait, square, standard, fullhd)"
    )))
}

/// Aspect ratios for export.
const ASPECTS: [(&str, u32, u32); 4] = [
    ("16:9", 16, 9),
    ("9:16", 9, 16),
    ("4:3", 4, 3),
    ("1:1", 1, 1),
];

/// Quality tiers — the short side of the canvas, in pixels.
const QUALITIES: [(&str, u32); 2] = [("fullhd", 1080), ("hd", 720)];

/// Compute the canvas `(width, height)` for an aspect ratio + quality.
fn canvas_from_aspect_quality(aspect: &str, quality: &str) -> Result<(u32, u32)> {
    let av = aspect.trim().to_ascii_lowercase();
    let &(_, a, b) = ASPECTS
        .iter()
        .find(|(name, ..)| *name == av)
        .ok_or_else(|| {
            Error::Export(format!(
                "invalid aspect '{aspect}' — try 16:9, 9:16, 4:3, or 1:1"
            ))
        })?;
    let qv = quality.trim().to_ascii_lowercase();
    let base = QUALITIES
        .iter()
        .find(|(name, _)| *name == qv)
        .map(|(_, b)| *b)
        .ok_or_else(|| Error::Export(format!("invalid quality '{quality}' — try fullhd or hd")))?;
    let short = a.min(b);
    Ok((a * base / short, b * base / short))
}

/// Rescale the layout and all panes to a new canvas size.
fn rescale_layout(score: &mut Score, new_w: u32, new_h: u32) {
    let old_w = score.layout.width;
    let old_h = score.layout.height;
    if old_w == new_w && old_h == new_h {
        return;
    }
    let scale_x = new_w as f64 / old_w as f64;
    let scale_y = new_h as f64 / old_h as f64;
    let font_scale = scale_x.min(scale_y);

    score.layout.width = new_w;
    score.layout.height = new_h;

    for pane in &mut score.layout.panes {
        pane.x = (pane.x as f64 * scale_x).round() as u32;
        pane.y = (pane.y as f64 * scale_y).round() as u32;
        pane.width = (pane.width as f64 * scale_x).round() as u32;
        pane.height = (pane.height as f64 * scale_y).round() as u32;
        if pane.kind == crate::model::PaneKind::Terminal {
            if let Some(ref mut fs) = pane.font_size {
                *fs = ((*fs as f64 * font_scale).round() as u32).max(1);
            }
        }
    }
}

fn resolution_override_note(new_w: u32, new_h: u32, old_w: u32, old_h: u32) -> String {
    format!(
        "note: overriding resolution to {}x{} (capture was {}x{})",
        new_w, new_h, old_w, old_h
    )
}

#[cfg(test)]
mod tests {
    /// The multiplier a demo is published at used to survive only in whoever ran
    /// the command. Recovering it from the published assets on 2026-08-27 (the
    /// five UniverLab demos: four at 2x, canopy at 3x) is what motivated this.
    #[test]
    fn the_score_supplies_the_speed_when_the_flag_does_not() {
        assert_eq!(resolve_speed(None, Some("2x")).unwrap(), 2.0);
        assert_eq!(resolve_speed(None, Some("3x")).unwrap(), 3.0);
        assert_eq!(resolve_speed(None, None).unwrap(), 1.0);
    }

    #[test]
    fn the_flag_beats_the_score() {
        assert_eq!(resolve_speed(Some(1.0), Some("3x")).unwrap(), 1.0);
    }

    #[test]
    fn a_bad_speed_in_the_score_is_an_error_naming_the_score() {
        let err = resolve_speed(None, Some("fast")).unwrap_err().to_string();
        assert!(err.contains("[demo] speed"), "unhelpful message: {err}");
    }

    #[test]
    fn the_score_supplies_the_targets_when_the_argument_does_not() {
        assert_eq!(
            resolve_targets(Some(&["gif".to_string()])).unwrap(),
            vec![Target::Gif]
        );
        assert_eq!(resolve_targets(None).unwrap(), all_targets());
        assert_eq!(resolve_targets(Some(&[])).unwrap(), all_targets());
    }

    #[test]
    fn an_unknown_target_in_the_score_is_an_error() {
        let err = resolve_targets(Some(&["webm".to_string()]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("webm"), "unhelpful message: {err}");
    }

    #[test]
    fn the_score_targets_list_accepts_svg() {
        assert_eq!(
            resolve_targets(Some(&["svg".to_string()])).unwrap(),
            vec![Target::Svg]
        );
        assert_eq!(
            resolve_targets(Some(&["gif".to_string(), "svg".to_string()])).unwrap(),
            vec![Target::Gif, Target::Svg]
        );
    }

    use super::*;

    fn test_score(w: u32, h: u32) -> Score {
        Score {
            demo: crate::model::DemoMeta {
                name: "test".into(),
                output_dir: "./dist".into(),
                prompt: None,
                speed: None,
                targets: None,
            },
            env: None,
            typing: None,
            sources: vec![],
            layout: crate::model::Layout {
                width: w,
                height: h,
                fps: 15,
                line_height: 1.2,
                background: None,
                font_family: None,
                font_size: None,
                panes: vec![
                    crate::model::Pane {
                        id: "main".into(),
                        kind: crate::model::PaneKind::Terminal,
                        x: 0,
                        y: 0,
                        width: w / 2,
                        height: h,
                        font_family: None,
                        font_size: None,
                        url: None,
                        theme: None,
                        reveal_at: None,
                        hide_at: None,
                        ignore_speed: false,
                    },
                    crate::model::Pane {
                        id: "browser".into(),
                        kind: crate::model::PaneKind::Browser,
                        x: w / 2,
                        y: 0,
                        width: w / 2,
                        height: h,
                        font_family: None,
                        font_size: None,
                        url: Some("http://example.com".into()),
                        theme: None,
                        reveal_at: None,
                        hide_at: None,
                        ignore_speed: false,
                    },
                ],
            },
            timeline: vec![],
        }
    }

    #[test]
    fn parse_resolution_override_presets() {
        assert_eq!(
            parse_resolution_override("landscape").unwrap(),
            (1920, 1080)
        );
        assert_eq!(parse_resolution_override("portrait").unwrap(), (1080, 1920));
        assert_eq!(parse_resolution_override("square").unwrap(), (1080, 1080));
        assert_eq!(parse_resolution_override("standard").unwrap(), (1280, 720));
        assert_eq!(parse_resolution_override("fullhd").unwrap(), (1920, 1080));
    }

    #[test]
    fn parse_resolution_override_wxh() {
        assert_eq!(parse_resolution_override("800x600").unwrap(), (800, 600));
        assert_eq!(parse_resolution_override("1024×768").unwrap(), (1024, 768));
        assert_eq!(
            parse_resolution_override(" 800 x 600 ").unwrap(),
            (800, 600)
        );
    }

    #[test]
    fn parse_resolution_override_rejects_invalid() {
        assert!(parse_resolution_override("huge").is_err());
        assert!(parse_resolution_override("0x100").is_err());
        assert!(parse_resolution_override("abcxdef").is_err());
    }

    #[test]
    fn canvas_from_aspect_quality_all_ratios() {
        assert_eq!(
            canvas_from_aspect_quality("16:9", "fullhd").unwrap(),
            (1920, 1080)
        );
        assert_eq!(
            canvas_from_aspect_quality("9:16", "fullhd").unwrap(),
            (1080, 1920)
        );
        assert_eq!(
            canvas_from_aspect_quality("4:3", "fullhd").unwrap(),
            (1440, 1080)
        );
        assert_eq!(
            canvas_from_aspect_quality("1:1", "fullhd").unwrap(),
            (1080, 1080)
        );
        assert_eq!(
            canvas_from_aspect_quality("16:9", "hd").unwrap(),
            (1280, 720)
        );
    }

    #[test]
    fn canvas_from_aspect_quality_case_insensitive() {
        assert_eq!(
            canvas_from_aspect_quality("16:9", "FullHD").unwrap(),
            (1920, 1080)
        );
        assert_eq!(canvas_from_aspect_quality("1:1", "HD").unwrap(), (720, 720));
    }

    #[test]
    fn canvas_from_aspect_quality_errors() {
        assert!(canvas_from_aspect_quality("3:2", "fullhd").is_err());
        assert!(canvas_from_aspect_quality("16:9", "4k").is_err());
    }

    #[test]
    fn rescale_layout_doubles_dimensions() {
        let mut score = test_score(100, 100);
        rescale_layout(&mut score, 200, 200);
        assert_eq!(score.layout.width, 200);
        assert_eq!(score.layout.height, 200);
        assert_eq!(score.layout.panes[0].x, 0);
        assert_eq!(score.layout.panes[0].width, 100);
        assert_eq!(score.layout.panes[1].x, 100);
        assert_eq!(score.layout.panes[1].width, 100);
    }

    #[test]
    fn rescale_layout_noop_when_same() {
        let mut score = test_score(100, 100);
        rescale_layout(&mut score, 100, 100);
        assert_eq!(score.layout.panes[0].width, 50);
    }

    #[test]
    fn rescale_layout_handles_non_square() {
        let mut score = test_score(100, 100);
        rescale_layout(&mut score, 200, 100);
        assert_eq!(score.layout.width, 200);
        assert_eq!(score.layout.height, 100);
        assert_eq!(score.layout.panes[0].width, 100);
        assert_eq!(score.layout.panes[0].height, 100);
    }

    #[test]
    fn rescale_layout_scales_terminal_font_size_by_half() {
        use crate::export::run::Recording;
        let mut score: Score = toml::from_str(
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
        let rec = Recording {
            cols: 80,
            rows: 24,
            title: "t".into(),
            events: vec![],
            captions: vec![],
            focuses: vec![],
            duration: 0.0,
        };
        let plan_before = crate::export::raster::plan(&rec, &score);
        rescale_layout(&mut score, 400, 240);
        let plan_after = crate::export::raster::plan(&rec, &score);
        assert_eq!(plan_after.width, plan_before.width / 2);
        assert_eq!(plan_after.height, plan_before.height / 2);
    }

    #[test]
    fn rescale_layout_non_square_picks_smaller_font_factor() {
        let mut score: Score = toml::from_str(
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
        rescale_layout(&mut score, 50, 200);
        assert_eq!(score.layout.panes[0].font_size, Some(10));
    }

    #[test]
    fn rescale_layout_font_size_floor_is_one() {
        let mut score: Score = toml::from_str(
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
  font_size = 2
"#,
        )
        .unwrap();
        rescale_layout(&mut score, 10, 10);
        assert_eq!(score.layout.panes[0].font_size, Some(1));
    }

    #[test]
    fn rescale_layout_does_not_scale_browser_font_size() {
        let mut score = test_score(100, 100);
        score.layout.panes[1].font_size = Some(20);
        rescale_layout(&mut score, 200, 200);
        assert_eq!(score.layout.panes[1].font_size, Some(20));
    }

    #[test]
    fn resolution_override_note_names_real_original_size() {
        let note = resolution_override_note(1280, 720, 800, 480);
        assert!(note.contains("capture was 800x480"));
        assert!(!note.contains("capture was 1280x720"));
        assert!(note.contains("overriding resolution to 1280x720"));
    }

    #[test]
    fn resolve_export_resolution_from_resolution() {
        let args = ExportArgs {
            input: "test.toml".into(),
            targets: None,
            resolution: Some("800x600".into()),
            aspect: None,
            quality: None,
            speed: Some(1.0),
            force: false,
            at: None,
        };
        let r = resolve_export_resolution(&args, 1920, 1080).unwrap();
        assert_eq!(r, Some((800, 600)));
    }

    #[test]
    fn resolve_export_resolution_from_aspect() {
        let args = ExportArgs {
            input: "test.toml".into(),
            targets: None,
            resolution: None,
            aspect: Some("16:9".into()),
            quality: Some("hd".into()),
            speed: Some(1.0),
            force: false,
            at: None,
        };
        let r = resolve_export_resolution(&args, 1920, 1080).unwrap();
        assert_eq!(r, Some((1280, 720)));
    }

    #[test]
    fn resolve_export_resolution_from_quality() {
        let args = ExportArgs {
            input: "test.toml".into(),
            targets: None,
            resolution: None,
            aspect: None,
            quality: Some("fullhd".into()),
            speed: Some(1.0),
            force: false,
            at: None,
        };
        let r = resolve_export_resolution(&args, 1920, 1080).unwrap();
        assert_eq!(r, Some((1920, 1080)));
    }

    #[test]
    fn resolve_export_resolution_nothing() {
        let args = ExportArgs {
            input: "test.toml".into(),
            targets: None,
            resolution: None,
            aspect: None,
            quality: None,
            speed: Some(1.0),
            force: false,
            at: None,
        };
        let r = resolve_export_resolution(&args, 1920, 1080).unwrap();
        assert_eq!(r, None);
    }
}
