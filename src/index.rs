use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use git_topology::{
    is_stale, read_cluster_map, run_index as topology_run_index, ClusterMap, EmbeddingConfig,
};

use crate::cognitive_debt::{
    detect_ai_attribution, now_rfc3339, ActivityItem, Classification, DebtStore, EndorsementStatus,
};
use crate::db::Database;
use crate::session::{attribute_commit, Attribution};
use crate::treesitter::{complexity_delta, detect_language, doc_gap_score};

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
    pub timestamp: u64,
    pub prev_timestamp: u64,
    pub message: String,
    pub files_changed: Vec<String>,
}

fn hydrate_cluster_map(repo_path: &Path) -> Result<ClusterMap> {
    if is_stale(repo_path) {
        if !EmbeddingConfig::is_provider_configured() {
            anyhow::bail!(
                "Topology index is missing or stale but topology.provider is not configured.\n\
                Run: git config topology.provider gemma\n\
                  or: git config topology.provider openai"
            );
        }
        println!("Topology index is stale — re-indexing...");
        let config = EmbeddingConfig::load_or_default()?;
        return topology_run_index(repo_path, config);
    }
    Ok(read_cluster_map(repo_path)?.unwrap_or_else(ClusterMap::empty))
}

pub fn run_index(
    repo_path: &Path,
    since_sha: Option<&str>,
    single_commit: Option<&str>,
    check_zombies: bool,
) -> Result<()> {
    let cluster_map = hydrate_cluster_map(repo_path)?;

    let commits = if let Some(sha) = single_commit {
        vec![fetch_commit(repo_path, sha)?]
    } else {
        let since = since_sha
            .map(|s| s.to_string())
            .or_else(|| read_last_audit_sha(repo_path));
        fetch_commits_since(repo_path, since.as_deref())?
    };

    if commits.is_empty() && !check_zombies {
        println!("Nothing to index — already up to date.");
        return Ok(());
    }

    let db = Database::init().context("Failed to initialize database")?;

    let store = DebtStore::open(repo_path).context("Failed to open debt store")?;

    let mut audited = 0usize;

    for commit in &commits {
        let (item, session_slice) = build_activity_item(repo_path, commit, &cluster_map)?;
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
        println!("Index complete — {} commit(s) processed.", audited);
    }

    Ok(())
}

fn build_activity_item(
    repo_path: &Path,
    commit: &CommitInfo,
    cluster_map: &ClusterMap,
) -> Result<(ActivityItem, Vec<String>)> {
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

    let classification = classify_commit(commit, ai_attributed, attribution_pct, cluster_map);

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
    let fatigue = ai_attributed && session_duration_secs.map(|d| d >= 3 * 3600).unwrap_or(false);

    let friction = compute_friction_score(repo_path, commit, large_diff, fatigue)?;

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
            lines_changed,
            large_diff,
            session_duration_secs,
            fatigue,
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
    cluster_map: &ClusterMap,
) -> Classification {
    if commit.files_changed.iter().any(|f| is_dependency_file(f)) {
        return Classification::DependencyUpdate;
    }

    if ai_attributed
        && !cluster_map
            .clusters_for_files(&commit.files_changed)
            .is_empty()
    {
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

fn is_dependency_file(file: &str) -> bool {
    DEPENDENCY_PATTERNS
        .iter()
        .any(|p| file.ends_with(p) || file == *p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_commit(message: &str, files: &[&str]) -> CommitInfo {
        CommitInfo {
            sha: "abcdef1234567890".to_string(),
            short_sha: "abcdef12".to_string(),
            timestamp: 0,
            prev_timestamp: 0,
            message: message.to_string(),
            files_changed: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn dependency_file_detection() {
        assert!(is_dependency_file("Cargo.lock"));
        assert!(is_dependency_file("package-lock.json"));
        assert!(is_dependency_file("go.sum"));
        assert!(is_dependency_file("frontend/yarn.lock"));
        assert!(!is_dependency_file("src/main.rs"));
    }

    #[test]
    fn classify_dependency_update() {
        let commit = make_commit("chore: bump deps", &["Cargo.lock"]);
        let map = ClusterMap::empty();
        assert_eq!(
            classify_commit(&commit, false, None, &map),
            Classification::DependencyUpdate
        );
    }

    #[test]
    fn classify_bug_fix() {
        let commit = make_commit("fix: null pointer in auth", &["src/auth.rs"]);
        let map = ClusterMap::empty();
        assert_eq!(
            classify_commit(&commit, false, None, &map),
            Classification::BugFix
        );
    }

    #[test]
    fn classify_new_feature_human() {
        let commit = make_commit("feat: add search endpoint", &["src/search.rs"]);
        let map = ClusterMap::empty();
        assert_eq!(
            classify_commit(&commit, false, None, &map),
            Classification::NewFeature
        );
    }

    #[test]
    fn classify_risk_high_ai_feat() {
        let commit = make_commit("feat: add payment flow", &["src/payments.rs"]);
        let map = ClusterMap::empty();
        assert_eq!(
            classify_commit(&commit, true, Some(0.9), &map),
            Classification::Risk
        );
    }

    #[test]
    fn classify_tech_debt_high_ai_refactor() {
        let commit = make_commit("refactor: extract helper", &["src/lib.rs"]);
        let map = ClusterMap::empty();
        assert_eq!(
            classify_commit(&commit, true, Some(0.8), &map),
            Classification::TechDebt
        );
    }

    #[test]
    fn classify_refactor_low_ai() {
        let commit = make_commit("refactor: clean up", &["src/lib.rs"]);
        let map = ClusterMap::empty();
        assert_eq!(
            classify_commit(&commit, false, Some(0.1), &map),
            Classification::Refactor
        );
    }

    #[test]
    fn classify_minor_docs() {
        let commit = make_commit("docs: update readme", &["README.md"]);
        let map = ClusterMap::empty();
        assert_eq!(
            classify_commit(&commit, false, None, &map),
            Classification::Minor
        );
    }

    #[test]
    fn classify_other() {
        let commit = make_commit("wip: something", &["src/foo.rs"]);
        let map = ClusterMap::empty();
        assert_eq!(
            classify_commit(&commit, false, None, &map),
            Classification::Other
        );
    }
}

fn compute_friction_score(
    repo_path: &Path,
    commit: &CommitInfo,
    large_diff: bool,
    fatigue: bool,
) -> Result<f32> {
    let complexity = compute_complexity_delta(repo_path, &commit.sha, &commit.files_changed);
    let doc_gap = compute_doc_gap(repo_path, &commit.sha, &commit.files_changed);
    let author_churn = compute_author_churn(repo_path, &commit.files_changed);

    let mut score = (complexity * 0.4) + (doc_gap * 0.4) + (author_churn * 0.2);
    if large_diff {
        score += 0.15;
    }
    if fatigue {
        score += 0.20;
    }
    Ok(score.clamp(0.0, 1.0))
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
        .args(["log", &range, "--format=%H %at %s", "--reverse"])
        .output()
        .context("Failed to run git log")?;

    if !out.status.success() {
        let out_all = Command::new("git")
            .current_dir(repo_path)
            .args(["log", "-50", "--format=%H %at %s", "--reverse"])
            .output()
            .context("Failed to run git log")?;
        return parse_commit_log(repo_path, &String::from_utf8_lossy(&out_all.stdout));
    }

    parse_commit_log(repo_path, &String::from_utf8_lossy(&out.stdout))
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

fn parse_commit_log(repo_path: &Path, log: &str) -> Result<Vec<CommitInfo>> {
    let mut commits = Vec::new();

    for line in log.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() < 2 {
            continue;
        }

        let full_sha = parts[0].to_string();
        let timestamp: u64 = parts[1].parse().unwrap_or(0);

        let message = fetch_commit_message(repo_path, &full_sha).unwrap_or_default();
        let files_changed = fetch_changed_files(repo_path, &full_sha).unwrap_or_default();
        let prev_timestamp = fetch_parent_timestamp(repo_path, &full_sha);

        commits.push(CommitInfo {
            short_sha: full_sha[..8.min(full_sha.len())].to_string(),
            sha: full_sha,
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

const LAST_INDEX_FILE: &str = ".git/cognitive-last-index";

fn read_last_audit_sha(repo_path: &Path) -> Option<String> {
    let path = repo_path.join(LAST_INDEX_FILE);
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_last_audit_sha(repo_path: &Path, sha: &str) -> Result<()> {
    let path = repo_path.join(LAST_INDEX_FILE);
    std::fs::write(path, sha).context("Failed to write last index SHA")
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
