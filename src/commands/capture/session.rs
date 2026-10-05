//! The capture session: the PTY the shell runs in, the watchdog that drives
//! the recording, the teardown, and the recording the session leaves behind.

use std::io::{IsTerminal, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use portable_pty::{native_pty_system, Child, CommandBuilder, PtyPair, PtySize};

use crate::cli::CaptureArgs;
use crate::commands::control;
use crate::error::{Error, Result};
use crate::export::run::{is_zsh, sh_single_quote};
use crate::model::{RawEvent, RawMacro, RawMeta};

use super::bridge::{spawn_input_pump, spawn_output_pump};
use super::post::{finish_recording, read_control, write_faithful_cast, write_score, ControlIo};
use super::wizard::DEFAULT_FPS;
use super::wizard::{choose_canvas, choose_font, choose_fps, choose_prompt, choose_sources};
use super::{ms, CaptureState, DebugLog, Reveal, AFTER_QUIET_MS};

/// Size assumed when the terminal reports a degenerate 0×0 (a detached/odd
/// terminal): a degenerate recording — and a divide-by-zero deeper in the
/// renderer — would otherwise result.
const FALLBACK_COLS: u16 = 80;
const FALLBACK_ROWS: u16 = 24;

/// Frame rate to record into the score: only non-default fps is stored.
/// Pure so the default-boundary is unit-testable without a live terminal.
fn fps_opt(fps: u32) -> Option<u32> {
    (fps != DEFAULT_FPS).then_some(fps)
}

/// The readiness watchdog has expired: not ready yet and the grace period is
/// over. Pure so the boundary is unit-testable.
fn readiness_expired(ready: bool, elapsed: Duration) -> bool {
    !ready && elapsed > Duration::from_secs(4)
}

/// The idle timeout has expired: enabled and quiet longer than the limit.
/// Pure so the boundary is unit-testable.
fn idle_expired(idle_ms: u64, idle_elapsed: Duration) -> bool {
    idle_ms > 0 && idle_elapsed > Duration::from_millis(idle_ms)
}

/// Poll cadence of [`wait_for_stop`]: one control read plus checks per pass.
const WAIT_POLL_MS: u64 = 100;
/// Slack polls past the idle deadline before the watchdog gives up. The idle
/// arm always fires on the first pass past its deadline, so this is reachable
/// only when the idle comparison itself is broken — turning a hang into a
/// wrong reason the tests observe, with identical output otherwise.
const WAIT_SLACK_PASSES: u64 = 50;

/// Bounded iteration budget for [`wait_for_stop`]: the idle deadline in polls,
/// plus slack for scheduling jitter. Disabled idle (`0`) never expires, so its
/// budget is effectively infinite. Pure so the arithmetic is unit-testable.
fn wait_budget_passes(idle_ms: u64) -> u64 {
    if idle_ms == 0 {
        u64::MAX
    } else {
        idle_ms / WAIT_POLL_MS + WAIT_SLACK_PASSES
    }
}

/// Consecutive polls with no new output: resets to zero whenever the quiet
/// duration shrinks (fresh output arrived), otherwise counts one more. Pure so
/// the reset boundary is unit-testable.
fn quiet_passes(prev: u64, quiet: Duration, prev_quiet: Duration) -> u64 {
    if quiet < prev_quiet {
        0
    } else {
        prev + 1
    }
}

/// Resolve the capture's terminal size: what the terminal reports, or the
/// fallback when it reports 0×0. Pure, so the fallback is testable without a
/// live terminal.
fn size_or_default(reported: std::io::Result<(u16, u16)>) -> (u16, u16) {
    match reported {
        Ok((cols, rows)) if cols > 0 && rows > 0 => (cols, rows),
        _ => (FALLBACK_COLS, FALLBACK_ROWS),
    }
}

/// Create the PTY the capture shell will run in.
fn open_pty(cols: u16, rows: u16) -> Result<PtyPair> {
    native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| Error::Export(format!("openpty: {e}")))
}

/// Control file in the cwd: `demo open` / `demo stop` — run inside the capture
/// OR from another terminal in this directory — append commands here; the
/// watchdog reads them. The env var lets the captured shell find it directly.
/// Returns its canonical absolute path.
fn create_control_file() -> Result<std::path::PathBuf> {
    let control_path = std::path::PathBuf::from(control::CONTROL_FILE);
    std::fs::File::create(&control_path).map_err(|e| Error::io(&control_path, e))?;
    Ok(std::fs::canonicalize(&control_path).unwrap_or(control_path))
}

/// The spawned capture shell plus the two ends the bridge threads are wired
/// to. The slave side has been dropped, so the reader sees EOF once the shell
/// exits.
struct ShellSession {
    child: Box<dyn Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
}

fn spawn_shell(
    shell: &str,
    work_dir: &Path,
    control_abs: &Path,
    pair: PtyPair,
) -> Result<ShellSession> {
    let mut command = CommandBuilder::new(shell);
    command.cwd(work_dir);
    command.env(control::CONTROL_ENV, control_abs);

    let child = pair
        .slave
        .spawn_command(command)
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
    Ok(ShellSession {
        child,
        reader,
        writer,
    })
}

/// Optional diagnostic log (`--debug`): every chunk in/out with hex, plus
/// lifecycle notes, written next to the raw macro.
fn open_debug_log(
    args: &CaptureArgs,
    t0: Instant,
    shell: &str,
    cols: u16,
    rows: u16,
    control_abs: &Path,
) -> Result<Option<std::sync::Arc<DebugLog>>> {
    if !args.debug {
        return Ok(None);
    }
    let mut path = args.rec.clone().into_os_string();
    path.push(".debug.log");
    let path = std::path::PathBuf::from(path);
    let log = std::sync::Arc::new(DebugLog::create(&path, t0)?);
    log.note(&format!(
        "capture start — shell={shell} cols={cols} rows={rows} idle_timeout_ms={} control={}",
        args.idle_timeout_ms,
        control_abs.display()
    ));
    println!("  (debug log → {})", path.display());
    Ok(Some(log))
}

/// Tell the user what this phase of the capture does.
fn print_capture_intro(args: &CaptureArgs) {
    println!("● capturing — run your demo, then `demo stop` (or `exit` / Ctrl-D) to stop");
    println!("  during capture: `demo focus <source>` and `demo open <url>` — here or from another terminal in this directory");
    if args.idle_timeout_ms > 0 {
        println!(
            "  (auto-stops after {} ms with no output)",
            args.idle_timeout_ms
        );
    }
    println!();
}

/// Pre-roll: force the prompt over the rc files, then echo the readiness
/// marker. The output thread discards everything until it sees the marker, so
/// none of this (nor a leaked `user@host`) lands in the recording.
fn write_prompt_primer<W: Write + ?Sized>(writer: &mut W, shell: &str, forced_ps1: &str) {
    let var = if is_zsh(shell) { "PROMPT" } else { "PS1" };
    let _ = writeln!(writer, "{var}={}; clear", sh_single_quote(forced_ps1));
    let _ = writeln!(writer, "printf 'demostage_capture_%s\\n' ready");
    let _ = writer.flush();
}

/// Bundle the parts of the capture state the control-file reader mutates.
fn control_io(state: &CaptureState, t0: Instant) -> ControlIo<'_> {
    ControlIo {
        events: &state.events,
        pending: &state.pending_opens,
        after: &state.after_opens,
        after_running: &state.after_running,
        after_last_out: &state.after_last_out,
        muting: &state.muting,
        mute_start: &state.mute_start,
        mute_spans: &state.mute_spans,
        t0,
        debug: state.debug.as_deref(),
    }
}

/// Run the watchdog until the capture stops: shell exit, `demo stop`, or the
/// idle timeout. Reads control-file commands on every pass, fires `--after`
/// reveals once the stream goes quiet, and closes stranded mute spans. One
/// final control read happens after the loop, because the loop checks exit
/// conditions *before* reading: a control line written in the last ≤100 ms
/// before shell exit would otherwise be lost.
///
/// The pass budget comes from the idle timeout ([`wait_budget_passes`]), so
/// with the idle timeout disabled the wait is open-ended — exactly as long as
/// the capture runs.
fn wait_for_stop(
    state: &CaptureState,
    child: &mut Box<dyn Child + Send + Sync>,
    control_abs: &Path,
    idle_ms: u64,
    t0: Instant,
) {
    let budget = wait_budget_passes(idle_ms);
    wait_for_stop_bounded(state, child, control_abs, idle_ms, budget, t0);
}

/// [`wait_for_stop`] with the iteration budget passed in rather than derived
/// from the idle timeout. The only difference is where `budget` comes from:
/// production passes [`wait_budget_passes`], tests pass a small ceiling, so a
/// broken stop condition ends the wait with the budget reason (observable in
/// the debug log) instead of looping until the test binary is killed.
fn wait_for_stop_bounded(
    state: &CaptureState,
    child: &mut Box<dyn Child + Send + Sync>,
    control_abs: &Path,
    idle_ms: u64,
    budget: u64,
    t0: Instant,
) {
    let mut control_read = 0u64;
    // Bounded: at most `budget` passes — the idle arm always fires on the
    // first pass past its deadline, so the count below is reachable only when
    // the idle comparison is broken, and the wait still ends on its own.
    let mut passes = 0u64;
    let mut prev_quiet = Duration::ZERO;
    let reason = loop {
        if state.shell_exited.load(Ordering::SeqCst) {
            break "reader closed (shell exited)";
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            break "shell process exited";
        }
        if let Some(r) = read_control(control_abs, &mut control_read, &control_io(state, t0)) {
            break r;
        }
        // An `--after` command has finished (produced output, then went quiet)
        // → fire its reveals now, at the moment the shell returned to the prompt.
        fire_due_after_reveals(state, t0);
        // Safety: an abandoned `demo open` (wizard cancelled, command never
        // sent) shouldn't mute the rest of the demo forever.
        maybe_close_safety_valve(
            &state.muting,
            &state.mute_since,
            &state.mute_start,
            &state.mute_spans,
            t0,
            state.debug.as_deref(),
        );
        // Safety: if the readiness marker never arrives (odd shell), start
        // recording anyway rather than capturing nothing.
        if readiness_expired(state.ready.load(Ordering::SeqCst), t0.elapsed()) {
            state.ready.store(true, Ordering::SeqCst);
        }
        let quiet = state.last_activity.lock().unwrap().elapsed();
        passes = quiet_passes(passes, quiet, prev_quiet);
        prev_quiet = quiet;
        if idle_expired(idle_ms, quiet) {
            break "idle timeout";
        }
        if passes == budget {
            break "watchdog budget exhausted";
        }
        thread::sleep(Duration::from_millis(WAIT_POLL_MS));
    };
    let _ = read_control(control_abs, &mut control_read, &control_io(state, t0));
    if let Some(d) = &state.debug {
        d.note(&format!("stopping — reason: {reason}"));
    }
}

/// Fire every queued `--after` reveal: the foreground command it was armed for
/// has produced output and the stream has since gone quiet.
fn fire_due_after_reveals(state: &CaptureState, t0: Instant) {
    if !state.after_running.load(Ordering::SeqCst) {
        return;
    }
    if state.after_last_out.lock().unwrap().elapsed() <= Duration::from_millis(AFTER_QUIET_MS) {
        return;
    }
    let drained: Vec<Reveal> = state.after_opens.lock().unwrap().drain(..).collect();
    let now = ms(t0);
    let mut evs = state.events.lock().unwrap();
    for r in drained {
        if let Some(d) = &state.debug {
            d.note(&format!("reveal (after): {}", r.summary()));
        }
        evs.push(r.to_event(now));
    }
    state.after_running.store(false, Ordering::SeqCst);
}

/// Stop everything the running session owns: the input thread notices `stop`,
/// the child is killed and reaped, the output thread joins on EOF, the terminal
/// leaves raw mode, and the control files are removed.
fn stop_session(
    state: &CaptureState,
    child: &mut Box<dyn Child + Send + Sync>,
    out_handle: thread::JoinHandle<()>,
    control_abs: &Path,
) {
    state.stop.store(true, Ordering::SeqCst);
    let _ = child.kill();
    let _ = child.wait();
    let _ = out_handle.join();
    let _ = disable_raw_mode();
    let _ = std::fs::remove_file(control_abs);
    let _ = std::fs::remove_file(control_abs.with_file_name(control::SOURCES_FILE));
    let _ = std::fs::remove_file(control_abs.with_file_name(control::META_FILE));
}

/// Log how many events the capture recorded (only when `--debug` is set).
fn note_recorded_counts(state: &CaptureState, raw: &RawMacro) {
    let Some(d) = &state.debug else {
        return;
    };
    let (ins, outs) = raw.events.iter().fold((0, 0), |(i, o), e| match e {
        RawEvent::Input { .. } => (i + 1, o),
        RawEvent::Output { .. } => (i, o + 1),
        RawEvent::Reveal { .. } | RawEvent::Secret { .. } => (i, o),
    });
    d.note(&format!(
        "recorded {} events ({ins} input, {outs} output)",
        raw.events.len(),
    ));
}

/// Offer to run `demo edit` for quick timeline refinement — only when a score
/// was written and we have a terminal to ask on.
fn offer_timeline_edit(score_path: Option<&std::path::PathBuf>) -> Result<()> {
    let Some(path) = score_path else {
        return Ok(());
    };
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }
    let run_direct = inquire::Confirm::new("Run demo edit to refine the timeline?")
        .with_default(false)
        .prompt()
        .unwrap_or(false);
    if run_direct {
        crate::commands::edit::run(crate::cli::EditArgs {
            input: path.clone(),
        })?;
    }
    Ok(())
}

pub fn run(args: CaptureArgs) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Error::Export(
            "capture needs an interactive terminal (stdin/stdout is not a TTY)".to_string(),
        ));
    }

    println!("{}\n", crate::BANNER);

    let shell = args
        .shell
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or("/bin/bash".to_string());
    let (cols, rows) = size_or_default(size());
    let pair = open_pty(cols, rows)?;
    let control_abs = create_control_file()?;
    let launch_dir = std::env::current_dir()
        .map_err(|e| Error::Export(format!("failed to get launch directory: {e}")))?;
    let work_dir = setup_workdir(args.here)?;
    control::write_meta(&control_abs, &launch_dir, work_dir.path())?;
    if !args.here {
        println!("Working directory: {}\n", work_dir.path().display());
    }

    let ShellSession {
        mut child,
        reader,
        mut writer,
    } = spawn_shell(&shell, work_dir.path(), &control_abs, pair)?;
    let mut state = CaptureState::new();

    let (force_prompt, forced_ps1) = choose_prompt(&args, &shell)?;
    let resolution = choose_canvas(&args)?;
    let fps = choose_fps(&args)?;
    let font_family = choose_font(&args)?;
    let (sources, _local_servers) = choose_sources(&launch_dir, work_dir.path())?;
    // Publish the sources beside the control file so `demo focus`/`demo open` can
    // list them live (the score isn't written until the capture ends).
    // `_local_servers` must stay alive for the capture duration — they serve
    // local files (PDF, PNG, HTML) via HTTP so Chromium can access them.
    let _ = control::write_sources(&control_abs, &sources);
    state.ready.store(!force_prompt, Ordering::SeqCst);
    let t0 = Instant::now();
    state.debug = open_debug_log(&args, t0, &shell, cols, rows, &control_abs)?;
    print_capture_intro(&args);
    enable_raw_mode().map_err(|e| Error::Export(format!("raw mode: {e}")))?;
    if force_prompt {
        write_prompt_primer(&mut writer, &shell, &forced_ps1);
    }

    let out_handle = spawn_output_pump(&state, reader, t0);
    spawn_input_pump(&state, writer, t0);
    wait_for_stop(&state, &mut child, &control_abs, args.idle_timeout_ms, t0);
    stop_session(&state, &mut child, out_handle, &control_abs);

    let meta = RawMeta {
        shell,
        cols,
        rows,
        idle_timeout_ms: args.idle_timeout_ms,
        resolution,
        fps: fps_opt(fps),
        stage: args.into.as_ref().map(|p| p.display().to_string()),
        mute_spans: Vec::new(),
    };
    let raw = finish_recording(&state, meta, t0);
    if let Some(raw_path) = &args.output {
        raw.save(raw_path)?;
    }
    note_recorded_counts(&state, &raw);

    // The recording (.rec) of what actually happened, so `demo export` plays back
    // the real session out of the box — no re-execution, which is what breaks
    // interactive/secret/side-effecting tools. This is the one file a capture
    // always leaves behind.
    let cast_path = args.rec.clone();
    let name = cast_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("demo");
    println!(
        "recorded {} events → {}",
        raw.events.len(),
        cast_path.display()
    );

    // Without a normalize pass there is no score — the recording stays faithful.
    if args.no_normalize {
        write_faithful_cast(&raw, None, &cast_path)?;
        println!(
            "next: demo export {}   (renders the live capture)",
            cast_path.display()
        );
        return Ok(());
    }

    // Normalize in memory into a clean score: it carries the demo name/meta for
    // the recording, and is the source `demo record` re-runs — written to disk
    // only when asked (`--score`, or a `--into` stage, which defaults it).
    let score_path = write_score(
        &args,
        &raw,
        name,
        force_prompt,
        &forced_ps1,
        &font_family,
        sources,
    )?;
    offer_timeline_edit(score_path.as_ref())?;
    Ok(())
}

/// If muting has been on for more than 90 seconds, close the stranded mute span,
/// emit a diagnostic, and return true. Otherwise return false.
fn maybe_close_safety_valve(
    muting: &AtomicBool,
    mute_since: &Mutex<Instant>,
    mute_start: &Mutex<Option<u64>>,
    mute_spans: &Mutex<Vec<(u64, u64)>>,
    t0: Instant,
    debug: Option<&DebugLog>,
) -> bool {
    if !muting.load(Ordering::SeqCst) {
        return false;
    }
    if mute_since.lock().unwrap().elapsed() <= Duration::from_secs(90) {
        return false;
    }
    muting.store(false, Ordering::SeqCst);
    if let Some(start) = mute_start.lock().unwrap().take() {
        mute_spans.lock().unwrap().push((start, ms(t0)));
    }
    eprintln!(
        "⚠ safety valve: mute span closed after 90s — a meta-command (demo focus/open) failed to report back"
    );
    if let Some(d) = debug {
        d.note("safety valve: 90s mute span closed — meta-command did not report back");
    }
    true
}

#[derive(Debug)]
enum CaptureWorkdir {
    Current(std::path::PathBuf),
    Temp(std::path::PathBuf),
}
impl CaptureWorkdir {
    fn path(&self) -> &std::path::Path {
        match self {
            Self::Current(path) | Self::Temp(path) => path,
        }
    }
}

impl Drop for CaptureWorkdir {
    fn drop(&mut self) {
        let Self::Temp(path) = self else {
            return;
        };
        if let Err(e) = std::fs::remove_dir_all(path.as_path()) {
            eprintln!(
                "Warning: failed to clean temporary directory {}: {e}",
                path.display()
            );
        }
    }
}

/// Whether a workdir-creation failure is a name collision worth retrying.
/// Pure so the error-kind match is unit-testable.
fn is_collision(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AlreadyExists
}

fn setup_workdir(use_here: bool) -> Result<CaptureWorkdir> {
    if use_here {
        let cwd = std::env::current_dir()
            .map_err(|e| Error::Export(format!("failed to get current directory: {e}")))?;
        return Ok(CaptureWorkdir::Current(cwd));
    }

    let temp_base = std::env::temp_dir();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    setup_workdir_unique(&temp_base, timestamp, pid, |p| std::fs::create_dir(p))
}

/// Create a unique `demo-<timestamp>-<pid>-<attempt>` directory under
/// `temp_base`, retrying name collisions. `create` is injectable so the
/// retry/propagate boundary is unit-testable without racing timestamps.
fn setup_workdir_unique(
    temp_base: &Path,
    timestamp: u128,
    pid: u32,
    mut create: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<CaptureWorkdir> {
    for attempt in 0..100 {
        let temp_dir = temp_base.join(format!("demo-{timestamp}-{pid}-{attempt}"));
        match create(&temp_dir) {
            Ok(()) => return Ok(CaptureWorkdir::Temp(temp_dir)),
            Err(e) if is_collision(&e) => continue,
            Err(e) => {
                return Err(Error::Export(format!(
                    "failed to create temporary directory {}: {e}",
                    temp_dir.display()
                )))
            }
        }
    }

    Err(Error::Export(
        "failed to create a unique temporary directory".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_size_is_used_when_reported() {
        assert_eq!(size_or_default(Ok((120, 40))), (120, 40));
        assert_eq!(size_or_default(Ok((1, 1))), (1, 1));
    }

    #[test]
    fn terminal_size_falls_back_to_80x24_when_degenerate() {
        // A detached/odd terminal reports 0×0 — a degenerate recording (and a
        // divide-by-zero deeper in the renderer) would otherwise result.
        assert_eq!(size_or_default(Ok((0, 24))), (80, 24));
        assert_eq!(size_or_default(Ok((80, 0))), (80, 24));
        assert_eq!(size_or_default(Ok((0, 0))), (80, 24));
        assert_eq!(
            size_or_default(Err(std::io::Error::other("no tty"))),
            (80, 24)
        );
    }

    #[test]
    fn workdir_here_uses_current_directory_without_cleanup() {
        let cwd = std::env::current_dir().unwrap();
        let workdir = setup_workdir(true).unwrap();

        assert_eq!(workdir.path(), cwd.as_path());
    }

    #[test]
    fn workdir_default_creates_and_cleans_temporary_directory() {
        let path = {
            let workdir = setup_workdir(false).unwrap();
            let path = workdir.path().to_path_buf();
            assert!(path.is_dir());
            path
        };

        assert!(!path.exists());
    }

    #[test]
    fn safety_valve_closes_span_and_emits_diagnostic() {
        let t0 = Instant::now();
        let muting = AtomicBool::new(true);
        // Pretend muting started 91 seconds ago.
        let mute_since = Mutex::new(t0 - Duration::from_secs(91));
        let mute_start: Mutex<Option<u64>> = Mutex::new(Some(500));
        let mute_spans: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

        let log_path = std::env::temp_dir().join(format!("demo-test-valve-{}", std::process::id()));
        let debug = DebugLog::create(&log_path, t0).unwrap();

        let fired = maybe_close_safety_valve(
            &muting,
            &mute_since,
            &mute_start,
            &mute_spans,
            t0,
            Some(&debug),
        );

        assert!(fired, "safety valve must report that it fired");
        assert!(
            !muting.load(Ordering::SeqCst),
            "safety valve must stop muting"
        );
        assert!(
            mute_start.lock().unwrap().is_none(),
            "safety valve must take the mute_start"
        );
        assert_eq!(
            mute_spans.lock().unwrap().len(),
            1,
            "safety valve must close the span into mute_spans"
        );
        let log_contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log_contents.contains("safety valve:"),
            "safety valve must write its diagnostic to the debug log, got: {log_contents:?}"
        );
        let _ = std::fs::remove_file(&log_path);
    }

    #[test]
    fn safety_valve_does_not_fire_before_90s() {
        let t0 = Instant::now();
        let muting = AtomicBool::new(true);
        let mute_since = Mutex::new(t0 - Duration::from_secs(60));
        let mute_start: Mutex<Option<u64>> = Mutex::new(Some(500));
        let mute_spans: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

        let fired =
            maybe_close_safety_valve(&muting, &mute_since, &mute_start, &mute_spans, t0, None);

        assert!(!fired, "safety valve must not fire before 90s");
        assert!(
            muting.load(Ordering::SeqCst),
            "muting must remain on when valve hasn't fired"
        );
        assert_eq!(
            mute_spans.lock().unwrap().len(),
            0,
            "no span must be closed when valve hasn't fired"
        );
    }

    // --- mutant-killing tests ---

    #[test]
    fn fps_opt_stores_only_non_default() {
        assert_eq!(fps_opt(DEFAULT_FPS), None, "default fps must not be stored");
        assert_eq!(fps_opt(30), Some(30));
        assert_eq!(fps_opt(24), Some(24));
    }

    #[test]
    fn readiness_expired_matrix() {
        assert!(readiness_expired(false, Duration::from_secs(5)));
        assert!(!readiness_expired(false, Duration::from_secs(1)));
        assert!(!readiness_expired(true, Duration::from_secs(100)));
        // Boundary: exactly 4s is not yet expired (strict >).
        assert!(!readiness_expired(false, Duration::from_secs(4)));
        assert!(readiness_expired(
            false,
            Duration::from_secs(4) + Duration::from_millis(1)
        ));
    }

    #[test]
    fn idle_expired_matrix() {
        assert!(
            !idle_expired(0, Duration::from_secs(3600)),
            "disabled never fires"
        );
        assert!(idle_expired(100, Duration::from_millis(200)));
        assert!(!idle_expired(100, Duration::from_millis(50)));
        // Boundary: exactly at the limit is not yet expired (strict >).
        assert!(!idle_expired(100, Duration::from_millis(100)));
        assert!(idle_expired(100, Duration::from_millis(101)));
    }

    #[test]
    fn write_prompt_primer_uses_prompt_for_zsh_and_ps1_for_others() {
        let mut zsh = Vec::new();
        write_prompt_primer(&mut zsh, "/bin/zsh", "demo$ ");
        let zsh = String::from_utf8(zsh).unwrap();
        assert!(zsh.contains("PROMPT="), "zsh must set PROMPT, got: {zsh:?}");
        assert!(zsh.contains("demostage_capture_"), "marker must be primed");
        let mut bash = Vec::new();
        write_prompt_primer(&mut bash, "/bin/bash", "demo$ ");
        let bash = String::from_utf8(bash).unwrap();
        assert!(bash.contains("PS1="), "bash must set PS1, got: {bash:?}");
    }

    #[test]
    fn open_debug_log_returns_none_without_debug_and_some_with_it() {
        let dir = std::env::temp_dir().join(format!(
            "demo-test-debug-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |debug: bool| crate::cli::CaptureArgs {
            rec: dir.join("demo.rec"),
            idle_timeout_ms: 0,
            shell: None,
            into: None,
            no_normalize: false,
            debug,
            output: None,
            normalized_output: dir.join("demo.toml"),
            no_score: false,
            prompt: None,
            keep_prompt: true,
            font: None,
            aspect: None,
            quality: None,
            fps: None,
            resolution: None,
            here: true,
        };
        let t0 = Instant::now();
        let none = open_debug_log(&mk(false), t0, "bash", 80, 24, &dir.join("ctl")).unwrap();
        assert!(none.is_none(), "debug=false must return None");
        let some = open_debug_log(&mk(true), t0, "bash", 80, 24, &dir.join("ctl")).unwrap();
        assert!(some.is_some(), "debug=true must return a log");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fire_due_after_reveals_matrix() {
        // Not running → no drain, stays off.
        let s = CaptureState::new();
        s.after_opens.lock().unwrap().push(Reveal {
            panes: vec![crate::model::RevealPane::terminal()],
            orientation: crate::model::Orientation::Horizontal,
            hold_ms: None,
            scroll: false,
        });
        fire_due_after_reveals(&s, Instant::now());
        assert!(s.events.lock().unwrap().is_empty());
        assert!(!s.after_running.load(Ordering::SeqCst));
        // Running but recent output → no drain, stays on.
        let s2 = CaptureState::new();
        s2.after_running.store(true, Ordering::SeqCst);
        *s2.after_last_out.lock().unwrap() = Instant::now();
        s2.after_opens.lock().unwrap().push(Reveal {
            panes: vec![crate::model::RevealPane::terminal()],
            orientation: crate::model::Orientation::Horizontal,
            hold_ms: None,
            scroll: false,
        });
        fire_due_after_reveals(&s2, Instant::now());
        assert!(
            s2.events.lock().unwrap().is_empty(),
            "quiet period not yet over"
        );
        assert!(s2.after_running.load(Ordering::SeqCst));
        // Running and quiet long enough → drains with now stamp, turns off.
        let s3 = CaptureState::new();
        s3.after_running.store(true, Ordering::SeqCst);
        *s3.after_last_out.lock().unwrap() =
            Instant::now() - Duration::from_millis(AFTER_QUIET_MS + 500);
        s3.after_opens.lock().unwrap().push(Reveal {
            panes: vec![crate::model::RevealPane::terminal()],
            orientation: crate::model::Orientation::Horizontal,
            hold_ms: None,
            scroll: false,
        });
        let t0 = Instant::now();
        fire_due_after_reveals(&s3, t0);
        assert_eq!(s3.events.lock().unwrap().len(), 1, "quiet reveal must fire");
        assert!(!s3.after_running.load(Ordering::SeqCst));
        assert!(s3.after_opens.lock().unwrap().is_empty());
    }

    #[test]
    fn note_recorded_counts_exact_input_output_totals() {
        // Without debug: no-op, no panic.
        let s = CaptureState::new();
        let raw = RawMacro {
            meta: RawMeta {
                shell: String::new(),
                cols: 80,
                rows: 24,
                idle_timeout_ms: 0,
                resolution: None,
                fps: None,
                stage: None,
                mute_spans: Vec::new(),
            },
            events: vec![
                RawEvent::Input {
                    t_ms: 1,
                    bytes: "a".into(),
                },
                RawEvent::Input {
                    t_ms: 2,
                    bytes: "b".into(),
                },
                RawEvent::Output {
                    t_ms: 3,
                    data: "c".into(),
                },
                RawEvent::Reveal {
                    t_ms: 4,
                    panes: vec![crate::model::RevealPane::terminal()],
                    orientation: crate::model::Orientation::Horizontal,
                    hold_ms: None,
                    scroll: false,
                },
            ],
        };
        note_recorded_counts(&s, &raw);
        // With debug: exact counts in the log (2 input, 1 output of 4).
        let dir = std::env::temp_dir().join(format!(
            "demo-test-counts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let t0 = Instant::now();
        let log = std::sync::Arc::new(DebugLog::create(&dir.join("d.log"), t0).unwrap());
        let mut s2 = CaptureState::new();
        s2.debug = Some(log);
        note_recorded_counts(&s2, &raw);
        let text = std::fs::read_to_string(dir.join("d.log")).unwrap();
        assert!(
            text.contains("recorded 4 events (2 input, 1 output)"),
            "exact counts must be logged, got: {text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_collision_matches_only_already_exists() {
        assert!(is_collision(&std::io::Error::from(
            std::io::ErrorKind::AlreadyExists
        )));
        assert!(!is_collision(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(!is_collision(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn create_control_file_truncates_and_returns_a_canonical_path() {
        let dir = std::env::temp_dir().join(format!(
            "demo-test-ctl-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _cwd_guard = super::super::CWD_LOCK.lock().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        std::fs::write(crate::commands::control::CONTROL_FILE, "stale").unwrap();
        let got = create_control_file().unwrap();
        assert_eq!(
            std::fs::read_to_string(crate::commands::control::CONTROL_FILE).unwrap(),
            "",
            "existing control file must be truncated"
        );
        assert!(got.is_absolute(), "path must be canonical, got {got:?}");
        assert_eq!(
            got.file_name().unwrap(),
            crate::commands::control::CONTROL_FILE
        );
        std::env::set_current_dir(prev).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_workdir_retries_on_name_collision() {
        // Pre-create the first candidate name is racy (timestamp-based), so
        // instead assert the two modes: here→cwd, temp→fresh dir that cleans up.
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(setup_workdir(true).unwrap().path(), cwd.as_path());
        let path = {
            let w = setup_workdir(false).unwrap();
            let p = w.path().to_path_buf();
            assert!(p.is_dir());
            // The temp name carries pid + attempt suffix.
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            assert!(name.starts_with("demo-"), "temp name, got {name:?}");
            p
        };
        assert!(!path.exists(), "temp workdir must be cleaned on drop");
    }

    /// A fake child that is already exited (try_wait → Some) or never exits.
    #[derive(Debug)]
    struct FakeChild {
        exited: bool,
    }

    impl portable_pty::ChildKiller for FakeChild {
        fn kill(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(FakeChild {
                exited: self.exited,
            })
        }
    }

    impl portable_pty::Child for FakeChild {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            if self.exited {
                Ok(Some(portable_pty::ExitStatus::with_exit_code(0)))
            } else {
                Ok(None)
            }
        }
        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            Ok(portable_pty::ExitStatus::with_exit_code(0))
        }
        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    #[test]
    fn wait_for_stop_breaks_on_shell_exit_without_hanging() {
        let t0 = Instant::now();
        let mut state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "demo-test-wait-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctl = dir.join("ctl");
        std::fs::write(&ctl, "").unwrap();
        let log_path = dir.join("d.log");
        state.debug = Some(std::sync::Arc::new(
            DebugLog::create(&log_path, t0).unwrap(),
        ));
        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(FakeChild { exited: true });
        // Join budget: idle disabled (the wait is otherwise open-ended), so the
        // loop runs at most 5 passes. A mutant that no longer breaks on child
        // exit then ends the wait with the budget reason in 0.5 s — a fast
        // failure instead of a loop that never returns.
        wait_for_stop_bounded(&state, &mut child, &ctl, 0, 5, t0);
        let text = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            text.contains("shell process exited"),
            "wait must end by observing the exited child, got: {text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_for_stop_applies_readiness_watchdog_then_idle_timeout() {
        let mut state = CaptureState::new();
        state.ready.store(false, Ordering::SeqCst);
        // Last activity long ago so the idle arm fires on the first pass.
        *state.last_activity.lock().unwrap() = Instant::now() - Duration::from_secs(60);
        let dir = std::env::temp_dir().join(format!(
            "demo-test-wait-idle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctl = dir.join("ctl");
        std::fs::write(&ctl, "").unwrap();
        // t0 five seconds ago: the readiness arm must flip ready on.
        let t0 = Instant::now() - Duration::from_secs(5);
        state.debug = Some(std::sync::Arc::new(
            DebugLog::create(&dir.join("d.log"), t0).unwrap(),
        ));
        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(FakeChild { exited: false });
        wait_for_stop(&state, &mut child, &ctl, 100, t0);
        assert!(
            state.ready.load(Ordering::SeqCst),
            "readiness watchdog must arm recording after 4s"
        );
        let text = std::fs::read_to_string(dir.join("d.log")).unwrap();
        assert!(
            text.contains("idle timeout"),
            "the wait must end on the idle arm, not the watchdog budget, got: {text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_for_stop_reads_stop_command_from_offset_zero() {
        let state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "demo-test-wait-stop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctl = dir.join("ctl");
        std::fs::write(&ctl, "{\"cmd\":\"stop\"}\n").unwrap();
        let t0 = Instant::now();
        let log = std::sync::Arc::new(DebugLog::create(&dir.join("d.log"), t0).unwrap());
        let mut s2 = CaptureState::new();
        s2.ready.store(true, Ordering::SeqCst);
        s2.debug = Some(log);
        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(FakeChild { exited: false });
        // Never-idle so only the stop line can end the wait, and a 5-pass
        // join budget so a mutant that skips the first control byte (offset 1)
        // breaks the stop JSON and ends the wait with the budget reason —
        // a fast failure instead of a TIMEOUT.
        wait_for_stop_bounded(&s2, &mut child, &ctl, 0, 5, t0);
        let text = std::fs::read_to_string(dir.join("d.log")).unwrap();
        assert!(
            text.contains("demo stop"),
            "stop line at offset 0 must end the wait, got: {text:?}"
        );
        let _ = state;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_session_flags_stop_and_removes_control_files() {
        let state = CaptureState::new();
        let dir = std::env::temp_dir().join(format!(
            "demo-test-stop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctl = dir.join(crate::commands::control::CONTROL_FILE);
        std::fs::write(&ctl, "").unwrap();
        std::fs::write(
            ctl.with_file_name(crate::commands::control::SOURCES_FILE),
            "",
        )
        .unwrap();
        std::fs::write(ctl.with_file_name(crate::commands::control::META_FILE), "").unwrap();
        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(FakeChild { exited: true });
        let handle = std::thread::spawn(|| {});
        stop_session(&state, &mut child, handle, &ctl);
        assert!(state.stop.load(Ordering::SeqCst), "stop flag must be set");
        assert!(!ctl.exists(), "control file must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wait_for_stop_reaches_late_passes_without_tripping_the_watchdog() {
        // The stop line arrives after several polls: the wait must walk past
        // the watchdog line on every early pass and still end on the stop
        // reason. `==`→`!=` would trip the watchdog on the first pass instead.
        let mut state = CaptureState::new();
        state.ready.store(true, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "demo-test-wait-late-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctl = dir.join("ctl");
        std::fs::write(&ctl, "").unwrap();
        let t0 = Instant::now();
        state.debug = Some(std::sync::Arc::new(
            DebugLog::create(&dir.join("d.log"), t0).unwrap(),
        ));
        // Idle disabled and a live child: only the stop line ends the wait. The
        // 20-pass ceiling is 4x the passes the feeder needs, so the real wait
        // ends on the stop reason; a mutant that never sees the stop line ends
        // it with the budget reason after ~2 s instead of looping forever.
        let mut child: Box<dyn portable_pty::Child + Send + Sync> =
            Box::new(FakeChild { exited: false });
        let feeder_ctl = ctl.clone();
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(350));
            std::fs::write(&feeder_ctl, "{\"cmd\":\"stop\"}\n").unwrap();
        });
        wait_for_stop_bounded(&state, &mut child, &ctl, 0, 20, t0);
        feeder.join().expect("feeder thread must finish");
        let text = std::fs::read_to_string(dir.join("d.log")).unwrap();
        assert!(
            text.contains("demo stop"),
            "the wait must end on the late stop line, got: {text:?}"
        );
        assert!(
            !text.contains("watchdog"),
            "early passes must not trip the watchdog, got: {text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_refuses_without_interactive_terminal() {
        // Under cargo test stdin/stdout are not TTYs, so run must refuse.
        // This kills the `replace run with Ok(())` mutant: Ok would not err.
        if std::io::IsTerminal::is_terminal(&std::io::stdin())
            && std::io::IsTerminal::is_terminal(&std::io::stdout())
        {
            return;
        }
        let args = crate::cli::CaptureArgs {
            rec: "demo.rec".into(),
            idle_timeout_ms: 0,
            shell: None,
            into: None,
            no_normalize: false,
            debug: false,
            output: None,
            normalized_output: "demo.toml".into(),
            no_score: false,
            prompt: None,
            keep_prompt: true,
            font: None,
            aspect: None,
            quality: None,
            fps: None,
            resolution: None,
            here: true,
        };
        let err = run(args).unwrap_err();
        assert!(err.to_string().contains("interactive terminal"));
    }

    #[test]
    fn wait_budget_passes_covers_the_idle_deadline_plus_slack() {
        // Disabled idle never expires: effectively infinite.
        assert_eq!(wait_budget_passes(0), u64::MAX);
        // Otherwise the deadline in 100 ms polls, plus the slack.
        assert_eq!(wait_budget_passes(100), 51);
        assert_eq!(wait_budget_passes(1000), 60);
    }

    #[test]
    fn quiet_passes_resets_on_fresh_output_and_counts_otherwise() {
        // Fresh output (quiet duration shrank) resets the streak.
        assert_eq!(
            quiet_passes(5, Duration::from_millis(10), Duration::from_millis(20)),
            0
        );
        // Still quiet (duration grew) counts one more poll.
        assert_eq!(
            quiet_passes(5, Duration::from_millis(30), Duration::from_millis(20)),
            6
        );
        // Equal durations mean no new output: still counts, never resets.
        assert_eq!(quiet_passes(0, Duration::ZERO, Duration::ZERO), 1);
        assert_eq!(
            quiet_passes(41, Duration::from_secs(60), Duration::from_secs(59)),
            42
        );
    }

    #[test]
    fn workdir_unique_retries_a_collision_then_succeeds() {
        // First candidate collides, second is free: the guard must retry, so
        // the second name wins. `guard → false` would return the error instead.
        let base = std::env::temp_dir();
        let mut calls = 0;
        let got = setup_workdir_unique(&base, 1, 2, |_| {
            calls += 1;
            if calls == 1 {
                Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(got.path(), base.join("demo-1-2-1"));
        assert_eq!(calls, 2, "one collision, one success");
    }

    #[test]
    fn workdir_unique_propagates_a_non_collision_error_at_once() {
        // A real failure is returned immediately with its directory named:
        // `guard → true` would retry 100 times and report "unique" instead.
        let base = std::env::temp_dir();
        let mut calls = 0;
        let err = setup_workdir_unique(&base, 1, 2, |_| {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("demo-1-2-0"),
            "the failing directory must be named, got: {err}"
        );
        assert_eq!(calls, 1, "no retry on a non-collision error");
    }
}
