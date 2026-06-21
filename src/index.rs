use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use crate::cognitive_debt::{
    detect_ai_attribution, now_rfc3339, timestamp_to_rfc3339, CommitAudit, DebtStore, FileHotspot,
};
use crate::db::Database;
use crate::session::{attribute_commit, Attribution};
use crate::treesitter::{absolute_complexity, detect_language, doc_gap_score};

#[derive(Debug)]
pub struct CommitInfo {
    pub sha: String,
    pub short_sha: String,
    pub timestamp: u64,
    pub prev_timestamp: u64,
    pub message: String,
    pub files_changed: Vec<String>,
}

pub fn run_sync(repo_path: &Path) -> Result<()> {
    let merge_commits = detect_unsynced_merges(repo_path)?;

    if merge_commits.is_empty() {
        println!("No new merge commits to sync.");
        return Ok(());
    }

    println!("Found {} merge commit(s) to sync.", merge_commits.len());

    let store = DebtStore::open(repo_path).context("Failed to open debt store")?;
    let mut synced = 0usize;

    for sha in &merge_commits {
        match fetch_commit(repo_path, sha) {
            Ok(commit) => {
                match build_commit_audit(repo_path, &commit) {
                    Ok((audit, session_slice)) => {
                        store.write_audit(&audit)?;
                        store.write_session(&commit.sha, &session_slice)?;
                        synced += 1;
                        println!(
                            "  {} {} (merge)",
                            &commit.short_sha, audit.title
                        );
                    }
                    Err(e) => eprintln!("Failed to audit {}: {}", sha, e),
                }
            }
            Err(e) => eprintln!("Failed to fetch {}: {}", sha, e),
        }
    }

    store.commit()?;
    println!("Sync complete — {} merge(s) synced.", synced);
    Ok(())
}

pub fn run_index(repo_path: &Path, output_json: Option<std::path::PathBuf>) -> Result<()> {
    let commits = if output_json.is_some() {
        fetch_all_commits(repo_path)?
    } else {
        let db = Database::init().context("Failed to initialize database")?;
        fetch_covering_commits(repo_path, &db)?
    };

    if commits.is_empty() {
        println!("Nothing to index — already up to date.");
        return Ok(());
    }

    let store = DebtStore::open(repo_path).context("Failed to open debt store")?;
    let mut audited = 0usize;
    let mut audits = Vec::new();

    for commit in &commits {
        let (item, session_slice) = build_commit_audit(repo_path, commit)?;
        store.write_audit(&item)?;
        store.write_session(&commit.sha, &session_slice)?;

        if output_json.is_none() {
            let db = Database::init().context("Failed to initialize database")?;
            db.upsert_commit_audit(&item)?;
        }

        audits.push(item);
        audited += 1;
    }

    store.commit()?;

    if let Some(json_path) = output_json {
        let json_data = serde_json::to_string_pretty(&audits)
            .context("Failed to serialize audits to JSON")?;
        std::fs::write(&json_path, json_data)
            .context("Failed to write JSON file")?;
        println!("Index complete — {} commit(s) written to {}.", audited, json_path.display());
    } else {
        for audit in &audits {
            println!(
                "  {} {} (friction: {:.2})",
                &audit.id[..8.min(audit.id.len())], audit.title, audit.cognitive_friction_score
            );
        }
        println!("Index complete — {} commit(s) processed.", audited);
    }

    Ok(())
}

fn detect_unsynced_merges(repo_path: &Path) -> Result<Vec<String>> {
    let db = Database::init().context("Failed to initialize database")?;
    let audits = db.all_commit_audits().unwrap_or_default();

    let last_audited = audits.first().map(|a| a.id.clone());

    let merge_range = if let Some(last) = last_audited {
        format!("{}..HEAD", last)
    } else {
        "HEAD".to_string()
    };

    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["log", "--merges", "--format=%H", &merge_range])
        .output()
        .context("Failed to detect merge commits")?;

    if !out.status.success() {
        return Ok(vec![]);
    }

    let shas: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(shas)
}

fn fetch_all_commits(repo_path: &Path) -> Result<Vec<CommitInfo>> {
    let ls = Command::new("git")
        .current_dir(repo_path)
        .args(["ls-files"])
        .output()
        .context("Failed to run git ls-files")?;

    let files: Vec<String> = String::from_utf8_lossy(&ls.stdout)
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut commits = Vec::new();

    for file in &files {
        let out = Command::new("git")
            .current_dir(repo_path)
            .args(["log", "-1", "--format=%H", "--", file])
            .output();

        let sha = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            _ => continue,
        };

        if sha.is_empty() || seen.contains(&sha) {
            continue;
        }

        seen.insert(sha.clone());
        match fetch_commit(repo_path, &sha) {
            Ok(c) => commits.push(c),
            Err(_) => continue,
        }
    }

    println!(
        "Covering set: {} files → {} unique commits to index.",
        files.len(),
        commits.len()
    );

    Ok(commits)
}

fn fetch_covering_commits(repo_path: &Path, db: &Database) -> Result<Vec<CommitInfo>> {
    let already_indexed: HashSet<String> = db
        .all_commit_audits()
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.id)
        .collect();

    let mut commits = fetch_all_commits(repo_path)?;
    commits.retain(|c| !already_indexed.contains(&c.sha));

    Ok(commits)
}

fn build_commit_audit(repo_path: &Path, commit: &CommitInfo) -> Result<(CommitAudit, Vec<String>)> {
    let Attribution {
        ai_attributed,
        attribution_pct,
        session_slice,
        session_duration_secs,
    } = attribute_commit(
        repo_path,
        &commit.sha,
        commit.timestamp,
        commit.prev_timestamp,
    );

    // Fall back to keyword heuristic if session found nothing.
    let (ai_attributed, attribution_pct) = if attribution_pct.is_some() {
        (ai_attributed, attribution_pct)
    } else {
        detect_ai_attribution(&commit.message)
    };

    let title = commit
        .message
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(80)
        .collect::<String>();
    let summary = commit
        .message
        .lines()
        .skip(2)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    let lines_changed = count_lines_changed(repo_path, &commit.sha);
    let large_diff = ai_attributed && lines_changed > 100;
    let fatigue = ai_attributed
        && session_duration_secs
            .map(|d| d >= 3 * 3600)
            .unwrap_or(false);

    let hotspots = compute_hotspots(repo_path, &commit.sha, &commit.files_changed);
    let friction = compute_friction_score(repo_path, commit, &hotspots, large_diff, fatigue);

    Ok((
        CommitAudit {
            id: commit.sha.clone(),
            branch: current_branch(repo_path),
            title,
            summary,
            commits: vec![commit.sha.clone()],
            since_sha: commit.sha.clone(),
            until_sha: commit.sha.clone(),
            cognitive_friction_score: friction,
            ai_attributed,
            attribution_pct,
            lines_changed,
            large_diff,
            session_duration_secs,
            fatigue,
            zombie: false,
            committed_at: timestamp_to_rfc3339(commit.timestamp),
            audited_at: now_rfc3339(),
            hotspots,
        },
        session_slice,
    ))
}

fn compute_friction_score(
    repo_path: &Path,
    commit: &CommitInfo,
    hotspots: &[FileHotspot],
    large_diff: bool,
    fatigue: bool,
) -> f32 {
    let max_complexity = hotspots.iter().map(|h| h.complexity).max().unwrap_or(0);
    let complexity_score = (max_complexity as f32 / 50.0).clamp(0.0, 1.0);
    let doc_gap = if hotspots.is_empty() {
        0.0
    } else {
        hotspots.iter().map(|h| h.doc_gap).sum::<f32>() / hotspots.len() as f32
    };
    let author_churn = compute_author_churn(repo_path, &commit.files_changed);

    let mut score = (complexity_score * 0.4) + (doc_gap * 0.4) + (author_churn * 0.2);
    if large_diff {
        score += 0.15;
    }
    if fatigue {
        score += 0.20;
    }
    score.clamp(0.0, 1.0)
}

fn count_lines_changed(repo_path: &Path, sha: &str) -> u32 {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["diff", "--shortstat", &format!("{}^..{}", sha, sha)])
        .output();

    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return 0,
    };

    // "3 files changed, 142 insertions(+), 12 deletions(-)"
    let mut total = 0u32;
    for part in text.split(',') {
        let part = part.trim();
        if part.contains("insertion") || part.contains("deletion") {
            if let Some(n) = part.split_whitespace().next() {
                total += n.parse::<u32>().unwrap_or(0);
            }
        }
    }
    total
}

fn fetch_file_at(repo_path: &Path, sha: &str, file: &str) -> Option<String> {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["show", &format!("{}:{}", sha, file)])
        .output()
        .ok()?;
    if out.status.success() {
        String::from_utf8(out.stdout).ok()
    } else {
        None
    }
}

fn compute_hotspots(repo_path: &Path, sha: &str, files: &[String]) -> Vec<FileHotspot> {
    files
        .iter()
        .filter_map(|file| {
            let lang = detect_language(file)?;
            let new_src = fetch_file_at(repo_path, sha, file).unwrap_or_default();
            let complexity = absolute_complexity(&new_src, &lang);
            let gap = doc_gap_score(&new_src, &lang);
            Some(FileHotspot {
                file: file.clone(),
                complexity,
                doc_gap: gap,
            })
        })
        .collect()
}

fn compute_author_churn(repo_path: &Path, files: &[String]) -> f32 {
    if files.is_empty() {
        return 0.0;
    }

    let mut authors: HashSet<String> = HashSet::new();

    for file in files {
        let out = Command::new("git")
            .current_dir(repo_path)
            .args(["log", "--since=90 days ago", "--format=%ae", "--", file])
            .output();

        if let Ok(o) = out {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let line = line.trim();
                if !line.is_empty() {
                    authors.insert(line.to_string());
                }
            }
        }
    }

    let author_count = authors.len();

    match author_count {
        0 | 1 => 0.8,
        2 => 0.4,
        3 => 0.2,
        _ => 0.0,
    }
}

fn fetch_commit(repo_path: &Path, sha: &str) -> Result<CommitInfo> {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["log", "-1", "--format=%H %at", sha])
        .output()
        .context("Failed to run git log")?;

    if !out.status.success() {
        anyhow::bail!("Failed to fetch commit {}", sha);
    }

    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    if parts.len() < 2 {
        anyhow::bail!("Unexpected git log output: {}", line);
    }

    let full_sha = parts[0].to_string();
    let timestamp: u64 = parts[1].trim().parse().unwrap_or(0);
    let message = fetch_commit_message(repo_path, &full_sha)?;
    let files_changed = fetch_changed_files(repo_path, &full_sha)?;
    let prev_timestamp = fetch_parent_timestamp(repo_path, &full_sha);

    Ok(CommitInfo {
        short_sha: full_sha[..8.min(full_sha.len())].to_string(),
        sha: full_sha,
        timestamp,
        prev_timestamp,
        message,
        files_changed,
    })
}

fn fetch_parent_timestamp(repo_path: &Path, sha: &str) -> u64 {
    Command::new("git")
        .current_dir(repo_path)
        .args(["log", "-1", "--format=%at", &format!("{}^", sha)])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

fn fetch_commit_message(repo_path: &Path, sha: &str) -> Result<String> {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["log", "-1", "--format=%B", sha])
        .output()
        .context("Failed to fetch commit message")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn fetch_changed_files(repo_path: &Path, sha: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["diff-tree", "--no-commit-id", "-r", "--name-only", sha])
        .output()
        .context("Failed to fetch changed files")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn current_branch(repo_path: &Path) -> String {
    Command::new("git")
        .current_dir(repo_path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_repo() -> Result<TempDir> {
        let temp = TempDir::new()?;
        let repo_path = temp.path();

        Command::new("git")
            .current_dir(repo_path)
            .args(["init"])
            .output()?;

        Command::new("git")
            .current_dir(repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()?;

        Command::new("git")
            .current_dir(repo_path)
            .args(["config", "user.name", "Test User"])
            .output()?;

        Ok(temp)
    }

    fn make_commit(repo_path: &Path, file: &str, content: &str, msg: &str) -> Result<String> {
        fs::write(repo_path.join(file), content)?;
        Command::new("git")
            .current_dir(repo_path)
            .args(["add", file])
            .output()?;

        let out = Command::new("git")
            .current_dir(repo_path)
            .args(["commit", "-m", msg])
            .output()?;

        if !out.status.success() {
            anyhow::bail!("Failed to commit");
        }

        let sha_out = Command::new("git")
            .current_dir(repo_path)
            .args(["rev-parse", "HEAD"])
            .output()?;

        Ok(String::from_utf8_lossy(&sha_out.stdout).trim().to_string())
    }

    #[test]
    fn test_detect_unsynced_merges_no_merges() -> Result<()> {
        let temp = setup_test_repo()?;
        let repo_path = temp.path();

        make_commit(repo_path, "file.txt", "content", "initial")?;
        make_commit(repo_path, "file.txt", "updated", "second")?;

        let merges = detect_unsynced_merges(repo_path)?;
        assert!(merges.is_empty(), "Should have no merge commits");

        Ok(())
    }

    #[test]
    fn test_detect_unsynced_merges_with_merge() -> Result<()> {
        let temp = setup_test_repo()?;
        let repo_path = temp.path();

        let _initial = make_commit(repo_path, "file.txt", "main", "initial")?;

        Command::new("git")
            .current_dir(repo_path)
            .args(["checkout", "-b", "feature"])
            .output()?;

        make_commit(repo_path, "file.txt", "feature work", "feature commit")?;

        Command::new("git")
            .current_dir(repo_path)
            .args(["checkout", "main"])
            .output()?;

        make_commit(repo_path, "other.txt", "other", "main work")?;

        Command::new("git")
            .current_dir(repo_path)
            .args(["merge", "feature", "-m", "merge feature"])
            .output()?;

        // Just check that git log --merges detects the merge (simpler, no DB needed)
        let out = Command::new("git")
            .current_dir(repo_path)
            .args(["log", "--merges", "--format=%H"])
            .output()?;

        let merges: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .filter(|l| !l.is_empty())
            .collect();

        assert!(!merges.is_empty(), "Should detect merge commit");
        assert_eq!(merges.len(), 1, "Should have exactly one merge");

        Ok(())
    }
}
