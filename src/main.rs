use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use serde::Deserialize;
use unicode_width::UnicodeWidthChar;

// ANSI color codes (matches the original context-bar.sh "blue" theme).
// Diag work confirmed CC strips ANSI properly when measuring width — these
// are safe to use without affecting the per-line budget.
const RESET: &str = "\x1b[0m";
const GRAY: &str = "\x1b[38;5;245m";
const BAR_EMPTY: &str = "\x1b[38;5;238m";
const ACCENT: &str = "\x1b[38;5;74m";

const BAR_WIDTH: usize = 10;
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
// Conservative pre-conversation estimate: system prompt + tools + memory + framing.
const BASELINE_TOKENS: u64 = 20_000;

// Width budget. Empirically at term=101 the visible content area is ~80 cells
// (term − Spindrizzle right-side art − padding). Row 1 gets an extra
// reservation to dodge weekly-usage popups + effort-strength indicators that
// only ever appear on the top row.
const RIGHT_RESERVATION: usize = 21;
const FIRST_LINE_EXTRA: usize = 60;
const MIN_USABLE_WIDTH: usize = 20;
const FALLBACK_TERM_WIDTH: usize = 80;

// Diagnostic mode: if this marker file exists, ccbar emits a test pattern
// instead of the normal status line and logs every invocation. Toggle with
// `touch /tmp/ccbar_diag` / `rm /tmp/ccbar_diag` — no rebuild needed.
const DIAG_MARKER: &str = "/tmp/ccbar_diag";
const DIAG_LOG: &str = "/tmp/ccbar_diag.log";

#[derive(Debug, Deserialize, Default)]
struct Input {
    #[serde(default)]
    model: Model,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    context_window: Option<ContextWindow>,
}

#[derive(Debug, Deserialize, Default)]
struct Model {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ContextWindow {
    #[serde(default)]
    context_window_size: Option<u64>,
}

struct GitInfo {
    branch: String,
    status: String,
}

#[derive(Default)]
struct TranscriptScan {
    last_total_tokens: Option<u64>,
    last_user_message: Option<String>,
}

fn main() {
    let mut buf = String::new();
    if io::stdin().read_to_string(&mut buf).is_err() {
        return;
    }

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--dump") {
        dump_input(&buf);
        return;
    }

    if std::path::Path::new(DIAG_MARKER).exists() {
        run_diag_mode();
        return;
    }

    let parsed: Input = serde_json::from_str(&buf).unwrap_or_default();

    let model_raw = parsed
        .model
        .display_name
        .as_deref()
        .or(parsed.model.id.as_deref())
        .unwrap_or("?");
    // Strip parenthetical context-window suffix: "Opus 4.6 (1M context)" → "Opus 4.6".
    let model_name = model_raw
        .split('(')
        .next()
        .unwrap_or(model_raw)
        .trim();

    let cwd_str = parsed.cwd.as_deref().unwrap_or("");
    let dir = if cwd_str.is_empty() {
        "?".to_string()
    } else {
        Path::new(cwd_str)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "?".to_string())
    };

    let git_info = if !cwd_str.is_empty() && Path::new(cwd_str).is_dir() {
        gather_git_info(cwd_str)
    } else {
        None
    };

    let max_context = parsed
        .context_window
        .as_ref()
        .and_then(|cw| cw.context_window_size)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);

    let scan = parsed
        .transcript_path
        .as_deref()
        .and_then(scan_transcript)
        .unwrap_or_default();

    let (pct, pct_prefix) = compute_context_pct(scan.last_total_tokens, max_context);
    let bar = render_bar(pct);

    let (first_budget, rest_budget) = line_budgets();
    let separator = format!("{} | {}", GRAY, RESET);

    // Order: most-useful (context) → mostly-static (dir) → state (git) →
    // deprioritized (model). Model trails so it lands on the last wrap line.
    let mut segments: Vec<String> = Vec::with_capacity(4);
    segments.push(format!(
        "{} {}{}{}% of {}{}",
        bar,
        GRAY,
        pct_prefix,
        pct,
        format_token_count(max_context),
        RESET
    ));
    segments.push(format!("{}📁{}{}", GRAY, dir, RESET));
    if let Some(g) = &git_info {
        segments.push(format!("{}🔀{} {}{}", GRAY, g.branch, g.status, RESET));
    }
    segments.push(format!("{}{}{}", ACCENT, model_name, RESET));

    for line in wrap_segments(&segments, &separator, first_budget, rest_budget) {
        println!("{}", line);
    }

    if let Some(msg) = scan.last_user_message {
        let prefix = "💬 ";
        let prefix_w = visible_width(prefix);
        let max_msg = rest_budget.saturating_sub(prefix_w);
        let trimmed = truncate_to_width(&msg, max_msg);
        println!("{}{}", prefix, trimmed);
    }
}

// --- git -------------------------------------------------------------------

fn gather_git_info(cwd: &str) -> Option<GitInfo> {
    let branch = git_cmd(cwd, &["branch", "--show-current"]).ok()?;
    let branch = branch.trim().to_string();
    if branch.is_empty() {
        return None;
    }

    let porcelain = git_cmd(
        cwd,
        &["--no-optional-locks", "status", "--porcelain", "-uall"],
    )
    .unwrap_or_default();
    let lines: Vec<&str> = porcelain.lines().filter(|l| !l.is_empty()).collect();
    let file_count = lines.len();

    let sync_status = compute_sync_status(cwd);

    // Build the parenthetical inner. For one dirty file, name it; for many,
    // count them; for none, show sync only.
    let mut parts: Vec<String> = Vec::new();
    if file_count == 1 {
        // porcelain line is `XY filename`; strip the 3-char status prefix.
        let single = lines[0].get(3..).unwrap_or(lines[0]);
        parts.push(single.to_string());
    } else if file_count > 1 {
        parts.push(format!("{} dirty", file_count));
    }
    if !sync_status.is_empty() {
        parts.push(sync_status);
    }
    let status = if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join(", "))
    };

    Some(GitInfo { branch, status })
}

fn compute_sync_status(cwd: &str) -> String {
    let upstream = match git_cmd(cwd, &["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return "no upstream".to_string(),
    };
    if upstream.is_empty() {
        return "no upstream".to_string();
    }

    let counts = git_cmd(
        cwd,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    )
    .ok();
    let (ahead, behind) = match counts {
        Some(s) => {
            let mut it = s.split_whitespace();
            let a: u64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let b: u64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            (a, b)
        }
        None => (0, 0),
    };

    if ahead == 0 && behind == 0 {
        match fetch_head_age(cwd) {
            Some(age) => format!("synced {}", age),
            None => "synced".to_string(),
        }
    } else if ahead > 0 && behind == 0 {
        format!("{} ahead", ahead)
    } else if ahead == 0 && behind > 0 {
        format!("{} behind", behind)
    } else {
        format!("{} ahead, {} behind", ahead, behind)
    }
}

fn fetch_head_age(cwd: &str) -> Option<String> {
    let path = Path::new(cwd).join(".git/FETCH_HEAD");
    let metadata = std::fs::metadata(&path).ok()?;
    let modified = metadata.modified().ok()?;
    let diff = SystemTime::now().duration_since(modified).ok()?.as_secs();
    Some(humanize_age(diff))
}

fn humanize_age(secs: u64) -> String {
    if secs < 60 {
        "<1m".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn git_cmd(cwd: &str, args: &[&str]) -> io::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("git failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// --- transcript ------------------------------------------------------------

fn scan_transcript(path: &str) -> Option<TranscriptScan> {
    let content = std::fs::read_to_string(path).ok()?;
    let entries: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    let mut last_total: Option<u64> = None;
    for v in &entries {
        if v.get("isSidechain").and_then(|x| x.as_bool()) == Some(true) {
            continue;
        }
        if v.get("isApiErrorMessage").and_then(|x| x.as_bool()) == Some(true) {
            continue;
        }
        let Some(usage) = v.get("message").and_then(|m| m.get("usage")) else {
            continue;
        };
        let input = usage
            .get("input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let cache_creation = usage
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        last_total = Some(input + cache_read + cache_creation);
    }

    let mut last_user: Option<String> = None;
    for v in entries.iter().rev() {
        if v.get("type").and_then(|x| x.as_str()) != Some("user") {
            continue;
        }
        let Some(text) = extract_user_text(v) else {
            continue;
        };
        let cleaned = clean_message(&text);
        if cleaned.is_empty()
            || cleaned.starts_with("[Request interrupted")
            || cleaned.starts_with("[Request cancelled")
        {
            continue;
        }
        last_user = Some(cleaned);
        break;
    }

    Some(TranscriptScan {
        last_total_tokens: last_total,
        last_user_message: last_user,
    })
}

fn extract_user_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            let parts: Vec<String> = arr
                .iter()
                .filter(|item| item.get("type").and_then(|x| x.as_str()) == Some("text"))
                .filter_map(|item| {
                    item.get("text")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        }
        _ => None,
    }
}

fn clean_message(s: &str) -> String {
    let replaced = s.replace('\n', " ");
    let mut out = String::with_capacity(replaced.len());
    let mut prev_space = false;
    for c in replaced.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(c);
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

// --- context bar -----------------------------------------------------------

fn compute_context_pct(measured: Option<u64>, max_context: u64) -> (u64, &'static str) {
    if let Some(tokens) = measured.filter(|t| *t > 0) {
        let pct = (tokens * 100 / max_context).min(100);
        (pct, "")
    } else {
        let pct = (BASELINE_TOKENS * 100 / max_context).min(100);
        (pct, "~")
    }
}

/// Compact token count: 1_000_000 → "1M", 1_500_000 → "1.5M", 200_000 → "200k".
fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        let m_int = n / 1_000_000;
        let frac_tenths = (n % 1_000_000) / 100_000;
        if frac_tenths == 0 {
            format!("{}M", m_int)
        } else {
            format!("{}.{}M", m_int, frac_tenths)
        }
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn render_bar(pct: u64) -> String {
    // 8 sub-cells per character cell using Unicode left-block partials.
    // BAR_WIDTH=10 → 80 levels of resolution across 0..100%.
    const SUBCELLS_PER_CELL: u64 = 8;
    let total = (BAR_WIDTH as u64) * SUBCELLS_PER_CELL;
    let filled = (pct * total / 100).min(total);

    let mut bar = String::new();
    for i in 0..(BAR_WIDTH as u64) {
        let cell_start = i * SUBCELLS_PER_CELL;
        let cell_end = cell_start + SUBCELLS_PER_CELL;
        if filled >= cell_end {
            bar.push_str(ACCENT);
            bar.push('█');
            bar.push_str(RESET);
        } else if filled <= cell_start {
            bar.push_str(BAR_EMPTY);
            bar.push('░');
            bar.push_str(RESET);
        } else {
            let sub = filled - cell_start;
            let ch = match sub {
                1 => '▏',
                2 => '▎',
                3 => '▍',
                4 => '▌',
                5 => '▋',
                6 => '▊',
                7 => '▉',
                _ => '█',
            };
            bar.push_str(ACCENT);
            bar.push(ch);
            bar.push_str(RESET);
        }
    }
    bar
}

// --- width / wrap helpers --------------------------------------------------

/// Visible cell width of `s`, ignoring ANSI CSI escape sequences.
fn visible_width(s: &str) -> usize {
    let mut width = 0usize;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ESC [ ... <letter> (ANSI CSI sequence).
            if let Some('[') = chars.next() {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    width
}

/// Greedy reflow: pack `segments` into lines joined by `separator`. Line 1
/// uses `first_budget`, every subsequent line uses `rest_budget`. A segment
/// wider than its line's budget goes on its own line and may overflow.
fn wrap_segments(
    segments: &[String],
    separator: &str,
    first_budget: usize,
    rest_budget: usize,
) -> Vec<String> {
    let sep_w = visible_width(separator);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    let mut budget = first_budget;
    for seg in segments {
        let seg_w = visible_width(seg);
        if current.is_empty() {
            current.push_str(seg);
            current_w = seg_w;
        } else if current_w + sep_w + seg_w <= budget {
            current.push_str(separator);
            current.push_str(seg);
            current_w += sep_w + seg_w;
        } else {
            lines.push(std::mem::take(&mut current));
            budget = rest_budget;
            current.push_str(seg);
            current_w = seg_w;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Truncate `s` to at most `max` visible cells, appending `…` if it had to cut.
fn truncate_to_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if visible_width(s) <= max {
        return s.to_string();
    }
    let limit = max.saturating_sub(1); // reserve one cell for the ellipsis
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > limit {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Visible-cell budgets for the first line and all subsequent lines.
/// Row 1 gets extra reservation to dodge weekly-usage popups + the
/// `● high · /effort` indicator that only ever appear on the top row.
fn line_budgets() -> (usize, usize) {
    let term_w = terminal_width()
        .map(|w| w as usize)
        .unwrap_or(FALLBACK_TERM_WIDTH);
    let first = term_w
        .saturating_sub(RIGHT_RESERVATION + FIRST_LINE_EXTRA)
        .max(MIN_USABLE_WIDTH);
    let rest = term_w
        .saturating_sub(RIGHT_RESERVATION)
        .max(MIN_USABLE_WIDTH);
    (first, rest)
}

// --- terminal width --------------------------------------------------------

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Winsize {
    row: u16,
    col: u16,
    xpixel: u16,
    ypixel: u16,
}

// Linux: TIOCGWINSZ = 0x5413, ioctl request type is unsigned long.
const TIOCGWINSZ: u64 = 0x5413;

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, argp: *mut Winsize) -> i32;
}

/// Width of the controlling terminal in columns, or None if it can't be read.
/// We open `/dev/tty` directly because stdout is a pipe in the status-line
/// invocation context.
fn terminal_width() -> Option<u16> {
    use std::os::fd::AsRawFd;
    let f = std::fs::File::open("/dev/tty").ok()?;
    let mut ws = Winsize::default();
    let r = unsafe { ioctl(f.as_raw_fd(), TIOCGWINSZ, &mut ws as *mut _) };
    if r == 0 && ws.col > 0 {
        Some(ws.col)
    } else {
        None
    }
}

// --- diagnostic mode -------------------------------------------------------

fn run_diag_mode() {
    use std::io::Write;

    let term = terminal_width();
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tag = now % 10_000;
    let term_str = term
        .map(|w| w.to_string())
        .unwrap_or_else(|| "?".to_string());

    let mut emitted: Vec<String> = Vec::with_capacity(7);

    // R1: 20-cell row-1 candidate. Carries the tag so the user can read it
    // off the screen and quote it back. Tests popup-safe row-1 width.
    emitted.push(diag_pad(format!("R1 t={} tag={:04} ", term_str, tag), 20));

    // R2-R4: 8-cell narrow rows. If all render but a wider row up top
    // truncates, sub-row budgets are uniform (effort indicator is row-1 only).
    // If R3/R4 also truncate, the indicator/popup clips multiple rows.
    emitted.push(diag_pad("R2 ".to_string(), 8));
    emitted.push(diag_pad("R3 ".to_string(), 8));
    emitted.push(diag_pad("R4 ".to_string(), 8));

    // R5: 20-cell emoji content. Tests CC's measurement of wide-cell chars.
    emitted.push(diag_emoji_line(20));

    // R6: 20-cell visible content wrapped in ANSI gray. Tests if CC counts
    // ANSI escape bytes as visible cells.
    let r6 = diag_pad("R6 ".to_string(), 20);
    emitted.push(format!("{}{}{}", GRAY, r6, RESET));

    // R7: 20-cell plain content. Control for R5 / R6.
    emitted.push(diag_pad("R7 ".to_string(), 20));

    for line in &emitted {
        println!("{}", line);
    }

    // Append every invocation to the log so we can correlate with what the
    // user reports seeing on screen.
    let mut log = format!(
        "=== ts={} tag={:04} term={:?} pid={} ===\n",
        now,
        tag,
        term,
        std::process::id()
    );
    for line in &emitted {
        log.push_str(line);
        log.push('\n');
    }
    log.push('\n');
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(DIAG_LOG)
    {
        let _ = f.write_all(log.as_bytes());
    }
}

/// Pad `s` with `-` until it reaches `target - 1` cells, then append `]`.
fn diag_pad(mut s: String, target: usize) -> String {
    while visible_width(&s) < target - 1 {
        s.push('-');
    }
    s.push(']');
    s
}

/// Build a `target`-cell line filled with double-wide emoji glyphs.
fn diag_emoji_line(target: usize) -> String {
    let mut s = String::from("R5 ");
    // Each emoji is 2 cells; stop before exceeding target - 1 (saves space for `]`).
    while visible_width(&s) + 2 <= target - 1 {
        s.push('📁');
    }
    while visible_width(&s) < target - 1 {
        s.push('-');
    }
    s.push(']');
    s
}

// --- ruler (kept for follow-up probes) -------------------------------------

/// Build a column ruler `width` chars wide:
/// - tens digits at multiples of 10 (cycles after col 99)
/// - `+` at multiples of 5
/// - `-` everywhere else
#[allow(dead_code)] // kept for follow-up width probes
fn ruler_line(width: usize) -> String {
    let mut s = String::with_capacity(width);
    for c in 0..width {
        if c % 10 == 0 {
            let tens = ((c / 10) % 10) as u32;
            s.push(char::from_digit(tens, 10).unwrap());
        } else if c % 5 == 0 {
            s.push('+');
        } else {
            s.push('-');
        }
    }
    s
}

// --- dump mode -------------------------------------------------------------

fn dump_input(raw: &str) {
    println!("=== stdin (raw bytes: {}) ===", raw.len());
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            println!();
            dump_transcript_info(&v);
        }
        Err(e) => {
            println!("(failed to parse as JSON: {})", e);
            println!("{}", raw);
        }
    }

    println!();
    println!("=== process args ===");
    for (i, arg) in std::env::args().enumerate() {
        println!("argv[{}]={}", i, arg);
    }

    println!();
    println!("=== environment (CLAUDE_*) ===");
    let mut claude_vars: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.to_uppercase().contains("CLAUDE"))
        .collect();
    claude_vars.sort();
    for (k, v) in &claude_vars {
        println!("{}={}", k, v);
    }
    if claude_vars.is_empty() {
        println!("(none)");
    }

    println!();
    println!("=== environment (selected) ===");
    for var in &["PWD", "USER", "HOME", "TERM", "SHELL", "LANG", "TERM_PROGRAM"] {
        if let Ok(val) = std::env::var(var) {
            println!("{}={}", var, val);
        }
    }

    println!();
    println!("=== process ===");
    println!("pid={}", std::process::id());
    if let Ok(cwd) = std::env::current_dir() {
        println!("cwd={}", cwd.display());
    }
}

fn dump_transcript_info(input: &serde_json::Value) {
    let Some(path) = input.get("transcript_path").and_then(|x| x.as_str()) else {
        println!("=== transcript ===");
        println!("(no transcript_path in input)");
        return;
    };
    println!("=== transcript: {} ===", path);
    match std::fs::metadata(path) {
        Ok(m) => {
            println!("size: {} bytes", m.len());
            if let Ok(modified) = m.modified() {
                if let Ok(d) = SystemTime::now().duration_since(modified) {
                    println!("modified: {}s ago", d.as_secs());
                }
            }
        }
        Err(e) => {
            println!("metadata error: {}", e);
            return;
        }
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        println!("(could not read file)");
        return;
    };

    let mut total_lines = 0usize;
    let mut parsed_lines = 0usize;
    use std::collections::BTreeMap;
    let mut top_keys: BTreeMap<String, usize> = BTreeMap::new();
    let mut entry_types: BTreeMap<String, usize> = BTreeMap::new();
    let mut usage_keys: BTreeMap<String, usize> = BTreeMap::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        total_lines += 1;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        parsed_lines += 1;
        if let Some(obj) = v.as_object() {
            for k in obj.keys() {
                *top_keys.entry(k.clone()).or_insert(0) += 1;
            }
        }
        if let Some(t) = v.get("type").and_then(|x| x.as_str()) {
            *entry_types.entry(t.to_string()).or_insert(0) += 1;
        }
        if let Some(usage) = v
            .get("message")
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.as_object())
        {
            for k in usage.keys() {
                *usage_keys.entry(k.clone()).or_insert(0) += 1;
            }
        }
    }
    println!("lines: {} (parsed: {})", total_lines, parsed_lines);
    println!("entry types: {:?}", entry_types);
    println!("top-level keys (count): {:?}", top_keys);
    println!("usage keys (count): {:?}", usage_keys);
}
