//! The two I/O threads of a capture — the *bridge* between the real terminal
//! and the shell running in the PTY — plus the byte-level helpers they share:
//! streaming UTF-8 decoding, keystroke routing, and secret-prompt detection.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Instant;

use crate::model::RawEvent;

use super::{ms, CaptureState, PROMPT_READY};

/// Track the current terminal line and detect a secret prompt at each line
/// boundary (`\r` redraw or `\n`) — crucially BEFORE the boundary clears the line.
/// `inquire` emits the prompt immediately followed by `\r` (`Vault passphrase:\r…`),
/// so checking only at the chunk end (after the `\r` cleared it) missed it and the
/// secret leaked. Latches `sensitive` and records the prompt label when found.
/// When a completed non-secret line is seen, sets `secret_prompt_cleared` to
/// signal the input thread that the dedup guard can be cleared. Only set on
/// completed lines (at `\n`/`\r`), not on partial lines at chunk boundaries,
/// to avoid spurious clears when a prompt label is split across PTY reads.
fn track_and_detect(
    line: &mut String,
    text: &str,
    sensitive: &AtomicBool,
    secret_prompt: &Mutex<Option<String>>,
    secret_prompt_cleared: &AtomicBool,
) {
    let detect_secret = |line: &str| {
        if is_secret_prompt(line) {
            sensitive.store(true, Ordering::SeqCst);
            *secret_prompt.lock().unwrap() = Some(clean_prompt(line));
        }
    };
    let mark_cleared = |line: &str| {
        if !is_secret_prompt(line) && !line.is_empty() {
            secret_prompt_cleared.store(true, Ordering::SeqCst);
        }
    };
    for ch in text.chars() {
        match ch {
            '\n' | '\r' => {
                detect_secret(line);
                mark_cleared(line);
                line.clear();
            }
            c if c.is_control() => {}
            c => {
                // Full-screen TUIs paint without newlines, so the "line" can grow
                // without bound. A real secret prompt is short — past the cap this
                // is screen paint, not a prompt; stop growing (is_secret_prompt
                // rejects anything this long anyway).
                if line.len() < MAX_PROMPT_LINE {
                    line.push(c);
                }
            }
        }
    }
    // The prompt may sit at the chunk end with no trailing newline yet.
    // Only detect secrets here; don't set the cleared flag on partial lines.
    detect_secret(line);
}

/// Longest a terminal line can be and still count as a secret prompt. Real
/// prompts ("Vault passphrase:") are far shorter; anything bigger is a TUI
/// repainting the screen without newlines.
const MAX_PROMPT_LINE: usize = 256;

/// Tidy a captured prompt line into a label for `demo record` to show: drop ANSI
/// CSI residue left after the ESC was stripped (`[..m` colours, `[?25h` cursor
/// codes, etc.) and the leading prompt glyphs (`? > ◆ ●`), leaving e.g.
/// `Vault passphrase:`.
fn clean_prompt(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '[' {
            // A CSI residue (ESC already stripped): `[` then params `0-9;?` then a
            // letter final byte. Drop it; otherwise keep the literal `[`.
            let mut params = String::new();
            let mut final_letter = None;
            while let Some(&n) = chars.peek() {
                if n.is_ascii_digit() || n == ';' || n == '?' {
                    params.push(n);
                    chars.next();
                } else if n.is_ascii_alphabetic() {
                    final_letter = Some(n);
                    chars.next();
                    break;
                } else {
                    break;
                }
            }
            if final_letter.is_some() {
                continue;
            }
            out.push('[');
            out.push_str(&params);
        } else {
            out.push(c);
        }
    }
    out.trim()
        .trim_start_matches(['?', '>', '◆', '●', '*', ' '])
        .trim()
        .to_string()
}

/// Decide whether a submitted secret prompt should produce a `Secret` event.
/// Returns true when the caller must record one.
///
/// `prompt_left_screen` is true when the output thread has observed a completed
/// non-secret line since the last submission, meaning the prompt has left the
/// screen and the dedup guard should be cleared before checking.
///
/// The dedup covers only redraws of the prompt currently being answered: once
/// `prompt_left_screen` is true the guard is cleared, so the same prompt text
/// detected later records a new event. Two consecutive detections of the same
/// prompt with no submission in between still collapse into one.
fn secret_step_on_submit(
    last_secret_prompt: &mut Option<String>,
    prompt_left_screen: bool,
    prompt: &str,
) -> bool {
    if prompt_left_screen {
        *last_secret_prompt = None;
    }
    let is_dup = last_secret_prompt.as_ref().map(|s| s.as_str()) == Some(prompt);
    if !is_dup {
        *last_secret_prompt = Some(prompt.to_string());
        true
    } else {
        false
    }
}

/// Heuristic: does this line look like a program prompting for a secret? Matches
/// a secret keyword on a line that ends like a prompt (`:` or `?`), so a typed
/// command that merely mentions "password" is not mistaken for a prompt. Long
/// lines are rejected outright — they're TUI screen paint, not prompts.
fn is_secret_prompt(line: &str) -> bool {
    if line.len() > 200 {
        return false;
    }
    let lower = line.to_ascii_lowercase();
    let trimmed = lower.trim_end();
    if !(trimmed.ends_with(':') || trimmed.ends_with('?')) {
        return false;
    }
    const HINTS: [&str; 10] = [
        "password",
        "passphrase",
        "passcode",
        "secret",
        "[sudo]",
        "verification code",
        "token",
        "api key",
        "access key",
        "credential",
    ];
    HINTS.iter().any(|h| trimmed.contains(h))
}

/// Decode PTY bytes to text across read boundaries. `pending` holds bytes left
/// over from a previous chunk that ended mid-sequence; new `bytes` are appended,
/// the longest valid UTF-8 prefix is returned, and any incomplete trailing
/// sequence is kept in `pending` for the next call. Genuinely invalid bytes are
/// replaced with `U+FFFD` so a bad byte can't stall the stream. Without this, a
/// multi-byte glyph split across two reads (dense braille from `mapscii`) would
/// be corrupted in the recording even though the live terminal looks right.
fn decode_streaming(pending: &mut Vec<u8>, bytes: &[u8]) -> String {
    pending.extend_from_slice(bytes);
    let mut out = String::new();
    // Bounded passes: every `Some(bad)` pass drains at least one byte, so one
    // pass per buffered byte always covers the whole buffer, and the clean
    // remainder is flushed after the budget. The pass count is fixed up front
    // so the loop cannot be steered into spinning by its own drain.
    let budget = pending.len();
    for _ in 0..budget {
        match std::str::from_utf8(pending) {
            Ok(s) => {
                out.push_str(s);
                pending.clear();
                return out;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY: `valid_up_to` is the length of a checked valid prefix.
                out.push_str(unsafe { std::str::from_utf8_unchecked(&pending[..valid]) });
                match e.error_len() {
                    // An invalid byte (not merely incomplete): emit a replacement
                    // and skip past it, then keep decoding the remainder.
                    Some(bad) => {
                        out.push('\u{FFFD}');
                        pending.drain(..valid + bad);
                    }
                    // Incomplete sequence at the end: hold it for the next read.
                    None => {
                        pending.drain(..valid);
                        return out;
                    }
                }
            }
        }
    }
    // Budget spent: every pass consumed a byte, so whatever decodes cleanly
    // now (usually nothing) is flushed in one last step.
    if let Ok(s) = std::str::from_utf8(pending) {
        out.push_str(s);
        pending.clear();
    }
    out
}

/// Length in bytes of a UTF-8 sequence given its leading byte. A stray
/// continuation byte (`0x80..=0xbf`, not a valid lead) is treated as length 1.
fn utf8_len(b: u8) -> usize {
    if b < 0xc0 {
        1
    } else if b < 0xe0 {
        2
    } else if b < 0xf0 {
        3
    } else {
        4
    }
}

/// What routing a chunk of keystrokes implies for the recorder. Input is passed
/// straight through to the PTY (so the shell echoes it — no hidden commands); we
/// only *watch* the typed line to know when a demo meta-command needs muting or
/// when an `--after` reveal should be armed.
struct RouteOutcome {
    to_pty: Vec<u8>,
    /// A `demo open`/`demo stop`/`demo focus` was just entered — its echo and any
    /// wizard/confirmation must be excised from the recording.
    mute_command: bool,
}

/// Forward a keystroke chunk to the PTY and track the current command line so the
/// caller can mute demo meta-commands and arm `--after` reveals. Everything typed
/// reaches the shell verbatim (and is echoed by it) — control now lives in the
/// top-level `demo stop`/`demo focus`/`demo open` commands, not hidden keystrokes.
fn route_input_chunk(
    chunk: &[u8],
    cmd_line: &mut String,
    cmd_start: &mut Option<u64>,
    now: u64,
) -> RouteOutcome {
    let mut to_pty: Vec<u8> = Vec::with_capacity(chunk.len());
    let mut mute_command = false;
    let mut i = 0;
    let n = chunk.len();
    // Mark the start of a fresh command line at its first printable char, so a
    // muted meta-command span covers the whole echoed line, not just its Enter.
    let mark_start = |cmd_line: &String, cmd_start: &mut Option<u64>| {
        if cmd_line.is_empty() {
            *cmd_start = Some(now);
        }
    };
    // Bounded: one pass per input byte at most, so the walk always ends even
    // if a branch fails to advance `i`; the read below stops the pass when the
    // chunk is consumed.
    for _ in 0..n {
        let Some(&b) = chunk.get(i) else {
            break;
        };
        if b == b'\r' || b == b'\n' {
            to_pty.push(b);
            let t = cmd_line.trim_start();
            if is_meta_command(t) {
                mute_command = true;
            }
            cmd_line.clear();
            *cmd_start = None;
            i += 1;
            continue;
        }
        if b == 0x7f {
            to_pty.push(b);
            cmd_line.pop();
            i += 1;
            continue;
        }
        if b < 0x20 {
            to_pty.push(b);
            i += 1;
            continue;
        }
        if b >= 0x80 {
            let seq_len = utf8_len(b);
            let end = (i + seq_len).min(n);
            to_pty.extend_from_slice(&chunk[i..end]);
            if let Ok(s) = std::str::from_utf8(&chunk[i..end]) {
                if let Some(ch) = s.chars().next() {
                    mark_start(cmd_line, cmd_start);
                    cmd_line.push(ch);
                }
            }
            i = end;
            continue;
        }
        mark_start(cmd_line, cmd_start);
        to_pty.push(b);
        cmd_line.push(b as char);
        i += 1;
    }
    RouteOutcome {
        to_pty,
        mute_command,
    }
}

/// Does this typed line invoke a `demo` control command whose echo + wizard must
/// stay out of the recording?
fn is_meta_command(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("demo open") || t.starts_with("demo stop") || t.starts_with("demo focus")
}

/// Does the recent output match a `--when` cue? A `re:` prefix is a regular
/// expression; otherwise it's a plain substring match.
fn cue_matches(recent: &str, pattern: &str) -> bool {
    if let Some(rx) = pattern.strip_prefix("re:") {
        match regex::Regex::new(rx) {
            Ok(re) => re.is_match(recent),
            // A bad pattern never matches (rather than firing spuriously).
            Err(_) => false,
        }
    } else {
        recent.contains(pattern)
    }
}

/// PTY → stdout, recorded as output events. Returns the thread handle so the
/// session can join it after the child is killed (EOF).
pub(super) fn spawn_output_pump(
    state: &CaptureState,
    reader: Box<dyn Read + Send>,
    t0: Instant,
) -> thread::JoinHandle<()> {
    let mut pump = OutputPump::new(state.clone(), reader, t0);
    thread::spawn(move || pump.run())
}

/// stdin → PTY, recorded as input events. (Detached: stdin reads block; the
/// process exits once recording stops, which tears this down.)
pub(super) fn spawn_input_pump(
    state: &CaptureState,
    writer: Box<dyn Write + Send>,
    t0: Instant,
) -> thread::JoinHandle<()> {
    let mut pump = InputPump::new(state.clone(), writer, t0);
    thread::spawn(move || pump.run())
}

/// The PTY→stdout half of the bridge, holding the buffers that carry across
/// reads: the partial UTF-8 sequence, the partial prompt line, the pre-roll
/// output and the recent-output window used for `--when` cue matching.
struct OutputPump {
    state: CaptureState,
    reader: Box<dyn Read + Send>,
    stdout: std::io::Stdout,
    t0: Instant,
    /// Current terminal line, for secret-prompt detection.
    line: String,
    /// Rolling recent output, for matching `demo open --when` cues across
    /// chunk/line boundaries (a per-line check misses a cue then a newline).
    recent: String,
    /// Pre-roll buffer: output before the readiness marker (prompt setup).
    pre: String,
    /// Trailing bytes of an incomplete UTF-8 sequence, carried to the next
    /// read: a PTY read can split a multi-byte glyph (e.g. braille from
    /// mapscii) across the 4 KiB boundary, and decoding each chunk in
    /// isolation would corrupt it into replacement chars in the recording.
    pending: Vec<u8>,
}

impl OutputPump {
    fn new(state: CaptureState, reader: Box<dyn Read + Send>, t0: Instant) -> Self {
        Self {
            state,
            reader,
            stdout: std::io::stdout(),
            t0,
            line: String::new(),
            recent: String::new(),
            pre: String::new(),
            pending: Vec::new(),
        }
    }

    /// Read the PTY until EOF, echoing each chunk to the real terminal.
    fn run(&mut self) {
        let mut buf = [0u8; 4096];
        loop {
            let n = match self.reader.read(&mut buf) {
                Ok(0) => break,
                // A signal (e.g. SIGWINCH on resize) interrupts the blocking
                // read; that is not the shell exiting — retry, don't stop.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
                Ok(n) => n,
            };
            self.feed(&buf[..n]);
        }
        self.state.shell_exited.store(true, Ordering::SeqCst);
    }

    /// Echo one chunk to the local terminal and record it, unless it belongs to
    /// the pre-roll (prompt setup) or to a muted meta-command.
    fn feed(&mut self, bytes: &[u8]) {
        let _ = self.stdout.write_all(bytes);
        let _ = self.stdout.flush();
        *self.state.last_activity.lock().unwrap() = Instant::now();
        if let Some(d) = &self.state.debug {
            d.chunk("OUT", bytes);
        }
        // Decode across the read boundary: keep any incomplete trailing
        // sequence in `pending` for the next chunk.
        let text = decode_streaming(&mut self.pending, bytes);
        // Detect the secret prompt at each line boundary and LATCH the flag
        // (only set here; the input thread clears it on Enter), so the masked
        // redraws can't unset it mid-secret.
        track_and_detect(
            &mut self.line,
            &text,
            &self.state.sensitive,
            &self.state.secret_prompt,
            &self.state.secret_prompt_cleared,
        );
        // Pre-roll: discard prompt-setup output until the readiness marker;
        // record only what follows it (the clean prompt).
        if !self.state.ready.load(Ordering::SeqCst) {
            self.absorb_preroll(&text);
            return;
        }
        // While a typed `demo open`/`demo stop` is running, drop its output
        // entirely (no record, no cue-matching) so it never appears in the demo.
        if self.state.muting.load(Ordering::SeqCst) {
            return;
        }
        self.record_output(text);
    }

    /// Buffer pre-roll output until the readiness marker shows up; everything
    /// after the marker is the first recorded output of the demo.
    fn absorb_preroll(&mut self, text: &str) {
        self.pre.push_str(text);
        let Some(idx) = self.pre.find(PROMPT_READY) else {
            return;
        };
        let after = self.pre[idx + PROMPT_READY.len()..].to_string();
        self.pre.clear();
        self.state.ready.store(true, Ordering::SeqCst);
        if after.is_empty() {
            return;
        }
        self.recent.push_str(&after);
        self.state.events.lock().unwrap().push(RawEvent::Output {
            t_ms: ms(self.t0),
            data: after,
        });
    }

    /// Record one output chunk that passed the pre-roll and mute gates, firing
    /// any `--when` reveal its text just matched.
    fn record_output(&mut self, text: String) {
        let now = ms(self.t0);
        self.recent.push_str(&text);
        // An `--after` command is in flight → note this output, so the watchdog
        // can fire once the stream goes quiet again.
        if self.state.after_running.load(Ordering::SeqCst) {
            *self.state.after_last_out.lock().unwrap() = Instant::now();
        }
        self.fire_due_reveals(now);
        if self.recent.len() > 8192 {
            self.recent.clear();
        }
        self.state.events.lock().unwrap().push(RawEvent::Output {
            t_ms: now,
            data: text,
        });
    }

    /// Fire any `--when <pat>` reveal whose cue just appeared in the recent
    /// output window.
    fn fire_due_reveals(&self, now: u64) {
        let mut pend = self.state.pending_opens.lock().unwrap();
        if pend.is_empty() {
            return;
        }
        let mut evs = self.state.events.lock().unwrap();
        let recent = &self.recent;
        pend.retain(|(r, pat)| {
            if cue_matches(recent, pat) {
                evs.push(r.to_event(now));
                false
            } else {
                true
            }
        });
    }
}

/// The stdin→PTY half of the bridge, holding the typed-line state the recorder
/// needs to mute meta-commands and to dedup secret-prompt submissions.
struct InputPump {
    state: CaptureState,
    writer: Box<dyn Write + Send>,
    t0: Instant,
    /// The command line being typed, so meta-commands can be recognised.
    cmd_line: String,
    /// When the current input line started (first char), so a `demo open`/
    /// `demo stop`/`demo focus` span can be excised from the command's echo,
    /// not just from the Enter that follows it.
    cmd_start: Option<u64>,
    /// The prompt text of the secret last submitted, for dedup.
    last_secret_prompt: Option<String>,
}

impl InputPump {
    fn new(state: CaptureState, writer: Box<dyn Write + Send>, t0: Instant) -> Self {
        Self {
            state,
            writer,
            t0,
            cmd_line: String::new(),
            cmd_start: None,
            last_secret_prompt: None,
        }
    }

    /// Read stdin until EOF/stop, routing each chunk through [`Self::handle`].
    fn run(&mut self) {
        let mut buf = [0u8; 1024];
        let mut stdin = std::io::stdin();
        while !self.state.stop.load(Ordering::SeqCst) {
            let n = match stdin.read(&mut buf) {
                Ok(0) => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
                Ok(n) => n,
            };
            if self.handle(&buf[..n]) {
                break;
            }
        }
    }

    /// Route one stdin chunk: watch the typed line, forward everything to the
    /// PTY verbatim, and record it unless a secret prompt, a mute or the
    /// pre-roll says otherwise. Returns true when the PTY write failed and the
    /// read loop must stop.
    fn handle(&mut self, chunk: &[u8]) -> bool {
        *self.state.last_activity.lock().unwrap() = Instant::now();
        // Don't record input during the prompt-setup pre-roll.
        if !self.state.ready.load(Ordering::SeqCst) {
            return false;
        }
        let saved_cmd_start = self.cmd_start;
        let outcome =
            route_input_chunk(chunk, &mut self.cmd_line, &mut self.cmd_start, ms(self.t0));
        if outcome.mute_command {
            *self.state.mute_since.lock().unwrap() = Instant::now();
            self.state.muting.store(true, Ordering::SeqCst);
            let mut s = self.state.mute_start.lock().unwrap();
            if s.is_none() {
                *s = Some(saved_cmd_start.unwrap_or_else(|| ms(self.t0)));
            }
        }
        if !outcome.to_pty.is_empty() {
            if self.writer.write_all(&outcome.to_pty).is_err() {
                return true;
            }
            let _ = self.writer.flush();
        }
        // Secret prompt active → forward the keystrokes but do NOT record them;
        // clear once the prompt is answered (Enter).
        let masked = self.state.sensitive.load(Ordering::SeqCst);
        if let Some(d) = &self.state.debug {
            if masked {
                d.note(&format!(
                    "IN* {} bytes (redacted — secret prompt)",
                    chunk.len()
                ));
            } else {
                d.chunk("IN ", &outcome.to_pty);
            }
        }
        if masked {
            self.record_secret_submission(&outcome.to_pty);
            return false;
        }
        // Don't record input while a meta-command (demo open/stop) is running —
        // its wizard answers must not enter the demo.
        if self.state.muting.load(Ordering::SeqCst) {
            return false;
        }
        if outcome.to_pty.is_empty() {
            return false;
        }
        self.state.events.lock().unwrap().push(RawEvent::Input {
            t_ms: ms(self.t0),
            bytes: String::from_utf8_lossy(&outcome.to_pty).into_owned(),
        });
        false
    }

    /// While a secret prompt is being answered: Enter submits it, so record one
    /// `Secret` event (the prompt text, never the value) unless the same prompt
    /// was already recorded and has not left the screen since.
    fn record_secret_submission(&mut self, to_pty: &[u8]) {
        if !to_pty.iter().any(|b| *b == b'\r' || *b == b'\n') {
            return;
        }
        self.state.sensitive.store(false, Ordering::SeqCst);
        let prompt_left_screen = self
            .state
            .secret_prompt_cleared
            .swap(false, Ordering::SeqCst);
        let prompt = self
            .state
            .secret_prompt
            .lock()
            .unwrap()
            .take()
            .unwrap_or_default();
        if secret_step_on_submit(&mut self.last_secret_prompt, prompt_left_screen, &prompt) {
            self.state.events.lock().unwrap().push(RawEvent::Secret {
                t_ms: ms(self.t0),
                prompt,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_secret_at_a_carriage_return_boundary() {
        // inquire emits the prompt immediately followed by `\r` and cursor codes —
        // detection must fire on the prompt BEFORE the `\r` clears the line.
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        // `\x1b` (ESC) is a control char; `[?25h` is the residue after it.
        track_and_detect(
            &mut line,
            " Vault passphrase:\r\x1b[?25h",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(
            sensitive.load(Ordering::SeqCst),
            "should latch on the prompt"
        );
        assert_eq!(
            secret_prompt.lock().unwrap().as_deref(),
            Some("Vault passphrase:")
        );
    }

    #[test]
    fn clean_prompt_strips_colour_and_cursor_codes() {
        // After ESC is stripped, both `[38;5;10m` colour and `[?25l` cursor codes
        // remain — clean_prompt removes them and the leading glyph.
        assert_eq!(
            clean_prompt("[?25h[?25l> [38;5;10mVault passphrase:[39m"),
            "Vault passphrase:"
        );
    }

    #[test]
    fn flags_secret_prompts_only() {
        assert!(is_secret_prompt(
            "Enter passphrase for key '/home/u/.ssh/id_ed25519':"
        ));
        assert!(is_secret_prompt("Password: "));
        assert!(is_secret_prompt("[sudo] password for jheison:"));
        assert!(is_secret_prompt("Vault passphrase?"));
        assert!(!is_secret_prompt("$ echo my secret plan"));
        assert!(!is_secret_prompt("Cloning into 'repo'..."));
        assert!(!is_secret_prompt("Refreshing access token cache"));
        assert!(!is_secret_prompt("Vault passphrase: ***"));
    }

    #[test]
    fn tui_screen_paint_never_matches_as_a_secret_prompt() {
        // A full-screen TUI (opencode, vim, …) repaints without newlines, so the
        // tracked "line" is huge even if it happens to contain "token …:". That
        // must never latch the secret redactor (it used to capture ~450KB of
        // screen paint as the prompt label).
        let huge = format!("{} tokens used - Context:", "x".repeat(5000));
        assert!(!is_secret_prompt(&huge));

        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        track_and_detect(
            &mut line,
            &huge,
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(!sensitive.load(Ordering::SeqCst));
        assert!(secret_prompt.lock().unwrap().is_none());
        // And the tracker's memory stays bounded while the paint streams on.
        assert!(line.len() <= MAX_PROMPT_LINE);
    }

    #[test]
    fn decode_streaming_reassembles_a_split_braille_glyph() {
        // U+2839 (⠹) is 3 bytes: e2 a0 b9. Split it across two reads, as a PTY
        // can at a 4 KiB boundary, and it must reassemble — not corrupt.
        let glyph = "⠹";
        let bytes = glyph.as_bytes();
        let mut pending = Vec::new();
        let first = decode_streaming(&mut pending, &bytes[..2]);
        assert_eq!(first, "", "an incomplete sequence yields nothing yet");
        let second = decode_streaming(&mut pending, &bytes[2..]);
        assert_eq!(second, glyph, "the rest completes the glyph intact");
        assert!(pending.is_empty());
    }

    #[test]
    fn decode_streaming_replaces_a_truly_invalid_byte() {
        // A lone 0xFF is invalid UTF-8 — it must become U+FFFD, not stall.
        let mut pending = Vec::new();
        let out = decode_streaming(&mut pending, &[b'a', 0xff, b'b']);
        assert_eq!(out, "a\u{FFFD}b");
        assert!(pending.is_empty());
    }

    /// Route a sequence of read chunks, returning the bytes forwarded to the PTY
    /// and whether a `demo` meta-command was seen (so its echo gets muted).
    fn route(input: &[&[u8]]) -> (Vec<u8>, bool) {
        let mut cmd_line = String::new();
        let mut cmd_start: Option<u64> = None;
        let mut to_pty = Vec::new();
        let mut mute = false;
        for chunk in input {
            let o = route_input_chunk(chunk, &mut cmd_line, &mut cmd_start, 0);
            to_pty.extend(o.to_pty);
            mute |= o.mute_command;
        }
        (to_pty, mute)
    }

    #[test]
    fn everything_typed_reaches_the_pty_and_is_echoed() {
        // No hidden commands anymore: whatever you type is forwarded verbatim so
        // the shell echoes it. A leading `/` is just a normal character now.
        let (to_pty, mute) = route(&[b"/stop\r"]);
        assert_eq!(to_pty, b"/stop\r");
        assert!(!mute);
    }

    #[test]
    fn regular_command_still_works() {
        let (to_pty, mute) = route(&[b"ls -la\n"]);
        assert_eq!(to_pty, b"ls -la\n");
        assert!(!mute);
    }

    #[test]
    fn meta_command_is_flagged_for_muting() {
        // `demo focus`/`demo open`/`demo stop` typed in-session must mute so their
        // echo and wizard never reach the recording — even split across reads.
        assert!(route(&[b"demo focus fill-main\n"]).1);
        assert!(route(&[b"demo ", b"open ", b"github.com\r"]).1);
        assert!(route(&[b"demo stop\n"]).1);
        assert!(
            !route(&[b"demodocs\n"]).1,
            "a lookalike command must not mute"
        );
    }

    #[test]
    fn backspace_is_forwarded_and_erases_cmd_line() {
        let (to_pty, _) = route(&[b"ab\x7f\n"]);
        assert_eq!(to_pty, b"ab\x7f\n");
    }

    #[test]
    fn utf8_round_trip_through_routing() {
        let (to_pty, _) = route(&["héllo\n".as_bytes()]);
        assert_eq!(to_pty, "héllo\n".as_bytes());
    }

    #[test]
    fn utf8_len_returns_correct_sequence_lengths() {
        assert_eq!(utf8_len(0x00), 1);
        assert_eq!(utf8_len(0x7f), 1);
        assert_eq!(utf8_len(0x80), 1);
        assert_eq!(utf8_len(0xbf), 1);
        assert_eq!(utf8_len(0xc0), 2);
        assert_eq!(utf8_len(0xdf), 2);
        assert_eq!(utf8_len(0xe0), 3);
        assert_eq!(utf8_len(0xef), 3);
        assert_eq!(utf8_len(0xf0), 4);
        assert_eq!(utf8_len(0xf4), 4);
    }

    #[test]
    fn is_meta_command_matches_demo_stop_open_focus() {
        assert!(is_meta_command("demo stop"));
        assert!(is_meta_command("demo open http://example.com"));
        assert!(is_meta_command("demo focus main"));
        assert!(is_meta_command("  demo stop"));
        assert!(!is_meta_command("echo demo stop"));
        assert!(!is_meta_command("ls"));
        assert!(!is_meta_command(""));
    }

    #[test]
    fn cue_matches_plain_substring() {
        assert!(cue_matches("Report generated successfully.", "Report"));
        assert!(!cue_matches("Report generated", "Error"));
    }

    #[test]
    fn cue_matches_regex_with_prefix() {
        assert!(cue_matches("done in 123ms", "re:\\d+ms"));
        assert!(!cue_matches("no numbers", "re:\\d+ms"));
        assert!(!cue_matches("anything", "re:[invalid"));
    }

    #[test]
    fn decode_streaming_valid_utf8_passthrough() {
        let mut pending = Vec::new();
        let out = decode_streaming(&mut pending, "hello".as_bytes());
        assert_eq!(out, "hello");
        assert!(pending.is_empty());
    }

    #[test]
    fn decode_streaming_empty_input() {
        let mut pending = Vec::new();
        let out = decode_streaming(&mut pending, &[]);
        assert_eq!(out, "");
        assert!(pending.is_empty());
    }

    #[test]
    fn decode_streaming_only_incomplete() {
        let mut pending = Vec::new();
        // 2-byte lead (0xc2) without continuation
        let out = decode_streaming(&mut pending, &[0xc2]);
        assert_eq!(out, "");
        assert_eq!(pending, vec![0xc2]);
        // Now complete it
        let out2 = decode_streaming(&mut pending, &[0xa9]);
        assert_eq!(out2, "\u{00a9}"); // ©
        assert!(pending.is_empty());
    }

    #[test]
    fn is_meta_command_edge_cases() {
        assert!(is_meta_command("demo stop"));
        assert!(is_meta_command("demo open https://example.com"));
        assert!(is_meta_command("demo focus main"));
        assert!(!is_meta_command("echo demo stop"));
        assert!(!is_meta_command("ls demo"));
        assert!(!is_meta_command(""));
        assert!(!is_meta_command("demo"));
        assert!(!is_meta_command("demo "));
    }

    #[test]
    fn clean_prompt_basic() {
        assert_eq!(clean_prompt("Password:"), "Password:");
        assert_eq!(clean_prompt("  Password:  "), "Password:");
    }

    #[test]
    fn utf8_len_ascii() {
        assert_eq!(utf8_len(b'A'), 1);
        assert_eq!(utf8_len(b' '), 1);
        assert_eq!(utf8_len(b'0'), 1);
    }

    #[test]
    fn decode_streaming_multiple_chunks() {
        let mut pending = Vec::new();
        let out1 = decode_streaming(&mut pending, "hel".as_bytes());
        assert_eq!(out1, "hel");
        let out2 = decode_streaming(&mut pending, "lo\n".as_bytes());
        assert_eq!(out2, "lo\n");
        assert!(pending.is_empty());
    }

    #[test]
    fn cue_matches_empty_pattern() {
        assert!(cue_matches("anything", ""));
    }

    #[test]
    fn clean_prompt_strips_question_mark_prefix() {
        assert_eq!(clean_prompt("? Password:"), "Password:");
    }

    #[test]
    fn clean_prompt_strips_arrow_prefix() {
        assert_eq!(clean_prompt("> Enter secret:"), "Enter secret:");
    }

    #[test]
    fn clean_prompt_strips_diamond_prefix() {
        assert_eq!(clean_prompt("◆ Token:"), "Token:");
    }

    #[test]
    fn clean_prompt_strips_bullet_prefix() {
        assert_eq!(clean_prompt("● API key:"), "API key:");
    }

    #[test]
    fn clean_prompt_strips_asterisk_prefix() {
        assert_eq!(clean_prompt("* Secret:"), "Secret:");
    }

    #[test]
    fn clean_prompt_preserves_literal_bracket_without_final_letter() {
        // A `[` followed by digits but no final letter is NOT a CSI — keep it.
        assert_eq!(clean_prompt("item[1]"), "item[1]");
    }

    #[test]
    fn clean_prompt_empty_string() {
        assert_eq!(clean_prompt(""), "");
    }

    #[test]
    fn is_secret_prompt_rejects_long_lines() {
        let long = format!("{}:", "x".repeat(300));
        assert!(!is_secret_prompt(&long));
    }

    #[test]
    fn is_secret_prompt_rejects_no_colon_or_question() {
        assert!(!is_secret_prompt("Password"));
        assert!(!is_secret_prompt("secret"));
    }

    #[test]
    fn is_secret_prompt_accepts_question_mark() {
        assert!(is_secret_prompt("Enter passphrase?"));
    }

    #[test]
    fn is_secret_prompt_accepts_token() {
        assert!(is_secret_prompt("Token:"));
    }

    #[test]
    fn is_secret_prompt_accepts_api_key() {
        assert!(is_secret_prompt("API key:"));
    }

    #[test]
    fn is_secret_prompt_accepts_access_key() {
        assert!(is_secret_prompt("Access key:"));
    }

    #[test]
    fn is_secret_prompt_accepts_credential() {
        assert!(is_secret_prompt("Credential:"));
    }

    #[test]
    fn is_secret_prompt_accepts_verification_code() {
        assert!(is_secret_prompt("Verification code:"));
    }

    #[test]
    fn is_secret_prompt_rejects_case_insensitive() {
        // Should match regardless of case
        assert!(is_secret_prompt("PASSWORD:"));
        assert!(is_secret_prompt("PASSPHRASE:"));
    }

    #[test]
    fn is_secret_prompt_rejects_tui_painting() {
        // TUI paint without colon/question
        assert!(!is_secret_prompt("some random text without prompt marker"));
    }

    #[test]
    fn track_and_detect_multiple_lines() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        track_and_detect(
            &mut line,
            "hello\nworld\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(!sensitive.load(Ordering::SeqCst));
        assert_eq!(line, "");
    }

    #[test]
    fn track_and_detect_partial_line() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        track_and_detect(
            &mut line,
            "partial",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(!sensitive.load(Ordering::SeqCst));
        assert_eq!(line, "partial");
    }

    #[test]
    fn track_and_detect_csi_residue_in_line() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Vault passphrase:\r\x1b[?25h",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(sensitive.load(Ordering::SeqCst));
    }

    #[test]
    fn track_and_detect_control_chars_ignored() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        track_and_detect(
            &mut line,
            "abc\x01\x02\x03def",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert_eq!(line, "abcdef");
    }

    #[test]
    fn cue_matches_regex_prefix() {
        assert!(cue_matches("done in 123ms", "re:\\d+ms"));
        assert!(!cue_matches("no numbers", "re:\\d+ms"));
    }

    #[test]
    fn cue_matches_regex_invalid_is_ignored() {
        // Invalid regex should not match
        assert!(!cue_matches("anything", "re:[invalid"));
    }

    #[test]
    fn cue_matches_substring() {
        assert!(cue_matches("Report generated successfully.", "Report"));
        assert!(!cue_matches("Report generated", "Error"));
    }

    #[test]
    fn is_meta_command_demo_focus_with_args() {
        assert!(is_meta_command("demo focus main docs"));
    }

    #[test]
    fn is_meta_command_demo_open_with_url() {
        assert!(is_meta_command("demo open http://example.com"));
    }

    #[test]
    fn is_meta_command_demo_stop() {
        assert!(is_meta_command("demo stop"));
    }

    #[test]
    fn is_meta_command_leading_whitespace() {
        assert!(is_meta_command("  demo stop"));
    }

    #[test]
    fn is_meta_command_not_a_command() {
        assert!(!is_meta_command("echo demo stop"));
    }

    #[test]
    fn is_meta_command_empty() {
        assert!(!is_meta_command(""));
    }

    #[test]
    fn is_meta_command_just_demo() {
        assert!(!is_meta_command("demo"));
    }

    #[test]
    fn is_meta_command_demo_space() {
        assert!(!is_meta_command("demo "));
    }

    #[test]
    fn utf8_len_continuation_byte() {
        assert_eq!(utf8_len(0x80), 1);
    }

    #[test]
    fn utf8_len_two_byte_lead() {
        assert_eq!(utf8_len(0xc0), 2);
    }

    #[test]
    fn utf8_len_three_byte_lead() {
        assert_eq!(utf8_len(0xe0), 3);
    }

    #[test]
    fn utf8_len_four_byte_lead() {
        assert_eq!(utf8_len(0xf0), 4);
    }

    #[test]
    fn utf8_len_ascii_max() {
        assert_eq!(utf8_len(0x7f), 1);
    }

    #[test]
    fn decode_streaming_rejects_overlong_encoding() {
        let mut pending = Vec::new();
        // Overlong encoding of '/' (0x2f) as 0xc0 0xaf — invalid UTF-8
        let out = decode_streaming(&mut pending, &[0xc0, 0xaf]);
        // Should produce replacement character
        assert!(out.contains('\u{FFFD}') || out.is_empty());
    }

    #[test]
    fn decode_streaming_rejects_surrogate_half() {
        let mut pending = Vec::new();
        // Surrogate half (0xED 0xA0 0x80 = U+D800) is invalid UTF-8.
        // It should be replaced with U+FFFD.
        let out = decode_streaming(&mut pending, &[0xed, 0xa0, 0x80]);
        assert!(!out.is_empty());
        // The output should contain the replacement character
        assert!(out.contains('\u{FFFD}') || out.contains('\u{fffd}'));
        assert!(pending.is_empty());
    }

    #[test]
    fn route_input_chunk_echoes_normal_text() {
        let (to_pty, mute) = route(&[b"hello world\n"]);
        assert_eq!(to_pty, b"hello world\n");
        assert!(!mute);
    }

    #[test]
    fn route_input_chunk_split_across_chunks() {
        let (to_pty, _) = route(&[b"hel", b"lo\n"]);
        assert_eq!(to_pty, b"hello\n");
    }

    #[test]
    fn route_input_chunk_backspace() {
        let (to_pty, _) = route(&[b"ab\x7f"]);
        assert_eq!(to_pty, b"ab\x7f");
    }

    #[test]
    fn route_input_chunk_utf8() {
        let (to_pty, _) = route(&["café\n".as_bytes()]);
        assert_eq!(to_pty, "café\n".as_bytes());
    }

    #[test]
    fn route_input_chunk_meta_command_across_chunks() {
        let (_, mute) = route(&[b"demo ", b"stop\n"]);
        assert!(mute);
    }

    #[test]
    fn route_input_chunk_not_meta_command() {
        let (_, mute) = route(&[b"demodocs\n"]);
        assert!(!mute);
    }

    #[test]
    fn track_and_detect_secret_at_newline_boundary() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Password:\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(sensitive.load(Ordering::SeqCst));
    }

    #[test]
    fn track_and_detect_no_secret_without_colon() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Password\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(!sensitive.load(Ordering::SeqCst));
    }

    #[test]
    fn track_and_detect_long_line_not_truncated() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        // Line under MAX_PROMPT_LINE should be kept
        let short = "a".repeat(100);
        track_and_detect(
            &mut line,
            &short,
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert_eq!(line.len(), 100);
    }

    #[test]
    fn track_and_detect_long_line_truncated() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        // Line over MAX_PROMPT_LINE should be truncated
        let long = "a".repeat(300);
        track_and_detect(
            &mut line,
            &long,
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(line.len() <= MAX_PROMPT_LINE);
    }

    #[test]
    fn clean_prompt_csi_sequence_with_params() {
        assert_eq!(clean_prompt("[38;5;10mPassword:[39m"), "Password:");
    }

    #[test]
    fn clean_prompt_csi_sequence_cursor() {
        assert_eq!(clean_prompt("[?25lPassword:"), "Password:");
    }

    #[test]
    fn clean_prompt_mixed_content() {
        assert_eq!(clean_prompt("[31m[?25l> Password:"), "Password:");
    }

    #[test]
    fn is_secret_prompt_with_space_before_colon() {
        assert!(is_secret_prompt("Password :"));
    }

    #[test]
    fn is_secret_prompt_with_tab() {
        assert!(is_secret_prompt("Password:\t"));
    }

    #[test]
    fn is_secret_prompt_case_insensitive() {
        assert!(is_secret_prompt("PASSWORD:"));
        assert!(is_secret_prompt("Passphrase:"));
        assert!(is_secret_prompt("PASSPHRASE:"));
    }

    #[test]
    fn is_secret_prompt_with_leading_whitespace() {
        assert!(is_secret_prompt("  Password:"));
    }

    #[test]
    fn cue_matches_regex_pattern() {
        assert!(cue_matches("done in 123ms", "re:\\d+ms"));
        assert!(!cue_matches("no numbers", "re:\\d+ms"));
    }

    #[test]
    fn cue_matches_plain_substring_long() {
        assert!(cue_matches("Report generated successfully.", "Report"));
        assert!(!cue_matches("Report generated", "Error"));
    }

    #[test]
    fn cue_matches_empty_pattern_matches_anything() {
        assert!(cue_matches("anything", ""));
    }

    #[test]
    fn decode_streaming_complete_utf8() {
        let mut pending = Vec::new();
        let out = decode_streaming(&mut pending, "hello world".as_bytes());
        assert_eq!(out, "hello world");
        assert!(pending.is_empty());
    }

    #[test]
    fn decode_streaming_split_emoji() {
        let mut pending = Vec::new();
        // 🎉 is 4 bytes: f0 9f 8e 89
        let emoji = "🎉";
        let bytes = emoji.as_bytes();
        let first = decode_streaming(&mut pending, &bytes[..2]);
        assert_eq!(first, "");
        let second = decode_streaming(&mut pending, &bytes[2..]);
        assert_eq!(second, emoji);
        assert!(pending.is_empty());
    }

    #[test]
    fn decode_streaming_empty_pending() {
        let mut pending = Vec::new();
        let out = decode_streaming(&mut pending, &[]);
        assert_eq!(out, "");
        assert!(pending.is_empty());
    }

    #[test]
    fn route_input_chunk_multiple_chars() {
        let (to_pty, _) = route(&[b"abc\n"]);
        assert_eq!(to_pty, b"abc\n");
    }

    #[test]
    fn route_input_chunk_control_chars() {
        let (to_pty, _) = route(&[b"\x03"]);
        assert_eq!(to_pty, b"\x03");
    }

    #[test]
    fn utf8_len_all_ranges() {
        // ASCII
        assert_eq!(utf8_len(0x00), 1);
        assert_eq!(utf8_len(0x7f), 1);
        // Continuation bytes
        assert_eq!(utf8_len(0x80), 1);
        assert_eq!(utf8_len(0xbf), 1);
        // 2-byte leads
        assert_eq!(utf8_len(0xc0), 2);
        assert_eq!(utf8_len(0xdf), 2);
        // 3-byte leads
        assert_eq!(utf8_len(0xe0), 3);
        assert_eq!(utf8_len(0xef), 3);
        // 4-byte leads
        assert_eq!(utf8_len(0xf0), 4);
        assert_eq!(utf8_len(0xf4), 4);
    }

    #[test]
    fn is_meta_command_variations() {
        assert!(is_meta_command("demo stop"));
        assert!(is_meta_command("demo open http://example.com"));
        assert!(is_meta_command("demo focus main"));
        assert!(!is_meta_command("echo demo stop"));
        assert!(!is_meta_command("ls"));
        assert!(!is_meta_command(""));
        assert!(!is_meta_command("demo"));
        assert!(!is_meta_command("demo "));
    }

    #[test]
    fn secret_step_on_submit_no_guard_records() {
        let mut last: Option<String> = None;
        assert!(secret_step_on_submit(&mut last, false, "Password:"));
        assert_eq!(last.as_deref(), Some("Password:"));
    }

    #[test]
    fn secret_step_on_submit_same_prompt_is_dup() {
        let mut last = Some("Password:".to_string());
        assert!(!secret_step_on_submit(&mut last, false, "Password:"));
    }

    #[test]
    fn secret_step_on_submit_different_prompt_records() {
        let mut last = Some("Password:".to_string());
        assert!(secret_step_on_submit(&mut last, false, "Token:"));
        assert_eq!(last.as_deref(), Some("Token:"));
    }

    #[test]
    fn secret_step_on_submit_clears_guard_when_prompt_left_screen() {
        let mut last = Some("Password:".to_string());
        assert!(secret_step_on_submit(&mut last, true, "Password:"));
        assert_eq!(last.as_deref(), Some("Password:"));
    }

    #[test]
    fn secret_dedup_same_prompt_with_submission_produces_two_events() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut last_secret_prompt: Option<String> = None;
        let mut events = Vec::new();

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Password:\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        let prompt = secret_prompt.lock().unwrap().take().unwrap();
        let cleared = secret_prompt_cleared.swap(false, Ordering::SeqCst);
        if secret_step_on_submit(&mut last_secret_prompt, cleared, &prompt) {
            events.push(prompt);
        }
        sensitive.store(false, Ordering::SeqCst);

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Welcome to sudo\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(secret_prompt_cleared.load(Ordering::SeqCst));

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Password:\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        let prompt = secret_prompt.lock().unwrap().take().unwrap();
        let cleared = secret_prompt_cleared.swap(false, Ordering::SeqCst);
        if secret_step_on_submit(&mut last_secret_prompt, cleared, &prompt) {
            events.push(prompt);
        }

        assert_eq!(events.len(), 2);
    }

    #[test]
    fn secret_dedup_same_prompt_no_submission_produces_one_event() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut last_secret_prompt: Option<String> = None;
        let mut events = Vec::new();

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Password:\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Password:\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );

        let prompt = secret_prompt.lock().unwrap().take().unwrap();
        let cleared = secret_prompt_cleared.swap(false, Ordering::SeqCst);
        if secret_step_on_submit(&mut last_secret_prompt, cleared, &prompt) {
            events.push(prompt);
        }

        assert_eq!(events.len(), 1);
    }

    #[test]
    fn secret_dedup_different_prompts_produce_two_events() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut last_secret_prompt: Option<String> = None;
        let mut events = Vec::new();

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Password:\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        let prompt = secret_prompt.lock().unwrap().take().unwrap();
        let cleared = secret_prompt_cleared.swap(false, Ordering::SeqCst);
        if secret_step_on_submit(&mut last_secret_prompt, cleared, &prompt) {
            events.push(prompt);
        }
        sensitive.store(false, Ordering::SeqCst);

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Some output\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Token:\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        let prompt = secret_prompt.lock().unwrap().take().unwrap();
        let cleared = secret_prompt_cleared.swap(false, Ordering::SeqCst);
        if secret_step_on_submit(&mut last_secret_prompt, cleared, &prompt) {
            events.push(prompt);
        }

        assert_eq!(events.len(), 2);
    }

    #[test]
    fn secret_dedup_guard_must_be_cleared_by_prompt_leaving_screen() {
        // Exercises the extracted helper directly: same prompt submitted twice
        // with prompt_left_screen=true between them must produce two events.
        // If the guard were capture-wide (prompt_left_screen ignored), the
        // second call would return false and this test would fail.
        let mut last: Option<String> = None;
        assert!(secret_step_on_submit(&mut last, false, "Password:"));
        assert!(secret_step_on_submit(&mut last, true, "Password:"));
    }

    #[test]
    fn secret_dedup_immediate_redraw_after_enter_is_suppressed() {
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut last_secret_prompt: Option<String> = None;
        let mut events = Vec::new();

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Password:\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        let prompt = secret_prompt.lock().unwrap().take().unwrap();
        let cleared = secret_prompt_cleared.swap(false, Ordering::SeqCst);
        if secret_step_on_submit(&mut last_secret_prompt, cleared, &prompt) {
            events.push(prompt);
        }
        sensitive.store(false, Ordering::SeqCst);

        let mut line = String::new();
        track_and_detect(
            &mut line,
            "Password:\n",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        let prompt = secret_prompt.lock().unwrap().take().unwrap();
        let cleared = secret_prompt_cleared.swap(false, Ordering::SeqCst);
        if secret_step_on_submit(&mut last_secret_prompt, cleared, &prompt) {
            events.push(prompt);
        }

        assert_eq!(events.len(), 1);
    }

    #[test]
    fn secret_prompt_cleared_not_set_on_partial_line() {
        // A prompt split across PTY reads must not spuriously set the cleared
        // flag: "[sudo] password for " at chunk end (no newline) is a partial
        // line, not a completed non-secret line.
        let sensitive = AtomicBool::new(false);
        let secret_prompt = Mutex::new(None);
        let secret_prompt_cleared = AtomicBool::new(false);
        let mut line = String::new();
        track_and_detect(
            &mut line,
            "[sudo] password for ",
            &sensitive,
            &secret_prompt,
            &secret_prompt_cleared,
        );
        assert!(
            !secret_prompt_cleared.load(Ordering::SeqCst),
            "partial line at chunk end must not set secret_prompt_cleared"
        );
    }

    // ---------------------------------------------------------------------
    // Pump tests: OutputPump / InputPump driven directly with scripted I/O.
    // ---------------------------------------------------------------------

    use std::sync::Arc;

    /// One step a scripted reader replays: some bytes, or an error.
    enum ReadStep {
        Data(&'static [u8]),
        Err(std::io::ErrorKind),
    }

    /// A `Read` that plays back a fixed sequence of results and then returns
    /// `Ok(0)` (EOF) forever. The trailing EOF guarantee means the read loop
    /// terminates under every mutant of its match arms — none can spin.
    struct ScriptedReader {
        steps: Vec<ReadStep>,
        idx: usize,
    }

    fn scripted(steps: Vec<ReadStep>) -> ScriptedReader {
        ScriptedReader { steps, idx: 0 }
    }

    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.steps.get(self.idx) {
                // Scripted EOF: `idx` never advances again, so a loop that
                // keeps reading past the script still terminates.
                None => Ok(0),
                Some(ReadStep::Data(b)) => {
                    self.idx += 1;
                    let n = b.len().min(buf.len());
                    buf[..n].copy_from_slice(&b[..n]);
                    Ok(n)
                }
                Some(ReadStep::Err(kind)) => {
                    self.idx += 1;
                    Err(std::io::Error::from(*kind))
                }
            }
        }
    }

    /// A writer that records what was written, for asserting PTY forwarding.
    #[derive(Clone, Default)]
    struct VecWriter(Arc<Mutex<Vec<u8>>>);

    impl VecWriter {
        fn written(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer whose every write fails — the PTY went away mid-capture.
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty write failed",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A minimal reveal armed by `--when`, as the control layer builds it.
    fn test_reveal() -> super::super::Reveal {
        super::super::Reveal {
            panes: vec![crate::model::RevealPane::terminal()],
            orientation: crate::model::Orientation::Horizontal,
            hold_ms: None,
            scroll: false,
        }
    }

    /// How many `Reveal` events the recording holds.
    fn reveal_count(state: &CaptureState) -> usize {
        state
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, RawEvent::Reveal { .. }))
            .count()
    }

    #[test]
    fn is_secret_prompt_accepts_a_200_byte_line_and_rejects_201() {
        // The length guard is `> 200`, so exactly 200 bytes is still a prompt.
        let at_limit = format!("password{}:", "x".repeat(191));
        assert_eq!(at_limit.len(), 200);
        assert!(is_secret_prompt(&at_limit), "200 bytes must still match");

        let over_limit = format!("password{}:", "x".repeat(192));
        assert_eq!(over_limit.len(), 201);
        assert!(!is_secret_prompt(&over_limit), "201 bytes is screen paint");

        // At the very same 200-byte length, no keyword still means no match.
        let no_hint = format!("{}:", "x".repeat(199));
        assert_eq!(no_hint.len(), 200);
        assert!(!is_secret_prompt(&no_hint));
    }

    #[test]
    fn route_input_chunk_decodes_multibyte_utf8_into_the_cmd_line() {
        // "é" = [0xc3, 0xa9]: a byte >= 0x80 must take the UTF-8 branch, so
        // the tracked line gets the decoded character — not one latin-1
        // mojibake char per raw byte.
        let chunk = "é".as_bytes();
        let mut cmd_line = String::new();
        let mut cmd_start = None;
        let out = route_input_chunk(chunk, &mut cmd_line, &mut cmd_start, 0);
        assert_eq!(out.to_pty, chunk, "keystrokes reach the PTY byte-for-byte");
        assert_eq!(
            cmd_line, "é",
            "the tracked line holds the decoded character"
        );
        assert!(
            !cmd_line.contains('Ã') && !cmd_line.contains('©'),
            "raw bytes must not leak into the line as mojibake"
        );
    }

    #[test]
    fn output_pump_run_retries_an_interrupted_read_until_eof() {
        // EINTR (e.g. SIGWINCH) is not the shell exiting: the loop must retry
        // and still record the next chunk, then flag shell_exited at EOF.
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        let mut pump = OutputPump::new(
            state.clone(),
            Box::new(scripted(vec![
                ReadStep::Err(std::io::ErrorKind::Interrupted),
                ReadStep::Data(b"data"),
            ])),
            Instant::now(),
        );
        pump.run();
        {
            let events = state.events.lock().unwrap();
            assert_eq!(events.len(), 1, "the read after an EINTR must be recorded");
            match &events[0] {
                RawEvent::Output { data, .. } => assert_eq!(data.as_str(), "data"),
                other => panic!("expected Output, got {other:?}"),
            }
        }
        assert!(
            state.shell_exited.load(Ordering::SeqCst),
            "EOF must flag the shell as exited"
        );
    }

    #[test]
    fn output_pump_run_stops_on_a_non_interrupted_read_error() {
        // A hard read error is not retryable: the pump must stop WITHOUT
        // consuming (and recording) whatever the reader offers next.
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        let mut pump = OutputPump::new(
            state.clone(),
            Box::new(scripted(vec![
                ReadStep::Err(std::io::ErrorKind::Other),
                ReadStep::Data(b"data"),
            ])),
            Instant::now(),
        );
        pump.run();
        assert!(
            state.events.lock().unwrap().is_empty(),
            "a hard read error must stop the pump before later data is recorded"
        );
        assert!(
            state.shell_exited.load(Ordering::SeqCst),
            "the stopped pump still flags the shell as exited"
        );
    }

    #[test]
    fn output_pump_feed_records_output_when_ready() {
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        let mut pump = OutputPump::new(state.clone(), Box::new(scripted(vec![])), Instant::now());
        pump.feed(b"hello");
        let events = state.events.lock().unwrap();
        assert_eq!(events.len(), 1, "ready output must be recorded");
        match &events[0] {
            RawEvent::Output { data, .. } => assert_eq!(data.as_str(), "hello"),
            other => panic!("expected Output, got {other:?}"),
        }
    }

    #[test]
    fn output_pump_absorbs_preroll_and_records_only_the_tail_after_the_marker() {
        // Before the readiness marker nothing is recorded; the marker itself
        // and everything before it are discarded, the tail after it is the
        // first Output event — exactly once, byte-for-byte.
        let state = CaptureState::new();
        let mut pump = OutputPump::new(state.clone(), Box::new(scripted(vec![])), Instant::now());
        pump.feed(b"setup chatter ");
        assert!(!state.ready.load(Ordering::SeqCst));
        assert!(
            state.events.lock().unwrap().is_empty(),
            "pre-marker output must not be recorded"
        );
        pump.feed(format!("xx{PROMPT_READY}TAIL").as_bytes());
        assert!(
            state.ready.load(Ordering::SeqCst),
            "the marker must arm recording"
        );
        let events = state.events.lock().unwrap();
        assert_eq!(events.len(), 1, "only the post-marker tail is recorded");
        match &events[0] {
            RawEvent::Output { data, .. } => assert_eq!(data.as_str(), "TAIL"),
            other => panic!("expected Output, got {other:?}"),
        }
    }

    #[test]
    fn recent_window_keeps_a_cue_split_across_two_chunks() {
        // The `--when` cue spans a read boundary: the first half must survive
        // in the recent-output window so the second half can complete it.
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        state
            .pending_opens
            .lock()
            .unwrap()
            .push((test_reveal(), "XY".to_string()));
        let mut pump = OutputPump::new(state.clone(), Box::new(scripted(vec![])), Instant::now());
        pump.feed(b"abcX");
        pump.feed(b"Y done");
        assert_eq!(
            reveal_count(&state),
            1,
            "a cue spanning two chunks must fire once its second half arrives"
        );
        assert!(
            state.pending_opens.lock().unwrap().is_empty(),
            "the fired reveal must be drained"
        );
    }

    #[test]
    fn recent_window_is_not_cleared_at_exactly_8192_bytes() {
        // The window trims only when it exceeds 8192 bytes. Right at the
        // boundary the accumulated output must survive so a cue completed by
        // the next chunk still matches.
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        state
            .pending_opens
            .lock()
            .unwrap()
            .push((test_reveal(), "MARKER".to_string()));
        let mut pump = OutputPump::new(state.clone(), Box::new(scripted(vec![])), Instant::now());
        let mut first = vec![b'x'; 8192 - 4];
        first.extend_from_slice(b"MARK");
        assert_eq!(first.len(), 8192, "boundary chunk is exactly 8192 bytes");
        pump.feed(&first);
        pump.feed(b"ER");
        assert_eq!(
            reveal_count(&state),
            1,
            "the window must keep exactly-8192 bytes so the cue can complete"
        );
    }

    #[test]
    fn fire_due_reveals_records_the_matched_reveal_at_the_given_time() {
        let state = CaptureState::new();
        state
            .pending_opens
            .lock()
            .unwrap()
            .push((test_reveal(), "build ok".to_string()));
        let mut pump = OutputPump::new(state.clone(), Box::new(scripted(vec![])), Instant::now());
        pump.recent.push_str("everything build ok now");
        pump.fire_due_reveals(1234);
        {
            let events = state.events.lock().unwrap();
            assert_eq!(events.len(), 1, "a matched cue must produce a Reveal");
            match &events[0] {
                RawEvent::Reveal { t_ms, .. } => assert_eq!(*t_ms, 1234),
                other => panic!("expected Reveal, got {other:?}"),
            }
        }
        assert!(
            state.pending_opens.lock().unwrap().is_empty(),
            "the fired reveal must be drained"
        );
    }

    #[test]
    fn handle_returns_true_when_the_pty_write_fails() {
        // A failed PTY write means the session is dead: `handle` must report
        // it so the read loop stops, and record nothing after the failure.
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        let mut pump = InputPump::new(state.clone(), Box::new(FailingWriter), Instant::now());
        let stop = pump.handle(b"hi");
        assert!(stop, "a failed PTY write must tell the read loop to stop");
        assert!(
            state.events.lock().unwrap().is_empty(),
            "nothing is recorded after a failed write"
        );
    }

    #[test]
    fn handle_forwards_and_records_input_and_returns_false_when_healthy() {
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        let writer = VecWriter::default();
        let mut pump = InputPump::new(state.clone(), Box::new(writer.clone()), Instant::now());
        let stop = pump.handle(b"hi");
        assert!(!stop, "a healthy write must not stop the read loop");
        assert_eq!(
            writer.written(),
            b"hi",
            "keystrokes must reach the PTY verbatim"
        );
        let events = state.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            RawEvent::Input { bytes, .. } => assert_eq!(bytes.as_str(), "hi"),
            other => panic!("expected Input, got {other:?}"),
        }
    }

    #[test]
    fn handle_ignores_input_during_the_pre_roll() {
        // Until the readiness marker arms recording, keystrokes are neither
        // forwarded nor recorded.
        let state = CaptureState::new();
        let writer = VecWriter::default();
        let mut pump = InputPump::new(state.clone(), Box::new(writer.clone()), Instant::now());
        let stop = pump.handle(b"hi");
        assert!(!stop);
        assert!(
            state.events.lock().unwrap().is_empty(),
            "pre-roll keystrokes must not be recorded"
        );
        assert!(
            writer.written().is_empty(),
            "pre-roll keystrokes must not reach the PTY"
        );
    }

    #[test]
    fn secret_submission_records_the_prompt_event_on_enter() {
        // Enter submits the secret: exactly one `Secret` event holding the
        // prompt label (never the typed value), and the latch clears.
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        state.sensitive.store(true, Ordering::SeqCst);
        *state.secret_prompt.lock().unwrap() = Some("Vault passphrase:".to_string());
        let mut pump = InputPump::new(
            state.clone(),
            Box::new(VecWriter::default()),
            Instant::now(),
        );
        let stop = pump.handle(b"hunter2\r");
        assert!(!stop);
        assert!(
            !state.sensitive.load(Ordering::SeqCst),
            "Enter clears the sensitive latch"
        );
        let events = state.events.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "exactly one Secret event, and the typed value is never recorded"
        );
        match &events[0] {
            RawEvent::Secret { prompt, .. } => assert_eq!(prompt.as_str(), "Vault passphrase:"),
            other => panic!("expected Secret, got {other:?}"),
        }
    }

    /// A lone carriage return submits: only `==` on `\\r` sees it — `!=`
    /// would find no differing byte and skip the submission.
    #[test]
    fn lone_carriage_return_submits_the_secret() {
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        state.sensitive.store(true, Ordering::SeqCst);
        *state.secret_prompt.lock().unwrap() = Some("Password:".to_string());
        let mut pump = InputPump::new(
            state.clone(),
            Box::new(VecWriter::default()),
            Instant::now(),
        );
        pump.handle(b"\r");
        assert_eq!(
            state.events.lock().unwrap().len(),
            1,
            "a lone CR must submit the secret"
        );
    }

    /// A lone line feed submits too: only `==` on `\\n` sees it.
    #[test]
    fn lone_line_feed_submits_the_secret() {
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        state.sensitive.store(true, Ordering::SeqCst);
        *state.secret_prompt.lock().unwrap() = Some("Password:".to_string());
        let mut pump = InputPump::new(
            state.clone(),
            Box::new(VecWriter::default()),
            Instant::now(),
        );
        pump.handle(b"\n");
        assert_eq!(
            state.events.lock().unwrap().len(),
            1,
            "a lone LF must submit the secret"
        );
    }

    #[test]
    fn keystrokes_during_a_secret_prompt_are_not_a_submission() {
        // Without Enter there is no submission: the latch stays set, the
        // prompt stays armed, and no Secret event is recorded.
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        state.sensitive.store(true, Ordering::SeqCst);
        *state.secret_prompt.lock().unwrap() = Some("Password:".to_string());
        let mut pump = InputPump::new(
            state.clone(),
            Box::new(VecWriter::default()),
            Instant::now(),
        );
        let stop = pump.handle(b"abc");
        assert!(!stop);
        assert!(
            state.sensitive.load(Ordering::SeqCst),
            "the latch must stay set until Enter"
        );
        assert!(
            state.secret_prompt.lock().unwrap().is_some(),
            "the prompt must stay armed"
        );
        assert!(
            state.events.lock().unwrap().is_empty(),
            "no Secret event without Enter"
        );
    }

    #[test]
    fn secret_submission_dedups_redraws_until_the_prompt_leaves_the_screen() {
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        let mut pump = InputPump::new(
            state.clone(),
            Box::new(VecWriter::default()),
            Instant::now(),
        );
        let submit = |cleared: bool| {
            state.sensitive.store(true, Ordering::SeqCst);
            *state.secret_prompt.lock().unwrap() = Some("Password:".to_string());
            state.secret_prompt_cleared.store(cleared, Ordering::SeqCst);
        };
        // 1) First submission records one Secret event.
        submit(false);
        assert!(!pump.handle(b"pw1\r"));
        assert!(!state.sensitive.load(Ordering::SeqCst));
        assert_eq!(state.events.lock().unwrap().len(), 1);
        // 2) An immediate redraw of the same prompt (still on screen) dedups.
        submit(false);
        assert!(!pump.handle(b"pw2\r"));
        assert!(!state.sensitive.load(Ordering::SeqCst));
        assert_eq!(
            state.events.lock().unwrap().len(),
            1,
            "a redraw without the prompt leaving the screen must not duplicate"
        );
        // 3) Once a completed line sent the prompt off screen, it records again.
        submit(true);
        assert!(!pump.handle(b"pw3\r"));
        assert_eq!(
            state.events.lock().unwrap().len(),
            2,
            "prompt left screen → the same prompt records a new event"
        );
        for ev in state.events.lock().unwrap().iter() {
            match ev {
                RawEvent::Secret { prompt, .. } => {
                    assert_eq!(prompt.as_str(), "Password:")
                }
                other => panic!("expected only Secret events, got {other:?}"),
            }
        }
    }
}
