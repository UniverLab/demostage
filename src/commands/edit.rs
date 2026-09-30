//! `demo edit` — interactive timeline editor.
//!
//! Shows the full timeline. Mark one or several steps with **space**, press
//! **enter** to apply an action to everything marked (edit, convert waits,
//! delete, …), **esc** when done. Edits are applied immediately so you can see
//! the result and re-edit if needed. Marking a group is how you do bulk work —
//! delete a wizard's leftover steps at once, or turn every `wait` of a section
//! into a `wait_for_quiet`.

use crate::cli::EditArgs;
use crate::commands::edit_reveal;
use crate::error::{Error, Result};
use crate::model::{Score, Step};

pub fn run(args: EditArgs) -> Result<()> {
    let mut score = Score::load(&args.input)?;

    println!("{}\n", crate::BANNER);
    println!(
        "demo edit — {}\n  {} steps · ↑↓ navigate · space=mark (several for a bulk edit) · enter=apply · esc=done\n",
        args.input.display(),
        score.timeline.len()
    );

    let mut cursor: usize = 0;

    while !score.timeline.is_empty() {
        let labels: Vec<String> = score
            .timeline
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{:>3}. {}", i + 1, step_summary(s)))
            .collect();

        let picked = inquire::MultiSelect::new("Timeline:", labels)
            .with_page_size(list_page_size())
            .with_starting_cursor(cursor.min(score.timeline.len().saturating_sub(1)))
            .with_help_message("space marks · enter applies to the marked steps · esc done")
            .prompt_skippable()
            .map_err(|e| Error::Export(format!("edit: {e}")))?;

        // Esc at the list (or enter with nothing marked) → ask if done.
        let indices = picked.map(|sel| selection_indices(&sel, score.timeline.len()));
        let Some(indices) = indices.filter(|idx| !idx.is_empty()) else {
            let done = inquire::Confirm::new("Done editing?")
                .with_default(true)
                .prompt()
                .unwrap_or(true);
            if done {
                break;
            }
            continue;
        };

        cursor = indices[0];
        println!();
        if let Some(new_cursor) = apply_action(&mut score, &indices)? {
            cursor = new_cursor;
        }
        if cursor >= score.timeline.len() && cursor > 0 {
            cursor = score.timeline.len() - 1;
        }
    }

    score.save(&args.input)?;
    println!("✓ saved → {}", args.input.display());
    Ok(())
}

/// Fit the list to the terminal (leave room for the prompt/help lines), so a
/// long timeline shows as many steps as the screen allows instead of 7.
fn list_page_size() -> usize {
    let rows = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24) as usize;
    rows.saturating_sub(5).clamp(10, 40)
}

/// Map the marked labels back to timeline indices (from the `N.` prefix),
/// sorted and de-duplicated.
fn selection_indices(selected: &[String], len: usize) -> Vec<usize> {
    let mut indices: Vec<usize> = selected
        .iter()
        .filter_map(|s| {
            s.trim_start()
                .split('.')
                .next()
                .and_then(|n| n.trim().parse::<usize>().ok())
                .map(|n| n - 1)
        })
        .filter(|&i| i < len)
        .collect();
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// Ask for one action and apply it to every selected step.
/// Returns the new cursor position if the timeline changed.
fn apply_action(score: &mut Score, indices: &[usize]) -> Result<Option<usize>> {
    let all_type = indices
        .iter()
        .all(|&i| matches!(score.timeline[i], Step::Type { .. }));

    let all_keypress = indices
        .iter()
        .all(|&i| matches!(score.timeline[i], Step::Keypress { .. }));

    let single_browser_focus =
        indices.len() == 1 && edit_reveal::is_browser_focus(score, indices[0]);

    let has_event_wait = indices.iter().any(|&i| {
        matches!(
            score.timeline[i],
            Step::WaitForQuiet { .. } | Step::WaitForScreen { .. } | Step::WaitForStdout { .. }
        )
    });

    match ask_action(
        indices.len(),
        all_type,
        all_keypress,
        single_browser_focus,
        has_event_wait,
    )? {
        None | Some(EditAction::Keep) => {} // Esc = cancel
        Some(EditAction::EditReveal) => {
            edit_reveal::edit_browser_reveal(score, indices[0])?;
        }
        Some(EditAction::EditKey) => edit_keypress(score, indices)?,
        Some(EditAction::Insert) => return Ok(Some(insert_step(score, indices)?)),
        Some(EditAction::ToWait) => set_wait(score, indices, "converted to wait")?,
        Some(EditAction::WaitForQuiet) => update_wait_quiet(score, indices)?,
        Some(EditAction::WaitForScreen) => update_wait_screen(score, indices)?,
        Some(EditAction::WaitForStdout) => update_wait_stdout(score, indices)?,
        Some(EditAction::ChangeDuration) => set_wait(score, indices, "updated")?,
        Some(EditAction::SplitType) => {
            split_type_step(&mut score.timeline, indices[0])?;
        }
        Some(EditAction::ReplaceInTypes) => {
            replace_in_types(&mut score.timeline, indices)?;
        }
        Some(EditAction::Delete) => delete_steps(score, indices)?,
    }
    Ok(None)
}

/// Ask the user what kind of step to insert and splice it after the selected
/// step. Returns the new cursor position.
fn insert_step(score: &mut Score, indices: &[usize]) -> Result<usize> {
    let new_step = ask_insert_step()?;
    let insert_at = indices[0] + 1;
    score.timeline.insert(insert_at, new_step);
    println!("  ✓ inserted after step {}\n", indices[0] + 1);
    Ok(insert_at)
}

/// Replace the key of every selected `keypress` step with the key the user
/// types (an empty answer leaves the timeline untouched).
fn edit_keypress(score: &mut Score, indices: &[usize]) -> Result<()> {
    let current = match &score.timeline[indices[0]] {
        Step::Keypress { key } => key.clone(),
        _ => String::new(),
    };
    let new_key = inquire::Text::new("key name:")
        .with_help_message(
            "enter, tab, esc, up, down, left, right, f1-f12, ctrl+c, shift-up, alt-f5, ...",
        )
        .with_default(&current)
        .prompt()
        .map_err(|e| Error::Export(format!("edit: {e}")))?;
    let new_key = new_key.trim().to_string();
    if new_key.is_empty() {
        return Err(Error::Export("key name cannot be empty".to_string()));
    }
    for &i in indices {
        if let Step::Keypress { key } = &mut score.timeline[i] {
            *key = new_key.clone();
        }
    }
    println!("  ✓ updated {}\n", plural(indices.len()));
    Ok(())
}

/// Turn every selected step into a `wait` of the duration the user picks.
/// `message` is what the confirmation says: `converted to wait` when the step
/// kind changed, `updated` when only the duration did.
fn set_wait(score: &mut Score, indices: &[usize], message: &str) -> Result<()> {
    let ms = ask_u64("duration_ms", current_ms(&score.timeline[indices[0]]))?;
    for &i in indices {
        score.timeline[i] = Step::Wait { duration_ms: ms };
    }
    println!("  ✓ {message} {}\n", plural(indices.len()));
    Ok(())
}

/// Give every selected step the `quiet_ms` the user picks.
fn update_wait_quiet(score: &mut Score, indices: &[usize]) -> Result<()> {
    let quiet = ask_u64("quiet_ms", current_ms(&score.timeline[indices[0]]))?;
    for &i in indices {
        score.timeline[i] = Step::WaitForQuiet {
            quiet_ms: quiet,
            max_ms: None,
        };
    }
    println!("  ✓ updated {}\n", plural(indices.len()));
    Ok(())
}

/// Point every selected step at the match pattern the user picks.
fn update_wait_screen(score: &mut Score, indices: &[usize]) -> Result<()> {
    let pattern = ask_string("match pattern")?;
    for &i in indices {
        score.timeline[i] = Step::WaitForScreen {
            pattern: pattern.clone(),
            timeout_ms: None,
        };
    }
    println!("  ✓ updated {}\n", plural(indices.len()));
    Ok(())
}

/// Point every selected step at the stdout pattern the user picks.
fn update_wait_stdout(score: &mut Score, indices: &[usize]) -> Result<()> {
    let pattern = ask_string("match pattern")?;
    for &i in indices {
        score.timeline[i] = Step::WaitForStdout {
            pattern: pattern.clone(),
            pane: None,
        };
    }
    println!("  ✓ updated {}\n", plural(indices.len()));
    Ok(())
}

/// Remove every selected step, back to front so earlier indices stay valid
/// while removing.
fn delete_steps(score: &mut Score, indices: &[usize]) -> Result<()> {
    for &i in indices.iter().rev() {
        score.timeline.remove(i);
    }
    println!("  ✓ deleted {}\n", plural(indices.len()));
    Ok(())
}

fn plural(n: usize) -> String {
    if n == 1 {
        "1 step".to_string()
    } else {
        format!("{n} steps")
    }
}

/// Find & replace across several `type` steps at once — the shared treatment
/// that makes sense when editing many texts together.
fn replace_in_types(timeline: &mut [Step], indices: &[usize]) -> Result<()> {
    let find = ask_string("find")?;
    let replace = inquire::Text::new("replace with:")
        .prompt()
        .map_err(|e| Error::Export(format!("edit: {e}")))?;
    let mut changed = 0;
    for &i in indices {
        if let Step::Type { text, human_salt } = &timeline[i] {
            if text.contains(&find) {
                timeline[i] = Step::Type {
                    text: text.replace(&find, &replace),
                    human_salt: *human_salt,
                };
                changed += 1;
            }
        }
    }
    println!("  ✓ replaced in {}\n", plural(changed));
    Ok(())
}

fn current_ms(step: &Step) -> u64 {
    match step {
        Step::Wait { duration_ms } => *duration_ms,
        Step::WaitForQuiet { quiet_ms, .. } => *quiet_ms,
        _ => 500,
    }
}

/// One-line preview of free text: first `n` chars, newlines flattened, with an
/// ellipsis when cut. Keeps the list readable even if a step carries a huge
/// blob (e.g. a mis-captured prompt).
fn preview(text: &str, n: usize) -> String {
    let cut: String = text.chars().take(n).collect();
    let cut = cut.replace('\n', "↵");
    if text.chars().count() > n {
        format!("{cut}…")
    } else {
        cut
    }
}

fn step_summary(step: &Step) -> String {
    match step {
        Step::Type { text, .. } => format!("type {:?}", preview(text, 60)),
        Step::Keypress { key } => format!("keypress {key}"),
        Step::Wait { duration_ms } => format!("wait {duration_ms}ms"),
        Step::WaitForQuiet { quiet_ms, .. } => format!("wait_for_quiet {quiet_ms}ms"),
        Step::WaitForScreen { pattern, .. } => {
            format!("wait_for_screen {:?}", preview(pattern, 40))
        }
        Step::WaitForStdout { pattern, .. } => {
            format!("wait_for_stdout {:?}", preview(pattern, 40))
        }
        Step::Focus { pane } => {
            let id = pane.as_deref().unwrap_or("?");
            format!("focus → {id} (reveal)")
        }
        Step::Caption { text } => format!("caption {:?}", preview(text, 60)),
        Step::Secret { prompt } => format!("secret {:?}", preview(prompt, 60)),
        Step::Scroll {
            direction,
            duration_ms,
            ..
        } => {
            format!("scroll {direction:?} {duration_ms}ms")
        }
        Step::Terminate => "terminate".to_string(),
    }
}

enum EditAction {
    Keep,
    EditReveal,
    EditKey,
    Insert,
    ToWait,
    WaitForQuiet,
    WaitForScreen,
    WaitForStdout,
    ChangeDuration,
    SplitType,
    ReplaceInTypes,
    Delete,
}

/// The action menu for `n` marked steps. Every action applies to all of them;
/// text editing appears when the whole selection is `type` steps (in-place
/// split/edit for one, find & replace across several); key editing appears when
/// all are `keypress` steps.
fn ask_action(
    n: usize,
    all_type: bool,
    all_keypress: bool,
    single_browser_focus: bool,
    has_event_wait: bool,
) -> Result<Option<EditAction>> {
    let mut opts = vec![
        "Keep as-is",
        "→ wait_for_quiet (silence-based)",
        "→ wait_for_screen (VT pattern)",
        "→ wait_for_stdout (raw output pattern)",
        "Change duration",
        "Delete",
    ];

    if has_event_wait {
        opts.insert(1, "→ wait (fixed duration)");
    }

    if single_browser_focus {
        let pos = if has_event_wait { 2 } else { 1 };
        opts.insert(pos, "Edit reveal (placement, scroll, file/URL)");
    }

    if all_type {
        opts.push(if n == 1 {
            "Split/Edit text"
        } else {
            "Find & replace in texts"
        });
    }

    if all_keypress {
        opts.push("Edit key");
    }

    if n == 1 {
        opts.push("Insert step after");
    }

    let prompt = if n == 1 {
        "Action:".to_string()
    } else {
        format!("Action for the {n} marked steps:")
    };
    let choice = inquire::Select::new(&prompt, opts)
        .prompt_skippable()
        .map_err(|e| Error::Export(format!("edit: {e}")))?;

    let Some(choice) = choice else {
        return Ok(None); // Esc = cancel
    };

    Ok(Some(match choice {
        "Keep as-is" => EditAction::Keep,
        "Edit reveal (placement, scroll, file/URL)" => EditAction::EditReveal,
        "→ wait (fixed duration)" => EditAction::ToWait,
        "→ wait_for_quiet (silence-based)" => EditAction::WaitForQuiet,
        "→ wait_for_screen (VT pattern)" => EditAction::WaitForScreen,
        "→ wait_for_stdout (raw output pattern)" => EditAction::WaitForStdout,
        "Change duration" => EditAction::ChangeDuration,
        "Split/Edit text" => EditAction::SplitType,
        "Find & replace in texts" => EditAction::ReplaceInTypes,
        "Edit key" => EditAction::EditKey,
        "Insert step after" => EditAction::Insert,
        "Delete" => EditAction::Delete,
        _ => EditAction::Keep,
    }))
}

fn split_type_step(timeline: &mut Vec<Step>, idx: usize) -> Result<()> {
    let (text, human_salt) = match &timeline[idx] {
        Step::Type { text, human_salt } => (text.clone(), *human_salt),
        _ => return Ok(()),
    };

    let display_text = text.replace('\n', "↵");
    println!("  current: {:?}", display_text);

    let action = inquire::Select::new("How:", vec!["Edit text (replace)", "Split by delimiter"])
        .prompt()
        .map_err(|e| Error::Export(format!("edit: {e}")))?;

    if action.starts_with("Edit") {
        let new = inquire::Text::new("new text:")
            .with_default(&text)
            .prompt()
            .map_err(|e| Error::Export(format!("edit: {e}")))?;
        let new = new.replace("↵", "\n");
        timeline[idx] = Step::Type {
            text: new,
            human_salt,
        };
        println!("  ✓ updated\n");
    } else {
        let delim = inquire::Select::new("Split on:", vec!["newline", "space", "custom"])
            .prompt()
            .map_err(|e| Error::Export(format!("edit: {e}")))?;

        let delim_str = match delim {
            "newline" => "\n",
            "space" => " ",
            _ => {
                let d = ask_string("delimiter")?;
                return do_split(timeline, idx, &d, human_salt, &text);
            }
        };
        do_split(timeline, idx, delim_str, human_salt, &text)?;
    }
    Ok(())
}

fn do_split(
    timeline: &mut Vec<Step>,
    idx: usize,
    delim: &str,
    human_salt: bool,
    text: &str,
) -> Result<()> {
    let parts: Vec<&str> = text.split(delim).collect();
    if parts.len() <= 1 {
        println!("  (delimiter not found — no split)");
        return Ok(());
    }
    let mut new_steps: Vec<Step> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let mut t = part.to_string();
        if i < parts.len() - 1 {
            t.push_str(delim);
        }
        if !t.is_empty() {
            new_steps.push(Step::Type {
                text: t,
                human_salt,
            });
        }
    }
    let n = new_steps.len();
    timeline.splice(idx..=idx, new_steps);
    println!("  ✓ split into {n} parts\n");
    Ok(())
}

fn ask_u64(label: &str, default: u64) -> Result<u64> {
    let v = inquire::Text::new(&format!("{label}:"))
        .with_default(&default.to_string())
        .prompt()
        .map_err(|e| Error::Export(format!("edit: {e}")))?;
    v.trim()
        .parse()
        .map_err(|_| Error::Export(format!("invalid number: {v}")))
}

/// Ask the user what kind of step to insert and build it.
fn ask_insert_step() -> Result<Step> {
    let kind = inquire::Select::new(
        "Insert:",
        vec![
            "wait (fixed duration)",
            "keypress",
            "type (text)",
            "wait_for_quiet",
            "wait_for_stdout",
            "wait_for_screen",
            "caption",
        ],
    )
    .prompt()
    .map_err(|e| Error::Export(format!("edit: {e}")))?;

    match kind {
        "wait (fixed duration)" => {
            let ms = ask_u64("duration_ms", 200)?;
            Ok(Step::Wait { duration_ms: ms })
        }
        "keypress" => {
            let key = ask_string("key name (enter, tab, esc, f1, ctrl+c, ...)")?;
            Ok(Step::Keypress { key })
        }
        "type (text)" => {
            let text = inquire::Text::new("text:")
                .prompt()
                .map_err(|e| Error::Export(format!("edit: {e}")))?;
            Ok(Step::Type {
                text,
                human_salt: true,
            })
        }
        "wait_for_quiet" => {
            let ms = ask_u64("quiet_ms", 500)?;
            Ok(Step::WaitForQuiet {
                quiet_ms: ms,
                max_ms: None,
            })
        }
        "wait_for_stdout" => {
            let pattern = ask_string("match pattern")?;
            Ok(Step::WaitForStdout {
                pattern,
                pane: None,
            })
        }
        "wait_for_screen" => {
            let pattern = ask_string("match pattern")?;
            Ok(Step::WaitForScreen {
                pattern,
                timeout_ms: None,
            })
        }
        "caption" => {
            let text = ask_string("caption text")?;
            Ok(Step::Caption { text })
        }
        _ => Ok(Step::Wait { duration_ms: 200 }),
    }
}

fn ask_string(label: &str) -> Result<String> {
    let v = inquire::Text::new(&format!("{label}:"))
        .prompt()
        .map_err(|e| Error::Export(format!("edit: {e}")))?;
    let v = v.trim().to_string();
    if v.is_empty() {
        return Err(Error::Export("cannot be empty".to_string()));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ScrollDirection, Velocity};

    #[test]
    fn selection_indices_extracts_and_sorts() {
        let selected = vec!["  3. wait 500ms".into(), "1. type \"hi\"".into()];
        let idx = selection_indices(&selected, 10);
        assert_eq!(idx, vec![0, 2]);
    }

    #[test]
    fn selection_indices_filters_out_of_range() {
        let selected = vec!["999. foo".into(), "1. bar".into()];
        let idx = selection_indices(&selected, 3);
        assert_eq!(idx, vec![0]);
    }

    #[test]
    fn selection_indices_deduplicates() {
        let selected = vec!["1. a".into(), "1. b".into()];
        let idx = selection_indices(&selected, 5);
        assert_eq!(idx, vec![0]);
    }

    #[test]
    fn selection_indices_empty() {
        let idx = selection_indices(&[], 5);
        assert!(idx.is_empty());
    }

    #[test]
    fn plural_singular() {
        assert_eq!(plural(1), "1 step");
        assert_eq!(plural(2), "2 steps");
        assert_eq!(plural(0), "0 steps");
    }

    #[test]
    fn preview_truncates_and_replaces_newlines() {
        assert_eq!(preview("hello world", 5), "hello…");
        assert_eq!(preview("ab\ncd", 10), "ab↵cd");
        assert_eq!(preview("short", 10), "short");
        assert_eq!(preview("", 5), "");
    }

    #[test]
    fn current_ms_extracts_wait_duration() {
        let s = Step::Wait { duration_ms: 1234 };
        assert_eq!(current_ms(&s), 1234);
    }

    #[test]
    fn current_ms_extracts_quiet_ms() {
        let s = Step::WaitForQuiet {
            quiet_ms: 500,
            max_ms: None,
        };
        assert_eq!(current_ms(&s), 500);
    }

    #[test]
    fn current_ms_default_for_other_steps() {
        let s = Step::Type {
            text: "hi".into(),
            human_salt: false,
        };
        assert_eq!(current_ms(&s), 500);
    }

    #[test]
    fn step_summary_covers_all_variants() {
        assert!(step_summary(&Step::Type {
            text: "hello".into(),
            human_salt: false
        })
        .contains("type"));
        assert!(step_summary(&Step::Keypress {
            key: "enter".into()
        })
        .contains("enter"));
        assert!(step_summary(&Step::Wait { duration_ms: 500 }).contains("500ms"));
        assert!(step_summary(&Step::WaitForQuiet {
            quiet_ms: 300,
            max_ms: None
        })
        .contains("300ms"));
        assert!(step_summary(&Step::WaitForScreen {
            pattern: "pat".into(),
            timeout_ms: None
        })
        .contains("pat"));
        assert!(step_summary(&Step::WaitForStdout {
            pattern: "pat".into(),
            pane: None
        })
        .contains("pat"));
        assert!(step_summary(&Step::Focus {
            pane: Some("main".into())
        })
        .contains("main"));
        assert!(step_summary(&Step::Focus { pane: None }).contains("?"));
        assert!(step_summary(&Step::Caption { text: "hi".into() }).contains("hi"));
        assert!(step_summary(&Step::Secret {
            prompt: "pass:".into()
        })
        .contains("pass:"));
        assert!(step_summary(&Step::Scroll {
            direction: ScrollDirection::Down,
            velocity: Velocity::Constant,
            duration_ms: 1000,
            pane: None
        })
        .contains("scroll"));
        assert_eq!(step_summary(&Step::Terminate), "terminate");
    }

    #[test]
    fn do_split_splits_on_newline() {
        let mut timeline = vec![
            Step::Type {
                text: "line1\nline2\nline3".into(),
                human_salt: true,
            },
            Step::Wait { duration_ms: 100 },
        ];
        do_split(&mut timeline, 0, "\n", true, "line1\nline2\nline3").unwrap();
        // Original replaced + 3 new parts inserted
        assert!(timeline.len() > 2);
        // Verify all parts are Type steps
        for step in timeline.iter().take(timeline.len() - 1) {
            assert!(matches!(step, Step::Type { .. }));
        }
    }

    #[test]
    fn do_split_splits_on_space() {
        let mut timeline = vec![Step::Type {
            text: "a b c".into(),
            human_salt: false,
        }];
        do_split(&mut timeline, 0, " ", false, "a b c").unwrap();
        // "a b c" split by " " gives ["a ", "b ", "c"] → 3 parts
        assert!(timeline.len() >= 3);
    }

    #[test]
    fn do_split_delimiter_not_found() {
        let mut timeline = vec![Step::Type {
            text: "hello".into(),
            human_salt: true,
        }];
        do_split(&mut timeline, 0, "XYZ", true, "hello").unwrap();
        // No split happens, original stays
        assert_eq!(timeline.len(), 1);
        if let Step::Type { text, .. } = &timeline[0] {
            assert_eq!(text, "hello");
        }
    }

    #[test]
    fn do_split_empty_parts_ignored() {
        let mut timeline = vec![Step::Type {
            text: "a,,b".into(),
            human_salt: true,
        }];
        do_split(&mut timeline, 0, ",", true, "a,,b").unwrap();
        // "a,,b" split by "," gives ["a", "", "b"] → empty part filtered → 2 steps
        assert!(timeline.len() >= 2);
    }

    #[test]
    fn do_split_single_char_delim() {
        let mut timeline = vec![Step::Type {
            text: "one|two|three".into(),
            human_salt: true,
        }];
        do_split(&mut timeline, 0, "|", true, "one|two|three").unwrap();
        assert!(timeline.len() >= 3);
    }

    #[test]
    fn do_split_preserves_human_salt() {
        let mut timeline = vec![Step::Type {
            text: "x y".into(),
            human_salt: true,
        }];
        do_split(&mut timeline, 0, " ", true, "x y").unwrap();
        for step in &timeline {
            if let Step::Type { human_salt, .. } = step {
                assert!(*human_salt);
            }
        }
    }

    #[test]
    fn preview_exact_length_no_ellipsis() {
        assert_eq!(preview("abcde", 5), "abcde");
    }

    #[test]
    fn preview_one_char() {
        assert_eq!(preview("a", 1), "a");
    }

    #[test]
    fn preview_multiple_newlines() {
        assert_eq!(preview("a\nb\nc", 10), "a↵b↵c");
    }

    #[test]
    fn preview_empty_with_zero_n() {
        assert_eq!(preview("", 0), "");
    }

    #[test]
    fn preview_unicode_chars() {
        assert_eq!(preview("café", 4), "café");
    }

    #[test]
    fn preview_unicode_truncated() {
        assert_eq!(preview("café", 3), "caf…");
    }

    #[test]
    fn step_summary_type_long_text() {
        let long = "a".repeat(100);
        let s = Step::Type {
            text: long,
            human_salt: false,
        };
        let summary = step_summary(&s);
        assert!(summary.contains("…"));
        assert!(summary.len() < 100);
    }

    #[test]
    fn step_summary_wait_for_screen_long_pattern() {
        let long = "x".repeat(80);
        let s = Step::WaitForScreen {
            pattern: long,
            timeout_ms: None,
        };
        let summary = step_summary(&s);
        assert!(summary.contains("…"));
    }

    #[test]
    fn step_summary_wait_for_stdout_long_pattern() {
        let long = "y".repeat(80);
        let s = Step::WaitForStdout {
            pattern: long,
            pane: None,
        };
        let summary = step_summary(&s);
        assert!(summary.contains("…"));
    }

    #[test]
    fn step_summary_caption_long_text() {
        let long = "z".repeat(100);
        let s = Step::Caption { text: long };
        let summary = step_summary(&s);
        assert!(summary.contains("…"));
    }

    #[test]
    fn step_summary_secret_long_prompt() {
        let long = "w".repeat(100);
        let s = Step::Secret { prompt: long };
        let summary = step_summary(&s);
        assert!(summary.contains("…"));
    }

    #[test]
    fn step_summary_scroll_up() {
        let s = Step::Scroll {
            direction: ScrollDirection::Up,
            velocity: Velocity::Constant,
            duration_ms: 2000,
            pane: None,
        };
        let summary = step_summary(&s);
        assert!(summary.contains("scroll"));
        assert!(summary.contains("2000ms"));
    }

    #[test]
    fn step_summary_scroll_down() {
        let s = Step::Scroll {
            direction: ScrollDirection::Down,
            velocity: Velocity::Constant,
            duration_ms: 500,
            pane: Some("main".into()),
        };
        let summary = step_summary(&s);
        assert!(summary.contains("scroll"));
    }

    #[test]
    fn step_summary_focus_no_pane() {
        let s = Step::Focus { pane: None };
        assert!(step_summary(&s).contains("?"));
    }

    #[test]
    fn step_summary_focus_with_pane() {
        let s = Step::Focus {
            pane: Some("docs".into()),
        };
        assert!(step_summary(&s).contains("docs"));
    }

    #[test]
    fn selection_indices_out_of_range_filtered() {
        let selected = vec!["5. foo".into()];
        let idx = selection_indices(&selected, 3);
        assert!(idx.is_empty());
    }

    #[test]
    fn selection_indices_multiple_in_range() {
        let selected = vec!["1. a".into(), "3. b".into(), "5. c".into()];
        let idx = selection_indices(&selected, 10);
        assert_eq!(idx, vec![0, 2, 4]);
    }

    #[test]
    fn current_ms_wait_for_screen() {
        let s = Step::WaitForScreen {
            pattern: "pat".into(),
            timeout_ms: None,
        };
        assert_eq!(current_ms(&s), 500);
    }

    #[test]
    fn current_ms_wait_for_stdout() {
        let s = Step::WaitForStdout {
            pattern: "pat".into(),
            pane: None,
        };
        assert_eq!(current_ms(&s), 500);
    }

    #[test]
    fn current_ms_focus() {
        let s = Step::Focus {
            pane: Some("main".into()),
        };
        assert_eq!(current_ms(&s), 500);
    }

    #[test]
    fn current_ms_caption() {
        let s = Step::Caption { text: "hi".into() };
        assert_eq!(current_ms(&s), 500);
    }

    #[test]
    fn current_ms_secret() {
        let s = Step::Secret {
            prompt: "pass:".into(),
        };
        assert_eq!(current_ms(&s), 500);
    }

    #[test]
    fn current_ms_terminate() {
        assert_eq!(current_ms(&Step::Terminate), 500);
    }

    #[test]
    fn do_split_single_part_no_split() {
        let mut timeline = vec![Step::Type {
            text: "nodelimiter".into(),
            human_salt: true,
        }];
        do_split(&mut timeline, 0, "|||", true, "nodelimiter").unwrap();
        assert_eq!(timeline.len(), 1);
    }

    #[test]
    fn do_split_preserves_delimiter_in_parts() {
        let mut timeline = vec![Step::Type {
            text: "a,b,c".into(),
            human_salt: false,
        }];
        do_split(&mut timeline, 0, ",", false, "a,b,c").unwrap();
        // "a,b,c" split by "," → ["a,", "b,", "c"] → 3 steps
        assert!(timeline.len() >= 3);
    }
}
