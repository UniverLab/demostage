//! `demo export` — compile a score to a target format.
//!
//! `cast`/`html` run the score in a PTY and capture text (no external deps).
//! `gif` rasterizes that capture in pure Rust. `mp4` provisions ffmpeg on first
//! use. `svg` (opt-in) exports the whole timeline as animated vector text for
//! terminal-only demos, or one static poster frame with `--at <seconds>`.
//! Multi-pane scores (with a `browser` pane) composite via the stage,
//! which drives Chromium for browser panes (PDF panes render natively via hayro).

pub mod browser;
pub mod composite;
pub mod gif;
pub mod local_server;
pub mod mp4;
pub mod pdf;
pub mod provision;
pub mod raster;
pub mod recording;
pub mod run;
pub mod stage;
pub mod svg;

use std::path::{Path, PathBuf};

use crate::cli::Target;
use crate::error::{Error, Result};
use crate::model::Score;
use crate::validate::validate;

use run::{progress_bar, progress_clear, Recording};

/// Start a local HTTP server if any pane needs one (local `file://` URLs or
/// wizard localhost URLs). Returns the server (must be kept alive while
/// rendering) and a port number, or `None` if no server is needed.
pub fn ensure_local_server(score: &Score) -> Result<Option<local_server::LocalServer>> {
    let has_local_files = score.layout.panes.iter().any(|p| {
        p.url
            .as_ref()
            .is_some_and(|u| u.starts_with("file://") || is_localhost_wizard_url(u))
    });
    if has_local_files {
        let server = local_server::LocalServer::start(std::path::Path::new("/"))?;
        eprintln!(
            "● serving local files on http://127.0.0.1:{}",
            server.port()
        );
        Ok(Some(server))
    } else {
        Ok(None)
    }
}

/// Rewrite local/wizard URLs in a score clone using the given server port.
pub fn rewrite_local_urls(score: &Score, server_port: u16) -> Score {
    let mut score = score.clone();
    for pane in &mut score.layout.panes {
        if let Some(url) = &pane.url {
            if url.starts_with("file://") {
                // Build the http URL using the server port
                if let Some(rest) = url.strip_prefix("file://") {
                    pane.url = Some(format!(
                        "http://127.0.0.1:{}/{}",
                        server_port,
                        rest.trim_start_matches('/')
                    ));
                }
            } else if is_localhost_wizard_url(url) {
                pane.url = Some(rewrite_wizard_url(url, server_port));
            }
        }
    }
    score
}

/// The resolved geometry and rate a target renders at, plus how the frames are
/// produced (staged compositing vs. a single terminal grid).
struct RenderPlan {
    /// Multi-pane scores composite on the canvas through [`stage::render_stage`].
    staged: bool,
    cw: usize,
    ch: usize,
    fps: u32,
    total_frames: usize,
    /// The resolved export speed multiplier, threaded through to the PDF pan path.
    speed: f64,
}

/// Print one line per browser pane that was captured for a staged export.
fn report_browser_captures(reports: &[browser::BrowserCaptureReport]) {
    for br in reports {
        eprintln!(
            "demo: browser pane '{}' — {} frames captured in {:.1}s",
            br.pane_id,
            br.frame_count,
            br.elapsed.as_secs_f64()
        );
    }
}

/// Render an already-captured `recording` to `target`, returning the path
/// written. Pure playback — it never executes the demo. `score` carries the
/// layout/styling (its timeline is unused here). `speed` is the resolved export
/// speed multiplier, threaded through to the PDF pan path. `at_secs` is only
/// `Target::Svg`: `None` exports the animated timeline (terminal-only), while
/// `Some(t)` draws the poster of the frame at `t` seconds (default: the last
/// one) — gif/mp4 ignore it.
pub fn render(
    rec: &Recording,
    score: &Score,
    target: Target,
    speed: f64,
    at_secs: Option<f64>,
) -> Result<PathBuf> {
    let problems = validate(score);
    if !problems.is_empty() {
        return Err(Error::Validation(problems.join("\n")));
    }

    let score = score.clone();

    let staged = stage::needs_stage(&score);
    let fps = score.layout.fps.max(1);
    // Two distinct frame geometries:
    //  - staged: compositing on the canvas → layout.width/height
    //  - single-terminal: the pane grid → cols*cell_w × rows*cell_h
    let (cw, ch) = if staged {
        (score.layout.width as usize, score.layout.height as usize)
    } else {
        let plan = raster::plan(rec, &score);
        (plan.width, plan.height)
    };
    let total_frames = (rec.duration * fps as f64).ceil() as usize + 1;
    let plan = RenderPlan {
        staged,
        cw,
        ch,
        fps,
        total_frames,
        speed,
    };

    match target {
        Target::Gif => render_gif(rec, &score, &plan),
        Target::Mp4 => render_mp4(rec, &score, &plan),
        Target::Svg => render_svg(rec, &score, &plan, at_secs),
    }
}

/// Render every frame into an animated GIF at the resolved geometry.
fn render_gif(rec: &Recording, score: &Score, plan: &RenderPlan) -> Result<PathBuf> {
    let path = resolve_output(score, "gif");
    ensure_parent(&path)?;
    let mut report = raster::FallbackReport::new();
    if plan.staged {
        let mut n = 0usize;
        let mut browser_reports = Vec::new();
        gif::encode(&path, plan.cw, plan.ch, plan.fps, |emit| {
            let r = stage::render_stage(rec, score, plan.speed, |f| {
                n += 1;
                progress_bar("exporting gif", n, plan.total_frames);
                emit(f);
            })?;
            report = r.0;
            browser_reports = r.1;
            Ok(())
        })?;
        progress_clear();
        report_browser_captures(&browser_reports);
    } else {
        let mut n = 0usize;
        gif::encode(&path, plan.cw, plan.ch, plan.fps, |emit| {
            let (_plan, r) = raster::render_frames(rec, score, |f| {
                n += 1;
                progress_bar("exporting gif", n, plan.total_frames);
                emit(f);
            })?;
            report = r;
            Ok(())
        })?;
        progress_clear();
    }
    for line in report.format(&score.demo.name) {
        eprintln!("{line}");
    }
    Ok(path)
}

/// Render every frame into an MP4 (via ffmpeg) at the resolved geometry.
fn render_mp4(rec: &Recording, score: &Score, plan: &RenderPlan) -> Result<PathBuf> {
    let path = resolve_output(score, "mp4");
    ensure_parent(&path)?;
    let mut report = raster::FallbackReport::new();
    if plan.staged {
        let mut n = 0usize;
        let mut browser_reports = Vec::new();
        mp4::encode(&path, plan.cw, plan.ch, plan.fps, |emit| {
            let r = stage::render_stage(rec, score, plan.speed, |f| {
                n += 1;
                progress_bar("exporting mp4", n, plan.total_frames);
                emit(f);
            })?;
            report = r.0;
            browser_reports = r.1;
            Ok(())
        })?;
        progress_clear();
        report_browser_captures(&browser_reports);
    } else {
        let mut n = 0usize;
        mp4::encode(&path, plan.cw, plan.ch, plan.fps, |emit| {
            let (_plan, r) = raster::render_frames(rec, score, |f| {
                n += 1;
                progress_bar("exporting mp4", n, plan.total_frames);
                emit(f);
            })?;
            report = r;
            Ok(())
        })?;
        progress_clear();
    }
    for line in report.format(&score.demo.name) {
        eprintln!("{line}");
    }
    Ok(path)
}

/// Draw one frame as vector text (the poster) — staged scores replay the stage
/// and keep ONE composited frame, embedded as a base64 PNG in the SVG.
/// Without `--at`, a single-terminal score exports the whole timeline as an
/// animated SVG; staged scores refuse the animated path (no cell grid to walk)
/// and must name `--at` for a poster.
fn render_svg(
    rec: &Recording,
    score: &Score,
    plan: &RenderPlan,
    at_secs: Option<f64>,
) -> Result<PathBuf> {
    let path = resolve_svg_output(score, at_secs);
    ensure_parent(&path)?;
    if at_secs.is_some() {
        return render_svg_poster(rec, score, plan, &path, at_secs);
    }
    if plan.staged {
        return Err(Error::Export(
            svg::animated_refusal(score).expect("staged score without --at must refuse"),
        ));
    }
    // Single terminal: walk the whole replay as vector states — the frames in
    // between are never rasterized.
    progress_bar("exporting svg", 1, 1);
    svg::write_animated(&path, rec, score)?;
    progress_clear();
    Ok(path)
}

/// The `--at` poster: one frame, vector text for a single terminal or one
/// embedded PNG for a staged score.
fn render_svg_poster(
    rec: &Recording,
    score: &Score,
    plan: &RenderPlan,
    path: &Path,
    at_secs: Option<f64>,
) -> Result<PathBuf> {
    let mut report = raster::FallbackReport::new();
    if plan.staged {
        // Fallback, documented in docs/export-targets.md: a staged
        // score has no cell grid to draw, so replay the stage and keep
        // ONE composited frame, embedded as a base64 PNG in the SVG.
        let keep = svg::frame_index(at_secs, plan.fps, plan.total_frames);
        let mut n = 0usize;
        let mut browser_reports = Vec::new();
        svg::encode(path, plan.cw, plan.ch, keep, |emit| {
            let r = stage::render_stage(rec, score, plan.speed, |f| {
                n += 1;
                progress_bar("exporting svg", n, plan.total_frames);
                emit(&svg::PosterFrame::Rgba(f));
            })?;
            report = r.0;
            browser_reports = r.1;
            Ok(())
        })?;
        progress_clear();
        report_browser_captures(&browser_reports);
    } else {
        // Single terminal: seek to the chosen frame and draw it as
        // vector text — the frames in between are never rasterized.
        progress_bar("exporting svg", 1, 1);
        svg::write_svg(path, rec, score, at_secs)?;
        progress_clear();
    }
    for line in report.format(&score.demo.name) {
        eprintln!("{line}");
    }
    Ok(path.to_path_buf())
}

/// Retime a recording by `1/speed` (so `speed = 2.0` plays twice as fast, `0.5`
/// half as fast): every output event, caption and focus, plus the duration.
pub fn scale_recording(rec: &mut Recording, speed: f64) {
    if speed == 1.0 {
        return;
    }
    let scale = |t: f64| t / speed;
    for (t, _) in &mut rec.events {
        *t = scale(*t);
    }
    for (t, _) in &mut rec.captions {
        *t = scale(*t);
    }
    for (t, _) in &mut rec.focuses {
        *t = scale(*t);
    }
    rec.duration = scale(rec.duration);
}

/// Retime the layout's pane reveal/hide windows by `1/speed`. They're absolute
/// times on the same clock as the recording, so a `--speed` export must scale
/// them together with [`scale_recording`] — otherwise a pane's window can slide
/// past the (shortened) playback and the pane never shows.
///
/// Also retimes every `Step::Scroll`'s `duration_ms`, so the whole timeline is
/// in one unit (output seconds). A pane with `ignore_speed` is exempt: its
/// scroll duration stays in recording seconds, so it pans at the 1x cap.
pub fn scale_pane_windows(score: &mut Score, speed: f64) {
    if speed == 1.0 {
        return;
    }
    for pane in &mut score.layout.panes {
        if let Some(t) = &mut pane.reveal_at {
            *t /= speed;
        }
        if let Some(t) = &mut pane.hide_at {
            *t /= speed;
        }
    }
    let ignore_speed_pane_ids: Vec<String> = score
        .layout
        .panes
        .iter()
        .filter(|p| p.ignore_speed)
        .map(|p| p.id.clone())
        .collect();
    let mut focused: Option<&str> = None;
    for step in &mut score.timeline {
        match step {
            crate::model::Step::Focus { pane } => {
                focused = pane.as_deref();
            }
            crate::model::Step::Scroll {
                duration_ms, pane, ..
            } => {
                let target = pane.as_deref().or(focused);
                let is_exempt = target
                    .and_then(|id| ignore_speed_pane_ids.iter().find(|eid| eid.as_str() == id))
                    .is_some();
                if !is_exempt {
                    *duration_ms = (*duration_ms as f64 / speed).round() as u64;
                }
            }
            _ => {}
        }
    }
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
    }
    Ok(())
}

fn resolve_output(score: &Score, ext: &str) -> PathBuf {
    score
        .demo
        .output_dir
        .join(format!("{}.{ext}", sanitize(&score.demo.name)))
}

/// The SVG output path: the animated timeline is `dist/<name>.svg`, while a
/// `--at` poster carries its timestamp (`dist/<name>-at-12.5.svg`) so posters
/// never overwrite each other or the animation. The filename uses the
/// requested value, not the clamped frame.
fn resolve_svg_output(score: &Score, at_secs: Option<f64>) -> PathBuf {
    match at_secs {
        None => resolve_output(score, "svg"),
        Some(t) => score
            .demo
            .output_dir
            .join(format!("{}-at-{t}.svg", sanitize(&score.demo.name))),
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Check if a URL is a localhost URL generated by a wizard (http://127.0.0.1:PORT/path).
fn is_localhost_wizard_url(url: &str) -> bool {
    url.starts_with("http://127.0.0.1:")
}

/// Rewrite a wizard-generated localhost URL to use a new port.
/// e.g. "http://127.0.0.1:8001/home/user/doc.pdf" with new port 9001
///   → "http://127.0.0.1:9001/home/user/doc.pdf"
fn rewrite_wizard_url(url: &str, new_port: u16) -> String {
    let rest = url.strip_prefix("http://127.0.0.1:").unwrap_or(url);
    if let Some(path_start) = rest.find('/') {
        format!("http://127.0.0.1:{}{}", new_port, &rest[path_start..])
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize("my demo!"), "my-demo-");
        assert_eq!(sanitize("ok_name-1"), "ok_name-1");
        assert_eq!(sanitize("a/b\\c"), "a-b-c");
    }

    fn rec() -> Recording {
        Recording {
            cols: 80,
            rows: 24,
            title: "t".into(),
            events: vec![(0.5, "a".into()), (1.0, "b".into())],
            captions: vec![(0.5, "cap".into())],
            focuses: vec![(0.25, "main".into())],
            duration: 1.0,
        }
    }

    #[test]
    fn speed_2x_halves_recorded_timestamps() {
        let mut r = rec();
        scale_recording(&mut r, 2.0);
        assert_eq!(r.events, vec![(0.25, "a".into()), (0.5, "b".into())]);
        assert_eq!(r.captions, vec![(0.25, "cap".into())]);
        assert_eq!(r.focuses, vec![(0.125, "main".into())]);
        assert_eq!(r.duration, 0.5);
    }

    #[test]
    fn speed_half_doubles_recorded_timestamps() {
        let mut r = rec();
        scale_recording(&mut r, 0.5);
        assert_eq!(r.events, vec![(1.0, "a".into()), (2.0, "b".into())]);
        assert_eq!(r.duration, 2.0);
    }

    #[test]
    fn speed_1x_is_a_no_op() {
        let mut r = rec();
        scale_recording(&mut r, 1.0);
        assert_eq!(r.events, rec().events);
    }

    #[test]
    fn speed_scales_pane_windows_with_the_recording() {
        let mut score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "main"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "b"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "https://x"
  reveal_at = 20.0
  hide_at = 30.0
"#,
        )
        .unwrap();
        scale_pane_windows(&mut score, 2.0);
        let b = &score.layout.panes[1];
        assert_eq!(b.reveal_at, Some(10.0));
        assert_eq!(b.hide_at, Some(15.0));
        // The terminal pane has no window — untouched.
        assert_eq!(score.layout.panes[0].reveal_at, None);
    }

    #[test]
    fn is_localhost_wizard_url_true() {
        assert!(is_localhost_wizard_url("http://127.0.0.1:8080/file.pdf"));
        assert!(is_localhost_wizard_url("http://127.0.0.1:3000/"));
    }

    #[test]
    fn is_localhost_wizard_url_false() {
        assert!(!is_localhost_wizard_url("https://example.com"));
        assert!(!is_localhost_wizard_url("http://localhost:3000/"));
        assert!(!is_localhost_wizard_url("file:///tmp/test.pdf"));
    }

    #[test]
    fn rewrite_wizard_url_changes_port() {
        let url = "http://127.0.0.1:8080/home/user/doc.pdf";
        assert_eq!(
            rewrite_wizard_url(url, 9001),
            "http://127.0.0.1:9001/home/user/doc.pdf"
        );
    }

    #[test]
    fn rewrite_wizard_url_no_path() {
        let url = "http://127.0.0.1:8080";
        assert_eq!(rewrite_wizard_url(url, 9001), url);
    }

    #[test]
    fn rewrite_local_urls_converts_file_to_http() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "main"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "b"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "file:///tmp/test.pdf"
"#,
        )
        .unwrap();
        let rewritten = rewrite_local_urls(&score, 8080);
        let b = &rewritten.layout.panes[1];
        assert!(b.url.as_ref().unwrap().contains("8080"));
        assert!(b.url.as_ref().unwrap().starts_with("http://127.0.0.1:"));
    }

    #[test]
    fn rewrite_local_urls_converts_wizard_url() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "main"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "b"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "http://127.0.0.1:3000/page.html"
"#,
        )
        .unwrap();
        let rewritten = rewrite_local_urls(&score, 9000);
        let b = &rewritten.layout.panes[1];
        assert_eq!(b.url.as_deref(), Some("http://127.0.0.1:9000/page.html"));
    }

    #[test]
    fn rewrite_local_urls_leaves_https_untouched() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "main"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "b"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "https://example.com"
"#,
        )
        .unwrap();
        let rewritten = rewrite_local_urls(&score, 9000);
        let b = &rewritten.layout.panes[1];
        assert_eq!(b.url.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn resolve_output_sanitizes_name() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "my demo!"
output_dir = "./dist"
[layout]
width = 100
height = 100
"#,
        )
        .unwrap();
        let path = resolve_output(&score, "gif");
        assert_eq!(path, std::path::PathBuf::from("./dist/my-demo-.gif"));
    }

    fn svg_score() -> Score {
        toml::from_str(
            r#"
[demo]
name = "demo"
output_dir = "./dist"
[layout]
width = 100
height = 100
"#,
        )
        .unwrap()
    }

    #[test]
    fn poster_filename_is_per_at() {
        assert_eq!(
            resolve_svg_output(&svg_score(), None),
            std::path::PathBuf::from("./dist/demo.svg")
        );
        assert_eq!(
            resolve_svg_output(&svg_score(), Some(12.5)),
            std::path::PathBuf::from("./dist/demo-at-12.5.svg")
        );
        // Rust prints 12.0 as "12": posters never overwrite each other or the
        // animation, and the name uses the requested value, not the clamp.
        assert_eq!(
            resolve_svg_output(&svg_score(), Some(12.0)),
            std::path::PathBuf::from("./dist/demo-at-12.svg")
        );
    }

    #[test]
    fn resolve_output_html() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "test"
output_dir = "./out"
[layout]
width = 100
height = 100
"#,
        )
        .unwrap();
        let path = resolve_output(&score, "html");
        assert_eq!(path, std::path::PathBuf::from("./out/test.html"));
    }

    #[test]
    fn ensure_parent_creates_directory() {
        let dir = std::env::temp_dir().join("demostage_test_ensure_parent");
        let file = dir.join("sub/file.txt");
        let _ = std::fs::remove_dir_all(&dir);
        ensure_parent(&file).unwrap();
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_parent_empty_path_is_ok() {
        let p = std::path::Path::new("");
        ensure_parent(p).unwrap();
    }

    #[test]
    fn sanitize_empty_string() {
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn sanitize_all_special_chars() {
        assert_eq!(sanitize("!@#$%^&*()"), "----------");
    }

    #[test]
    fn sanitize_preserves_underscores() {
        assert_eq!(sanitize("my_demo_name"), "my_demo_name");
    }

    #[test]
    fn sanitize_preserves_hyphens() {
        assert_eq!(sanitize("my-demo-name"), "my-demo-name");
    }

    #[test]
    fn is_localhost_wizard_url_with_port_only() {
        assert!(is_localhost_wizard_url("http://127.0.0.1:8080"));
    }

    #[test]
    fn is_localhost_wizard_url_with_path() {
        assert!(is_localhost_wizard_url("http://127.0.0.1:3000/page.html"));
    }

    #[test]
    fn is_localhost_wizard_url_localhost_not_127() {
        assert!(!is_localhost_wizard_url("http://localhost:3000/"));
    }

    #[test]
    fn is_localhost_wizard_url_https() {
        assert!(!is_localhost_wizard_url("https://127.0.0.1:8080/"));
    }

    #[test]
    fn rewrite_wizard_url_with_complex_path() {
        let url = "http://127.0.0.1:8080/home/user/file.pdf?query=1";
        assert_eq!(
            rewrite_wizard_url(url, 9001),
            "http://127.0.0.1:9001/home/user/file.pdf?query=1"
        );
    }

    #[test]
    fn rewrite_local_urls_no_browser_panes() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "main"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
"#,
        )
        .unwrap();
        let rewritten = rewrite_local_urls(&score, 8080);
        assert_eq!(rewritten.layout.panes.len(), 1);
    }

    #[test]
    fn rewrite_local_urls_multiple_panes() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "main"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "b1"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "file:///tmp/test1.pdf"
  [[layout.panes]]
  id = "b2"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "file:///tmp/test2.pdf"
"#,
        )
        .unwrap();
        let rewritten = rewrite_local_urls(&score, 8080);
        let b1 = &rewritten.layout.panes[1];
        let b2 = &rewritten.layout.panes[2];
        assert!(b1.url.as_ref().unwrap().contains("8080"));
        assert!(b2.url.as_ref().unwrap().contains("8080"));
    }

    #[test]
    fn resolve_output_custom_dir() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "test"
output_dir = "/tmp/demos"
[layout]
width = 100
height = 100
"#,
        )
        .unwrap();
        let path = resolve_output(&score, "gif");
        assert_eq!(path, std::path::PathBuf::from("/tmp/demos/test.gif"));
    }

    #[test]
    fn scale_recording_with_empty_events() {
        let mut r = Recording {
            cols: 80,
            rows: 24,
            title: "t".into(),
            events: vec![],
            captions: vec![],
            focuses: vec![],
            duration: 0.0,
        };
        scale_recording(&mut r, 2.0);
        assert_eq!(r.duration, 0.0);
    }

    #[test]
    fn scale_pane_windows_with_no_reveal() {
        let mut score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "main"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
"#,
        )
        .unwrap();
        scale_pane_windows(&mut score, 2.0);
        assert_eq!(score.layout.panes[0].reveal_at, None);
    }

    #[test]
    fn ensure_parent_nonexistent_path() {
        let dir = std::env::temp_dir().join(format!("demostage_test_{}", std::process::id()));
        let file = dir.join("deep/nested/file.txt");
        let _ = std::fs::remove_dir_all(&dir);
        ensure_parent(&file).unwrap();
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scroll step's duration_ms is scaled by scale_pane_windows for a normal
    /// pane and left alone for an exempt one (ignore_speed = true).
    #[test]
    fn scale_pane_windows_scales_scroll_duration_for_normal_pane() {
        let mut score: Score = toml::from_str(
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
duration_ms = 8000
pane = "p"
"#,
        )
        .unwrap();
        scale_pane_windows(&mut score, 2.0);
        if let crate::model::Step::Scroll { duration_ms, .. } = &score.timeline[1] {
            assert_eq!(*duration_ms, 4000, "duration_ms should be halved at 2x");
        } else {
            panic!("expected Scroll step");
        }
    }

    #[test]
    fn scale_pane_windows_leaves_scroll_duration_for_ignore_speed_pane() {
        let mut score: Score = toml::from_str(
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
  ignore_speed = true
[[timeline]]
action = "focus"
pane = "p"
[[timeline]]
action = "scroll"
direction = "down"
duration_ms = 8000
pane = "p"
"#,
        )
        .unwrap();
        scale_pane_windows(&mut score, 2.0);
        if let crate::model::Step::Scroll { duration_ms, .. } = &score.timeline[1] {
            assert_eq!(
                *duration_ms, 8000,
                "duration_ms should be unchanged for ignore_speed pane"
            );
        } else {
            panic!("expected Scroll step");
        }
    }
}
