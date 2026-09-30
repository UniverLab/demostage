---
title: Export targets
description: gif is pure-Rust and offline; mp4 needs ffmpeg; browser panes need chromium.
order: 7
---

# Export targets

`demo export [fmt[,fmt…]] [recording]` renders a recording to **`gif`** and/or
**`mp4`**. `gif` is pure-Rust and offline; `mp4` and multi-scene **browser panes**
auto-provision their tool (ffmpeg / Chromium) on first use. Pass both at once
(`demo export gif,mp4`), **omit the format to build them all** (`demo export`),
and use `--speed 2x` (or `3x`, `0.5x`) to retime the whole demo.

There is also a third, **opt-in** target: **`svg`**, a static poster drawn as
vector text. Naming no format (or naming `all`) still builds **only `gif` +
`mp4`** — `svg` is built when you ask for it by name, alone or combined
(`demo export svg`, `demo export gif,svg`).

| Target | Output | Best for | External tool |
|---|---|---|---|
| `gif`  | animated GIF | READMEs, chat, GitHub — anywhere `<img>` works | — (pure Rust) |
| `mp4`  | H.264 video | landings / the web (`<video>`), CDN-friendly | ffmpeg — **auto-fetched** |
| `svg`  | animated SVG (vector text), terminal-only | README embeds (`<img>`), crisp at any size — **opt-in** | — (pure Rust) |
| browser panes | composited into gif/mp4 | a PDF / web scene beside the terminal | Chromium — **auto-fetched** |

> A text-based, framework-agnostic web player (a *DemoStagePlayer*, with crisp
> selectable text and no asciinema dependency) is planned as a separate piece —
> for now, embed `gif` (READMEs/chat) or `mp4` (`<video>` on a landing).

## gif

Pure-Rust rasterization. The captured output is replayed through a vt100 parser at
the score's `fps`, each frame is drawn with an **embedded monospace font**,
identical frames are deduped, and the result is encoded with the `gif` crate. No
ffmpeg. Covers printable ASCII and ANSI colors; exotic glyphs are skipped.

## mp4

Encoded with **ffmpeg**, which DemoStage provisions **tectonic-style**: if it
isn't on your `PATH`, the first `mp4` export notifies you and downloads a managed
static build into a cache, then reuses it. No manual install step. If the download
can't run (offline), you get a clear message and can install ffmpeg yourself.

## svg (opt-in)

An **animated** SVG of the whole timeline — for demos whose visible panes are
**terminal panes only**. Every distinct screen state becomes a `<g>` of merged
text runs, shown and hidden with CSS `@keyframes` using `steps()` timing so
each state holds for its real duration; the animation loops forever. To stay
small, a state identical to an earlier one reuses it (`<use href>`), and runs
that do not change between consecutive states are drawn once in a persistent
layer spanning their lifetime.

- **Text alignment without embedded fonts.** The stack is `ui-monospace,
  SFMono-Regular, Menlo, Consolas, "DejaVu Sans Mono", monospace`, and every
  run carries `textLength` + `lengthAdjust="spacingAndGlyphs"` computed from
  its cell count, so the grid holds with whatever monospace the viewer has.
- **`prefers-reduced-motion: reduce` shows the final state**, static.
- **README-safe:** no scripts, no external references, no `<foreignObject>` —
  everything inline. Embed it with `![demo](dist/demo.svg)` or
  `<img src="dist/demo.svg" alt="demo">`.
- **`--at <seconds>`** keeps producing the **static poster** of that frame
  (e.g. `demo export svg --at 12.5 demo.rec`), now written to
  `dist/<name>-at-12.5.svg` so posters never overwrite each other or the
  animated `dist/<name>.svg`. It defaults to the **last frame**, and a value
  past the end clamps to it. Ignored by `gif`/`mp4`.
- **`demo export` (no format / `all`) does not build it** — `svg` is opt-in and
  must be named: `demo export svg` or `demo export gif,svg`.
- **Braille cells** (`U+2800`–`U+28FF`, what tools like mapscii draw with) become
  procedural `<circle>` dots: viewer fonts generally lack those glyphs.
- **Multi-pane demos refuse the animated path**: `demo export svg` without
  `--at` fails with `animated svg supports terminal-only demos; this score
  shows a <kind> pane at <time>s — use gif/mp4, or --at <s> for a poster`.
  The poster path keeps its PNG fallback (one rasterized frame embedded as a
  base64 PNG — a composited canvas has no cell grid to draw from).

## browser panes (multi-scene)

A score can place a `browser` pane next to the terminal (the spec's *Stage
Matrix*) — e.g. a PDF viewer or a live web preview. When exporting such a score to
`gif`/`mp4`, the **stage** runs the terminal in a PTY, drives a headless
**Chromium** to capture the `url` (scrolling per the `scroll` steps), and
composites both panes onto the canvas frame by frame.

Chromium is provisioned the same tectonic-style way as ffmpeg: a system Chrome is
used if present, otherwise `headless_chrome` downloads a managed build on first
use. (Browser panes only appear on `gif`/`mp4` — there's no text target.)

**Reveal on focus:** a browser pane is blank until the timeline `focus`es it, then
appears — so you can `focus` it right after a server comes up or a PDF compiles,
and it "opens" at that exact moment. (The focus time is recorded during the run.)

**Capture order:** the terminal runs to completion first, then each browser pane
is captured. So a browser pane must point at something still available at capture
time — a persistent file (a PDF, a rendered `preview.svg`/`.png`) or a server the
terminal leaves running (don't stop it mid-score). A step like `caption` overlays
a label on the canvas to narrate the demo.
