//! The `demo.toml` score: the clean, declarative output of `demo normalize`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// A complete demo score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub demo: DemoMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Env>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typing: Option<Typing>,
    /// Pre-defined content sources (terminal, browser). Configured in the capture
    /// wizard; `demo focus`/`demo open` show them live.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Source>,
    pub layout: Layout,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<Step>,
}

impl Score {
    /// Load and parse a score from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        super::load_toml(path)
    }

    /// Render this score as a pretty TOML string.
    pub fn to_toml(&self) -> Result<String> {
        super::to_toml_string(self)
    }

    /// Write this score to a TOML file.
    pub fn save(&self, path: &Path) -> Result<()> {
        super::write_toml(path, self)
    }

    /// Find a pane by id.
    pub fn pane(&self, id: &str) -> Option<&Pane> {
        self.layout.panes.iter().find(|p| p.id == id)
    }

    /// Find a source by id.
    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }
}

/// `[demo]` — top-level metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DemoMeta {
    pub name: String,
    #[serde(default = "default_output_dir")]
    pub output_dir: PathBuf,
    /// Shell prompt shown in the exported demo (bash `PS1` syntax, so colours via
    /// `\[\e[..m\]` and escapes like `\w` work). Absent → the built-in default, a
    /// realistic generic Linux prompt (`user@demo:~$`, green/blue). Set it to e.g.
    /// `"$ "` for a bare prompt or `"\[\e[32m\]❯\[\e[0m\] "` for a green arrow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// How this demo is meant to be exported: the speed multiplier, in the same
    /// syntax as `--speed` (`"2x"`, `"3x"`, `"0.5x"`, or a bare number). A demo
    /// recorded at a comfortable pace is usually published faster, and without
    /// this the multiplier lives only in whoever ran the command — the published
    /// assets are the only remaining evidence of it. `--speed` still wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    /// Which formats this demo publishes (`["gif", "mp4", "svg"]`). Absent →
    /// the defaults (gif + mp4 — `svg` is opt-in and must be named). A
    /// positional target on the command line still wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<String>>,
}

fn default_output_dir() -> PathBuf {
    PathBuf::from("./dist")
}

/// `[env]` — optional sandbox setup/teardown for an isolated run.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Env {
    #[serde(default)]
    pub isolated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub teardown_script: Option<String>,
    /// Environment variables the export run needs (e.g. a token that lets a flow
    /// skip a secret prompt). Names only — values come from the runner's
    /// environment, never the score. `check` reports any that are unset.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
}

/// `[typing]` — humanized-typing parameters consumed at export time. Set by
/// `demo normalize`; controls the per-character jitter for `human_salt` steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Typing {
    /// Base speed, milliseconds per character.
    #[serde(default = "default_base_ms")]
    pub base_ms: u64,
    /// Maximum jitter added/removed per character, in milliseconds.
    #[serde(default = "default_salt_ms")]
    pub salt_ms: u64,
    /// Seed for reproducible jitter (random each run when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
}

fn default_base_ms() -> u64 {
    80
}

fn default_salt_ms() -> u64 {
    15
}

impl Default for Typing {
    fn default() -> Self {
        Typing {
            base_ms: default_base_ms(),
            salt_ms: default_salt_ms(),
            seed: None,
        }
    }
}

/// A content source the demo can show: the terminal, or a browser page.
/// Configured in the `demo capture` wizard (or authored in the score) and
/// revealed live with `demo focus <source>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// Unique identifier (e.g. "main", "google", "github").
    pub id: String,
    /// The kind of source.
    #[serde(rename = "type")]
    pub kind: SourceKind,
    /// URL for browser sources (http, https, file://).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Colour scheme to emulate for browser sources (`light`/`dark`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
}

/// The kind of content source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Terminal,
    Browser,
}

/// `[layout]` — the global canvas and its panes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// Line height as a multiple of the font size on the pixel targets (gif/mp4).
    /// Defaults to `1.2` (room for descenders like `j p q g`). Box-drawing and
    /// block glyphs are drawn to fill the cell, so they stay solid at any value.
    #[serde(default = "default_line_height")]
    pub line_height: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// Font used for rasterizing the pixel targets. One of the bundled fonts:
    /// "DejaVu Sans Mono", "JetBrains Mono", "IBM Plex Mono", "Liberation Mono",
    /// "Ubuntu Mono". Defaults to DejaVu Sans Mono.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Font size in pixels for the rasterizer (each cell is `font_size` tall).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<Pane>,
}

fn default_fps() -> u32 {
    15
}

fn default_line_height() -> f32 {
    1.2
}

/// `[[layout.panes]]` — one scene placed on the canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: PaneKind,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    // terminal-only
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<u32>,
    // browser-only. For an interactive `--view` scene this is a `viewframes:<dir>`
    // pointer to pre-recorded frames; `export` plays those back instead of
    // capturing the page via headless Chromium (see [`super::view_frames_dir`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Colour scheme to emulate when capturing the page (`light`/`dark`), so
    /// theme-aware sites render the chosen theme. `None` = the page default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Playback time (seconds) at which this pane becomes visible. `None` = from
    /// the start. A capture sets it so a reveal "opens" at the moment it fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal_at: Option<f64>,
    /// Playback time (seconds) at which this pane hides again (the next reveal
    /// switched away). `None` = stays to the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_at: Option<f64>,
    /// When true, this pane's pan (for a PDF) ignores the export speed multiplier
    /// and pans at the 1x cap. The rest of the demo still accelerates. Useful for
    /// a document that should remain readable at any export speed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignore_speed: bool,
}

/// The kind of renderer backing a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneKind {
    Terminal,
    Browser,
}

/// One step on the shared timeline, tagged by its `action`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Step {
    /// Make a layout pane the active target (typing goes to it; a browser pane is
    /// revealed/scrolled).
    Focus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
    },
    /// Type text into the focused terminal pane.
    Type {
        text: String,
        #[serde(default, skip_serializing_if = "is_false")]
        human_salt: bool,
    },
    /// Press a single named key (e.g. `enter`, `tab`, `ctrl+c`).
    Keypress { key: String },
    /// Supply a secret at a secret prompt. The value is NOT stored — `demo record`
    /// prompts for it (in memory) at the start of the run and types it here; the
    /// captured demo masks it, so nothing secret ever lands on disk. `prompt` is
    /// the label shown to the user (e.g. `Vault passphrase:`).
    Secret { prompt: String },
    /// Block until a substring appears in a pane's output.
    WaitForStdout {
        #[serde(rename = "match")]
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
    },
    /// Block until output has been quiet for `quiet_ms` (no new data), capped by
    /// `max_ms`. Useful for waiting until a TUI finishes its initial render.
    WaitForQuiet {
        quiet_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_ms: Option<u64>,
    },
    /// Block until a pattern is visible on the rendered terminal screen (parsed
    /// through a VT emulator, so escape codes are stripped). More robust than
    /// `wait_for_stdout` for TUIs that redraw frequently.
    WaitForScreen {
        #[serde(rename = "match")]
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Hold for a fixed duration.
    Wait { duration_ms: u64 },
    /// Show an on-canvas caption (a step indicator) until the next caption;
    /// an empty `text` clears it. Rendered on pixel targets (gif/mp4); ignored
    /// by the text-only `cast`/`html`.
    Caption { text: String },
    /// Scroll a (browser) pane.
    Scroll {
        direction: ScrollDirection,
        #[serde(default)]
        velocity: Velocity,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane: Option<String>,
    },
    /// End the demo.
    Terminate,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Velocity {
    #[default]
    Constant,
    EaseInOut,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact example from SPEC-0002 §5 must parse into our model.
    const SPEC_EXAMPLE: &str = r##"
[demo]
name = "univerlab-agent-experiment"
output_dir = "./dist"

[env]
isolated = true
setup_script = "mkdir -p /tmp/demo-sandbox && cd /tmp/demo-sandbox"
teardown_script = "rm -rf /tmp/demo-sandbox"

[layout]
width = 1920
height = 1080
fps = 15
background = "#0b0f14"

  [[layout.panes]]
  id = "console"
  type = "terminal"
  x = 0
  y = 0
  width = 960
  height = 1080
  font_family = "JetBrainsMono Nerd Font"
  font_size = 16

  [[layout.panes]]
  id = "preview"
  type = "browser"
  x = 960
  y = 0
  width = 960
  height = 1080
  url = "file:///tmp/demo-sandbox/output.pdf"

[[timeline]]
action = "focus"
pane = "console"

[[timeline]]
action = "type"
text = "opencode --agent --generate-report"
human_salt = true

[[timeline]]
action = "keypress"
key = "enter"

[[timeline]]
action = "wait_for_stdout"
match = "Report generated successfully."
pane = "console"

[[timeline]]
action = "focus"
pane = "preview"

[[timeline]]
action = "scroll"
direction = "down"
velocity = "constant"
duration_ms = 4000

[[timeline]]
action = "terminate"
"##;

    #[test]
    fn parses_the_spec_example() {
        let score: Score = toml::from_str(SPEC_EXAMPLE).expect("spec example parses");
        assert_eq!(score.demo.name, "univerlab-agent-experiment");
        assert_eq!(score.layout.panes.len(), 2);
        assert_eq!(score.pane("console").unwrap().kind, PaneKind::Terminal);
        assert_eq!(score.pane("preview").unwrap().kind, PaneKind::Browser);
        assert_eq!(score.timeline.len(), 7);
        assert!(matches!(score.timeline[0], Step::Focus { .. }));
        assert!(matches!(
            score.timeline[1],
            Step::Type {
                human_salt: true,
                ..
            }
        ));
        assert!(matches!(score.timeline.last(), Some(Step::Terminate)));
    }

    #[test]
    fn round_trips_through_toml() {
        let original: Score = toml::from_str(SPEC_EXAMPLE).unwrap();
        let rendered = original.to_toml().unwrap();
        let reparsed: Score = toml::from_str(&rendered).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn typing_has_sane_defaults() {
        let t = Typing::default();
        assert_eq!(t.base_ms, 80);
        assert_eq!(t.salt_ms, 15);
        assert!(t.seed.is_none());
    }

    #[test]
    fn pane_not_found_returns_none() {
        let score: Score = toml::from_str(SPEC_EXAMPLE).unwrap();
        assert!(score.pane("nonexistent").is_none());
    }

    #[test]
    fn source_not_found_returns_none() {
        let score: Score = toml::from_str(SPEC_EXAMPLE).unwrap();
        assert!(score.source("nonexistent").is_none());
        assert!(score.source("").is_none());
    }

    #[test]
    fn source_finds_matching_source() {
        // SPEC_EXAMPLE has no sources, so test with a custom score
        let toml_str = r#"
[demo]
name = "test"
output_dir = "./dist"

[[sources]]
id = "docs"
type = "browser"
url = "https://example.com"

[layout]
width = 800
height = 600
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        assert!(score.source("docs").is_some());
        assert_eq!(
            score.source("docs").unwrap().url.as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn default_output_dir_is_dist() {
        let score: Score = toml::from_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(score.demo.output_dir, std::path::PathBuf::from("./dist"));
    }

    #[test]
    fn typing_with_seed() {
        let toml_str = r#"
[demo]
name = "test"
output_dir = "./dist"

[typing]
base_ms = 50
salt_ms = 10
seed = 42

[layout]
width = 800
height = 600
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        let t = score.typing.unwrap();
        assert_eq!(t.base_ms, 50);
        assert_eq!(t.salt_ms, 10);
        assert_eq!(t.seed, Some(42));
    }

    #[test]
    fn score_save_and_load_round_trip() {
        let score: Score = toml::from_str(SPEC_EXAMPLE).unwrap();
        let dir = std::env::temp_dir().join(format!("demo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.toml");
        score.save(&path).unwrap();
        let loaded = Score::load(&path).unwrap();
        assert_eq!(score, loaded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn score_load_error_on_missing_file() {
        let dir = std::env::temp_dir().join(format!("demo-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nope.toml");
        let err = Score::load(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("nope.toml"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn layout_defaults() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 800
height = 600
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        assert_eq!(score.layout.fps, 15);
        assert!(score.layout.background.is_none());
    }

    #[test]
    fn pane_defaults() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 400
  height = 300
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        let pane = &score.layout.panes[0];
        assert!(pane.font_family.is_none());
        assert!(pane.font_size.is_none());
        assert!(pane.url.is_none());
        assert!(pane.reveal_at.is_none());
        assert!(pane.hide_at.is_none());
    }

    #[test]
    fn step_focus_without_pane() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 600
[[timeline]]
action = "focus"
[[timeline]]
action = "terminate"
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        assert!(matches!(&score.timeline[0], Step::Focus { pane: None }));
    }

    #[test]
    fn step_wait_with_duration() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 600
[[timeline]]
action = "wait"
duration_ms = 1000
[[timeline]]
action = "terminate"
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        assert!(matches!(
            &score.timeline[0],
            Step::Wait { duration_ms: 1000 }
        ));
    }

    #[test]
    fn step_wait_for_stdout_with_match() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 600
[[timeline]]
action = "focus"
pane = "c"
[[timeline]]
action = "wait_for_stdout"
match = "ready"
pane = "c"
[[timeline]]
action = "terminate"
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        assert!(matches!(&score.timeline[1], Step::WaitForStdout { .. }));
    }

    #[test]
    fn step_secret() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 600
[[timeline]]
action = "focus"
pane = "c"
[[timeline]]
action = "secret"
prompt = "Password:"
[[timeline]]
action = "terminate"
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        assert!(matches!(
            &score.timeline[1],
            Step::Secret { prompt } if prompt == "Password:"
        ));
    }

    #[test]
    fn env_requires() {
        let toml_str = r#"
[demo]
name = "test"
[env]
requires = ["TOKEN", "API_KEY"]
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 600
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        let env = score.env.unwrap();
        assert_eq!(env.requires, vec!["TOKEN", "API_KEY"]);
        assert!(!env.isolated);
    }

    #[test]
    fn env_isolated_with_scripts() {
        let toml_str = r#"
[demo]
name = "test"
[env]
isolated = true
setup_script = "setup.sh"
teardown_script = "teardown.sh"
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 600
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        let env = score.env.unwrap();
        assert!(env.isolated);
        assert_eq!(env.setup_script.as_deref(), Some("setup.sh"));
        assert_eq!(env.teardown_script.as_deref(), Some("teardown.sh"));
    }

    #[test]
    fn custom_prompt() {
        let toml_str = r#"
[demo]
name = "test"
prompt = "$ "
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 600
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        assert_eq!(score.demo.prompt.as_deref(), Some("$ "));
    }

    #[test]
    fn pane_full_config() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 1920
height = 1080
  [[layout.panes]]
  id = "term"
  type = "terminal"
  x = 10
  y = 20
  width = 960
  height = 540
  font_family = "FiraCode"
  font_size = 14
  [[layout.panes]]
  id = "web"
  type = "browser"
  x = 970
  y = 20
  width = 960
  height = 540
  url = "https://example.com"
  theme = "dark"
  reveal_at = 1.0
  hide_at = 5.0
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        let term = score.pane("term").unwrap();
        assert_eq!(term.font_family.as_deref(), Some("FiraCode"));
        assert_eq!(term.font_size, Some(14));
        let web = score.pane("web").unwrap();
        assert_eq!(web.url.as_deref(), Some("https://example.com"));
        assert_eq!(web.theme.as_deref(), Some("dark"));
        assert_eq!(web.reveal_at, Some(1.0));
        assert_eq!(web.hide_at, Some(5.0));
    }

    #[test]
    fn velocity_ease_in_out_round_trips() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "b"
  type = "browser"
  x = 0
  y = 0
  width = 800
  height = 600
  url = "file:///x.pdf"
[[timeline]]
action = "scroll"
direction = "down"
velocity = "ease_in_out"
duration_ms = 1000
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        let rendered = score.to_toml().unwrap();
        let reparsed: Score = toml::from_str(&rendered).unwrap();
        assert_eq!(score, reparsed);
        if let Step::Scroll { velocity, .. } = &reparsed.timeline[0] {
            assert_eq!(*velocity, Velocity::EaseInOut);
        } else {
            panic!("expected Scroll step");
        }
    }

    /// A score with ignore_speed = true round-trips through serialization.
    #[test]
    fn ignore_speed_round_trips_through_serialization() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "b"
  type = "browser"
  x = 0
  y = 0
  width = 800
  height = 600
  url = "file:///x.pdf"
  ignore_speed = true
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        assert!(score.layout.panes[0].ignore_speed);
        let rendered = score.to_toml().unwrap();
        assert!(rendered.contains("ignore_speed = true"));
        let reparsed: Score = toml::from_str(&rendered).unwrap();
        assert_eq!(score, reparsed);
    }

    /// ignore_speed defaults to false and is absent from serialization when false.
    #[test]
    fn ignore_speed_defaults_to_false_and_is_absent_when_false() {
        let toml_str = r#"
[demo]
name = "test"
[layout]
width = 800
height = 600
  [[layout.panes]]
  id = "b"
  type = "browser"
  x = 0
  y = 0
  width = 800
  height = 600
  url = "file:///x.pdf"
"#;
        let score: Score = toml::from_str(toml_str).unwrap();
        assert!(!score.layout.panes[0].ignore_speed);
        let rendered = score.to_toml().unwrap();
        assert!(!rendered.contains("ignore_speed"));
    }
}
