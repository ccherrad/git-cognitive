use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::PathBuf;

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn init() -> Result<Self> {
        let db_path = PathBuf::from(".git/cognitive.db");
        let conn = Connection::open(&db_path).context("Failed to open database connection")?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS activity_items (
                id TEXT PRIMARY KEY,
                branch TEXT NOT NULL,
                classification TEXT NOT NULL,
                title TEXT NOT NULL,
                summary TEXT NOT NULL,
                commits_json TEXT NOT NULL,
                since_sha TEXT NOT NULL,
                until_sha TEXT NOT NULL,
                cognitive_friction_score REAL NOT NULL,
                ai_attributed INTEGER NOT NULL,
                attribution_pct REAL,
                lines_changed INTEGER NOT NULL DEFAULT 0,
                large_diff INTEGER NOT NULL DEFAULT 0,
                session_duration_secs INTEGER,
                fatigue INTEGER NOT NULL DEFAULT 0,
                zombie INTEGER NOT NULL DEFAULT 0,
                endorsement_status TEXT NOT NULL DEFAULT 'unendorsed',
                audited_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS endorsements (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                sha TEXT NOT NULL,
                status TEXT NOT NULL,
                author TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                reason TEXT
            );",
        )
        .context("Failed to create tables")?;

        Ok(Database { conn })
    }

    pub fn upsert_activity_item(&self, item: &crate::cognitive_debt::ActivityItem) -> Result<()> {
        let commits_json =
            serde_json::to_string(&item.commits).context("Failed to serialize commits")?;

        self.conn
            .execute(
                "INSERT INTO activity_items
                (id, branch, classification, title, summary, commits_json,
                 since_sha, until_sha, cognitive_friction_score, ai_attributed,
                 attribution_pct, lines_changed, large_diff, session_duration_secs,
                 fatigue, zombie, endorsement_status, audited_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
             ON CONFLICT(id) DO UPDATE SET
                classification=excluded.classification,
                title=excluded.title,
                summary=excluded.summary,
                commits_json=excluded.commits_json,
                since_sha=excluded.since_sha,
                until_sha=excluded.until_sha,
                cognitive_friction_score=excluded.cognitive_friction_score,
                ai_attributed=excluded.ai_attributed,
                attribution_pct=excluded.attribution_pct,
                lines_changed=excluded.lines_changed,
                large_diff=excluded.large_diff,
                session_duration_secs=excluded.session_duration_secs,
                fatigue=excluded.fatigue,
                zombie=excluded.zombie,
                endorsement_status=excluded.endorsement_status,
                audited_at=excluded.audited_at",
                params![
                    &item.id,
                    &item.branch,
                    item.classification.to_string(),
                    &item.title,
                    &item.summary,
                    &commits_json,
                    &item.since_sha,
                    &item.until_sha,
                    item.cognitive_friction_score,
                    item.ai_attributed as i64,
                    item.attribution_pct,
                    item.lines_changed as i64,
                    item.large_diff as i64,
                    item.session_duration_secs.map(|v| v as i64),
                    item.fatigue as i64,
                    item.zombie as i64,
                    item.endorsement_status.to_string(),
                    &item.audited_at,
                ],
            )
            .context("Failed to upsert activity item")?;

        Ok(())
    }

    pub fn insert_endorsement(
        &self,
        record: &crate::cognitive_debt::EndorsementRecord,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO endorsements (sha, status, author, timestamp, reason)
             VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    &record.sha,
                    record.status.to_string(),
                    &record.author,
                    &record.timestamp,
                    &record.reason,
                ],
            )
            .context("Failed to insert endorsement")?;

        self.conn
            .execute(
                "UPDATE activity_items SET endorsement_status = ?1 WHERE id = ?2",
                params![record.status.to_string(), &record.sha],
            )
            .context("Failed to update endorsement status on activity item")?;

        Ok(())
    }

    pub fn all_activity_items(&self) -> Result<Vec<crate::cognitive_debt::ActivityItem>> {
        use crate::cognitive_debt::{ActivityItem, Classification, EndorsementStatus};

        let mut stmt = self.conn.prepare(
            "SELECT id, branch, classification, title, summary, commits_json,
                    since_sha, until_sha, cognitive_friction_score, ai_attributed,
                    attribution_pct, lines_changed, large_diff, session_duration_secs,
                    fatigue, zombie, endorsement_status, audited_at
             FROM activity_items ORDER BY audited_at DESC",
        )?;

        let items = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, f64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<f64>>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, Option<i64>>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, i64>(15)?,
                    row.get::<_, String>(16)?,
                    row.get::<_, String>(17)?,
                ))
            })?
            .map(|row| {
                let (
                    id,
                    branch,
                    classification,
                    title,
                    summary,
                    commits_json,
                    since_sha,
                    until_sha,
                    friction,
                    ai_attributed,
                    attribution_pct,
                    lines_changed,
                    large_diff,
                    session_duration_secs,
                    fatigue,
                    zombie,
                    endorsement_status,
                    audited_at,
                ) = row?;

                let commits: Vec<String> = serde_json::from_str(&commits_json)
                    .map_err(|e| anyhow::anyhow!("Failed to parse commits: {}", e))?;

                let classification = match classification.as_str() {
                    "new_feature" => Classification::NewFeature,
                    "refactor" => Classification::Refactor,
                    "bug_fix" => Classification::BugFix,
                    "minor" => Classification::Minor,
                    "risk" => Classification::Risk,
                    "tech_debt" => Classification::TechDebt,
                    "dependency_update" => Classification::DependencyUpdate,
                    _ => Classification::Other,
                };

                let endorsement_status = match endorsement_status.as_str() {
                    "endorsed" => EndorsementStatus::Endorsed,
                    "excluded" => EndorsementStatus::Excluded,
                    _ => EndorsementStatus::Unendorsed,
                };

                Ok(ActivityItem {
                    id,
                    branch,
                    classification,
                    title,
                    summary,
                    commits,
                    since_sha,
                    until_sha,
                    cognitive_friction_score: friction as f32,
                    ai_attributed: ai_attributed != 0,
                    attribution_pct: attribution_pct.map(|v| v as f32),
                    lines_changed: lines_changed as u32,
                    large_diff: large_diff != 0,
                    session_duration_secs: session_duration_secs.map(|v| v as u64),
                    fatigue: fatigue != 0,
                    zombie: zombie != 0,
                    endorsement_status,
                    audited_at,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(items)
    }
}
