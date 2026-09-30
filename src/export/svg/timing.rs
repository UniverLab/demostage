//! When an element is on screen, and the CSS that says so.
//!
//! The animated document is pure show/hide: every element carries a
//! `@keyframes` rule that holds `opacity:1` exactly over the frame windows it
//! belongs to and `0` everywhere else, driven by `steps(1,end)` so each state
//! keeps its real duration and jumps at the boundary. This module owns that
//! arithmetic — frame windows to percentages, percentages to a rule, a rule to
//! a `style` attribute — and nothing about what the elements draw.

/// Fresh `@keyframes` names (`k0`, `k1`, …) in emission order.
pub(crate) struct KeyGen {
    next: usize,
}

impl KeyGen {
    pub(crate) fn new() -> Self {
        KeyGen { next: 0 }
    }

    pub(crate) fn fresh(&mut self) -> String {
        let key = format!("k{}", self.next);
        self.next += 1;
        key
    }
}

/// Total timeline seconds for `n` frames at `fps`.
pub(crate) fn duration_secs(n_frames: usize, fps: u32) -> f64 {
    n_frames as f64 / fps.max(1) as f64
}

/// One `@keyframes` rule showing the element exactly over `windows`
/// (`(start_incl, end_excl)` frames of `total`), hidden elsewhere.
pub(crate) fn keyframes_rule(name: &str, windows: &[(usize, usize)], total: usize) -> String {
    let mut stops: Vec<(f64, u8)> = vec![(0.0, 0)];
    for (start, end) in windows {
        push_stop(&mut stops, pct_of(*start, total), 1);
        push_stop(&mut stops, pct_of(*end, total), 0);
    }
    push_stop(&mut stops, 100.0, 0);
    let body: Vec<String> = stops
        .iter()
        .map(|(pct, opacity)| format!("{}%{{opacity:{opacity}}}", trim_num(*pct)))
        .collect();
    format!("@keyframes {name}{{{}}}", body.join(""))
}

/// Emit a rule for `frames` unless it covers the whole timeline statically;
/// returns the key, or `""` for a plain static element.
pub(crate) fn item_rule(
    keys: &mut KeyGen,
    rules: &mut Vec<String>,
    frames: &[(usize, usize)],
    total: usize,
) -> String {
    if frames.len() == 1 && frames[0] == (0, total) {
        return String::new();
    }
    let key = keys.fresh();
    rules.push(keyframes_rule(&key, frames, total));
    key
}

/// The `style` attribute animating one element, or `""` for a static one. The
/// inline `opacity:0` is the resting state, so a viewer that ignores the
/// animation shows the last frame's layers rather than every state at once.
pub(crate) fn style_for(key: &str, total_secs: f64) -> String {
    if key.is_empty() {
        String::new()
    } else {
        format!(" style=\"animation:{key} {total_secs}s steps(1,end) infinite;opacity:0\"")
    }
}

/// Raw percentage value for one frame boundary.
fn pct_of(frame: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    frame as f64 / total as f64 * 100.0
}

/// Four decimals, trimmed: deterministic and strictly orderable.
fn trim_num(value: f64) -> String {
    let text = format!("{value:.4}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() {
        "0".to_string()
    } else {
        text.to_string()
    }
}

/// Append a stop, keeping percentages strictly increasing so keyframes never
/// collide after four-decimal rounding.
///
/// A stop on the boundary that is already there MERGES into it instead of being
/// nudged past it. That matters twice: a window starting at frame 0 must read
/// `0%{opacity:1}` — nudging it would leave the first state invisible for a
/// rounding step of every loop — and two windows that share a boundary (a run
/// that survives into a hold it reappears in) must stay one unbroken visible
/// stretch instead of blinking for that same sliver.
fn push_stop(stops: &mut Vec<(f64, u8)>, pct: f64, opacity: u8) {
    match stops.last() {
        Some((last, _)) if (pct - *last).abs() < 1e-9 => merge_stop(stops, opacity),
        // Strictly earlier: it would collide with the stop already there, so
        // nudge forward by the smallest representable step.
        Some((last, _)) if pct < *last => stops.push(((*last + 0.0001).min(100.0), opacity)),
        _ => stops.push((pct.min(100.0), opacity)),
    }
}

/// Fold a stop that landed on the previous stop's boundary into the list. When
/// the previous stop already carries this opacity the boundary is redundant —
/// drop it, and the value simply holds across.
fn merge_stop(stops: &mut Vec<(f64, u8)>, opacity: u8) {
    let redundant = stops.len() > 1 && stops[stops.len() - 2].1 == opacity;
    if redundant {
        stops.pop();
    } else if let Some(last) = stops.last_mut() {
        last.1 = opacity;
    }
}
