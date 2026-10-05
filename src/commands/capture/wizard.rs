//! The interactive setup a capture runs before the shell starts: prompt style,
//! export font, canvas geometry, frame rate and browser sources. Every choice
//! is skipped when a flag already provides it (or stdin is not a terminal).

use std::io::IsTerminal;

use crate::cli::CaptureArgs;
use crate::error::{Error, Result};
use crate::export::local_server::{self, LocalServer};
use crate::export::run::is_zsh;
use crate::file_picker::{pick_local_file, BrowseRoots};
use crate::paths::{file_url_absolute, repair_browser_url};

/// Escape a user-entered label for safe embedding in a bash `PS1`: a literal
/// backslash must be doubled, else bash reads it as a prompt escape (`\d` = date,
/// `\w` = cwd, …) — e.g. a PowerShell path `C:\Users` would otherwise mangle.
fn ps1_text(s: &str) -> String {
    s.replace('\\', "\\\\")
}

/// Build a prompt string with coloured segments for the detected shell.
fn colored(shell: &str, ansi: &str, zsh_col: &str, text: &str) -> String {
    if is_zsh(shell) {
        format!("%B%F{{{zsh_col}}}{text}%f%b")
    } else {
        format!("\\[\\e[{ansi}m\\]{text}\\[\\e[0m\\]")
    }
}

/// Default prompt for the given shell.
pub fn default_prompt(shell: &str) -> String {
    let user = colored(shell, "1;32", "green", "user@demo");
    let path = colored(shell, "1;34", "blue", "~");
    format!("{user}:{path}$ ")
}

/// Decide the captured shell's prompt: `--keep-prompt` keeps yours, `--prompt`
/// forces a given `PS1`, and with neither a quick wizard offers ready-made styles
/// (you edit only the label text; colours are chosen for you). Returns
/// `(force_prompt, ps1)`.
pub(super) fn choose_prompt(args: &CaptureArgs, shell: &str) -> Result<(bool, String)> {
    if args.keep_prompt {
        return Ok((false, default_prompt(shell)));
    }
    if let Some(p) = &args.prompt {
        return Ok((true, p.clone()));
    }

    let style = inquire::Select::new(
        "Prompt style for this demo:",
        vec![
            "Linux        user@host:~$",
            "macOS        user@host ~ %",
            "PowerShell   PS path>",
            "Minimal      ❯",
            "Keep my real prompt",
        ],
    )
    .prompt()
    .map_err(|e| Error::Export(format!("prompt wizard: {e}")))?;

    // Ask one editable text with a sensible default (blank keeps the default).
    let ask = |q: &str, default: &str| -> Result<String> {
        let v = inquire::Text::new(q)
            .with_default(default)
            .prompt()
            .map_err(|e| Error::Export(format!("prompt wizard: {e}")))?;
        let v = v.trim();
        Ok(if v.is_empty() {
            default.to_string()
        } else {
            v.to_string()
        })
    };

    // Colours are baked into each template; the user only fills the text.
    let ps1 = if style.starts_with("Linux") {
        let l = ps1_text(&ask("Text (user@host):", "user@demo")?);
        let user = colored(shell, "1;32", "green", &l);
        let path = colored(shell, "1;34", "blue", "~");
        format!("{user}:{path}$ ")
    } else if style.starts_with("macOS") {
        let l = ps1_text(&ask("Text (user@host):", "user@mac")?);
        let user = colored(shell, "1;36", "cyan", &l);
        format!("{user} ~ % ")
    } else if style.starts_with("PowerShell") {
        let p = ps1_text(&ask("Path:", "C:\\Users\\demo")?);
        let path = colored(shell, "1;36", "cyan", &p);
        format!("PS {path}> ")
    } else if style.starts_with("Minimal") {
        let s = ps1_text(&ask("Symbol:", "❯")?);
        let sym = colored(shell, "1;32", "green", &s);
        format!("{sym} ")
    } else {
        return Ok((false, default_prompt(shell)));
    };
    Ok((true, ps1))
}

/// Choose the export font. `--font` skips the wizard; otherwise a picker
/// offers the bundled options.
pub(super) fn choose_font(args: &CaptureArgs) -> Result<String> {
    if let Some(f) = &args.font {
        return Ok(f.clone());
    }
    if !std::io::stdin().is_terminal() {
        return Ok(crate::fonts::DEFAULT_FONT.to_string());
    }
    let choice = inquire::Select::new("Export font:", crate::fonts::FONT_NAMES.to_vec())
        .prompt()
        .map_err(|e| Error::Export(format!("font wizard: {e}")))?;
    Ok(crate::fonts::parse_font_name(choice).to_string())
}

/// Legacy named resolution presets, accepted as `--resolution` aliases. Each
/// maps onto an aspect-ratio × quality pair under the new scheme (e.g.
/// `landscape` = `16:9` × `fullhd`, `standard` = `16:9` × `hd`).
const RESOLUTIONS: [(&str, u32, u32); 4] = [
    ("landscape", 1920, 1080),
    ("portrait", 1080, 1920),
    ("square", 1080, 1080),
    ("standard", 1280, 720),
];

/// Aspect ratios offered at capture. `a:b` means width:height = a:b; the canvas
/// is scaled so its short side matches the quality base.
const ASPECTS: [(&str, u32, u32); 4] = [
    ("16:9", 16, 9),
    ("9:16", 9, 16),
    ("4:3", 4, 3),
    ("1:1", 1, 1),
];

/// Quality tiers — the short side of the canvas, in pixels.
const QUALITIES: [(&str, u32); 2] = [("fullhd", 1080), ("hd", 720)];

/// Default frame rate (matches `[layout] fps` default in the score model).
pub(super) const DEFAULT_FPS: u32 = 15;

/// Permitted frame rates for the exported gif/mp4.
const FPS_CHOICES: [u32; 3] = [15, 24, 30];

/// Compute the canvas `(width, height)` for an aspect ratio + quality. The
/// short side is the quality base; the long side scales by the ratio. Every
/// combination lands on integer pixels (1080 and 720 are divisible by 9, 3, 1).
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

/// Parse a `--resolution` value: a legacy preset name, `WxH`, or `auto` (→
/// `None`, meaning the canvas derives from the terminal size).
fn parse_resolution(s: &str) -> Result<Option<(u32, u32)>> {
    let v = s.trim().to_ascii_lowercase();
    if v == "auto" {
        return Ok(None);
    }
    if let Some(&(_, w, h)) = RESOLUTIONS.iter().find(|(name, ..)| *name == v) {
        return Ok(Some((w, h)));
    }
    if let Some((w, h)) = v.split_once(['x', '×']) {
        if let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
            if w > 0 && h > 0 {
                return Ok(Some((w, h)));
            }
        }
    }
    Err(Error::Export(format!(
        "invalid resolution '{s}' — try a WxH pair (e.g. 1600x900) or auto; or use --aspect/--quality"
    )))
}

/// Parse a `--fps` value: must be one of 15, 24, 30.
fn parse_fps(s: &str) -> Result<u32> {
    let n: u32 = s
        .trim()
        .parse()
        .map_err(|_| Error::Export(format!("invalid fps '{s}' — try 15, 24, or 30")))?;
    if FPS_CHOICES.contains(&n) {
        Ok(n)
    } else {
        Err(Error::Export(format!(
            "unsupported fps {n} — try 15, 24, or 30"
        )))
    }
}

/// Canvas for the export: `--resolution` (explicit/auto) wins, else
/// `--aspect`×`--quality`, else a wizard. `None` = auto (derive from the
/// terminal size) — also the non-interactive default.
pub(super) fn choose_canvas(args: &CaptureArgs) -> Result<Option<(u32, u32)>> {
    if let Some(r) = &args.resolution {
        return parse_resolution(r);
    }
    if let Some(a) = &args.aspect {
        let q = args.quality.as_deref().unwrap_or("fullhd");
        return Ok(Some(canvas_from_aspect_quality(a, q)?));
    }
    if let Some(q) = &args.quality {
        return Ok(Some(canvas_from_aspect_quality("16:9", q)?));
    }
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let choice = inquire::Select::new(
        "Aspect ratio:",
        vec![
            "16:9   (widescreen)",
            "9:16   (portrait / vertical)",
            "4:3    (classic)",
            "1:1    (square)",
            "Auto   (derive from terminal size)",
            "Custom (enter WxH)",
        ],
    )
    .prompt()
    .map_err(|e| Error::Export(format!("aspect wizard: {e}")))?;

    let av = choice.to_ascii_lowercase();
    if av.starts_with("auto") {
        return Ok(None);
    }
    if av.starts_with("custom") {
        let w = inquire::Text::new("width:")
            .with_default("1920")
            .prompt()
            .map_err(|e| Error::Export(format!("aspect wizard: {e}")))?;
        let h = inquire::Text::new("height:")
            .with_default("1080")
            .prompt()
            .map_err(|e| Error::Export(format!("aspect wizard: {e}")))?;
        return parse_resolution(&format!("{}x{}", w.trim(), h.trim()));
    }
    let ratio = av.split_whitespace().next().unwrap_or(&av);
    let quality = inquire::Select::new("Quality:", vec!["FullHD  (1080p)", "HD      (720p)"])
        .prompt()
        .map_err(|e| Error::Export(format!("quality wizard: {e}")))?;
    let q = if quality.to_ascii_lowercase().starts_with("full") {
        "fullhd"
    } else {
        "hd"
    };
    canvas_from_aspect_quality(ratio, q).map(Some)
}

/// Frame rate for the export: `--fps`, else a wizard. Defaults to
/// [`DEFAULT_FPS`] when non-interactive.
pub(super) fn choose_fps(args: &CaptureArgs) -> Result<u32> {
    if let Some(f) = args.fps {
        return parse_fps(&f.to_string());
    }
    if !std::io::stdin().is_terminal() {
        return Ok(DEFAULT_FPS);
    }
    let choice = inquire::Select::new("Frame rate:", vec!["15 fps", "24 fps", "30 fps"])
        .prompt()
        .map_err(|e| Error::Export(format!("fps wizard: {e}")))?;
    choice
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u32>()
        .map_err(|e| Error::Export(format!("fps wizard: {e}")))
}

/// Ask if the user wants to add browser sources, loop through adding them.
pub(super) fn choose_sources(
    launch_dir: &std::path::Path,
    shell_dir: &std::path::Path,
) -> Result<(Vec<crate::model::Source>, Vec<LocalServer>)> {
    if !std::io::stdin().is_terminal() {
        return Ok((vec![], vec![]));
    }
    let add = inquire::Confirm::new("Add browser sources? (repo pages, docs, localhost)")
        .with_default(false)
        .prompt()
        .map_err(|e| Error::Export(format!("source wizard: {e}")))?;
    if !add {
        return Ok((vec![], vec![]));
    }
    let mut sources = vec![crate::model::Source {
        id: "main".to_string(),
        kind: crate::model::SourceKind::Terminal,
        url: None,
        theme: None,
    }];
    let mut servers = Vec::new();
    let roots = BrowseRoots {
        launch_dir: launch_dir.to_path_buf(),
        shell_dir: shell_dir.to_path_buf(),
    };
    loop {
        let id = inquire::Text::new("Source ID:")
            .with_help_message("unique name (e.g. 'github', 'docs', 'preview')")
            .prompt()
            .map_err(|e| Error::Export(format!("source wizard: {e}")))?;
        let id = id.trim().to_string();
        if id.is_empty() {
            break;
        }
        let source_kind = inquire::Select::new(
            "Source:",
            vec!["URL (web page, localhost)", "Local file (PDF, PNG, HTML)"],
        )
        .prompt()
        .map_err(|e| Error::Export(format!("source wizard: {e}")))?;
        let url = if source_kind.starts_with("Local") {
            // Store the durable file:// URL — the score outlives this session, and
            // a wizard server's port dies with it (export serves/renders on its
            // own). The live server below only backs `demo focus`/`open` previews
            // during the capture itself.
            let path = pick_local_file(&roots, false)?;
            match local_server::serve_local_file(&path) {
                Ok((live_url, server)) => {
                    servers.push(server);
                    eprintln!("● live preview served on {live_url}");
                }
                Err(e) => eprintln!("demo: live preview server failed ({e}) — continuing"),
            }
            file_url_absolute(&path)?
        } else {
            let raw = inquire::Text::new("URL:")
                .with_help_message("https://github.com/..., http://localhost:3000")
                .prompt()
                .map_err(|e| Error::Export(format!("source wizard: {e}")))?;
            repair_browser_url(raw.trim(), launch_dir)?
        };
        let theme = inquire::Select::new(
            "Browser theme:",
            vec!["default (page preference)", "light", "dark"],
        )
        .prompt()
        .map_err(|e| Error::Export(format!("source wizard: {e}")))?;
        let theme = match theme {
            "light" => Some("light".to_string()),
            "dark" => Some("dark".to_string()),
            _ => None,
        };
        sources.push(crate::model::Source {
            id,
            kind: crate::model::SourceKind::Browser,
            url: Some(url),
            theme,
        });
        let more = inquire::Confirm::new("Add another source?")
            .with_default(false)
            .prompt()
            .map_err(|e| Error::Export(format!("source wizard: {e}")))?;
        if !more {
            break;
        }
    }
    Ok((sources, servers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resolution_presets_and_custom_sizes() {
        assert_eq!(parse_resolution("landscape").unwrap(), Some((1920, 1080)));
        assert_eq!(parse_resolution("Portrait").unwrap(), Some((1080, 1920)));
        assert_eq!(parse_resolution("square").unwrap(), Some((1080, 1080)));
        assert_eq!(parse_resolution("standard").unwrap(), Some((1280, 720)));
        assert_eq!(parse_resolution("1600x900").unwrap(), Some((1600, 900)));
        assert_eq!(parse_resolution("1600×900").unwrap(), Some((1600, 900)));
        assert_eq!(parse_resolution("auto").unwrap(), None);
        assert!(parse_resolution("0x100").is_err());
        assert!(parse_resolution("huge").is_err());
    }

    #[test]
    fn canvas_from_aspect_and_quality_covers_all_combos() {
        // FullHD (short side 1080).
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
        // HD (short side 720).
        assert_eq!(
            canvas_from_aspect_quality("16:9", "hd").unwrap(),
            (1280, 720)
        );
        assert_eq!(
            canvas_from_aspect_quality("9:16", "hd").unwrap(),
            (720, 1280)
        );
        assert_eq!(canvas_from_aspect_quality("4:3", "hd").unwrap(), (960, 720));
        assert_eq!(canvas_from_aspect_quality("1:1", "hd").unwrap(), (720, 720));
        // Case-insensitive.
        assert_eq!(
            canvas_from_aspect_quality("16:9", "FullHD").unwrap(),
            (1920, 1080)
        );
        assert_eq!(canvas_from_aspect_quality("1:1", "HD").unwrap(), (720, 720));
        // The legacy presets map onto the new scheme exactly.
        assert_eq!(
            canvas_from_aspect_quality("16:9", "fullhd").unwrap(),
            parse_resolution("landscape").unwrap().unwrap()
        );
        assert_eq!(
            canvas_from_aspect_quality("16:9", "hd").unwrap(),
            parse_resolution("standard").unwrap().unwrap()
        );
    }

    #[test]
    fn canvas_rejects_unknown_aspect_or_quality() {
        assert!(canvas_from_aspect_quality("3:2", "fullhd").is_err());
        assert!(canvas_from_aspect_quality("16:9", "4k").is_err());
    }

    #[test]
    fn parse_fps_accepts_15_24_30_and_rejects_others() {
        assert_eq!(parse_fps("15").unwrap(), 15);
        assert_eq!(parse_fps("24").unwrap(), 24);
        assert_eq!(parse_fps("30").unwrap(), 30);
        assert!(parse_fps("60").is_err());
        assert!(parse_fps("smooth").is_err());
    }

    #[test]
    fn ps1_text_doubles_backslashes() {
        assert_eq!(ps1_text(r"C:\Users"), r"C:\\Users");
        assert_eq!(ps1_text("no backslash"), "no backslash");
        assert_eq!(ps1_text(""), "");
    }

    #[test]
    fn default_prompt_contains_user_at_demo_and_dollar() {
        let bash_prompt = default_prompt("/bin/bash");
        assert!(bash_prompt.contains("user@demo"));
        assert!(bash_prompt.contains("$ "));

        let zsh_prompt = default_prompt("/bin/zsh");
        assert!(zsh_prompt.contains("user@demo"));
        assert!(zsh_prompt.contains("$ "));
    }

    #[test]
    fn colored_bash_produces_ansi_codes() {
        let result = colored("/bin/bash", "\\[\\e[32m\\]", "", "test");
        assert!(result.contains("32m"));
        assert!(result.contains("test"));
    }

    #[test]
    fn colored_zsh_uses_colon_syntax() {
        let result = colored("/bin/zsh", "", "%F{green}", "test");
        assert!(result.contains("%F{green}"));
        assert!(result.contains("test"));
    }

    #[test]
    fn parse_fps_valid() {
        assert_eq!(parse_fps("15").unwrap(), 15);
        assert_eq!(parse_fps("24").unwrap(), 24);
        assert_eq!(parse_fps("30").unwrap(), 30);
    }

    #[test]
    fn parse_fps_invalid() {
        assert!(parse_fps("60").is_err());
        assert!(parse_fps("abc").is_err());
        assert!(parse_fps("0").is_err());
    }

    #[test]
    fn ps1_text_various() {
        assert_eq!(ps1_text("no special chars"), "no special chars");
        assert_eq!(ps1_text("one\\slash"), "one\\\\slash");
        assert_eq!(ps1_text("\\n"), "\\\\n");
    }

    #[test]
    fn colored_bash_produces_ansi_codes_more() {
        let result = colored("/bin/bash", "\\[\\e[1;31m\\]", "", "test");
        assert!(result.contains("1;31m"));
        assert!(result.contains("test"));
        assert!(result.contains("\\[\\e[0m\\]"));
    }

    #[test]
    fn colored_zsh_uses_colon_syntax_more() {
        let result = colored("/bin/zsh", "", "%F{red}", "hello");
        assert!(result.contains("red"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn default_prompt_bash_has_dollar() {
        let p = default_prompt("/bin/bash");
        assert!(p.contains("$ "));
        assert!(p.contains("user@demo"));
    }

    #[test]
    fn default_prompt_zsh_has_dollar() {
        let p = default_prompt("/bin/zsh");
        assert!(p.contains("$ "));
        assert!(p.contains("user@demo"));
    }

    #[test]
    fn canvas_from_aspect_quality_case_insensitive_fullhd() {
        assert_eq!(
            canvas_from_aspect_quality("16:9", "FullHD").unwrap(),
            (1920, 1080)
        );
    }

    #[test]
    fn canvas_from_aspect_quality_case_insensitive_hd() {
        assert_eq!(canvas_from_aspect_quality("1:1", "HD").unwrap(), (720, 720));
    }

    #[test]
    fn canvas_from_aspect_quality_invalid_aspect() {
        assert!(canvas_from_aspect_quality("3:2", "fullhd").is_err());
    }

    #[test]
    fn canvas_from_aspect_quality_invalid_quality() {
        assert!(canvas_from_aspect_quality("16:9", "4k").is_err());
    }

    #[test]
    fn parse_resolution_auto() {
        assert_eq!(parse_resolution("auto").unwrap(), None);
    }

    #[test]
    fn parse_resolution_invalid_format() {
        assert!(parse_resolution("huge").is_err());
    }

    #[test]
    fn parse_resolution_zero_width() {
        assert!(parse_resolution("0x100").is_err());
    }

    #[test]
    fn parse_resolution_zero_height() {
        assert!(parse_resolution("100x0").is_err());
    }

    #[test]
    fn parse_fps_whitespace() {
        assert_eq!(parse_fps("  15  ").unwrap(), 15);
    }

    #[test]
    fn parse_fps_negative() {
        assert!(parse_fps("-1").is_err());
    }

    #[test]
    fn ps1_text_backslash_at_start() {
        assert_eq!(ps1_text(r"\start"), r"\\start");
    }

    #[test]
    fn ps1_text_multiple_backslashes() {
        assert_eq!(ps1_text(r"\a\b\c"), r"\\a\\b\\c");
    }

    #[test]
    fn ps1_text_backslash_in_middle() {
        assert_eq!(ps1_text("a\\b"), "a\\\\b");
    }

    #[test]
    fn ps1_text_multiple_consecutive_backslashes() {
        assert_eq!(ps1_text("\\\\a"), "\\\\\\\\a");
    }

    #[test]
    fn colored_bash_basic() {
        let result = colored("/bin/bash", "\\[\\e[32m\\]", "", "test");
        assert!(result.contains("32m"));
        assert!(result.contains("test"));
        assert!(result.contains("\\[\\e[0m\\]"));
    }

    #[test]
    fn colored_zsh_basic() {
        let result = colored("/bin/zsh", "", "%F{green}", "test");
        assert!(result.contains("%F{green}"));
        assert!(result.contains("test"));
    }

    /// Capture args with every wizard prompt left unset; individual tests set
    /// the flag whose skip-path they exercise (the prompts themselves need a
    /// TTY, so only the flag-driven paths are tested headless).
    fn args() -> CaptureArgs {
        CaptureArgs {
            rec: "demo.rec".into(),
            idle_timeout_ms: 0,
            shell: None,
            into: None,
            no_normalize: false,
            debug: false,
            output: None,
            normalized_output: "demo.toml".into(),
            no_score: false,
            prompt: None,
            keep_prompt: false,
            font: None,
            aspect: None,
            quality: None,
            fps: None,
            resolution: None,
            here: false,
        }
    }

    #[test]
    fn keep_prompt_flag_skips_the_prompt_wizard() {
        let mut a = args();
        a.keep_prompt = true;
        let (force_prompt, ps1) = choose_prompt(&a, "/bin/bash").unwrap();
        assert!(!force_prompt, "--keep-prompt must not force a PS1");
        assert_eq!(ps1, default_prompt("/bin/bash"));
    }

    #[test]
    fn explicit_prompt_flag_is_forced_verbatim() {
        let mut a = args();
        a.prompt = Some("custom$ ".to_string());
        let (force_prompt, ps1) = choose_prompt(&a, "/bin/bash").unwrap();
        assert!(force_prompt);
        assert_eq!(ps1, "custom$ ");
    }

    #[test]
    fn flag_given_values_skip_the_wizards() {
        let mut a = args();
        a.font = Some("DejaVu Sans Mono".to_string());
        a.fps = Some(30);
        a.resolution = Some("1600x900".to_string());
        assert_eq!(choose_font(&a).unwrap(), "DejaVu Sans Mono");
        assert_eq!(choose_fps(&a).unwrap(), 30);
        assert_eq!(choose_canvas(&a).unwrap(), Some((1600, 900)));
    }

    #[test]
    fn aspect_and_quality_build_the_canvas() {
        let mut a = args();
        a.aspect = Some("16:9".to_string());
        a.quality = Some("hd".to_string());
        assert_eq!(choose_canvas(&a).unwrap(), Some((1280, 720)));
    }

    #[test]
    fn default_prompt_bash_contains_dollar() {
        let p = default_prompt("/bin/bash");
        assert!(p.contains("$ "));
        assert!(p.contains("user@demo"));
    }

    #[test]
    fn default_prompt_zsh_contains_dollar() {
        let p = default_prompt("/bin/zsh");
        assert!(p.contains("$ "));
        assert!(p.contains("user@demo"));
    }
}
