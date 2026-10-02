//! Embedded font registry. Each bundled monospace font is included at compile
//! time and looked up by the `font_family` name stored in the score's layout.

use fontdue::{Font, FontSettings};

/// A bundled font: its human-readable name and raw bytes.
struct BundledFont {
    name: &'static str,
    bytes: &'static [u8],
}

/// All fonts embedded in the binary.
const BUNDLED: &[BundledFont] = &[
    BundledFont {
        name: "DejaVu Sans Mono",
        bytes: include_bytes!("../assets/DejaVuSansMono.ttf"),
    },
    BundledFont {
        name: "JetBrains Mono",
        bytes: include_bytes!("../assets/JetBrainsMono-Regular.ttf"),
    },
    BundledFont {
        name: "IBM Plex Mono",
        bytes: include_bytes!("../assets/IBMPlexMono.ttf"),
    },
    BundledFont {
        name: "Liberation Mono",
        bytes: include_bytes!("../assets/LiberationMono-Regular.ttf"),
    },
    BundledFont {
        name: "Ubuntu Mono",
        bytes: include_bytes!("../assets/UbuntuMono.ttf"),
    },
];

/// Wizard display strings (shown in the capture prompt picker).
pub const FONT_NAMES: &[&str] = &[
    "DejaVu Sans Mono   (best Unicode — MapSCII, box drawing)",
    "JetBrains Mono     (clean, modern, ligatures)",
    "IBM Plex Mono      (UniverLab landing font)",
    "Liberation Mono    (Courier New compatible)",
    "Ubuntu Mono        (compact, classic Ubuntu)",
];

/// Short key names stored in `layout.font_family`.
pub const FONT_KEYS: &[&str] = &[
    "DejaVu Sans Mono",
    "JetBrains Mono",
    "IBM Plex Mono",
    "Liberation Mono",
    "Ubuntu Mono",
];

/// Default font (first entry).
pub const DEFAULT_FONT: &str = "DejaVu Sans Mono";

/// Parse a font key from the wizard display string.
/// "DejaVu Sans Mono   (best Unicode …)" → "DejaVu Sans Mono"
pub fn parse_font_name(display: &str) -> &str {
    display.split("   ").next().unwrap_or(display).trim()
}

/// Raw bytes of a bundled font by its key name. Falls back to the default.
/// Mirrors [`load`]'s lookup so the SVG embedder subsets the same bytes the
/// raster path draws with.
pub fn bytes(name: &str) -> &'static [u8] {
    BUNDLED
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case(name))
        .map(|f| f.bytes)
        .unwrap_or(BUNDLED[0].bytes)
}

/// Load a font by its key name. Falls back to the default if unknown.
pub fn load(name: &str) -> Font {
    let bytes = BUNDLED
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case(name))
        .map(|f| f.bytes)
        .unwrap_or(BUNDLED[0].bytes);
    Font::from_bytes(bytes, FontSettings::default()).expect("bundled font failed to parse")
}

/// Load a font by wizard display string.
pub fn load_from_display(display: &str) -> Font {
    load(parse_font_name(display))
}

/// Load the monochrome emoji fallback font.
pub fn load_emoji() -> Font {
    const EMOJI_BYTES: &[u8] = include_bytes!("../assets/NotoEmoji-Regular.ttf");
    Font::from_bytes(EMOJI_BYTES, FontSettings::default())
        .expect("bundled emoji font failed to parse")
}

/// Load the last-resort fallback font (DejaVu Sans Mono).
pub fn load_last_resort() -> Font {
    const LAST_RESORT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSansMono.ttf");
    Font::from_bytes(LAST_RESORT_BYTES, FontSettings::default())
        .expect("bundled DejaVu font failed to parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_font_name_strips_description() {
        assert_eq!(
            parse_font_name("DejaVu Sans Mono   (best Unicode — MapSCII, box drawing)"),
            "DejaVu Sans Mono"
        );
        assert_eq!(
            parse_font_name("JetBrains Mono     (clean, modern, ligatures)"),
            "JetBrains Mono"
        );
    }

    #[test]
    fn parse_font_name_plain_name() {
        assert_eq!(parse_font_name("DejaVu Sans Mono"), "DejaVu Sans Mono");
    }

    #[test]
    fn parse_font_name_empty() {
        assert_eq!(parse_font_name(""), "");
    }

    #[test]
    fn font_constants_not_empty() {
        assert!(!FONT_NAMES.is_empty());
        assert!(!FONT_KEYS.is_empty());
        assert!(!DEFAULT_FONT.is_empty());
    }

    #[test]
    fn load_default_font_works() {
        let font = load(DEFAULT_FONT);
        let metrics = font.metrics('A', 16.0);
        assert!(metrics.width > 0);
    }

    #[test]
    fn load_unknown_font_falls_back_to_default() {
        let font = load("NonExistent Font Name");
        let metrics = font.metrics('A', 16.0);
        assert!(metrics.width > 0);
    }

    #[test]
    fn load_emoji_works() {
        let font = load_emoji();
        let metrics = font.metrics('\u{1F600}', 16.0);
        assert!(metrics.width > 0);
    }
}
