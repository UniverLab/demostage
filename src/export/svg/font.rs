//! The embedded terminal font for the SVG targets: one `@font-face`
//! (`'ds-term'`) subset with `fontcull` to the glyphs on screen, so a README
//! `<img>` renders the same shapes the GIF shows. Only bundled assets are
//! used — never a network fetch.

use std::collections::{BTreeSet, HashSet};

use super::animated::Scene;
use super::blocks::is_block;
use super::paint::{escape_attr, is_braille, merge_runs};
use super::poster::base64;
use crate::error::{Error, Result};
use crate::export::raster::TextFrame;
use crate::fonts;

/// The embedded family's name: first in every `<text>` font stack.
pub const EMBED_FAMILY: &str = "ds-term";

/// The `<text>` font stack: the embedded face first, then the score's font
/// (fallback for glyphs the bundled font lacks), then generic monospace.
pub fn text_family(score_font: &str) -> String {
    format!("'{EMBED_FAMILY}', '{}', monospace", escape_attr(score_font))
}

/// A character the subset must shape: printable text the font can supply.
/// Controls draw nothing, braille rides as `<circle>` dots and block elements
/// as `<rect>` geometry — none of them ever reach a `<text>` node, so shaping
/// them in the face would only bloat it.
fn keep_char(ch: char) -> bool {
    !ch.is_control() && !is_braille(ch) && !is_block(ch)
}

/// The distinct printable characters of the animated timeline's text runs,
/// plus space (word gaps ride inside runs via `textLength`, so the face must
/// shape it even when no run holds one).
pub fn collect_animated_glyphs(unique: &[Scene]) -> BTreeSet<char> {
    let mut out = BTreeSet::new();
    for scene in unique {
        for item in &scene.text {
            out.extend(item.text.chars().filter(|c| keep_char(*c)));
        }
    }
    out.insert(' ');
    out
}

/// The distinct printable characters of one poster frame, plus space.
pub fn collect_poster_glyphs(tf: &TextFrame) -> BTreeSet<char> {
    let mut out = BTreeSet::new();
    for row in 0..tf.rows {
        for run in merge_runs(tf, row) {
            out.extend(run.text.chars().filter(|c| keep_char(*c)));
        }
    }
    out.insert(' ');
    out
}

/// The bundled bytes for `name` (the default font when unknown): the same
/// bytes the raster path draws with, so the subset covers what the GIF shows.
pub fn bytes_for(name: &str) -> &'static [u8] {
    fonts::bytes(name)
}

/// Subset `font_bytes` to `chars`, leaving out what the font lacks (those
/// keep the viewer fallback). Space is always kept.
pub fn subset_to_chars(font_bytes: &[u8], chars: &BTreeSet<char>) -> Result<Vec<u8>> {
    let original = fontdue::Font::from_bytes(font_bytes, fontdue::FontSettings::default())
        .map_err(|e| Error::Export(format!("svg font: bundled font failed: {e}")))?;
    let mut wanted: HashSet<char> = HashSet::new();
    for c in chars {
        if original.has_glyph(*c) {
            wanted.insert(*c);
        }
    }
    wanted.insert(' ');
    fontcull::subset_font_data(font_bytes, &wanted, &[])
        .map_err(|e| Error::Export(format!("svg font: subset failed: {e}")))
}

/// One `@font-face` rule for the subset, a single line for the `<style>`.
pub fn font_face_css(subset: &[u8]) -> String {
    format!(
        "@font-face{{font-family:'{EMBED_FAMILY}';\
         src:url(data:font/ttf;base64,{}) format('truetype');\
         font-weight:normal;font-style:normal}}",
        base64(subset)
    )
}

/// The `(family, face_rule)` pair for `chars` in the score's font.
pub fn embed_for(chars: &BTreeSet<char>, font_name: &str) -> Result<(String, String)> {
    let subset = subset_to_chars(bytes_for(font_name), chars)?;
    Ok((text_family(font_name), font_face_css(&subset)))
}

/// The subset bytes behind a face rule's `data:` URI. Test seam: decoding
/// has no production use, so it lives behind `cfg(test)`.
#[cfg(test)]
pub(crate) fn subset_bytes(face: &str) -> Vec<u8> {
    let payload = face.split("base64,").nth(1).expect("no data URI");
    decode_b64(payload)
}

/// Test-only base64 inverse (the crate hand-rolls the encoder, so the tests
/// hand-roll the decoder rather than adding a dependency).
#[cfg(test)]
fn decode_b64(s: &str) -> Vec<u8> {
    let mut vals = [0u8; 256];
    for (i, c) in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .bytes()
        .enumerate()
    {
        vals[c as usize] = i as u8;
    }
    let digits: Vec<u8> = s
        .bytes()
        .filter(|b| *b != b'=')
        .map(|b| vals[b as usize])
        .collect();
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    for quad in digits.chunks(4) {
        let n = (u32::from(quad[0]) << 18)
            | (u32::from(*quad.get(1).unwrap_or(&0)) << 12)
            | (u32::from(*quad.get(2).unwrap_or(&0)) << 6)
            | u32::from(*quad.get(3).unwrap_or(&0));
        out.push((n >> 16) as u8);
        if quad.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if quad.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_family_names_the_embed_first() {
        assert_eq!(
            text_family("IBM Plex Mono"),
            "'ds-term', 'IBM Plex Mono', monospace"
        );
        let quoted = text_family("a\"b");
        assert!(quoted.starts_with("'ds-term',"), "embed first: {quoted}");
        assert!(quoted.contains("&quot;"), "quote escaped: {quoted}");
    }

    #[test]
    fn animated_collection_skips_controls_and_braille_but_keeps_space() {
        let scene = Scene {
            text: vec![crate::export::svg::animated::TextItem {
                row: 0,
                start: 0,
                text: "a\nb\u{28ff}c".into(),
                fg: [0, 0, 0],
                bold: false,
            }],
            ..Default::default()
        };
        let chars = collect_animated_glyphs(&[scene]);
        assert!(chars.contains(&'a'));
        assert!(chars.contains(&'b'));
        assert!(chars.contains(&'c'));
        assert!(chars.contains(&' '), "space always kept");
        assert!(!chars.contains(&'\n'), "controls skipped");
        assert!(!chars.contains(&'\u{28ff}'), "braille skipped");
    }

    #[test]
    fn subset_round_trips_with_exact_coverage() {
        let chars: BTreeSet<char> = ['a', 'Z', ' ', '\u{2591}', '\u{2500}'].into();
        let subset = subset_to_chars(bytes_for("IBM Plex Mono"), &chars).unwrap();
        let face = font_face_css(&subset);
        assert!(face.contains("data:font/ttf;base64,"), "TTF URI: {face}");
        let reparsed = subset_bytes(&face);
        let font = fontdue::Font::from_bytes(reparsed, fontdue::FontSettings::default()).unwrap();
        for c in ['a', 'Z', ' ', '\u{2591}', '\u{2500}'] {
            assert!(font.has_glyph(c), "subset lost {c:?}");
        }
        for mapped in font.chars().keys() {
            assert!(chars.contains(mapped), "extra glyph {mapped:?} in subset");
        }
    }

    #[test]
    fn characters_the_font_lacks_stay_out_of_the_subset() {
        let chars: BTreeSet<char> = ['a', '\u{1f680}'].into();
        assert!(
            !bytes_for("IBM Plex Mono").is_empty(),
            "fixture font must load"
        );
        let original =
            fontdue::Font::from_bytes(bytes_for("IBM Plex Mono"), fontdue::FontSettings::default())
                .unwrap();
        assert!(!original.has_glyph('\u{1f680}'), "fixture assumption");
        let subset = subset_to_chars(bytes_for("IBM Plex Mono"), &chars).unwrap();
        let font = fontdue::Font::from_bytes(subset, fontdue::FontSettings::default()).unwrap();
        assert!(font.has_glyph('a'));
        assert!(!font.has_glyph('\u{1f680}'), "missing char must stay out");
        assert!(font.has_glyph(' '), "space kept even so");
    }
}
