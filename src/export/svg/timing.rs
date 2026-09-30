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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_names_are_fresh_and_strictly_sequential() {
        let mut keys = KeyGen::new();
        assert_eq!(keys.fresh(), "k0");
        assert_eq!(keys.fresh(), "k1");
        assert_eq!(keys.fresh(), "k2");
    }

    #[test]
    fn duration_is_frames_divided_by_fps() {
        assert_eq!(duration_secs(10, 5), 2.0);
        assert_eq!(duration_secs(0, 5), 0.0);
        // fps 0 is raised to 1, so the duration is the frame count itself.
        assert_eq!(duration_secs(7, 0), 7.0);
    }

    #[test]
    fn pct_of_is_the_frame_boundary_as_a_percentage() {
        assert_eq!(pct_of(5, 10), 50.0);
        assert_eq!(pct_of(3, 10), 30.0);
        assert_eq!(pct_of(10, 10), 100.0);
        assert_eq!(pct_of(0, 10), 0.0);
        // A zero total has no timeline at all: the guard fires before division.
        assert_eq!(pct_of(4, 0), 0.0);
        assert_eq!(pct_of(0, 0), 0.0);
    }

    #[test]
    fn trim_num_rounds_to_four_decimals_and_drops_padding() {
        assert_eq!(trim_num(0.0), "0");
        assert_eq!(trim_num(30.0), "30");
        assert_eq!(trim_num(100.0), "100");
        assert_eq!(trim_num(66.6666666), "66.6667");
        assert_eq!(trim_num(1.5), "1.5");
    }

    #[test]
    fn a_window_lands_on_exactly_its_frame_percentages() {
        // Frames 3..6 of 10 → 30% on, 60% off, hidden at both ends.
        assert_eq!(
            keyframes_rule("k0", &[(3, 6)], 10),
            "@keyframes k0{0%{opacity:0}30%{opacity:1}60%{opacity:0}100%{opacity:0}}"
        );
        // Closing window: it must still read 100%{opacity:0} at the end.
        assert_eq!(
            keyframes_rule("k1", &[(7, 10)], 10),
            "@keyframes k1{0%{opacity:0}70%{opacity:1}100%{opacity:0}}"
        );
    }

    #[test]
    fn a_window_opening_at_frame_zero_merges_into_the_first_stop() {
        // Nudging the equal stop would hide the opening state for a rounding
        // step of every loop — 0% must read opacity:1 directly.
        assert_eq!(
            keyframes_rule("k0", &[(0, 3)], 10),
            "@keyframes k0{0%{opacity:1}30%{opacity:0}100%{opacity:0}}"
        );
    }

    #[test]
    fn windows_sharing_a_boundary_stay_visible_across_it() {
        // The shared 40% boundary merges into one continuous visible stretch —
        // no hidden 40% sliver — while the closing 100% stop stays put.
        assert_eq!(
            keyframes_rule("k0", &[(2, 4), (4, 6)], 10),
            "@keyframes k0{0%{opacity:0}20%{opacity:1}60%{opacity:0}100%{opacity:0}}"
        );
    }

    #[test]
    fn a_stop_that_goes_backwards_is_nudged_strictly_forward() {
        // end frame 2 lands at 20% after a start at 50%: it cannot collide or
        // run backwards, so it is nudged to 50.0001%.
        assert_eq!(
            keyframes_rule("k0", &[(5, 2)], 10),
            "@keyframes k0{0%{opacity:0}50%{opacity:1}50.0001%{opacity:0}100%{opacity:0}}"
        );
    }

    #[test]
    fn a_run_that_holds_into_its_own_reappearance_merges_to_one_stretch() {
        // The second window starts exactly where the first ends while both
        // are visible: one unbroken visible stretch, no 50% blink. This is
        // also the two-element-list merge case in merge_stop.
        assert_eq!(
            keyframes_rule("k0", &[(0, 5), (5, 7)], 10),
            "@keyframes k0{0%{opacity:1}70%{opacity:0}100%{opacity:0}}"
        );
    }

    #[test]
    fn item_rule_is_silent_only_for_a_static_full_timeline() {
        let mut keys = KeyGen::new();
        let mut rules: Vec<String> = Vec::new();

        // Covers the whole timeline statically: no key, no rule.
        assert_eq!(item_rule(&mut keys, &mut rules, &[(0, 4)], 4), "");
        assert!(rules.is_empty(), "a static element has no rule");

        // A partial window gets the first fresh key and pushes its rule.
        assert_eq!(item_rule(&mut keys, &mut rules, &[(0, 2)], 4), "k0");
        assert_eq!(
            rules,
            vec!["@keyframes k0{0%{opacity:1}50%{opacity:0}100%{opacity:0}}"]
        );

        // Two windows sharing a boundary still count as animated, and take
        // the next key.
        assert_eq!(item_rule(&mut keys, &mut rules, &[(0, 2), (2, 4)], 4), "k1");
        assert_eq!(rules[1], "@keyframes k1{0%{opacity:1}100%{opacity:0}}");
    }

    #[test]
    fn style_for_animates_a_keyed_element_and_stays_silent_otherwise() {
        assert_eq!(
            style_for("k0", 0.6),
            " style=\"animation:k0 0.6s steps(1,end) infinite;opacity:0\""
        );
        assert_eq!(style_for("", 1.0), "");
    }
}
