//! `demo capture` — record a live interactive session into a raw macro, then
//! normalize it into a clean demo score.
//!
//! Spawns a shell in a PTY and bridges it to the real terminal (raw mode):
//! local stdin → PTY (recorded as `input` events), PTY → local stdout
//! (recorded as `output` events), each timestamped. The capture ends when the
//! shell exits or after `idle_timeout_ms` with no terminal output (SPEC §3.3).
//!
//! The module is split by responsibility:
//! - [`wizard`] — the interactive setup prompts (prompt style, canvas, fps, sources)
//! - [`session`] — the session lifecycle: PTY setup, watchdog, teardown, output
//! - [`bridge`] — the two I/O threads piping stdin/PTY, plus secret detection
//! - [`post`] — control-file commands, reveal draining, the faithful cast

mod bridge;
mod post;
mod session;
mod wizard;

pub use session::run;
pub use wizard::default_prompt;

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::error::{Error, Result};
use crate::model::RawEvent;

/// Marker the captured shell echoes once it's at our forced prompt — recording
/// starts after it, so the prompt-setup chatter is discarded. Assembled by the
/// shell so the typed command doesn't itself match (only the printed output).
const PROMPT_READY: &str = "demostage_capture_ready";

/// A resolved reveal requested during capture (via `demo focus` or `demo open`):
/// the 1–2 panes to show and how they're arranged, plus hold/scroll. The
/// `--when`/`--after` deferral is handled at the control layer, so by the time a
/// `Reveal` is recorded it fires *now*.
#[derive(Clone)]
struct Reveal {
    panes: Vec<crate::model::RevealPane>,
    orientation: crate::model::Orientation,
    hold_ms: Option<u64>,
    scroll: bool,
}

impl Reveal {
    /// Turn this reveal into the recorded event at time `t_ms`.
    fn to_event(&self, t_ms: u64) -> RawEvent {
        RawEvent::Reveal {
            t_ms,
            panes: self.panes.clone(),
            orientation: self.orientation,
            hold_ms: self.hold_ms,
            scroll: self.scroll,
        }
    }
    /// One-line summary for the debug log.
    fn summary(&self) -> String {
        let ids: Vec<&str> = self.panes.iter().map(|p| p.id.as_str()).collect();
        format!("{:?} ({:?})", ids, self.orientation)
    }
}

/// Reveals armed by `--when <pat>`, each with its cue pattern.
type PendingWhen = Arc<Mutex<Vec<(Reveal, String)>>>;
/// Reveals armed by `--after`, fired when the running command finishes.
type PendingAfter = Arc<Mutex<Vec<Reveal>>>;

/// How long the output must stay quiet, after a command produced output, before
/// an `--after` reveal fires (i.e. the shell is back at the prompt).
const AFTER_QUIET_MS: u64 = 800;

/// When a `demo open` wizard's start has to be inferred from its `open_begin`
/// marker (input detection missed the typed command), back-date the excision span
/// by this much so the wizard's first lines (already printed) are still removed.
const OPEN_BEGIN_BACKDATE_MS: u64 = 800;

/// Serializes tests that change the process working directory (creating a
/// control file / bare recording by relative name). Cargo runs tests as
/// threads in one process, so two chdir tests overlapping would land in
/// each other's directory.
#[cfg(test)]
pub(super) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ms(t0: Instant) -> u64 {
    t0.elapsed().as_millis() as u64
}

/// A timestamped diagnostic log written when `record --debug` is set. Both I/O
/// threads write to it, so it lives behind a mutex.
struct DebugLog {
    file: Mutex<std::fs::File>,
    t0: Instant,
}

impl DebugLog {
    fn create(path: &std::path::Path, t0: Instant) -> Result<Self> {
        let file = std::fs::File::create(path).map_err(|e| Error::io(path, e))?;
        Ok(DebugLog {
            file: Mutex::new(file),
            t0,
        })
    }

    /// Write one timestamped line.
    fn note(&self, msg: &str) {
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "[+{:>8}ms] {msg}", self.t0.elapsed().as_millis());
            let _ = f.flush();
        }
    }

    /// Log a byte chunk in both escaped-text and hex form — the escaped form
    /// makes control bytes (arrows = `\u{1b}[A`, etc.) visible at a glance.
    fn chunk(&self, dir: &str, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let hex: String = bytes.iter().map(|b| format!("{b:02x} ")).collect();
        self.note(&format!(
            "{dir} {:>4}B  repr={text:?}  hex=[{}]",
            bytes.len(),
            hex.trim_end()
        ));
    }
}

/// Everything a running capture shares between its watchdog, its control-file
/// reader and its two bridge threads. The struct is cloned freely; every clone
/// points at the same allocations, so a write in one thread is seen by the rest.
#[derive(Clone)]
struct CaptureState {
    /// The recording being built, in order.
    events: Arc<Mutex<Vec<RawEvent>>>,
    /// When a PTY/stdin byte last moved — what the idle timeout watches.
    last_activity: Arc<Mutex<Instant>>,
    /// Set when the PTY reader sees EOF: the shell exited.
    shell_exited: Arc<AtomicBool>,
    /// Set at stop time; the input thread checks it before each read.
    stop: Arc<AtomicBool>,
    /// Set while a password/passphrase prompt shows: input typed during it is
    /// forwarded to the PTY but NEVER recorded.
    sensitive: Arc<AtomicBool>,
    /// The text of the secret prompt currently showing (e.g. `Vault passphrase:`),
    /// captured so a `Secret` event can record WHICH secret was entered — never the
    /// value. Set by the output thread, consumed by the input thread on Enter.
    secret_prompt: Arc<Mutex<Option<String>>>,
    /// Set by the output thread when a non-secret line is seen after a secret
    /// prompt was detected; the input thread reads it on Enter to decide whether
    /// the dedup guard can be cleared (the prompt has left the screen).
    secret_prompt_cleared: Arc<AtomicBool>,
    /// Browser reveals armed by `demo open --when <pat>`, fired by the output
    /// thread when the pattern appears.
    pending_opens: PendingWhen,
    /// Reveals armed by `demo open --after`: fired when the current foreground
    /// command finishes (produces output, then goes quiet, back at the prompt).
    after_opens: PendingAfter,
    /// An `--after` command is in flight; `after_last_out` is its last output.
    after_running: Arc<AtomicBool>,
    after_last_out: Arc<Mutex<Instant>>,
    /// True while a control command (`demo open` / `demo stop`) typed inside the
    /// capture is running: its output (wizard, confirmation) is NOT recorded, so
    /// it never leaks into the demo.
    muting: Arc<AtomicBool>,
    mute_since: Arc<Mutex<Instant>>,
    /// Recorded meta-command spans `(start_ms, end_ms)` for the finished demo:
    /// each `demo open` (its echo + in-session wizard) is excised in post. The
    /// span's start is held in `mute_start` until the control command arrives.
    mute_spans: Arc<Mutex<Vec<(u64, u64)>>>,
    mute_start: Arc<Mutex<Option<u64>>>,
    /// Recording may begin: the forced prompt echoed the readiness marker (or no
    /// prompt was forced), or the readiness watchdog gave up waiting.
    ready: Arc<AtomicBool>,
    /// Optional diagnostic log (`--debug`), attached once the args are read.
    debug: Option<Arc<DebugLog>>,
}

impl CaptureState {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            last_activity: Arc::new(Mutex::new(Instant::now())),
            shell_exited: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            sensitive: Arc::new(AtomicBool::new(false)),
            secret_prompt: Arc::new(Mutex::new(None)),
            secret_prompt_cleared: Arc::new(AtomicBool::new(false)),
            pending_opens: Arc::new(Mutex::new(Vec::new())),
            after_opens: Arc::new(Mutex::new(Vec::new())),
            after_running: Arc::new(AtomicBool::new(false)),
            after_last_out: Arc::new(Mutex::new(Instant::now())),
            muting: Arc::new(AtomicBool::new(false)),
            mute_since: Arc::new(Mutex::new(Instant::now())),
            mute_spans: Arc::new(Mutex::new(Vec::new())),
            mute_start: Arc::new(Mutex::new(None)),
            ready: Arc::new(AtomicBool::new(false)),
            debug: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_log_create_and_note() {
        let dir = std::env::temp_dir().join(format!("dbg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("debug.log");
        let t0 = Instant::now();
        let log = DebugLog::create(&log_path, t0).unwrap();
        log.note("hello world");
        log.chunk("PTY→", b"test\x1b[A");
        drop(log);
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("hello world"));
        assert!(content.contains("PTY→"));
        assert!(content.contains("test"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn debug_log_chunk_hex_format() {
        let dir = std::env::temp_dir().join(format!("dbg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("debug.log");
        let t0 = Instant::now();
        let log = DebugLog::create(&log_path, t0).unwrap();
        log.chunk("IN", b"\x1b[32m");
        drop(log);
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("hex="));
        assert!(content.contains("1b "));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn debug_log_empty_chunk() {
        let dir = std::env::temp_dir().join(format!("dbg3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("debug.log");
        let t0 = Instant::now();
        let log = DebugLog::create(&log_path, t0).unwrap();
        log.chunk("PTY→", b"");
        drop(log);
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("0B"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ms() returns elapsed milliseconds, not 1 and not micros: 1.5s ago
    /// reads ~1500. Kills `replace ms with 1`.
    #[test]
    fn ms_returns_elapsed_millis() {
        let t0 = Instant::now() - std::time::Duration::from_millis(1500);
        let got = ms(t0);
        assert!(
            (1400..1700).contains(&got),
            "ms must be ~1500 for a 1.5s-old t0, got {got}"
        );
        // Monotonic: a later call reads >= an earlier one.
        assert!(ms(t0) >= got);
    }
}
