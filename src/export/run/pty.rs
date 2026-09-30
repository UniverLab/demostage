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
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 || tx.send((Instant::now(), buf[..n].to_vec())).is_err() {
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
        if d > 0 {
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
    let deadline = Instant::now() + Duration::from_millis(EXIT_GRACE_MS);
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        if Instant::now() >= deadline {
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
    let join_deadline = Instant::now() + Duration::from_millis(READER_JOIN_MS);
    while !reader_done.load(Ordering::SeqCst) && Instant::now() < join_deadline {
        collect(&mut events, &rx, t0);
        thread::sleep(Duration::from_millis(20));
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
        assert_eq!(chunks.concat(), b"hello\r\nworld");
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
}
