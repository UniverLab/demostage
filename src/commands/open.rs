//! `demo open` — reveal a browser scene in the running capture.
//!
//! Run it inside the capture, or **from another terminal in the same directory**
//! (so it works even while a full-screen TUI owns the captured shell). It signals
//! the recorder (see [`super::control`]); the reveal is baked into the recording
//! and composited at `export` time.
//!
//! With a URL + flags it's non-interactive; with no URL (on a terminal) it runs a
//! small wizard. Running it from a second terminal keeps the prompts out of the
//! recording. `--view` instead opens a real (headed) browser you drive yourself
//! and records it until you close the window.

use std::io::IsTerminal;
use std::time::{SystemTime, UNIX_EPOCH};

use inquire::{Select, Text};

use crate::cli::{OpenArgs, OpenMode};
use crate::commands::control;
use crate::error::{Error, Result};
use crate::export::local_server::{self, LocalServer};
use crate::file_picker::{pick_local_file, BrowseRoots};
use crate::paths::{local_file_url, looks_like_local_path, normalize_url, repair_browser_url};

/// Monospace cell size assumed by the renderer (matches `export::recording`), so a
/// `--view` recording is sized to the terminal canvas it'll be composited onto.
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;

/// A resolved reveal request: where, how, and when to open it.
struct Reveal {
    url: String,
    name: Option<String>,
    mode: String,
    /// Defer until this substring appears in the output.
    when: Option<String>,
    /// Defer until the current foreground command finishes.
    after: bool,
    hold_ms: Option<u64>,
    scroll: bool,
    /// Open a headed browser, navigate live, and record until the window closes.
    view: bool,
    /// Emulated colour scheme (`light`/`dark`), or `None` for the page default.
    theme: Option<String>,
    /// Temporary HTTP server for local files — kept alive for `--view` sessions.
    _server: Option<LocalServer>,
}

pub fn run(args: OpenArgs) -> Result<()> {
    // Running inside the captured shell (found via the env var, not the cwd)?
    // Then the command's echo + wizard print into the recording — tell the
    // recorder to mute from now, so it can excise them. From a second terminal
    // there's nothing in the captured shell to mute, so skip it.
    let in_session = std::env::var(control::CONTROL_ENV)
        .map(|p| !p.is_empty() && std::path::Path::new(&p).exists())
        .unwrap_or(false);
    if in_session {
        let _ = control::send(serde_json::json!({ "cmd": "reveal_begin" }));
    }

    let result = run_inner(args, in_session);
    if result.is_err() && in_session {
        // Close the mute span so a failure doesn't leave 90s of black.
        let _ = control::send(serde_json::json!({ "cmd": "reveal_cancel" }));
    }
    result
}

fn run_inner(args: OpenArgs, in_session: bool) -> Result<()> {
    let r = resolve(args, in_session)?;

    if r.view {
        return run_view(&r);
    }

    // Print the confirmation BEFORE signalling the recorder: in-session, the
    // control command closes the mute span, so a line printed after it would leak
    // into the demo. Printed before, it's still inside the muted span (excised).
    let how = if r.scroll {
        format!("{}, scrolling", r.mode)
    } else if let Some(ms) = r.hold_ms {
        format!("{}, hold {ms}ms", r.mode)
    } else {
        r.mode.clone()
    };
    if let Some(pat) = &r.when {
        println!("● will open {} ({how}) when output matches {pat:?}", r.url);
    } else if r.after {
        println!(
            "● will open {} ({how}) when the current command finishes",
            r.url
        );
    } else {
        println!("● opening {} ({how})", r.url);
    }

    control::send(serde_json::json!({
        "cmd": "reveal",
        "panes": reveal_panes(&r.url, &r.name, &r.mode, &r.theme),
        "orientation": "horizontal",
        "when": r.when,
        "after": r.after,
        "hold": r.hold_ms,
        "scroll": r.scroll,
    }))?;
    Ok(())
}

/// Panes for an ad-hoc `demo open`: a `split` sits the browser beside the
/// terminal (horizontal); otherwise the browser fills the canvas.
fn reveal_panes(
    url: &str,
    name: &Option<String>,
    mode: &str,
    theme: &Option<String>,
) -> Vec<serde_json::Value> {
    let browser = serde_json::json!({
        "id": name.clone().unwrap_or_else(|| "browser".to_string()),
        "url": url,
        "theme": theme,
    });
    if mode == "split" {
        vec![serde_json::json!({ "id": "main" }), browser]
    } else {
        vec![browser]
    }
}

/// `--view`: open a headed browser, record the user's session to a frames dir,
/// then reveal that pre-recorded scene (no headless Chromium needed at export).
fn run_view(r: &Reveal) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let full_w = cols as u32 * CELL_W;
    let full_h = rows as u32 * CELL_H;
    // In split mode the browser pane is half the canvas width.
    let (w, h) = if r.mode == "split" {
        (full_w / 2, full_h)
    } else {
        (full_w, full_h)
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = format!("demo-scenes/scene-{ts}");

    let n = crate::export::browser::record_view(
        &r.url,
        w,
        h,
        r.theme.as_deref(),
        std::path::Path::new(&dir),
    )?;
    if n == 0 {
        return Err(Error::Export(
            "no frames were recorded (the browser window closed before anything rendered)"
                .to_string(),
        ));
    }
    let hold_ms = (n as u64 * 1000) / crate::export::browser::VIEW_FPS as u64;

    // Confirm before signalling (see `run`): keeps the line out of the recording.
    println!("● recorded {n} frames → {dir} ({hold_ms} ms scene)");
    control::send(serde_json::json!({
        "cmd": "reveal",
        "panes": reveal_panes(&crate::model::view_frames_url(&dir), &r.name, &r.mode, &None),
        "orientation": "horizontal",
        "when": serde_json::Value::Null,
        "after": false,
        "hold": hold_ms,
        "scroll": false,
    }))?;
    Ok(())
}

/// Resolve a reveal from flags, or from the wizard when no URL is given.
fn resolve(args: OpenArgs, in_session: bool) -> Result<Reveal> {
    let mode = |a: &OpenArgs| {
        if a.split || a.mode == OpenMode::Split {
            "split"
        } else {
            "replace"
        }
        .to_string()
    };

    match &args.url {
        Some(url) if !args.wizard => {
            let launch_dir = capture_launch_dir();
            let url = if looks_like_local_path(url) {
                local_file_url(url, &launch_dir)?
            } else {
                normalize_url(url)
            };
            Ok(Reveal {
                url,
                name: None,
                mode: mode(&args),
                when: args.when.clone(),
                after: args.after,
                hold_ms: args.hold,
                scroll: args.scroll,
                view: args.view,
                theme: args.theme.map(|t| t.as_str().to_string()),
                _server: None,
            })
        }
        _ => {
            if !std::io::stdin().is_terminal() {
                return Err(Error::Export(
                    "demo open needs a URL (or a terminal for the wizard)".to_string(),
                ));
            }
            wizard(in_session)
        }
    }
}

/// Unwrap a prompt result, mapping inquire's failure onto our own error.
fn ask<T>(r: std::result::Result<T, inquire::InquireError>) -> Result<T> {
    r.map_err(|e| Error::Export(format!("wizard: {e}")))
}

fn capture_launch_dir() -> std::path::PathBuf {
    control::read_meta()
        .map(|m| m.launch_dir)
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
}

fn capture_roots() -> BrowseRoots {
    if let Some(meta) = control::read_meta() {
        BrowseRoots {
            launch_dir: meta.launch_dir,
            shell_dir: meta.shell_dir,
        }
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        BrowseRoots {
            launch_dir: cwd.clone(),
            shell_dir: cwd,
        }
    }
}

/// Ask which source to reveal: a URL (fixed up against the launch directory)
/// or a local file, which is also served over HTTP for this session. Returns
/// the URL and the live server backing it, if any.
fn pick_source_url(roots: &BrowseRoots, in_session: bool) -> Result<(String, Option<LocalServer>)> {
    let source = ask(Select::new(
        "Source:",
        vec!["URL (web page, localhost)", "Local file (PDF, PNG, HTML)"],
    )
    .prompt())?;

    if source.starts_with("Local") {
        let path = pick_local_file(roots, in_session)?;
        let (url, server) = local_server::serve_local_file(&path)?;
        eprintln!("● serving local file on http://127.0.0.1:{}", server.port());
        return Ok((url, Some(server)));
    }
    let raw = ask(Text::new("URL:")
        .with_help_message("a repo page, http://localhost…")
        .prompt())?;
    Ok((repair_browser_url(&raw, &roots.launch_dir)?, None))
}

/// How to present the reveal — a static hold, a scroll, or an interactive view.
/// These are mutually exclusive, so they're one question. Returns
/// `(view, scroll, hold_ms)`.
fn ask_behavior() -> Result<(bool, bool, Option<u64>)> {
    let behavior = ask(Select::new(
        "Show it as:",
        vec![
            "Static — hold for a few seconds",
            "Scroll the page down (pan)",
            "Interactive — open a real browser, navigate, record until you close it",
        ],
    )
    .prompt())?;
    let view = behavior.starts_with("Interactive");
    let scroll = behavior.starts_with("Scroll");
    let hold_ms = if behavior.starts_with("Static") {
        // Reject non-numbers instead of silently defaulting (a stray letter would
        // otherwise pass through as the default).
        let secs = ask(Text::new("Hold for how many seconds?")
            .with_default("6")
            .with_validator(|s: &str| {
                let s = s.trim();
                match s.parse::<f64>() {
                    Ok(n) if n > 0.0 => Ok(inquire::validator::Validation::Valid),
                    _ => Ok(inquire::validator::Validation::Invalid(
                        "enter a positive number of seconds (e.g. 6)".into(),
                    )),
                }
            })
            .prompt())?;
        let secs: f64 = secs.trim().parse().unwrap_or(6.0);
        Some((secs.max(0.5) * 1000.0) as u64)
    } else {
        None
    };
    Ok((view, scroll, hold_ms))
}

/// When the reveal fires: now, after the current command, or when a line
/// appears in the output. Returns `(when_pattern, after)`.
fn ask_trigger() -> Result<(Option<String>, bool)> {
    let trigger = ask(Select::new(
        "Reveal:",
        vec![
            "now",
            "when the current command finishes",
            "when a line appears in the output",
        ],
    )
    .prompt())?;
    if trigger.starts_with("when the current") {
        return Ok((None, true));
    }
    if trigger.starts_with("when a line") {
        let pat = ask(Text::new("Cue line (a substring of the output):").prompt())?;
        let pat = pat.trim();
        return Ok(((!pat.is_empty()).then(|| pat.to_string()), false));
    }
    Ok((None, false))
}

/// Ask for the scene identifier (e.g. `browser`, `preview`).
fn ask_scene_name() -> Result<String> {
    let scene_name = ask(inquire::Text::new("Scene name:")
        .with_help_message("identifier for this scene (e.g. 'browser', 'preview')")
        .with_default("browser")
        .prompt())?;
    Ok(scene_name.trim().to_string())
}

/// Ask for the emulated colour scheme, or `None` for the page default.
fn ask_theme() -> Result<Option<String>> {
    let theme = ask(Select::new("Browser theme:", vec!["default", "light", "dark"]).prompt())?;
    Ok(match theme {
        "light" => Some("light".to_string()),
        "dark" => Some("dark".to_string()),
        _ => None,
    })
}

/// Ask how the reveal is placed: full-screen scene swap or split beside the
/// terminal. Returns the mode string.
fn ask_placement() -> Result<String> {
    let mode = ask(Select::new(
        "Place it:",
        vec![
            "replace — full screen (scene swap)",
            "split — beside the terminal",
        ],
    )
    .prompt())?;
    Ok(if mode.starts_with("split") {
        "split"
    } else {
        "replace"
    }
    .to_string())
}

fn wizard(in_session: bool) -> Result<Reveal> {
    println!("\n  demo open — reveal a browser scene\n");

    let roots = capture_roots();
    let (url, local_server) = pick_source_url(&roots, in_session)?;

    let scene_name = ask_scene_name()?;
    let theme = ask_theme()?;

    let (view, scroll, hold_ms) = ask_behavior()?;

    // An interactive view always takes over the whole frame and opens immediately.
    if view {
        return Ok(Reveal {
            url: finalize_url(&url),
            name: Some(scene_name.clone()),
            mode: "replace".to_string(),
            when: None,
            after: false,
            hold_ms: None,
            scroll: false,
            view: true,
            theme,
            _server: local_server,
        });
    }

    let mode = ask_placement()?;

    let (when, after) = ask_trigger()?;

    Ok(Reveal {
        url: finalize_url(&url),
        name: Some(scene_name),
        mode: mode.to_string(),
        when,
        after,
        hold_ms,
        scroll,
        view: false,
        theme,
        _server: local_server,
    })
}

fn finalize_url(url: &str) -> String {
    let u = url.trim();
    if u.starts_with("file://") {
        u.to_string()
    } else {
        normalize_url(u)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_maps_inquire_errors_to_export_errors() {
        // The wizard's `ask` wrapper compiles against inquire's error type
        // (0.9): a prompt that cannot run (no TTY) surfaces as our own error.
        let err = ask::<String>(Err(inquire::InquireError::NotTTY)).unwrap_err();
        assert!(matches!(err, Error::Export(_)));
        assert_eq!(ask::<String>(Ok("ok".to_string())).unwrap(), "ok");
    }

    #[test]
    fn reveal_panes_split_returns_two() {
        let panes = reveal_panes("http://example.com", &None, "split", &None);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0]["id"], "main");
        assert_eq!(panes[1]["url"], "http://example.com");
    }

    #[test]
    fn reveal_panes_replace_returns_one() {
        let panes = reveal_panes("http://example.com", &None, "replace", &None);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["url"], "http://example.com");
    }

    #[test]
    fn reveal_panes_uses_name_as_id() {
        let panes = reveal_panes(
            "http://example.com",
            &Some("mypage".into()),
            "replace",
            &None,
        );
        assert_eq!(panes[0]["id"], "mypage");
    }

    #[test]
    fn reveal_panes_includes_theme() {
        let panes = reveal_panes("http://example.com", &None, "replace", &Some("dark".into()));
        assert_eq!(panes[0]["theme"], "dark");
    }

    #[test]
    fn reveal_panes_no_theme_is_null() {
        let panes = reveal_panes("http://example.com", &None, "replace", &None);
        assert!(panes[0]["theme"].is_null());
    }

    #[test]
    fn finalize_url_file_protocol_unchanged() {
        assert_eq!(finalize_url("file:///tmp/test.pdf"), "file:///tmp/test.pdf");
    }

    #[test]
    fn finalize_url_http_gets_normalized() {
        let result = finalize_url("example.com");
        assert!(result.starts_with("http"));
    }

    #[test]
    fn finalize_url_trims_whitespace() {
        assert_eq!(
            finalize_url("  file:///tmp/test.pdf  "),
            "file:///tmp/test.pdf"
        );
    }

    #[test]
    fn finalize_url_preserves_http() {
        assert_eq!(finalize_url("http://example.com"), "http://example.com");
    }

    #[test]
    fn finalize_url_preserves_https() {
        assert_eq!(finalize_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn reveal_panes_split_has_terminal_and_browser() {
        let panes = reveal_panes("https://docs.rs", &Some("docs".into()), "split", &None);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0]["id"], "main");
        assert_eq!(panes[1]["id"], "docs");
        assert_eq!(panes[1]["url"], "https://docs.rs");
    }

    #[test]
    fn reveal_panes_replace_only_browser() {
        let panes = reveal_panes(
            "https://example.com",
            &None,
            "replace",
            &Some("dark".into()),
        );
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["theme"], "dark");
    }

    #[test]
    fn reveal_panes_split_with_theme() {
        let panes = reveal_panes(
            "https://docs.rs",
            &Some("docs".into()),
            "split",
            &Some("light".into()),
        );
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[1]["theme"], "light");
    }

    #[test]
    fn reveal_panes_replace_with_name_and_theme() {
        let panes = reveal_panes(
            "https://example.com",
            &Some("mypage".into()),
            "replace",
            &Some("dark".into()),
        );
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["id"], "mypage");
        assert_eq!(panes[0]["theme"], "dark");
    }

    #[test]
    fn finalize_url_trailing_whitespace() {
        assert_eq!(
            finalize_url("  https://example.com  "),
            "https://example.com"
        );
    }

    #[test]
    fn finalize_url_just_whitespace() {
        // Whitespace only, after trim it's empty, goes to normalize_url
        let result = finalize_url("  ");
        assert!(result.starts_with("http"));
    }

    #[test]
    fn reveal_panes_split_has_two() {
        let panes = reveal_panes("https://example.com", &None, "split", &None);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0]["id"], "main");
        assert_eq!(panes[1]["url"], "https://example.com");
    }

    #[test]
    fn reveal_panes_replace_has_one() {
        let panes = reveal_panes("https://example.com", &None, "replace", &None);
        assert_eq!(panes.len(), 1);
    }

    #[test]
    fn reveal_panes_split_default_browser_id() {
        let panes = reveal_panes("https://example.com", &None, "split", &None);
        assert_eq!(panes[1]["id"], "browser");
    }

    #[test]
    fn finalize_url_file_with_trailing_space() {
        assert_eq!(
            finalize_url("  file:///tmp/test.pdf  "),
            "file:///tmp/test.pdf"
        );
    }

    #[test]
    fn finalize_url_preserves_file_protocol() {
        assert_eq!(
            finalize_url("file:///home/user/doc.html"),
            "file:///home/user/doc.html"
        );
    }

    #[test]
    fn reveal_panes_replace_no_theme_is_null() {
        let panes = reveal_panes("https://example.com", &None, "replace", &None);
        assert!(panes[0]["theme"].is_null());
    }

    #[test]
    fn reveal_panes_split_both_have_url() {
        let panes = reveal_panes("https://example.com", &Some("docs".into()), "split", &None);
        assert_eq!(panes[0]["id"], "main");
        assert!(panes[0]["url"].is_null());
        assert_eq!(panes[1]["url"], "https://example.com");
    }

    #[test]
    fn finalize_url_http_unchanged() {
        assert_eq!(
            finalize_url("http://localhost:3000"),
            "http://localhost:3000"
        );
    }

    #[test]
    fn finalize_url_https_unchanged() {
        assert_eq!(
            finalize_url("https://example.com/path"),
            "https://example.com/path"
        );
    }
}
