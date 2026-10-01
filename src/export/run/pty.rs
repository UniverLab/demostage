//! The PTY harness one score replay runs on: the shell it spawns, the pre-roll
//! that forces a clean prompt, the two typed-step helpers the timeline drives,
//! and the teardown that turns the captured output into a [`Recording`].

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use super::{Captures, Recording};
use crate::error::{Error, Result};
use crate::model::Score;
use crate::normalize::salt::humanize_delays;
use crate::normalize::Rng;

use super::{
    collect, default_prompt_for, drain_until_quiet, is_zsh, recent_contains, secret_needle,
    sh_single_quote, sleep_collecting, wait_for, wait_for_marker, CELL_H, CELL_W, EXIT_GRACE_MS,
    PREROLL_MAX_MS, PREROLL_QUIET_MS, READER_JOIN_MS,
};

/// Upper bound on [`pump_pty_output`] passes: at 4 KiB per chunk this is far
/// more output than any capture records, so only a mutated stop condition can
/// ever reach it.
const MAX_PUMP_CHUNKS: usize = 1 << 20;

/// Poll cadence of [`finish`]'s reader-join window: one check/collect/sleep
/// round per tick, `READER_JOIN_MS / JOIN_POLL_MS` rounds in total.
const JOIN_POLL_MS: u64 = 20;

/// Poll rounds in [`finish`]'s bounded reader-join window. Pure so the `/`
/// is unit-testable: `*` would run 40_000 rounds instead of 50 (the wall-clock
/// backstop in the loop still ends them after the same window).
fn join_rounds() -> u64 {
    READER_JOIN_MS / JOIN_POLL_MS
}

/// Whether [`finish`]'s reader-join window has used up its wall clock. The
/// loop's round count is its stop condition; this is the backstop that keeps
/// the teardown bounded even when that count overstates the window. Pure so
/// both sides of the deadline are unit-testable.
fn join_window_expired(deadline: Instant) -> bool {
    deadline.saturating_duration_since(Instant::now()).is_zero()
}

/// The PTY a score's shell runs in for one capture: the child we shut down at
/// the end, the writer we type into, the channel the reader thread delivers the
/// shell's output on, and the cell grid everything is sized to.
pub(super) struct CapturePty {
    pub(super) child: Box<dyn portable_pty::Child + Send + Sync>,
    pub(super) writer: Box<dyn Write + Send>,
    pub(super) rx: Receiver<(Instant, Vec<u8>)>,
    pub(super) reader_done: Arc<AtomicBool>,
    pub(super) cols: u16,
    pub(super) rows: u16,
    /// `PS1` (or `PROMPT` on zsh) — the variable the pre-roll forces.
    pub(super) ps_var: &'static str,
    pub(super) prompt: String,
}

/// Read the PTY until EOF, forwarding each chunk to the recorder's channel
/// (stopping early when the recorder went away), then flag the reader as done.
fn pump_pty_output(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<(Instant, Vec<u8>)>,
    reader_done: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 4096];
    // Bounded: one pass per forwarded chunk at most — a 4 KiB cap per chunk
    // means the bound is far beyond any real capture, and it keeps a reader
    // that never reports EOF from spinning here forever.
    for _ in 0..MAX_PUMP_CHUNKS {
        let Ok(n) = reader.read(&mut buf) else {
            break;
        };
        if n == 0 {
            break;
        }
        if tx.send((Instant::now(), buf[..n].to_vec())).is_err() {
            break;
        }
    }
    reader_done.store(true, Ordering::SeqCst);
}

impl CapturePty {
    /// Open the PTY sized to `pane`, start the shell with the score's prompt
    /// forced, and start the reader thread that collects its output.
    pub(super) fn new(score: &Score, pane: &crate::model::Pane) -> Result<Self> {
        let cols = (pane.width / CELL_W).clamp(1, 1000) as u16;
        let rows = (pane.height / CELL_H).clamp(1, 1000) as u16;

        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Export(format!("openpty: {e}")))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let prompt = score
            .demo
            .prompt
            .as_deref()
            .map(|p| p.to_string())
            .unwrap_or_else(|| default_prompt_for(&shell));
        let mut cmd = CommandBuilder::new(&shell);
        let ps_var = if is_zsh(&shell) { "PROMPT" } else { "PS1" };
        cmd.env(ps_var, &prompt);
        cmd.env("PS2", "> ");
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| Error::Export(format!("spawn {shell}: {e}")))?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| Error::Export(format!("pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| Error::Export(format!("pty writer: {e}")))?;

        let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
        let reader_done = Arc::new(AtomicBool::new(false));
        let reader_done_for_thread = reader_done.clone();
        thread::spawn(move || pump_pty_output(reader, tx, reader_done_for_thread));

        Ok(Self {
            child,
            writer,
            rx,
            reader_done,
            cols,
            rows,
            ps_var,
            prompt,
        })
    }

    /// ── Pre-roll (untimed): force a clean prompt + run setup, then discard. ──
    pub(super) fn pre_roll(&mut self, score: &Score) {
        if let Some(setup) = score.env.as_ref().and_then(|e| e.setup_script.as_deref()) {
            let _ = writeln!(self.writer, "{setup}");
        }
        // Force the prompt after the rc files (which usually set their own PS1), so a
        // demo never leaks `user@host`. Other env (tokens, etc.) is inherited.
        let _ = writeln!(
            self.writer,
            "{}={}; clear",
            self.ps_var,
            sh_single_quote(&self.prompt)
        );
        // A readiness marker makes the start deterministic: wait until the shell
        // echoes it back, which proves all rc/startup chatter and our setup have
        // flushed. A fixed delay (or plain quiet-detection) is racy on slow shells
        // (e.g. WSL) where startup output arrives *after* the silence we'd wait out,
        // leaking `user@host`/`PS1=…` into the demo.
        // The printed marker is assembled by the shell so it differs from the typed
        // command text — otherwise we'd match the PTY's *instant* line-discipline
        // echo of the command (15 ms) instead of the shell actually running it.
        let _ = writeln!(self.writer, "printf 'demostage_%s_ready\\n' OK");
        wait_for_marker(&self.rx, "demostage_OK_ready", PREROLL_MAX_MS);
        // Discard the marker's own output and the prompt that follows it.
        drain_until_quiet(&self.rx, PREROLL_QUIET_MS, PREROLL_MAX_MS);
    }
}

/// Whether a keystroke delay needs a real sleep: zero is a no-op. Pure so
/// the boundary is unit-testable without timing a live PTY.
fn should_sleep(delay_ms: u64) -> bool {
    delay_ms > 0
}

/// Deadline for the bounded shell-shutdown grace period. Pure so the
/// direction (future, not past) is unit-testable.
fn grace_deadline(grace_ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(grace_ms)
}

/// Whether a shutdown deadline has passed. Pure so the comparison direction
/// is unit-testable.
fn past_deadline(deadline: Instant) -> bool {
    Instant::now() >= deadline
}

/// Type `text` into the PTY with the score's humanized pacing, collecting the
/// output every keystroke produces.
pub(super) fn type_step(
    text: &str,
    human_salt: bool,
    typing: &crate::model::Typing,
    rng: &mut Rng,
    pty: &mut CapturePty,
    events: &mut Vec<(f64, String)>,
    t0: Instant,
) {
    let delays = if human_salt {
        humanize_delays(text, typing.base_ms, typing.salt_ms, rng)
    } else {
        vec![0; text.chars().count()]
    };
    let mut b = [0u8; 4];
    for (ch, d) in text.chars().zip(delays) {
        if should_sleep(d) {
            thread::sleep(Duration::from_millis(d));
        }
        let _ = pty.writer.write_all(ch.encode_utf8(&mut b).as_bytes());
        let _ = pty.writer.flush();
        collect(events, &pty.rx, t0);
    }
}

/// Supply the secret ONLY once the matching prompt is actually showing, so it
/// can never land in the wrong field (e.g. the repo name). Wait for the prompt
/// label to appear (or confirm it already printed), then type the value
/// collected up front (in memory only).
pub(super) fn secret_step(
    prompt: &str,
    secrets: &std::collections::HashMap<String, String>,
    pty: &mut CapturePty,
    events: &mut Vec<(f64, String)>,
    t0: Instant,
) {
    let needle = secret_needle(prompt);
    if !needle.is_empty() && !recent_contains(events, &needle) {
        wait_for(&needle, events, &pty.rx, t0);
    }
    sleep_collecting(150, events, &pty.rx, t0);
    if let Some(val) = secrets.get(prompt) {
        let _ = pty.writer.write_all(val.as_bytes());
    }
    let _ = pty.writer.write_all(b"\r");
    let _ = pty.writer.flush();
    sleep_collecting(150, events, &pty.rx, t0);
}

/// Write the teardown script, exit the shell (bounded — kill it after a grace
/// period so a lingering foreground process can't hang the export), drain the
/// last output, and build the [`Recording`].
pub(super) fn finish(
    score: &Score,
    pty: CapturePty,
    settle_end: f64,
    t0: Instant,
    caps: Captures,
) -> Recording {
    let Captures {
        mut events,
        captions,
        focuses,
    } = caps;

    // ── Settle: hold after the last step until output goes quiet, so the final
    // result (a command's output, an error) finishes rendering and is held on
    // screen — rather than being cut off by a fixed timer. ──────────────────
    let CapturePty {
        mut child,
        mut writer,
        rx,
        reader_done,
        cols,
        rows,
        ..
    } = pty;

    // ── Teardown + close. ──────────────────────────────────────────────────
    if let Some(td) = score
        .env
        .as_ref()
        .and_then(|e| e.teardown_script.as_deref())
    {
        let _ = writeln!(writer, "{td} >/dev/null 2>&1");
    }
    let _ = writer.write_all(b"\nexit\n");
    let _ = writer.flush();
    drop(writer);

    // Bounded shutdown: give the shell a grace period to exit on its own, then
    // kill it. Without this, a demo whose last command leaves a process in the
    // foreground hangs export indefinitely.
    let deadline = grace_deadline(EXIT_GRACE_MS);
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        if past_deadline(deadline) {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    // Drain trailing output, waiting — bounded — for the capture thread to reach
    // EOF. A stray foreground process can keep the PTY open, so we never join it
    // unconditionally; the thread is detached and reaped when the process exits.
    thread::sleep(Duration::from_millis(50));
    // Bounded join: check `reader_done` FIRST each round, then collect and
    // sleep, for at most READER_JOIN_MS / JOIN_POLL_MS rounds. Equivalent to
    // the old `while !done && now < join_deadline { collect; sleep(20) }`
    // loop: same first-check ordering (a reader that already finished skips
    // straight to the unconditional collect below), the same 20 ms cadence
    // over the same READER_JOIN_MS window (~50 rounds either way), and the
    // same early exit the moment the flag flips — the round count is the stop
    // condition, and the wall-clock deadline below is the backstop that keeps
    // the teardown bounded even if the count itself is ever wrong.
    //
    // Both bounds stop at the same round in practice (50 polls x 20 ms), so
    // the deadline break below never fires before the count runs out: it only
    // ends the loop when the count overstates the window.
    let join_deadline = Instant::now() + Duration::from_millis(READER_JOIN_MS);
    for _ in 0..join_rounds() {
        if reader_done.load(Ordering::SeqCst) {
            break;
        }
        collect(&mut events, &rx, t0);
        if join_window_expired(join_deadline) {
            break;
        }
        thread::sleep(Duration::from_millis(JOIN_POLL_MS));
    }
    collect(&mut events, &rx, t0);

    // Drop the post-settle teardown noise (the `exit` echo) and hold the final
    // frame for the settle's quiet period.
    events.retain(|(t, _)| *t <= settle_end);
    let duration = settle_end.max(events.last().map(|(t, _)| *t).unwrap_or(0.0)) + 0.2;
    Recording {
        cols,
        rows,
        title: score.demo.name.clone(),
        events,
        captions,
        focuses,
        duration,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pump_pty_output_forwards_chunks_until_eof() {
        let (tx, rx) = mpsc::channel();
        let done = Arc::new(AtomicBool::new(false));
        let reader = std::io::Cursor::new(b"hello\r\nworld".to_vec());

        pump_pty_output(Box::new(reader), tx, done.clone());

        assert!(done.load(Ordering::SeqCst), "reader must flag itself done");
        let chunks: Vec<Vec<u8>> = rx.try_iter().map(|(_, b)| b).collect();
        // Exactly the data, exactly once: no empty frames at EOF, none dropped.
        assert_eq!(chunks, vec![b"hello\r\nworld".to_vec()]);
    }

    #[test]
    fn pump_pty_output_stops_when_the_recorder_went_away() {
        let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
        drop(rx);
        let done = Arc::new(AtomicBool::new(false));

        pump_pty_output(
            Box::new(std::io::Cursor::new(b"x".to_vec())),
            tx,
            done.clone(),
        );

        assert!(done.load(Ordering::SeqCst));
    }

    /// The portable-pty path a replay runs on: open a PTY, spawn the shell,
    /// type into it, and read back what it printed (the reader must hit EOF
    /// once the shell exits).
    #[test]
    fn pty_round_trip_delivers_the_shells_output() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "read line; printf 'demostage_%s\\n' \"$line\""]);
        let mut child = pair.slave.spawn_command(cmd).expect("spawn sh");
        drop(pair.slave);

        let mut writer = pair.master.take_writer().expect("pty writer");
        writer.write_all(b"pty-ok\r").expect("type into the pty");
        writer.flush().expect("flush the pty");

        let mut reader = pair.master.try_clone_reader().expect("pty reader");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut out = Vec::new();
            let mut buf = [0u8; 256];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                }
            }
            let _ = tx.send(out);
        });
        let out = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the reader should reach EOF once the shell exits");
        let _ = child.wait();

        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains("demostage_pty-ok"),
            "expected the echoed line back, got {text:?}"
        );
    }

    // --- mutant-killing tests: sleep/deadline seams, pre_roll drain,
    // type/secret delivery, teardown redirect ---

    #[test]
    fn should_sleep_only_above_zero() {
        assert!(!should_sleep(0), "zero delay needs no sleep");
        assert!(should_sleep(1));
        assert!(should_sleep(200));
    }

    #[test]
    fn grace_deadline_lies_in_the_future() {
        let before = Instant::now();
        let deadline = grace_deadline(2_000);
        assert!(deadline > before, "+ must point forward, not into the past");
        assert!(deadline <= Instant::now() + Duration::from_millis(2_100));
    }

    #[test]
    fn past_deadline_compares_in_the_right_direction() {
        assert!(past_deadline(Instant::now() - Duration::from_secs(1)));
        assert!(!past_deadline(Instant::now() + Duration::from_secs(60)));
    }

    fn pty_score() -> crate::model::Score {
        toml::from_str(
            r#"
[demo]
name = "pty"
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
"#,
        )
        .unwrap()
    }

    /// Poll the PTY until `needle` shows up in `events` (or the budget runs
    /// out): shell echo timing varies under load, so collect with a budget
    /// rather than asserting on one fixed sleep.
    fn poll_for(
        events: &mut Vec<(f64, String)>,
        rx: &std::sync::mpsc::Receiver<(Instant, Vec<u8>)>,
        t0: Instant,
        needle: &str,
        budget_ms: u64,
    ) -> bool {
        let until = Instant::now() + Duration::from_millis(budget_ms);
        while Instant::now() < until {
            super::super::sleep_collecting(50, events, rx, t0);
            if events.iter().any(|(_, d)| d.contains(needle)) {
                return true;
            }
        }
        events.iter().any(|(_, d)| d.contains(needle))
    }

    #[test]
    fn pre_roll_discards_previously_queued_output() {
        let score = pty_score();
        let mut pty = CapturePty::new(&score, &score.layout.panes[0]).unwrap();
        // Queue known output before the pre-roll: it must be gone after.
        use std::io::Write;
        pty.writer.write_all(b"echo PRE_ROLL_NOISE123\n").unwrap();
        pty.writer.flush().unwrap();
        std::thread::sleep(Duration::from_millis(400));
        pty.pre_roll(&score);
        assert!(
            pty.rx.try_recv().is_err(),
            "pre-roll must drain everything queued before it"
        );
    }

    #[test]
    fn type_step_delivers_characters_to_the_shell() {
        let score = pty_score();
        let mut pty = CapturePty::new(&score, &score.layout.panes[0]).unwrap();
        pty.pre_roll(&score);
        let t0 = Instant::now();
        let mut events = Vec::new();
        let typing = crate::model::Typing::default();
        let mut rng = super::super::Rng::new(0);
        type_step("zxq", false, &typing, &mut rng, &mut pty, &mut events, t0);
        assert!(
            poll_for(&mut events, &pty.rx, t0, "zxq", 5_000),
            "typed characters must echo back, got {events:?}"
        );
    }

    #[test]
    fn secret_step_skips_the_wait_for_empty_or_seen_needles() {
        let score = pty_score();
        let mut pty = CapturePty::new(&score, &score.layout.panes[0]).unwrap();
        pty.pre_roll(&score);
        let t0 = Instant::now();
        // Empty needle: no wait. Deleting the first `!` would wait 15 s.
        let start = Instant::now();
        secret_step(
            "",
            &std::collections::HashMap::new(),
            &mut pty,
            &mut Vec::new(),
            t0,
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "empty needle must skip the wait"
        );
        // Seen needle: no wait. Deleting the second `!` would wait 15 s.
        let mut events = vec![(0.0, "Password: prompt showing".to_string())];
        let start = Instant::now();
        secret_step(
            "Password:",
            &std::collections::HashMap::new(),
            &mut pty,
            &mut events,
            t0,
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "already-seen needle must skip the wait"
        );
    }

    #[test]
    fn secret_step_types_the_collected_value() {
        let score = pty_score();
        let mut pty = CapturePty::new(&score, &score.layout.panes[0]).unwrap();
        pty.pre_roll(&score);
        let t0 = Instant::now();
        let mut secrets = std::collections::HashMap::new();
        secrets.insert("Password:".to_string(), "s3cr3t-test-value".to_string());
        let mut events = vec![(0.0, "Password: prompt showing".to_string())];
        secret_step("Password:", &secrets, &mut pty, &mut events, t0);
        assert!(
            poll_for(&mut events, &pty.rx, t0, "s3cr3t-test-value", 5_000),
            "the collected secret must be typed, got {events:?}"
        );
    }

    #[test]
    fn finish_runs_teardown_quietly_and_keeps_pre_settle_events() {
        let score: crate::model::Score = toml::from_str(
            r#"
[demo]
name = "fin"
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
[env]
teardown_script = "echo TEARDOWN_NOISE_456"
"#,
        )
        .unwrap();
        let mut pty = CapturePty::new(&score, &score.layout.panes[0]).unwrap();
        pty.pre_roll(&score);
        let t0 = Instant::now();
        let caps = super::super::Captures {
            events: vec![(0.1, "early".to_string())],
            captions: vec![],
            focuses: vec![],
        };
        // settle_end far in the future: nothing is cut, teardown noise (after
        // it, redirected) never lands in events.
        let rec = finish(&score, pty, 60.0, t0, caps);
        assert_eq!(rec.title, "fin");
        assert!(
            rec.events.iter().any(|(_, d)| d == "early"),
            "pre-settle kept"
        );
        // The teardown command line itself is echoed by the PTY (expected),
        // but its OUTPUT must be redirected away: no bare output line.
        assert!(
            !rec.events
                .iter()
                .any(|(_, d)| d.contains("TEARDOWN_NOISE_456") && !d.contains("echo ")),
            "teardown output must be redirected away, got {:?}",
            rec.events.iter().map(|(_, d)| d).collect::<Vec<_>>()
        );
    }

    // --- mutant-killing tests on a hand-built CapturePty: exact bytes for
    // pre-roll/type/secret, the wait gate in secret_step, and finish's
    // settle filter, duration arithmetic, and bounded reader join. ---

    /// Writer backed by a shared buffer, so tests can assert the exact bytes
    /// written through the `Box<dyn Write + Send>` after it is consumed.
    #[derive(Clone, Default)]
    struct SharedBytes(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for SharedBytes {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SharedBytes {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    /// Fake child that reports "already exited" on the first `try_wait`, so
    /// `finish`'s kill loop breaks immediately instead of burning its grace
    /// period.
    #[derive(Debug)]
    struct ExitedChild;

    impl portable_pty::ChildKiller for ExitedChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(ExitedChild)
        }
    }

    impl portable_pty::Child for ExitedChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            Ok(Some(portable_pty::ExitStatus::with_exit_code(0)))
        }
        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Ok(portable_pty::ExitStatus::with_exit_code(0))
        }
        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    /// A `CapturePty` with no shell behind it: writes land in a shared buffer,
    /// and the returned sender feeds the recorder's channel directly. The
    /// reader-done flag starts false (no reader thread runs).
    fn fake_pty() -> (CapturePty, SharedBytes, mpsc::Sender<(Instant, Vec<u8>)>) {
        let writer = SharedBytes::default();
        let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
        let pty = CapturePty {
            child: Box::new(ExitedChild),
            writer: Box::new(writer.clone()),
            rx,
            reader_done: Arc::new(AtomicBool::new(false)),
            cols: 80,
            rows: 24,
            ps_var: "PS1",
            prompt: "$ ".to_string(),
        };
        (pty, writer, tx)
    }

    /// pre_roll must write the prompt-forcing primer and the printf marker
    /// line, byte for byte — a body replaced with `()` writes nothing.
    #[test]
    fn pre_roll_writes_the_primer_and_marker_lines_exactly() {
        let score = pty_score();
        let (mut pty, writer, tx) = fake_pty();
        // Pre-queue the marker so wait_for_marker returns on its first pass
        // (bounded: the drain below then costs only its quiet period).
        tx.send((Instant::now(), b"demostage_OK_ready\n".to_vec()))
            .unwrap();
        pty.pre_roll(&score);
        assert_eq!(
            writer.bytes(),
            b"PS1='$ '; clear\nprintf 'demostage_%s_ready\\n' OK\n",
            "pre-roll must force the prompt and print the readiness marker"
        );
    }

    /// No human salt → every delay is 0: the exact characters must reach the
    /// writer in order, and the echoed channel output must land in `events`.
    #[test]
    fn type_step_writes_the_text_and_collects_the_echo() {
        let (mut pty, writer, tx) = fake_pty();
        let t0 = Instant::now();
        tx.send((t0 + Duration::from_millis(500), b"ab".to_vec()))
            .unwrap();
        let mut events = Vec::new();
        let typing = crate::model::Typing {
            base_ms: 80,
            salt_ms: 15,
            seed: None,
        };
        let mut rng = Rng::new(0);
        type_step("ab", false, &typing, &mut rng, &mut pty, &mut events, t0);
        assert_eq!(writer.bytes(), b"ab", "exactly the typed bytes, in order");
        assert_eq!(
            events,
            vec![(0.5, "ab".to_string())],
            "the echoed output must be collected"
        );
    }

    /// Human salt with a positive base → the delays are > 0 and actually
    /// slept; the writer still receives exactly the text. Guards the
    /// `d > 0` boundary in both directions: skipping the sleep shows up as
    /// an elapsed time near zero, and the `should_sleep` unit test pins the
    /// zero side.
    #[test]
    fn type_step_sleeps_only_for_positive_humanized_delays() {
        let (mut pty, writer, _tx) = fake_pty();
        let t0 = Instant::now();
        let mut events = Vec::new();
        let typing = crate::model::Typing {
            base_ms: 5,
            salt_ms: 0,
            seed: None,
        };
        let mut rng = Rng::new(0);
        let start = Instant::now();
        type_step("abc", true, &typing, &mut rng, &mut pty, &mut events, t0);
        let elapsed = start.elapsed();
        assert_eq!(writer.bytes(), b"abc", "typed bytes must be exact");
        // Three 5 ms humanized delays: at least ~15 ms of real sleeping.
        assert!(
            elapsed >= Duration::from_millis(10),
            "positive delays must sleep, took {elapsed:?}"
        );
        assert!(events.is_empty(), "nothing was echoed on the channel");
    }

    /// The prompt label has NOT printed yet (events don't contain the needle)
    /// and only arrives on the channel 400 ms in: secret_step must wait for
    /// it. Deleting either `!` in the wait gate skips the wait, both 150 ms
    /// sleeps are over before the echo, and nothing lands in `events`.
    #[test]
    fn secret_step_waits_for_a_prompt_that_has_not_shown_up_yet() {
        let (mut pty, writer, tx) = fake_pty();
        let t0 = Instant::now();
        let mut events: Vec<(f64, String)> = Vec::new();
        let secrets = std::collections::HashMap::new();
        let feeder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(400));
            let _ = tx.send((Instant::now(), b"Vault passphrase: ".to_vec()));
        });
        let start = Instant::now();
        secret_step("Vault passphrase:", &secrets, &mut pty, &mut events, t0);
        let elapsed = start.elapsed();
        feeder.join().expect("feeder thread must finish");
        assert!(
            events.iter().any(|(_, d)| d.contains("Vault passphrase")),
            "the awaited prompt must be collected while waiting, got {events:?}"
        );
        assert_eq!(
            writer.bytes(),
            b"\r",
            "nothing may be typed before the prompt appears"
        );
        assert!(
            elapsed < Duration::from_secs(14),
            "the wait must stay bounded, took {elapsed:?}"
        );
    }

    /// finish keeps only events at or before settle_end (the boundary event
    /// itself survives `<=`) and holds the final frame: duration =
    /// max(settle_end, last retained) + 0.2.
    #[test]
    fn finish_retains_only_pre_settle_events_and_holds_the_last_frame() {
        let score = pty_score();
        let (pty, writer, tx) = fake_pty();
        pty.reader_done.store(true, Ordering::SeqCst);
        let t0 = Instant::now();
        for (ms, data) in [
            (500u64, "early"),
            (1000, "boundary"),
            (1500, "late"),
            (2000, "post"),
        ] {
            tx.send((t0 + Duration::from_millis(ms), data.as_bytes().to_vec()))
                .unwrap();
        }
        let caps = super::super::Captures {
            events: vec![],
            captions: vec![],
            focuses: vec![],
        };
        let rec = finish(&score, pty, 1.0, t0, caps);
        assert_eq!(
            rec.events,
            vec![(0.5, "early".to_string()), (1.0, "boundary".to_string()),],
            "events past settle_end must be dropped, the boundary kept"
        );
        assert!(
            (rec.duration - 1.2).abs() < 1e-9,
            "duration = max(settle_end, last retained) + 0.2, got {}",
            rec.duration
        );
        assert_eq!(
            writer.bytes(),
            b"\nexit\n",
            "no teardown script → exactly the exit line"
        );
    }

    /// With a reader that never flags done, the join window still drains
    /// output queued *while* it runs, and still returns: bounded to
    /// READER_JOIN_MS worth of polls, not the reader's lifetime.
    #[test]
    fn finish_drains_late_output_within_the_bounded_join_window() {
        let score = pty_score();
        let (pty, _writer, tx) = fake_pty();
        // reader_done stays false: no reader thread will ever finish.
        let t0 = Instant::now();
        let feeder = thread::spawn(move || {
            thread::sleep(Duration::from_millis(600));
            let _ = tx.send((Instant::now(), b"late-output".to_vec()));
        });
        let caps = super::super::Captures {
            events: vec![],
            captions: vec![],
            focuses: vec![],
        };
        let start = Instant::now();
        let rec = finish(&score, pty, 60.0, t0, caps);
        let elapsed = start.elapsed();
        feeder.join().expect("feeder thread must finish");
        assert!(
            rec.events.iter().any(|(_, d)| d.contains("late-output")),
            "output queued mid-drain must be collected by the join window, got {:?}",
            rec.events
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "the join window must stay bounded to READER_JOIN_MS, took {elapsed:?}"
        );
    }

    /// The join window is READER_JOIN_MS of JOIN_POLL_MS polls: `/`→`*`
    /// would run 20_000 rounds (~400 s) instead of 50.
    #[test]
    fn join_rounds_divides_the_window_by_the_cadence() {
        assert_eq!(join_rounds(), 50);
    }

    /// The wall-clock backstop: the join loop ends once the window's wall
    /// clock is spent, in both directions of the deadline.
    #[test]
    fn join_window_backstop_ends_the_loop_once_the_deadline_is_spent() {
        assert!(
            !join_window_expired(Instant::now() + Duration::from_millis(READER_JOIN_MS)),
            "a deadline in the future must leave the window open"
        );
        assert!(
            join_window_expired(Instant::now() - Duration::from_millis(1)),
            "a spent deadline must close the window"
        );
        assert!(
            join_window_expired(Instant::now()),
            "the deadline instant itself is spent"
        );
    }
}
