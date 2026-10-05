---
title: The demo.toml DSL
description: Every table and timeline action in a demo score — demo, env, typing, layout, panes and the timeline.
order: 4
---

# The `demo.toml` DSL

A score is a TOML document with four areas: metadata, the (optional) sandbox env,
the typing parameters, the canvas layout, and the timeline.

## `[demo]`

```toml
[demo]
name = "my-demo"          # used for the output filename
output_dir = "./dist"     # default
speed = "2x"              # how this demo is published; omit for 1x
targets = ["gif", "mp4"]  # formats to build; omit for the defaults (gif, mp4; svg is opt-in)
# prompt = "\[\e[32m\]❯\[\e[0m\] "   # custom prompt; omit for the default `$ `
```

`speed` and `targets` say **how this demo is meant to be exported**, so the next
person to rebuild it gets the same result without knowing which flags you used.
A demo is usually recorded at a comfortable pace and published faster, and
without this the multiplier survives nowhere: the published file's duration is
the only remaining evidence of it.

Both are defaults, not locks — `--speed` and a positional target still win:

```sh
demo export demo.rec              # 2x and gif+mp4, per the score above
demo export gif demo.rec          # 2x, gif only
demo export demo.rec --speed 1x   # the recorded pace, both formats
```

`prompt` is bash `PS1` syntax, so colours (`\[\e[36m\]…\[\e[0m\]`) and escapes
like `\w` work. The built-in default is the plain Linux `$ `. Export always forces
this prompt over your rc files, so a demo never leaks `user@host`. The pixel
targets (gif/mp4) render a green-arrow `❯` and a handful of common prompt symbols,
so a custom prompt rasterizes too; very exotic glyphs may not.

## `[env]` (optional)

A sandbox for an isolated, reproducible run. Scripts run **before** the clock
starts (setup) and **after** the timeline (teardown), so their output is not
captured.

```toml
[env]
isolated = true
setup_script = "mkdir -p /tmp/demo-sandbox && cd /tmp/demo-sandbox"
teardown_script = "rm -rf /tmp/demo-sandbox"
requires = ["GITHUB_TOKEN"]   # env vars export needs; values come from your shell
```

`requires` lists environment variables the export run needs — typically a token
that lets a flow skip a secret prompt (so the demo stays reproducible without
storing the secret). The values come from whoever runs `export`; the score never
holds them. `demo record` validates the score before running and fails if a
required variable is unset.

## `[typing]` (optional)

Controls the humanized per-character jitter applied to `type` steps that set
`human_salt = true`.

```toml
[typing]
base_ms = 80     # base speed, ms per character
salt_ms = 15     # max jitter ±, ms
seed = 42        # omit for a random feel; set for reproducible output
```

## `[[sources]]` (optional)

The content sources a demo can show: the terminal (`main`) and any browser pages
(repo, docs, localhost, a local PDF). The `demo capture` wizard writes them;
during a capture, `demo focus <source>` reveals them. Authoring them by hand
works too.

```toml
[[sources]]
id = "main"
type = "terminal"

[[sources]]
id = "docs"
type = "browser"
url = "https://docs.example.com"
# theme = "dark"        # emulated colour scheme: "light" | "dark"
```

## `[layout]` and `[[layout.panes]]`

The canvas (pixels) and the panes placed on it.

```toml
[layout]
width = 1920
height = 1080
fps = 15               # 15, 24, or 30 — set at capture via --fps
line_height = 1.0      # gif/mp4 line spacing × font size; 1.0 connects box-drawing
background = "#0b0f14"

  [[layout.panes]]
  id = "console"        # unique id, referenced by the timeline
  type = "terminal"     # "terminal" | "browser"
  x = 0
  y = 0
  width = 960
  height = 1080
  font_family = "monospace"   # terminal only
  font_size = 16              # terminal only

  [[layout.panes]]
  id = "preview"
  type = "browser"
  x = 960
  y = 0
  width = 960
  height = 1080
  url = "file:///tmp/demo-sandbox/output.pdf"   # browser requires a url
  # theme = "dark"       # emulated colour scheme: "light" | "dark"
  # reveal_at = 3.5      # visibility window (seconds): overlay shows from here…
  # hide_at = 8.0        # …until here (omit either for "from the start"/"to the end")
```

`demo record` validates the score before it runs: pane ids are unique, panes fit
the canvas, and browser panes have a `url`.

`reveal_at`/`hide_at` (browser panes) window a pane in time: the terminal is the
always-on background and each browser pane overlays it only inside its window —
this is what a live `demo focus` records, and how switching views (or going back
to the terminal) renders.

`ignore_speed` (browser panes, optional, default `false`) exempts a pane from the
export speed multiplier. A PDF pane with `ignore_speed = true` pans at the 1x cap
(1200 px/s) regardless of `--speed`, while the rest of the demo still accelerates.
Use it for a document that should remain readable at any export speed.

```toml
  [[layout.panes]]
  id = "pdf-r1"
  type = "browser"
  x = 0
  y = 0
  width = 960
  height = 1080
  url = "file:///tmp/demo-sandbox/output.pdf"
  ignore_speed = true   # pan at the 1x cap even when the demo is exported at 2x
```

`line_height` (optional, default `1.2`) is the line spacing as a multiple of the
font size. `1.0` makes box-drawing characters (`│ ─ ┌ ┘ …`) join into continuous
lines for TUIs; raise it (e.g. `1.25`) for airier, prose-style spacing.

## `[[timeline]]`

Steps share one timeline, each tagged by `action`:

| `action` | Fields | Meaning |
|---|---|---|
| `focus` | `pane` | Make a pane the active input target. |
| `type` | `text`, `human_salt?` | Type into the focused terminal. |
| `keypress` | `key` | Press a named key — `enter`, `tab`, `esc`, `up`, `ctrl+c`, … |
| `wait` | `duration_ms` | Hold for a fixed time. |
| `wait_for_stdout` | `match`, `pane?` | Block until a substring appears in the raw output. |
| `wait_for_quiet` | `quiet_ms`, `max_ms?` | Block until the output has been silent this long. |
| `wait_for_screen` | `match`, `timeout_ms?` | Block until a pattern is visible on the rendered screen. |
| `caption` | `text` | Show an on-canvas step label (empty `text` clears it). Rendered on `gif`/`mp4`. |
| `secret` | `prompt` | Re-supply a secret at this point on `demo record` (the value is asked for, never stored). |
| `scroll` | `direction`, `velocity?`, `duration_ms`, `pane?` | Scroll a browser pane. `direction` is `up` or `down` (default `down`); `up` starts at the bottom and pans to the top. `velocity` is `constant` (linear, default) or `ease_in_out` (smooth acceleration/deceleration). When several scroll steps target the same pane, the first wins. |
| `terminate` | — | End the demo. |

```toml
[[timeline]]
action = "focus"
pane = "console"

[[timeline]]
action = "type"
text = "cargo build --release"
human_salt = true

[[timeline]]
action = "keypress"
key = "enter"

[[timeline]]
action = "wait_for_stdout"
match = "Finished"

[[timeline]]
action = "terminate"
```
