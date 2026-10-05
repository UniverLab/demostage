//! Static validation of a [`Score`] — the engine behind CLI checks and the
//! pre-capture gate.
//!
//! Pure and dependency-free: returns the list of problems found (empty = valid)
//! so it is trivially unit-testable and reusable by `export`.

use crate::model::{PaneKind, Score, Step};

/// Validate a score, returning a human-readable problem for each issue found.
/// An empty result means the score is valid.
pub fn validate(score: &Score) -> Vec<String> {
    let mut problems = Vec::new();

    // ── Required environment (provided at export time, not stored) ────────
    if let Some(env) = &score.env {
        for var in &env.requires {
            if std::env::var(var).is_err() {
                problems.push(format!(
                    "env: required variable ${var} is not set (export reads it from your environment)"
                ));
            }
        }
    }

    let layout = &score.layout;

    // ── Canvas ────────────────────────────────────────────────────────────
    if layout.width == 0 || layout.height == 0 {
        problems.push(format!(
            "layout: width and height must be > 0 (got {}x{})",
            layout.width, layout.height
        ));
    }
    if layout.fps == 0 {
        problems.push("layout: fps must be > 0".to_string());
    }
    if layout.panes.is_empty() {
        problems.push("layout: at least one pane is required".to_string());
    }

    // ── Panes ─────────────────────────────────────────────────────────────
    let mut seen = std::collections::HashSet::new();
    for pane in &layout.panes {
        let id = &pane.id;
        if !seen.insert(id.as_str()) {
            problems.push(format!("pane '{id}': duplicate id"));
        }
        if pane.width == 0 || pane.height == 0 {
            problems.push(format!("pane '{id}': width and height must be > 0"));
        }
        // Use u64 math to avoid overflow on absurd values.
        let (right, bottom) = (
            pane.x as u64 + pane.width as u64,
            pane.y as u64 + pane.height as u64,
        );
        if right > layout.width as u64 || bottom > layout.height as u64 {
            problems.push(format!(
                "pane '{id}': extends beyond the {}x{} canvas (x={}, y={}, w={}, h={})",
                layout.width, layout.height, pane.x, pane.y, pane.width, pane.height
            ));
        }
        if pane.kind == PaneKind::Browser && pane.url.is_none() {
            problems.push(format!("pane '{id}': browser panes require a `url`"));
        }
    }

    // ── Timeline (focus state machine + pane references) ──────────────────
    let kind_of = |id: &str| layout.panes.iter().find(|p| p.id == id).map(|p| p.kind);
    let mut focused: Option<&str> = None;
    for (i, step) in score.timeline.iter().enumerate() {
        let at = format!("timeline[{i}]");
        match step {
            Step::Focus { pane } => {
                if let Some(pane_id) = pane {
                    match kind_of(pane_id) {
                        None => problems
                            .push(format!("{at}: focus references unknown pane '{pane_id}'")),
                        Some(_) => focused = Some(pane_id.as_str()),
                    }
                } else {
                    problems.push(format!("{at}: focus must reference a 'pane'"));
                }
            }
            Step::Type { .. } | Step::Keypress { .. } | Step::Secret { .. } => match focused {
                None => problems.push(format!(
                    "{at}: input with no focused pane (add a `focus` first)"
                )),
                Some(id) if kind_of(id) != Some(PaneKind::Terminal) => problems.push(format!(
                    "{at}: input requires a focused terminal pane ('{id}' is a browser)"
                )),
                Some(_) => {}
            },
            Step::WaitForStdout { pane, .. } => {
                check_target(
                    &mut problems,
                    &at,
                    "wait_for_stdout",
                    pane.as_deref().or(focused),
                    &kind_of,
                    PaneKind::Terminal,
                );
            }
            Step::Scroll { pane, .. } => {
                check_target(
                    &mut problems,
                    &at,
                    "scroll",
                    pane.as_deref().or(focused),
                    &kind_of,
                    PaneKind::Browser,
                );
            }
            Step::Wait { .. }
            | Step::WaitForQuiet { .. }
            | Step::WaitForScreen { .. }
            | Step::Caption { .. }
            | Step::Terminate => {}
        }
    }

    problems
}

/// Shared check for steps that target a pane of an expected kind.
fn check_target(
    problems: &mut Vec<String>,
    at: &str,
    action: &str,
    target: Option<&str>,
    kind_of: &impl Fn(&str) -> Option<PaneKind>,
    want: PaneKind,
) {
    match target {
        None => problems.push(format!(
            "{at}: {action} needs a pane (none given, none focused)"
        )),
        Some(id) => match kind_of(id) {
            None => problems.push(format!("{at}: {action} references unknown pane '{id}'")),
            Some(k) if k != want => {
                problems.push(format!(
                    "{at}: {action} target '{id}' must be a {want:?} pane"
                ));
            }
            Some(_) => {}
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score_from(toml_str: &str) -> Score {
        toml::from_str(toml_str).expect("test score parses")
    }

    const VALID: &str = r##"
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
pane = "c"
[[timeline]]
action = "type"
text = "ls"
[[timeline]]
action = "keypress"
key = "enter"
[[timeline]]
action = "focus"
pane = "p"
[[timeline]]
action = "scroll"
direction = "down"
duration_ms = 1000
[[timeline]]
action = "terminate"
"##;

    #[test]
    fn valid_score_has_no_problems() {
        assert!(validate(&score_from(VALID)).is_empty());
    }

    #[test]
    fn flags_pane_outside_canvas() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
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
        assert!(validate(&s).iter().any(|p| p.contains("beyond the")));
    }

    #[test]
    fn flags_browser_without_url() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "p"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("require a `url`")));
    }

    #[test]
    fn flags_input_without_focus() {
        let s = score_from(
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
[[timeline]]
action = "type"
text = "ls"
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("no focused pane")));
    }

    #[test]
    fn flags_scroll_on_terminal() {
        let s = score_from(
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
[[timeline]]
action = "focus"
pane = "c"
[[timeline]]
action = "scroll"
direction = "down"
duration_ms = 1000
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("must be a Browser")));
    }

    #[test]
    fn flags_missing_required_env() {
        let s = score_from(
            r#"
[demo]
name = "t"
[env]
requires = ["DEMOSTAGE_DEFINITELY_UNSET_VAR_XYZ"]
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
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("DEMOSTAGE_DEFINITELY_UNSET")));
    }

    #[test]
    fn flags_duplicate_pane_id() {
        let s = score_from(
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
  id = "c"
  type = "terminal"
  x = 100
  y = 0
  width = 100
  height = 100
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("duplicate id")));
    }

    // ── Multiple overlapping panes ────────────────────────────────────────

    #[test]
    fn overlapping_panes_are_valid() {
        let s = score_from(
            r##"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "a"
  type = "terminal"
  x = 0
  y = 0
  width = 80
  height = 80
  [[layout.panes]]
  id = "b"
  type = "browser"
  x = 20
  y = 20
  width = 80
  height = 80
  url = "file:///x.pdf"
[[timeline]]
action = "focus"
pane = "a"
[[timeline]]
action = "type"
text = "hello"
[[timeline]]
action = "terminate"
"##,
        );
        assert!(validate(&s).is_empty());
    }

    #[test]
    fn fully_overlapping_panes_are_valid() {
        let s = score_from(
            r##"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "a"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "b"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
[[timeline]]
action = "focus"
pane = "a"
[[timeline]]
action = "type"
text = "hello"
[[timeline]]
action = "terminate"
"##,
        );
        assert!(validate(&s).is_empty());
    }

    // ── Scroll on terminal pane (explicit pane target) ────────────────────

    #[test]
    fn flags_scroll_on_terminal_via_explicit_pane() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "t1"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
[[timeline]]
action = "scroll"
pane = "t1"
direction = "up"
duration_ms = 500
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("must be a Browser")));
    }

    #[test]
    fn flags_scroll_on_unknown_pane() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "t1"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
[[timeline]]
action = "scroll"
pane = "ghost"
direction = "down"
duration_ms = 1000
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("unknown pane")));
    }

    #[test]
    fn flags_scroll_with_no_target_and_no_focus() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "b1"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "file:///x.pdf"
[[timeline]]
action = "scroll"
direction = "down"
duration_ms = 1000
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("scroll needs a pane")));
    }

    // ── Missing required env vars ─────────────────────────────────────────

    #[test]
    fn flags_multiple_missing_env_vars() {
        let s = score_from(
            r#"
[demo]
name = "t"
[env]
requires = ["DEMOSTAGE_FAKE_A", "DEMOSTAGE_FAKE_B"]
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
        let problems = validate(&s);
        assert!(problems.iter().any(|p| p.contains("DEMOSTAGE_FAKE_A")));
        assert!(problems.iter().any(|p| p.contains("DEMOSTAGE_FAKE_B")));
    }

    #[test]
    fn no_env_section_is_valid() {
        let s = score_from(
            r##"
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
[[timeline]]
action = "focus"
pane = "c"
[[timeline]]
action = "type"
text = "ls"
[[timeline]]
action = "terminate"
"##,
        );
        assert!(validate(&s).is_empty());
    }

    // ── Browser pane without URL ──────────────────────────────────────────

    #[test]
    fn flags_multiple_browsers_without_url() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 200
height = 100
  [[layout.panes]]
  id = "b1"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  [[layout.panes]]
  id = "b2"
  type = "browser"
  x = 100
  y = 0
  width = 100
  height = 100
"#,
        );
        let problems = validate(&s);
        assert!(problems
            .iter()
            .any(|p| p.contains("b1") && p.contains("url")));
        assert!(problems
            .iter()
            .any(|p| p.contains("b2") && p.contains("url")));
    }

    // ── Input pane without focus ──────────────────────────────────────────

    #[test]
    fn flags_keypress_without_focus() {
        let s = score_from(
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
[[timeline]]
action = "keypress"
key = "enter"
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("no focused pane")));
    }

    #[test]
    fn flags_secret_without_focus() {
        let s = score_from(
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
[[timeline]]
action = "secret"
prompt = "Password:"
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("no focused pane")));
    }

    #[test]
    fn flags_type_on_browser_pane() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "b1"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "file:///x.pdf"
[[timeline]]
action = "focus"
pane = "b1"
[[timeline]]
action = "type"
text = "hello"
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("requires a focused terminal")));
    }

    #[test]
    fn flags_keypress_on_browser_pane() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "b1"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "file:///x.pdf"
[[timeline]]
action = "focus"
pane = "b1"
[[timeline]]
action = "keypress"
key = "enter"
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("requires a focused terminal")));
    }

    #[test]
    fn flags_secret_on_browser_pane() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "b1"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "file:///x.pdf"
[[timeline]]
action = "focus"
pane = "b1"
[[timeline]]
action = "secret"
prompt = "Pass:"
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("requires a focused terminal")));
    }

    // ── Focus edge cases ──────────────────────────────────────────────────

    #[test]
    fn flags_focus_with_none_pane() {
        let s = score_from(
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
[[timeline]]
action = "focus"
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("focus must reference a 'pane'")));
    }

    #[test]
    fn flags_focus_on_unknown_pane() {
        let s = score_from(
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
[[timeline]]
action = "focus"
pane = "nonexistent"
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("unknown pane")));
    }

    // ── Canvas bounds checking edge cases ─────────────────────────────────

    #[test]
    fn flags_zero_width_canvas() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 0
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 0
  height = 100
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("width and height must be > 0")));
    }

    #[test]
    fn flags_zero_height_canvas() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 0
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 0
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("width and height must be > 0")));
    }

    #[test]
    fn flags_zero_fps() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
fps = 0
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 100
  height = 100
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("fps must be > 0")));
    }

    #[test]
    fn flags_no_panes() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("at least one pane")));
    }

    #[test]
    fn flags_zero_width_pane() {
        let s = score_from(
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
  width = 0
  height = 100
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("width and height must be > 0")));
    }

    #[test]
    fn flags_zero_height_pane() {
        let s = score_from(
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
  height = 0
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("width and height must be > 0")));
    }

    #[test]
    fn pane_at_exact_canvas_edge_is_valid() {
        let s = score_from(
            r##"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 50
  y = 50
  width = 50
  height = 50
[[timeline]]
action = "focus"
pane = "c"
[[timeline]]
action = "type"
text = "ok"
[[timeline]]
action = "terminate"
"##,
        );
        assert!(validate(&s).is_empty());
    }

    #[test]
    fn flags_pane_extending_beyond_bottom_right_corner() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 10
height = 10
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 5
  y = 5
  width = 10
  height = 10
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("beyond the")));
    }

    #[test]
    fn flags_pane_with_large_coords_overflowing() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 4294967295
  y = 0
  width = 1
  height = 1
"#,
        );
        assert!(validate(&s).iter().any(|p| p.contains("beyond the")));
    }

    // ── WaitForStdout edge cases ──────────────────────────────────────────

    #[test]
    fn flags_wait_for_stdout_on_browser_pane() {
        let s = score_from(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
  [[layout.panes]]
  id = "b1"
  type = "browser"
  x = 0
  y = 0
  width = 100
  height = 100
  url = "file:///x.pdf"
[[timeline]]
action = "wait_for_stdout"
match = "ready"
pane = "b1"
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("must be a Terminal")));
    }

    #[test]
    fn flags_wait_for_stdout_with_no_target_and_no_focus() {
        let s = score_from(
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
[[timeline]]
action = "wait_for_stdout"
match = "ready"
"#,
        );
        assert!(validate(&s)
            .iter()
            .any(|p| p.contains("wait_for_stdout needs a pane")));
    }

    // ── Multiple problems reported at once ────────────────────────────────

    #[test]
    fn reports_all_problems_not_just_first() {
        let s = score_from(
            r#"
[demo]
name = "t"
[env]
requires = ["DEMOSTAGE_MISSING_A", "DEMOSTAGE_MISSING_B"]
[layout]
width = 0
height = 0
"#,
        );
        let problems = validate(&s);
        assert!(problems.len() >= 3);
    }
}
