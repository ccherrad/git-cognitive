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
    let cwd_key = cwd_key.trim_start_matches('-').to_string();

    let project_dir = std::fs::read_dir(&projects_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().contains(&cwd_key))?
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

    let out = Command::new("date")
        .args(["-j", "-f", "%Y-%m-%dT%H:%M:%S", &ts[..19], "+%s"])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            return String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
        }
    }

    let out = Command::new("date").args(["-d", ts, "+%s"]).output();
    if let Ok(o) = out {
        if o.status.success() {
            return String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0);
        }
    }

    0
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
        };
    };

    let content = match std::fs::read_to_string(&session_path) {
        Ok(c) => c,
        Err(_) => {
            return Attribution {
                ai_attributed: false,
                attribution_pct: None,
                session_slice: vec![],
            }
        }
    };

    // Slice JSONL to the window (prev_commit_ts, commit_ts + 60s grace]
    let mut session_slice: Vec<String> = Vec::new();
    let mut agent_edits: Vec<(String, Vec<String>)> = Vec::new(); // (file, lines)

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

    if session_slice.is_empty() {
        return Attribution {
            ai_attributed: false,
            attribution_pct: None,
            session_slice: vec![],
        };
    }

    let diff = diff_added_lines(repo_path, sha);
    let total_added: usize = diff.values().map(|v| v.len()).sum();

    if total_added == 0 || agent_edits.is_empty() {
        return Attribution {
            ai_attributed: false,
            attribution_pct: None,
            session_slice,
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
        };
    }

    let pct = (matched as f32 / total_added as f32).clamp(0.0, 1.0);
    Attribution {
        ai_attributed: pct >= 0.3,
        attribution_pct: Some(pct),
        session_slice,
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
