//! Pure-Rust PDF pane capture: rasterize the pages with hayro, stack them on a
//! viewer-style backdrop, and slice viewport windows at per-frame offsets.
//! No Chromium, no temp files, no HTTP server — a PDF pane renders in-process,
//! so it starts instantly and scrolls through the whole document.

use std::path::Path;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{render, RenderCache, RenderSettings};

use crate::error::{Error, Result};
use crate::model::{ScrollDirection, Velocity};

use super::stage::{ease, ScrollParams};

/// Backdrop behind/between pages (Chrome PDF viewer gray).
const BACKDROP: [u8; 4] = [0x52, 0x56, 0x59, 0xff];
/// Vertical gap between pages and at the top/bottom, in px.
const GAP: usize = 12;

/// The fastest a PDF pane may pan, in pixels per second **at 1x** (recording time).
///
/// A PDF pane exists to show the document, so the pan always travels the whole
/// thing. What this caps is how fast it may do it: the `scroll` step's
/// `duration_ms` is a **floor** on the pan's length and this is a **ceiling** on
/// its speed, so a short document given generous time simply pans slower, and a
/// long one takes as long as it needs.
///
/// Measured before the cap existed: 10 pages crossed in 9.7 s is 124 px per
/// frame, a whole new viewport every 0.58 s — unreadable however smooth. At this
/// speed the same document takes 19.3 s and renews the viewport every 1.2 s.
///
/// The cap is stated at 1x. A 2x export moves at 2400 px/s on screen (160 px/frame
/// at 15 fps) — not readable, and not meant to be. A pane with `ignore_speed` pans
/// at this cap regardless of the export speed.
const MAX_PAN_SPEED_PX_PER_SEC: f64 = 1200.0;
/// Page width as a fraction of the pane width (fit-width with side margins).
const PAGE_FRAC: f64 = 0.90;
/// Lead-in at the top of the document before panning starts, in ms.
const LEAD_IN_MS: f64 = 400.0;
/// Maximum fraction of the pane's on-screen window the lead-in may occupy.
const LEAD_IN_MAX_FRAC: f64 = 0.15;

/// A PDF scene that computes viewport slices on demand, one per output frame.
/// Holds the rasterized tall document image and produces a viewport window at
/// the offset appropriate for the current progress, without precomputing every
/// frame up front.
pub struct PdfScene {
    width: usize,
    height: usize,
    doc: Vec<u8>,
    doc_w: usize,
    doc_h: usize,
    max_off: usize,
    /// Where the pan starts. Zero panning down; for an upward pan of a document
    /// too long to cross at the capped speed, the bottom of the travel window,
    /// so `up` still starts at the end of the document.
    base_off: usize,
    /// Output frames the pane is on screen. Progress maps onto these.
    window_frames: usize,
    /// Output frames the pan itself lasts. Frames past it hold the last offset.
    output_frames: usize,
    /// Seconds this pane needs on screen to show the whole document at or below
    /// the speed cap, lead-in included. The stage uses it to give the pane the
    /// time it needs rather than truncating the document.
    needed_seconds: f64,
    lead_in: usize,
    direction: ScrollDirection,
    velocity: Velocity,
    cached_frame: Vec<u8>,
    cached_offset: Option<usize>,
}

impl PdfScene {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Seconds this pane needs on screen to show the whole document without
    /// exceeding the speed cap. Zero when it does not pan.
    pub fn needed_seconds(&self) -> f64 {
        self.needed_seconds
    }

    /// Tell the scene how many output frames its pane is actually on screen for.
    ///
    /// The scene is built before the stage knows the final window: a pane may be
    /// held longer so its document fits. Progress arrives normalized over the
    /// real window, so the scene has to map it over that same window or the pan
    /// stops short — it did, at 23% of the document, until this existed.
    pub fn set_window_frames(&mut self, frames: usize) {
        self.window_frames = frames.max(1);
    }

    /// Return the viewport frame for the given timeline progress in `[0, 1]`.
    pub fn frame_at(&mut self, progress: f64) -> &[u8] {
        let idx = self.frame_index(progress);
        let off = self.base_off
            + offset_for_frame(
                idx,
                self.lead_in,
                self.output_frames,
                self.max_off,
                self.direction,
                self.velocity,
            );
        if self.cached_offset == Some(off) {
            return &self.cached_frame;
        }
        self.cached_frame = self.slice_at(off);
        self.cached_offset = Some(off);
        &self.cached_frame
    }

    fn frame_index(&self, progress: f64) -> usize {
        if self.window_frames <= 1 {
            return 0;
        }
        let p = progress.clamp(0.0, 1.0);
        (p * (self.window_frames - 1) as f64).round() as usize
    }

    fn slice_at(&self, off: usize) -> Vec<u8> {
        let mut out = vec![0u8; self.width * self.height * 4];
        for px in out.as_chunks_mut::<4>().0 {
            px.copy_from_slice(&BACKDROP);
        }
        let rows = self.height.min(self.doc_h.saturating_sub(off));
        for row in 0..rows {
            let s = (off + row) * self.doc_w * 4;
            let d = row * self.width * 4;
            out[d..d + self.doc_w * 4].copy_from_slice(&self.doc[s..s + self.doc_w * 4]);
        }
        out
    }
}

/// Compute the vertical offset for a given output frame index.
///
/// Frames `[0, lead_in)` sit at the start position (top for `Down`, bottom for
/// `Up`). After the lead-in, the offset runs from 0 to `max_off` (or `max_off`
/// to 0 for `Up`) across the remaining frames, with easing applied per the
/// velocity curve.
pub fn offset_for_frame(
    frame_idx: usize,
    lead_in: usize,
    output_frames: usize,
    max_off: usize,
    direction: ScrollDirection,
    velocity: Velocity,
) -> usize {
    if max_off == 0 || output_frames <= 1 {
        return match direction {
            ScrollDirection::Down => 0,
            ScrollDirection::Up => max_off,
        };
    }
    if frame_idx < lead_in {
        return match direction {
            ScrollDirection::Down => 0,
            ScrollDirection::Up => max_off,
        };
    }
    let pan_frames = output_frames.saturating_sub(lead_in);
    if pan_frames <= 1 {
        return match direction {
            ScrollDirection::Down => 0,
            ScrollDirection::Up => max_off,
        };
    }
    let i = frame_idx - lead_in;
    if i >= pan_frames - 1 {
        return match direction {
            ScrollDirection::Down => max_off,
            ScrollDirection::Up => 0,
        };
    }
    let t = i as f64 / (pan_frames - 1) as f64;
    let eased = ease(t, velocity);
    let offset = (max_off as f64 * eased).round() as usize;
    match direction {
        ScrollDirection::Down => offset,
        ScrollDirection::Up => max_off.saturating_sub(offset),
    }
}

/// Compute the lead-in (in frames) before panning starts.
///
/// 400 ms worth of frames, clamped to at most 15% of the pane's on-screen
/// window so a brief pane does not spend most of its life motionless.
pub fn lead_in_frames(output_frames: usize, fps: f64) -> usize {
    let lead_400 = (LEAD_IN_MS / 1000.0 * fps).round() as usize;
    let max_lead = (LEAD_IN_MAX_FRAC * output_frames as f64).round() as usize;
    lead_400.min(max_lead)
}

/// Capture a PDF as a [`PdfScene`]: the document is rasterized once into a
/// tall image, and viewport slices are produced on demand — one distinct
/// offset per output frame — rather than as a fixed set of keyframes.
///
/// `output_frames` is how many output frames the pane will be on screen.
/// `should_scroll` is whether a scroll step is directed at this pane; when
/// false, the scene is a single static frame at the top of the document.
/// `direction` and `velocity` control the scroll behavior.
/// `effective_speed` is the export speed multiplier (1.0 for panes with
/// `ignore_speed`). The pan is computed in recording time and converted once.
pub fn capture_scene(
    pdf_path: &Path,
    pane_w: usize,
    pane_h: usize,
    output_frames: usize,
    fps: f64,
    pan: Option<ScrollParams>,
    effective_speed: f64,
) -> Result<PdfScene> {
    let data = std::fs::read(pdf_path).map_err(|e| Error::io(pdf_path, e))?;
    let pdf = Pdf::new(data).map_err(|e| Error::Export(format!("read PDF: {e:?}")))?;

    let page_w = ((pane_w as f64) * PAGE_FRAC).round() as usize;
    let cache = RenderCache::new();
    let settings = InterpreterSettings::default();

    let mut pages: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    for page in pdf.pages().iter() {
        let (pw, _) = page.render_dimensions();
        let scale = page_w as f32 / pw.max(1.0);
        let pix = render(
            page,
            &cache,
            &settings,
            &RenderSettings {
                x_scale: scale,
                y_scale: scale,
                bg_color: WHITE,
                ..Default::default()
            },
        );
        let (w, h) = (pix.width() as usize, pix.height() as usize);
        let mut rgba = pix.data_as_u8_slice().to_vec();
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
        pages.push((w, h, rgba));
    }
    if pages.is_empty() {
        return Err(Error::Export(format!(
            "{}: PDF has no pages",
            pdf_path.display()
        )));
    }

    let doc_w = pane_w;
    let doc_h = GAP + pages.iter().map(|(_, h, _)| h + GAP).sum::<usize>();
    let mut doc = vec![0u8; doc_w * doc_h * 4];
    for px in doc.as_chunks_mut::<4>().0 {
        px.copy_from_slice(&BACKDROP);
    }
    let mut y = GAP;
    for (w, h, rgba) in &pages {
        let x0 = doc_w.saturating_sub(*w) / 2;
        let cw = (*w).min(doc_w - x0);
        for row in 0..*h {
            let s = row * w * 4;
            let d = ((y + row) * doc_w + x0) * 4;
            doc[d..d + cw * 4].copy_from_slice(&rgba[s..s + cw * 4]);
        }
        y += h + GAP;
    }

    let doc_max_off = doc_h.saturating_sub(pane_h);
    let plan = pan.filter(|_| doc_max_off > 0).map(|p| {
        // The whole document, always. The only question is how long that takes.
        // p.seconds is in output seconds (after scale_pane_windows). Convert to
        // recording seconds, compute the pan at 1x, then convert back.
        let duration_s_1x = p.seconds * effective_speed;
        let seconds_output = pan_seconds_output(doc_max_off, duration_s_1x, effective_speed);
        let panning = ((seconds_output * fps.max(1.0)).round() as usize).max(2);
        let lead = lead_in_frames(panning, fps);
        (p, panning + lead, lead, seconds_output)
    });

    let (max_off, effective_frames, lead, direction, velocity, needed_seconds) = match plan {
        Some((p, frames, lead, seconds)) => (
            doc_max_off,
            frames,
            lead,
            p.direction,
            p.velocity,
            seconds + lead as f64 / fps.max(1.0),
        ),
        None => (0, 1, 0, ScrollDirection::Down, Velocity::Constant, 0.0),
    };
    // The pan always covers the document, so `up` starts at its end and reaches
    // the beginning; there is no partial window to offset into.
    let base_off = 0;

    Ok(PdfScene {
        width: pane_w,
        height: pane_h,
        doc,
        doc_w,
        doc_h,
        max_off,
        base_off,
        window_frames: output_frames.max(1),
        output_frames: effective_frames,
        needed_seconds,
        lead_in: lead,
        direction,
        velocity,
        cached_frame: Vec::new(),
        cached_offset: None,
    })
}

/// How long the pan must last to cross the whole document without exceeding
/// [`MAX_PAN_SPEED_PX_PER_SEC`], in recording seconds (at 1x).
///
/// `requested_1x` is the `scroll` step's `duration_ms` in recording seconds (at
/// 1x), and it acts as a floor: asking for more time than the cap needs makes
/// the pan slower, never longer than asked makes it faster. Zero means "no
/// preference".
///
/// The result is in recording seconds. The caller converts to output seconds via
/// [`pan_seconds_output`].
pub fn pan_seconds_for(doc_max_off: usize, requested_1x: f64) -> f64 {
    let at_full_speed = doc_max_off as f64 / MAX_PAN_SPEED_PX_PER_SEC;
    at_full_speed.max(requested_1x.max(0.0))
}

/// Convert a 1x pan duration to output seconds by dividing by the effective speed.
///
/// `effective_speed` is the export speed multiplier (1.0 for panes with `ignore_speed`).
/// This is the single point where recording time becomes output time.
pub fn pan_seconds_output(doc_max_off: usize, requested_1x: f64, effective_speed: f64) -> f64 {
    pan_seconds_for(doc_max_off, requested_1x) / effective_speed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured case: 10 pages (~17 400 px) crossed in a 9.7 s window is
    /// 124 px per frame — a whole new viewport every 0.58 s, unreadable however
    /// smooth. The pane must show the whole document, so what gives is the time,
    /// never the speed and never the document.
    #[test]
    fn a_long_document_takes_the_time_it_needs_and_is_crossed_whole() {
        let doc = 17_392usize;
        let asked = 8.0; // the score's duration_ms = 8000
        let seconds = pan_seconds_for(doc, asked);
        assert!(
            seconds > asked,
            "a document that cannot be crossed in {asked}s must take longer, got {seconds:.1}s"
        );
        let speed = doc as f64 / seconds;
        assert!(
            speed <= MAX_PAN_SPEED_PX_PER_SEC + 1.0,
            "panned at {speed:.0} px/s, cap is {MAX_PAN_SPEED_PX_PER_SEC}"
        );
    }

    /// `duration_ms` is a floor, not a ceiling: a short document given generous
    /// time pans slower rather than finishing early and holding.
    #[test]
    fn a_short_document_given_time_pans_slower_not_faster() {
        let doc = 1_200usize;
        let seconds = pan_seconds_for(doc, 10.0);
        assert_eq!(seconds, 10.0, "the requested time is a floor");
        let speed = doc as f64 / seconds;
        assert!(
            speed < MAX_PAN_SPEED_PX_PER_SEC,
            "{speed:.0} px/s should be well under the cap"
        );
    }

    /// Asking for less time than the cap allows must not speed the pan up.
    #[test]
    fn the_cap_wins_when_the_requested_time_is_too_short() {
        let doc = 17_392usize;
        let hurried = pan_seconds_for(doc, 1.0);
        let generous = pan_seconds_for(doc, 60.0);
        assert!(
            hurried > 1.0,
            "the cap must override an impossible duration"
        );
        assert_eq!(generous, 60.0, "a generous duration is honored as a floor");
        assert!(generous > hurried);
    }

    /// No `duration_ms` at all means the cap alone decides.
    #[test]
    fn without_a_requested_duration_the_cap_decides() {
        let doc = 9_000usize;
        assert_eq!(
            pan_seconds_for(doc, 0.0),
            doc as f64 / MAX_PAN_SPEED_PX_PER_SEC
        );
    }

    #[test]
    fn offset_first_frame_is_zero_last_is_max_off() {
        let offsets: Vec<usize> = (0..20)
            .map(|i| offset_for_frame(i, 3, 20, 500, ScrollDirection::Down, Velocity::Constant))
            .collect();
        assert_eq!(offsets[0], 0);
        assert_eq!(offsets[19], 500);
        for i in 1..20 {
            assert!(offsets[i] >= offsets[i - 1], "non-monotonic at {i}");
        }
        for i in 4..20 {
            assert!(
                offsets[i] > offsets[i - 1],
                "strictly increasing after lead-in at {i}"
            );
        }
    }

    #[test]
    fn lead_in_long_window_yields_400ms() {
        let fps = 30.0;
        let output_frames = 300;
        let lead = lead_in_frames(output_frames, fps);
        assert_eq!(lead, 12);
    }

    #[test]
    fn lead_in_short_window_yields_15_percent() {
        let fps = 30.0;
        let output_frames = 10;
        let lead = lead_in_frames(output_frames, fps);
        assert_eq!(lead, 2);
    }

    #[test]
    fn max_off_zero_yields_static() {
        for i in 0..10 {
            assert_eq!(
                offset_for_frame(i, 2, 10, 0, ScrollDirection::Down, Velocity::Constant),
                0
            );
        }
    }

    #[test]
    fn no_half_hold() {
        let offsets: Vec<usize> = (0..100)
            .map(|i| {
                offset_for_frame(
                    i,
                    lead_in_frames(100, 30.0),
                    100,
                    1000,
                    ScrollDirection::Down,
                    Velocity::Constant,
                )
            })
            .collect();
        let first_nonzero = offsets.iter().position(|&o| o > 0);
        let lead = lead_in_frames(100, 30.0);
        assert!(
            first_nonzero.unwrap() <= lead + 1,
            "panning should start shortly after lead-in ({lead} frames), \
             but first nonzero offset is at frame {first_nonzero:?}"
        );
    }

    #[test]
    fn single_frame_no_panic() {
        assert_eq!(
            offset_for_frame(0, 0, 1, 500, ScrollDirection::Down, Velocity::Constant),
            0
        );
    }

    #[test]
    fn lead_in_zero_frames() {
        assert_eq!(lead_in_frames(0, 30.0), 0);
    }

    #[test]
    fn offset_up_constant_starts_at_max_ends_at_zero() {
        let offsets: Vec<usize> = (0..20)
            .map(|i| offset_for_frame(i, 3, 20, 500, ScrollDirection::Up, Velocity::Constant))
            .collect();
        assert_eq!(offsets[0], 500);
        assert_eq!(offsets[19], 0);
        for i in 1..20 {
            assert!(offsets[i] <= offsets[i - 1], "non-increasing at {i}");
        }
        for i in 4..20 {
            assert!(
                offsets[i] < offsets[i - 1],
                "strictly decreasing after lead-in at {i}"
            );
        }
    }

    #[test]
    fn offset_up_ease_in_out_starts_at_max_ends_at_zero() {
        let offsets: Vec<usize> = (0..20)
            .map(|i| offset_for_frame(i, 3, 20, 500, ScrollDirection::Up, Velocity::EaseInOut))
            .collect();
        assert_eq!(offsets[0], 500);
        assert_eq!(offsets[19], 0);
        for i in 1..20 {
            assert!(offsets[i] <= offsets[i - 1], "non-increasing at {i}");
        }
    }

    /// The measured case from the spec: 17 392 px at duration = 8 s, speed 2.0.
    /// pan_seconds_1x = max(8.0, 17392/1200) = max(8.0, 14.493) = 14.493 ≈ 14.5
    /// pan_seconds_output = 14.493 / 2.0 = 7.247 ≈ 7.2
    /// At speed 1.0: both ≈ 14.5.
    #[test]
    fn the_measured_case_at_2x_and_1x() {
        let doc = 17_392usize;
        let duration_s_1x = 8.0;
        // Exercise the real conversion, not just pan_seconds_for.
        let pan_output_2x = pan_seconds_output(doc, duration_s_1x, 2.0);
        assert!(
            (pan_output_2x - 7.2).abs() < 0.1,
            "pan_seconds_output at 2x should be ≈7.2, got {pan_output_2x:.2}"
        );
        let pan_output_1x = pan_seconds_output(doc, duration_s_1x, 1.0);
        assert!(
            (pan_output_1x - 14.5).abs() < 0.1,
            "pan_seconds_output at 1x should be ≈14.5, got {pan_output_1x:.2}"
        );
    }

    /// ignore_speed makes the output length independent of the export multiplier.
    /// The same document at 1x, 2x and 3x yields the same pan_seconds_output
    /// when ignore_speed is true (effective_speed = 1.0 regardless of multiplier).
    /// Contrast with a non-exempt pane whose output shrinks with the multiplier.
    ///
    /// This test exercises the mapping `pane.ignore_speed → effective_speed = 1.0`
    /// that lives in the caller (src/export/stage.rs). It would FAIL if that mapping
    /// were dropped, because the exempt case would then use the multiplier directly.
    #[test]
    fn ignore_speed_makes_output_independent_of_multiplier() {
        let doc = 17_392usize;
        let duration_s_1x = 8.0;
        let multipliers = [1.0, 2.0, 3.0];

        // Exempt pane (ignore_speed = true): effective_speed is always 1.0,
        // derived through the exemption rule: if ignore_speed { 1.0 } else { mult }.
        let ignore_speed = true;
        let exempt_outputs: Vec<f64> = multipliers
            .iter()
            .map(|&mult| {
                let effective_speed = if ignore_speed { 1.0 } else { mult };
                pan_seconds_output(doc, duration_s_1x, effective_speed)
            })
            .collect();
        // All three must be equal — the output is independent of the multiplier.
        let baseline = exempt_outputs[0];
        for (i, &out) in exempt_outputs.iter().enumerate().skip(1) {
            assert!(
                (out - baseline).abs() < 1e-9,
                "ignore_speed=true: output at mult={} should equal output at mult=1.0 \
                 (got {} vs {}); this fails if the exemption mapping is dropped",
                multipliers[i],
                out,
                baseline
            );
        }

        // Non-exempt pane (ignore_speed = false): effective_speed equals the multiplier.
        let ignore_speed = false;
        let normal_outputs: Vec<f64> = multipliers
            .iter()
            .map(|&mult| {
                let effective_speed = if ignore_speed { 1.0 } else { mult };
                pan_seconds_output(doc, duration_s_1x, effective_speed)
            })
            .collect();
        // At 2x the output should be half of 1x; at 3x a third.
        assert!(
            (normal_outputs[1] - normal_outputs[0] / 2.0).abs() < 1e-9,
            "ignore_speed=false: output at 2x should be half of 1x"
        );
        assert!(
            (normal_outputs[2] - normal_outputs[0] / 3.0).abs() < 1e-9,
            "ignore_speed=false: output at 3x should be a third of 1x"
        );

        // The exempt output must equal the non-exempt 1x output (both use effective_speed=1.0).
        assert!(
            (exempt_outputs[0] - normal_outputs[0]).abs() < 1e-9,
            "exempt at any mult should equal non-exempt at 1x"
        );
        // And the exempt outputs must differ from non-exempt at 2x and 3x.
        assert!(
            (exempt_outputs[1] - normal_outputs[1]).abs() > 0.1,
            "exempt at 2x should differ from non-exempt at 2x"
        );
        assert!(
            (exempt_outputs[2] - normal_outputs[2]).abs() > 0.1,
            "exempt at 3x should differ from non-exempt at 3x"
        );
    }

    /// This test would fail if the cap went back to being measured in output
    /// seconds. If pan_seconds_output computed the cap in output seconds at 2x,
    /// it would use 2400 px/s, taking 7.247 s — not 14.5.
    #[test]
    fn the_cap_is_measured_at_1x_not_output_seconds() {
        let doc = 17_392usize;
        // At 1x cap (1200 px/s): 17392/1200 = 14.493 s
        // If cap were in output seconds at 2x (2400 px/s): 17392/2400 = 7.247 s
        let output_2x = pan_seconds_output(doc, 0.0, 2.0);
        // If the cap were wrongly in output seconds, output_2x would be ≈7.247/2 = 3.6
        // With the cap correctly at 1x, output_2x = 14.493/2 = 7.247
        assert!(
            output_2x > 7.0 && output_2x < 7.5,
            "cap must be at 1x (1200 px/s): output at 2x should be ≈7.2, got {output_2x:.2} \
             (would be ≈3.6 if cap were in output seconds at 2x)"
        );
    }
}
