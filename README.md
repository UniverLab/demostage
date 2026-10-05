```
     █████                                    █████████   █████                               
    ░░███                                    ███░░░░░███ ░░███                                
  ███████   ██████  █████████████    ██████ ░███    ░░░  ███████    ██████    ███████  ██████ 
 ███░░███  ███░░███░░███░░███░░███  ███░░███░░█████████ ░░░███░    ░░░░░███  ███░░███ ███░░███
░███ ░███ ░███████  ░███ ░███ ░███ ░███ ░███ ░░░░░░░░███  ░███      ███████ ░███ ░███░███████ 
░███ ░███ ░███░░░   ░███ ░███ ░███ ░███ ░███ ███    ░███  ░███ ███ ███░░███ ░███ ░███░███░░░  
░░████████░░██████  █████░███ █████░░██████ ░░█████████   ░░█████ ░░████████░░███████░░██████ 
 ░░░░░░░░  ░░░░░░  ░░░░░ ░░░ ░░░░░  ░░░░░░   ░░░░░░░░░     ░░░░░   ░░░░░░░░  ░░░░░███ ░░░░░░  
                                                                             ███ ░███         
                                                                            ░░██████          
                                                                             ░░░░░░           
```

<p align="center">
  <a href="https://github.com/UniverLab/demo-stage/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/UniverLab/demo-stage/ci.yml?branch=main&style=for-the-badge&label=CI" alt="CI"/></a>
  <img src="https://img.shields.io/badge/Status-Alpha-E6C84A?style=for-the-badge" alt="Status"/>
  <img src="https://img.shields.io/badge/Demos-as%20Code-8e8ff0?style=for-the-badge" alt="Demos as Code"/>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-2E8B57?style=for-the-badge" alt="License"/></a>
</p>

**DemoStage** turns terminal demos into reproducible engineering — *Demos as Code*.
Instead of capturing brittle pixels, it records a session as **events**, normalizes
human imperfections into a clean **score** (`demo.toml`), and compiles that score to
several formats. One demo, version-controlled, re-runnable, diffable.

Project home: [univerlab.org/demostage](https://univerlab.org/demostage/)

<p align="center">
  <img src="demo/dist/demo.svg" alt="A DemoStage tour: a score recorded in a real PTY, exported to gif and svg, checked by doctor" width="800"/>
</p>

<p align="center"><em>DemoStage demoing itself: write a score, record it in a real PTY, export gif and svg from one recording. This animation is the svg export.</em></p>

---

## Features

- **🎬 Event-based capture** — Records keystrokes, commands, and window events as structured data, not pixels.
- **✏️ Editable score** — The `demo.toml` score is human-readable TOML you can prune, reorder, or retime by hand.
- **🧹 Backspace pruning** — Typos corrected while recording never reach the demo.
- **⌨️ Humanized typing** — Seeded, bounded jitter so playback reads like a fast human, not robotic paste.
- **🔇 Idle trimming** — Dead time between commands is clamped; trailing idle is dropped.
- **🔒 Clean prompt** — Capture forces a realistic generic prompt (`user@demo:~$`), so demos never leak your real `user@host`.
- **🏝️ Isolated sandbox** — Commands execute in a temporary directory by default, keeping your workspace clean. Use `--here` to opt out.
- **🎬 Multi-scene** — Composite browser scenes (repo pages, PDFs, localhost) beside the terminal.
- **🔤 Font selection** — Choose from 5 bundled monospace fonts during capture (DejaVu Sans Mono default, IBM Plex Mono, JetBrains Mono, Liberation Mono, Ubuntu Mono).
- **🖼️ Canvas & frame rate** — Pick the canvas at capture by **aspect ratio** (`16:9`, `9:16`, `4:3`, `1:1`) × **quality** (FullHD 1080p or HD 720p), plus a **frame rate** of 15/24/30 fps (`--aspect`, `--quality`, `--fps`, or the wizard). A custom `WxH` or `auto` is still available via `--resolution`.
- **⚡ Live control commands** — Run `demo stop` to finish, `demo focus <source>` to switch the view (one or two sources, split or stacked), or `demo open <url>` to reveal an ad-hoc browser page mid-session — from the captured shell or another terminal.
- **✂️ Bulk timeline editing** — `demo edit` marks several steps at once (space) and applies one action to all: delete, convert waits, find & replace.
- **📦 Single binary** — Pure Rust, no Node.js, no Python, no runtime dependencies.

---

## Install

```sh
curl -fsSL https://install.univerlab.org/demostage | sh
cargo install demo-stage        # from crates.io
cargo install --path .          # from a checkout
```

### Uninstall

```bash
rm -f ~/.local/bin/demo-stage
```

---

## Quick Start

```sh
demo capture                      # run the demo, then `demo stop` (or exit / Ctrl-D) to finish
                                  # → demo.toml (editable score) + demo.rec (faithful take)
demo record                       # re-run demo.toml for a clean, humanized demo.rec
demo export                       # no args → the defaults (gif, mp4 — svg is opt-in)
demo export gif,mp4 --speed 2x        # several at once, retimed 2× faster
```

## Documentation

See [`docs/`](docs/) — overview, the `demo.toml` DSL, the commands, the normalizer,
and the export targets. (Published on the UniverLab site at `/demostage/docs`.)

---

## The loop

```
demo capture  ──>  demo.toml ──> demo record ──> demo.rec ──> demo export ──> dist/
                  (the editable score)  (re-run clean)     (render — never executes)
                       └──────────────────────────────────────────┐
                       demo.rec (faithful)  ──>  demo export --force ┘  (no re-run)
```

**Capture** runs the demo live, writing an editable `demo.toml` score and a
faithful `demo.rec` of the real session; it forces a clean prompt so your real
`user@host` never shows. Browser **sources** (repo pages, docs, localhost) are
configured in the capture wizard and revealed at the live moment with
`demo focus <source>`.

**Record** re-executes `demo.toml` for a clean, humanized recording — the default
render path. **Export** is pure playback: it renders a recording and never executes
anything.

**Can't re-run it?** Interactive tools, flows needing secrets, or demos that create
real resources would be repeated or desynced by `record` — so skip it and render
the faithful capture directly with `demo export --force` (typing isn't re-humanized,
but nothing re-executes). Export refuses a faithful capture without `--force`, keeping
the clean path the default.

---

## Commands

### Top-level

| Command | Description |
|---|---|
| `demo capture` | Live capture: record the session, auto-normalize into a clean score and faithful `.rec` |
| `demo record` | Re-execute `demo.toml` cleanly, producing a humanized recording |
| `demo export` | Pure playback: render to gif or mp4 — or the opt-in `svg` animation (terminal-only; `--at` for a poster) (no re-execution, ffmpeg/chromium auto-provisioned) |
| `demo edit` | Edit the timeline interactively — mark several steps (space) for bulk delete/convert/replace |
| `demo doctor` | Verify the environment and install missing tools |
| `demo update` | Check for / install a newer stable release — always asks first (default: no) |

### demo update

```sh
demo update            # look for a newer stable release, then ask (default: NO)
demo update --check    # report only — exit 0 = current, exit 1 = update available, exit 2 = the check could not be completed
demo update --yes      # install without the prompt (scriptable)
```

Never runs on its own: DemoStage has **no background update check** — the only
network `demo` opens is this explicit command, and it fails loudly when it
can't reach GitHub. It picks the newest *stable* GitHub release (drafts and
prereleases excluded), compares it to your version with a semver compare, and
**asks before doing anything** (the prompt defaults to **no**).

- **Your state survives.** Only the running binary is replaced — `demo.toml`
  scores, recordings (`.rec`), the raw macro and the capture sources are never
  touched.
- **Checksums.** The downloaded archive is verified against the release's
  `SHA256SUMS.txt` when one ships; a mismatch aborts, a missing file (older
  releases) skips.
- **Cargo installs are refused.** A `demo` under `~/.cargo/bin` is owned by
  cargo: run `cargo install --force demo-stage`.
- **Exit codes.** `exit 0` = up to date (update installed, prompt declined, or cargo
  refusal), `exit 1` = an update is available (`--check`), `exit 2` = the check could not
  be completed (network, DNS, TLS, HTTP ≥ 400 or an unparsable response — the cause is
  the single line on stderr). Plain `demo update` exits `2` for the same failures.
- **No published asset? No guessing.** Windows and linux/arm64 have no
  `tar.gz` in the release matrix, so `update` refuses instead of inventing a
  name.

### Live control (during a capture)

Run these while a `demo capture` is in progress — from the captured shell itself,
or from **another terminal in the same directory** (handy when a full-screen TUI
owns the captured terminal). They signal the running recorder; their own echo and
wizards are kept out of the finished demo.

| Command | Description |
|---|---|
| `demo stop` | End the capture (also: type `exit` or press Ctrl-D) |
| `demo focus <source> [<source2>]` | Switch the view to one or two configured sources (`demo focus main docs`); `--vertical`, `--hold`, `--scroll`, `--when`, `--after` (no source → a picker) |
| `demo open <url>` | Reveal an ad-hoc browser page not pre-configured as a source — same flags plus `--split`/`--view` (no URL → a wizard) |

---

## What makes it different

- **Backspace pruning** — typos corrected while recording never reach the demo.
- **Humanized typing** — a seeded, bounded jitter so playback reads like a fast
  human, not a robotic paste (reproducible with `[typing].seed`).
- **Idle trimming** — dead time between commands is clamped; the trailing idle that
  stops the recording is dropped.
- **Clean prompt** — capture forces a realistic generic prompt (`user@demo:~$`),
  so demos never leak your real `user@host` (`--keep-prompt`/`--prompt` to override).

---

## Export targets

| Target | Output | Best for | Needs |
|---|---|---|---|
| `gif`  | animated GIF (rasterized) | READMEs, chat, anywhere `<img>` works | — (pure Rust, embedded font) |
| `mp4`  | H.264 video | landings / the web (`<video>`) | ffmpeg — **auto-fetched on first use** |
| `svg`  | animated SVG (vector text), terminal-only; `--at` for a poster | crisp README embeds (`<img>`), print, docs — **opt-in** | — (pure Rust) |
| browser panes (PDF/web) | composited into gif/mp4 | a scene beside the terminal | Chromium — **auto-fetched on first use** |

`gif` works fully offline. `mp4` and multi-scene **browser panes** provision
their tool **tectonic-style** — the first export downloads a managed ffmpeg /
Chromium into a cache (a system install is used if present). Run **`demo doctor`**
to check these and get platform-specific fixes (`--fix` installs them on apt-based
Linux; it also flags the snap Chromium, which can't be driven headless).

### Embed the animated SVG

For a terminal-only demo, `demo export svg` writes `dist/<name>.svg` — an
animated, terminal-only vector version of the timeline (no scripts, so it plays
inside a README `<img>`):

```md
![demo](dist/demo.svg)
```

```html
<img src="dist/demo.svg" alt="demo">
```

The SVG embeds its terminal font (subset to the glyphs on screen) as an inline @font-face, so it renders identically inside a README `<img>`.

Need one still instead (`--at 12.5` writes `dist/<name>-at-12.5.svg`), or a
multi-pane demo (animated SVG refuses those — use gif/mp4, or `--at` for a
poster)? See [`docs/export-targets.md`](docs/export-targets.md).

---

## Development

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

---

## Contributing

`docs/` is published at univerlab.org/demostage/docs and holds user documentation only; design records are kept outside the repository.

---

## License

MIT — see [`LICENSE`](LICENSE). The embedded font has its own permissive license,
see [`assets/FONT-LICENSE.md`](assets/FONT-LICENSE.md).

---

An experiment of [UniverLab](https://github.com/UniverLab) — an open computational laboratory.
Made with ❤️ by [JheisonMB](https://github.com/JheisonMB)
