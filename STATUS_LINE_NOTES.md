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

The input JSON is sparse. Observed fields:

| Field | Example | Notes |
|---|---|---|
| `model.display_name` | `"Opus 4.6 (1M context)"` | May include parenthetical |
| `model.id` | `"claude-opus-4-6"` | Fallback if display_name missing |
| `cwd` | `"/home/user/project"` | Working directory |
| `transcript_path` | `"/home/user/.claude/projects/.../uuid.jsonl"` | JSONL transcript file |
| `context_window.context_window_size` | `1000000` | Max context in tokens |

No session ID, no permission mode, no version, no hooks state in the input.
Everything else must come from env vars or the transcript file.

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

**`/dev/tty` works.** Opening `/dev/tty` and calling `ioctl(TIOCGWINSZ)` returns
correct terminal dimensions. This is what git/less/etc. do in piped contexts.
Linux constant: `TIOCGWINSZ = 0x5413`.

The width value is **fresh per invocation** — if the terminal was resized between
renders, the next ccbar invocation sees the new size. However, since CC caches
the rendered output and only re-invokes on the triggers listed above, the user
may see stale layout after resizing until the next interaction.

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

### Cascade drop
**CC stops rendering after the first truncated row.** If row N exceeds its
visible-cell budget and gets `…`-truncated, rows N+1 and beyond are dropped
entirely. This is the single most important layout constraint.

Implication: every row must fit within its budget or all subsequent rows
(including the message line) are lost. There is no graceful degradation.

### Per-row budget
The visible-cell budget per row is approximately:

```
usable_width ≈ terminal_width − right_reservation
```

Where `right_reservation` accounts for CC's own padding and any right-side
UI elements (buddy art, notification badges, etc.). Empirically measured as
~21 cells at the time of initial testing (subject to change — see below).

### First-row popups
CC occasionally overlays the top row with:
- **Weekly usage popup**: `"You've used 78% of your weekly limit · resets Apr 10, 12am (Amer…"`
- **Effort indicator**: `"● high · /effort"`

These share the first row with our content and reduce the available width.
When our row-1 content gets truncated by a popup, cascade-drop kills all
subsequent rows.

**Mitigation:** use a tighter budget for row 1 (`term − 81` currently) so
content is short enough to coexist with popups. Accept that very wide popups
will occasionally truncate row 1.

## What needs re-probing

**The "Spindrizzle" buddy was an April 1st feature that has since been
removed.** All measurements of the right-side reservation (21 cells) were
taken with the buddy active. The available canvas is likely wider now.

To recalibrate:
1. `touch /tmp/ccbar_diag` to enable diagnostic mode
2. Trigger a render (send a message to CC)
3. Check `/tmp/ccbar_diag.log` and observe which lines fit
4. `rm /tmp/ccbar_diag` to return to normal mode

Specifically, re-probe:
- **Right reservation**: may shrink from 21 to ~2-4 (just CC's own padding)
- **First-row popup budget**: may be wider without the buddy eating right-side space
- **Total visible rows**: were getting 7+ rows with buddy; may get more without

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
