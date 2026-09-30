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
fn wait_for_stop(
    state: &CaptureState,
    child: &mut Box<dyn Child + Send + Sync>,
    control_abs: &Path,
    idle_ms: u64,
    t0: Instant,
) {
    let mut control_read = 0u64;
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
        if !state.ready.load(Ordering::SeqCst) && t0.elapsed() > Duration::from_secs(4) {
            state.ready.store(true, Ordering::SeqCst);
        }
        if idle_ms > 0
            && state.last_activity.lock().unwrap().elapsed() > Duration::from_millis(idle_ms)
        {
            break "idle timeout";
        }
        thread::sleep(Duration::from_millis(100));
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
        fps: (fps != DEFAULT_FPS).then_some(fps),
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
    for attempt in 0..100 {
        let temp_dir = temp_base.join(format!("demo-{timestamp}-{pid}-{attempt}"));
        match std::fs::create_dir(&temp_dir) {
            Ok(()) => return Ok(CaptureWorkdir::Temp(temp_dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
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
}
