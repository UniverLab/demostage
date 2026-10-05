//! SVG targets: the animated vector export (terminal-only demos) and the
//! static `--at <seconds>` poster. No scripts, no external references, no
//! `<foreignObject>` — the animated document renders in a README `<img>`.

mod animated;
mod blocks;
mod font;
mod paint;
mod poster;
mod timing;
mod writer;

#[cfg(test)]
mod tests;

pub use poster::{encode, frame_index, write_svg, PosterFrame};
pub use writer::write_animated;

use crate::model::{Pane, PaneKind, Score};

/// Why the animated target refuses this score, or `None` when the animated
/// path applies (a single fullscreen terminal). Only consulted for staged
/// scores without `--at`: anything composited has no single cell grid to walk.
pub fn animated_refusal(score: &Score) -> Option<String> {
    if is_single_terminal(score) {
        return None;
    }
    let mut earliest: Option<(&Pane, f64)> = None;
    for pane in &score.layout.panes {
        if pane.kind != PaneKind::Browser {
            continue;
        }
        let time = pane.reveal_at.unwrap_or(0.0);
        let is_earlier = earliest.is_none_or(|(_, best)| time < best);
        if is_earlier {
            earliest = Some((pane, time));
        }
    }
    match earliest {
        Some((pane, time)) => Some(refusal_message(pane_kind_label(pane.url.as_deref()), time)),
        None => Some(refusal_message("terminal", 0.0)),
    }
}

/// One fullscreen terminal and nothing else: the animated path's fast lane.
fn is_single_terminal(score: &Score) -> bool {
    if score.layout.panes.len() != 1 {
        return false;
    }
    let pane = &score.layout.panes[0];
    pane.kind == PaneKind::Terminal
        && pane.x == 0
        && pane.y == 0
        && pane.width == score.layout.width
        && pane.height == score.layout.height
}

/// `PDF` for documents, `image` for pictures, `browser` for live pages.
fn pane_kind_label(url: Option<&str>) -> &'static str {
    let lower = url.unwrap_or("").to_ascii_lowercase();
    let base = lower.split(['?', '#']).next().unwrap_or("");
    if base.ends_with(".pdf") {
        "PDF"
    } else if [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".tif", ".svg",
    ]
    .iter()
    .any(|ext| base.ends_with(ext))
    {
        "image"
    } else {
        "browser"
    }
}

fn refusal_message(kind: &str, time: f64) -> String {
    let article = if kind
        .chars()
        .next()
        .is_some_and(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u'))
    {
        "an"
    } else {
        "a"
    };
    format!(
        "animated svg supports terminal-only demos; this score shows {article} {kind} pane at {time:.1}s \
         — use gif/mp4, or --at <s> for a poster"
    )
}
