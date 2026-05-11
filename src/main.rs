mod cognitive_debt;
mod db;
mod index;
mod picker;
mod session;
mod treesitter;

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
    #[command(about = "Index commits for cognitive debt")]
    Index {
        #[arg(long, help = "Index a specific commit SHA or HEAD")]
        commit: Option<String>,

        #[arg(long, help = "Index all commits since this SHA")]
        since: Option<String>,

        #[arg(long, help = "Backfill all history (last 500 commits)")]
        all: bool,

        #[arg(long, help = "Scan for zombie AI commits (>30 days unendorsed)")]
        check_zombies: bool,
    },

    #[command(about = "Endorse a commit as understood and vouched for")]
    Endorse {
        #[arg(help = "Commit SHA or HEAD (omit for interactive picker)")]
        sha: Option<String>,

        #[arg(long, help = "One sentence explaining what this commit does")]
        reason: Option<String>,
    },

    #[command(about = "Show cognitive debt — flat list of commits with friction and status")]
    Debt {
        #[arg(long, help = "Open interactive picker to endorse items")]
        interactive: bool,
    },

    #[command(about = "Show activity item details and endorsement history for a commit")]
    Show {
        #[arg(help = "Commit SHA or HEAD")]
        sha: String,
    },

    #[command(about = "Show the session slice captured for a commit")]
    Session {
        #[arg(help = "Commit SHA or HEAD")]
        sha: String,
    },

    #[command(about = "Push cognitive debt data to origin")]
    Push,

    #[command(about = "Pull cognitive debt data from origin")]
    Pull,

    #[command(about = "Enable a coding agent for this project (e.g. claude)")]
    Enable {
        #[arg(help = "Agent to enable: claude")]
        agent: String,
    },

    #[command(about = "Start the MCP server (JSON-RPC over stdio)")]
    Mcp,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Index {
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
            index::run_index(
                &repo_path,
                since.as_deref(),
                commit.as_deref(),
                check_zombies,
            )?;
        }
        Commands::Endorse { sha, reason } => {
            endorse_command(sha.as_deref(), reason.as_deref())?;
        }
        Commands::Debt { interactive } => {
            if interactive {
                debt_interactive()?;
            } else {
                debt_command()?;
            }
        }
        Commands::Show { sha } => {
            let resolved = resolve_sha(&sha)?;
            show_command(&resolved)?;
        }
        Commands::Session { sha } => {
            let repo_path = PathBuf::from(".");
            let resolved = resolve_sha(&sha)?;
            session::run_show_session(&repo_path, &resolved)?;
        }
        Commands::Push => sync_push()?,
        Commands::Pull => sync_pull()?,
        Commands::Enable { agent } => match agent.as_str() {
            "claude" => enable_claude()?,
            other => anyhow::bail!("Unknown agent '{}'. Supported: claude", other),
        },
        Commands::Mcp => mcp_serve()?,
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

fn do_endorse(sha: &str, reason: Option<&str>) -> Result<()> {
    use cognitive_debt::{DebtStore, EndorsementRecord, EndorsementStatus};

    let author = std::process::Command::new("git")
        .args(["config", "user.email"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let repo_path = PathBuf::from(".");
    let store = DebtStore::open(&repo_path)?;
    let record = EndorsementRecord {
        sha: sha.to_string(),
        status: EndorsementStatus::Endorsed,
        author,
        timestamp: cognitive_debt::now_rfc3339(),
        reason: reason.map(|s| s.to_string()),
    };

    store.write_endorsement(&record)?;
    let db = db::Database::init()?;
    db.insert_endorsement(&record)?;
    store.commit()?;
    sync_push().ok();

    println!("Endorsed {}.", &sha[..8.min(sha.len())]);
    Ok(())
}

fn endorse_command(sha: Option<&str>, reason: Option<&str>) -> Result<()> {
    match sha {
        Some(s) => {
            let resolved = resolve_sha(s)?;
            let reason = match reason {
                Some(r) => Some(r.to_string()),
                None => prompt_reason()?,
            };
            do_endorse(&resolved, reason.as_deref())
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
                    Some(sha) => {
                        let reason = prompt_reason()?;
                        do_endorse(&sha, reason.as_deref())?;
                    }
                }
            }
            Ok(())
        }
    }
}

fn prompt_reason() -> Result<Option<String>> {
    use std::io::Write;
    print!("  reason (one sentence, or Enter to skip): ");
    std::io::stdout().flush()?;
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let trimmed = buf.trim().to_string();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed))
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
                "Run `git-cognitive index --commit {}` first.",
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
                    if let Some(r) = &e.reason {
                        println!("      \"{}\"", r);
                    }
                }
            }
            println!();
        }
    }

    Ok(())
}

fn debt_interactive() -> Result<()> {
    loop {
        let db = db::Database::init().context("Failed to initialize database")?;
        let items = db.all_activity_items()?;

        if items.is_empty() {
            println!("No activity items. Run `git-cognitive index` first.");
            break;
        }

        let picker_items = picker::build_picker_items(&items, false);

        match picker::run_picker(picker_items)? {
            None => break,
            Some(sha) => {
                let reason = prompt_reason()?;
                do_endorse(&sha, reason.as_deref())?;
            }
        }
    }

    Ok(())
}

fn debt_command() -> Result<()> {
    use cognitive_debt::{Classification, EndorsementStatus};

    let db = db::Database::init().context("Failed to initialize database")?;
    let items = db
        .all_activity_items()
        .context("Failed to load activity items")?;

    if items.is_empty() {
        println!("No activity items found. Run `git-cognitive index` first.");
        return Ok(());
    }

    let visible: Vec<_> = items
        .iter()
        .filter(|i| !matches!(i.endorsement_status, EndorsementStatus::Excluded))
        .collect();

    println!(
        "\n{:<10} {:<12} {:<10} {:<8} {:<59} STATUS",
        "COMMIT", "CLASS", "FRICTION", "AI", "TITLE"
    );
    println!("{}", "-".repeat(105));

    for item in &visible {
        let status = if item.zombie {
            "\x1B[31mZOMBIE\x1B[0m"
        } else {
            match item.endorsement_status {
                EndorsementStatus::Endorsed => "\x1B[32mendorsed\x1B[0m",
                _ => "\x1B[31munendorsed\x1B[0m",
            }
        };

        let ai = item
            .attribution_pct
            .map(|p| format!("{:3.0}%", p * 100.0))
            .unwrap_or_else(|| {
                if item.ai_attributed {
                    " ai ".to_string()
                } else {
                    "    ".to_string()
                }
            });

        println!(
            "{:<10} {:<12} {:<10} {:<8} {:<59} {}",
            &item.id[..8.min(item.id.len())],
            &item.classification.to_string()[..12.min(item.classification.to_string().len())],
            format!("{:.2}", item.cognitive_friction_score),
            ai,
            &item.title[..59.min(item.title.len())],
            status,
        );
    }

    println!();

    let risk_items: Vec<_> = visible
        .iter()
        .filter(|i| {
            matches!(i.classification, Classification::Risk)
                && !matches!(i.endorsement_status, EndorsementStatus::Endorsed)
        })
        .collect();

    if !risk_items.is_empty() {
        println!("  {} unendorsed RISK item(s):", risk_items.len());
        for item in risk_items.iter().take(5) {
            println!("   {} {}", &item.id[..8.min(item.id.len())], item.title);
        }
        println!();
    }

    let zombie_items: Vec<_> = visible.iter().filter(|i| i.zombie).collect();
    if !zombie_items.is_empty() {
        println!("  {} zombie(s) detected:", zombie_items.len());
        for item in zombie_items.iter().take(5) {
            println!("   {} {}", &item.id[..8.min(item.id.len())], item.title);
        }
        println!();
    }

    Ok(())
}

fn sync_push() -> Result<()> {
    let out = std::process::Command::new("git")
        .args(["push", "origin", "cognitive/v1"])
        .output()
        .context("Failed to run git push")?;
    if out.status.success() {
        println!("Pushed cognitive/v1 to origin.");
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("Push failed: {}", stderr.trim());
    }
    Ok(())
}

fn sync_pull() -> Result<()> {
    let out = std::process::Command::new("git")
        .args(["fetch", "origin", "cognitive/v1:cognitive/v1"])
        .output()
        .context("Failed to run git fetch")?;
    if out.status.success() {
        println!("Pulled cognitive/v1 from origin.");
        println!("Run `git-cognitive debt` to see the updated state.");
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("Pull failed: {}", stderr.trim());
    }
    Ok(())
}

fn mcp_serve() -> Result<()> {
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => l,
            Err(_) => break,
        };

        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = req["method"].as_str().unwrap_or("");

        let response = match method {
            "initialize" => mcp_ok(
                id,
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "git-cognitive", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
            "notifications/initialized" => continue,
            "tools/list" => mcp_ok(
                id,
                serde_json::json!({
                    "tools": [
                        {
                            "name": "debt",
                            "description": "List all indexed commits with their cognitive friction score, AI attribution, classification, and endorsement status. Use this to get an overview of the repo's cognitive debt.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "filter": {
                                        "type": "string",
                                        "enum": ["all", "unendorsed", "risk", "zombie"],
                                        "description": "Filter results. Default: all."
                                    }
                                }
                            }
                        },
                        {
                            "name": "show",
                            "description": "Get full details for a commit: classification, friction score, AI attribution percentage, summary, and endorsement history.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "sha": { "type": "string", "description": "Commit SHA or HEAD" }
                                },
                                "required": ["sha"]
                            }
                        },
                        {
                            "name": "endorse",
                            "description": "Endorse a commit as understood and vouched for. Records the endorsement with the git user identity. Provide a reason — one sentence explaining what the commit does. If you cannot write it, do not endorse.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "sha": { "type": "string", "description": "Commit SHA or HEAD" },
                                    "reason": { "type": "string", "description": "One sentence explaining what this commit does" }
                                },
                                "required": ["sha"]
                            }
                        }
                    ]
                }),
            ),
            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("");
                let args = &req["params"]["arguments"];
                match mcp_dispatch(name, args) {
                    Ok(data) => mcp_ok(
                        id,
                        serde_json::json!({
                            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&data).unwrap_or_default() }],
                            "structuredContent": data
                        }),
                    ),
                    Err(e) => mcp_err(id, -32000, &e.to_string()),
                }
            }
            _ => mcp_err(id, -32601, "method not found"),
        };

        writeln!(out, "{}", serde_json::to_string(&response)?)?;
        out.flush()?;
    }

    Ok(())
}

fn mcp_ok(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn mcp_err(id: serde_json::Value, code: i32, msg: &str) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
}

fn mcp_dispatch(name: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
    match name {
        "debt" => {
            let filter = args["filter"].as_str().unwrap_or("all");
            mcp_debt(filter)
        }
        "show" => {
            let sha = args["sha"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing required argument: sha"))?;
            let resolved = resolve_sha(sha)?;
            mcp_show(&resolved)
        }
        "endorse" => {
            let sha = args["sha"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing required argument: sha"))?;
            let reason = args["reason"].as_str();
            let resolved = resolve_sha(sha)?;
            do_endorse(&resolved, reason)?;
            Ok(serde_json::json!({ "sha": resolved, "status": "endorsed", "reason": reason }))
        }
        _ => anyhow::bail!("unknown tool: {}", name),
    }
}

fn mcp_debt(filter: &str) -> Result<serde_json::Value> {
    use cognitive_debt::{Classification, EndorsementStatus};

    let db = db::Database::init()?;
    let items = db.all_activity_items()?;

    let visible: Vec<_> = items
        .iter()
        .filter(|i| !matches!(i.endorsement_status, EndorsementStatus::Excluded))
        .filter(|i| match filter {
            "unendorsed" => !matches!(i.endorsement_status, EndorsementStatus::Endorsed),
            "risk" => matches!(i.classification, Classification::Risk),
            "zombie" => i.zombie,
            _ => true,
        })
        .map(|i| {
            serde_json::json!({
                "sha": i.id,
                "branch": i.branch,
                "title": i.title,
                "classification": i.classification.to_string(),
                "friction": i.cognitive_friction_score,
                "ai_attributed": i.ai_attributed,
                "attribution_pct": i.attribution_pct,
                "zombie": i.zombie,
                "status": i.endorsement_status.to_string(),
                "audited_at": i.audited_at,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "items": visible,
        "total": visible.len(),
    }))
}

fn mcp_show(sha: &str) -> Result<serde_json::Value> {
    let repo_path = PathBuf::from(".");

    let item = cognitive_debt::read_activity_from_branch(&repo_path, sha)?;
    let endorsements = cognitive_debt::read_endorsements_from_branch(&repo_path, sha)?;

    match item {
        None => Ok(serde_json::json!({
            "error": "not_found",
            "sha": sha,
            "hint": format!("Run `git-cognitive index --commit {}` first.", &sha[..8.min(sha.len())])
        })),
        Some(item) => {
            let endorsements_json: Vec<_> = endorsements
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "author": e.author,
                        "status": e.status.to_string(),
                        "timestamp": e.timestamp,
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "sha": item.id,
                "branch": item.branch,
                "title": item.title,
                "summary": item.summary,
                "classification": item.classification.to_string(),
                "friction": item.cognitive_friction_score,
                "ai_attributed": item.ai_attributed,
                "attribution_pct": item.attribution_pct,
                "zombie": item.zombie,
                "status": item.endorsement_status.to_string(),
                "audited_at": item.audited_at,
                "endorsements": endorsements_json,
            }))
        }
    }
}

fn enable_claude() -> Result<()> {
    let git_hooks_dir = PathBuf::from(".git/hooks");
    if !git_hooks_dir.exists() {
        anyhow::bail!("No .git/hooks directory found — are you in a git repository?");
    }

    // --- post-commit hook ---
    let post_commit = git_hooks_dir.join("post-commit");
    let post_commit_script = "#!/bin/sh\nsleep 2\ngit-cognitive index --commit HEAD 2>/dev/null || true\ngit-cognitive push 2>/dev/null || true\n";

    let should_write = if post_commit.exists() {
        let existing = std::fs::read_to_string(&post_commit).unwrap_or_default();
        !existing.contains("git-cognitive index")
    } else {
        true
    };

    if should_write {
        if post_commit.exists() {
            let existing = std::fs::read_to_string(&post_commit).unwrap_or_default();
            let appended = format!(
                "{}\n# git-cognitive cognitive debt index\nsleep 2\ngit-cognitive index --commit HEAD 2>/dev/null || true\ngit-cognitive push 2>/dev/null || true\n",
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

    println!("\nDone. Every commit will now:");
    println!("  • snapshot the active Claude session");
    println!("  • attribute AI lines to that commit");
    println!("  • score friction and classify");
    Ok(())
}
