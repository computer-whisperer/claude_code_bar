# Claude Code Status Line: Lessons Learned

## How the status line works

Claude Code invokes `statusLine.command` as a subprocess. The process receives
a JSON blob on stdin and writes its output to stdout. CC captures that output
and renders it in the bottom status area of the TUI.

**Refresh triggers** (from CC docs + observation):
- New assistant message arrives
- Permission mode changes
- Vim mode toggles
- Debounced at 300ms
- **No refresh on terminal resize** — output is cached until the next trigger

**`statusLine` settings schema** (`~/.claude/settings.json`):
```json
{
  "statusLine": {
    "type": "command",
    "command": "/path/to/ccbar",
    "padding": 2          // optional, default 0
  }
}
```
No other fields exist. No refresh interval, no resize handler.

## stdin JSON fields

The input JSON was sparse in the buddy era; **as of CC 2.1.200 it is rich** —
much of what we used to scrape from the transcript or infer from screen popups
is now delivered directly. Full field set observed 2026-07-03:

| Field | Example | Notes |
|---|---|---|
| `model.display_name` | `"Opus 4.8 (1M context)"` | May include parenthetical |
| `model.id` | `"claude-opus-4-8[1m]"` | Fallback if display_name missing |
| `cwd` | `"/home/user/project"` | Working directory |
| `transcript_path` | `".../uuid.jsonl"` | JSONL transcript file |
| `session_id` | `"46139f0f-…"` | Stable per session |
| `session_name` | `"Re-enable measurement modes…"` | Human session title |
| `prompt_id` | `"4bce30cc-…"` | Current prompt |
| `version` | `"2.1.200"` | CC version |
| `workspace.{current_dir,project_dir,added_dirs}` | paths | |
| `output_style.name` | `"default"` | |
| `effort.level` | `"high"` | The effort indicator, as data |
| `fast_mode` | `false` | |
| `thinking.enabled` | `true` | |
| `exceeds_200k_tokens` | `false` | |
| **`context_window.used_percentage`** | `8` | **CC computes context % for us** |
| `context_window.remaining_percentage` | `92` | |
| `context_window.total_input_tokens` | `73048` | |
| `context_window.current_usage.*` | `{input,output,cache_creation,cache_read}` | Full breakdown |
| `context_window.context_window_size` | `1000000` | Max context in tokens |
| **`rate_limits.five_hour.{used_percentage,resets_at}`** | `{6, 1783105200}` | 5-hour limit |
| **`rate_limits.seven_day.{used_percentage,resets_at}`** | `{15, 1783551600}` | Weekly limit (was popup-only) |
| `cost.{total_cost_usd,total_duration_ms,total_api_duration_ms,total_lines_added,total_lines_removed}` | | Session cost + churn |

Still **not** in the input: permission mode, hooks state. Terminal geometry
comes from the `COLUMNS`/`LINES` env vars (see Width detection).

### Follow-ups these fields unlock

**Done (2026-07-03):**
- **Context bar now reads `context_window.used_percentage`** (primary), falling
  back to `current_usage` token sums, then the baseline estimate. The transcript
  token-scan is gone; `scan_transcript` survives only for the 💬 message line.
- **Cache-efficiency indicator.** A segment after the context bar shows the
  latest turn's input hit ratio `cache_read / (cache_read + cache_creation +
  input)`: gray `hit 99%` when warm (≥`CACHE_WARN` 0.98), yellow at 0.80–0.98,
  red `MISS n%` below `CACHE_MISS` 0.80. The red state fires when the prompt
  cache TTL lapses and CC re-creates the whole prefix. Thresholds are provisional
  first-attempt values pending calibration.
  - **Calibration:** `touch /tmp/ccbar_cachelog` to passively append every
    turn's cache breakdown to `/tmp/ccbar_cache.log` (`read/create/input/ratio`)
    without disturbing the rendered line. Idle past the ~5-min cache TTL, then
    send a turn, to capture a real miss and set `CACHE_MISS` on evidence.

**Not pursued (by request):** `rate_limits` — user has a separate widget.

**Available if wanted:** `effort.level`, `fast_mode`, `thinking.enabled`,
session `cost`, `session_name`.

## Environment variables available

| Variable | Example | Notes |
|---|---|---|
| `CLAUDECODE` | `1` | Always set in CC subprocess |
| `CLAUDE_CODE_ENTRYPOINT` | `cli` | How CC was launched |
| `CLAUDE_CODE_EXECPATH` | `/home/user/.local/share/claude/versions/2.1.94` | Version info |
| `CLAUDE_PROJECT_DIR` | varies | **Unreliable** — inherited from launching shell, may not match `cwd` in JSON |
| `COLUMNS` | *(not set)* | Not exported by CC |

## Width detection

**stdout is a pipe** — `isatty(1) == false` and `ioctl(1, TIOCGWINSZ)` fails.

**`/dev/tty` no longer works (as of CC 2.1.200).** Earlier versions gave the
status-line subprocess a controlling terminal, so `open("/dev/tty")` +
`ioctl(TIOCGWINSZ)` returned correct dimensions. Current CC spawns us **without
a controlling terminal**: the open fails with `ENXIO` ("No such device or
address"). All three standard fds are pipes (`ENOTTY`). The 2026-07-03 probe
confirmed every ioctl path is dead.

**`COLUMNS` is the current source.** CC now exports `COLUMNS` (and `LINES`) into
our environment — observed `COLUMNS=101 LINES=48` on a 101-col Alacritty.
`terminal_width()` reads `COLUMNS` first and keeps the `/dev/tty` ioctl only as
a fallback for environments that still provide a ctty. Value is fresh per
invocation. Constant retained: `TIOCGWINSZ = 0x5413`.

## CC's rendering behavior

### ANSI escape handling
CC **correctly strips ANSI CSI sequences** when measuring line width. Colors
do not consume visible-cell budget. Confirmed by emitting side-by-side plain
and ANSI-wrapped lines at the same visible width — both rendered identically.

Earlier observations suggesting "ANSI eats budget" were artifacts of the
weekly-usage popup being active during those tests.

### Emoji / Unicode handling
CC **correctly measures emoji as double-wide cells**. A line containing 8
folder emojis (📁 × 8 = 16 cells) at target width 20 rendered identically to
a 20-cell plain ASCII line.

### Cascade drop — GONE as of CC 2.1.200
Earlier CC **stopped rendering after the first truncated row** — an overflow on
row N dropped rows N+1 and beyond entirely. That was the single most important
layout constraint, and the reason `FIRST_LINE_EXTRA` was so large.

**No longer true.** The 2026-07-03 sweep at term=101 showed each overflowing row
(w=98,99,100,101) **clips individually** with no effect on the rows below. A
clipped row now costs only that row's right edge — graceful degradation. Because
our segments are ordered with `model` trailing last, a wide top-row popup clips
`model` first, and the message line below is always safe.

### Per-row budget
The visible-cell budget per row is approximately:

```
usable_width ≈ terminal_width − right_reservation
```

Where `right_reservation` accounts for CC's own padding. **Re-measured
2026-07-03 (buddy retired):** the sweep at term=101 rendered fully to w=97, so
the hard right edge reserves **4 cells**; `RIGHT_RESERVATION = 5` is used to
balance the left/right margins. (Was 21 in the buddy era — the buddy art
accounted for the difference.)

### First-row popups
CC occasionally overlays the top row with:
- **Weekly usage popup**: `"You've used 78% of your weekly limit · resets Apr 10, 12am (Amer…"`
- **Effort indicator**: `"● high · /effort"`

These share the first row with our content and reduce the available width. With
cascade-drop gone, a popup only clips row 1's right edge (the trailing `model`
segment) — the rows below are unaffected.

**Mitigation:** a modest row-1 penalty, `FIRST_LINE_EXTRA = 24` (row 1 budget
`term − 5 − 24`), keeps the effort indicator off our content. Wide weekly-usage
popups may still clip `model`; that's accepted. Note the popup data is also now
available structurally in stdin (`rate_limits`, see below), so we no longer have
to infer it from screen artifacts.

## Re-profile results (2026-07-03, CC 2.1.200)

Original re-probe questions, now answered by the width sweep + probe:

- **Width source**: `/dev/tty` ioctl is dead (no ctty); **use `COLUMNS` env**.
- **Right reservation**: dropped from 21 → **4** (use 5 for margin balance).
- **Cascade-drop**: **gone** — rows clip individually now.
- **Total visible rows**: not re-measured (no longer a binding constraint now
  that clipping is graceful; `LINES=48` is exported if a vertical cap ever
  matters). The sweep emitted 14 rows and all within budget rendered.

Diagnostic procedure (unchanged, probe now also logs width sources + raw stdin):
1. `touch /tmp/ccbar_diag` to enable diagnostic mode
2. Trigger a render (send a message to CC)
3. Read `/tmp/ccbar_diag.log` — width-source probe, raw stdin, and per-row widths
4. `rm /tmp/ccbar_diag` to return to normal mode

## Transcript JSONL structure

The transcript file is a goldmine for future features. Per-entry top-level keys:

```
cwd, entrypoint, gitBranch, isSidechain, messageId, parentUuid,
permissionMode, promptId, requestId, sessionId, slug,
sourceToolAssistantUUID, timestamp, toolUseResult, type, userType,
uuid, version
```

Entry types: `assistant`, `user`, `attachment`, `file-history-snapshot`, `permission-mode`

Per-assistant `usage` keys:
```
input_tokens, output_tokens, cache_read_input_tokens,
cache_creation_input_tokens, cache_creation, inference_geo,
iterations, server_tool_use, service_tier, speed
```

## Feature ideas (from transcript data)

Ranked by estimated value, not yet implemented:

1. **Permission mode indicator** — `permissionMode` field + `permission-mode` entry type
2. **Speed/fast indicator** — `usage.speed` present on some turns
3. **Session age** — first vs last `timestamp`
4. **Cache hit ratio** — `cache_read / total` on last assistant turn
5. **Files touched this session** — walk `toolUseResult` entries
6. **Sub-agent activity count** — count `isSidechain: true` entries

## Architecture notes

- Single binary `ccbar`, Rust, ~600 lines in `src/main.rs`
- Deps: `serde`, `serde_json`, `unicode-width`
- Reads stdin JSON once, parses transcript once (shared between context-pct and last-user-message)
- Width detection via raw `ioctl` FFI (no libc crate, Linux-only `TIOCGWINSZ = 0x5413`)
- Greedy line-wrap with per-segment ANSI isolation (each segment ends with RESET)
- Diag mode gated by marker file `/tmp/ccbar_diag`, logs to `/tmp/ccbar_diag.log`
