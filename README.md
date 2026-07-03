# ccbar

A fast Rust status line for [Claude Code](https://claude.com/claude-code). It
reads the JSON that CC pipes to `statusLine.command` on stdin and renders a
compact, multi-line status bar.

```
▊░░░░░░░░░ 8% of 1M | hit 99% | 📁claude_code_bar
🔀main (synced 2m) | Opus 4.8
💬 last thing you asked…
```

## What it shows

- **Context bar** — fraction of the context window in use, from CC's own
  `context_window.used_percentage` (with a token-breakdown fallback).
- **Cache indicator** — the latest turn's prompt-cache hit ratio: quiet gray
  `hit 99%` when warm, yellow when it dips, red `MISS n%` when the cache TTL
  lapsed and the whole prefix had to be re-created.
- **Directory**, **git** branch + dirty/sync status, **model**, and the **last
  user message**.

Content reflows across lines to fit the terminal width, which is read from the
`COLUMNS` env var CC exports (with a `/dev/tty` ioctl fallback).

## Install

```sh
cargo build --release
```

Point Claude Code at the binary in `~/.claude/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "/path/to/claude_code_bar/target/release/ccbar"
  }
}
```

## Diagnostics

The layout depends on how CC spawns and measures the status line, which shifts
between versions. Two marker files toggle instrumentation at runtime (no
rebuild):

- `touch /tmp/ccbar_diag` — render a width-sweep test pattern and log every
  width source + the raw stdin to `/tmp/ccbar_diag.log`. `rm` to restore.
- `touch /tmp/ccbar_cachelog` — passively append each turn's cache token
  breakdown (session-tagged) to `/tmp/ccbar_cache.log`, for calibrating the
  cache thresholds. `rm` to stop.

`ccbar --dump` prints the parsed stdin, environment, and transcript summary.

See [`STATUS_LINE_NOTES.md`](STATUS_LINE_NOTES.md) for the measured details of
CC's rendering behavior.

## Requirements

Linux (the width-detection fallback uses a Linux `ioctl`); `git` on `PATH` for
the git segment.
