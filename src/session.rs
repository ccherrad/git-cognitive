use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Find the active Claude Code JSONL for this project.
/// Returns the path to the most recently modified JSONL.
fn find_active_session(repo_path: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let projects_dir = PathBuf::from(&home).join(".claude").join("projects");

    let cwd = repo_path.canonicalize().ok()?;
    let cwd_key = cwd.to_string_lossy().replace('/', "-");

    let project_dir = std::fs::read_dir(&projects_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .find(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name == cwd_key || name.trim_start_matches('-') == cwd_key.trim_start_matches('-')
        })?
        .path();

    let mut sessions: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&project_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();

    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    sessions.into_iter().next().map(|(_, p)| p)
}

/// Parse ISO 8601 timestamp to unix seconds.
fn parse_iso_ts(ts: &str) -> u64 {
    if ts.len() < 19 {
        return 0;
    }

    let date = &ts[..10];
    let time = &ts[11..19];
    let tz = &ts[19..];

    let parts: Vec<u64> = date.split('-').filter_map(|p| p.parse().ok()).collect();
    let tparts: Vec<u64> = time.split(':').filter_map(|p| p.parse().ok()).collect();
    if parts.len() != 3 || tparts.len() != 3 {
        return 0;
    }

    let (y, m, d) = (parts[0] as i64, parts[1] as i64, parts[2] as i64);
    let (h, min, s) = (tparts[0] as i64, tparts[1] as i64, tparts[2] as i64);

    let days = days_from_epoch(y, m, d);
    let mut unix = days * 86400 + h * 3600 + min * 60 + s;

    // strip milliseconds e.g. ".318" before parsing timezone
    let tz = if tz.starts_with('.') {
        let rest = tz.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
        rest.trim()
    } else {
        tz.trim()
    };

    // parse timezone offset e.g. "+0200", "-0530", "+02:00", "Z", ""
    if tz == "Z" || tz.is_empty() {
        // UTC
    } else if tz.len() >= 5 {
        let sign: i64 = if tz.starts_with('-') { -1 } else { 1 };
        let tz_clean = tz[1..].replace(':', "");
        let tz_h: i64 = tz_clean[..2].parse().unwrap_or(0);
        let tz_m: i64 = tz_clean[2..4].parse().unwrap_or(0);
        unix -= sign * (tz_h * 3600 + tz_m * 60);
    }

    unix.max(0) as u64
}

fn days_from_epoch(y: i64, m: i64, d: i64) -> i64 {
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let a = y / 100;
    let b = 2 - a + a / 4;
    ((365.25 * (y + 4716) as f64) as i64) + ((30.6001 * (m + 1) as f64) as i64) + d + b
        - 1524
        - 2440588
}

fn normalize_path(path: &str) -> String {
    let p = Path::new(path);
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(rel) = p.strip_prefix(&cwd) {
            return rel.to_string_lossy().to_string();
        }
    }
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Get added lines per file from git diff.
fn diff_added_lines(repo_path: &Path, sha: &str) -> HashMap<String, Vec<String>> {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["diff", &format!("{}^..{}", sha, sha), "-U0"])
        .output();

    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return HashMap::new(),
    };

    let mut result: HashMap<String, Vec<String>> = HashMap::new();
    let mut current_file = String::new();

    for line in text.lines() {
        if let Some(stripped) = line.strip_prefix("+++ b/") {
            current_file = stripped.to_string();
        } else if line.starts_with('+') && !line.starts_with("+++") {
            let content = line[1..].trim().to_string();
            if !content.is_empty() {
                result
                    .entry(current_file.clone())
                    .or_default()
                    .push(content);
            }
        }
    }

    result
}

/// Result of attributing a commit against a session.
pub struct Attribution {
    pub ai_attributed: bool,
    pub attribution_pct: Option<f32>,
    /// The raw JSONL lines from the session that fall within this commit's window.
    pub session_slice: Vec<String>,
    /// Duration in seconds of the session window (first to last message timestamp).
    pub session_duration_secs: Option<u64>,
}

/// Attribute a commit to a Claude session.
///
/// 1. Find the active session JSONL in ~/.claude/projects/<project>/
/// 2. Slice it to the window (prev_commit_ts, commit_ts]
/// 3. Match agent Write/Edit lines against git diff lines by file+content
/// 4. Return attribution + the raw JSONL slice for storage
pub fn attribute_commit(
    repo_path: &Path,
    sha: &str,
    commit_ts: u64,
    prev_commit_ts: u64,
) -> Attribution {
    let Some(session_path) = find_active_session(repo_path) else {
        return Attribution {
            ai_attributed: false,
            attribution_pct: None,
            session_slice: vec![],
            session_duration_secs: None,
        };
    };

    let content = match std::fs::read_to_string(&session_path) {
        Ok(c) => c,
        Err(_) => {
            return Attribution {
                ai_attributed: false,
                attribution_pct: None,
                session_slice: vec![],
                session_duration_secs: None,
            }
        }
    };

    // Slice JSONL to the window (prev_commit_ts, commit_ts + 60s grace]
    let mut session_slice: Vec<String> = Vec::new();
    let mut agent_edits: Vec<(String, Vec<String>)> = Vec::new(); // (file, lines)
    let mut first_ts: Option<u64> = None;
    let mut last_ts: Option<u64> = None;

    for raw_line in content.lines() {
        let raw_line = raw_line.trim();
        if raw_line.is_empty() {
            continue;
        }

        let record: serde_json::Value = match serde_json::from_str(raw_line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let ts = parse_iso_ts(
            record
                .get("timestamp")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );

        if ts == 0 || ts <= prev_commit_ts || ts > commit_ts + 60 {
            continue;
        }

        if first_ts.is_none() {
            first_ts = Some(ts);
        }
        last_ts = Some(ts);

        session_slice.push(raw_line.to_string());

        if record.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }

        let blocks = match record.pointer("/message/content") {
            Some(serde_json::Value::Array(b)) => b,
            _ => continue,
        };

        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }

            let tool = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let input = match block.get("input") {
                Some(v) => v,
                None => continue,
            };

            let file = match input.get("file_path").and_then(|v| v.as_str()) {
                Some(f) => normalize_path(f),
                None => continue,
            };

            let written = match tool {
                "Write" => input.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                "Edit" => input
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                _ => continue,
            };

            let lines: Vec<String> = written
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();

            if !lines.is_empty() {
                agent_edits.push((file, lines));
            }
        }
    }

    let session_duration_secs = match (first_ts, last_ts) {
        (Some(f), Some(l)) if l > f => Some(l - f),
        _ => None,
    };

    if session_slice.is_empty() {
        return Attribution {
            ai_attributed: false,
            attribution_pct: None,
            session_slice: vec![],
            session_duration_secs: None,
        };
    }

    let diff = diff_added_lines(repo_path, sha);
    let total_added: usize = diff.values().map(|v| v.len()).sum();

    if total_added == 0 || agent_edits.is_empty() {
        return Attribution {
            ai_attributed: false,
            attribution_pct: None,
            session_slice,
            session_duration_secs,
        };
    }

    let diff_sets: HashMap<String, std::collections::HashSet<String>> = diff
        .into_iter()
        .map(|(f, lines)| (f, lines.into_iter().collect()))
        .collect();

    let mut matched = 0usize;
    for (file, lines) in &agent_edits {
        if let Some(diff_lines) = diff_sets.get(file) {
            for line in lines {
                if diff_lines.contains(line) {
                    matched += 1;
                }
            }
        }
    }

    if matched == 0 {
        return Attribution {
            ai_attributed: false,
            attribution_pct: None,
            session_slice,
            session_duration_secs,
        };
    }

    let pct = (matched as f32 / total_added as f32).clamp(0.0, 1.0);
    Attribution {
        ai_attributed: pct >= 0.3,
        attribution_pct: Some(pct),
        session_slice,
        session_duration_secs,
    }
}

/// Print the session slice stored for a commit.
pub fn run_show_session(repo_path: &Path, sha: &str) -> Result<()> {
    use crate::cognitive_debt::read_session_slice_from_branch;

    let lines = read_session_slice_from_branch(repo_path, sha)?;
    if lines.is_empty() {
        println!("No session slice stored for {}.", &sha[..8.min(sha.len())]);
        return Ok(());
    }

    println!("\n--- session slice for {} ---\n", &sha[..8.min(sha.len())]);
    for line in &lines {
        let record: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                println!("{}", line);
                continue;
            }
        };

        let role = record.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        let ts = record
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .get(..19)
            .unwrap_or("");

        match role {
            "user" => {
                let text = record
                    .pointer("/message/content")
                    .and_then(|v| match v {
                        serde_json::Value::String(s) => Some(s.clone()),
                        serde_json::Value::Array(arr) => {
                            let parts: Vec<&str> = arr
                                .iter()
                                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                                .collect();
                            if parts.is_empty() {
                                None
                            } else {
                                Some(parts.join(" "))
                            }
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    println!("[{}] human: {}", ts, &text[..120.min(text.len())]);
                }
            }
            "assistant" => {
                let blocks = match record.pointer("/message/content") {
                    Some(serde_json::Value::Array(b)) => b,
                    _ => continue,
                };
                for block in blocks {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            let text = block
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .trim();
                            if !text.is_empty() {
                                println!("[{}] agent: {}", ts, &text[..120.min(text.len())]);
                            }
                        }
                        Some("tool_use") => {
                            let tool = block.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let file = block
                                .pointer("/input/file_path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            println!("[{}] tool:  {} {}", ts, tool, file);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    println!();
    Ok(())
}
