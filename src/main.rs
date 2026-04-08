use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use serde::Deserialize;

// ANSI color codes — matches the existing context-bar.sh "blue" theme.
const RESET: &str = "\x1b[0m";
const GRAY: &str = "\x1b[38;5;245m";
const BAR_EMPTY: &str = "\x1b[38;5;238m";
const ACCENT: &str = "\x1b[38;5;74m";

const BAR_WIDTH: usize = 10;
const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
// Conservative pre-conversation estimate: system prompt + tools + memory + framing.
const BASELINE_TOKENS: u64 = 20_000;

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

    let parsed: Input = serde_json::from_str(&buf).unwrap_or_default();

    let model_name = parsed
        .model
        .display_name
        .as_deref()
        .or(parsed.model.id.as_deref())
        .unwrap_or("?");

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
    let max_k = max_context / 1000;

    let scan = parsed
        .transcript_path
        .as_deref()
        .and_then(scan_transcript)
        .unwrap_or_default();

    let (pct, pct_prefix) = compute_context_pct(scan.last_total_tokens, max_context);
    let bar = render_bar(pct);

    let mut out = String::new();
    out.push_str(ACCENT);
    out.push_str(model_name);
    out.push_str(GRAY);
    out.push_str(" | 📁");
    out.push_str(&dir);
    if let Some(g) = &git_info {
        out.push_str(" | 🔀");
        out.push_str(&g.branch);
        out.push(' ');
        out.push_str(&g.status);
    }
    out.push_str(" | ");
    out.push_str(&bar);
    out.push(' ');
    out.push_str(GRAY);
    out.push_str(pct_prefix);
    out.push_str(&format!("{}% of {}k tokens", pct, max_k));
    out.push_str(RESET);
    println!("{}", out);

    if let Some(msg) = scan.last_user_message {
        let plain_len = plain_status_len(model_name, &dir, git_info.as_ref(), pct, max_k);
        let display = truncate(&msg, plain_len);
        println!("💬 {}", display);
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

    let status = if file_count == 0 {
        format!("(0 files uncommitted, {})", sync_status)
    } else if file_count == 1 {
        // porcelain line is `XY filename`; strip the 3-char status prefix.
        let single = lines[0].get(3..).unwrap_or(lines[0]);
        format!("({} uncommitted, {})", single, sync_status)
    } else {
        format!("({} files uncommitted, {})", file_count, sync_status)
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
        "<1m ago".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
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

fn render_bar(pct: u64) -> String {
    let pct = pct as i64;
    let mut bar = String::new();
    for i in 0..BAR_WIDTH as i64 {
        let bar_start = i * 10;
        let progress = pct - bar_start;
        if progress >= 8 {
            bar.push_str(ACCENT);
            bar.push('█');
            bar.push_str(RESET);
        } else if progress >= 3 {
            bar.push_str(ACCENT);
            bar.push('▄');
            bar.push_str(RESET);
        } else {
            bar.push_str(BAR_EMPTY);
            bar.push('░');
            bar.push_str(RESET);
        }
    }
    bar
}

// --- formatting helpers ----------------------------------------------------

fn plain_status_len(model: &str, dir: &str, git: Option<&GitInfo>, pct: u64, max_k: u64) -> usize {
    let mut s = format!("{} | 📁{}", model, dir);
    if let Some(g) = git {
        s.push_str(&format!(" | 🔀{} {}", g.branch, g.status));
    }
    s.push_str(&format!(" | xxxxxxxxxx {}% of {}k tokens", pct, max_k));
    s.chars().count()
}

fn truncate(s: &str, max_len: usize) -> String {
    let count = s.chars().count();
    if count <= max_len {
        s.to_string()
    } else {
        let cutoff = max_len.saturating_sub(3);
        let truncated: String = s.chars().take(cutoff).collect();
        format!("{}...", truncated)
    }
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
