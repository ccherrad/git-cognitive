mod audit;
mod cognitive_debt;
mod db;
mod picker;
mod session;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "git-cognitive")]
#[command(
    version,
    about = "Cognitive debt detection and management for Git repositories"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Audit commits for cognitive debt")]
    Audit {
        #[arg(long, help = "Audit a specific commit SHA or HEAD")]
        commit: Option<String>,

        #[arg(long, help = "Audit all commits since this SHA")]
        since: Option<String>,

        #[arg(long, help = "Backfill all history (last 500 commits)")]
        all: bool,

        #[arg(long, help = "Scan for zombie AI commits (>30 days unendorsed)")]
        check_zombies: bool,
    },

    #[command(about = "Endorse an activity item as reviewed or understood")]
    Endorse {
        #[arg(help = "Commit SHA or HEAD (omit for interactive picker)")]
        sha: Option<String>,

        #[arg(
            long,
            default_value = "endorsed",
            help = "Endorsement status: reviewed | endorsed"
        )]
        status: String,
    },

    #[command(about = "Show cognitive debt heatmap by subsystem")]
    Debt {
        #[arg(long, help = "Filter by subsystem name")]
        subsystem: Option<String>,

        #[arg(long, help = "Open interactive picker to endorse items")]
        interactive: bool,

        #[arg(
            long,
            help = "Show knowledge concentration (who endorsed what per subsystem)"
        )]
        who: bool,
    },

    #[command(about = "Show activity item details and endorsement history for a commit")]
    Show {
        #[arg(help = "Commit SHA or HEAD")]
        sha: String,
    },

    #[command(about = "Capture Claude Code session for AI attribution")]
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },

    #[command(about = "Enable a coding agent for this project (e.g. claude)")]
    Enable {
        #[arg(help = "Agent to enable: claude")]
        agent: String,
    },
}

#[derive(Subcommand)]
enum SessionAction {
    #[command(about = "Capture a specific session by ID (omit for latest)")]
    Capture {
        #[arg(
            long,
            help = "Session ID (UUID from ~/.claude/projects/<project>/<id>.jsonl)"
        )]
        session_id: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Audit {
            commit,
            since,
            all,
            check_zombies,
        } => {
            let repo_path = PathBuf::from(".");
            let since = if all {
                Some("HEAD~500".to_string())
            } else {
                since
            };
            audit::run_audit(
                &repo_path,
                since.as_deref(),
                commit.as_deref(),
                check_zombies,
            )?;
        }
        Commands::Endorse { sha, status } => {
            endorse_command(sha.as_deref(), &status)?;
        }
        Commands::Debt {
            subsystem,
            interactive,
            who,
        } => {
            if interactive {
                debt_interactive(subsystem.as_deref())?;
            } else if who {
                debt_who_command(subsystem.as_deref())?;
            } else {
                debt_command(subsystem.as_deref())?;
            }
        }
        Commands::Show { sha } => {
            let resolved = resolve_sha(&sha)?;
            show_command(&resolved)?;
        }
        Commands::Session { action } => {
            let repo_path = PathBuf::from(".");
            match action {
                SessionAction::Capture { session_id } => match session_id {
                    Some(id) => session::run_session_capture(&repo_path, &id)?,
                    None => session::run_session_capture_latest(&repo_path)?,
                },
            }
        }
        Commands::Enable { agent } => match agent.as_str() {
            "claude" => enable_claude()?,
            other => anyhow::bail!("Unknown agent '{}'. Supported: claude", other),
        },
    }

    Ok(())
}

fn resolve_sha(sha: &str) -> Result<String> {
    if sha == "HEAD" {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .context("Failed to resolve HEAD")?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Ok(sha.to_string())
    }
}

fn do_endorse(sha: &str, status_str: &str) -> Result<()> {
    use cognitive_debt::{DebtStore, EndorsementRecord, EndorsementStatus};

    let status = match status_str {
        "reviewed" => EndorsementStatus::Reviewed,
        "endorsed" => EndorsementStatus::Endorsed,
        other => anyhow::bail!("Unknown status '{}'. Use: reviewed | endorsed", other),
    };

    let author = std::process::Command::new("git")
        .args(["config", "user.email"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let repo_path = PathBuf::from(".");
    let store = DebtStore::open(&repo_path)?;
    let record = EndorsementRecord {
        sha: sha.to_string(),
        status,
        author,
        timestamp: cognitive_debt::now_rfc3339(),
    };

    store.write_endorsement(&record)?;
    let db = db::Database::init()?;
    db.insert_endorsement(&record)?;
    store.commit()?;

    std::process::Command::new("git")
        .args(["push", "origin", "cognitive-debt/v1"])
        .output()
        .ok();

    println!("Endorsed {} as '{}'.", &sha[..8.min(sha.len())], status_str);
    Ok(())
}

fn endorse_command(sha: Option<&str>, status_str: &str) -> Result<()> {
    match sha {
        Some(s) => {
            let resolved = resolve_sha(s)?;
            do_endorse(&resolved, status_str)
        }
        None => {
            loop {
                let db = db::Database::init()?;
                let items = db.all_activity_items()?;
                let picker_items = picker::build_picker_items(&items, true);

                if picker_items.is_empty() {
                    println!("No unendorsed items remaining.");
                    break;
                }

                match picker::run_picker(picker_items)? {
                    None => break,
                    Some(result) => {
                        let (sha, status) = if let Some(rest) = result.strip_prefix("reviewed:") {
                            (rest.to_string(), "reviewed")
                        } else {
                            (result, status_str)
                        };
                        do_endorse(&sha, status)?;
                    }
                }
            }
            Ok(())
        }
    }
}

fn show_command(sha: &str) -> Result<()> {
    let repo_path = PathBuf::from(".");

    let item = cognitive_debt::read_activity_from_branch(&repo_path, sha)?;
    let endorsements = cognitive_debt::read_endorsements_from_branch(&repo_path, sha)?;

    match item {
        None => {
            println!("No activity item found for {}.", &sha[..8.min(sha.len())]);
            println!(
                "Run `git-cognitive audit --commit {}` first.",
                &sha[..8.min(sha.len())]
            );
        }
        Some(item) => {
            println!();
            println!("  commit   {}", item.id);
            println!("  branch   {}", item.branch);
            println!("  title    {}", item.title);
            if !item.summary.is_empty() {
                println!("  summary  {}", item.summary);
            }
            println!();
            println!("  class    {}", item.classification);
            println!("  subsys   {}", item.subsystem);
            println!("  friction {:.2}", item.cognitive_friction_score);
            if let Some(pct) = item.attribution_pct {
                println!("  agent    {:.0}%", pct * 100.0);
            }
            println!("  zombie   {}", if item.zombie { "yes" } else { "no" });
            println!("  status   {}", item.endorsement_status);
            println!("  audited  {}", item.audited_at);
            println!();

            if endorsements.is_empty() {
                println!("  No endorsements yet.");
            } else {
                println!("  Endorsements ({}):", endorsements.len());
                for e in &endorsements {
                    println!("    {}  {}  {}", e.timestamp, e.status, e.author);
                }
            }
            println!();
        }
    }

    Ok(())
}

fn debt_who_command(subsystem_filter: Option<&str>) -> Result<()> {
    use std::collections::{HashMap, HashSet};

    let db = db::Database::init().context("Failed to initialize database")?;
    let items = db.all_activity_items()?;

    if items.is_empty() {
        println!("No activity items. Run `git-cognitive audit` first.");
        return Ok(());
    }

    let repo_path = PathBuf::from(".");

    let items: Vec<_> = if let Some(filter) = subsystem_filter {
        items
            .into_iter()
            .filter(|i| i.subsystem == filter)
            .collect()
    } else {
        items
    };

    let mut subsystem_endorsers: HashMap<String, HashSet<String>> = HashMap::new();
    let mut subsystem_last: HashMap<String, String> = HashMap::new();
    let mut subsystem_items: HashMap<String, usize> = HashMap::new();

    for item in &items {
        *subsystem_items.entry(item.subsystem.clone()).or_insert(0) += 1;

        let endorsements =
            cognitive_debt::read_endorsements_from_branch(&repo_path, &item.id).unwrap_or_default();

        for e in &endorsements {
            subsystem_endorsers
                .entry(item.subsystem.clone())
                .or_default()
                .insert(e.author.clone());

            let last = subsystem_last.entry(item.subsystem.clone()).or_default();
            if e.timestamp > *last {
                *last = e.timestamp.clone();
            }
        }
    }

    println!(
        "\n{:<20} {:<7} {:<6} {:<40} LAST ENDORSED",
        "SUBSYSTEM", "ITEMS", "BUS", "ENDORSERS"
    );
    println!("{}", "-".repeat(85));

    let mut subsystems: Vec<&String> = subsystem_items.keys().collect();
    subsystems.sort();

    for name in subsystems {
        let items_count = subsystem_items[name];
        let endorsers = subsystem_endorsers.get(name).cloned().unwrap_or_default();
        let bus_factor = endorsers.len();
        let last = subsystem_last
            .get(name)
            .cloned()
            .unwrap_or_else(|| "-".to_string());
        let last_short = if last.len() > 10 { &last[..10] } else { &last };

        let bus_display = if bus_factor == 0 {
            format!("\x1B[31m{}\x1B[0m", bus_factor)
        } else if bus_factor == 1 {
            format!("\x1B[33m{}\x1B[0m", bus_factor)
        } else {
            format!("\x1B[32m{}\x1B[0m", bus_factor)
        };

        let endorsers_list = if endorsers.is_empty() {
            "\x1B[31mnone\x1B[0m".to_string()
        } else {
            endorsers.into_iter().collect::<Vec<_>>().join(", ")
        };

        let bus_pad = if bus_display.contains('\x1B') {
            6 + 9
        } else {
            6
        };

        println!(
            "{:<20} {:<7} {:<bus_pad$} {:<40} {}",
            &name[..20.min(name.len())],
            items_count,
            bus_display,
            &endorsers_list[..40.min(endorsers_list.len())],
            last_short,
            bus_pad = bus_pad,
        );
    }

    println!();
    println!("BUS = number of distinct people who have endorsed items in this subsystem");
    println!("\x1B[31m1\x1B[0m = single point of knowledge failure  \x1B[33m2\x1B[0m = at risk  \x1B[32m3+\x1B[0m = healthy");
    println!();

    Ok(())
}

fn debt_interactive(subsystem_filter: Option<&str>) -> Result<()> {
    loop {
        let db = db::Database::init().context("Failed to initialize database")?;
        let items = db.all_activity_items()?;

        if items.is_empty() {
            println!("No activity items. Run `git-cognitive audit` first.");
            break;
        }

        let items: Vec<_> = if let Some(filter) = subsystem_filter {
            items
                .into_iter()
                .filter(|i| i.subsystem == filter)
                .collect()
        } else {
            items
        };

        let picker_items = picker::build_picker_items(&items, false);

        match picker::run_picker(picker_items)? {
            None => break,
            Some(result) => {
                let (sha, status) = if let Some(rest) = result.strip_prefix("reviewed:") {
                    (rest.to_string(), "reviewed")
                } else {
                    (result, "endorsed")
                };
                do_endorse(&sha, status)?;
            }
        }
    }

    Ok(())
}

fn debt_command(subsystem_filter: Option<&str>) -> Result<()> {
    use cognitive_debt::{Classification, EndorsementStatus};

    let db = db::Database::init().context("Failed to initialize database")?;
    let items = db
        .all_activity_items()
        .context("Failed to load activity items")?;

    if items.is_empty() {
        println!("No activity items found. Run `git-cognitive audit` first.");
        return Ok(());
    }

    let items: Vec<_> = if let Some(filter) = subsystem_filter {
        items
            .into_iter()
            .filter(|i| i.subsystem == filter)
            .collect()
    } else {
        items
    };

    let mut subsystems: std::collections::HashMap<String, (usize, usize, usize, f32, usize)> =
        std::collections::HashMap::new();

    for item in &items {
        if matches!(item.endorsement_status, EndorsementStatus::Excluded) {
            continue;
        }
        let entry = subsystems
            .entry(item.subsystem.clone())
            .or_insert((0, 0, 0, 0.0, 0));
        entry.0 += 1;
        if matches!(
            item.endorsement_status,
            EndorsementStatus::Endorsed | EndorsementStatus::Reviewed
        ) {
            entry.1 += 1;
        } else {
            entry.2 += 1;
        }
        entry.3 += item.cognitive_friction_score;
        if item.zombie {
            entry.4 += 1;
        }
    }

    println!(
        "\n{:<20} {:<7} {:<10} {:<12} {:<10} {:<8} STATUS",
        "SUBSYSTEM", "ITEMS", "ENDORSED", "UNENDORSED", "AVG FRIC", "ZOMBIES"
    );
    println!("{}", "-".repeat(80));

    let mut subsystem_list: Vec<_> = subsystems.iter().collect();
    subsystem_list.sort_by(|a, b| b.1 .2.cmp(&a.1 .2));

    for (name, (total, endorsed, unendorsed, friction_sum, zombies)) in &subsystem_list {
        let avg_friction = if *total > 0 {
            friction_sum / *total as f32
        } else {
            0.0
        };

        let status = if *zombies > 0 {
            "\x1B[31m██ ZOMBIE\x1B[0m".to_string()
        } else if *unendorsed == 0 {
            "\x1B[32m✓ healthy\x1B[0m".to_string()
        } else {
            let pct = *endorsed as f32 / *total as f32;
            if pct < 0.5 {
                "\x1B[31m██ CRITICAL\x1B[0m".to_string()
            } else {
                "\x1B[33m▓ WARNING\x1B[0m".to_string()
            }
        };

        let zombie_str = if *zombies > 0 {
            format!("\x1B[31m{}\x1B[0m", zombies)
        } else {
            "0".to_string()
        };

        println!(
            "{:<20} {:<7} {:<10} {:<12} {:<10} {:<8} {}",
            &name[..20.min(name.len())],
            total,
            endorsed,
            unendorsed,
            format!("{:.2}", avg_friction),
            zombie_str,
            status,
        );
    }

    println!();

    let risk_items: Vec<_> = items
        .iter()
        .filter(|i| {
            matches!(i.classification, Classification::Risk)
                && !matches!(
                    i.endorsement_status,
                    EndorsementStatus::Endorsed | EndorsementStatus::Excluded
                )
        })
        .collect();

    if !risk_items.is_empty() {
        println!("  {} unendorsed RISK item(s):", risk_items.len());
        for item in risk_items.iter().take(5) {
            println!(
                "   {} [{}] {}",
                &item.id[..8.min(item.id.len())],
                item.subsystem,
                item.title
            );
        }
        println!();
    }

    let zombie_items: Vec<_> = items.iter().filter(|i| i.zombie).collect();
    if !zombie_items.is_empty() {
        println!("  {} zombie(s) detected:", zombie_items.len());
        for item in zombie_items.iter().take(5) {
            println!(
                "   {} [{}] {}",
                &item.id[..8.min(item.id.len())],
                item.subsystem,
                item.title
            );
        }
        println!();
    }

    Ok(())
}

fn enable_claude() -> Result<()> {
    let git_hooks_dir = PathBuf::from(".git/hooks");
    if !git_hooks_dir.exists() {
        anyhow::bail!("No .git/hooks directory found — are you in a git repository?");
    }

    // --- post-commit hook ---
    let post_commit = git_hooks_dir.join("post-commit");
    let post_commit_script = "#!/bin/sh\ngit-cognitive audit --commit HEAD 2>/dev/null || true\ngit push origin cognitive-debt/v1 2>/dev/null || true\n";

    let should_write = if post_commit.exists() {
        let existing = std::fs::read_to_string(&post_commit).unwrap_or_default();
        !existing.contains("git-cognitive audit")
    } else {
        true
    };

    if should_write {
        if post_commit.exists() {
            let existing = std::fs::read_to_string(&post_commit).unwrap_or_default();
            let appended = format!(
                "{}\n# git-cognitive cognitive debt audit\ngit-cognitive audit --commit HEAD 2>/dev/null || true\ngit push origin cognitive-debt/v1 2>/dev/null || true\n",
                existing.trim()
            );
            std::fs::write(&post_commit, appended)?;
        } else {
            std::fs::write(&post_commit, post_commit_script)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&post_commit, std::fs::Permissions::from_mode(0o755))
                .context("Failed to set post-commit hook permissions")?;
        }
        println!("  wrote .git/hooks/post-commit");
    } else {
        println!("  .git/hooks/post-commit already configured — nothing to do.");
    }

    // --- Claude Code Stop hook (session capture) ---
    let claude_hooks_dir = PathBuf::from(".claude/hooks");
    std::fs::create_dir_all(&claude_hooks_dir).context("Failed to create .claude/hooks")?;

    let capture_script = r#"#!/bin/bash
INPUT=$(cat)
SESSION_ID=$(echo "$INPUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('session_id',''))" 2>/dev/null)
if [ -n "$SESSION_ID" ]; then
  git-cognitive session capture --session-id "$SESSION_ID" 2>/dev/null || true
  git push origin cognitive-debt/v1 2>/dev/null || true
fi
exit 0
"#;

    let capture_path = claude_hooks_dir.join("cognitive-capture.sh");
    std::fs::write(&capture_path, capture_script)
        .context("Failed to write cognitive-capture.sh")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&capture_path, std::fs::Permissions::from_mode(0o755))
            .context("Failed to set cognitive-capture.sh permissions")?;
    }
    println!("  wrote .claude/hooks/cognitive-capture.sh");

    // --- settings.json Stop hook ---
    let settings_path = PathBuf::from(".claude/settings.json");
    let marker = "cognitive:claude-setup";

    if settings_path.exists() {
        let existing = std::fs::read_to_string(&settings_path).unwrap_or_default();
        if existing.contains(marker) {
            println!("  .claude/settings.json already configured — nothing to do.");
        } else if existing.contains("\"Stop\"") {
            println!("  .claude/settings.json has existing Stop hooks — manually add:");
            println!("    {{ \"type\": \"command\", \"command\": \".claude/hooks/cognitive-capture.sh\" }}");
        } else {
            println!("  .claude/settings.json exists — manually add Stop hook:");
            println!("    {{ \"hooks\": {{ \"Stop\": [{{ \"hooks\": [{{ \"type\": \"command\", \"command\": \".claude/hooks/cognitive-capture.sh\" }}] }}] }} }}");
        }
    } else {
        let settings = format!(
            r#"{{
  "_comment": "{}",
  "hooks": {{
    "Stop": [
      {{
        "hooks": [{{ "type": "command", "command": ".claude/hooks/cognitive-capture.sh" }}]
      }}
    ]
  }}
}}
"#,
            marker
        );
        std::fs::write(&settings_path, settings)
            .context("Failed to write .claude/settings.json")?;
        println!("  wrote .claude/settings.json");
    }

    println!("\nDone. Claude Code will now:");
    println!("  • audit every commit for cognitive debt (post-commit hook)");
    println!("  • capture session transcripts for AI attribution (Stop hook)");
    println!("  • push cognitive-debt/v1 automatically");
    Ok(())
}
