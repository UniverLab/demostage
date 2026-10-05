//! Command-line interface definition (clap).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// `demo` — the DemoStage command-line tool.
#[derive(Debug, Parser)]
#[command(
    name = "demo",
    version,
    about = "Demos as Code — capture, record and export terminal demos"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Capture a live interactive session, then normalize it to a demo score.
    Capture(CaptureArgs),
    /// Execute a demo score in a PTY to (re)produce a recording (a .rec).
    Record(RecordArgs),
    /// Render a recording to one or more formats (playback — never executes).
    Export(ExportArgs),
    /// Check the environment for browser/video dependencies and report fixes.
    Doctor(DoctorArgs),
    /// Check for and install a newer stable release. Always asks first
    /// (default: no) and never runs on its own; refuses cargo installs.
    /// Exit codes: 0 = up to date, 1 = update available (`--check`),
    /// 2 = the check could not be completed.
    ///
    /// Exit 2 covers every failed release check — network, DNS, TLS,
    /// HTTP ≥ 400 or an unparsable response — printed as one line on stderr,
    /// for `--check` and plain `demo update` alike. Exit 0 also covers an
    /// installed update, a declined prompt and a cargo-managed install;
    /// failures after a successful check (download, checksum, permissions)
    /// exit 1.
    Update(UpdateArgs),
    /// Interactively edit timing/wait steps in a demo score.
    Edit(EditArgs),
    /// End the in-progress capture. Run from inside it, or from another
    /// terminal in the same directory.
    Stop,
    /// Reveal an ad-hoc browser page (a URL not pre-configured as a source) in the
    /// running capture. Run from inside it or another terminal in the same
    /// directory. With no URL on a terminal it runs a wizard. For pre-configured
    /// sources use `demo focus`.
    Open(OpenArgs),
    /// Switch the live capture's view to one or two sources (`demo focus main`,
    /// `demo focus main docs`). Run from inside the capture or another terminal in
    /// the same directory. With no source on a terminal it runs a wizard.
    Focus(FocusArgs),
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Try to install what's missing (Linux/apt: a non-snap Google Chrome).
    /// Without it, `doctor` only reports and prints the commands to run.
    #[arg(long)]
    pub fix: bool,

    /// On WSL, route human-facing links (http/https) to the Windows browser
    /// instead of the installed Linux one. Reachable without `--fix`.
    #[arg(long)]
    pub route_browser: bool,
}

#[derive(Debug, Args)]
pub struct UpdateArgs {
    /// Only check whether an update exists: exit 0 = up to date, exit 1 =
    /// update available, exit 2 = the check could not be completed. Downloads
    /// nothing and changes nothing.
    #[arg(long)]
    pub check: bool,

    /// Skip the confirmation prompt (install without asking).
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    /// The demo score to edit interactively.
    #[arg(default_value = "demo.toml")]
    pub input: PathBuf,
}

#[derive(Debug, Args)]
pub struct CaptureArgs {
    /// Where to write the recording (`.rec`) — the one artifact a capture needs,
    /// the thing `demo export` plays back. The raw macro and the editable score
    /// are optional extras (`--raw` / `--score`).
    #[arg(short = 'r', long, default_value = "demo.rec")]
    pub rec: PathBuf,

    /// Auto-stop after this many milliseconds with no terminal output
    /// (0 disables — stop the capture yourself with `demo stop`).
    #[arg(long, default_value_t = 0)]
    pub idle_timeout_ms: u64,

    /// Shell/command to run inside the capture (defaults to `$SHELL`).
    #[arg(long)]
    pub shell: Option<String>,

    /// Capture into a prepared stage: the captured terminal flow is spliced into
    /// this stage's timeline (writes the resulting score to `--score`, default
    /// `demo.toml`).
    #[arg(long)]
    pub into: Option<PathBuf>,

    /// Skip the normalize pass — don't derive a score (the recording is faithful
    /// either way).
    #[arg(long)]
    pub no_normalize: bool,

    /// Write a timestamped diagnostic log of every input/output chunk (with hex)
    /// next to the recording (`<rec>.debug.log`), for debugging captures.
    #[arg(long)]
    pub debug: bool,

    /// Also write the low-level raw capture macro here (an intermediate, for
    /// inspection/debugging). Omitted by default.
    #[arg(short = 'o', long = "raw", value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Where to write the normalized demo score — the editable "demo as code"
    /// you can re-run with `demo record`. Defaults to `demo.toml`; `--no-score`
    /// skips it if you only want the recording.
    #[arg(
        short = 'O',
        long = "score",
        default_value = "demo.toml",
        value_name = "FILE"
    )]
    pub normalized_output: PathBuf,

    /// Don't write the `demo.toml` score — leave only the recording.
    #[arg(long, conflicts_with = "normalized_output")]
    pub no_score: bool,

    /// Force a clean PS1 in the captured shell (default: the built-in realistic
    /// prompt), so the demo shows a tidy prompt instead of your real one. Pass a
    /// value to customize.
    #[arg(long, value_name = "PS1")]
    pub prompt: Option<String>,

    /// Keep your shell's real prompt during capture (don't force a clean one).
    #[arg(long, conflicts_with = "prompt")]
    pub keep_prompt: bool,

    /// Font for the exported demo (DejaVu Sans Mono, JetBrains Mono, IBM Plex
    /// Mono, Liberation Mono, Ubuntu Mono). Omit for a wizard prompt.
    #[arg(long)]
    pub font: Option<String>,

    /// Export canvas aspect ratio: `16:9`, `9:16`, `4:3`, or `1:1`. Combined
    /// with `--quality` to pick the pixel resolution (e.g. `16:9` + `fullhd` →
    /// 1920×1080). Omit for a wizard prompt. Use `--resolution` instead for an
    /// explicit `WxH` or `auto`.
    #[arg(long, conflicts_with = "resolution")]
    pub aspect: Option<String>,

    /// Quality tier for the canvas: `fullhd` (short side 1080) or `hd` (short
    /// side 720). Defaults to `fullhd` when an `--aspect` is given.
    #[arg(long, conflicts_with = "resolution")]
    pub quality: Option<String>,

    /// Frame rate of the exported gif/mp4: `15`, `24`, or `30`. Defaults to
    /// `15`. Omit for a wizard prompt.
    #[arg(long)]
    pub fps: Option<u32>,

    /// Export canvas as an explicit resolution: a `WxH` pair (e.g.
    /// `1600x900`), `auto` (derive from the terminal size — the default), or a
    /// legacy preset name (`landscape`, `portrait`, `square`, `standard`).
    /// Conflicts with `--aspect`/`--quality`. Omit for a wizard prompt.
    #[arg(long)]
    pub resolution: Option<String>,

    /// Start the captured shell in the current directory instead of an isolated
    /// temporary directory.
    #[arg(long)]
    pub here: bool,
}

#[derive(Debug, Args)]
pub struct RecordArgs {
    /// The demo score to execute.
    #[arg(default_value = "demo.toml")]
    pub input: PathBuf,

    /// Where to write the recording (a `.rec` that `export` plays back).
    #[arg(short, long, default_value = "demo.rec")]
    pub output: PathBuf,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Formats to build, comma-separated — `gif`, `mp4`, `svg`, or `all` (e.g.
    /// `gif,mp4` or `gif,svg`). `all` means gif + mp4 — `svg` is opt-in and
    /// only built when asked for by name. Omit it to build every default format.
    #[arg(value_parser = parse_targets)]
    pub targets: Option<TargetList>,

    /// The recording to render: a `.rec` from `demo record`, or a raw capture
    /// (`macro.raw.toml`) to render the live session directly.
    #[arg(default_value = "demo.rec")]
    pub input: PathBuf,

    /// Speed multiplier applied to typing and waits — e.g. `2x`, `3x`, `0.5x`
    /// (a bare number works too). `1x` keeps the recorded pace. Omit it to use
    /// the score's `[demo] speed`, and `1x` when the score doesn't set one.
    #[arg(long, value_parser = parse_speed)]
    pub speed: Option<f64>,

    /// Render a **faithful capture** as-is. By default `export` refuses one (its
    /// typing/idle aren't humanized) and points you at `demo record`; pass this to
    /// render the live capture directly anyway — needed for interactive tools and
    /// side-effecting demos that can't be re-executed.
    #[arg(long)]
    pub force: bool,

    /// Canvas aspect ratio: `16:9`, `9:16`, `4:3`, or `1:1`. Combined with
    /// `--quality` to compute the pixel resolution (e.g. `16:9` + `fullhd` →
    /// 1920×1080). Overrides the capture-time resolution.
    #[arg(long, conflicts_with = "resolution")]
    pub aspect: Option<String>,

    /// Canvas quality tier: `fullhd` (1080p) or `hd` (720p). The short side of
    /// the canvas; the long side scales by the aspect ratio.
    #[arg(long, conflicts_with = "resolution")]
    pub quality: Option<String>,

    /// Export canvas as an explicit resolution: a `WxH` pair (e.g.
    /// `1920x1080`). Overrides the capture-time resolution.
    #[arg(long, conflicts_with_all = ["aspect", "quality"])]
    pub resolution: Option<String>,

    /// For `svg`: without it, a terminal-only demo exports as an animated SVG
    /// of the whole timeline; with it (e.g. `--at 12.5`), the SVG is a static
    /// poster of that frame, written to `dist/<name>-at-12.5.svg` (defaults to
    /// the last frame; values past the end clamp to it). Multi-pane demos
    /// refuse the animated path — use gif/mp4, or `--at` for a poster.
    /// Ignored by gif/mp4, which render the whole timeline.
    #[arg(long, value_name = "SECONDS", value_parser = parse_at)]
    pub at: Option<f64>,
}

/// One or more export targets parsed from a comma-separated token.
#[derive(Debug, Clone)]
pub struct TargetList(pub Vec<Target>);

/// Every format `demo export` builds when no target is given. `svg` is
/// opt-in — it is deliberately absent here and must be asked for by name.
pub fn all_targets() -> Vec<Target> {
    vec![Target::Gif, Target::Mp4]
}

/// Parse `gif,mp4,svg` (or `all`) into a deduplicated list of targets.
fn parse_targets(s: &str) -> Result<TargetList, String> {
    let mut out: Vec<Target> = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if p.eq_ignore_ascii_case("all") {
            return Ok(TargetList(all_targets()));
        }
        let t = <Target as ValueEnum>::from_str(p, true)
            .map_err(|_| format!("invalid format '{p}' (expected gif, mp4, svg or all)"))?;
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.is_empty() {
        return Err("no export formats given (try gif, mp4, svg or all)".to_string());
    }
    Ok(TargetList(out))
}

/// Parse a speed multiplier like `2x`, `3x`, `0.5x` or a bare `2`.
pub fn parse_speed(s: &str) -> Result<f64, String> {
    let trimmed = s.trim();
    let value = trimmed.strip_suffix(['x', 'X']).unwrap_or(trimmed);
    let v: f64 = value
        .parse()
        .map_err(|_| format!("invalid speed '{s}' (try 2x, 3x or 0.5x)"))?;
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err(format!("speed must be a positive number (got '{s}')"))
    }
}

/// Parse an `--at` seconds value like `12.5`. Both failure paths name the
/// flag, so the message is actionable wherever it surfaces.
pub fn parse_at(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .trim()
        .parse()
        .map_err(|_| format!("invalid --at value '{s}' (expected seconds, e.g. 12.5)"))?;
    if v.is_finite() && v >= 0.0 {
        Ok(v)
    } else {
        Err(format!(
            "--at must be a non-negative number of seconds (got '{s}')"
        ))
    }
}

/// Supported export targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Target {
    Gif,
    Mp4,
    /// The animated vector target (terminal-only) — opt-in, never part of `all`.
    /// Without `--at` it exports the whole timeline as an animated SVG; with
    /// `--at <seconds>` it draws a static poster of that frame instead.
    Svg,
}

#[derive(Debug, Args)]
pub struct OpenArgs {
    /// URL (or local file) to reveal. Omit on a terminal to run the wizard.
    pub url: Option<String>,

    /// Force the wizard even when a URL is given.
    #[arg(long)]
    pub wizard: bool,

    /// Place beside the terminal (split) instead of replacing the whole frame.
    #[arg(long)]
    pub split: bool,

    /// `replace` (full screen) or `split` (beside terminal).
    #[arg(long, value_enum, default_value = "replace")]
    pub mode: OpenMode,

    /// Reveal only when a line containing this substring appears in the output.
    #[arg(long)]
    pub when: Option<String>,

    /// Reveal when the current foreground command finishes (shell returns to prompt).
    #[arg(long)]
    pub after: bool,

    /// Hold the scene on screen for this many milliseconds (static, not scrolling).
    #[arg(long)]
    pub hold: Option<u64>,

    /// Scroll the page down while the scene is on screen.
    #[arg(long)]
    pub scroll: bool,

    /// Emulated colour scheme: `light` or `dark`. Default = page preference.
    #[arg(long)]
    pub theme: Option<String>,

    /// Open a real (headed) browser you drive yourself; records until you close it.
    #[arg(long)]
    pub view: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OpenMode {
    Replace,
    Split,
}

#[derive(Debug, Args)]
pub struct FocusArgs {
    /// Source(s) to show: `main` (the terminal) or a browser source id (e.g.
    /// `docs`). One fills the canvas; two are shown side by side. Omit on a
    /// terminal for a wizard that lists the capture's sources.
    #[arg(value_name = "SOURCE", num_args = 0..=2)]
    pub sources: Vec<String>,

    /// Stack the two sources (top/bottom) instead of side by side.
    #[arg(long, conflicts_with = "horizontal")]
    pub vertical: bool,

    /// Place the two sources side by side (the default).
    #[arg(long)]
    pub horizontal: bool,

    /// Hold the view on screen for this many seconds, then move on.
    #[arg(long)]
    pub hold: Option<f64>,

    /// Scroll any browser pane down while it's on screen.
    #[arg(long)]
    pub scroll: bool,

    /// Reveal only when a line matches this cue — a substring, or a regex with a
    /// `re:` prefix (e.g. `--when 're:built .*\.pdf'`).
    #[arg(long)]
    pub when: Option<String>,

    /// Reveal when the current foreground command finishes (back at the prompt).
    #[arg(long)]
    pub after: bool,

    /// Emulated colour scheme for browser panes: `light` or `dark`.
    #[arg(long)]
    pub theme: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_help_documents_the_three_exit_codes() {
        use clap::CommandFactory;
        let mut command = crate::cli::Cli::command();
        let plain = command.clone().render_help().to_string();
        let mut update = command.find_subcommand_mut("update").unwrap().clone();
        let detailed = update.render_long_help().to_string();
        let flat = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let (plain, detailed) = (flat(&plain), flat(&detailed));
        for phrase in [
            "0 = up to date",
            "1 = update available",
            "2 = the check could not be completed",
        ] {
            assert!(
                plain.contains(phrase),
                "demo --help missing {phrase:?}:\n{plain}"
            );
            assert!(
                detailed.contains(&format!("exit {phrase}")),
                "demo update --help missing {phrase:?}:\n{detailed}"
            );
        }
    }

    #[test]
    fn parse_targets_gif_only() {
        let result = parse_targets("gif").unwrap();
        assert_eq!(result.0.len(), 1);
        assert!(result.0.contains(&Target::Gif));
    }

    #[test]
    fn parse_targets_mp4_only() {
        let result = parse_targets("mp4").unwrap();
        assert_eq!(result.0.len(), 1);
        assert!(result.0.contains(&Target::Mp4));
    }

    #[test]
    fn parse_targets_both() {
        let result = parse_targets("gif,mp4").unwrap();
        assert_eq!(result.0.len(), 2);
    }

    #[test]
    fn parse_targets_all() {
        let result = parse_targets("all").unwrap();
        assert_eq!(result.0.len(), 2);
    }

    #[test]
    fn parse_targets_all_case_insensitive() {
        let result = parse_targets("ALL").unwrap();
        assert_eq!(result.0.len(), 2);
    }

    #[test]
    fn parse_targets_deduplicates() {
        let result = parse_targets("gif,gif,mp4").unwrap();
        assert_eq!(result.0.len(), 2);
    }

    #[test]
    fn parse_targets_empty_string() {
        assert!(parse_targets("").is_err());
    }

    #[test]
    fn parse_targets_invalid_format() {
        assert!(parse_targets("invalid").is_err());
    }

    #[test]
    fn parse_targets_with_spaces() {
        let result = parse_targets("gif , mp4").unwrap();
        assert_eq!(result.0.len(), 2);
    }

    #[test]
    fn parse_targets_leading_trailing_comma() {
        let result = parse_targets(",gif,mp4,").unwrap();
        assert_eq!(result.0.len(), 2);
    }

    #[test]
    fn parse_targets_accepts_svg() {
        let result = parse_targets("svg").unwrap();
        assert_eq!(result.0, vec![Target::Svg]);
    }

    #[test]
    fn parse_targets_mixes_svg_with_gif_and_mp4() {
        let result = parse_targets("gif,svg,mp4").unwrap();
        assert_eq!(result.0, vec![Target::Gif, Target::Svg, Target::Mp4]);
        // And `all` still means gif + mp4, even when named beside svg.
        assert_eq!(
            parse_targets("svg,all").unwrap().0,
            vec![Target::Gif, Target::Mp4]
        );
    }

    #[test]
    fn all_targets_stays_gif_and_mp4_only() {
        let targets = all_targets();
        assert_eq!(targets, vec![Target::Gif, Target::Mp4]);
        assert!(
            !targets.contains(&Target::Svg),
            "svg is opt-in and must never join `all`"
        );
    }

    #[test]
    fn the_at_parser_errors_name_the_flag() {
        let not_a_number = parse_at("soon").unwrap_err();
        assert!(
            not_a_number.contains("--at"),
            "unhelpful message: {not_a_number}"
        );
        let negative = parse_at("-3").unwrap_err();
        assert!(negative.contains("--at"), "unhelpful message: {negative}");
        let not_finite = parse_at("nan").unwrap_err();
        assert!(
            not_finite.contains("--at"),
            "unhelpful message: {not_finite}"
        );
        let infinite = parse_at("inf").unwrap_err();
        assert!(infinite.contains("--at"), "unhelpful message: {infinite}");
    }

    #[test]
    fn parse_at_accepts_bare_seconds() {
        assert_eq!(parse_at("0").unwrap(), 0.0);
        assert_eq!(parse_at("12.5").unwrap(), 12.5);
        assert_eq!(parse_at(" 3 ").unwrap(), 3.0);
    }

    #[test]
    fn parse_speed_with_x_suffix() {
        let result = parse_speed("2x").unwrap();
        assert!((result - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_speed_with_x_suffix_uppercase() {
        let result = parse_speed("3X").unwrap();
        assert!((result - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_speed_bare_number() {
        let result = parse_speed("0.5").unwrap();
        assert!((result - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_speed_fraction() {
        let result = parse_speed("1.5x").unwrap();
        assert!((result - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_speed_zero_is_error() {
        assert!(parse_speed("0").is_err());
    }

    #[test]
    fn parse_speed_negative_is_error() {
        assert!(parse_speed("-1").is_err());
    }

    #[test]
    fn parse_speed_non_number_is_error() {
        assert!(parse_speed("abc").is_err());
    }

    #[test]
    fn parse_speed_empty_is_error() {
        assert!(parse_speed("").is_err());
    }

    #[test]
    fn all_targets_returns_gif_and_mp4() {
        let targets = all_targets();
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&Target::Gif));
        assert!(targets.contains(&Target::Mp4));
    }

    #[test]
    fn parse_speed_with_whitespace() {
        let result = parse_speed(" 2x ").unwrap();
        assert!((result - 2.0).abs() < f64::EPSILON);
    }
}
