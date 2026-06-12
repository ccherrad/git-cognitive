mod blame;
mod cognitive_debt;
mod db;
mod index;
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
    #[command(about = "Index the minimal set of commits covering the current repo state")]
    Index,

    #[command(about = "Interactive blame view with cognitive audit overlay")]
    Blame {
        #[arg(help = "File path to blame")]
        file: String,
    },

    #[command(about = "Show commit audit details")]
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
        Commands::Index => {
            let repo_path = PathBuf::from(".");
            index::run_index(&repo_path)?;
        }
        Commands::Blame { file } => {
            blame_command(&file)?;
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

fn show_command(sha: &str) -> Result<()> {
    let repo_path = PathBuf::from(".");

    let item = cognitive_debt::read_commit_audit_from_branch(&repo_path, sha)?;

    match item {
        None => {
            println!("No audit found for {}.", &sha[..8.min(sha.len())]);
            println!("Run `git-cognitive index` first.");
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
            println!("  friction {:.2}", item.cognitive_friction_score);
            if let Some(pct) = item.attribution_pct {
                println!("  agent    {:.0}%", pct * 100.0);
            }
            println!("  lines    {}", item.lines_changed);
            if item.large_diff {
                println!("  large_diff  yes (>100 lines, AI-attributed)");
            }
            if let Some(dur) = item.session_duration_secs {
                println!("  session  {}h {}m", dur / 3600, (dur % 3600) / 60);
            }
            if item.fatigue {
                println!("  fatigue  yes (commit after 3h+ session)");
            }
            println!("  zombie   {}", if item.zombie { "yes" } else { "no" });
            println!("  audited  {}", item.audited_at);
            if !item.hotspots.is_empty() {
                println!();
                println!("  hotspots:");
                for h in &item.hotspots {
                    println!(
                        "    {:<50} complexity {:>3}  doc_gap {:.2}",
                        h.file, h.complexity, h.doc_gap
                    );
                }
            }
            println!();
        }
    }

    Ok(())
}

fn blame_command(file: &str) -> Result<()> {
    let db = db::Database::init().context("Failed to initialize database")?;
    let audits = db
        .all_commit_audits()
        .context("Failed to load commit audits")?;
    blame::run_blame(file, &audits)
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
        println!("Run `git-cognitive blame <file>` to inspect the updated state.");
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
                            "name": "show",
                            "description": "Get full details for a commit: friction score, AI attribution, hotspots, and session info.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "sha": { "type": "string", "description": "Commit SHA or HEAD" }
                                },
                                "required": ["sha"]
                            }
                        },
                        {
                            "name": "blame",
                            "description": "Return every line of a file with its last-touching commit SHA and cognitive audit data (friction, AI attribution, zombie flag). Use this to identify which lines carry the most cognitive risk.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "file": { "type": "string", "description": "File path relative to repo root" }
                                },
                                "required": ["file"]
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
        "show" => {
            let sha = args["sha"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing required argument: sha"))?;
            let resolved = resolve_sha(sha)?;
            mcp_show(&resolved)
        }
        "blame" => {
            let file = args["file"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing required argument: file"))?;
            mcp_blame(file)
        }
        _ => anyhow::bail!("unknown tool: {}", name),
    }
}

fn mcp_blame(file: &str) -> Result<serde_json::Value> {
    let db = db::Database::init()?;
    let audits = db.all_commit_audits()?;

    let audit_map: std::collections::HashMap<String, &cognitive_debt::CommitAudit> = audits
        .iter()
        .flat_map(|a| {
            let key8 = a.id[..8.min(a.id.len())].to_string();
            vec![(a.id.clone(), a), (key8, a)]
        })
        .collect();

    let out = std::process::Command::new("git")
        .args(["blame", "--porcelain", file])
        .output()
        .context("Failed to run git blame")?;

    if !out.status.success() {
        anyhow::bail!(
            "git blame failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = Vec::new();
    let mut line_no = 0usize;
    let mut current_sha = String::new();

    for raw in text.lines() {
        if let Some(stripped) = raw.strip_prefix('\t') {
            line_no += 1;
            let short = current_sha[..8.min(current_sha.len())].to_string();
            let audit = audit_map
                .get(&current_sha)
                .or_else(|| audit_map.get(&short));
            lines.push(serde_json::json!({
                "line_no": line_no,
                "sha": &current_sha,
                "content": stripped,
                "friction": audit.map(|a| a.cognitive_friction_score),
                "ai_attributed": audit.map(|a| a.ai_attributed),
                "attribution_pct": audit.and_then(|a| a.attribution_pct),
                "zombie": audit.map(|a| a.zombie),
            }));
        } else {
            let parts: Vec<&str> = raw.splitn(4, ' ').collect();
            if parts.len() >= 3 && parts[0].len() == 40 {
                current_sha = parts[0].to_string();
            }
        }
    }

    Ok(serde_json::json!({
        "file": file,
        "lines": lines,
    }))
}

fn mcp_show(sha: &str) -> Result<serde_json::Value> {
    let repo_path = PathBuf::from(".");

    let item = cognitive_debt::read_commit_audit_from_branch(&repo_path, sha)?;

    match item {
        None => Ok(serde_json::json!({
            "error": "not_found",
            "sha": sha,
            "hint": "Run `git-cognitive index` first."
        })),
        Some(item) => {
            let hotspots_json: Vec<_> = item
                .hotspots
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "file": h.file,
                        "complexity": h.complexity,
                        "doc_gap": h.doc_gap,
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "sha": item.id,
                "branch": item.branch,
                "title": item.title,
                "summary": item.summary,
                "friction": item.cognitive_friction_score,
                "ai_attributed": item.ai_attributed,
                "attribution_pct": item.attribution_pct,
                "lines_changed": item.lines_changed,
                "large_diff": item.large_diff,
                "session_duration_secs": item.session_duration_secs,
                "fatigue": item.fatigue,
                "zombie": item.zombie,
                "audited_at": item.audited_at,
                "hotspots": hotspots_json,
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
    let post_commit_script = "#!/bin/sh\nsleep 2\ngit-cognitive index 2>/dev/null || true\ngit-cognitive push 2>/dev/null || true\n";

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
                "{}\n# git-cognitive\nsleep 2\ngit-cognitive index 2>/dev/null || true\ngit-cognitive push 2>/dev/null || true\n",
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
