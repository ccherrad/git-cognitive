use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use crate::cognitive_debt::{
    detect_ai_attribution, now_rfc3339, ActivityItem, Classification, DebtStore, EndorsementStatus,
};
use crate::db::Database;
use crate::session::{attribute_commit, Attribution};
use crate::treesitter::{complexity_delta, detect_language, doc_gap_score};

const AI_FREE_ZONE_PATTERNS: &[&str] = &[
    "auth/",
    "authentication/",
    "authorization/",
    "payments/",
    "payment/",
    "billing/",
    "migrations/",
    "migration/",
    "schema",
    ".sql",
];

const DEPENDENCY_PATTERNS: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "go.sum",
    "poetry.lock",
    "Gemfile.lock",
    "requirements.txt",
];

#[derive(Debug)]
pub struct CommitInfo {
    pub sha: String,
    pub short_sha: String,
    #[allow(dead_code)]
    pub author: String,
    pub timestamp: u64,
    pub prev_timestamp: u64,
    pub message: String,
    pub files_changed: Vec<String>,
}

pub fn run_audit(
    repo_path: &Path,
    since_sha: Option<&str>,
    single_commit: Option<&str>,
    check_zombies: bool,
) -> Result<()> {
    let commits = if let Some(sha) = single_commit {
        vec![fetch_commit(repo_path, sha)?]
    } else {
        let since = since_sha
            .map(|s| s.to_string())
            .or_else(|| read_last_audit_sha(repo_path));
        fetch_commits_since(repo_path, since.as_deref())?
    };

    if commits.is_empty() && !check_zombies {
        println!("Nothing to audit — already up to date.");
        return Ok(());
    }

    let db = Database::init().context("Failed to initialize database")?;

    let store = DebtStore::open(repo_path).context("Failed to open debt store")?;

    let mut audited = 0usize;

    for commit in &commits {
        let (item, session_slice) = build_activity_item(repo_path, commit)?;
        store.write_activity(&item)?;
        store.write_session(&commit.sha, &session_slice)?;
        db.upsert_activity_item(&item)?;
        audited += 1;
        println!(
            "  {} [{}] {} (friction: {:.2})",
            &commit.short_sha, item.classification, item.title, item.cognitive_friction_score
        );
    }

    if check_zombies {
        let zombie_count = detect_zombies(repo_path, &store, &db)?;
        if zombie_count > 0 {
            println!("{} zombie(s) detected and flagged.", zombie_count);
        } else {
            println!("No zombies detected.");
        }
    }

    store.commit()?;

    if audited > 0 {
        write_last_audit_sha(repo_path, &commits.last().unwrap().sha)?;
    }

    if audited > 0 || check_zombies {
        println!("Audit complete — {} commit(s) processed.", audited);
    }

    Ok(())
}

fn build_activity_item(
    repo_path: &Path,
    commit: &CommitInfo,
) -> Result<(ActivityItem, Vec<String>)> {
    let Attribution {
        ai_attributed,
        attribution_pct,
        session_slice,
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

    let classification = classify_commit(commit, ai_attributed, attribution_pct);

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

    let friction = compute_friction_score(repo_path, commit)?;

    let endorsement_status = match &classification {
        Classification::Minor | Classification::DependencyUpdate => EndorsementStatus::Excluded,
        _ => EndorsementStatus::Unendorsed,
    };

    Ok((
        ActivityItem {
            id: commit.sha.clone(),
            branch: current_branch(repo_path),
            classification,
            title,
            summary,
            commits: vec![commit.sha.clone()],
            since_sha: commit.sha.clone(),
            until_sha: commit.sha.clone(),
            cognitive_friction_score: friction,
            ai_attributed,
            attribution_pct,
            zombie: false,
            endorsement_status,
            audited_at: now_rfc3339(),
        },
        session_slice,
    ))
}

fn classify_commit(
    commit: &CommitInfo,
    ai_attributed: bool,
    attribution_pct: Option<f32>,
) -> Classification {
    if commit.files_changed.iter().any(|f| is_dependency_file(f)) {
        return Classification::DependencyUpdate;
    }

    if commit.files_changed.iter().any(|f| is_ai_free_zone(f)) {
        return Classification::Risk;
    }

    let high_ai = attribution_pct.map(|p| p >= 0.7).unwrap_or(ai_attributed);

    let msg = commit.message.to_lowercase();

    if msg.starts_with("fix") || msg.starts_with("bug") {
        return Classification::BugFix;
    }
    if msg.starts_with("refactor") || msg.starts_with("chore") || msg.starts_with("cleanup") {
        if high_ai {
            return Classification::TechDebt;
        }
        return Classification::Refactor;
    }
    if msg.starts_with("feat") || msg.starts_with("add") || msg.starts_with("new") {
        if high_ai {
            return Classification::Risk;
        }
        return Classification::NewFeature;
    }
    if msg.starts_with("docs")
        || msg.starts_with("test")
        || msg.starts_with("ci")
        || msg.starts_with("style")
    {
        return Classification::Minor;
    }

    Classification::Other
}

fn is_ai_free_zone(file: &str) -> bool {
    let lower = file.to_lowercase();
    AI_FREE_ZONE_PATTERNS.iter().any(|p| lower.contains(p))
}

fn is_dependency_file(file: &str) -> bool {
    DEPENDENCY_PATTERNS
        .iter()
        .any(|p| file.ends_with(p) || file == *p)
}

fn compute_friction_score(repo_path: &Path, commit: &CommitInfo) -> Result<f32> {
    let complexity = compute_complexity_delta(repo_path, &commit.sha, &commit.files_changed);
    let doc_gap = compute_doc_gap(repo_path, &commit.sha, &commit.files_changed);
    let author_churn = compute_author_churn(repo_path, &commit.files_changed);

    let score = (complexity * 0.4) + (doc_gap * 0.4) + (author_churn * 0.2);
    Ok(score.clamp(0.0, 1.0))
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

fn fetch_file_at_parent(repo_path: &Path, sha: &str, file: &str) -> Option<String> {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["show", &format!("{}^:{}", sha, file)])
        .output()
        .ok()?;
    if out.status.success() {
        String::from_utf8(out.stdout).ok()
    } else {
        None
    }
}

fn compute_complexity_delta(repo_path: &Path, sha: &str, files: &[String]) -> f32 {
    let mut total_delta = 0.0f32;
    let mut count = 0usize;

    for file in files {
        let Some(lang) = detect_language(file) else {
            continue;
        };
        let new_src = fetch_file_at(repo_path, sha, file).unwrap_or_default();
        let old_src = fetch_file_at_parent(repo_path, sha, file).unwrap_or_default();
        total_delta += complexity_delta(&old_src, &new_src, &lang);
        count += 1;
    }

    if count == 0 {
        return 0.0;
    }
    (total_delta / count as f32).clamp(0.0, 1.0)
}

fn compute_doc_gap(repo_path: &Path, sha: &str, files: &[String]) -> f32 {
    let mut total = 0.0f32;
    let mut count = 0usize;

    for file in files {
        let Some(lang) = detect_language(file) else {
            continue;
        };
        let new_src = fetch_file_at(repo_path, sha, file).unwrap_or_default();
        total += doc_gap_score(&new_src, &lang);
        count += 1;
    }

    if count == 0 {
        return 0.0;
    }
    (total / count as f32).clamp(0.0, 1.0)
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

fn fetch_commits_since(repo_path: &Path, since_sha: Option<&str>) -> Result<Vec<CommitInfo>> {
    let range = match since_sha {
        Some(sha) => format!("{}..HEAD", sha),
        None => "HEAD~50..HEAD".to_string(),
    };

    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["log", &range, "--format=%H %ae %at %s", "--reverse"])
        .output()
        .context("Failed to run git log")?;

    if !out.status.success() {
        let out_all = Command::new("git")
            .current_dir(repo_path)
            .args(["log", "-50", "--format=%H %ae %at %s", "--reverse"])
            .output()
            .context("Failed to run git log")?;
        return parse_commit_log(repo_path, &String::from_utf8_lossy(&out_all.stdout));
    }

    parse_commit_log(repo_path, &String::from_utf8_lossy(&out.stdout))
}

fn fetch_commit(repo_path: &Path, sha: &str) -> Result<CommitInfo> {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["log", "-1", "--format=%H %ae %at", sha])
        .output()
        .context("Failed to run git log")?;

    if !out.status.success() {
        anyhow::bail!("Failed to fetch commit {}", sha);
    }

    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    if parts.len() < 3 {
        anyhow::bail!("Unexpected git log output: {}", line);
    }

    let full_sha = parts[0].to_string();
    let author = parts[1].to_string();
    let timestamp: u64 = parts[2].trim().parse().unwrap_or(0);
    let message = fetch_commit_message(repo_path, &full_sha)?;
    let files_changed = fetch_changed_files(repo_path, &full_sha)?;
    let prev_timestamp = fetch_parent_timestamp(repo_path, &full_sha);

    Ok(CommitInfo {
        short_sha: full_sha[..8.min(full_sha.len())].to_string(),
        sha: full_sha,
        author,
        timestamp,
        prev_timestamp,
        message,
        files_changed,
    })
}

fn parse_commit_log(repo_path: &Path, log: &str) -> Result<Vec<CommitInfo>> {
    let mut commits = Vec::new();

    for line in log.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(4, ' ').collect();
        if parts.len() < 3 {
            continue;
        }

        let full_sha = parts[0].to_string();
        let author = parts[1].to_string();
        let timestamp: u64 = parts[2].parse().unwrap_or(0);

        let message = fetch_commit_message(repo_path, &full_sha).unwrap_or_default();
        let files_changed = fetch_changed_files(repo_path, &full_sha).unwrap_or_default();
        let prev_timestamp = fetch_parent_timestamp(repo_path, &full_sha);

        commits.push(CommitInfo {
            short_sha: full_sha[..8.min(full_sha.len())].to_string(),
            sha: full_sha,
            author,
            timestamp,
            prev_timestamp,
            message,
            files_changed,
        });
    }

    Ok(commits)
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

const LAST_AUDIT_FILE: &str = ".git/cognitive-debt-last-audit";

fn read_last_audit_sha(repo_path: &Path) -> Option<String> {
    let path = repo_path.join(LAST_AUDIT_FILE);
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_last_audit_sha(repo_path: &Path, sha: &str) -> Result<()> {
    let path = repo_path.join(LAST_AUDIT_FILE);
    std::fs::write(path, sha).context("Failed to write last audit SHA")
}

pub fn detect_zombies(repo_path: &Path, store: &DebtStore, db: &Database) -> Result<usize> {
    let items = store.read_all_activity()?;
    let threshold_days = 30u64;
    let threshold_secs = threshold_days * 24 * 3600;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut zombie_count = 0usize;

    for mut item in items {
        if !item.ai_attributed {
            continue;
        }
        if matches!(
            item.endorsement_status,
            EndorsementStatus::Endorsed | EndorsementStatus::Excluded
        ) {
            continue;
        }
        if item.zombie {
            continue;
        }

        let commit_ts = fetch_commit_timestamp(repo_path, &item.until_sha).unwrap_or(0);
        if commit_ts == 0 || (now - commit_ts) < threshold_secs {
            continue;
        }

        let files = fetch_changed_files(repo_path, &item.until_sha).unwrap_or_default();
        let has_human_followup = check_human_followup(repo_path, &item.until_sha, &files)?;
        if has_human_followup {
            continue;
        }

        item.zombie = true;
        item.classification = Classification::TechDebt;

        store.write_activity(&item)?;
        db.upsert_activity_item(&item)?;

        println!(
            "  ZOMBIE {} [{}] {} — untouched {} days",
            &item.until_sha[..8.min(item.until_sha.len())],
            item.classification,
            item.title,
            (now - commit_ts) / 86400
        );

        zombie_count += 1;
    }

    Ok(zombie_count)
}

fn fetch_commit_timestamp(repo_path: &Path, sha: &str) -> Result<u64> {
    let out = Command::new("git")
        .current_dir(repo_path)
        .args(["log", "-1", "--format=%at", sha])
        .output()
        .context("Failed to fetch commit timestamp")?;
    let ts = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    Ok(ts)
}

fn check_human_followup(repo_path: &Path, since_sha: &str, files: &[String]) -> Result<bool> {
    if files.is_empty() {
        return Ok(false);
    }

    for file in files {
        let out = Command::new("git")
            .current_dir(repo_path)
            .args([
                "log",
                &format!("{}..HEAD", since_sha),
                "--format=%H",
                "--",
                file,
            ])
            .output()
            .context("Failed to run git log for followup check")?;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let commits: Vec<&str> = stdout
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();

        for commit_sha in commits {
            let msg_out = Command::new("git")
                .current_dir(repo_path)
                .args(["log", "-1", "--format=%B", commit_sha])
                .output();

            if let Ok(o) = msg_out {
                let msg = String::from_utf8_lossy(&o.stdout).to_lowercase();
                let (ai, _) = detect_ai_attribution(&msg);
                if !ai {
                    return Ok(true);
                }
            }
        }
    }

    Ok(false)
}
