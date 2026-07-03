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
const YELLOW: &str = "\x1b[38;5;179m";
const RED: &str = "\x1b[38;5;203m";

const BAR_WIDTH: usize = 10;
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
// Conservative pre-conversation estimate: system prompt + tools + memory + framing.
const BASELINE_TOKENS: u64 = 20_000;

// Width budget. Re-profiled 2026-07-03 on CC 2.1.200 after the "Spindrizzle"
// buddy was retired: a width sweep at term=101 (via COLUMNS) rendered fully up
// to w=97, so the hard right edge reserves 4 cells; we use 5 to balance the
// left/right margins. CC also no longer cascade-drops rows below a truncated
// one — overflow now clips each row individually — so FIRST_LINE_EXTRA is no
// longer a safety measure, only popup-dodging on the top row: 24 cells of
// slack keeps a modest top-row popup (e.g. the effort indicator) off our
// content, and a rare wide popup now clips only the trailing `model` segment.
const RIGHT_RESERVATION: usize = 5;
const FIRST_LINE_EXTRA: usize = 24;
const MIN_USABLE_WIDTH: usize = 20;
const FALLBACK_TERM_WIDTH: usize = 80;

// Diagnostic mode: if this marker file exists, ccbar emits a test pattern
// instead of the normal status line and logs every invocation. Toggle with
// `touch /tmp/ccbar_diag` / `rm /tmp/ccbar_diag` — no rebuild needed.
const DIAG_MARKER: &str = "/tmp/ccbar_diag";
const DIAG_LOG: &str = "/tmp/ccbar_diag.log";

// Cache-efficiency thresholds (input hit ratio). Provisional first-attempt
// values — calibrate against a real miss captured via the cache log below.
const CACHE_WARN: f64 = 0.98; // below → yellow
const CACHE_MISS: f64 = 0.80; // below → red MISS alert

// Passive calibration logger: if this marker exists, every normal render
// appends the latest turn's cache token breakdown to CACHE_LOG. Off by
// default; toggle with `touch /tmp/ccbar_cachelog` to collect samples
// (including any misses) without disturbing the rendered status line.
const CACHE_LOG_MARKER: &str = "/tmp/ccbar_cachelog";
const CACHE_LOG: &str = "/tmp/ccbar_cache.log";

#[derive(Debug, Deserialize, Default)]
struct Input {
    #[serde(default)]
    session_id: Option<String>,
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
    // CC computes this for us (2.1.x); primary source for the context bar.
    #[serde(default)]
    used_percentage: Option<f64>,
    // Latest assistant turn's token breakdown; source for the cache indicator.
    #[serde(default)]
    current_usage: Option<CurrentUsage>,
}

#[derive(Debug, Deserialize, Default)]
struct CurrentUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

struct GitInfo {
    branch: String,
    status: String,
}

#[derive(Default)]
struct TranscriptScan {
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
        run_diag_mode(&buf);
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

    let (pct, pct_prefix) = compute_context_pct(parsed.context_window.as_ref(), max_context);
    let bar = render_bar(pct);

    let current_usage = parsed
        .context_window
        .as_ref()
        .and_then(|cw| cw.current_usage.as_ref());
    if std::path::Path::new(CACHE_LOG_MARKER).exists() {
        if let Some(u) = current_usage {
            log_cache_sample(parsed.session_id.as_deref(), u);
        }
    }
    let cache_seg = current_usage.and_then(cache_segment);

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
    if let Some(c) = cache_seg {
        segments.push(c);
    }
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

    // Context tokens now come from stdin (`context_window.used_percentage` /
    // `current_usage`), so the transcript is scanned only for the last user
    // message shown on the 💬 line.
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

fn compute_context_pct(cw: Option<&ContextWindow>, max_context: u64) -> (u64, &'static str) {
    // Primary: the percentage CC computes for us.
    if let Some(p) = cw.and_then(|c| c.used_percentage).filter(|p| *p > 0.0) {
        return (p.round().min(100.0) as u64, "");
    }
    // Fallback: derive from the latest turn's token breakdown.
    if let Some(total) = cw
        .and_then(|c| c.current_usage.as_ref())
        .map(|u| u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens)
        .filter(|t| *t > 0)
    {
        return ((total * 100 / max_context).min(100), "");
    }
    // Last resort: conservative pre-conversation estimate.
    ((BASELINE_TOKENS * 100 / max_context).min(100), "~")
}

/// Fraction of the latest turn's input tokens served from the prompt cache.
/// `None` when the turn had no input tokens to classify.
fn cache_hit_ratio(u: &CurrentUsage) -> Option<f64> {
    let denom = u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens;
    if denom == 0 {
        None
    } else {
        Some(u.cache_read_input_tokens as f64 / denom as f64)
    }
}

/// Compact cache-efficiency indicator for the latest turn. Shows the input hit
/// ratio, quiet (gray) when warm; the label flips to a red `MISS` when the
/// ratio drops — which happens when the prompt-cache TTL lapsed and CC had to
/// re-create the whole prefix (cache_creation spikes, cache_read collapses).
fn cache_segment(u: &CurrentUsage) -> Option<String> {
    let ratio = cache_hit_ratio(u)?;
    let pct = (ratio * 100.0).round() as u64;
    let seg = if ratio >= CACHE_WARN {
        format!("{}hit {}%{}", GRAY, pct, RESET)
    } else if ratio >= CACHE_MISS {
        format!("{}hit {}%{}", YELLOW, pct, RESET)
    } else {
        format!("{}MISS {}%{}", RED, pct, RESET)
    };
    Some(seg)
}

/// Append the latest turn's cache token breakdown to CACHE_LOG (calibration).
/// Tagged with `session_id` so samples from parallel CC sessions (all appending
/// to the one shared log) can be separated when calibrating.
fn log_cache_sample(session_id: Option<&str>, u: &CurrentUsage) {
    use std::io::Write;
    let ratio = cache_hit_ratio(u).unwrap_or(0.0);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!(
        "ts={} sid={} read={} create={} input={} out={} ratio={:.4}\n",
        now,
        session_id.unwrap_or("?"),
        u.cache_read_input_tokens,
        u.cache_creation_input_tokens,
        u.input_tokens,
        u.output_tokens,
        ratio
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(CACHE_LOG)
    {
        let _ = f.write_all(line.as_bytes());
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
///
/// Primary source is the `COLUMNS` env var, which Claude Code exports into the
/// status-line subprocess (observed in CC 2.1.x). The older `/dev/tty` ioctl
/// path stopped working when CC dropped the controlling terminal for this
/// subprocess (open fails with ENXIO), but we keep it as a fallback for
/// environments that still provide one.
fn terminal_width() -> Option<u16> {
    if let Some(w) = std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|w| *w > 0)
    {
        return Some(w);
    }

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

/// ioctl(TIOCGWINSZ) on a raw fd, reporting the result or the OS errno.
fn ioctl_winsize(fd: i32) -> String {
    let mut ws = Winsize::default();
    let r = unsafe { ioctl(fd, TIOCGWINSZ, &mut ws as *mut _) };
    if r == 0 {
        format!("ok col={} row={}", ws.col, ws.row)
    } else {
        format!("FAIL rc={} errno={}", r, io::Error::last_os_error())
    }
}

/// Diagnostic dump of every terminal-width channel and its failure reason.
/// Written to the diag log so we can see, inside the real CC subprocess, which
/// source (if any) yields the width.
fn probe_width_sources() -> String {
    use std::os::fd::AsRawFd;
    let mut s = String::from("--- width sources ---\n");
    // ioctl on the three standard fds (may be pipes under CC).
    for (name, fd) in [("stdin(0)", 0), ("stdout(1)", 1), ("stderr(2)", 2)] {
        s.push_str(&format!("ioctl {:<9} {}\n", name, ioctl_winsize(fd)));
    }
    // ioctl on /dev/tty (the current strategy).
    match std::fs::File::open("/dev/tty") {
        Ok(f) => s.push_str(&format!("/dev/tty open ok -> {}\n", ioctl_winsize(f.as_raw_fd()))),
        Err(e) => s.push_str(&format!("/dev/tty open FAIL errno={}\n", e)),
    }
    // Environment channels.
    for var in ["COLUMNS", "LINES", "TERM"] {
        match std::env::var(var) {
            Ok(v) => s.push_str(&format!("env {}={}\n", var, v)),
            Err(_) => s.push_str(&format!("env {}=(unset)\n", var)),
        }
    }
    s
}

// --- diagnostic mode -------------------------------------------------------

// Reservations to sweep, in cells subtracted from the terminal width. Ordered
// large→small so rows go narrow→wide top→bottom: the LAST fully-visible row's
// `r=NN` label is the true right-reservation. Range brackets both the old
// buddy-era value (~21) and the expected post-buddy value (~2-6).
const DIAG_SWEEP: &[usize] = &[24, 18, 14, 12, 10, 8, 6, 5, 4, 3, 2, 1, 0];

fn run_diag_mode(stdin_raw: &str) {
    use std::io::Write;

    let term = terminal_width();
    let term_w = term.map(|w| w as usize).unwrap_or(FALLBACK_TERM_WIDTH);
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tag = now % 10_000;

    let mut emitted: Vec<String> = Vec::with_capacity(DIAG_SWEEP.len() + 1);

    // Row 1: short, always-fits anchor. Carries the measured terminal width and
    // a tag the user can read off-screen and quote back. Kept narrow so a row-1
    // popup can't clip it and cascade-drop the sweep below.
    emitted.push(format!(
        "DIAG term={} tag={:04}  read last full r= row",
        term_w, tag
    ));

    // Width sweep: each row is a ruler of exactly `term - NN` visible cells,
    // labeled `r=NN` on the left and `w=<width>|` flush-right. Scanning down,
    // the last row whose right `|` marker is intact fits the canvas; the first
    // `…`-truncated row (and everything below it) is cascade-dropped. The
    // ruler's tens-digit ticks show absolute columns, so the rightmost visible
    // tick cross-checks the reservation (reservation = term − rightmost col).
    for &nn in DIAG_SWEEP {
        emitted.push(sweep_row(nn, term_w.saturating_sub(nn)));
    }

    for line in &emitted {
        println!("{}", line);
    }

    // Append every invocation to the log so on-screen truncation can be
    // correlated with the exact bytes emitted (and their intended widths).
    let mut log = format!(
        "=== ts={} tag={:04} term={:?} pid={} ===\n",
        now,
        tag,
        term,
        std::process::id()
    );
    // Width-source probe: terminal_width() has been returning None under CC,
    // so log every candidate channel and its exact failure reason.
    log.push_str(&probe_width_sources());
    // Raw stdin: newer CC versions may carry width/columns in the JSON.
    log.push_str(&format!("--- stdin ({} bytes) ---\n{}\n", stdin_raw.len(), stdin_raw));
    log.push_str("--- rows ---\n");
    for line in &emitted {
        log.push_str(&format!("[{:>3}] {}\n", visible_width(line), line));
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

/// Build one sweep row of exactly `width` visible cells: `r=NN ` on the left,
/// ` w=<width>|` flush-right, and an absolute-column ruler filling the middle.
/// If `width` is too small to hold both labels, returns a best-effort short row.
fn sweep_row(nn: usize, width: usize) -> String {
    let left = format!("r={:02} ", nn);
    let right = format!(" w={}|", width);
    let left_w = visible_width(&left);
    let right_w = visible_width(&right);
    if width <= left_w + right_w {
        return truncate_to_width(&format!("r={:02} w={}", nn, width), width);
    }
    let mid_w = width - left_w - right_w;
    // Ticks reflect the absolute screen column (offset by the left label), so
    // the rightmost readable tens digit tells the user the true cutoff column.
    let mut mid = String::with_capacity(mid_w);
    for i in 0..mid_w {
        let col = left_w + i;
        if col % 10 == 0 {
            mid.push(char::from_digit(((col / 10) % 10) as u32, 10).unwrap());
        } else if col % 5 == 0 {
            mid.push('+');
        } else {
            mid.push('-');
        }
    }
    format!("{}{}{}", left, mid, right)
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
