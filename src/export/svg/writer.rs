//! The animated target's entry point: walk a replay once, fold it into scenes
//! and holds, and write the document [`render_document`] assembles.
//!
//! Split from the assembler so the timeline walk (the only part that touches a
//! [`FrameSource`]) and the pure string building stay independently readable.

use std::path::Path;

use super::animated::{fold_scenes, render_document, scene_of, DocInput, Geom, Scene};
use super::font::{collect_animated_glyphs, embed_for};
use crate::error::{Error, Result};
use crate::export::raster::{FrameSource, TextFrame};
use crate::export::run::Recording;
use crate::model::Score;

/// The canvas geometry and background every frame of a replay shares, taken
/// from its first emitted frame.
#[derive(Clone, Copy)]
struct FrameDefaults {
    geom: Geom,
    default_bg: [u8; 3],
}

impl Default for FrameDefaults {
    fn default() -> Self {
        FrameDefaults {
            geom: Geom {
                cw: 10,
                ch: 20,
                px: 16.0,
            },
            default_bg: [11, 15, 20],
        }
    }
}

impl FrameDefaults {
    fn of(tf: &TextFrame) -> Self {
        FrameDefaults {
            geom: Geom {
                cw: tf.cell_w,
                ch: tf.cell_h,
                px: tf.px,
            },
            default_bg: tf.default_bg,
        }
    }
}

/// Walk the whole replay once (allocation only — nothing rasterized), folding
/// each grid frame into its scene and keeping the shared geometry from the
/// first frame.
fn walk_scenes(source: &mut FrameSource<'_>) -> (Vec<Scene>, FrameDefaults) {
    let mut scenes = Vec::new();
    let mut defaults = FrameDefaults::default();
    // Bounded: the source yields exactly `n_frames` frames, so walk them by
    // count — the walk ends on its own whatever the per-frame result is.
    let n = source.n_frames();
    for _ in 0..n {
        let Some(tf) = source.next_text_frame() else {
            break;
        };
        if scenes.is_empty() {
            defaults = FrameDefaults::of(&tf);
        }
        scenes.push(scene_of(&tf));
    }
    (scenes, defaults)
}

/// Walk the replay, fold it into scenes + holds, and write the animated SVG.
pub fn write_animated(path: &Path, rec: &Recording, score: &Score) -> Result<()> {
    let mut source = FrameSource::new(rec, score)?;
    let (w, h) = source.dims();
    let fps = score.layout.fps.max(1);
    let (scenes, defaults) = walk_scenes(&mut source);
    if scenes.is_empty() {
        return Err(Error::Export("svg: no frames were emitted".to_string()));
    }
    let final_tf = source.text_frame();
    let (unique, holds) = fold_scenes(scenes);
    let (font_family, font_face) =
        embed_for(&collect_animated_glyphs(&unique), &final_tf.font_family)?;
    let input = DocInput {
        unique: &unique,
        holds: &holds,
        // The frames actually walked, not `n_frames()`: the holds index real
        // frames, so the percentages that place every state on the timeline
        // must be scaled by however many the walk produced.
        n_frames: holds.last().map_or(0, |h| h.end),
        fps,
        w,
        h,
        geom: defaults.geom,
        default_bg: defaults.default_bg,
        final_tf: &final_tf,
        font_family,
        font_face,
    };
    let doc = render_document(&input);
    std::fs::write(path, &doc).map_err(|e| Error::io(path, e))?;
    eprintln!(
        "demo: animated svg: {} states, {:.1} KB",
        unique.len(),
        doc.len() as f64 / 1024.0
    );
    Ok(())
}
