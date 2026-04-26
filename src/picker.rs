use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, queue,
    style::{self, Color, Stylize},
    terminal::{self, ClearType},
};
use std::io::{self, Write};

use crate::cognitive_debt::{ActivityItem, EndorsementStatus};

#[derive(Clone)]
pub struct PickerItem {
    pub sha: String,
    pub short_sha: String,
    pub classification: String,
    pub title: String,
    pub friction: f32,
    pub attribution_pct: Option<f32>,
    pub endorsement_status: String,
    pub zombie: bool,
    pub days_old: Option<u64>,
}

impl PickerItem {
    pub fn from_activity(item: &ActivityItem) -> Self {
        let days_old = parse_days_old(&item.audited_at);
        Self {
            sha: item.id.clone(),
            short_sha: item.id[..8.min(item.id.len())].to_string(),
            classification: item.classification.to_string(),
            title: item.title.chars().take(63).collect(),
            friction: item.cognitive_friction_score,
            attribution_pct: item.attribution_pct,
            endorsement_status: item.endorsement_status.to_string(),
            zombie: item.zombie,
            days_old,
        }
    }
}

fn parse_days_old(audited_at: &str) -> Option<u64> {
    if audited_at.is_empty() {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();

    let out = std::process::Command::new("date")
        .args(["-j", "-f", "%Y-%m-%dT%H:%M:%S", &audited_at[..19], "+%s"])
        .output()
        .ok()?;

    let ts: u64 = if out.status.success() {
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()?
    } else {
        let out2 = std::process::Command::new("date")
            .args(["-d", audited_at, "+%s"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out2.stdout).trim().parse().ok()?
    };

    Some((now.saturating_sub(ts)) / 86400)
}

struct Col {
    x: u16,
    width: u16,
}

const COL_CURSOR: Col = Col { x: 0, width: 2 };
const COL_BADGE: Col = Col { x: 2, width: 7 };
const COL_ZOMBIE: Col = Col { x: 9, width: 2 };
const COL_TITLE: Col = Col { x: 11, width: 59 };
const COL_FRICTION: Col = Col { x: 70, width: 11 };
const COL_ATTRIB: Col = Col { x: 81, width: 7 };
const COL_DAYS: Col = Col { x: 88, width: 5 };
const COL_STATUS: Col = Col { x: 93, width: 12 };
const ROW_WIDTH: u16 = 105;

fn write_cell(
    stdout: &mut io::Stdout,
    row: u16,
    col: &Col,
    text: &str,
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
) -> Result<()> {
    let chars: Vec<char> = text.chars().collect();
    let visible_len = chars.len().min(col.width as usize);
    let cell: String = chars[..visible_len].iter().collect();
    let padding = " ".repeat((col.width as usize).saturating_sub(visible_len));
    let full = format!("{}{}", cell, padding);

    queue!(stdout, cursor::MoveTo(col.x, row))?;

    let mut styled = style::style(full);
    if let Some(f) = fg {
        styled = styled.with(f);
    }
    if let Some(b) = bg {
        styled = styled.on(b);
    }
    if bold {
        styled = styled.bold();
    }

    queue!(stdout, style::PrintStyledContent(styled))?;
    Ok(())
}

fn badge_colors(c: &str) -> (&'static str, Color, Color) {
    match c {
        "risk" => ("RISK", Color::White, Color::Red),
        "tech_debt" => ("DEBT", Color::Black, Color::Yellow),
        "new_feature" => ("FEAT", Color::White, Color::Blue),
        "bug_fix" => ("FIX ", Color::Black, Color::Green),
        "refactor" => ("RFCT", Color::White, Color::DarkGrey),
        "minor" => ("MIN ", Color::White, Color::DarkGrey),
        "dependency_update" => ("DEP ", Color::White, Color::DarkGrey),
        _ => ("OTH ", Color::White, Color::DarkGrey),
    }
}

fn status_color(s: &str) -> (&'static str, Color) {
    match s {
        "endorsed" => ("endorsed", Color::Green),
        "excluded" => ("excluded", Color::DarkGrey),
        _ => ("unendorsed", Color::Red),
    }
}

fn friction_bar(score: f32) -> (String, Color) {
    let filled = (score * 10.0).round() as usize;
    let bar: String = (0..10)
        .map(|i| if i < filled { '█' } else { '░' })
        .collect();
    let color = if score >= 0.7 {
        Color::Red
    } else if score >= 0.4 {
        Color::Yellow
    } else {
        Color::Green
    };
    (bar, color)
}

fn render_row(
    stdout: &mut io::Stdout,
    row: u16,
    item: &PickerItem,
    is_selected: bool,
) -> Result<()> {
    let bg = if is_selected {
        Some(Color::DarkBlue)
    } else {
        None
    };

    queue!(stdout, cursor::MoveTo(0, row))?;
    let blank = " ".repeat(ROW_WIDTH as usize);
    if let Some(b) = bg {
        queue!(stdout, style::PrintStyledContent(style::style(blank).on(b)))?;
    } else {
        queue!(stdout, style::Print(blank))?;
    }

    let cursor_char = if is_selected { "▶" } else { " " };
    write_cell(
        stdout,
        row,
        &COL_CURSOR,
        cursor_char,
        Some(Color::White),
        bg,
        false,
    )?;

    let (badge_text, badge_fg, badge_bg) = badge_colors(&item.classification);
    write_cell(
        stdout,
        row,
        &COL_BADGE,
        badge_text,
        Some(badge_fg),
        Some(badge_bg),
        true,
    )?;

    if item.zombie {
        write_cell(stdout, row, &COL_ZOMBIE, "☠", Some(Color::Red), bg, true)?;
    }

    write_cell(
        stdout,
        row,
        &COL_TITLE,
        &item.title,
        Some(Color::White),
        bg,
        false,
    )?;

    let (bar, bar_color) = friction_bar(item.friction);
    write_cell(stdout, row, &COL_FRICTION, &bar, Some(bar_color), bg, false)?;

    let attrib = item
        .attribution_pct
        .map(|p| format!("{:3.0}%ai", p * 100.0))
        .unwrap_or_else(|| "      ".to_string());
    write_cell(
        stdout,
        row,
        &COL_ATTRIB,
        &attrib,
        Some(Color::DarkGrey),
        bg,
        false,
    )?;

    let days = item
        .days_old
        .map(|d| format!("{}d", d))
        .unwrap_or_else(|| "-".to_string());
    write_cell(
        stdout,
        row,
        &COL_DAYS,
        &days,
        Some(Color::DarkGrey),
        bg,
        false,
    )?;

    let (status_text, status_color) = status_color(&item.endorsement_status);
    write_cell(
        stdout,
        row,
        &COL_STATUS,
        status_text,
        Some(status_color),
        bg,
        false,
    )?;

    Ok(())
}

pub fn run_picker(items: Vec<PickerItem>) -> Result<Option<String>> {
    if items.is_empty() {
        println!("No items to review.");
        return Ok(None);
    }

    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;

    let result = picker_loop(&mut stdout, &items);

    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
    terminal::disable_raw_mode()?;

    result
}

fn picker_loop(stdout: &mut io::Stdout, items: &[PickerItem]) -> Result<Option<String>> {
    let mut selected = 0usize;
    let mut status_msg = String::new();

    loop {
        let (_, rows) = terminal::size().unwrap_or((120, 40));
        let header_rows = 3u16;
        let footer_rows = 4u16;
        let visible_rows = (rows.saturating_sub(header_rows + footer_rows)) as usize;

        execute!(stdout, terminal::Clear(ClearType::All))?;

        let header = format!(
            " git-semantic endorse  ({} items, {} unendorsed) ",
            items.len(),
            items
                .iter()
                .filter(|i| i.endorsement_status == "unendorsed")
                .count()
        );
        queue!(
            stdout,
            cursor::MoveTo(0, 0),
            style::PrintStyledContent(style::style(header).on(Color::DarkBlue).white().bold())
        )?;

        let col_header = format!(
            "  {:<6} {:<2} {:<59}{:<11}{:<7}{:<5}{}",
            "TYPE", "☠", "TITLE", "FRICTION", "AI", "AGE", "STATUS"
        );
        queue!(
            stdout,
            cursor::MoveTo(0, 1),
            style::PrintStyledContent(style::style(col_header).dark_grey())
        )?;

        let scroll_offset = if selected >= visible_rows {
            selected - visible_rows + 1
        } else {
            0
        };

        for (i, item) in items
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_rows)
        {
            let row = header_rows + (i - scroll_offset) as u16;
            render_row(stdout, row, item, i == selected)?;
        }

        let detail_row = rows - footer_rows;
        let item = &items[selected];
        let detail = format!(
            " {}  friction {:.2}  {}",
            item.short_sha, item.friction, item.endorsement_status,
        );
        queue!(
            stdout,
            cursor::MoveTo(0, detail_row),
            style::PrintStyledContent(style::style(&*detail).white())
        )?;

        let help = "  ↑↓/jk navigate   e/Enter endorse   s git show   q quit";
        queue!(
            stdout,
            cursor::MoveTo(0, detail_row + 1),
            style::PrintStyledContent(style::style(help).dark_grey())
        )?;

        if !status_msg.is_empty() {
            queue!(
                stdout,
                cursor::MoveTo(0, detail_row + 2),
                style::PrintStyledContent(style::style(&*status_msg).green().bold())
            )?;
            status_msg.clear();
        }

        stdout.flush()?;

        if let Event::Key(key) = event::read()? {
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(None),
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(None),
                (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                    selected = selected.saturating_sub(1);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                    if selected < items.len() - 1 {
                        selected += 1;
                    }
                }
                (KeyCode::Char('e'), _) | (KeyCode::Enter, _) => {
                    return Ok(Some(items[selected].sha.clone()));
                }
                (KeyCode::Char('s'), _) => {
                    let sha = items[selected].sha.clone();
                    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show)?;
                    terminal::disable_raw_mode()?;

                    std::process::Command::new("git")
                        .args(["show", "--stat", &sha])
                        .status()
                        .ok();

                    println!("\nPress Enter to return...");
                    let mut buf = String::new();
                    io::stdin().read_line(&mut buf).ok();

                    terminal::enable_raw_mode()?;
                    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
                    status_msg = format!("git show {}", &sha[..8]);
                }
                _ => {}
            }
        }
    }
}

pub fn build_picker_items(items: &[ActivityItem], filter_unendorsed: bool) -> Vec<PickerItem> {
    let mut picker: Vec<PickerItem> = items
        .iter()
        .filter(|i| {
            if filter_unendorsed {
                !matches!(
                    i.endorsement_status,
                    EndorsementStatus::Endorsed | EndorsementStatus::Excluded
                )
            } else {
                !matches!(i.endorsement_status, EndorsementStatus::Excluded)
            }
        })
        .map(PickerItem::from_activity)
        .collect();

    picker.sort_by(|a, b| {
        let a_risk = a.classification == "risk";
        let b_risk = b.classification == "risk";
        b_risk.cmp(&a_risk).then(b.zombie.cmp(&a.zombie)).then(
            b.friction
                .partial_cmp(&a.friction)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    picker
}
