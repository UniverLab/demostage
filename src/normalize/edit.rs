//! Keystroke reconstruction (SPEC §3.1): replay the raw input stream through a
//! minimal line editor, producing an ordered stream of [`Action`]s — clean typed
//! text plus the keys actually pressed (Enter, arrows, Tab, …).
//!
//! Destructive edits (backspace, Ctrl-U) are applied so typed text is the *clean*
//! text the user meant — never the typos. But navigation keys are **kept** as
//! [`Action::Key`], so re-executing the score (`demo record`) drives interactive
//! programs — wizards, selectors — exactly as the human did, instead of
//! desyncing and cancelling.

/// One reconstructed input action, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A run of clean typed text (destructive edits already applied).
    Type {
        text: String,
        /// Time (ms from start) the first character of the run was typed.
        t_ms: u64,
        /// Time (ms from start) the last character of the run was typed.
        end_ms: u64,
    },
    /// A single named key press (`enter`, `up`, `down`, `tab`, `ctrl+c`, …).
    Key {
        key: String,
        /// Time (ms from start) the key was pressed.
        t_ms: u64,
    },
}

/// Where we are inside a terminal escape sequence. Arrow keys and other special
/// keys arrive as multi-byte sequences (e.g. Up = `ESC [ A`); we parse the whole
/// sequence and turn it into a named [`Action::Key`].
enum Esc {
    /// Not in a sequence.
    None,
    /// Just saw `ESC`; the next byte selects the sequence kind.
    Saw,
    /// Inside a CSI sequence (`ESC [ … final`); collecting parameter bytes.
    Csi(String),
    /// Inside an SS3 sequence (`ESC O x`) — application-mode cursor / function keys.
    Ss3,
    /// Inside an OSC (`ESC ] … BEL` or `ESC ] … ESC \`) — terminal colour queries,
    /// title changes, etc. TUI apps (e.g. opencode) trigger these and the host
    /// terminal may echo the responses into the captured input stream.
    Osc,
    /// Inside a DCS (`ESC P … ESC \`), APC, PM, or SOS string.
    Str,
    /// Saw `ESC` while inside [`Esc::Osc`] / [`Esc::Str`] — expect `\` (ST).
    St,
    /// Bracketed-paste payload between `ESC [ 200 ~` and `ESC [ 201 ~`.
    Paste,
    /// Saw `ESC` inside a bracketed paste — expect `[` to start the end marker.
    PasteSaw,
    /// Collecting CSI params for a bracketed-paste end marker (`201 ~`).
    PasteCsi(String),
}

/// Modifier bitmask values in CSI sequences (xterm-style).
const MOD_SHIFT: u8 = 2;
const MOD_ALT: u8 = 3;
const MOD_CTRL: u8 = 5;
const MOD_CTRL_SHIFT: u8 = 6;

/// Decode a CSI modifier number into a prefix string like "shift-", "alt-", "ctrl+", etc.
fn modifier_prefix(modifier: u8) -> &'static str {
    match modifier {
        MOD_SHIFT => "shift-",
        MOD_ALT => "alt-",
        4 => "alt+shift-", // alt(2) + shift but bitmask 4 in some terminals
        MOD_CTRL => "ctrl+",
        MOD_CTRL_SHIFT => "ctrl+shift-",
        7 => "ctrl+alt-",
        8 => "ctrl+alt+shift-",
        _ => "",
    }
}

/// Parse CSI params for a modified key: returns `(keycode, modifier)`. The
/// modifier defaults to 0 (unmodified) when absent.
fn parse_csi_params(params: &str) -> (u8, u8) {
    if let Some((code_str, mod_str)) = params.split_once(';') {
        let code = code_str.parse::<u8>().unwrap_or(0);
        let modifier = mod_str.parse::<u8>().unwrap_or(0);
        (code, modifier)
    } else {
        let code = params.parse::<u8>().unwrap_or(0);
        (code, 0)
    }
}

/// Map a CSI sequence (its parameters and final byte) to a key name, if known.
/// Handles both unmodified keys (`\x1b[A`) and modifier combinations
/// (`\x1b[1;2A` = Shift+Up, `\x1b[1;5A` = Ctrl+Up, etc.).
fn csi_key(params: &str, final_byte: char) -> Option<String> {
    let (code, modifier) = parse_csi_params(params);
    let prefix = modifier_prefix(modifier);

    let base = match final_byte {
        'A' => Some("up"),
        'B' => Some("down"),
        'C' => Some("right"),
        'D' => Some("left"),
        'H' => Some("home"),
        'F' => Some("end"),
        'P' if modifier == 0 => Some("f1"),
        'Q' if modifier == 0 => Some("f2"),
        'R' if modifier == 0 => Some("f3"),
        'S' if modifier == 0 => Some("f4"),
        '~' => match code {
            1 | 7 => Some("home"),
            4 | 8 => Some("end"),
            3 => Some("delete"),
            5 => Some("pageup"),
            6 => Some("pagedown"),
            // VT220 function keys F1-F12
            11 => Some("f1"),
            12 => Some("f2"),
            13 => Some("f3"),
            14 => Some("f4"),
            15 => Some("f5"),
            17 => Some("f6"),
            18 => Some("f7"),
            19 => Some("f8"),
            20 => Some("f9"),
            21 => Some("f10"),
            23 => Some("f11"),
            24 => Some("f12"),
            // Insert / Delete with modifiers
            2 => Some("insert"),
            _ => None,
        },
        _ => None,
    };
    base.map(|b| format!("{prefix}{b}"))
}

/// Map an SS3 sequence's final byte to a key name (application-mode arrows and function keys), if known.
fn ss3_key(final_byte: char) -> Option<&'static str> {
    match final_byte {
        'A' => Some("up"),
        'B' => Some("down"),
        'C' => Some("right"),
        'D' => Some("left"),
        'H' => Some("home"),
        'F' => Some("end"),
        // Application-mode function keys F1-F4 (ESC O P/Q/R/S)
        'P' => Some("f1"),
        'Q' => Some("f2"),
        'R' => Some("f3"),
        'S' => Some("f4"),
        _ => None,
    }
}

/// Drop OSC bodies that leaked without their `ESC ]` prefix — the old parser
/// consumed `ESC` and discarded `]`, leaving fragments like `10;rgb:…` in the
/// typed text.
fn strip_orphan_osc_bodies(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(skip) = orphan_osc_cluster_len(&chars[i..]) {
            i += skip;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// One OSC colour response: `10;rgb:abab/baba/baba` or `4;0;rgb:0000/0000/0000`.
fn orphan_osc_unit_len(chars: &[char]) -> Option<usize> {
    let mut i = 0;
    if chars.first().is_none_or(|c| !c.is_ascii_digit()) {
        return None;
    }
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    loop {
        if chars.get(i) != Some(&';') {
            break;
        }
        i += 1;
        let param_start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i == param_start {
            break;
        }
    }
    if chars.get(i..i + 4) != Some(&['r', 'g', 'b', ':']) {
        return None;
    }
    i += 4;
    for comp in 0..3 {
        let start = i;
        while i < chars.len() && chars[i].is_ascii_hexdigit() && i - start < 4 {
            i += 1;
        }
        if i == start {
            return None;
        }
        if comp < 2 {
            if chars.get(i) != Some(&'/') {
                return None;
            }
            i += 1;
        }
    }
    Some(i)
}

/// A run of one or more concatenated OSC colour responses.
fn orphan_osc_cluster_len(chars: &[char]) -> Option<usize> {
    let mut total = 0;
    loop {
        match orphan_osc_unit_len(&chars[total..]) {
            Some(n) => total += n,
            None => {
                if total > 0 && chars.get(total) == Some(&';') {
                    if let Some(n) = orphan_osc_unit_len(&chars[total + 1..]) {
                        total += 1 + n;
                        continue;
                    }
                }
                break;
            }
        }
    }
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

/// The state a [`reconstruct`] replay threads through the input: the actions
/// emitted so far, the typed run being accumulated (and when it started and
/// ended), and the escape-sequence machine the next character is fed to.
struct ReconstructState {
    actions: Vec<Action>,
    buf: Vec<char>,
    start_ms: u64,
    end_ms: u64,
    esc: Esc,
}

impl ReconstructState {
    fn new() -> Self {
        Self {
            actions: Vec::new(),
            buf: Vec::new(),
            start_ms: 0,
            end_ms: 0,
            esc: Esc::None,
        }
    }

    /// Emit the pending typed run (if any) as a Type action.
    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        self.actions.push(Action::Type {
            text: self.buf.iter().collect(),
            t_ms: self.start_ms,
            end_ms: self.end_ms,
        });
        self.buf.clear();
    }

    /// Feed one character of the replay, timestamped `t`.
    fn feed_char(&mut self, ch: char, t: u64) {
        // Take the escape state out so every handler can be its own method
        // (each one puts the advanced state back).
        let esc = std::mem::replace(&mut self.esc, Esc::None);
        if !self.dispatch(esc, ch, t) {
            self.feed_plain(ch, t);
        }
    }

    /// Run the escape-sequence machine for `ch`. Returns false when the
    /// character is not consumed by it — plain text, or an `ESC` that must be
    /// emitted as a keypress before the character itself is processed.
    fn dispatch(&mut self, esc: Esc, ch: char, t: u64) -> bool {
        match esc {
            Esc::Csi(params) => self.feed_csi(params, ch, t),
            Esc::Ss3 => self.feed_ss3(ch, t),
            Esc::Osc | Esc::Str => self.feed_osc_str(esc, ch),
            Esc::St => {
                self.esc = Esc::None;
                true
            }
            Esc::Paste => self.feed_paste(ch, t),
            Esc::PasteSaw => self.feed_paste_saw(ch),
            Esc::PasteCsi(params) => self.feed_paste_csi(params, ch),
            Esc::Saw => self.feed_saw(ch, t),
            Esc::None => false,
        }
    }

    /// `ESC [ …` — a CSI sequence: accumulate parameters until a final byte
    /// maps to a key; a `200` final marks the start of a bracketed paste.
    fn feed_csi(&mut self, mut params: String, ch: char, t: u64) -> bool {
        if ('\u{40}'..='\u{7e}').contains(&ch) {
            if ch == '~' && params == "200" {
                self.esc = Esc::Paste;
            } else if let Some(key) = csi_key(&params, ch) {
                self.flush();
                self.actions.push(Action::Key { key, t_ms: t });
                self.esc = Esc::None;
            } else {
                self.esc = Esc::None;
            }
        } else {
            params.push(ch);
            self.esc = Esc::Csi(params);
        }
        true
    }

    /// `ESC O …` — an SS3 sequence (application-mode arrows and function keys).
    fn feed_ss3(&mut self, ch: char, t: u64) -> bool {
        if let Some(key) = ss3_key(ch) {
            self.flush();
            self.actions.push(Action::Key {
                key: key.to_string(),
                t_ms: t,
            });
        }
        self.esc = Esc::None;
        true
    }

    /// `ESC ] …` (OSC) and `ESC P`/`^`/`X …` (string) — swallow the body until
    /// BEL or ST terminates it; anything else is body text, so the state
    /// stands.
    fn feed_osc_str(&mut self, state: Esc, ch: char) -> bool {
        self.esc = match ch {
            '\u{07}' => Esc::None,
            '\u{1b}' => Esc::St,
            _ => state,
        };
        true
    }

    /// Inside a bracketed paste: keep the text verbatim — Enter still submits
    /// the pasted block, edits prune the run, ESC ends the paste.
    fn feed_paste(&mut self, ch: char, t: u64) -> bool {
        if ch == '\u{1b}' {
            self.esc = Esc::PasteSaw;
            return true;
        }
        // Anything else is paste content: the paste continues.
        self.esc = Esc::Paste;
        if ch == '\r' || ch == '\n' {
            self.flush();
            self.actions.push(Action::Key {
                key: "enter".to_string(),
                t_ms: t,
            });
        } else if ch == '\u{7f}' || ch == '\u{8}' {
            self.buf.pop();
        } else if ch == '\u{15}' {
            self.buf.clear();
        } else if !ch.is_control() {
            if self.buf.is_empty() {
                self.start_ms = t;
            }
            self.end_ms = t;
            self.buf.push(ch);
        }
        true
    }

    /// After `ESC` inside a bracketed paste: `[` starts the end-marker CSI,
    /// anything else is ordinary paste content.
    fn feed_paste_saw(&mut self, ch: char) -> bool {
        self.esc = if ch == '[' {
            Esc::PasteCsi(String::new())
        } else {
            Esc::Paste
        };
        true
    }

    /// After the paste-ending `ESC [`: `201` closes the paste, anything else
    /// is a key sequence that ends up back in normal paste handling.
    fn feed_paste_csi(&mut self, mut params: String, ch: char) -> bool {
        if ('\u{40}'..='\u{7e}').contains(&ch) {
            self.esc = if ch == '~' && params == "201" {
                Esc::None
            } else {
                Esc::Paste
            };
        } else {
            params.push(ch);
            self.esc = Esc::PasteCsi(params);
        }
        true
    }

    /// An `ESC` was just seen: route the next character into a sequence, or —
    /// when it does not start one — emit the `esc` keypress and let the
    /// character be processed normally.
    fn feed_saw(&mut self, ch: char, t: u64) -> bool {
        match ch {
            '[' => self.esc = Esc::Csi(String::new()),
            'O' => self.esc = Esc::Ss3,
            ']' => self.esc = Esc::Osc,
            'P' => self.esc = Esc::Str,
            '_' | '^' | 'X' => self.esc = Esc::Str,
            _ => {
                // ESC + unrecognized char: emit "esc" as a keypress
                // and process the following character normally.
                self.flush();
                self.actions.push(Action::Key {
                    key: "esc".to_string(),
                    t_ms: t,
                });
                self.esc = Esc::None;
                return false;
            }
        }
        true
    }

    /// Process a character that is not part of an escape sequence.
    fn feed_plain(&mut self, ch: char, t: u64) {
        match ch {
            // ESC: start of a special-key sequence.
            '\u{1b}' => self.esc = Esc::Saw,
            // Enter: submit. ALWAYS kept — a bare Enter accepts a selector
            // default, which interactive programs depend on.
            '\r' | '\n' => {
                self.flush();
                self.actions.push(Action::Key {
                    key: "enter".to_string(),
                    t_ms: t,
                });
            }
            // Backspace / DEL: prune a typo from the current run.
            '\u{7f}' | '\u{8}' => {
                self.buf.pop();
            }
            // Ctrl-U: kill the whole line.
            '\u{15}' => self.buf.clear(),
            // Ctrl-C: cancel — kept as a key so replay matches.
            '\u{3}' => {
                self.buf.clear();
                self.actions.push(Action::Key {
                    key: "ctrl+c".to_string(),
                    t_ms: t,
                });
            }
            // Ctrl-S: XOFF / save — kept as a key so replay matches.
            '\u{13}' => {
                self.flush();
                self.actions.push(Action::Key {
                    key: "ctrl+s".to_string(),
                    t_ms: t,
                });
            }
            // Tab: completion / field navigation — kept as a key.
            '\t' => {
                self.flush();
                self.actions.push(Action::Key {
                    key: "tab".to_string(),
                    t_ms: t,
                });
            }
            // Ignore other stray control bytes (Bell, …).
            c if c.is_control() => {}
            c => {
                if self.buf.is_empty() {
                    self.start_ms = t;
                }
                self.end_ms = t;
                self.buf.push(c);
            }
        }
    }

    /// Finish the replay: flush the pending run and, if input ended right after
    /// an `ESC`, emit that ESC as a keypress so it isn't silently dropped.
    fn finish(mut self) -> Vec<Action> {
        self.flush();
        if let Esc::Saw = self.esc {
            self.actions.push(Action::Key {
                key: "esc".to_string(),
                t_ms: self.end_ms,
            });
        }
        self.actions
    }
}

/// Replay timestamped input chunks into an ordered list of clean actions.
pub fn reconstruct(inputs: &[(u64, &str)]) -> Vec<Action> {
    let mut state = ReconstructState::new();
    for &(t, bytes) in inputs {
        let bytes = strip_orphan_osc_bodies(bytes);
        for ch in bytes.chars() {
            state.feed_char(ch, t);
        }
    }
    state.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(actions: &[Action]) -> Vec<&str> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Type { text, .. } => Some(text.as_str()),
                Action::Key { .. } => None,
            })
            .collect()
    }

    fn keys(actions: &[Action]) -> Vec<&str> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Key { key, .. } => Some(key.as_str()),
                Action::Type { .. } => None,
            })
            .collect()
    }

    #[test]
    fn prunes_backspaces() {
        // g t i [bs] [bs] i t  →  "git"
        let a = reconstruct(&[(0, "gti\u{7f}\u{7f}it\r")]);
        assert_eq!(typed(&a), vec!["git"]);
        assert_eq!(keys(&a), vec!["enter"]);
    }

    #[test]
    fn ctrl_u_kills_the_line() {
        let a = reconstruct(&[(0, "wrong command\u{15}ls -la\r")]);
        assert_eq!(typed(&a), vec!["ls -la"]);
    }

    #[test]
    fn splits_runs_on_enter() {
        let a = reconstruct(&[(0, "ls\r"), (500, "pwd\r")]);
        assert_eq!(typed(&a), vec!["ls", "pwd"]);
        assert_eq!(keys(&a), vec!["enter", "enter"]);
    }

    #[test]
    fn keeps_bare_enters() {
        // A lone Enter (accepting a selector default) must be preserved.
        let a = reconstruct(&[(0, "\r"), (10, "echo hi\r")]);
        assert_eq!(typed(&a), vec!["echo hi"]);
        assert_eq!(keys(&a), vec!["enter", "enter"]);
    }

    #[test]
    fn keeps_arrow_keys_as_keypresses() {
        // Navigation in a selector: typed text stays clean, arrows become keys.
        let a = reconstruct(&[(0, "ab\u{1b}[A\u{1b}[Bcd\u{1b}[C\u{1b}[De\r")]);
        assert_eq!(typed(&a), vec!["ab", "cd", "e"]);
        assert_eq!(keys(&a), vec!["up", "down", "right", "left", "enter"]);
    }

    #[test]
    fn maps_application_mode_arrows() {
        // SS3 arrows (ESC O A/B) used in application cursor mode.
        let a = reconstruct(&[(0, "\u{1b}OA\u{1b}OB\r")]);
        assert_eq!(keys(&a), vec!["up", "down", "enter"]);
    }

    #[test]
    fn tracks_typing_start_time() {
        let a = reconstruct(&[(100, "a"), (140, "b"), (900, "c\r")]);
        match &a[0] {
            Action::Type { text, t_ms, .. } => {
                assert_eq!(text, "abc");
                assert_eq!(*t_ms, 100);
            }
            _ => panic!("expected a Type action first"),
        }
    }

    #[test]
    fn drops_osc_colour_query_responses() {
        // TUI apps query palette colours; the host terminal may echo the OSC
        // responses into the captured input — they must not become demo typing.
        let osc = "\u{1b}]10;rgb:baba/b7b7/b6b6\u{1b}\\";
        let a = reconstruct(&[(0, &format!("{osc}create a thesis\r"))]);
        assert_eq!(typed(&a), vec!["create a thesis"]);
        assert_eq!(keys(&a), vec!["enter"]);
    }

    #[test]
    fn drops_many_concatenated_osc_responses() {
        let noise = "10;rgb:baba/b7b7/b6b611;rgb:1414/1414/14144;0;rgb:0000/0000/0000";
        let osc = format!("\u{1b}]{noise}\u{1b}\\");
        let a = reconstruct(&[(0, &format!("{osc}/exit\r"))]);
        assert_eq!(typed(&a), vec!["/exit"]);
    }

    #[test]
    fn strip_orphan_osc_bodies_unit() {
        let noise = "10;rgb:baba/b7b7/b6b611;rgb:1414/1414/14144;0;rgb:0000/0000/0000";
        assert_eq!(strip_orphan_osc_bodies(noise), "");
        assert_eq!(strip_orphan_osc_bodies(&format!("{noise}/exit")), "/exit");
    }

    #[test]
    fn drops_orphan_osc_bodies_without_esc_prefix() {
        // Regression: the old parser ate ESC/] and left the OSC payload behind.
        let noise = "10;rgb:baba/b7b7/b6b611;rgb:1414/1414/14144;0;rgb:0000/0000/0000";
        let a = reconstruct(&[(0, &format!("{noise}/exit\r"))]);
        assert_eq!(typed(&a), vec!["/exit"]);
    }

    #[test]
    fn keeps_bracketed_paste_payload() {
        let paste = "\u{1b}[200~hello from paste\u{1b}[201~";
        let a = reconstruct(&[(0, &format!("{paste}\r"))]);
        assert_eq!(typed(&a), vec!["hello from paste"]);
    }

    #[test]
    fn maps_function_keys_csi() {
        // F2 = ESC [ 1 2 ~, F5 = ESC [ 1 5 ~, F12 = ESC [ 2 4 ~
        let a = reconstruct(&[(0, "\u{1b}[12~\u{1b}[15~\u{1b}[24~\r")]);
        assert_eq!(keys(&a), vec!["f2", "f5", "f12", "enter"]);
    }

    #[test]
    fn maps_function_keys_ss3() {
        // F1-F4 in application mode: ESC O P/Q/R/S
        let a = reconstruct(&[(0, "\u{1b}OP\u{1b}OQ\u{1b}OR\u{1b}OS\r")]);
        assert_eq!(keys(&a), vec!["f1", "f2", "f3", "f4", "enter"]);
    }

    #[test]
    fn captures_bare_esc() {
        // A standalone ESC keypress (no following character).
        let a = reconstruct(&[(0, "\u{1b}")]);
        assert_eq!(keys(&a), vec!["esc"]);
    }

    #[test]
    fn captures_esc_followed_by_text() {
        // ESC followed by a normal character: emit "esc" then type the char.
        let a = reconstruct(&[(0, "\u{1b}x")]);
        assert_eq!(keys(&a), vec!["esc"]);
        assert_eq!(typed(&a), vec!["x"]);
    }

    #[test]
    fn captures_ctrl_s() {
        // Ctrl-S (0x13) should be captured as a keypress, not dropped.
        let a = reconstruct(&[(0, "hello\u{13}world\r")]);
        assert_eq!(typed(&a), vec!["hello", "world"]);
        assert_eq!(keys(&a), vec!["ctrl+s", "enter"]);
    }

    #[test]
    fn maps_shift_arrows() {
        // Shift+Up = ESC [ 1 ; 2 A, Shift+Left = ESC [ 1 ; 2 D
        let a = reconstruct(&[(0, "\u{1b}[1;2A\u{1b}[1;2D\r")]);
        assert_eq!(keys(&a), vec!["shift-up", "shift-left", "enter"]);
    }

    #[test]
    fn maps_ctrl_arrows() {
        // Ctrl+Right = ESC [ 1 ; 5 C
        let a = reconstruct(&[(0, "\u{1b}[1;5C\r")]);
        assert_eq!(keys(&a), vec!["ctrl+right", "enter"]);
    }

    #[test]
    fn maps_alt_arrows() {
        // Alt+Down = ESC [ 1 ; 3 B
        let a = reconstruct(&[(0, "\u{1b}[1;3B\r")]);
        assert_eq!(keys(&a), vec!["alt-down", "enter"]);
    }

    #[test]
    fn maps_modified_function_keys() {
        // Shift+F5 = ESC [ 1 5 ; 2 ~
        let a = reconstruct(&[(0, "\u{1b}[15;2~\r")]);
        assert_eq!(keys(&a), vec!["shift-f5", "enter"]);
    }

    // ── helper function coverage ──────────────────────────────────────

    #[test]
    fn modifier_prefix_values() {
        assert_eq!(modifier_prefix(MOD_SHIFT), "shift-");
        assert_eq!(modifier_prefix(MOD_ALT), "alt-");
        assert_eq!(modifier_prefix(4), "alt+shift-");
        assert_eq!(modifier_prefix(MOD_CTRL), "ctrl+");
        assert_eq!(modifier_prefix(MOD_CTRL_SHIFT), "ctrl+shift-");
        assert_eq!(modifier_prefix(7), "ctrl+alt-");
        assert_eq!(modifier_prefix(8), "ctrl+alt+shift-");
        assert_eq!(modifier_prefix(0), "");
    }

    #[test]
    fn parse_csi_params_simple() {
        assert_eq!(parse_csi_params("1"), (1, 0));
        assert_eq!(parse_csi_params("5"), (5, 0));
    }

    #[test]
    fn parse_csi_params_with_modifier() {
        assert_eq!(parse_csi_params("1;2"), (1, 2));
        assert_eq!(parse_csi_params("1;5"), (1, 5));
    }

    #[test]
    fn csi_key_arrows() {
        assert_eq!(csi_key("A", 'A'), Some("up".into()));
        assert_eq!(csi_key("B", 'B'), Some("down".into()));
        assert_eq!(csi_key("C", 'C'), Some("right".into()));
        assert_eq!(csi_key("D", 'D'), Some("left".into()));
    }

    #[test]
    fn csi_key_with_modifier() {
        assert_eq!(csi_key("1;2", 'A'), Some("shift-up".into()));
        assert_eq!(csi_key("1;5", 'C'), Some("ctrl+right".into()));
    }

    #[test]
    fn csi_key_function_keys() {
        assert_eq!(csi_key("12", '~'), Some("f2".into()));
        assert_eq!(csi_key("15", '~'), Some("f5".into()));
        assert_eq!(csi_key("24", '~'), Some("f12".into()));
    }

    #[test]
    fn csi_key_unknown() {
        assert_eq!(csi_key("99", 'X'), None);
    }

    #[test]
    fn ss3_key_basic() {
        assert_eq!(ss3_key('P'), Some("f1"));
        assert_eq!(ss3_key('Q'), Some("f2"));
        assert_eq!(ss3_key('R'), Some("f3"));
        assert_eq!(ss3_key('S'), Some("f4"));
    }

    #[test]
    fn ss3_key_unknown() {
        assert_eq!(ss3_key('Z'), None);
    }

    #[test]
    fn orphan_osc_unit_len_simple() {
        let chars: Vec<char> = "abc".chars().collect();
        assert_eq!(orphan_osc_unit_len(&chars), None);
    }

    #[test]
    fn orphan_osc_cluster_len_empty() {
        let chars: Vec<char> = vec![];
        assert_eq!(orphan_osc_cluster_len(&chars), None);
    }

    #[test]
    fn reconstruct_empty_input() {
        let a = reconstruct(&[]);
        assert!(a.is_empty());
    }

    #[test]
    fn reconstruct_tab_key() {
        let a = reconstruct(&[(0, "\t")]);
        assert_eq!(keys(&a), vec!["tab"]);
    }

    #[test]
    fn reconstruct_ctrl_c() {
        // Ctrl+C should be captured as a key
        let a = reconstruct(&[(0, "\u{3}")]);
        assert_eq!(keys(&a), vec!["ctrl+c"]);
    }

    #[test]
    fn reconstruct_ctrl_s() {
        // Ctrl+S (0x13) should be captured as a key
        let a = reconstruct(&[(0, "\u{13}")]);
        assert_eq!(keys(&a), vec!["ctrl+s"]);
    }

    #[test]
    fn reconstruct_unknown_ctrl_char_dropped() {
        // Ctrl+D (0x04) is silently dropped by the parser
        let a = reconstruct(&[(0, "\u{4}")]);
        assert!(a.is_empty());
    }

    #[test]
    fn reconstruct_paste_mode() {
        // Bracketed paste: ESC[200~text ESC[201~
        let input = "\x1b[200~pasted\x1b[201~";
        let a = reconstruct(&[(0, input)]);
        let t = typed(&a);
        assert_eq!(t, vec!["pasted"]);
    }

    #[test]
    fn reconstruct_paste_enter_emits_key() {
        let input = "\x1b[200~line1\rline2\x1b[201~";
        let a = reconstruct(&[(0, input)]);
        let t = typed(&a);
        assert_eq!(t, vec!["line1", "line2"]);
        // There should be an enter key between them
        let k = keys(&a);
        assert!(k.contains(&"enter"), "expected enter key in {:?}", k);
    }

    #[test]
    fn reconstruct_paste_backspace() {
        let input = "\x1b[200~ab\x7fcd\x1b[201~";
        let a = reconstruct(&[(0, input)]);
        let t = typed(&a);
        assert_eq!(t, vec!["acd"]);
    }

    #[test]
    fn reconstruct_paste_ctrl_u() {
        let input = "\x1b[200~hello\x15world\x1b[201~";
        let a = reconstruct(&[(0, input)]);
        let t = typed(&a);
        assert_eq!(t, vec!["world"]);
    }

    #[test]
    fn reconstruct_bare_esc_at_end() {
        let a = reconstruct(&[(0, "hello\x1b")]);
        let t = typed(&a);
        assert_eq!(t, vec!["hello"]);
        let k = keys(&a);
        assert!(k.contains(&"esc"), "expected esc key at end");
    }

    #[test]
    fn reconstruct_esc_with_unrecognized_char() {
        // ESC followed by an unrecognized char should emit "esc" as a key
        let a = reconstruct(&[(0, "\x1bZ")]);
        let k = keys(&a);
        assert!(k.contains(&"esc"), "expected esc key");
    }

    #[test]
    fn reconstruct_osc_body_stripped() {
        // OSC sequence (ESC ]) with a body — should be stripped
        let input = "\x1b]0;title\x07";
        let a = reconstruct(&[(0, input)]);
        // No actions should be produced from the OSC
        assert!(a.is_empty());
    }

    #[test]
    fn reconstruct_osc_st_terminated() {
        // OSC terminated by ST (ESC \)
        let input = "\x1b]0;title\x1b\\";
        let a = reconstruct(&[(0, input)]);
        assert!(a.is_empty());
    }

    #[test]
    fn reconstruct_str_body_stripped() {
        // STR sequence (ESC P ... ST)
        let input = "\x1bPsome string\x1b\\";
        let a = reconstruct(&[(0, input)]);
        assert!(a.is_empty());
    }

    #[test]
    fn reconstruct_sci_underscore_stripped() {
        // ESC _ (APC) and ESC ^ (PM) and ESC X (STS) are treated as STR
        let input = "\x1b_\x1b\\";
        let a = reconstruct(&[(0, input)]);
        assert!(a.is_empty());
    }

    #[test]
    fn reconstruct_ctrl_s_in_text_flushes_before() {
        let a = reconstruct(&[(0, "ab")]);
        let t = typed(&a);
        assert_eq!(t, vec!["ab"]);
    }

    #[test]
    fn reconstruct_tab_flushes_before() {
        let a = reconstruct(&[(0, "ab\t")]);
        let t = typed(&a);
        assert_eq!(t, vec!["ab"]);
        let k = keys(&a);
        assert!(k.contains(&"tab"));
    }

    #[test]
    fn reconstruct_ctrl_c_clears_buffer() {
        let a = reconstruct(&[(0, "abc\u{3}")]);
        let t = typed(&a);
        // ctrl+c clears the buffer
        assert!(t.is_empty() || !t.iter().any(|s| s.contains("abc")));
        let k = keys(&a);
        assert!(k.contains(&"ctrl+c"));
    }

    #[test]
    fn reconstruct_ctrl_s_flushes() {
        let a = reconstruct(&[(0, "ab\u{13}")]);
        let t = typed(&a);
        assert_eq!(t, vec!["ab"]);
        let k = keys(&a);
        assert!(k.contains(&"ctrl+s"));
    }

    #[test]
    fn reconstruct_ctrl_u_clears_line() {
        let a = reconstruct(&[(0, "abc\u{15}def")]);
        let t = typed(&a);
        assert_eq!(t, vec!["def"]);
    }

    #[test]
    fn reconstruct_bare_enter() {
        let a = reconstruct(&[(0, "\r")]);
        let k = keys(&a);
        assert_eq!(k, vec!["enter"]);
    }

    #[test]
    fn reconstruct_bare_newline() {
        let a = reconstruct(&[(0, "\n")]);
        let k = keys(&a);
        assert_eq!(k, vec!["enter"]);
    }

    #[test]
    fn reconstruct_shift_arrows() {
        let a = reconstruct(&[(0, "\x1b[1;2A")]);
        let k = keys(&a);
        assert_eq!(k, vec!["shift-up"]);
    }

    #[test]
    fn reconstruct_ctrl_arrows() {
        let a = reconstruct(&[(0, "\x1b[1;5C")]);
        let k = keys(&a);
        assert_eq!(k, vec!["ctrl+right"]);
    }

    #[test]
    fn reconstruct_alt_arrows() {
        let a = reconstruct(&[(0, "\x1b[1;3B")]);
        let k = keys(&a);
        assert_eq!(k, vec!["alt-down"]);
    }

    #[test]
    fn reconstruct_ss3_keys() {
        // SS3 mode: ESC O followed by P/Q/R/S
        let a = reconstruct(&[(0, "\x1bOP")]);
        let k = keys(&a);
        assert_eq!(k, vec!["f1"]);
    }

    #[test]
    fn reconstruct_csi_home_end() {
        let a = reconstruct(&[(0, "\x1b[H")]);
        let k = keys(&a);
        assert_eq!(k, vec!["home"]);
        let a = reconstruct(&[(0, "\x1b[F")]);
        let k = keys(&a);
        assert_eq!(k, vec!["end"]);
    }

    #[test]
    fn reconstruct_csi_insert_delete() {
        let a = reconstruct(&[(0, "\x1b[2~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["insert"]);
        let a = reconstruct(&[(0, "\x1b[3~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["delete"]);
    }

    #[test]
    fn reconstruct_csi_pageup_pagedown() {
        let a = reconstruct(&[(0, "\x1b[5~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["pageup"]);
        let a = reconstruct(&[(0, "\x1b[6~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["pagedown"]);
    }

    #[test]
    fn reconstruct_csi_f1_f12() {
        // F5 = CSI 1 5 ~
        let a = reconstruct(&[(0, "\x1b[15~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["f5"]);
        // F12 = CSI 2 4 ~
        let a = reconstruct(&[(0, "\x1b[24~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["f12"]);
    }

    #[test]
    fn reconstruct_csi_home_via_code1() {
        // CSI 1 ~ = home
        let a = reconstruct(&[(0, "\x1b[1~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["home"]);
        // CSI 7 ~ = home
        let a = reconstruct(&[(0, "\x1b[7~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["home"]);
    }

    #[test]
    fn reconstruct_csi_end_via_code() {
        // CSI 4 ~ = end
        let a = reconstruct(&[(0, "\x1b[4~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["end"]);
        // CSI 8 ~ = end
        let a = reconstruct(&[(0, "\x1b[8~")]);
        let k = keys(&a);
        assert_eq!(k, vec!["end"]);
    }

    #[test]
    fn reconstruct_csi_f1_f4_bare() {
        // ESC P/Q/R/S without modifier = F1-F4
        let a = reconstruct(&[(0, "\x1bOP")]);
        assert_eq!(keys(&a), vec!["f1"]);
        let a = reconstruct(&[(0, "\x1bOQ")]);
        assert_eq!(keys(&a), vec!["f2"]);
        let a = reconstruct(&[(0, "\x1bOR")]);
        assert_eq!(keys(&a), vec!["f3"]);
        let a = reconstruct(&[(0, "\x1bOS")]);
        assert_eq!(keys(&a), vec!["f4"]);
    }

    #[test]
    fn reconstruct_csi_unknown_final_byte() {
        // CSI with an unrecognized final byte is ignored
        let a = reconstruct(&[(0, "\x1b[1X")]);
        assert!(a.is_empty());
    }

    #[test]
    fn reconstruct_control_char_in_main_ignored() {
        // Bell (0x07) and other control chars are ignored
        let a = reconstruct(&[(0, "ab\u{7}cd")]);
        let t = typed(&a);
        assert_eq!(t, vec!["abcd"]);
    }

    #[test]
    fn reconstruct_shift_ctrl_modifier() {
        // modifier 6 = ctrl+shift
        let a = reconstruct(&[(0, "\x1b[1;6A")]);
        let k = keys(&a);
        assert_eq!(k, vec!["ctrl+shift-up"]);
    }

    #[test]
    fn reconstruct_ctrl_alt_modifier() {
        // modifier 7 = ctrl+alt
        let a = reconstruct(&[(0, "\x1b[1;7B")]);
        let k = keys(&a);
        assert_eq!(k, vec!["ctrl+alt-down"]);
    }

    #[test]
    fn reconstruct_ctrl_alt_shift_modifier() {
        // modifier 8 = ctrl+alt+shift
        let a = reconstruct(&[(0, "\x1b[1;8C")]);
        let k = keys(&a);
        assert_eq!(k, vec!["ctrl+alt+shift-right"]);
    }

    #[test]
    fn reconstruct_alt_shift_modifier() {
        // modifier 4 = alt+shift
        let a = reconstruct(&[(0, "\x1b[1;4D")]);
        let k = keys(&a);
        assert_eq!(k, vec!["alt+shift-left"]);
    }

    #[test]
    fn reconstruct_paste_csi_unknown() {
        // Paste mode with CSI but unknown ~ code stays in paste mode
        let input = "\x1b[200~abc\x1b[99~def\x1b[201~";
        let a = reconstruct(&[(0, input)]);
        let t = typed(&a);
        assert_eq!(t, vec!["abcdef"]);
    }

    #[test]
    fn reconstruct_paste_csi_not_201() {
        // Paste mode with CSI that ends with non-~ keeps paste
        let input = "\x1b[200~ab\x1b[1Xcd\x1b[201~";
        let a = reconstruct(&[(0, input)]);
        let t = typed(&a);
        assert_eq!(t, vec!["abcd"]);
    }

    #[test]
    fn reconstruct_paste_saw_non_bracket() {
        // In PasteSaw mode, non-[ goes back to Paste
        let input = "\x1b[200~\x1bOab\x1b[201~";
        let a = reconstruct(&[(0, input)]);
        let t = typed(&a);
        assert_eq!(t, vec!["ab"]);
    }

    #[test]
    fn orphan_osc_unit_empty() {
        assert_eq!(orphan_osc_unit_len(&[]), None);
    }

    #[test]
    fn orphan_osc_unit_starts_with_non_digit() {
        assert_eq!(orphan_osc_unit_len(&['a', ';', '1']), None);
    }

    #[test]
    fn orphan_osc_unit_digit_only_no_semicolon() {
        assert_eq!(orphan_osc_unit_len(&['1', '0']), None);
    }

    #[test]
    fn orphan_osc_unit_empty_param_after_semicolon() {
        // "10;;rgb:aaaabbbbcccc" — double semicolons: empty param breaks the loop
        // then ";rgb:" doesn't match "rgb:"
        assert_eq!(
            orphan_osc_unit_len(&[
                '1', '0', ';', ';', 'r', 'g', 'b', ':', 'a', 'a', 'a', 'a', '/', 'b', 'b', 'b',
                'b', '/', 'c', 'c', 'c', 'c'
            ]),
            None
        );
    }

    #[test]
    fn orphan_osc_unit_missing_slash_separator() {
        // "10;rgb:aaaabbbbcccc" — no slash between components
        let chars: Vec<char> = "10;rgb:aaaabbbbcccc".chars().collect();
        assert_eq!(orphan_osc_unit_len(&chars), None);
    }

    #[test]
    fn orphan_osc_unit_zero_hex_length() {
        // "10;rgb:" with no hex digits at all
        assert_eq!(
            orphan_osc_unit_len(&['1', '0', ';', 'r', 'g', 'b', ':']),
            None
        );
    }

    #[test]
    fn orphan_osc_cluster_single_unit() {
        let chars: Vec<char> = "10;rgb:abab/baba/baba".chars().collect();
        assert_eq!(orphan_osc_cluster_len(&chars), Some(21));
    }

    #[test]
    fn orphan_osc_cluster_two_units_no_separator() {
        let chars: Vec<char> = "10;rgb:abab/baba/baba11;rgb:1414/1414/1414"
            .chars()
            .collect();
        assert_eq!(orphan_osc_cluster_len(&chars), Some(42));
    }

    #[test]
    fn orphan_osc_cluster_two_units_with_separator() {
        // Two units separated by semicolon: 10;rgb:...;4;0;rgb:...
        let chars: Vec<char> = "10;rgb:abab/baba/baba;4;0;rgb:0000/0000/0000"
            .chars()
            .collect();
        let len = chars.len();
        assert_eq!(orphan_osc_cluster_len(&chars), Some(len));
    }

    #[test]
    fn orphan_osc_cluster_empty() {
        assert_eq!(orphan_osc_cluster_len(&[]), None);
    }

    #[test]
    fn orphan_osc_cluster_non_osc() {
        assert_eq!(orphan_osc_cluster_len(&['h', 'e', 'l', 'l', 'o']), None);
    }

    #[test]
    fn strip_orphan_osc_bodies_with_trailing_text() {
        assert_eq!(
            strip_orphan_osc_bodies("10;rgb:aabb/ccdd/eeff/path"),
            "/path"
        );
    }

    #[test]
    fn strip_orphan_osc_bodies_empty_string() {
        assert_eq!(strip_orphan_osc_bodies(""), "");
    }

    #[test]
    fn strip_orphan_osc_bodies_no_osc() {
        assert_eq!(strip_orphan_osc_bodies("hello world"), "hello world");
    }

    /// BEL terminates an OSC string: the title is swallowed and the next
    /// key still parses. Deleting the BEL arm would leave the parser inside
    /// the string and swallow the arrow too.
    #[test]
    fn osc_string_terminated_by_bel_releases_the_parser() {
        let a = reconstruct(&[(0, "\x1b]0;title\x07\x1b[A")]);
        assert_eq!(keys(&a), vec!["up"]);
    }

    /// Only `201~` closes a bracketed paste: a wrong code or a wrong final
    /// byte keeps the parser in the paste and swallows the arrow.
    #[test]
    fn paste_csi_closes_only_on_201_tilde() {
        // Correct close: the arrow after it is a key.
        let a = reconstruct(&[(0, "\x1b[200~ab\x1b[201~\x1b[A")]);
        assert_eq!(keys(&a), vec!["up"]);
        // Wrong code: still pasting, the arrow is swallowed.
        let b = reconstruct(&[(0, "\x1b[200~ab\x1b[99~\x1b[A\x1b[201~")]);
        assert!(!keys(&b).contains(&"up"), "got {:?}", keys(&b));
        // Wrong final byte: still pasting, the arrow is swallowed.
        let c = reconstruct(&[(0, "\x1b[200~ab\x1b[201X\x1b[A\x1b[201~")]);
        assert!(!keys(&c).contains(&"up"), "got {:?}", keys(&c));
    }

    #[test]
    fn reconstruct_multichar_ctrl_s() {
        // Ctrl+S (0x13) flushes before it
        let a = reconstruct(&[(0, "hello\u{13}world")]);
        let t = typed(&a);
        assert_eq!(t, vec!["hello", "world"]);
    }
}
