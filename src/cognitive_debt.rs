use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

const DEBT_BRANCH: &str = "cognitive/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHotspot {
    pub file: String,
    pub complexity: u32,
    pub doc_gap: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitAudit {
    pub id: String,
    pub branch: String,
    pub title: String,
    pub summary: String,
    pub commits: Vec<String>,
    pub since_sha: String,
    pub until_sha: String,
    pub cognitive_friction_score: f32,
    pub ai_attributed: bool,
    pub attribution_pct: Option<f32>,
    pub lines_changed: u32,
    pub large_diff: bool,
    pub session_duration_secs: Option<u64>,
    pub fatigue: bool,
    pub zombie: bool,
    pub audited_at: String,
    pub hotspots: Vec<FileHotspot>,
}

fn shard_path(id: &str) -> String {
    let id = id.trim_start_matches('0');
    let id = if id.len() < 6 {
        &id[..id.len().min(6)]
    } else {
        &id[..6]
    };
    let chars: Vec<char> = id.chars().collect();
    let a: String = chars[..2.min(chars.len())].iter().collect();
    let b: String = chars[2..4.min(chars.len())].iter().collect();
    let rest: String = chars[4..].iter().collect();
    format!("{}/{}/{}", a, b, rest)
}

pub fn read_commit_audit_from_branch(repo_path: &Path, sha: &str) -> Result<Option<CommitAudit>> {
    let shard = shard_path(sha);
    let git_path = format!("cognitive/v1:{}/activity.json", shard);

    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["show", &git_path])
        .output()
        .context("Failed to run git show for activity item")?;

    if !out.status.success() {
        return Ok(None);
    }

    let item = serde_json::from_slice(&out.stdout).context("Failed to parse activity.json")?;
    Ok(Some(item))
}

pub fn read_session_slice_from_branch(repo_path: &Path, sha: &str) -> Result<Vec<String>> {
    let shard = shard_path(sha);
    let session_cache_path = repo_path.join(".git/cognitive-sessions").join(&shard).join("session.jsonl");

    if session_cache_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&session_cache_path) {
            return Ok(content
                .lines()
                .map(|l| l.to_string())
                .filter(|l| !l.is_empty())
                .collect());
        }
    }

    let git_path = format!("cognitive/v1:{}/session.jsonl", shard);

    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["show", &git_path])
        .output()
        .context("Failed to run git show for session slice")?;

    if !out.status.success() {
        return Ok(vec![]);
    }

    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn ensure_debt_branch(repo_path: &Path) -> Result<()> {
    let exists = Command::new("git")
        .current_dir(repo_path)
        .args(["rev-parse", "--verify", DEBT_BRANCH])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if exists {
        return Ok(());
    }

    let empty_tree = Command::new("git")
        .current_dir(repo_path)
        .args(["hash-object", "-t", "tree", "--stdin"])
        .stdin(std::process::Stdio::null())
        .output()
        .context("Failed to create empty tree")?;

    if !empty_tree.status.success() {
        anyhow::bail!(
            "Failed to create empty tree: {}",
            String::from_utf8_lossy(&empty_tree.stderr)
        );
    }

    let tree_sha = String::from_utf8_lossy(&empty_tree.stdout)
        .trim()
        .to_string();

    let commit = Command::new("git")
        .current_dir(repo_path)
        .args([
            "commit-tree",
            &tree_sha,
            "-m",
            "init: create cognitive-debt branch",
        ])
        .output()
        .context("Failed to create initial commit")?;

    if !commit.status.success() {
        anyhow::bail!(
            "Failed to create initial commit: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
    }

    let commit_sha = String::from_utf8_lossy(&commit.stdout).trim().to_string();

    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["branch", DEBT_BRANCH, &commit_sha])
        .output()
        .context("Failed to create cognitive-debt branch")?;

    if !out.status.success() {
        anyhow::bail!(
            "Failed to create cognitive-debt branch: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    Ok(())
}

pub struct DebtStore {
    repo_path: PathBuf,
    worktree_path: PathBuf,
}

impl DebtStore {
    pub fn open(repo_path: &Path) -> Result<Self> {
        ensure_debt_branch(repo_path)?;

        let worktree_path = repo_path.join(".git").join("debt-worktree");

        if worktree_path.exists() {
            Command::new("git")
                .current_dir(repo_path)
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    worktree_path.to_str().unwrap(),
                ])
                .output()
                .ok();
            std::fs::remove_dir_all(&worktree_path).ok();
            Command::new("git")
                .current_dir(repo_path)
                .args(["worktree", "prune"])
                .output()
                .ok();
        }

        let out = Command::new("git")
            .current_dir(repo_path)
            .args([
                "worktree",
                "add",
                "--no-checkout",
                worktree_path.to_str().unwrap(),
                DEBT_BRANCH,
            ])
            .output()
            .context("Failed to add debt worktree")?;

        if !out.status.success() {
            anyhow::bail!(
                "Failed to set up debt worktree: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        Command::new("git")
            .current_dir(&worktree_path)
            .args(["checkout", DEBT_BRANCH, "--", "."])
            .output()
            .ok();

        Ok(Self {
            repo_path: repo_path.to_path_buf(),
            worktree_path,
        })
    }

    pub fn write_audit(&self, item: &CommitAudit) -> Result<()> {
        let shard = shard_path(&item.id);
        let dir = self.worktree_path.join(&shard);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create shard dir {}", shard))?;

        let activity_path = dir.join("activity.json");
        let json =
            serde_json::to_string_pretty(item).context("Failed to serialize activity item")?;
        std::fs::write(&activity_path, json).context("Failed to write activity.json")?;

        Ok(())
    }

    pub fn write_session(&self, commit_sha: &str, slice: &[String]) -> Result<()> {
        if slice.is_empty() {
            return Ok(());
        }
        let shard = shard_path(commit_sha);
        let dir = self.worktree_path.join(&shard);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create shard dir {}", shard))?;

        let content = slice.join("\n") + "\n";
        std::fs::write(dir.join("session.jsonl"), content)
            .context("Failed to write session.jsonl")?;

        Ok(())
    }

    pub fn commit(self) -> Result<()> {
        Command::new("git")
            .current_dir(&self.worktree_path)
            .args(["add", "-A"])
            .output()
            .context("Failed to stage debt files")?;

        let status = Command::new("git")
            .current_dir(&self.worktree_path)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .context("Failed to check worktree status")?;

        if !status.success() {
            let head_sha = Command::new("git")
                .current_dir(&self.repo_path)
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());

            let message = format!("debt: update activity items from {}", head_sha);

            let out = Command::new("git")
                .current_dir(&self.worktree_path)
                .args(["commit", "-m", &message])
                .output()
                .context("Failed to commit to cognitive-debt branch")?;

            if !out.status.success() {
                anyhow::bail!(
                    "Failed to commit cognitive-debt branch: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }

        Command::new("git")
            .current_dir(&self.repo_path)
            .args([
                "worktree",
                "remove",
                "--force",
                self.worktree_path.to_str().unwrap(),
            ])
            .output()
            .context("Failed to remove debt worktree")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agent_attribution_percentage() {
        assert_eq!(
            parse_agent_attribution("feat: add thing\n\nAgent-Attribution: 75%"),
            Some(0.75)
        );
    }

    #[test]
    fn parse_agent_attribution_entire() {
        assert_eq!(
            parse_agent_attribution("Entire-Attribution: 100%"),
            Some(1.0)
        );
    }

    #[test]
    fn parse_agent_attribution_none() {
        assert_eq!(parse_agent_attribution("fix: normal commit"), None);
    }

    #[test]
    fn detect_ai_attribution_from_trailer() {
        let (ai, pct) = detect_ai_attribution("feat: thing\n\nAgent-Attribution: 80%");
        assert!(ai);
        assert_eq!(pct, Some(0.8));
    }

    #[test]
    fn detect_ai_attribution_co_authored_by_claude() {
        let (ai, pct) = detect_ai_attribution("fix: bug\n\nCo-Authored-By: Claude Sonnet");
        assert!(ai);
        assert_eq!(pct, None);
    }

    #[test]
    fn detect_ai_attribution_human_commit() {
        let (ai, pct) = detect_ai_attribution("refactor: clean up logic");
        assert!(!ai);
        assert_eq!(pct, None);
    }

    #[test]
    fn detect_ai_attribution_low_pct_not_attributed() {
        let (ai, pct) = detect_ai_attribution("fix: small tweak\n\nAgent-Attribution: 30%");
        assert!(!ai);
        assert_eq!(pct, Some(0.3));
    }

    #[test]
    fn shard_path_structure() {
        let s = shard_path("abcdef1234");
        assert_eq!(s, "ab/cd/ef");
    }

    #[test]
    fn now_rfc3339_format() {
        let ts = now_rfc3339();
        assert!(ts.contains('T'));
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.len(), 20);
    }
}

pub fn parse_agent_attribution(commit_message: &str) -> Option<f32> {
    for line in commit_message.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("Agent-Attribution:")
            .or_else(|| line.strip_prefix("Entire-Attribution:"))
        {
            let rest = rest.trim();
            if let Some(pct_str) = rest.split('%').next() {
                if let Ok(pct) = pct_str.trim().parse::<f32>() {
                    return Some(pct / 100.0);
                }
            }
        }
    }
    None
}

pub fn detect_ai_attribution(commit_message: &str) -> (bool, Option<f32>) {
    if let Some(pct) = parse_agent_attribution(commit_message) {
        return (pct >= 0.5, Some(pct));
    }

    let lower = commit_message.to_lowercase();
    let ai = [
        "generated by",
        "co-authored-by: claude",
        "co-authored-by: copilot",
        "cursor",
        "ai-generated",
    ]
    .iter()
    .any(|kw| lower.contains(kw));

    (ai, None)
}

pub fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = epoch_to_parts(secs);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, mi, s)
}

fn epoch_to_parts(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let s = secs % 60;
    let mins = secs / 60;
    let mi = mins % 60;
    let hours = mins / 60;
    let h = hours % 24;
    let days = hours / 24;

    let mut year = 1970u64;
    let mut remaining = days;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        year += 1;
    }

    let months = [
        31u64,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1u64;
    for days_in_month in &months {
        if remaining < *days_in_month {
            break;
        }
        remaining -= days_in_month;
        month += 1;
    }

    (year, month, remaining + 1, h, mi, s)
}

fn is_leap(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}
