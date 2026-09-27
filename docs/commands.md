---
title: Commands
description: Reference for demo capture, record, export, edit, doctor, update and the live capture-control commands.
order: 5
---

# Commands

## `demo capture`

Capture a live interactive session and save it as a **recording** (`demo.rec`,
played back by `demo export`) plus the editable **score** (`demo.toml`, re-run by
`demo record`). Needs a real terminal.

```sh
demo capture [-r demo.rec] [-O demo.toml | --no-score] [--raw macro.raw.toml] [--no-normalize] [--prompt "<PS1>" | --keep-prompt] [--debug] [--idle-timeout-ms 0] [--shell /bin/bash] [--into demo.toml] [--font <name>] [--aspect <16:9|9:16|4:3|1:1> [--quality <fullhd|hd>]] [--fps <15|24|30>] | --resolution <preset|WxH|auto>]
```

- `-r, --rec` — where to write the recording (default `demo.rec`). It's a faithful
  take, so `demo export --force` renders it directly; the clean path is
  `demo record` (re-run the score) then `demo export`.
- `-O, --score <file>` — where to write the demo score (default `demo.toml`) — the
  "demo as code" source you can hand-edit or re-run with `demo record`.
- `--no-score` — leave only the recording (skip writing `demo.toml`).
- `--raw <file>` (`-o`) — **also** write the low-level raw macro, a debugging
  intermediate. Off by default.
- `--no-normalize` — skip the normalize pass, so no score is derived (the
  recording is faithful either way).
- `--prompt "<PS1>"` — force a clean prompt in the captured shell so the demo
  shows a tidy prompt instead of your real `user@host`. **On by default** with a
  built-in realistic prompt (`user@demo:~$`, green/blue); pass a value (bash `PS1`
  syntax) to customize. With neither `--prompt` nor `--keep-prompt`, a quick
  one-question wizard asks (clean / customize / keep yours) before recording.
- `--keep-prompt` — keep your shell's real prompt (don't force a clean one).
- `--debug` — write a timestamped diagnostic log next to the recording
  (`<rec>.debug.log`): every input/output chunk in escaped + hex form (secret
  keystrokes are logged only as a byte count, never their value) and why the
  capture stopped. Use it when a capture behaves oddly (e.g. a wizard mis-reads a
  keystroke).
- `--idle-timeout-ms` — auto-stop after this long with no terminal output.
  **Defaults to `0` (disabled)** so a pause to think never cuts the capture
  short; set a positive value for an unattended capture. Any trailing idle is
  trimmed when normalizing.
- `--shell` — shell to run (defaults to `$SHELL`).
- `--into` — splice this capture into a hand-authored stage score's timeline
  instead of building a fresh single-pane score (advanced); defaults `--score` to
  `demo.toml`.
- `--font` — font for the exported demo (`dejavu`, `jetbrains`, `ibm-plex`,
  `liberation`, `ubuntu`). Omit for a wizard prompt.
- `--aspect` — canvas aspect ratio: `16:9`, `9:16`, `4:3`, or `1:1`. Combined
  with `--quality` to pick the pixel resolution (e.g. `16:9` + `fullhd` →
  1920×1080; `16:9` + `hd` → 1280×720). `--quality` defaults to `fullhd`.
  Omit for a wizard prompt. Conflicts with `--resolution`.
- `--quality` — quality tier: `fullhd` (short side 1080 px) or `hd` (short side
  720 px). Needs `--aspect` (defaults to `16:9` if given alone). Conflicts with
  `--resolution`.
- `--fps` — frame rate of the exported gif/mp4: `15`, `24`, or `30`. Defaults
  to `15`. Omit for a wizard prompt.
- `--resolution` — the canvas as an explicit size, for the power user: a `WxH`
  pair (e.g. `1600x900`), `auto` (derive from the terminal size — the default),
  or a legacy preset (`landscape` 1920×1080, `portrait` 1080×1920, `square`
  1080×1080, `standard` 1280×720). Conflicts with `--aspect`/`--quality`; omit
  both for the aspect/quality wizard. On the faithful `.rec` the recorded
  terminal is composited centered on the canvas (never cropped); a `demo record`
  re-run sizes its terminal grid to the canvas.

**Ending a capture.** Run `demo stop` — inside the captured shell, or from another
terminal in the same directory. (`exit` / Ctrl-D still work; a positive
`--idle-timeout-ms` also stops after that long with no output.)

## Live capture-control commands

While a `demo capture` is running, drive it with these ordinary `demo` commands.
Run them **inside the captured shell**, or from **another terminal in the same
directory** — the latter works even while a full-screen TUI owns the captured
terminal. They signal the running recorder through a control file; their own echo
and wizards are excised from the finished demo.

### `demo stop`

End the capture. Works from inside the session or another terminal.

```sh
demo stop
```

### `demo focus <source> [<source2>]`

Switch the view to one or two **sources** (configured in the `demo capture`
wizard), recorded at the live moment you run it. `main` is the terminal; a browser
source is referenced by its id. One source fills the canvas; two are shown side by
side (`--vertical` stacks them). Run with no source on a terminal for a picker.

```sh
demo focus main                    # back to the terminal, full screen
demo focus docs                    # a browser source, full screen
demo focus main docs               # terminal | docs, side by side
demo focus main docs --vertical    # terminal over docs
demo focus docs --scroll --hold 6  # reveal docs, scroll it, hold 6s
demo focus docs --when 'build *.pdf'   # reveal once that cue appears
```

Flags: `--vertical`/`--horizontal` (two-source arrangement), `--hold <seconds>`,
`--scroll`, `--when <cue>` (a substring, or a regex with a `re:` prefix),
`--after` (when the current command finishes), `--theme light|dark`.

### `demo open <url>`

Reveal an **ad-hoc** browser page — a URL (or local `file://…`, PDF/PNG/HTML) you
did *not* pre-configure as a source. For pre-configured sources use `demo focus`.
Flags cover placement (`--split` beside the terminal vs full-screen), timing
(`--when <cue>`, `--after`, `--hold <ms>`), `--scroll`, `--theme light|dark`, and
`--view` to drive a real browser yourself. With no URL on a terminal it runs a
wizard. Run from a second terminal to keep the prompts out of the recording.

```sh
demo open https://github.com/you/repo --after
demo open ./slides.pdf --hold 4000 --split
```

## `demo record`

Execute a demo score in a real PTY and save the result as a **recording** (a
`.rec` that `demo export` plays back). This is the repeatable step: re-run it
after the app changes and the recording refreshes.

```sh
demo record [demo.toml] [-o demo.rec]
```

- `[input]` — the score to execute; defaults to `demo.toml`.
- `-o, --output` — where to write the recording; defaults to `demo.rec`.

The score's commands **actually run**, so the recording reflects the real output.
A demo whose last command leaves a process in the foreground (a server, a REPL) is
killed after a short grace period rather than blocking; end such a step with
`Ctrl-C` or `terminate` to be clean. For a multi-pane stage, only the **terminal
pane** is executed and recorded; `export` composites the browser panes around it.

> Don't want to re-execute (interactive tool, needs secrets, has side effects)?
> Skip `record` and render the faithful capture directly with `demo export --force`
> (the `demo.rec` that `capture` already wrote).

The score `record` runs is validated first (canvas/fps sane, pane ids unique and
inside the canvas, browser panes have a `url`, every step targets the right kind of
pane); it refuses to run an invalid score.

## `demo export`

Render a **recording** to one or more formats. Pure playback — it replays a
recording and **never executes** the demo.

```sh
demo export [fmt[,fmt…]] [demo.rec] [--speed 2x]
```

- `[formats]` — which formats to build, the first argument, **comma-separated**:
  `gif`, `mp4`, or `all` (see [export targets](export-targets.md)).
  Pass several at once (`demo export gif,mp4`) — and **omit it entirely to build
  every supported format** (`demo export` ≡ `demo export all`).
- `[input]` — the recording to render; defaults to `demo.rec`. Accepts a `.rec`
  from `demo capture` (faithful — handles interactive tools, secrets and side
  effects that re-execution can't) or from `demo record` (a re-executed, humanized
  take), or a raw capture (`macro.raw.toml`, if you kept one with `--raw`) to
  render it directly.
- `--speed` — retimes the recording: `2x`, `3x`, `0.5x` (a bare number works too).
  `1x` (the default) keeps the recorded pace.
- `--force` — render a **faithful capture** as-is. By default `export` refuses a
  capture's `.rec` (its typing/idle aren't humanized) and points you at
  `demo record` for a clean re-take; pass `--force` to render the live capture
  directly anyway. This is the path for **interactive / side-effecting demos**
  (a wizard that creates a repo, a flow needing secrets) that a `demo record`
  re-run would repeat or desync — there, faithful + `--force` is the only option.

Each format is written to its default path `<output_dir>/<name>.<ext>`.

For a **multi-pane stage**, `gif`/`mp4` composite the recorded terminal with its
browser panes — each captured via headless Chromium (auto-provisioned) and
revealed at the moment the timeline focuses it.

## `demo doctor`

Check the environment for the optional dependencies that browser scenes
and the `mp4` target need, and report exactly how to fix what's missing on your
platform. The core pipeline (`capture` → `record` → `export gif`) is pure Rust
and needs none of this.

```sh
demo doctor [--fix]
```

It reports three checks:

- **chromium** — a browser the automation can drive. It flags the Ubuntu **snap**
  Chromium specifically: its sandbox blocks the remote-debug port, so it can't be
  driven (the `no available ports … for debugging` error). `demo` prefers a
  non-snap Google Chrome automatically.
- **ffmpeg** — needed for `mp4`. Missing is only a warning: `mp4` auto-downloads a
  managed copy on first use.
- **display** — headed browser sessions need a graphical display (on WSL, WSLg).
  Headless reveals don't.

`--fix` installs what's missing on apt-based Linux (a non-snap Google Chrome, and
ffmpeg) — it runs `sudo`, so it prompts in your terminal. On other platforms it
prints the exact `fix:` commands to run yourself.

## `demo update`

```sh
demo update [--check] [--yes]
```

Check GitHub for a newer stable release and — only after you say yes — replace
the running `demo` binary.

- **It always asks first.** The prompt defaults to **no**; declining (or
  Ctrl-C) leaves everything exactly as it was. `--yes` skips the prompt for
  scripts.
- **`--check` changes nothing.** Exit code **1** = a newer stable release
  exists, **0** = you're current. It downloads nothing and touches no local
  path.
- **Stable only.** Releases are read from the `/releases` list and filtered in
  code: no drafts, no prereleases, and only tags strictly newer than your
  version (numeric per-component compare, so `0.3.10` beats `0.3.9`).
- **The right asset, verified.** The archive is named exactly as the release
  workflow publishes it — `demo-<tag>-<target>.tar.gz`, e.g.
  `demo-v0.3.2-x86_64-unknown-linux-musl.tar.gz` — and is checked against the
  release's `SHA256SUMS.txt` when one ships (a missing file skips, a mismatch
  is fatal).
- **State survives the swap.** The new binary is staged beside the running one
  and renamed over it. `demo.toml` scores, recordings (`.rec`), the raw macro
  and the capture sources are never read or written.
- **Refusals.** A `demo` installed under `~/.cargo/bin` is owned by cargo —
  run `cargo install --force demo-stage`. Platforms with no published
  `tar.gz` asset (Windows, linux/arm64) are refused before any download.
- **No background check.** DemoStage only ever contacts GitHub when you run
  `demo update`, and it reports network failures loudly.

## `demo edit`

```sh
demo edit [input]
```

Interactively edit timing and wait steps in a demo score. Opens a TUI with the
full timeline (the list fills the terminal height) — navigate with **↑↓** or
type to filter (e.g. `wait`), **space** marks steps, **enter** applies an
action to everything marked, **esc** finishes.

Mark **several steps** to edit them in bulk — one action applies to the whole
selection: delete a wizard's leftover interaction steps at once, or convert a
group of `wait`s to `wait_for_quiet` in one go.

Actions (applied to every marked step):

- **Keep as-is** — no change
- **wait_for_quiet** — replace with a silence-based wait
- **wait_for_screen** — replace with a VT pattern match
- **wait_for_stdout** — replace with a raw output match
- **Change duration** — set the `duration_ms` (one value for all marked)
- **Delete** — remove the marked steps
- **Split/Edit text** — (one `type` step) rewrite the text or split by delimiter
- **Find & replace in texts** — (several `type` steps) one substitution across all

`[input]` defaults to `demo.toml`.
