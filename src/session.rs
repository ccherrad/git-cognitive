use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub role: String,
    pub text: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCallSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub tool: String,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCapture {
    pub session_id: String,
    pub project: String,
    pub start_ts: String,
    pub end_ts: String,
    pub complete: bool,
    pub turns: Vec<ConversationTurn>,
    pub agent_line_counts: HashMap<String, usize>,
    pub human_line_counts: HashMap<String, usize>,
    pub files_touched: Vec<String>,
    pub matched_commits: Vec<String>,
}

#[derive(Debug)]
struct AgentEdit {
    file: String,
    new_lines: usize,
}

pub fn find_project_sessions_dir(repo_path: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let projects_dir = PathBuf::from(&home).join(".claude").join("projects");

    let cwd = repo_path.canonicalize().ok()?;
    let cwd_key = cwd.to_string_lossy().replace('/', "-");
    let cwd_key = cwd_key.trim_start_matches('-');

    for entry in std::fs::read_dir(&projects_dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(cwd_key) {
            return Some(entry.path());
        }
    }

    None
}

pub fn list_sessions(sessions_dir: &Path) -> Vec<PathBuf> {
    let mut sessions: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(sessions_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();

    sessions.sort_by(|a, b| a.0.cmp(&b.0));
    sessions.into_iter().map(|(_, p)| p).collect()
}

pub fn parse_session(path: &Path) -> Result<SessionCapture> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("Failed to read {:?}", path))?;

    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut turns: Vec<ConversationTurn> = Vec::new();
    let mut agent_edits: Vec<AgentEdit> = Vec::new();
    let mut start_ts = String::new();
    let mut end_ts = String::new();
    let mut project = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let record: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let ts = record
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if !ts.is_empty() {
            if start_ts.is_empty() {
                start_ts = ts.clone();
            }
            end_ts = ts.clone();
        }

        if project.is_empty() {
            if let Some(cwd) = record.get("cwd").and_then(|v| v.as_str()) {
                project = PathBuf::from(cwd)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
            }
        }

        match record.get("type").and_then(|v| v.as_str()) {
            Some("user") => {
                let text = extract_user_text(&record);
                if !text.is_empty() {
                    turns.push(ConversationTurn {
                        role: "human".to_string(),
                        text,
                        timestamp: ts,
                        tool_calls: vec![],
                    });
                }
            }
            Some("assistant") => {
                let (text, tool_calls, edits) = extract_assistant_content(&record);
                agent_edits.extend(edits);
                if !text.is_empty() || !tool_calls.is_empty() {
                    turns.push(ConversationTurn {
                        role: "agent".to_string(),
                        text,
                        timestamp: ts,
                        tool_calls,
                    });
                }
            }
            _ => {}
        }
    }

    let mut agent_line_counts: HashMap<String, usize> = HashMap::new();
    let mut files_touched: Vec<String> = Vec::new();

    for edit in &agent_edits {
        *agent_line_counts.entry(edit.file.clone()).or_insert(0) += edit.new_lines;
        if !files_touched.contains(&edit.file) {
            files_touched.push(edit.file.clone());
        }
    }

    let human_line_counts: HashMap<String, usize> = HashMap::new();

    Ok(SessionCapture {
        session_id,
        project,
        start_ts,
        end_ts,
        complete: true,
        turns,
        agent_line_counts,
        human_line_counts,
        files_touched,
        matched_commits: vec![],
    })
}

fn extract_user_text(record: &serde_json::Value) -> String {
    let content = record.pointer("/message/content");
    match content {
        Some(serde_json::Value::String(s)) => {
            let s = s.trim();
            if s.len() > 5 {
                return s.to_string();
            }
        }
        Some(serde_json::Value::Array(blocks)) => {
            let mut parts: Vec<String> = Vec::new();
            for block in blocks {
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        let text = text.trim();
                        if text.len() > 5 {
                            parts.push(text.to_string());
                        }
                    }
                }
            }
            if !parts.is_empty() {
                return parts.join("\n");
            }
        }
        _ => {}
    }
    String::new()
}

fn extract_assistant_content(
    record: &serde_json::Value,
) -> (String, Vec<ToolCallSummary>, Vec<AgentEdit>) {
    let blocks = match record.pointer("/message/content") {
        Some(serde_json::Value::Array(b)) => b,
        _ => return (String::new(), vec![], vec![]),
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallSummary> = Vec::new();
    let mut edits: Vec<AgentEdit> = Vec::new();

    for block in blocks {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    let t = t.trim();
                    if t.len() > 5 {
                        text_parts.push(t.to_string());
                    }
                }
            }
            Some("tool_use") => {
                let tool_name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);

                let file = input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                if let Some(ref f) = file {
                    match tool_name.as_str() {
                        "Write" => {
                            let content =
                                input.get("content").and_then(|v| v.as_str()).unwrap_or("");
                            let line_count = content.lines().count();
                            edits.push(AgentEdit {
                                file: normalize_path(f),
                                new_lines: line_count,
                            });
                        }
                        "Edit" => {
                            let new_string = input
                                .get("new_string")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let line_count = new_string.lines().count();
                            edits.push(AgentEdit {
                                file: normalize_path(f),
                                new_lines: line_count,
                            });
                        }
                        _ => {}
                    }
                }

                tool_calls.push(ToolCallSummary {
                    tool: tool_name,
                    file,
                });
            }
            _ => {}
        }
    }

    (text_parts.join("\n\n"), tool_calls, edits)
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

pub fn correlate_session_to_commits(
    repo_path: &Path,
    capture: &SessionCapture,
) -> Result<Vec<String>> {
    let start_secs = parse_ts_to_secs(&capture.start_ts);
    let end_secs = parse_ts_to_secs(&capture.end_ts);

    if start_secs == 0 || end_secs == 0 {
        return Ok(vec![]);
    }

    let out = std::process::Command::new("git")
        .current_dir(repo_path)
        .args([
            "log",
            "--format=%H %at",
            &format!("--after={}", start_secs.saturating_sub(300)),
            &format!("--before={}", end_secs + 300),
        ])
        .output()
        .context("Failed to run git log for session correlation")?;

    let commits = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            let sha = parts.next()?.to_string();
            let ts: u64 = parts.next()?.trim().parse().ok()?;
            if ts >= start_secs.saturating_sub(300) && ts <= end_secs + 300 {
                Some(sha)
            } else {
                None
            }
        })
        .collect();

    Ok(commits)
}

pub fn compute_human_lines(
    repo_path: &Path,
    commit_sha: &str,
    agent_line_counts: &HashMap<String, usize>,
) -> HashMap<String, usize> {
    let mut human_counts: HashMap<String, usize> = HashMap::new();

    let out = std::process::Command::new("git")
        .current_dir(repo_path)
        .args([
            "diff",
            &format!("{}^..{}", commit_sha, commit_sha),
            "--numstat",
        ])
        .output();

    if let Ok(o) = out {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let parts: Vec<&str> = line.splitn(3, '\t').collect();
            if parts.len() < 3 {
                continue;
            }
            let added: usize = parts[0].parse().unwrap_or(0);
            let file = parts[2].to_string();

            let agent = agent_line_counts.get(&file).copied().unwrap_or(0);
            let human = added.saturating_sub(agent);
            if human > 0 {
                human_counts.insert(file, human);
            }
        }
    }

    human_counts
}

pub fn compute_attribution_pct(
    agent_line_counts: &HashMap<String, usize>,
    human_line_counts: &HashMap<String, usize>,
) -> f32 {
    let agent_total: usize = agent_line_counts.values().sum();
    let human_total: usize = human_line_counts.values().sum();
    let total = agent_total + human_total;

    if total == 0 {
        return 0.0;
    }

    agent_total as f32 / total as f32
}

fn parse_ts_to_secs(ts: &str) -> u64 {
    if ts.is_empty() {
        return 0;
    }

    let out = std::process::Command::new("date")
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

    let out = std::process::Command::new("date")
        .args(["-d", ts, "+%s"])
        .output();

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

pub fn run_session_capture(repo_path: &Path, session_id: &str) -> Result<()> {
    let sessions_dir = find_project_sessions_dir(repo_path)
        .context("Could not find Claude Code sessions directory for this project")?;

    let session_path = sessions_dir.join(format!("{}.jsonl", session_id));
    if !session_path.exists() {
        anyhow::bail!("Session file not found: {}", session_path.display());
    }

    println!(
        "Parsing session {}...",
        &session_id[..8.min(session_id.len())]
    );
    let mut capture = parse_session(&session_path)?;

    let matched = correlate_session_to_commits(repo_path, &capture)?;
    capture.matched_commits = matched.clone();

    println!(
        "  {} turn(s), {} file(s) touched, {} commit(s) matched",
        capture.turns.len(),
        capture.files_touched.len(),
        matched.len()
    );

    let store = crate::cognitive_debt::DebtStore::open(repo_path)?;
    let db = crate::db::Database::init()?;

    for commit_sha in &matched {
        let human_counts = compute_human_lines(repo_path, commit_sha, &capture.agent_line_counts);
        let attribution_pct = compute_attribution_pct(&capture.agent_line_counts, &human_counts);

        let mut per_commit_capture = capture.clone();
        per_commit_capture.human_line_counts = human_counts;
        per_commit_capture.matched_commits = vec![commit_sha.clone()];

        store.write_session(commit_sha, &per_commit_capture)?;

        if let Ok(Some(mut item)) = store.read_activity(commit_sha) {
            item.ai_attributed = attribution_pct >= 0.5;
            item.attribution_pct = Some(attribution_pct);
            store.write_activity(&item)?;
            db.upsert_activity_item(&item)?;
            println!(
                "  {} attribution: {:.0}% agent",
                &commit_sha[..8.min(commit_sha.len())],
                attribution_pct * 100.0
            );
        }
    }

    store.commit()?;

    if matched.is_empty() {
        println!("  No commits matched this session's time window — session.json not written.");
    } else {
        println!("Session capture complete.");
    }

    Ok(())
}

pub fn run_session_capture_latest(repo_path: &Path) -> Result<()> {
    let sessions_dir = match find_project_sessions_dir(repo_path) {
        Some(d) => d,
        None => {
            return Ok(());
        }
    };

    let sessions = list_sessions(&sessions_dir);
    let latest = match sessions.last() {
        Some(p) => p,
        None => return Ok(()),
    };

    let session_id = latest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    if session_id.is_empty() {
        return Ok(());
    }

    run_session_capture(repo_path, &session_id)
}
