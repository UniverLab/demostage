//! Executes a terminal-only score in a real PTY and captures the output as a
//! timestamped recording — the basis for the cast/html/gif targets.
//!
//! The shell echoes typed characters, so pacing the writes (humanized typing)
//! produces a natural char-by-char appearance in the capture. A clean `PS1` is
//! forced before the clock starts, so demos never leak `user@host`.

use std::io::Write;
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use vt100::Parser as VtParser;

use crate::error::{Error, Result};
use crate::model::{PaneKind, Score, Step};
use crate::normalize::Rng;

mod pty;

use pty::{finish, secret_step, type_step, CapturePty};

/// Assumed monospace cell size (px), inverse of the normalizer's sizing.
const CELL_W: u32 = 10;
const CELL_H: u32 = 20;
/// Built-in demo prompt (bash `PS1`): a realistic, generic Linux prompt —
/// `user@demo:~$` with the Ubuntu-style green user@host and blue path — used when
/// the score pins none. Generic on purpose (never the real user/host). Set
/// `[demo] prompt` to customize.
pub const DEFAULT_PROMPT: &str =
    "\\[\\e[1;32m\\]user@demo\\[\\e[0m\\]:\\[\\e[1;34m\\]~\\[\\e[0m\\]$ ";

/// Returns true if the shell path looks like zsh.
pub fn is_zsh(shell: &str) -> bool {
    shell.ends_with("/zsh") || shell == "zsh"
}

/// Build a shell-appropriate default prompt string.
pub fn default_prompt_for(shell: &str) -> String {
    if is_zsh(shell) {
        "%B%F{green}user@demo%f%b:%B%F{blue}~%f%b$ ".to_string()
    } else {
        DEFAULT_PROMPT.to_string()
    }
}
/// Default seed when the score pins none.
const DEFAULT_SEED: u64 = 0xD370_5EED;
/// Cap for `wait_for_stdout` so a missing match can't hang export.
const WAIT_FOR_TIMEOUT_MS: u64 = 15_000;
/// Grace period for the shell to exit after the demo's `exit` before we kill it.
/// A demo whose last command leaves a process in the foreground (a server, a
/// REPL — anything that swallows `exit`) would otherwise block teardown forever.
const EXIT_GRACE_MS: u64 = 2_000;
/// Cap on waiting for the capture thread to drain once the shell is gone. A
/// stray foreground process can keep the PTY open, so we never block on it.
const READER_JOIN_MS: u64 = 1_000;
/// Pre-roll: discard startup chatter once output has been quiet this long…
const PREROLL_QUIET_MS: u64 = 400;
/// …but never wait longer than this for it to settle.
const PREROLL_MAX_MS: u64 = 4_000;
/// End of run: consider the demo finished once output is quiet this long (this
/// also becomes how long the final frame is held).
const SETTLE_QUIET_MS: u64 = 1_500;
/// …capped, so a last command that streams forever can't hang the export.
const SETTLE_MAX_MS: u64 = 12_000;

/// A captured terminal recording.
#[derive(Clone)]
pub struct Recording {
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    /// `(seconds_from_start, utf8 chunk)` output events.
    pub events: Vec<(f64, String)>,
    /// `(seconds_from_start, text)` caption changes (empty text clears).
    pub captions: Vec<(f64, String)>,
    /// `(seconds_from_start, pane_id)` of each `focus` — used by the stage to
    /// reveal a browser pane at the moment it's focused.
    pub focuses: Vec<(f64, String)>,
    pub duration: f64,
}

/// Run a single-terminal score (cast/html/gif fast path) — rejects browser panes.
pub fn run_terminal(score: &Score) -> Result<Recording> {
    let pane = single_terminal_pane(score)?;
    run_with_pane(score, pane)
}

/// The three timestamped streams a timeline run produces: what the shell
/// printed, plus the captions/focuses that happened while it ran.
#[derive(Default)]
struct Captures {
    events: Vec<(f64, String)>,
    captions: Vec<(f64, String)>,
    focuses: Vec<(f64, String)>,
}

/// VT screen wait state: the cached parser, how many events have been fed
/// into it, and the grid dimensions it was built for.
struct ScreenWait {
    parser: Option<VtParser>,
    fed: usize,
    rows: u16,
    cols: u16,
}

/// Mutable state a timeline run threads through each step: the PTY, the
/// in-memory secrets, typing config, and the captures accumulated so far.
struct StepRunner<'a> {
    pty: &'a mut CapturePty,
    secrets: &'a std::collections::HashMap<String, String>,
    typing: crate::model::Typing,
    rng: Rng,
    events: Vec<(f64, String)>,
    captions: Vec<(f64, String)>,
    focuses: Vec<(f64, String)>,
    screen: ScreenWait,
    t0: Instant,
}

/// Type one keypress into the PTY, giving TUIs extra time for ESC.
fn press_key_step(key: &str, pty: &mut CapturePty, events: &mut Vec<(f64, String)>, t0: Instant) {
    let _ = pty.writer.write_all(&key_to_bytes(key));
    let _ = pty.writer.flush();
    let delay = key_delay(key);
    sleep_collecting(delay, events, &pty.rx, t0);
}

/// Settle delay after a keypress: TUIs need extra time for ESC. Pure so the
/// exact choice of arms is unit-testable without a live PTY.
fn key_delay(key: &str) -> u64 {
    if key == "esc" || key == "escape" {
        200
    } else {
        60
    }
}

/// Apply one timeline step, appending to the runner's captures.
/// Returns false when the timeline should stop (`Terminate`).
fn apply_step(step: &Step, runner: &mut StepRunner<'_>) -> bool {
    match step {
        Step::Focus { pane } => {
            if let Some(target) = pane.clone() {
                runner
                    .focuses
                    .push((runner.t0.elapsed().as_secs_f64(), target));
            }
        }
        Step::Caption { text } => {
            runner
                .captions
                .push((runner.t0.elapsed().as_secs_f64(), text.clone()));
        }
        Step::Type { text, human_salt } => {
            type_step(
                text,
                *human_salt,
                &runner.typing,
                &mut runner.rng,
                runner.pty,
                &mut runner.events,
                runner.t0,
            );
        }
        Step::Keypress { key } => {
            press_key_step(key, runner.pty, &mut runner.events, runner.t0);
        }
        Step::Wait { duration_ms } => {
            sleep_collecting(*duration_ms, &mut runner.events, &runner.pty.rx, runner.t0);
        }
        Step::WaitForStdout { pattern, .. } => {
            wait_for(pattern, &mut runner.events, &runner.pty.rx, runner.t0);
        }
        Step::WaitForQuiet { quiet_ms, max_ms } => {
            settle(
                &mut runner.events,
                &runner.pty.rx,
                runner.t0,
                *quiet_ms,
                max_ms.unwrap_or(WAIT_FOR_TIMEOUT_MS),
            );
        }
        Step::WaitForScreen {
            pattern,
            timeout_ms,
        } => {
            wait_for_screen(
                pattern,
                &mut runner.events,
                &runner.pty.rx,
                runner.t0,
                &mut runner.screen,
                timeout_ms.unwrap_or(WAIT_FOR_TIMEOUT_MS),
            );
        }
        Step::Secret { prompt } => {
            secret_step(
                prompt,
                runner.secrets,
                runner.pty,
                &mut runner.events,
                runner.t0,
            );
        }
        Step::Scroll { .. } => {} // browser-only; no-op for terminal capture
        Step::Terminate => return false,
    }
    true
}

/// Replay the score's timeline against the PTY, collecting the timed events and
/// the captions/focuses that happened during them.
fn drive_timeline(
    score: &Score,
    pty: &mut CapturePty,
    secrets: &std::collections::HashMap<String, String>,
    t0: Instant,
) -> Captures {
    let typing = score.typing.clone().unwrap_or_default();
    let rng = Rng::new(typing.seed.unwrap_or(DEFAULT_SEED));
    let (rows, cols) = (pty.rows, pty.cols);
    let mut runner = StepRunner {
        pty,
        secrets,
        typing,
        rng,
        events: Vec::new(),
        captions: Vec::new(),
        focuses: Vec::new(),
        screen: ScreenWait {
            parser: None,
            fed: 0,
            rows,
            cols,
        },
        t0,
    };

    // The startup prompt was discarded above; emit one fresh prompt so the first
    // command has a clean prompt (`$ `) in front of it.
    let _ = runner.pty.writer.write_all(b"\r");
    let _ = runner.pty.writer.flush();
    // Capture that fresh prompt so it leads the first command.
    sleep_collecting(120, &mut runner.events, &runner.pty.rx, t0);

    let total_steps = score.timeline.len();
    let mut progress = Progress::new("recording", total_steps);
    for step in score.timeline.iter() {
        progress.tick();
        if !apply_step(step, &mut runner) {
            break;
        }
    }

    // Clear the progress line.
    progress_clear();

    Captures {
        events: runner.events,
        captions: runner.captions,
        focuses: runner.focuses,
    }
}

/// Run the score's timeline in a PTY sized to `pane`, capturing its output.
/// Browser steps (focus/scroll on browser panes) are no-ops here; the stage
/// drives browser panes separately and composites the result.
pub fn run_with_pane(score: &Score, pane: &crate::model::Pane) -> Result<Recording> {
    // Secrets the demo enters are NOT stored — ask for them up front and keep them
    // only in memory for this run; they're typed at each `Secret` step below.
    let secrets = collect_secrets(score)?;

    let mut pty = CapturePty::new(score, pane)?;
    pty.pre_roll(score);

    // ── Timed run. ───────────────────────────────────────────────────────
    let t0 = Instant::now();
    let mut caps = drive_timeline(score, &mut pty, &secrets, t0);

    // ── Settle: hold after the last step until output goes quiet, so the final
    // result (a command's output, an error) finishes rendering and is held on
    // screen — rather than being cut off by a fixed timer. ──────────────────
    let settle_end = settle(
        &mut caps.events,
        &pty.rx,
        t0,
        SETTLE_QUIET_MS,
        SETTLE_MAX_MS,
    );
    Ok(finish(score, pty, settle_end, t0, caps))
}
/// Ask for every secret the score enters, up front, keeping the values only in
/// memory for this run (they're typed at each [`Step::Secret`]). Returns a map of
/// prompt label → value. No `Secret` steps → no prompts.
fn collect_secrets(score: &Score) -> Result<std::collections::HashMap<String, String>> {
    let mut prompts: Vec<&str> = Vec::new();
    for step in &score.timeline {
        if let Step::Secret { prompt } = step {
            if !prompts.contains(&prompt.as_str()) {
                prompts.push(prompt);
            }
        }
    }
    let mut out = std::collections::HashMap::new();
    if prompts.is_empty() {
        return Ok(out);
    }
    eprintln!(
        "This demo enters {} secret(s); they're asked now and kept only in memory \
         (never written to disk):",
        prompts.len()
    );
    for p in prompts {
        let val = inquire::Password::new(p)
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .without_confirmation()
            .prompt()
            .map_err(|e| Error::Export(format!("secret prompt: {e}")))?;
        out.insert(p.to_string(), val);
    }
    Ok(out)
}

/// A stable substring of a secret's prompt label to wait for in the output (the
/// label minus its trailing punctuation), e.g. `Vault passphrase` for
/// `Vault passphrase:`.
fn secret_needle(prompt: &str) -> String {
    prompt
        .trim()
        .trim_end_matches([':', '?', ' '])
        .trim()
        .to_string()
}

/// Has `needle` shown up in the recent captured output (so a prompt we're waiting
/// for has already printed)?
fn recent_contains(events: &[(f64, String)], needle: &str) -> bool {
    events
        .iter()
        .rev()
        .take(40)
        .any(|(_, d)| d.contains(needle))
}

fn single_terminal_pane(score: &Score) -> Result<&crate::model::Pane> {
    if score
        .layout
        .panes
        .iter()
        .any(|p| p.kind == PaneKind::Browser)
    {
        return Err(Error::Export(
            "browser panes need a browser renderer (chromium) and aren't supported yet — \
             terminal panes only for now"
                .to_string(),
        ));
    }
    let mut terms = score
        .layout
        .panes
        .iter()
        .filter(|p| p.kind == PaneKind::Terminal);
    match (terms.next(), terms.next()) {
        (Some(p), None) => Ok(p),
        (None, _) => Err(Error::Export("no terminal pane to record".to_string())),
        (Some(_), Some(_)) => Err(Error::Export(
            "cast/html support a single terminal pane (found several)".to_string(),
        )),
    }
}

fn collect(events: &mut Vec<(f64, String)>, rx: &Receiver<(Instant, Vec<u8>)>, t0: Instant) {
    while let Ok((ts, bytes)) = rx.try_recv() {
        let secs = ts.saturating_duration_since(t0).as_secs_f64();
        events.push((secs, String::from_utf8_lossy(&bytes).into_owned()));
    }
}

fn sleep_collecting(
    ms: u64,
    events: &mut Vec<(f64, String)>,
    rx: &Receiver<(Instant, Vec<u8>)>,
    t0: Instant,
) {
    let until = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < until {
        collect(events, rx, t0);
        thread::sleep(Duration::from_millis(10));
    }
    collect(events, rx, t0);
}

fn wait_for(
    pattern: &str,
    events: &mut Vec<(f64, String)>,
    rx: &Receiver<(Instant, Vec<u8>)>,
    t0: Instant,
) {
    let deadline = Instant::now() + Duration::from_millis(WAIT_FOR_TIMEOUT_MS);
    let mut seen = String::new();
    while Instant::now() < deadline {
        let before = events.len();
        collect(events, rx, t0);
        for (_, data) in &events[before..] {
            seen.push_str(data);
        }
        if seen.contains(pattern) {
            return;
        }
        thread::sleep(Duration::from_millis(15));
    }
}

/// Block until `pattern` is visible on the parsed VT screen buffer.
/// Unlike `wait_for`, this strips escape codes and checks only rendered text.
fn wait_for_screen(
    pattern: &str,
    events: &mut Vec<(f64, String)>,
    rx: &Receiver<(Instant, Vec<u8>)>,
    t0: Instant,
    screen: &mut ScreenWait,
    timeout_ms: u64,
) {
    let rows = screen.rows;
    let cols = screen.cols;
    if screen.parser.is_none() {
        let mut fresh = VtParser::new(rows, cols, 0);
        for (_, data) in events.iter() {
            fresh.process(data.as_bytes());
        }
        screen.fed = events.len();
        screen.parser = Some(fresh);
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        collect(events, rx, t0);
        // Feed new events into the VT parser.
        let fed = screen.fed;
        let found = {
            let parser = screen
                .parser
                .as_mut()
                .expect("screen parser initialised above");
            for (_, data) in &events[fed..] {
                parser.process(data.as_bytes());
            }
            screen_contains(parser, pattern)
        };
        screen.fed = events.len();
        // Check if pattern is visible on the rendered screen.
        if found {
            return;
        }
        thread::sleep(Duration::from_millis(15));
    }
}

/// Check whether `pattern` appears in the visible text of the VT screen.
fn screen_contains(parser: &VtParser, pattern: &str) -> bool {
    parser.screen().contents().contains(pattern)
}

/// Read and discard output until `marker` appears (or `max_ms` elapses). Used in
/// the pre-roll to wait out slow shell startup deterministically.
fn wait_for_marker(rx: &Receiver<(Instant, Vec<u8>)>, marker: &str, max_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(max_ms);
    let mut seen = String::new();
    while Instant::now() < deadline {
        let mut got = false;
        while let Ok((_, bytes)) = rx.try_recv() {
            seen.push_str(&String::from_utf8_lossy(&bytes));
            got = true;
        }
        if seen.contains(marker) {
            return;
        }
        if !got {
            thread::sleep(Duration::from_millis(15));
        }
    }
}

/// Discard output until none has arrived for `quiet_ms` (or `max_ms` elapses) —
/// used to wait out (and throw away) slow shell-startup chatter before the timed
/// run, instead of a racy fixed delay.
fn drain_until_quiet(rx: &Receiver<(Instant, Vec<u8>)>, quiet_ms: u64, max_ms: u64) {
    let start = Instant::now();
    let mut last = Instant::now();
    loop {
        let mut got = false;
        while rx.try_recv().is_ok() {
            got = true;
        }
        if got {
            last = Instant::now();
        }
        if last.elapsed() >= Duration::from_millis(quiet_ms)
            || start.elapsed() >= Duration::from_millis(max_ms)
        {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Capture output until none has arrived for `quiet_ms` (or `max_ms` elapses),
/// returning the elapsed time when it went quiet — so the final result finishes
/// rendering and the last frame is held for the quiet period.
fn settle(
    events: &mut Vec<(f64, String)>,
    rx: &Receiver<(Instant, Vec<u8>)>,
    t0: Instant,
    quiet_ms: u64,
    max_ms: u64,
) -> f64 {
    let start = Instant::now();
    let mut last_change = Instant::now();
    loop {
        let before = events.len();
        collect(events, rx, t0);
        if events.len() != before {
            last_change = Instant::now();
        }
        if last_change.elapsed() >= Duration::from_millis(quiet_ms)
            || start.elapsed() >= Duration::from_millis(max_ms)
        {
            return t0.elapsed().as_secs_f64();
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Print a progress bar to stderr: `  label… [████████░░░░░░░░] 42%`
pub fn progress_bar(label: &str, current: usize, total: usize) {
    const WIDTH: usize = 24;
    let pct = (current * 100)
        .checked_div(total)
        .map(|v| v.min(100))
        .unwrap_or(100);
    let filled = (pct * WIDTH) / 100;
    let bar: String = "█".repeat(filled) + &"░".repeat(WIDTH - filled);
    eprint!("\r  {label} [{bar}] {pct:>3}%");
}

/// A per-frame progress counter for render loops: counts emitted frames and
/// redraws the bar, so the `+ 1` step is unit-testable without rendering.
pub(crate) struct Progress {
    n: usize,
    total: usize,
    label: &'static str,
}

impl Progress {
    pub(crate) fn new(label: &'static str, total: usize) -> Self {
        Progress { n: 0, total, label }
    }

    /// Record one more emitted frame: redraws the bar and returns the new count.
    pub(crate) fn tick(&mut self) -> usize {
        self.n += 1;
        progress_bar(self.label, self.n, self.total);
        self.n
    }
}

/// Clear the progress bar line.
pub fn progress_clear() {
    eprint!("\r{}\r", " ".repeat(60));
}

/// Wrap a string in POSIX single quotes for safe substitution into a shell
/// command (each embedded `'` becomes `'\''`), so an arbitrary configured prompt
/// can't break out of the `PS1=…` assignment.
pub fn sh_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Translate a named key into the bytes a terminal expects.
fn key_to_bytes(key: &str) -> Vec<u8> {
    match key.to_ascii_lowercase().as_str() {
        "enter" | "return" | "\\n" => vec![b'\r'],
        "tab" => vec![b'\t'],
        "space" => vec![b' '],
        "esc" | "escape" => vec![0x1b],
        "backspace" => vec![0x7f],
        "up" => vec![0x1b, b'[', b'A'],
        "down" => vec![0x1b, b'[', b'B'],
        "right" => vec![0x1b, b'[', b'C'],
        "left" => vec![0x1b, b'[', b'D'],
        "home" => vec![0x1b, b'[', b'H'],
        "end" => vec![0x1b, b'[', b'F'],
        "insert" => vec![0x1b, b'[', b'2', b'~'],
        "delete" => vec![0x1b, b'[', b'3', b'~'],
        "pageup" => vec![0x1b, b'[', b'5', b'~'],
        "pagedown" => vec![0x1b, b'[', b'6', b'~'],
        // Function keys F1-F12 as CSI sequences (xterm/vt220 standard).
        "f1" => vec![0x1b, b'[', b'1', b'1', b'~'],
        "f2" => vec![0x1b, b'[', b'1', b'2', b'~'],
        "f3" => vec![0x1b, b'[', b'1', b'3', b'~'],
        "f4" => vec![0x1b, b'[', b'1', b'4', b'~'],
        "f5" => vec![0x1b, b'[', b'1', b'5', b'~'],
        "f6" => vec![0x1b, b'[', b'1', b'7', b'~'],
        "f7" => vec![0x1b, b'[', b'1', b'8', b'~'],
        "f8" => vec![0x1b, b'[', b'1', b'9', b'~'],
        "f9" => vec![0x1b, b'[', b'2', b'0', b'~'],
        "f10" => vec![0x1b, b'[', b'2', b'1', b'~'],
        "f11" => vec![0x1b, b'[', b'2', b'3', b'~'],
        "f12" => vec![0x1b, b'[', b'2', b'4', b'~'],
        "ctrl+c" => vec![0x03],
        "ctrl+d" => vec![0x04],
        "ctrl+u" => vec![0x15],
        "ctrl+l" => vec![0x0c],
        other => {
            // modifier + arrow/function key: "shift-up", "ctrl+up", "alt-f5", etc.
            if let Some(rest) = parse_modifier_key(other) {
                return rest;
            }
            // ctrl+<letter>
            if let Some(letter) = other.strip_prefix("ctrl+") {
                if let Some(c) = letter.chars().next() {
                    if c.is_ascii_alphabetic() {
                        return vec![(c.to_ascii_lowercase() as u8) - b'a' + 1];
                    }
                }
            }
            // a single character → itself; otherwise Enter as a safe default
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => c.to_string().into_bytes(),
                _ => vec![b'\r'],
            }
        }
    }
}

/// CSI modifier number for xterm-style encoding.
fn modifier_code(name: &str) -> Option<u8> {
    match name {
        "shift" => Some(2),
        "alt" => Some(3),
        "ctrl" => Some(5),
        "ctrl+shift" | "shift+ctrl" => Some(6),
        "ctrl+alt" | "alt+ctrl" => Some(7),
        _ => None,
    }
}

/// Arrow/function key code for CSI sequences.
fn arrow_code(key: &str) -> Option<(u8, char)> {
    match key {
        "up" => Some((1, 'A')),
        "down" => Some((1, 'B')),
        "right" => Some((1, 'C')),
        "left" => Some((1, 'D')),
        "home" => Some((1, 'H')),
        "end" => Some((1, 'F')),
        "insert" => Some((2, '~')),
        "delete" => Some((3, '~')),
        "pageup" => Some((5, '~')),
        "pagedown" => Some((6, '~')),
        "f1" => Some((11, '~')),
        "f2" => Some((12, '~')),
        "f3" => Some((13, '~')),
        "f4" => Some((14, '~')),
        "f5" => Some((15, '~')),
        "f6" => Some((17, '~')),
        "f7" => Some((18, '~')),
        "f8" => Some((19, '~')),
        "f9" => Some((20, '~')),
        "f10" => Some((21, '~')),
        "f11" => Some((23, '~')),
        "f12" => Some((24, '~')),
        _ => None,
    }
}

/// Try to parse a modified key like "shift-up", "ctrl+f5", "alt-home" into
/// the corresponding CSI byte sequence.
fn parse_modifier_key(key: &str) -> Option<Vec<u8>> {
    // Try "modifier-key" or "modifier+key" separators.
    let (mod_name, base_key) = if let Some(pos) = key.find('-') {
        (&key[..pos], &key[pos + 1..])
    } else {
        let pos = key.find('+')?;
        (&key[..pos], &key[pos + 1..])
    };
    let mod_code = modifier_code(mod_name)?;
    let (code, final_byte) = arrow_code(base_key)?;
    // Unmodified: emit plain CSI sequence without modifier param.
    if mod_code == 0 {
        return None;
    }
    let mut seq = vec![0x1b, b'['];
    if final_byte == '~' {
        // e.g. \x1b[15;2~ (Shift+F5)
        seq.extend_from_slice(code.to_string().as_bytes());
        seq.push(b';');
        seq.push(b'0' + mod_code);
        seq.push(b'~');
    } else {
        // e.g. \x1b[1;2A (Shift+Up)
        seq.extend_from_slice(b"1;");
        seq.push(b'0' + mod_code);
        seq.push(final_byte as u8);
    }
    Some(seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_named_keys() {
        assert_eq!(key_to_bytes("enter"), vec![b'\r']);
        assert_eq!(key_to_bytes("ctrl+c"), vec![0x03]);
        assert_eq!(key_to_bytes("ctrl+s"), vec![0x13]);
        assert_eq!(key_to_bytes("ctrl+a"), vec![0x01]);
        assert_eq!(key_to_bytes("tab"), vec![b'\t']);
        assert_eq!(key_to_bytes("a"), b"a".to_vec());
        assert_eq!(key_to_bytes("up"), vec![0x1b, b'[', b'A']);
    }

    #[test]
    fn maps_function_keys_to_csi() {
        // F1 = ESC [ 1 1 ~
        assert_eq!(key_to_bytes("f1"), vec![0x1b, b'[', b'1', b'1', b'~']);
        // F2 = ESC [ 1 2 ~
        assert_eq!(key_to_bytes("f2"), vec![0x1b, b'[', b'1', b'2', b'~']);
        // F12 = ESC [ 2 4 ~
        assert_eq!(key_to_bytes("f12"), vec![0x1b, b'[', b'2', b'4', b'~']);
    }

    #[test]
    fn maps_modified_arrow_keys() {
        // Shift+Up = ESC [ 1 ; 2 A
        assert_eq!(
            key_to_bytes("shift-up"),
            vec![0x1b, b'[', b'1', b';', b'2', b'A']
        );
        // Ctrl+Down = ESC [ 1 ; 5 B
        assert_eq!(
            key_to_bytes("ctrl-down"),
            vec![0x1b, b'[', b'1', b';', b'5', b'B']
        );
        // Alt+Right = ESC [ 1 ; 3 C
        assert_eq!(
            key_to_bytes("alt-right"),
            vec![0x1b, b'[', b'1', b';', b'3', b'C']
        );
    }

    #[test]
    fn maps_modified_function_keys() {
        // Shift+F5 = ESC [ 1 5 ; 2 ~
        assert_eq!(
            key_to_bytes("shift-f5"),
            vec![0x1b, b'[', b'1', b'5', b';', b'2', b'~']
        );
        // Ctrl+F12 = ESC [ 2 4 ; 5 ~
        assert_eq!(
            key_to_bytes("ctrl-f12"),
            vec![0x1b, b'[', b'2', b'4', b';', b'5', b'~']
        );
    }

    #[test]
    fn single_quotes_prompts_safely() {
        // The default prompt (colour escapes and all) is wrapped verbatim.
        assert_eq!(
            sh_single_quote(DEFAULT_PROMPT),
            format!("'{DEFAULT_PROMPT}'")
        );
        assert_eq!(sh_single_quote("$ "), "'$ '");
        // An embedded quote can't break out of the assignment.
        assert_eq!(sh_single_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn is_zsh_detects_zsh_paths() {
        assert!(is_zsh("/bin/zsh"));
        assert!(is_zsh("zsh"));
        assert!(!is_zsh("/bin/bash"));
        assert!(!is_zsh("bash"));
    }

    #[test]
    fn default_prompt_for_zsh() {
        let p = default_prompt_for("/bin/zsh");
        assert!(p.contains("user@demo"));
        assert!(p.contains("%B%F{green}"));
    }

    #[test]
    fn default_prompt_for_bash() {
        let p = default_prompt_for("/bin/bash");
        assert_eq!(p, DEFAULT_PROMPT);
    }

    #[test]
    fn key_to_bytes_special_keys() {
        assert_eq!(key_to_bytes("space"), vec![b' ']);
        assert_eq!(key_to_bytes("esc"), vec![0x1b]);
        assert_eq!(key_to_bytes("escape"), vec![0x1b]);
        assert_eq!(key_to_bytes("backspace"), vec![0x7f]);
        assert_eq!(key_to_bytes("delete"), vec![0x1b, b'[', b'3', b'~']);
        assert_eq!(key_to_bytes("insert"), vec![0x1b, b'[', b'2', b'~']);
        assert_eq!(key_to_bytes("home"), vec![0x1b, b'[', b'H']);
        assert_eq!(key_to_bytes("end"), vec![0x1b, b'[', b'F']);
        assert_eq!(key_to_bytes("pageup"), vec![0x1b, b'[', b'5', b'~']);
        assert_eq!(key_to_bytes("pagedown"), vec![0x1b, b'[', b'6', b'~']);
    }

    #[test]
    fn key_to_bytes_ctrl_keys() {
        assert_eq!(key_to_bytes("ctrl+d"), vec![0x04]);
        assert_eq!(key_to_bytes("ctrl+u"), vec![0x15]);
        assert_eq!(key_to_bytes("ctrl+l"), vec![0x0c]);
    }

    #[test]
    fn key_to_bytes_unknown_falls_back_to_enter() {
        // Multiple chars → enter
        assert_eq!(key_to_bytes("unknown"), vec![b'\r']);
    }

    #[test]
    fn key_to_bytes_single_char() {
        assert_eq!(key_to_bytes("x"), b"x".to_vec());
        // Note: to_ascii_lowercase is applied, so "Z" becomes "z"
        assert_eq!(key_to_bytes("Z"), b"z".to_vec());
    }

    #[test]
    fn modifier_code_values() {
        assert_eq!(modifier_code("shift"), Some(2));
        assert_eq!(modifier_code("alt"), Some(3));
        assert_eq!(modifier_code("ctrl"), Some(5));
        assert_eq!(modifier_code("ctrl+shift"), Some(6));
        assert_eq!(modifier_code("ctrl+alt"), Some(7));
        assert_eq!(modifier_code("unknown"), None);
    }

    #[test]
    fn arrow_code_values() {
        assert_eq!(arrow_code("up"), Some((1, 'A')));
        assert_eq!(arrow_code("down"), Some((1, 'B')));
        assert_eq!(arrow_code("right"), Some((1, 'C')));
        assert_eq!(arrow_code("left"), Some((1, 'D')));
        assert_eq!(arrow_code("f1"), Some((11, '~')));
        assert_eq!(arrow_code("f12"), Some((24, '~')));
        assert_eq!(arrow_code("unknown"), None);
    }

    #[test]
    fn parse_modifier_key_valid() {
        let seq = parse_modifier_key("shift-up").unwrap();
        assert_eq!(seq, vec![0x1b, b'[', b'1', b';', b'2', b'A']);
    }

    #[test]
    fn parse_modifier_key_with_plus_separator() {
        let seq = parse_modifier_key("ctrl+home").unwrap();
        assert_eq!(seq, vec![0x1b, b'[', b'1', b';', b'5', b'H']);
    }

    #[test]
    fn parse_modifier_key_unknown_mod() {
        assert!(parse_modifier_key("super-up").is_none());
    }

    #[test]
    fn parse_modifier_key_unknown_arrow() {
        assert!(parse_modifier_key("shift-unknown").is_none());
    }

    #[test]
    fn secret_needle_strips_punctuation() {
        assert_eq!(secret_needle("Vault passphrase:"), "Vault passphrase");
        assert_eq!(secret_needle("Password? "), "Password");
        assert_eq!(secret_needle("  Token  "), "Token");
        assert_eq!(secret_needle("API key:"), "API key");
    }

    #[test]
    fn recent_contains_checks_recent_events() {
        let events = vec![
            (0.0, "line 1".into()),
            (0.1, "line 2".into()),
            (0.2, "secret prompt:".into()),
        ];
        assert!(recent_contains(&events, "secret"));
        assert!(!recent_contains(&events, "missing"));
    }

    #[test]
    fn recent_contains_only_checks_last_40() {
        let mut events: Vec<(f64, String)> =
            (0..50).map(|i| (i as f64, format!("event {i}"))).collect();
        events.push((50.0, "needle found".into()));
        assert!(recent_contains(&events, "needle"));
        assert!(!recent_contains(&events, "event 0"));
    }

    #[test]
    fn progress_bar_zero_to_hundred() {
        progress_bar("test", 0, 100);
        progress_bar("test", 50, 100);
        progress_bar("test", 100, 100);
        progress_bar("test", 1, 3);
    }

    #[test]
    fn progress_bar_zero_total() {
        progress_bar("test", 0, 0);
    }

    /// Progress counts every tick exactly: `+=`→`*=` sticks at 0, `+=`→`-=`
    /// panics, both die on the exact sequence.
    #[test]
    fn progress_tick_counts_each_frame() {
        let mut p = Progress::new("test", 3);
        assert_eq!(p.tick(), 1);
        assert_eq!(p.tick(), 2);
        assert_eq!(p.tick(), 3);
        assert_eq!(p.tick(), 4, "ticks keep counting past the total");
    }

    #[test]
    fn collect_secrets_without_secret_steps_prompts_nothing() {
        // No `secret` steps → the inquire password prompts are never built,
        // so this runs headless (no TTY) and yields an empty map.
        let score: Score = toml::from_str(
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
        )
        .unwrap();
        let secrets = collect_secrets(&score).unwrap();
        assert!(secrets.is_empty());
    }

    #[test]
    fn single_terminal_pane_one_terminal() {
        let score: Score = toml::from_str(
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
        )
        .unwrap();
        let pane = single_terminal_pane(&score).unwrap();
        assert_eq!(pane.id, "c");
    }

    #[test]
    fn single_terminal_pane_no_panes() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "t"
[layout]
width = 100
height = 100
"#,
        )
        .unwrap();
        let err = single_terminal_pane(&score).unwrap_err();
        assert!(format!("{err}").contains("no terminal pane"));
    }

    #[test]
    fn single_terminal_pane_with_browser() {
        let score: Score = toml::from_str(
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
  id = "b"
  type = "browser"
  x = 100
  y = 0
  width = 100
  height = 100
  url = "https://example.com"
"#,
        )
        .unwrap();
        let err = single_terminal_pane(&score).unwrap_err();
        assert!(format!("{err}").contains("browser"));
    }

    #[test]
    fn single_terminal_pane_multiple_terminals() {
        let score: Score = toml::from_str(
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
        )
        .unwrap();
        let err = single_terminal_pane(&score).unwrap_err();
        assert!(format!("{err}").contains("single"));
    }

    #[test]
    fn progress_clear_does_not_panic() {
        progress_clear();
    }

    #[test]
    fn sh_single_quote_empty_string() {
        assert_eq!(sh_single_quote(""), "''");
    }

    #[test]
    fn sh_single_quote_with_newlines() {
        assert_eq!(sh_single_quote("a\nb"), "'a\nb'");
    }

    #[test]
    fn screen_contains_finds_pattern() {
        let mut parser = VtParser::new(2, 80, 0);
        parser.process(b"Hello World\r\n");
        assert!(screen_contains(&parser, "Hello"));
        assert!(!screen_contains(&parser, "Goodbye"));
    }

    #[test]
    fn screen_contains_empty_screen() {
        let parser = VtParser::new(2, 80, 0);
        assert!(!screen_contains(&parser, "anything"));
    }

    /// A demo whose last command leaves a process in the foreground must not hang
    /// teardown: bounded shutdown caps it at the grace period plus the drain, not
    /// the command's lifetime.
    #[cfg(unix)]
    #[test]
    fn replay_does_not_hang_on_a_long_running_command() {
        let score: Score = toml::from_str(
            r#"
[demo]
name = "hang"
[layout]
width = 800
height = 400
fps = 10
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 400
[[timeline]]
action = "type"
text = "sleep 120\n"
[[timeline]]
action = "wait"
duration_ms = 200
"#,
        )
        .unwrap();

        let start = Instant::now();
        let rec = run_terminal(&score).expect("replay should return");
        let elapsed = start.elapsed();
        // Well under `sleep 120`: pre-roll + grace (2s) + drain (≤1s) + slack.
        assert!(
            elapsed < Duration::from_secs(10),
            "teardown hung: took {elapsed:?}"
        );
        assert_eq!(rec.cols, 80);
    }

    // --- mutant-killing tests: key delay arms, step dispatch, timeline stop,
    // screen-parser init ---

    /// ESC gets the long TUI settle; every other key gets the short one.
    /// `==`→`!=` and `||`→`&&` both misroute "esc"/"escape"/"a" here.
    #[test]
    fn key_delay_gives_esc_extra_settle_time() {
        assert_eq!(key_delay("esc"), 200);
        assert_eq!(key_delay("escape"), 200);
        assert_eq!(key_delay("a"), 60);
        assert_eq!(key_delay("enter"), 60);
        assert_eq!(key_delay("up"), 60);
        assert_eq!(key_delay("ESC"), 60, "matching is case-sensitive");
    }

    fn live_score(timeline: &str) -> Score {
        toml::from_str(&format!(
            r#"
[demo]
name = "live"
[layout]
width = 800
height = 400
  [[layout.panes]]
  id = "c"
  type = "terminal"
  x = 0
  y = 0
  width = 800
  height = 400
{timeline}"#,
        ))
        .unwrap()
    }

    fn live_runner<'a>(
        pty: &'a mut CapturePty,
        secrets: &'a std::collections::HashMap<String, String>,
        typing: crate::model::Typing,
        rng: Rng,
        events: Vec<(f64, String)>,
        t0: Instant,
    ) -> StepRunner<'a> {
        let (rows, cols) = (pty.rows, pty.cols);
        StepRunner {
            pty,
            secrets,
            typing,
            rng,
            events,
            captions: Vec::new(),
            focuses: Vec::new(),
            screen: ScreenWait {
                parser: None,
                fed: 0,
                rows,
                cols,
            },
            t0,
        }
    }

    /// apply_step dispatches Focus/Caption/Terminate: the first two record and
    /// continue, Terminate stops. `->false`/`->true` mutants die on the return.
    #[test]
    fn apply_step_dispatches_focus_caption_and_terminate() {
        let score = live_score("");
        let mut pty = CapturePty::new(&score, &score.layout.panes[0]).unwrap();
        let secrets = std::collections::HashMap::new();
        let t0 = Instant::now();
        let mut runner = live_runner(
            &mut pty,
            &secrets,
            crate::model::Typing::default(),
            Rng::new(0),
            Vec::new(),
            t0,
        );
        assert!(apply_step(
            &Step::Focus {
                pane: Some("docs".into())
            },
            &mut runner
        ));
        assert_eq!(
            runner.focuses,
            vec![(runner.focuses[0].0, "docs".to_string())]
        );
        assert!(apply_step(
            &Step::Caption { text: "hi".into() },
            &mut runner
        ));
        assert_eq!(runner.captions.len(), 1);
        assert_eq!(runner.captions[0].1, "hi");
        assert!(!apply_step(&Step::Terminate, &mut runner));
    }

    /// drive_timeline stops at Terminate: later steps never run, and the run
    /// still produces its captures (not a default). Kills `->Default` and the
    /// `!` on the stop gate.
    #[test]
    fn drive_timeline_stops_at_terminate() {
        let score = live_score(
            r#"
[[timeline]]
action = "focus"
pane = "first"
[[timeline]]
action = "terminate"
[[timeline]]
action = "focus"
pane = "never"
"#,
        );
        let mut pty = CapturePty::new(&score, &score.layout.panes[0]).unwrap();
        let secrets = std::collections::HashMap::new();
        let caps = drive_timeline(&score, &mut pty, &secrets, Instant::now());
        let panes: Vec<&str> = caps.focuses.iter().map(|(_, p)| p.as_str()).collect();
        assert_eq!(panes, vec!["first"], "steps after Terminate must not run");
    }

    /// wait_for_screen with the pattern already in events returns with the
    /// parser initialized and fed — without touching the channel. `with ()`
    /// leaves the parser None and dies here.
    #[test]
    fn wait_for_screen_initializes_parser_from_prior_events() {
        let mut events = vec![(0.0, "hello world".to_string())];
        let (_tx, rx) = std::sync::mpsc::channel::<(Instant, Vec<u8>)>();
        let mut screen = ScreenWait {
            parser: None,
            fed: 0,
            rows: 24,
            cols: 80,
        };
        wait_for_screen("hello", &mut events, &rx, Instant::now(), &mut screen, 500);
        assert!(screen.parser.is_some(), "parser must be initialized");
        assert_eq!(screen.fed, events.len());
    }
}
