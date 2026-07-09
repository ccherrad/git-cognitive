use crate::agent::Agent;
use crate::parse;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Min and max message timestamp inside a session JSONL.
/// Returns None if the file has no parseable record timestamps.
fn session_ts_range(path: &Path) -> Option<(u64, u64)> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut min: Option<u64> = None;
    let mut max: Option<u64> = None;

    for line in parse::parse_lines(content.lines()) {
        let ts = parse_iso_ts(&line.timestamp);
        if ts == 0 {
            continue;
        }
        min = Some(min.map_or(ts, |m| m.min(ts)));
        max = Some(max.map_or(ts, |m| m.max(ts)));
    }

    Some((min?, max?))
}

/// Find the Claude Code JSONL whose message window overlaps this commit.
///
/// Sessions are selected by their internal message timestamps, not file
/// mtime — an mtime can be bumped by a sync or checkout long after the
/// conversation ended. A session overlaps the commit window when it has any
/// message in `(prev_commit_ts, commit_ts + 60]`. If several overlap, the one
/// whose last message is closest to (but not past) the commit wins. If none
/// overlap, fall back to the session with the newest last message.
fn find_active_session(
    repo_path: &Path,
    commit_ts: u64,
    prev_commit_ts: u64,
    agents: &[Box<dyn Agent>],
) -> Option<PathBuf> {
    // Gather transcript candidates across every enabled agent's project store.
    let mut candidates: Vec<(u64, u64, PathBuf)> = Vec::new();
    for agent in agents {
        let Some(project_dir) = agent.project_dir(repo_path) else {
            continue;
        };
        let ext = agent.transcript_ext();
        let files = std::fs::read_dir(&project_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some(ext))
            .filter_map(|p| session_ts_range(&p).map(|(min, max)| (min, max, p)));
        candidates.extend(files);
    }

    let window_hi = commit_ts + 60;

    // Prefer a session that actually overlaps (prev_commit_ts, commit_ts + 60].
    let overlapping = candidates
        .iter()
        .filter(|(min, max, _)| *max > prev_commit_ts && *min <= window_hi)
        .max_by_key(|(_, max, _)| *max);

    if let Some((_, _, path)) = overlapping {
        return Some(path.clone());
    }

    // No overlap — fall back to the session with the newest last message.
    candidates
        .into_iter()
        .max_by_key(|(_, max, _)| *max)
        .map(|(_, _, p)| p)
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
/// 1. Find the active session transcript in an enabled agent's project store
/// 2. Slice it to the window (prev_commit_ts, commit_ts]
/// 3. Match agent Write/Edit lines against git diff lines by file+content
/// 4. Return attribution + the raw JSONL slice for storage
pub fn attribute_commit(
    repo_path: &Path,
    sha: &str,
    commit_ts: u64,
    prev_commit_ts: u64,
    agents: &[Box<dyn Agent>],
) -> Attribution {
    let Some(session_path) = find_active_session(repo_path, commit_ts, prev_commit_ts, agents)
    else {
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

        let record = match parse::parse_line(raw_line) {
            Some(r) => r,
            None => continue,
        };

        let ts = parse_iso_ts(&record.timestamp);

        if ts == 0 || ts <= prev_commit_ts || ts > commit_ts + 60 {
            continue;
        }

        if first_ts.is_none() {
            first_ts = Some(ts);
        }
        last_ts = Some(ts);

        session_slice.push(raw_line.to_string());

        if record.type_ != parse::TYPE_ASSISTANT {
            continue;
        }

        for block in record.content_blocks() {
            if !block.is_tool_use() {
                continue;
            }

            let file = match block.tool_file() {
                Some(f) => normalize_path(f),
                None => continue,
            };

            let written = match block.written_content() {
                Some(w) => w,
                None => continue,
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

    let records = parse::parse_lines(lines.iter());
    let modified = parse::extract_modified_files(&records);

    println!("\n--- session slice for {} ---\n", &sha[..8.min(sha.len())]);
    if !modified.is_empty() {
        println!("modified files ({}):", modified.len());
        for file in &modified {
            println!("  {}", file);
        }
        println!();
    }

    for record in &records {
        let ts = record.short_ts();

        match record.type_.as_str() {
            parse::TYPE_USER => {
                let text = record.user_text();
                if !text.is_empty() {
                    println!("[{}] human: {}", ts, text);
                }
            }
            parse::TYPE_ASSISTANT => {
                for block in record.content_blocks() {
                    if block.is_text() {
                        let text = block.text.trim();
                        if !text.is_empty() {
                            println!("[{}] agent: {}", ts, text);
                        }
                    } else if block.is_tool_use() {
                        let tool = if block.name.is_empty() { "?" } else { &block.name };
                        let file = block.tool_file().unwrap_or("");
                        println!("[{}] tool:  {} {}", ts, tool, file);
                    }
                }
            }
            _ => {}
        }
    }
    println!();
    Ok(())
}
