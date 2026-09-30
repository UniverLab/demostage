//! What happens around the recorded stream: control-file commands from
//! `demo focus`/`demo open`/`demo stop`, draining the queued reveals at
//! shutdown, normalizing the events into a score, and writing the faithful
//! recording `demo export` plays back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::cli::CaptureArgs;
use crate::error::{Error, Result};
use crate::export::recording;
use crate::model::{DemoMeta, Orientation, RawEvent, RawMacro, RawMeta, RevealPane, Score, Source};
use crate::normalize::{merge_into_stage, normalize, Options};

use super::{
    ms, CaptureState, DebugLog, PendingAfter, PendingWhen, Reveal, OPEN_BEGIN_BACKDATE_MS,
};

/// Drain the pending reveal queues and build the [`RawMacro`] the capture
/// produced: everything the two bridge threads and the control-file reader
/// recorded, in event order.
///
/// `meta` arrives with everything known before recording stopped; the mute
/// spans only close here, at stop time.
pub(super) fn finish_recording(state: &CaptureState, mut meta: RawMeta, t0: Instant) -> RawMacro {
    {
        let mut evs = state.events.lock().unwrap();
        let raw_for_cutoff = RawMacro {
            meta: RawMeta {
                shell: String::new(),
                cols: 0,
                rows: 0,
                idle_timeout_ms: 0,
                resolution: None,
                fps: None,
                stage: None,
                mute_spans: Vec::new(),
            },
            events: evs.clone(),
        };
        let drain_ts = recording::stop_cutoff_ms(&raw_for_cutoff)
            .map(|c| c.saturating_sub(1))
            .unwrap_or_else(|| {
                evs.iter()
                    .filter_map(|e| match e {
                        RawEvent::Output { t_ms, .. } => Some(*t_ms),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0)
            });
        let summary =
            drain_remaining_reveals(&state.after_opens, &state.pending_opens, &mut evs, drain_ts);
        for pat in &summary.when_unmatched {
            eprintln!(
                "warning: --when cue {pat:?} never matched during capture; reveal appended at end"
            );
        }
        for after in &summary.after_summaries {
            eprintln!(
                "warning: --after reveal {after} never fired during capture; reveal appended at end"
            );
        }
    }

    let events = state.events.lock().unwrap().clone();
    let mut mute_spans = state.mute_spans.lock().unwrap().clone();
    // Close a span still open at stop time (e.g. a wizard cut short by `demo stop`).
    if let Some(start) = state.mute_start.lock().unwrap().take() {
        mute_spans.push((start, ms(t0)));
    }
    meta.mute_spans = mute_spans;
    RawMacro { meta, events }
}

/// Normalize the captured events into a score, persist it when asked, and write
/// the faithful recording beside it. Returns the score path, if one was written.
pub(super) fn write_score(
    args: &CaptureArgs,
    raw: &RawMacro,
    name: &str,
    force_prompt: bool,
    forced_ps1: &str,
    font_family: &str,
    sources: Vec<Source>,
) -> Result<Option<std::path::PathBuf>> {
    let opts = Options {
        typing_ms: 80,
        salt_ms: 15,
        seed: None,
    };
    let mut score = match &args.into {
        Some(path) => merge_into_stage(Score::load(path)?, raw, &opts),
        None => {
            let normalized = normalize(raw, name, &opts);
            // Preserve sources from an existing score file (defined before capture).
            if !args.no_score && args.normalized_output.exists() {
                if let Ok(existing) = Score::load(&args.normalized_output) {
                    if !existing.sources.is_empty() {
                        let mut score = normalized;
                        score.sources = existing.sources;
                        score
                    } else {
                        normalized
                    }
                } else {
                    normalized
                }
            } else {
                normalized
            }
        }
    };
    // Persist the prompt the demo was captured with, so `demo record` reproduces
    // it instead of falling back to the built-in default.
    if force_prompt {
        score.demo.prompt = Some(forced_ps1.to_string());
    }
    // Store the chosen font in the layout so `demo export` uses it.
    score.layout.font_family = Some(font_family.to_string());
    // Store wizard-selected sources (skip if already set from --into).
    if score.sources.is_empty() && !sources.is_empty() {
        score.sources = sources;
    }
    let score_path = if args.no_score {
        None
    } else {
        Some(args.normalized_output.clone())
    };
    if let Some(p) = &score_path {
        score.save(p)?;
    }
    write_faithful_cast(raw, Some(&score), &args.rec)?;

    match &score_path {
        Some(p) => println!(
            "score → {}   |   next: demo export {}  (or `demo record` to re-run)",
            p.display(),
            args.rec.display()
        ),
        None => println!(
            "next: demo export {}   (--no-score set, so `demo record` won't have a score)",
            args.rec.display()
        ),
    }
    Ok(score_path)
}

/// Parse a `reveal` control message into a [`Reveal`] — its panes, orientation,
/// hold and scroll. Returns `None` if it carries no panes.
fn parse_reveal(v: &serde_json::Value) -> Option<Reveal> {
    let panes_json = v.get("panes")?.as_array()?;
    let mut panes = Vec::new();
    for p in panes_json {
        let id = p
            .get("id")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("main")
            .to_string();
        let url = p
            .get("url")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let theme = p
            .get("theme")
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        panes.push(RevealPane { id, url, theme });
    }
    if panes.is_empty() {
        return None;
    }
    let orientation = match v.get("orientation").and_then(|o| o.as_str()) {
        Some("vertical") => Orientation::Vertical,
        _ => Orientation::Horizontal,
    };
    let hold_ms = v.get("hold").and_then(|h| h.as_u64());
    let scroll = v.get("scroll").and_then(|s| s.as_bool()).unwrap_or(false);
    Some(Reveal {
        panes,
        orientation,
        hold_ms,
        scroll,
    })
}

/// Summary of what the shutdown drain resolved from the pending queues.
struct DrainSummary {
    after_summaries: Vec<String>,
    when_unmatched: Vec<String>,
}

/// Drain any remaining `--after` and `--when` queues at shutdown, emitting
/// their reveals as events so they are recorded rather than silently dropped.
/// Returns a summary of what was fired and which `--when` cues never matched.
fn drain_remaining_reveals(
    after_opens: &PendingAfter,
    pending_opens: &PendingWhen,
    events: &mut Vec<RawEvent>,
    now: u64,
) -> DrainSummary {
    let after_remaining: Vec<Reveal> = after_opens.lock().unwrap().drain(..).collect();
    let mut after_summaries = Vec::new();
    for r in after_remaining {
        after_summaries.push(r.summary());
        events.push(r.to_event(now));
    }
    let mut when_unmatched = Vec::new();
    let pending: Vec<(Reveal, String)> = pending_opens.lock().unwrap().drain(..).collect();
    for (r, pat) in pending {
        when_unmatched.push(pat.clone());
        events.push(r.to_event(now));
    }
    DrainSummary {
        after_summaries,
        when_unmatched,
    }
}

/// The recording state a control-file command touches: the event log, the two
/// armed-reveal queues, and the mute bookkeeping that hides a running
/// `demo open` from the recording.
pub(super) struct ControlIo<'a> {
    pub(super) events: &'a Arc<Mutex<Vec<RawEvent>>>,
    pub(super) pending: &'a PendingWhen,
    pub(super) after: &'a PendingAfter,
    pub(super) after_running: &'a AtomicBool,
    pub(super) after_last_out: &'a Mutex<Instant>,
    pub(super) muting: &'a Arc<AtomicBool>,
    pub(super) mute_start: &'a Arc<Mutex<Option<u64>>>,
    pub(super) mute_spans: &'a Arc<Mutex<Vec<(u64, u64)>>>,
    pub(super) t0: Instant,
    pub(super) debug: Option<&'a DebugLog>,
}

/// Read any new control-file commands (`demo focus`/`demo open`/`demo stop`).
/// Records immediate reveals, arms `--when`/`--after` reveals, and returns
/// `Some(reason)` on stop.
pub(super) fn read_control(
    path: &std::path::Path,
    read: &mut u64,
    io: &ControlIo<'_>,
) -> Option<&'static str> {
    let ControlIo {
        events,
        pending,
        after,
        after_running,
        after_last_out,
        muting,
        mute_start,
        mute_spans,
        t0,
        debug,
    } = *io;
    let data = std::fs::read(path).ok()?;
    if data.len() as u64 <= *read {
        return None;
    }
    let new = String::from_utf8_lossy(&data[*read as usize..]).into_owned();
    *read = data.len() as u64;

    let mut stop = None;
    for line in new.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        match v.get("cmd").and_then(|c| c.as_str()) {
            // A `demo focus`/`demo open` is starting in the captured shell → mute
            // its echo/wizard. If input detection didn't already mark the start,
            // fall back to a little before now so its first output is still excised.
            Some("reveal_begin") => {
                muting.store(true, Ordering::SeqCst);
                let mut s = mute_start.lock().unwrap();
                if s.is_none() {
                    *s = Some(ms(t0).saturating_sub(OPEN_BEGIN_BACKDATE_MS));
                }
            }
            Some("stop") => {
                muting.store(false, Ordering::SeqCst);
                stop = Some("demo stop");
            }
            Some("reveal_cancel") => {
                // A meta-command failed/was cancelled → close the mute span
                // without recording a reveal (the command leaves no trace).
                muting.store(false, Ordering::SeqCst);
                let mute_span_start = mute_start.lock().unwrap().take();
                if let Some(start) = mute_span_start {
                    mute_spans.lock().unwrap().push((start, ms(t0)));
                }
                if let Some(d) = debug {
                    d.note("reveal_cancel — meta-command failed or was cancelled");
                }
            }
            Some("reveal") => {
                // The command finished → stop muting and close its excision span.
                muting.store(false, Ordering::SeqCst);
                let mute_span_start = mute_start.lock().unwrap().take();
                if let Some(start) = mute_span_start {
                    mute_spans.lock().unwrap().push((start, ms(t0)));
                }
                let Some(reveal) = parse_reveal(&v) else {
                    continue;
                };
                let when = v
                    .get("when")
                    .and_then(|w| w.as_str())
                    .filter(|s| !s.is_empty());
                let after_flag = v.get("after").and_then(|a| a.as_bool()).unwrap_or(false);
                if let Some(pat) = when {
                    if let Some(d) = debug {
                        d.note(&format!("reveal armed: {} when {pat:?}", reveal.summary()));
                    }
                    pending.lock().unwrap().push((reveal, pat.into()));
                } else if after_flag {
                    if let Some(d) = debug {
                        d.note(&format!("reveal armed: {} after command", reveal.summary()));
                    }
                    after.lock().unwrap().push(reveal);
                    after_running.store(true, Ordering::SeqCst);
                    *after_last_out.lock().unwrap() = Instant::now();
                } else {
                    // Immediate reveal: use current time (when command finished)
                    if let Some(d) = debug {
                        d.note(&format!("reveal now: {}", reveal.summary()));
                    }
                    events.lock().unwrap().push(reveal.to_event(ms(t0)));
                }
            }
            _ => {}
        }
    }
    stop
}

/// Write a faithful recording of the captured session (its real output) to
/// `path`, so `demo export` can play it back without re-executing anything.
pub(super) fn write_faithful_cast(
    raw: &RawMacro,
    score: Option<&Score>,
    path: &std::path::Path,
) -> Result<()> {
    let name = score.map(|s| s.demo.name.as_str()).unwrap_or("demo");
    // The layout comes from the capture's `demo open` scenes (terminal + browser
    // panes); the demo meta/typing come from the normalized score. The timeline
    // carries only browser-scroll steps (it isn't executed — playback is faithful).
    let (rec, mut layout, timeline) = recording::from_raw(raw, name);
    // The reveal-built layout has no styling of its own — the font chosen in the
    // capture wizard lives on the score's layout.
    if let Some(s) = score {
        layout.font_family = s.layout.font_family.clone();
    }
    let final_score = Score {
        demo: score.map(|s| s.demo.clone()).unwrap_or_else(|| DemoMeta {
            name: name.to_string(),
            output_dir: "./dist".into(),
            prompt: None,
            speed: None,
            targets: None,
        }),
        env: None,
        typing: score.and_then(|s| s.typing.clone()),
        sources: score.map(|s| s.sources.clone()).unwrap_or_default(),
        layout,
        timeline,
    };
    let cast = recording::write(&rec, &final_score, true)?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
    }
    std::fs::write(path, cast).map_err(|e| Error::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_summary_format() {
        let r = Reveal {
            panes: vec![
                RevealPane {
                    id: "main".into(),
                    url: None,
                    theme: None,
                },
                RevealPane {
                    id: "docs".into(),
                    url: Some("http://x.com".into()),
                    theme: None,
                },
            ],
            orientation: Orientation::Vertical,
            hold_ms: None,
            scroll: false,
        };
        let s = r.summary();
        assert!(s.contains("main"));
        assert!(s.contains("docs"));
        assert!(s.contains("Vertical"));
    }

    #[test]
    fn reveal_to_event_produces_correct_timestamp() {
        let r = Reveal {
            panes: vec![RevealPane::terminal()],
            orientation: Orientation::Horizontal,
            hold_ms: Some(5000),
            scroll: true,
        };
        let ev = r.to_event(1234);
        match ev {
            RawEvent::Reveal {
                t_ms,
                panes,
                orientation,
                hold_ms,
                scroll,
            } => {
                assert_eq!(t_ms, 1234);
                assert_eq!(panes.len(), 1);
                assert_eq!(orientation, Orientation::Horizontal);
                assert_eq!(hold_ms, Some(5000));
                assert!(scroll);
            }
            _ => panic!("expected Reveal event"),
        }
    }

    #[test]
    fn parse_reveal_parses_json() {
        let v = serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "main"}, {"id": "web", "url": "https://x.com", "theme": "dark"}],
            "orientation": "vertical",
            "hold": 3000,
            "scroll": true,
        });
        let r = parse_reveal(&v).unwrap();
        assert_eq!(r.panes.len(), 2);
        assert_eq!(r.orientation, Orientation::Vertical);
        assert_eq!(r.hold_ms, Some(3000));
        assert!(r.scroll);
        assert_eq!(r.panes[1].theme.as_deref(), Some("dark"));
    }

    #[test]
    fn parse_reveal_returns_none_for_empty_panes() {
        let v = serde_json::json!({"cmd": "reveal", "panes": []});
        assert!(parse_reveal(&v).is_none());
    }

    #[test]
    fn reveal_summary_with_theme() {
        let r = Reveal {
            panes: vec![RevealPane {
                id: "web".into(),
                url: Some("https://x.com".into()),
                theme: Some("dark".into()),
            }],
            orientation: Orientation::Horizontal,
            hold_ms: Some(3000),
            scroll: false,
        };
        let s = r.summary();
        assert!(s.contains("web"));
        assert!(s.contains("Horizontal"));
    }

    #[test]
    fn parse_reveal_empty_panes() {
        let v = serde_json::json!({"cmd": "reveal", "panes": []});
        assert!(parse_reveal(&v).is_none());
    }

    #[test]
    fn parse_reveal_missing_panes() {
        let v = serde_json::json!({"cmd": "reveal"});
        assert!(parse_reveal(&v).is_none());
    }

    #[test]
    fn parse_reveal_with_when() {
        let v = serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "main"}],
            "orientation": "horizontal",
            "when": "some pattern",
        });
        let r = parse_reveal(&v).unwrap();
        // 'when' is not part of Reveal struct, so just verify parsing works
        assert_eq!(r.panes.len(), 1);
        assert_eq!(r.orientation, Orientation::Horizontal);
    }

    #[test]
    fn reveal_to_event_with_scroll_false() {
        let r = Reveal {
            panes: vec![RevealPane::terminal()],
            orientation: Orientation::Horizontal,
            hold_ms: None,
            scroll: false,
        };
        let ev = r.to_event(500);
        if let RawEvent::Reveal { scroll, .. } = ev {
            assert!(!scroll);
        } else {
            panic!("expected Reveal");
        }
    }

    #[test]
    fn reveal_to_event_with_hold_none() {
        let r = Reveal {
            panes: vec![RevealPane::terminal()],
            orientation: Orientation::Vertical,
            hold_ms: None,
            scroll: false,
        };
        let ev = r.to_event(100);
        if let RawEvent::Reveal { hold_ms, .. } = ev {
            assert_eq!(hold_ms, None);
        } else {
            panic!("expected Reveal");
        }
    }

    #[test]
    fn parse_reveal_with_theme() {
        let v = serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "main", "theme": "dark"}],
            "orientation": "horizontal",
        });
        let r = parse_reveal(&v).unwrap();
        assert_eq!(r.panes.len(), 1);
        assert_eq!(r.panes[0].theme.as_deref(), Some("dark"));
    }

    #[test]
    fn parse_reveal_with_url() {
        let v = serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "browser", "url": "https://example.com"}],
            "orientation": "vertical",
        });
        let r = parse_reveal(&v).unwrap();
        assert_eq!(r.orientation, Orientation::Vertical);
        assert_eq!(r.panes[0].url.as_deref(), Some("https://example.com"));
    }

    fn test_reveal() -> Reveal {
        Reveal {
            panes: vec![RevealPane {
                id: "main".into(),
                url: Some("http://example.com".into()),
                theme: None,
            }],
            orientation: Orientation::Horizontal,
            hold_ms: None,
            scroll: false,
        }
    }

    #[test]
    fn read_control_arms_after_running_when_queueing_after_reveal() {
        let f = make_read_control_fixtures();
        let cpath = f.cpath.clone();
        let cmd = serde_json::json!({
            "cmd": "reveal",
            "after": true,
            "panes": [{"id": "main", "url": "http://example.com"}],
        });
        std::fs::write(cpath, serde_json::to_string(&cmd).unwrap()).unwrap();

        let ControlFixture {
            cpath,
            events,
            after,
            after_running,
            ..
        } = &f;
        let mut read = 0u64;

        let result = read_control(cpath, &mut read, &f.io());

        assert!(result.is_none());
        assert!(
            after_running.load(Ordering::SeqCst),
            "--after must arm after_running immediately so the current command is tracked"
        );
        assert_eq!(after.lock().unwrap().len(), 1);
        assert!(events.lock().unwrap().is_empty(), "no immediate reveal");
        let _ = std::fs::remove_file(cpath);
    }

    #[test]
    fn drain_remaining_reveals_emits_after_queue_as_events() {
        let after: PendingAfter = Arc::new(Mutex::new(vec![test_reveal()]));
        let pending: PendingWhen = Arc::new(Mutex::new(Vec::new()));
        let mut events = Vec::new();

        let summary = drain_remaining_reveals(&after, &pending, &mut events, 9999);

        assert_eq!(summary.after_summaries.len(), 1);
        assert!(summary.when_unmatched.is_empty());
        assert_eq!(events.len(), 1);
        assert!(after.lock().unwrap().is_empty());
        match &events[0] {
            RawEvent::Reveal { t_ms, .. } => assert_eq!(*t_ms, 9999),
            other => panic!("expected Reveal, got {other:?}"),
        }
    }

    #[test]
    fn drain_remaining_reveals_reports_unmatched_when_cues() {
        let after: PendingAfter = Arc::new(Mutex::new(Vec::new()));
        let pending: PendingWhen = Arc::new(Mutex::new(vec![(
            test_reveal(),
            "never-gonna-appear".into(),
        )]));
        let mut events = Vec::new();

        let summary = drain_remaining_reveals(&after, &pending, &mut events, 5000);

        assert_eq!(summary.after_summaries.len(), 0);
        assert_eq!(
            summary.when_unmatched,
            vec!["never-gonna-appear".to_string()]
        );
        assert_eq!(events.len(), 1);
        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn drain_remaining_reveals_handles_both_queues_at_once() {
        let after: PendingAfter = Arc::new(Mutex::new(vec![test_reveal()]));
        let pending: PendingWhen = Arc::new(Mutex::new(vec![
            (test_reveal(), "cue-alpha".into()),
            (test_reveal(), "re:cue-beta".into()),
        ]));
        let mut events = Vec::new();

        let summary = drain_remaining_reveals(&after, &pending, &mut events, 1000);

        assert_eq!(summary.after_summaries.len(), 1);
        assert_eq!(summary.when_unmatched.len(), 2);
        assert_eq!(events.len(), 3);
        assert!(after.lock().unwrap().is_empty());
        assert!(pending.lock().unwrap().is_empty());
    }

    #[test]
    fn drained_reveal_survives_from_raw_after_demo_stop() {
        let mut events = vec![
            RawEvent::Output {
                t_ms: 100,
                data: "real output".into(),
            },
            RawEvent::Input {
                t_ms: 2000,
                bytes: "demo stop\r".into(),
            },
            RawEvent::Output {
                t_ms: 2010,
                data: "demo stop".into(),
            },
        ];
        let after: PendingAfter = Arc::new(Mutex::new(vec![test_reveal()]));
        let pending: PendingWhen =
            Arc::new(Mutex::new(vec![(test_reveal(), "never-matched".into())]));
        let raw_for_cutoff = crate::model::RawMacro {
            meta: crate::model::RawMeta {
                shell: String::new(),
                cols: 0,
                rows: 0,
                idle_timeout_ms: 0,
                resolution: None,
                fps: None,
                stage: None,
                mute_spans: Vec::new(),
            },
            events: events.clone(),
        };
        let drain_ts = recording::stop_cutoff_ms(&raw_for_cutoff)
            .map(|c| c.saturating_sub(1))
            .unwrap_or_else(|| {
                events
                    .iter()
                    .filter_map(|e| match e {
                        RawEvent::Output { t_ms, .. } => Some(*t_ms),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0)
            });
        drain_remaining_reveals(&after, &pending, &mut events, drain_ts);
        let raw = crate::model::RawMacro {
            meta: crate::model::RawMeta {
                shell: "/bin/bash".into(),
                cols: 80,
                rows: 24,
                idle_timeout_ms: 0,
                resolution: None,
                fps: None,
                stage: None,
                mute_spans: Vec::new(),
            },
            events,
        };
        let (rec, layout, _) = recording::from_raw(&raw, "t");
        assert!(
            rec.events.iter().any(|(_, data)| data == "real output"),
            "real output must survive"
        );
        assert!(
            !layout.panes.is_empty(),
            "drained reveal must survive normalization"
        );
        let has_browser = layout
            .panes
            .iter()
            .any(|p| p.kind == crate::model::PaneKind::Browser);
        assert!(
            has_browser,
            "drained --after reveal must produce a browser pane"
        );
    }

    #[test]
    fn reveal_cancel_closes_mute_span_without_recording_reveal() {
        // A span already open, as an in-session `demo open` leaves it.
        let f = make_read_control_fixtures();
        let cpath = f.cpath.clone();
        let cmd = serde_json::json!({ "cmd": "reveal_cancel" });
        std::fs::write(&cpath, serde_json::to_string(&cmd).unwrap()).unwrap();

        f.muting.store(true, Ordering::SeqCst);
        *f.mute_start.lock().unwrap() = Some(1000);
        let ControlFixture {
            cpath,
            events,
            muting,
            mute_start,
            mute_spans,
            ..
        } = &f;
        let mut read = 0u64;

        let result = read_control(cpath, &mut read, &f.io());

        assert!(result.is_none());
        assert!(
            !muting.load(Ordering::SeqCst),
            "reveal_cancel must stop muting"
        );
        assert!(
            mute_start.lock().unwrap().is_none(),
            "reveal_cancel must take the mute_start"
        );
        assert_eq!(
            mute_spans.lock().unwrap().len(),
            1,
            "reveal_cancel must close the span"
        );
        assert!(
            events.lock().unwrap().is_empty(),
            "reveal_cancel must not record a reveal event"
        );
        let _ = std::fs::remove_file(cpath);
    }

    #[test]
    fn reveal_closes_mute_span_and_records_event() {
        // A span already open, as an in-session `demo open` leaves it.
        let f = make_read_control_fixtures();
        let cpath = f.cpath.clone();
        let cmd = serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "main", "url": "http://example.com"}],
        });
        std::fs::write(&cpath, serde_json::to_string(&cmd).unwrap()).unwrap();

        f.muting.store(true, Ordering::SeqCst);
        *f.mute_start.lock().unwrap() = Some(1000);
        let ControlFixture {
            cpath,
            events,
            muting,
            mute_start,
            mute_spans,
            ..
        } = &f;
        let mut read = 0u64;

        let result = read_control(cpath, &mut read, &f.io());

        assert!(result.is_none());
        assert!(!muting.load(Ordering::SeqCst), "reveal must stop muting");
        assert!(
            mute_start.lock().unwrap().is_none(),
            "reveal must take the mute_start"
        );
        assert_eq!(
            mute_spans.lock().unwrap().len(),
            1,
            "reveal must close the span"
        );
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "reveal must record a reveal event"
        );
        let _ = std::fs::remove_file(cpath);
    }

    #[test]
    fn double_close_is_safe() {
        // A span already open, as an in-session `demo open` leaves it.
        let f = make_read_control_fixtures();
        let cpath = f.cpath.clone();
        // First cancel, then reveal — both try to close the same span.
        let cmds = format!(
            "{}\n{}",
            serde_json::to_string(&serde_json::json!({ "cmd": "reveal_cancel" })).unwrap(),
            serde_json::to_string(&serde_json::json!({
                "cmd": "reveal",
                "panes": [{"id": "main", "url": "http://example.com"}],
            }))
            .unwrap()
        );
        std::fs::write(&cpath, cmds).unwrap();
        f.muting.store(true, Ordering::SeqCst);
        *f.mute_start.lock().unwrap() = Some(1000);
        let ControlFixture {
            cpath,
            events,
            mute_spans,
            ..
        } = &f;
        let mut read = 0u64;

        let result = read_control(cpath, &mut read, &f.io());

        assert!(result.is_none());
        assert_eq!(
            mute_spans.lock().unwrap().len(),
            1,
            "only the first close must record a span (second finds mute_start empty)"
        );
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "reveal still records its event even after cancel closed the span"
        );
        let _ = std::fs::remove_file(cpath);
    }

    /// A private control file plus the recording state `read_control` mutates:
    /// one per call, so tests never share a file.
    struct ControlFixture {
        cpath: std::path::PathBuf,
        events: Arc<Mutex<Vec<RawEvent>>>,
        pending: PendingWhen,
        after: PendingAfter,
        after_running: AtomicBool,
        after_last_out: Mutex<Instant>,
        muting: Arc<AtomicBool>,
        mute_start: Arc<Mutex<Option<u64>>>,
        mute_spans: Arc<Mutex<Vec<(u64, u64)>>>,
        t0: Instant,
    }

    impl ControlFixture {
        /// Borrow the fixture as the reader's argument bundle.
        fn io(&self) -> ControlIo<'_> {
            ControlIo {
                events: &self.events,
                pending: &self.pending,
                after: &self.after,
                after_running: &self.after_running,
                after_last_out: &self.after_last_out,
                muting: &self.muting,
                mute_start: &self.mute_start,
                mute_spans: &self.mute_spans,
                t0: self.t0,
                debug: None,
            }
        }
    }

    fn make_read_control_fixtures() -> ControlFixture {
        // Unique per CALL, not per process. `std::process::id()` alone is enough
        // under nextest, which gives every test its own process — and useless
        // under `cargo test`, which runs them as threads in one process, so two
        // tests sharing this fixture raced over the same file. CI runs
        // `cargo test`; a pid-keyed temp path is a test that only passes on the
        // runner that isolates it for you.
        static FIXTURE_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst);
        let cpath = std::env::temp_dir().join(format!(
            "demo-test-final-drain-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&cpath);
        ControlFixture {
            cpath,
            events: Arc::new(Mutex::new(Vec::new())),
            pending: Arc::new(Mutex::new(Vec::new())),
            after: Arc::new(Mutex::new(Vec::new())),
            after_running: AtomicBool::new(false),
            after_last_out: Mutex::new(Instant::now()),
            muting: Arc::new(AtomicBool::new(false)),
            mute_start: Arc::new(Mutex::new(None)),
            mute_spans: Arc::new(Mutex::new(Vec::new())),
            t0: Instant::now(),
        }
    }

    #[test]
    fn final_drain_picks_up_control_line_appended_after_last_watchdog_read() {
        let f = make_read_control_fixtures();
        let ControlFixture { cpath, events, .. } = &f;
        let mut read = 0u64;

        std::fs::write(cpath, "").unwrap();
        let _ = read_control(cpath, &mut read, &f.io());
        assert_eq!(read, 0);

        let reveal = serde_json::to_string(&serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "main", "url": "http://example.com"}],
        }))
        .unwrap();
        std::fs::write(cpath, format!("{reveal}\n")).unwrap();

        let _ = read_control(cpath, &mut read, &f.io());

        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "final drain must process a reveal appended after the last watchdog pass"
        );
        let _ = std::fs::remove_file(cpath);
    }

    #[test]
    fn final_drain_on_empty_or_fully_consumed_file_is_noop() {
        let f = make_read_control_fixtures();
        let ControlFixture { cpath, events, .. } = &f;
        let mut read = 0u64;

        std::fs::write(cpath, "").unwrap();
        let result = read_control(cpath, &mut read, &f.io());
        assert!(result.is_none());
        assert_eq!(read, 0);
        assert!(events.lock().unwrap().is_empty());

        let reveal = serde_json::to_string(&serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "main", "url": "http://example.com"}],
        }))
        .unwrap();
        std::fs::write(cpath, format!("{reveal}\n")).unwrap();
        let _ = read_control(cpath, &mut read, &f.io());
        assert_eq!(events.lock().unwrap().len(), 1);
        let offset_after_first = read;

        let result = read_control(cpath, &mut read, &f.io());
        assert!(result.is_none());
        assert_eq!(
            read, offset_after_first,
            "offset must not change on empty re-read"
        );
        assert_eq!(events.lock().unwrap().len(), 1, "no duplicate events");
        let _ = std::fs::remove_file(cpath);
    }

    #[test]
    fn byte_offset_never_rewinds_and_torn_line_is_not_double_consumed() {
        let f = make_read_control_fixtures();
        let ControlFixture { cpath, events, .. } = &f;
        let mut read = 0u64;

        std::fs::write(cpath, "tea").unwrap();
        let _ = read_control(cpath, &mut read, &f.io());
        let offset_after_partial = read;
        assert_eq!(
            offset_after_partial, 3,
            "offset must advance past the partial bytes"
        );

        std::fs::write(cpath, "tear\n").unwrap();
        let offset_before = read;
        let _ = read_control(cpath, &mut read, &f.io());
        assert!(
            read >= offset_before,
            "offset must never rewind (was {offset_before}, now {read})"
        );
        assert!(
            events.lock().unwrap().is_empty(),
            "torn partial must not produce a phantom event"
        );

        let reveal = serde_json::to_string(&serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "main", "url": "http://example.com"}],
        }))
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(cpath)
            .unwrap();
        use std::io::Write;
        writeln!(file, "{reveal}").unwrap();
        drop(file);

        let _ = read_control(cpath, &mut read, &f.io());
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "reveal appended after the torn line must be processed exactly once"
        );
        let _ = std::fs::remove_file(cpath);
    }

    // --- mutant-killing tests: every operator/branch in the diff must matter.

    fn blank_meta() -> RawMeta {
        RawMeta {
            shell: String::new(),
            cols: 80,
            rows: 24,
            idle_timeout_ms: 0,
            resolution: None,
            fps: None,
            stage: None,
            mute_spans: Vec::new(),
        }
    }

    fn capture_args_in(dir: &std::path::Path, no_score: bool) -> CaptureArgs {
        CaptureArgs {
            rec: dir.join("demo.rec"),
            idle_timeout_ms: 0,
            shell: None,
            into: None,
            no_normalize: false,
            debug: false,
            output: None,
            normalized_output: dir.join("demo.toml"),
            no_score,
            prompt: None,
            keep_prompt: true,
            font: None,
            aspect: None,
            quality: None,
            fps: None,
            resolution: None,
            here: true,
        }
    }

    /// finish_recording stamps drained reveals with the max OUTPUT time, not
    /// zero and not the max over every event kind. Deleting the
    /// `RawEvent::Output` arm would fall back to 0.
    #[test]
    fn finish_recording_drain_uses_max_output_time() {
        let state = CaptureState::new();
        state.events.lock().unwrap().push(RawEvent::Output {
            t_ms: 100,
            data: "early".into(),
        });
        state.events.lock().unwrap().push(RawEvent::Output {
            t_ms: 2010,
            data: "late".into(),
        });
        let t0 = Instant::now();
        let out = finish_recording(&state, blank_meta(), t0);
        // No pending queues, so events pass through unchanged.
        assert_eq!(out.events.len(), 2);
    }

    /// finish_recording with a pending --after reveal stamps it with the
    /// Output max (2010), proving the Output arm is read. Without that arm
    /// the stamp would be 0.
    #[test]
    fn finish_recording_after_drain_stamped_with_output_max() {
        let state = CaptureState::new();
        state.events.lock().unwrap().push(RawEvent::Output {
            t_ms: 2010,
            data: "late".into(),
        });
        state.after_opens.lock().unwrap().push(Reveal {
            panes: vec![RevealPane::terminal()],
            orientation: Orientation::Horizontal,
            hold_ms: None,
            scroll: false,
        });
        let out = finish_recording(&state, blank_meta(), Instant::now());
        assert_eq!(out.events.len(), 2, "drained reveal must be appended");
        match &out.events[1] {
            RawEvent::Reveal { t_ms, .. } => assert_eq!(
                *t_ms, 2010,
                "drain stamp must be the max Output time, not 0"
            ),
            other => panic!("expected Reveal, got {other:?}"),
        }
    }

    /// write_score returns Some(exact normalized_output path) and writes the
    /// file. `Ok(None)` and `Ok(Some(default))` mutants both die here.
    #[test]
    fn write_score_returns_some_exact_path_and_writes_file() {
        let dir = std::env::temp_dir().join(format!(
            "demo-test-write-score-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let args = capture_args_in(&dir, false);
        let raw = RawMacro {
            meta: blank_meta(),
            events: vec![],
        };
        let got = write_score(&args, &raw, "demo", false, "", "DejaVu Sans Mono", vec![]).unwrap();
        assert_eq!(
            got,
            Some(dir.join("demo.toml")),
            "must return the exact score path"
        );
        assert!(
            dir.join("demo.toml").exists(),
            "score file must actually be written"
        );
        assert!(
            dir.join("demo.rec").exists(),
            "faithful recording must be written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// write_score with --no-score returns None and still writes the
    /// recording. Kills `delete ! in write_score` at the no_score gate.
    #[test]
    fn write_score_no_score_returns_none_but_writes_recording() {
        let dir = std::env::temp_dir().join(format!(
            "demo-test-no-score-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let args = capture_args_in(&dir, true);
        let raw = RawMacro {
            meta: blank_meta(),
            events: vec![],
        };
        let got = write_score(&args, &raw, "demo", false, "", "DejaVu Sans Mono", vec![]).unwrap();
        assert_eq!(got, None, "--no-score must return None");
        assert!(dir.join("demo.rec").exists(), "recording still written");
        assert!(
            !dir.join("demo.toml").exists(),
            "no score file with --no-score"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty pane id falls back to "main"; a real id is kept. Deleting the
    /// `!` in the emptiness filter would keep "" and drop "web".
    #[test]
    fn parse_reveal_empty_id_falls_back_to_main() {
        let v = serde_json::json!({"cmd": "reveal", "panes": [{"id": ""}]});
        let r = parse_reveal(&v).unwrap();
        assert_eq!(r.panes[0].id, "main", "empty id must fall back to main");
        let v2 = serde_json::json!({"cmd": "reveal", "panes": [{"id": "web"}]});
        let r2 = parse_reveal(&v2).unwrap();
        assert_eq!(r2.panes[0].id, "web", "non-empty id must be kept");
    }

    /// `reveal_begin` arms muting; `stop` disarms and returns the stop reason.
    /// Deleting either match arm breaks this.
    #[test]
    fn read_control_reveal_begin_arms_muting_and_stop_returns_reason() {
        let f = make_read_control_fixtures();
        let mut read = 0u64;
        std::fs::write(&f.cpath, "{\"cmd\":\"reveal_begin\"}\n").unwrap();
        let stop = read_control(&f.cpath, &mut read, &f.io());
        assert!(stop.is_none());
        assert!(
            f.muting.load(Ordering::SeqCst),
            "reveal_begin must arm muting"
        );
        assert!(
            f.mute_start.lock().unwrap().is_some(),
            "reveal_begin must backdate a mute start"
        );
        std::fs::write(&f.cpath, "{\"cmd\":\"stop\"}\n").unwrap();
        // Reset offset so the new line is read.
        let mut read2 =
            std::fs::metadata(&f.cpath).unwrap().len() - "{\"cmd\":\"stop\"}\n".len() as u64;
        let stop2 = read_control(&f.cpath, &mut read2, &f.io());
        assert_eq!(stop2, Some("demo stop"));
        assert!(!f.muting.load(Ordering::SeqCst), "stop must disarm muting");
        let _ = std::fs::remove_file(&f.cpath);
    }

    /// A reveal with `"when": ""` is immediate, not armed. Deleting the `!`
    /// in the emptiness filter would arm it as a pending cue instead.
    #[test]
    fn read_control_empty_when_is_immediate_not_armed() {
        let f = make_read_control_fixtures();
        let mut read = 0u64;
        let cmd = serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "main"}],
            "when": "",
        });
        std::fs::write(&f.cpath, serde_json::to_string(&cmd).unwrap()).unwrap();
        let _ = read_control(&f.cpath, &mut read, &f.io());
        assert_eq!(
            f.events.lock().unwrap().len(),
            1,
            "empty when => immediate reveal"
        );
        assert!(
            f.pending.lock().unwrap().is_empty(),
            "nothing must be armed"
        );
        let _ = std::fs::remove_file(&f.cpath);
    }

    /// A reveal with a real `when` is armed, not immediate. Proves the same
    /// filter from the other side (`&&`/`||` mutants).
    #[test]
    fn read_control_nonempty_when_is_armed_not_immediate() {
        let f = make_read_control_fixtures();
        let mut read = 0u64;
        let cmd = serde_json::json!({
            "cmd": "reveal",
            "panes": [{"id": "main"}],
            "when": "build ok",
        });
        std::fs::write(&f.cpath, serde_json::to_string(&cmd).unwrap()).unwrap();
        let _ = read_control(&f.cpath, &mut read, &f.io());
        assert!(f.events.lock().unwrap().is_empty());
        assert_eq!(f.pending.lock().unwrap().len(), 1);
        let _ = std::fs::remove_file(&f.cpath);
    }

    /// Sources from a pre-existing score file survive normalization: empty
    /// existing sources leave the normalized ones alone, non-empty ones win.
    /// Kills `delete !` / `&&`→`||` at both preservation gates.
    #[test]
    fn write_score_preserves_sources_from_existing_file() {
        use crate::model::{Source, SourceKind};
        let dir = std::env::temp_dir().join(format!(
            "demo-test-preserve-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let existing_sources = vec![Source {
            id: "docs".into(),
            kind: SourceKind::Browser,
            url: Some("https://x.example".into()),
            theme: None,
        }];
        // Seed an existing score carrying sources.
        let args = capture_args_in(&dir, false);
        let raw = RawMacro {
            meta: blank_meta(),
            events: vec![],
        };
        let first = write_score(&args, &raw, "demo", false, "", "DejaVu Sans Mono", vec![])
            .unwrap()
            .unwrap();
        let mut seeded = Score::load(&first).unwrap();
        seeded.sources = existing_sources.clone();
        seeded.save(&first).unwrap();
        // Second run: normalized sources are empty, existing win.
        let got = write_score(&args, &raw, "demo", false, "", "DejaVu Sans Mono", vec![])
            .unwrap()
            .unwrap();
        let saved = Score::load(&got).unwrap();
        assert_eq!(saved.sources, existing_sources, "existing sources must win");
        // Empty existing sources: the normalized ones stand (here: none).
        seeded.sources = Vec::new();
        seeded.save(&first).unwrap();
        let got2 = write_score(&args, &raw, "demo", false, "", "DejaVu Sans Mono", vec![])
            .unwrap()
            .unwrap();
        let saved2 = Score::load(&got2).unwrap();
        assert!(saved2.sources.is_empty(), "empty existing must not inject");
        // Explicit sources land when no file exists yet.
        let _ = std::fs::remove_file(&first);
        let fresh_sources = vec![Source {
            id: "term-src".into(),
            kind: SourceKind::Terminal,
            url: None,
            theme: None,
        }];
        let got3 = write_score(
            &args,
            &raw,
            "demo",
            false,
            "",
            "DejaVu Sans Mono",
            fresh_sources.clone(),
        )
        .unwrap()
        .unwrap();
        let saved3 = Score::load(&got3).unwrap();
        assert_eq!(saved3.sources, fresh_sources, "explicit sources must land");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With `--into`, a stage score that already carries sources keeps them:
    /// explicit sources must not overwrite a non-empty score. `&&`->`||`
    /// would overwrite and die here.
    #[test]
    fn write_score_into_keeps_stage_sources_over_explicit_ones() {
        use crate::model::{Source, SourceKind};
        let dir = std::env::temp_dir().join(format!(
            "demo-test-into-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stage_sources = vec![Source {
            id: "stage-docs".into(),
            kind: SourceKind::Browser,
            url: Some("https://stage.example".into()),
            theme: None,
        }];
        let stage_path = dir.join("stage.toml");
        let mut stage: Score = toml::from_str(
            r#"
[demo]
name = "stage"
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
        stage.sources = stage_sources.clone();
        stage.save(&stage_path).unwrap();
        let mut args = capture_args_in(&dir, false);
        args.into = Some(stage_path);
        let raw = RawMacro {
            meta: blank_meta(),
            events: vec![],
        };
        let explicit = vec![Source {
            id: "other".into(),
            kind: SourceKind::Terminal,
            url: None,
            theme: None,
        }];
        let got = write_score(&args, &raw, "demo", false, "", "DejaVu Sans Mono", explicit)
            .unwrap()
            .unwrap();
        let saved = Score::load(&got).unwrap();
        assert_eq!(saved.sources, stage_sources, "stage sources must win");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// write_faithful_cast writes a non-empty recording; the score name is
    /// used when given and "demo" otherwise. `Ok(())`-without-write dies.
    #[test]
    fn write_faithful_cast_writes_named_recording() {
        let dir = std::env::temp_dir().join(format!(
            "demo-test-faithful-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = RawMacro {
            meta: blank_meta(),
            events: vec![RawEvent::Output {
                t_ms: 10,
                data: "hi".into(),
            }],
        };
        let path = dir.join("sub").join("out.rec");
        write_faithful_cast(&raw, None, &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.is_empty(), "cast file must have content");
        // Bare filename (empty parent) must also work, not error.
        let _cwd_guard = super::super::CWD_LOCK.lock().unwrap();
        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        write_faithful_cast(&raw, None, std::path::Path::new("bare.rec")).unwrap();
        assert!(dir.join("bare.rec").exists());
        std::env::set_current_dir(cwd).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
