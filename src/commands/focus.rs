//! `demo focus` — switch the live capture's view to one or two sources.
//!
//! A *live* control command (like `demo stop`/`demo open`): run it inside the
//! captured shell, or from **another terminal in the same directory** (so it
//! works even while a full-screen TUI owns the captured terminal). It signals the
//! running recorder, which records the view switch at the live moment.
//!
//! `demo focus main` shows just the terminal; `demo focus docs` a browser source
//! full-screen; `demo focus main docs` the two side by side (`--vertical` stacks
//! them). Timing/behaviour flags — `--hold`, `--scroll`, `--when`, `--after` —
//! defer or shape the reveal. With no source on a terminal it runs a wizard.

use std::io::IsTerminal;

use inquire::{MultiSelect, Select, Text};

use crate::cli::FocusArgs;
use crate::commands::control;
use crate::error::{Error, Result};
use crate::model::{Source, SourceKind};

/// Outcome of the no-arg `demo focus` wizard.
struct WizardOutcome {
    sources: Vec<String>,
    orientation: String,
    when: Option<String>,
    after: bool,
    hold_ms: Option<u64>,
    scroll: bool,
    /// When a single browser source is chosen, show it beside the terminal.
    split_with_main: bool,
}

pub fn run(args: FocusArgs) -> Result<()> {
    // Live command: it only makes sense while a capture is running. `find` gives
    // the shared "no capture in progress" error (also used by `demo stop`).
    control::find()?;
    let sources = control::read_sources();

    // Whether we're running inside the captured shell — if so, the capture's
    // input thread opened a mute span when it saw `demo focus` typed, and we
    // must close it on every exit path (success or failure).
    let in_session = in_session();

    let result = run_inner(args, &sources, in_session);
    if result.is_err() && in_session {
        // Close the mute span that the input thread opened when it saw the
        // command typed, so a failure doesn't leave 90s of black.
        let _ = control::send(serde_json::json!({ "cmd": "reveal_cancel" }));
    }
    result
}

fn run_inner(args: FocusArgs, sources: &[Source], in_session: bool) -> Result<()> {
    let wizard_out = if args.sources.is_empty() {
        if !std::io::stdin().is_terminal() {
            return Err(Error::Export(
                "demo focus needs a source (e.g. `demo focus main`), or a terminal for the wizard"
                    .to_string(),
            ));
        }
        Some(wizard(sources, &args)?)
    } else {
        None
    };

    let chosen = wizard_out
        .as_ref()
        .map(|w| w.sources.clone())
        .unwrap_or_else(|| args.sources.clone());
    let orientation = wizard_out
        .as_ref()
        .map(|w| w.orientation.clone())
        .unwrap_or_else(|| orientation_flag(&args));
    let when = wizard_out
        .as_ref()
        .and_then(|w| w.when.clone())
        .or_else(|| args.when.clone());
    let after = wizard_out.as_ref().map(|w| w.after).unwrap_or(args.after);
    let hold_ms = wizard_out
        .as_ref()
        .and_then(|w| w.hold_ms)
        .or_else(|| args.hold.map(|s| (s.max(0.0) * 1000.0) as u64));
    let scroll = wizard_out.as_ref().map(|w| w.scroll).unwrap_or(args.scroll);
    let split_with_main = wizard_out.as_ref().is_some_and(|w| w.split_with_main);

    if chosen.len() > 2 {
        return Err(Error::Export(
            "demo focus shows at most two sources at once".to_string(),
        ));
    }

    // Resolve each id to a reveal pane (terminal, or a browser source's URL).
    let panes = build_panes(&chosen, split_with_main, sources, args.theme.as_deref())?;

    // In-session, mute this command's echo/wizard from now (from another terminal
    // there's nothing in the captured shell to mute).
    if in_session {
        let _ = control::send(serde_json::json!({ "cmd": "reveal_begin" }));
    }

    let label = chosen.join(if orientation == "vertical" {
        " / "
    } else {
        " | "
    });
    if let Some(pat) = &when {
        println!("● focus → {label} (when output matches {pat:?})");
    } else if after {
        println!("● focus → {label} (when the current command finishes)");
    } else {
        println!("● focus → {label}");
    }
    control::send(serde_json::json!({
        "cmd": "reveal",
        "panes": panes,
        "orientation": orientation,
        "hold": hold_ms,
        "scroll": scroll,
        "when": when,
        "after": after,
    }))?;
    Ok(())
}

/// Build reveal panes from the chosen source ids. A single browser source can be
/// shown full-screen or beside the terminal (`split_with_main`).
fn build_panes(
    chosen: &[String],
    split_with_main: bool,
    sources: &[Source],
    theme_override: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    if chosen.len() == 1 && split_with_main && !is_terminal_id(&chosen[0]) {
        return Ok(vec![
            resolve_pane("main", sources, theme_override)?,
            resolve_pane(&chosen[0], sources, theme_override)?,
        ]);
    }
    chosen
        .iter()
        .map(|id| resolve_pane(id, sources, theme_override))
        .collect()
}

fn is_terminal_id(id: &str) -> bool {
    id == "main" || id == "terminal"
}

/// Resolve a source id to a reveal-pane JSON object. `main`/`terminal` is the
/// terminal (no URL); anything else must be a browser source from the capture.
fn resolve_pane(
    id: &str,
    sources: &[Source],
    theme_override: Option<&str>,
) -> Result<serde_json::Value> {
    if id == "main" || id == "terminal" {
        return Ok(serde_json::json!({ "id": "main" }));
    }
    match sources.iter().find(|s| s.id == id) {
        Some(s) if s.kind == SourceKind::Browser => Ok(serde_json::json!({
            "id": s.id,
            "url": s.url,
            "theme": theme_override.map(str::to_string).or_else(|| s.theme.clone()),
        })),
        Some(_) => Ok(serde_json::json!({ "id": "main" })), // a terminal source
        None => Err(Error::Export(format!(
            "unknown source '{id}' — configure it in `demo capture`, or use `demo open <url>` for an ad-hoc page{}",
            source_hint(sources)
        ))),
    }
}

/// `--vertical`/`--horizontal` → the orientation string (default horizontal).
fn orientation_flag(args: &FocusArgs) -> String {
    if args.vertical {
        "vertical"
    } else {
        "horizontal"
    }
    .to_string()
}

/// True when run inside the captured shell (found via the env var, not the cwd).
fn in_session() -> bool {
    std::env::var(control::CONTROL_ENV)
        .map(|p| !p.is_empty() && std::path::Path::new(&p).exists())
        .unwrap_or(false)
}

/// A hint listing the configured source ids, for error messages.
fn source_hint(sources: &[Source]) -> String {
    let ids: Vec<&str> = sources.iter().map(|s| s.id.as_str()).collect();
    if ids.is_empty() {
        String::new()
    } else {
        format!(" (sources: {})", ids.join(", "))
    }
}

/// The revealable ids: `main` (the terminal, always available) plus every
/// browser source of the capture — with a note when there are no browser
/// sources to pick from.
fn pick_pane_ids(sources: &[Source]) -> Vec<String> {
    let mut ids: Vec<String> = vec!["main".to_string()];
    ids.extend(
        sources
            .iter()
            .filter(|s| s.kind == SourceKind::Browser)
            .map(|s| s.id.clone()),
    );
    if ids.len() == 1 {
        println!("  (this capture has no browser sources — only the terminal.");
        println!("   configure them when starting `demo capture`, or reveal an ad-hoc page with `demo open <url>`)\n");
    }
    ids
}

/// The seconds prompt's validation rule, as a pure predicate: valid iff the
/// trimmed input parses as a positive number. Extracted from the validator
/// closure (which reads the real TTY) so the logic is unit-testable.
fn hold_secs_valid(s: &str) -> bool {
    match s.trim().parse::<f64>() {
        Ok(n) => n > 0.0,
        Err(_) => false,
    }
}

/// Entered seconds → hold in milliseconds: unparsable input falls back to the
/// 6s default, and the result is floored at 0.5s (500 ms).
fn hold_secs_to_ms(raw: &str) -> u64 {
    let secs: f64 = raw.trim().parse().unwrap_or(6.0);
    (secs.max(0.5) * 1000.0) as u64
}

/// How long a static browser reveal holds, in ms. Duplicates the seconds
/// validator locally (same message as `demo open`) rather than sharing it.
fn ask_hold_secs() -> Result<u64> {
    let secs = ask(Text::new("Hold for how many seconds?")
        .with_default("6")
        .with_validator(|s: &str| {
            if hold_secs_valid(s) {
                Ok(inquire::validator::Validation::Valid)
            } else {
                Ok(inquire::validator::Validation::Invalid(
                    "enter a positive number of seconds (e.g. 6)".into(),
                ))
            }
        })
        .prompt())?;
    Ok(hold_secs_to_ms(&secs))
}

/// How the chosen browser pane is shown: a static hold (its duration in ms) or
/// a scroll. Returns `(hold_ms, scroll)`.
fn ask_browser_behavior() -> Result<(Option<u64>, bool)> {
    let behavior = ask(Select::new(
        "Show it as:",
        vec![
            "Static — hold for a few seconds",
            "Scroll the page down (pan)",
        ],
    )
    .prompt())?;
    let scroll = behavior.starts_with("Scroll");
    let hold_ms = if behavior.starts_with("Static") {
        Some(ask_hold_secs()?)
    } else {
        None
    };
    Ok((hold_ms, scroll))
}

/// When the reveal fires. Same choices as the `demo open` wizard, so a focus
/// can be armed ahead of a long-running command instead of firing immediately.
/// Returns `(when_pattern, after)`.
fn ask_trigger() -> Result<(Option<String>, bool)> {
    let trigger = ask(Select::new(
        "Switch:",
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

/// Assemble the wizard result from its already-asked parts.
fn build_outcome(
    sources: Vec<String>,
    orientation: String,
    when: Option<String>,
    after: bool,
    hold_ms: Option<u64>,
    scroll: bool,
    split_with_main: bool,
) -> WizardOutcome {
    WizardOutcome {
        sources,
        orientation,
        when,
        after,
        hold_ms,
        scroll,
        split_with_main,
    }
}

/// Pick 1–2 sources (orientation for two, presentation, and when to reveal) from
/// the capture's sources.
fn wizard(sources: &[Source], args: &FocusArgs) -> Result<WizardOutcome> {
    println!("\n  demo focus — switch the view\n");
    let ids = pick_pane_ids(sources);

    let chosen = ask(MultiSelect::new("Show (pick one or two):", ids)
        .with_help_message("space toggles, enter accepts")
        .prompt())?;
    if chosen.is_empty() {
        return Err(Error::Export("pick at least one source".to_string()));
    }
    if chosen.len() > 2 {
        return Err(Error::Export("pick at most two sources".to_string()));
    }

    let orientation = if chosen.len() == 2 {
        let pick =
            ask(Select::new("Arrange:", vec!["side by side", "stacked (top/bottom)"]).prompt())?;
        if pick.starts_with("stacked") {
            "vertical"
        } else {
            "horizontal"
        }
        .to_string()
    } else {
        orientation_flag(args)
    };

    let has_browser = chosen.iter().any(|id| !is_terminal_id(id));
    let split_with_main = if chosen.len() == 1 && has_browser {
        let mode = ask(Select::new(
            "Place it:",
            vec![
                "replace — full screen (scene swap)",
                "split — beside the terminal",
            ],
        )
        .prompt())?;
        mode.starts_with("split")
    } else {
        false
    };

    let (hold_ms, scroll) = if has_browser {
        ask_browser_behavior()?
    } else {
        (None, false)
    };
    let (when, after) = ask_trigger()?;

    Ok(build_outcome(
        chosen,
        orientation,
        when,
        after,
        hold_ms,
        scroll,
        split_with_main,
    ))
}

/// Unwrap a prompt result, mapping inquire's failure onto our own error.
fn ask<T>(r: std::result::Result<T, inquire::InquireError>) -> Result<T> {
    r.map_err(|e| Error::Export(format!("wizard: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wizard's `ask` wrapper compiles against inquire's error type (0.9):
    /// a prompt that cannot run (no TTY) surfaces as our own error.
    #[test]
    fn ask_maps_inquire_errors_to_export_errors() {
        let err = ask::<String>(Err(inquire::InquireError::NotTTY)).unwrap_err();
        assert!(matches!(err, Error::Export(_)));
        assert_eq!(ask::<String>(Ok("ok".to_string())).unwrap(), "ok");
    }

    #[test]
    fn is_terminal_id_matches_main_and_terminal() {
        assert!(is_terminal_id("main"));
        assert!(is_terminal_id("terminal"));
        assert!(!is_terminal_id("docs"));
        assert!(!is_terminal_id("browser"));
    }

    #[test]
    fn orientation_flag_default_horizontal() {
        let args = FocusArgs {
            sources: vec![],
            vertical: false,
            horizontal: false,
            when: None,
            after: false,
            hold: None,
            scroll: false,
            theme: None,
        };
        assert_eq!(orientation_flag(&args), "horizontal");
    }

    #[test]
    fn orientation_flag_vertical() {
        let args = FocusArgs {
            sources: vec![],
            vertical: true,
            horizontal: false,
            when: None,
            after: false,
            hold: None,
            scroll: false,
            theme: None,
        };
        assert_eq!(orientation_flag(&args), "vertical");
    }

    #[test]
    fn source_hint_empty() {
        assert_eq!(source_hint(&[]), "");
    }

    #[test]
    fn source_hint_lists_ids() {
        let sources = vec![
            Source {
                id: "main".into(),
                kind: SourceKind::Terminal,
                url: None,
                theme: None,
            },
            Source {
                id: "docs".into(),
                kind: SourceKind::Browser,
                url: None,
                theme: None,
            },
        ];
        let hint = source_hint(&sources);
        assert!(hint.contains("main"));
        assert!(hint.contains("docs"));
        assert!(hint.starts_with(" (sources:"));
    }

    #[test]
    fn is_terminal_id_exact_match() {
        assert!(is_terminal_id("main"));
        assert!(is_terminal_id("terminal"));
        assert!(!is_terminal_id("Main"));
        assert!(!is_terminal_id("MAIN"));
        assert!(!is_terminal_id("web"));
    }

    #[test]
    fn orientation_flag_prefers_vertical() {
        let args = FocusArgs {
            sources: vec![],
            vertical: true,
            horizontal: true,
            when: None,
            after: false,
            hold: None,
            scroll: false,
            theme: None,
        };
        assert_eq!(orientation_flag(&args), "vertical");
    }

    #[test]
    fn source_hint_single_source() {
        let sources = vec![Source {
            id: "web".into(),
            kind: SourceKind::Browser,
            url: None,
            theme: None,
        }];
        let hint = source_hint(&sources);
        assert!(hint.contains("web"));
    }

    #[test]
    fn orientation_flag_horizontal_explicit() {
        let args = FocusArgs {
            sources: vec![],
            vertical: false,
            horizontal: true,
            when: None,
            after: false,
            hold: None,
            scroll: false,
            theme: None,
        };
        assert_eq!(orientation_flag(&args), "horizontal");
    }

    #[test]
    fn resolve_pane_main_returns_terminal() {
        let result = resolve_pane("main", &[], None).unwrap();
        assert_eq!(result["id"], "main");
    }

    #[test]
    fn resolve_pane_terminal_returns_terminal() {
        let result = resolve_pane("terminal", &[], None).unwrap();
        assert_eq!(result["id"], "main");
    }

    #[test]
    fn resolve_pane_browser_source_returns_url() {
        let sources = vec![Source {
            id: "docs".into(),
            kind: SourceKind::Browser,
            url: Some("https://x.com".into()),
            theme: None,
        }];
        let result = resolve_pane("docs", &sources, None).unwrap();
        assert_eq!(result["id"], "docs");
        assert_eq!(result["url"], "https://x.com");
    }

    #[test]
    fn resolve_pane_browser_with_theme_override() {
        let sources = vec![Source {
            id: "docs".into(),
            kind: SourceKind::Browser,
            url: Some("https://x.com".into()),
            theme: Some("light".into()),
        }];
        let result = resolve_pane("docs", &sources, Some("dark")).unwrap();
        assert_eq!(result["theme"], "dark");
    }

    #[test]
    fn resolve_pane_browser_without_theme_override_uses_source_theme() {
        let sources = vec![Source {
            id: "docs".into(),
            kind: SourceKind::Browser,
            url: Some("https://x.com".into()),
            theme: Some("light".into()),
        }];
        let result = resolve_pane("docs", &sources, None).unwrap();
        assert_eq!(result["theme"], "light");
    }

    #[test]
    fn resolve_pane_terminal_source_returns_main() {
        let sources = vec![Source {
            id: "web".into(),
            kind: SourceKind::Terminal,
            url: None,
            theme: None,
        }];
        let result = resolve_pane("web", &sources, None).unwrap();
        assert_eq!(result["id"], "main");
    }

    #[test]
    fn resolve_pane_unknown_source_errors() {
        let result = resolve_pane("nonexistent", &[], None);
        assert!(result.is_err());
    }

    #[test]
    fn build_panes_single_non_terminal_splits_with_main() {
        let sources = vec![Source {
            id: "docs".into(),
            kind: SourceKind::Browser,
            url: Some("https://x.com".into()),
            theme: None,
        }];
        let result = build_panes(&["docs".into()], true, &sources, None).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["id"], "main");
        assert_eq!(result[1]["id"], "docs");
    }

    #[test]
    fn build_panes_single_terminal_no_split() {
        let result = build_panes(&["main".into()], true, &[], None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "main");
    }

    #[test]
    fn build_panes_multiple_sources() {
        let sources = vec![
            Source {
                id: "docs".into(),
                kind: SourceKind::Browser,
                url: Some("https://x.com".into()),
                theme: None,
            },
            Source {
                id: "code".into(),
                kind: SourceKind::Browser,
                url: Some("https://y.com".into()),
                theme: None,
            },
        ];
        let result = build_panes(&["docs".into(), "code".into()], false, &sources, None).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn source_hint_multiple_sources() {
        let sources = vec![
            Source {
                id: "main".into(),
                kind: SourceKind::Terminal,
                url: None,
                theme: None,
            },
            Source {
                id: "docs".into(),
                kind: SourceKind::Browser,
                url: None,
                theme: None,
            },
            Source {
                id: "code".into(),
                kind: SourceKind::Browser,
                url: None,
                theme: None,
            },
        ];
        let hint = source_hint(&sources);
        assert!(hint.contains("main"));
        assert!(hint.contains("docs"));
        assert!(hint.contains("code"));
    }

    // --- pick_pane_ids -----------------------------------------------------

    /// No capture sources → just the terminal id. Exact vector equality (not
    /// `len()`/`contains`) so body-replacement mutants (`vec!["xyzzy".into()]`,
    /// `vec![String::new()]`, `vec![]`) die here.
    #[test]
    fn pick_pane_ids_no_sources_returns_exactly_main() {
        assert_eq!(pick_pane_ids(&[]), vec!["main".to_string()]);
    }

    /// Only `Browser` sources are appended after `main`; a `Terminal` source is
    /// skipped, and the order matches the capture's source order exactly.
    #[test]
    fn pick_pane_ids_adds_only_browser_sources_in_order() {
        let sources = vec![
            Source {
                id: "docs".into(),
                kind: SourceKind::Browser,
                url: None,
                theme: None,
            },
            Source {
                id: "term".into(),
                kind: SourceKind::Terminal,
                url: None,
                theme: None,
            },
        ];
        assert_eq!(
            pick_pane_ids(&sources),
            vec!["main".to_string(), "docs".to_string()]
        );
    }

    /// Several browsers keep the capture's order after the leading `main`.
    #[test]
    fn pick_pane_ids_keeps_capture_order_for_browsers() {
        let sources = vec![
            Source {
                id: "term".into(),
                kind: SourceKind::Terminal,
                url: None,
                theme: None,
            },
            Source {
                id: "docs".into(),
                kind: SourceKind::Browser,
                url: None,
                theme: None,
            },
            Source {
                id: "web".into(),
                kind: SourceKind::Browser,
                url: None,
                theme: None,
            },
        ];
        assert_eq!(
            pick_pane_ids(&sources),
            vec!["main".to_string(), "docs".to_string(), "web".to_string()]
        );
    }

    // --- ask_hold_secs: pure validation/conversion helpers -----------------

    /// The validator's positive-number rule. Kills the extracted `>` mutants
    /// in the helper: `>`→`<` and `>`→`==` both reject "6" (asserted true).
    #[test]
    fn hold_secs_valid_accepts_positive_numbers() {
        assert!(hold_secs_valid("6"));
        assert!(hold_secs_valid(" 3 "));
        assert!(hold_secs_valid("0.5"));
    }

    /// Zero, negatives and garbage are rejected. Also kills `>`→`>=`
    /// (0.0 >= 0.0 would be true) and any `false`→`true` literal mutant.
    #[test]
    fn hold_secs_valid_rejects_non_positive_or_unparsable() {
        assert!(!hold_secs_valid("0"));
        assert!(!hold_secs_valid("0.0"));
        assert!(!hold_secs_valid("-1"));
        assert!(!hold_secs_valid("abc"));
        assert!(!hold_secs_valid(""));
    }

    /// Seconds → milliseconds, exact values. `*`→`+` gives 1001 for "1.5",
    /// `*`→`/` gives 0 — both caught by the exact 1500; a `1000.0` literal
    /// change is caught by the exact 6000.
    #[test]
    fn hold_secs_to_ms_converts_seconds_exactly() {
        assert_eq!(hold_secs_to_ms("6"), 6000);
        assert_eq!(hold_secs_to_ms("1.5"), 1500);
        assert_eq!(hold_secs_to_ms(" 2 "), 2000);
    }

    /// Sub-half-second inputs are floored at 0.5s = 500 ms (kills `0.5`
    /// literal mutants in `.max(0.5)`).
    #[test]
    fn hold_secs_to_ms_floors_at_half_a_second() {
        assert_eq!(hold_secs_to_ms("0.1"), 500);
        assert_eq!(hold_secs_to_ms("0.01"), 500);
    }

    /// Unparsable input falls back to the 6s default (kills `6.0` literal
    /// mutants in `.unwrap_or(6.0)`).
    #[test]
    fn hold_secs_to_ms_defaults_to_six_seconds_when_unparsable() {
        assert_eq!(hold_secs_to_ms("abc"), 6000);
        assert_eq!(hold_secs_to_ms(""), 6000);
    }
}
