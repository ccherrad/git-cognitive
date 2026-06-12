use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, queue,
    style::{self, Color, Stylize},
    terminal::{self, ClearType},
};
use std::collections::HashMap;
use std::io::{self, Write};
use std::process::Command;

use crate::cognitive_debt::CommitAudit;

#[derive(Debug)]
struct BlameLine {
    line_no: usize,
    sha: String,
    short_sha: String,
    content: String,
}

pub fn run_blame(file: &str, audits: &[CommitAudit]) -> Result<()> {
    let lines = parse_blame(file)?;
    if lines.is_empty() {
        println!("No blame output for {}", file);
        return Ok(());
    }

    let audit_map: HashMap<String, &CommitAudit> = audits
        .iter()
        .flat_map(|a| {
            let key8 = a.id[..8.min(a.id.len())].to_string();
            vec![(a.id.clone(), a), (key8, a)]
        })
        .collect();

    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let result = blame_loop(&mut stdout, file, &lines, &audit_map);

    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;

    result
}

fn parse_blame(file: &str) -> Result<Vec<BlameLine>> {
    let out = Command::new("git")
        .args(["blame", "--porcelain", file])
        .output()
        .context("Failed to run git blame")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git blame failed: {}", stderr.trim());
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = Vec::new();
    let mut line_no = 0usize;
    let mut current_sha = String::new();

    for raw in text.lines() {
        if let Some(stripped) = raw.strip_prefix('\t') {
            line_no += 1;
            lines.push(BlameLine {
                line_no,
                sha: current_sha.clone(),
                short_sha: current_sha[..8.min(current_sha.len())].to_string(),
                content: stripped.to_string(),
            });
        } else {
            let parts: Vec<&str> = raw.splitn(4, ' ').collect();
            if parts.len() >= 3 && parts[0].len() == 40 {
                current_sha = parts[0].to_string();
            }
        }
    }

    Ok(lines)
}

fn friction_bar(score: f32) -> (String, Color) {
    let filled = (score * 8.0).round() as usize;
    let bar: String = (0..8).map(|i| if i < filled { '█' } else { '░' }).collect();
    let color = if score >= 0.7 {
        Color::Red
    } else if score >= 0.4 {
        Color::Yellow
    } else {
        Color::Green
    };
    (bar, color)
}

const SHA_W: usize = 8;
const FRICT_W: usize = 10;
const AI_W: usize = 6;
const FLAG_W: usize = 2;
const GUTTER_W: usize = SHA_W + 1 + FRICT_W + 1 + AI_W + 1 + FLAG_W + 1;

fn blame_loop(
    stdout: &mut io::Stdout,
    file: &str,
    lines: &[BlameLine],
    audit_map: &HashMap<String, &CommitAudit>,
) -> Result<()> {
    let mut selected = 0usize;

    loop {
        let (cols, rows) = terminal::size().unwrap_or((120, 40));
        let header_rows = 2u16;
        let footer_rows = 2u16;
        let visible_rows = rows.saturating_sub(header_rows + footer_rows) as usize;
        let content_w = (cols as usize).saturating_sub(GUTTER_W + 5);

        execute!(stdout, terminal::Clear(ClearType::All))?;

        let header = format!(" git-cognitive blame  {}  ({} lines) ", file, lines.len());
        queue!(
            stdout,
            cursor::MoveTo(0, 0),
            style::PrintStyledContent(style::style(header).on(Color::DarkBlue).white().bold())
        )?;

        let col_hdr = format!(
            " {:<sha$} {:<frict$} {:<ai$} {:<flag$} {}",
            "SHA",
            "FRICTION",
            "AI%",
            "☠",
            "SOURCE",
            sha = SHA_W,
            frict = FRICT_W,
            ai = AI_W,
            flag = FLAG_W,
        );
        queue!(
            stdout,
            cursor::MoveTo(0, 1),
            style::PrintStyledContent(style::style(col_hdr).dark_grey())
        )?;

        let scroll = if selected >= visible_rows {
            selected - visible_rows + 1
        } else {
            0
        };

        for (i, line) in lines.iter().enumerate().skip(scroll).take(visible_rows) {
            let row = header_rows + (i - scroll) as u16;
            let is_sel = i == selected;
            let bg = if is_sel { Some(Color::DarkBlue) } else { None };

            queue!(stdout, cursor::MoveTo(0, row))?;
            let blank = " ".repeat(cols as usize);
            if let Some(b) = bg {
                queue!(stdout, style::PrintStyledContent(style::style(blank).on(b)))?;
            } else {
                queue!(stdout, style::Print(blank))?;
            }

            let audit = audit_map
                .get(&line.sha)
                .or_else(|| audit_map.get(&line.short_sha));

            let sha_styled = if is_sel {
                style::style(format!(" {:>sha$}", &line.short_sha, sha = SHA_W))
                    .white()
                    .on(Color::DarkBlue)
            } else {
                style::style(format!(" {:>sha$}", &line.short_sha, sha = SHA_W)).dark_grey()
            };
            queue!(
                stdout,
                cursor::MoveTo(0, row),
                style::PrintStyledContent(sha_styled)
            )?;

            let frict_x = (SHA_W + 2) as u16;
            if let Some(a) = audit {
                let (bar, bar_color) = friction_bar(a.cognitive_friction_score);
                let bar_styled = if let Some(b) = bg {
                    style::style(format!("{:<frict$}", bar, frict = FRICT_W))
                        .with(bar_color)
                        .on(b)
                } else {
                    style::style(format!("{:<frict$}", bar, frict = FRICT_W)).with(bar_color)
                };
                queue!(
                    stdout,
                    cursor::MoveTo(frict_x, row),
                    style::PrintStyledContent(bar_styled)
                )?;

                let ai_x = frict_x + FRICT_W as u16 + 1;
                let ai_str = a
                    .attribution_pct
                    .map(|p| format!("{:3.0}%ai", p * 100.0))
                    .unwrap_or_else(|| "      ".to_string());
                let ai_styled = if let Some(b) = bg {
                    style::style(format!("{:<ai$}", ai_str, ai = AI_W))
                        .dark_grey()
                        .on(b)
                } else {
                    style::style(format!("{:<ai$}", ai_str, ai = AI_W)).dark_grey()
                };
                queue!(
                    stdout,
                    cursor::MoveTo(ai_x, row),
                    style::PrintStyledContent(ai_styled)
                )?;

                let flag_x = ai_x + AI_W as u16 + 1;
                if a.zombie {
                    let zs = if let Some(b) = bg {
                        style::style("☠ ").red().on(b)
                    } else {
                        style::style("☠ ").red()
                    };
                    queue!(
                        stdout,
                        cursor::MoveTo(flag_x, row),
                        style::PrintStyledContent(zs)
                    )?;
                }
            } else {
                let dash = if let Some(b) = bg {
                    style::style(format!("{:<w$}", "·", w = FRICT_W + 1 + AI_W + 1 + FLAG_W))
                        .dark_grey()
                        .on(b)
                } else {
                    style::style(format!("{:<w$}", "·", w = FRICT_W + 1 + AI_W + 1 + FLAG_W))
                        .dark_grey()
                };
                queue!(
                    stdout,
                    cursor::MoveTo(frict_x, row),
                    style::PrintStyledContent(dash)
                )?;
            }

            let src_x = (GUTTER_W + 1) as u16;
            let snippet: String = line.content.chars().take(content_w).collect();
            let src_styled = if let Some(b) = bg {
                style::style(snippet).white().on(b)
            } else {
                style::style(snippet).white()
            };
            queue!(
                stdout,
                cursor::MoveTo(src_x, row),
                style::PrintStyledContent(src_styled)
            )?;
        }

        let sel_line = &lines[selected];
        let sel_audit = audit_map
            .get(&sel_line.sha)
            .or_else(|| audit_map.get(&sel_line.short_sha));
        let detail = if let Some(a) = sel_audit {
            format!(
                " line {}  {}  friction {:.2}  {}",
                sel_line.line_no,
                &sel_line.short_sha,
                a.cognitive_friction_score,
                &a.title.chars().take(50).collect::<String>(),
            )
        } else {
            format!(
                " line {}  {}  (not indexed)",
                sel_line.line_no, sel_line.short_sha
            )
        };
        queue!(
            stdout,
            cursor::MoveTo(0, rows - footer_rows),
            style::PrintStyledContent(style::style(detail).white())
        )?;

        let help = "  ↑↓/jk navigate   Enter drill into audit   q quit";
        queue!(
            stdout,
            cursor::MoveTo(0, rows - footer_rows + 1),
            style::PrintStyledContent(style::style(help).dark_grey())
        )?;

        stdout.flush()?;

        if let Event::Key(key) = event::read()? {
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(()),
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    selected = selected.saturating_sub(1);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    if selected < lines.len() - 1 {
                        selected += 1;
                    }
                }
                (KeyCode::Enter, _) => {
                    let sha = sel_line.sha.clone();
                    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
                    terminal::disable_raw_mode()?;

                    if let Some(a) = sel_audit {
                        print_audit_detail(a);
                    } else {
                        println!("\nNo audit indexed for {}.", &sha[..8.min(sha.len())]);
                        println!("Run `git-cognitive index` to index it.");
                    }

                    println!("\nPress Enter to return...");
                    let mut buf = String::new();
                    io::stdin().read_line(&mut buf).ok();

                    terminal::enable_raw_mode()?;
                    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
                }
                _ => {}
            }
        }
    }
}

fn print_audit_detail(a: &CommitAudit) {
    println!();
    println!("  commit   {}", a.id);
    println!("  branch   {}", a.branch);
    println!("  title    {}", a.title);
    if !a.summary.is_empty() {
        println!("  summary  {}", a.summary);
    }
    println!();
    println!("  friction {:.2}", a.cognitive_friction_score);
    if let Some(pct) = a.attribution_pct {
        println!("  agent    {:.0}%", pct * 100.0);
    }
    println!("  lines    {}", a.lines_changed);
    if a.large_diff {
        println!("  large_diff  yes");
    }
    if let Some(dur) = a.session_duration_secs {
        println!("  session  {}h {}m", dur / 3600, (dur % 3600) / 60);
    }
    if a.fatigue {
        println!("  fatigue  yes");
    }
    println!("  zombie   {}", if a.zombie { "yes" } else { "no" });
    println!("  audited  {}", a.audited_at);
    if !a.hotspots.is_empty() {
        println!();
        println!("  hotspots:");
        for h in &a.hotspots {
            println!(
                "    {:<50} complexity {:>3}  doc_gap {:.2}",
                h.file, h.complexity, h.doc_gap
            );
        }
    }
    println!();
}
