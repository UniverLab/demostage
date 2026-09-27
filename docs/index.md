---
title: DemoStage
description: Demos as Code — record a terminal session, normalize it into a clean declarative score, and compile it to gif or mp4.
order: 1
---

# DemoStage

**DemoStage** turns terminal demonstrations into reproducible engineering. Instead
of capturing brittle, mutable pixels, it records a session as a sequence of
**events**, refines human imperfections into a clean **score** (`demo.toml`), and
compiles that score to several optimized formats.

A demo becomes a small, version-controlled, re-runnable artifact — diffable like
any other source file.

## The pipeline

```
demo capture  ──>  demo.toml  ──>  demo record  ──>  demo.rec  ──>  demo export  ──>  dist/
              (editable score)    (re-run clean)                   (render — no re-run)
                   └─ demo.rec (faithful) ─────────────────────>  demo export --force
```

| Command | In | Out | Does |
|---|---|---|---|
| `capture`   | TTY               | `demo.toml` + `demo.rec` (`--raw` adds the macro) | Capture a live session into an editable score + a faithful recording (forces a clean prompt). |
| `record`    | `demo.toml`       | `demo.rec`      | Validate, then re-execute the score in a PTY → a clean, humanized recording. |
| `export`    | `demo.rec`       | `dist/…`         | Render the recording to `gif` or `mp4`. Never executes. Needs `--force` for a faithful capture. |
| `edit`      | `demo.toml`       | `demo.toml`     | Edit the timeline interactively — mark several steps for bulk edits. |
| `doctor`    | —                 | a report         | Check the browser/ffmpeg/display deps and report fixes (`--fix` installs them on apt). |
| `update`    | —                 | a newer `demo`   | Explicitly check for and install a newer stable release — always asks first; refuses cargo installs. |

**Live control** (run during a `demo capture`, from the captured shell or another
terminal in the same directory):

| Command | Does |
|---|---|
| `demo stop` | End the capture (`exit` / Ctrl-D also work). |
| `demo focus <source> [<source2>]` | Switch the view to one or two configured sources. |
| `demo open <url>` | Reveal an ad-hoc browser page (a URL not pre-configured). |

The clean path is `capture → record → export`. A `demo.toml` can also be
**authored by hand**. When a demo **can't be re-run** (interactive, secrets, side
effects), skip `record` and render the faithful capture with `demo export --force`.

## Why it exists

Recording demos by hand is fiddly: typos, uneven pacing, dead air, and a prompt
that leaks your username and hostname. DemoStage fixes those mechanically — see
[the normalizer](normalizer.md) — so the result looks deliberate without being faked.

## Next

- [Installation](installation.md)
- [Quickstart](quickstart.md)
- [The `demo.toml` DSL](demo-toml.md)
- [Commands](commands.md)
- [The smart normalizer](normalizer.md)
- [Export targets](export-targets.md)
