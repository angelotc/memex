//! State ledger (`state.db`): processed sessions, run history, proposals, skill
//! deployments, and holdout scopes. One SQLite store, WAL mode.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};

use super::queue::now_ms;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS processed (
  source TEXT NOT NULL,
  session_id TEXT NOT NULL,
  processed_at INTEGER NOT NULL,
  outcome TEXT NOT NULL,
  attempts INTEGER DEFAULT 0,
  pattern_ids TEXT NOT NULL DEFAULT '[]',
  compiled_last_event_at INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (source, session_id)
);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  role TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  status TEXT NOT NULL DEFAULT 'running',
  sessions_read INTEGER DEFAULT 0,
  patterns_written INTEGER DEFAULT 0,
  error TEXT
);
CREATE TABLE IF NOT EXISTS proposals (
  id TEXT PRIMARY KEY,
  created_at INTEGER NOT NULL,
  skill_name TEXT NOT NULL,
  status TEXT NOT NULL,
  gate_results TEXT NOT NULL DEFAULT '{}',
  pattern_ids TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE IF NOT EXISTS deployment (
  skill_id TEXT NOT NULL,
  version INTEGER NOT NULL,
  active_from INTEGER NOT NULL,
  active_to INTEGER,
  scope TEXT NOT NULL,
  proposal_id TEXT NOT NULL,
  PRIMARY KEY (skill_id, version)
);
CREATE TABLE IF NOT EXISTS holdout (
  scope TEXT PRIMARY KEY,
  kind TEXT NOT NULL
);
"#;

pub struct StateLedger {
    conn: Connection,
    #[allow(dead_code)]
    path: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RunRow {
    pub id: i64,
    pub role: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub status: String,
    pub sessions_read: i64,
    pub patterns_written: i64,
    pub error: Option<String>,
}

impl StateLedger {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening state db {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA_SQL)?;
        // Migration for pre-`compiled_last_event_at` databases (CREATE TABLE IF NOT
        // EXISTS does not add columns); a duplicate-column error means it is already there.
        let _ = conn.execute(
            "ALTER TABLE processed ADD COLUMN compiled_last_event_at INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Crash recovery: a `running` run older than two hours can never report back
        // (nothing holds a run lease that long) — close it as an error so the circuit
        // breaker counts it instead of skipping it forever.
        let now = now_ms();
        let stale = conn
            .execute(
                "UPDATE runs SET status = 'error', error = 'stale running run (crashed?)',
                                   ended_at = ?1
                 WHERE status = 'running' AND started_at < ?2",
                params![now, now - 2 * 3_600_000],
            )
            .context("reaping stale running runs")?;
        if stale > 0 {
            eprintln!("wiki-loop: marked {stale} stale running run(s) as errored (crashed?)");
        }
        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    // ---- processed sessions ----

    /// A session counts as processed only through the `last_event_at` it was compiled
    /// at: a session resumed after compilation (new turns in the queue entry) is due a
    /// re-compile of its new activity, not a skip.
    pub fn is_processed(&self, source: &str, session_id: &str, last_event_at: i64) -> bool {
        self.conn
            .query_row(
                "SELECT outcome, compiled_last_event_at FROM processed
                 WHERE source = ? AND session_id = ?",
                params![source, session_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .map(|(outcome, compiled_at)| {
                (outcome == "ok" || outcome == "skipped_trivial") && compiled_at >= last_event_at
            })
            .unwrap_or(false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mark_processed(
        &self,
        source: &str,
        session_id: &str,
        outcome: &str,
        pattern_ids: &[String],
        compiled_last_event_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO processed
               (source, session_id, processed_at, outcome, pattern_ids, compiled_last_event_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(source, session_id) DO UPDATE SET
               processed_at = excluded.processed_at,
               outcome = excluded.outcome,
               pattern_ids = excluded.pattern_ids,
               compiled_last_event_at = excluded.compiled_last_event_at",
            params![
                source,
                session_id,
                now_ms(),
                outcome,
                serde_json::to_string(pattern_ids)?,
                compiled_last_event_at
            ],
        )?;
        Ok(())
    }

    // ---- runs ----

    pub fn start_run(&self, role: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (role, started_at, status) VALUES (?, ?, 'running')",
            params![role, now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_run(
        &self,
        run_id: i64,
        status: &str,
        sessions_read: i64,
        patterns_written: i64,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET ended_at = ?, status = ?, sessions_read = ?, patterns_written = ?, error = ?
             WHERE id = ?",
            params![now_ms(), status, sessions_read, patterns_written, error, run_id],
        )?;
        Ok(())
    }

    pub fn recent_runs(&self, limit: i64) -> Result<Vec<RunRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, started_at, ended_at, status, sessions_read, patterns_written, error
             FROM runs ORDER BY started_at DESC LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok(RunRow {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    started_at: row.get(2)?,
                    ended_at: row.get(3)?,
                    status: row.get(4)?,
                    sessions_read: row.get(5)?,
                    patterns_written: row.get(6)?,
                    error: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Consecutive most-recent maintainer failures (for the circuit breaker).
    pub fn consecutive_failures(&self, role: &str) -> Result<u32> {
        let mut stmt = self
            .conn
            .prepare("SELECT status FROM runs WHERE role = ? ORDER BY started_at DESC LIMIT 10")?;
        let mut failures = 0u32;
        let mut rows = stmt.query(params![role])?;
        while let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            if status == "ok" {
                break;
            }
            if status == "error" {
                failures += 1;
            }
        }
        Ok(failures)
    }

    // ---- proposals ----

    pub fn insert_proposal(
        &self,
        id: &str,
        skill_name: &str,
        pattern_ids: &[String],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO proposals (id, created_at, skill_name, status, pattern_ids)
             VALUES (?, ?, ?, 'pending', ?)",
            params![
                id,
                now_ms(),
                skill_name,
                serde_json::to_string(pattern_ids)?
            ],
        )?;
        Ok(())
    }

    pub fn set_proposal_status(&self, id: &str, status: &str, gate_results: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE proposals SET status = ?, gate_results = ? WHERE id = ?",
            params![status, gate_results, id],
        )?;
        Ok(())
    }

    pub fn rejected_proposals_since(&self, since_ms: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT skill_name FROM proposals WHERE status = 'rejected' AND created_at >= ?",
        )?;
        let names = stmt
            .query_map(params![since_ms], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names)
    }

    pub fn proposals_since(&self, since_ms: i64) -> Result<u32> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM proposals WHERE created_at >= ?",
                params![since_ms],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n as u32)
            .context("counting proposals")
    }

    // ---- deployment ----

    /// Highest active (non-retired) deployment version of a skill. Rollback uses this
    /// to find the version it is undoing; it is NOT the next version to allocate.
    pub fn latest_deployment_version(&self, skill_id: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT MAX(version) FROM deployment WHERE skill_id = ? AND active_to IS NULL",
                params![skill_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .context("querying latest deployment version")
    }

    /// Next free deployment version for a skill, counting ALL rows (active or retired)
    /// — 1 for a skill with no history. Version numbers are immutable history
    /// ((skill_id, version) is the PK), so a rolled-back skill re-applies at v{n+1}
    /// instead of colliding with the retired v1 row.
    pub fn next_deployment_version(&self, skill_id: &str) -> Result<i64> {
        let max = self
            .conn
            .query_row(
                "SELECT MAX(version) FROM deployment WHERE skill_id = ?",
                params![skill_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .context("querying next deployment version")?;
        Ok(max.unwrap_or(0) + 1)
    }

    pub fn record_deployment(
        &self,
        skill_id: &str,
        version: i64,
        scope: &str,
        proposal_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO deployment (skill_id, version, active_from, active_to, scope, proposal_id)
             VALUES (?, ?, ?, NULL, ?, ?)",
            params![skill_id, version, now_ms(), scope, proposal_id],
        )?;
        Ok(())
    }

    pub fn retire_deployments(&self, skill_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE deployment SET active_to = ? WHERE skill_id = ? AND active_to IS NULL",
            params![now_ms(), skill_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> (StateLedger, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let led = StateLedger::open(&tmp.path().join("state.db")).expect("ledger");
        (led, tmp)
    }

    #[test]
    fn processed_roundtrip() {
        let (led, _tmp) = ledger();
        assert!(!led.is_processed("claude", "s1", 0));
        led.mark_processed("claude", "s1", "ok", &["pat_1".into()], 100)
            .expect("mark");
        assert!(led.is_processed("claude", "s1", 100));
        // error outcome does not count as processed
        led.mark_processed("claude", "s2", "error", &[], 100)
            .expect("mark");
        assert!(!led.is_processed("claude", "s2", 100));
    }

    #[test]
    fn resumed_sessions_are_recompiled_for_new_turns() {
        let (led, _tmp) = ledger();
        led.mark_processed("claude", "s1", "ok", &[], 100)
            .expect("mark");
        // Same last_event_at: already compiled.
        assert!(led.is_processed("claude", "s1", 100));
        // The session resumed and the queue entry's last_event_at moved: re-compile.
        assert!(!led.is_processed("claude", "s1", 250));
        led.mark_processed("claude", "s1", "ok", &[], 250)
            .expect("re-mark");
        assert!(led.is_processed("claude", "s1", 250));
    }

    #[test]
    fn run_lifecycle_and_circuit_breaker() {
        let (led, _tmp) = ledger();
        let id = led.start_run("maintainer").expect("start");
        led.finish_run(id, "error", 3, 0, Some("boom"))
            .expect("finish");
        let id = led.start_run("maintainer").expect("start");
        led.finish_run(id, "error", 1, 0, None).expect("finish");
        assert_eq!(led.consecutive_failures("maintainer").expect("failures"), 2);
        let id = led.start_run("maintainer").expect("start");
        led.finish_run(id, "ok", 1, 2, None).expect("finish");
        assert_eq!(led.consecutive_failures("maintainer").expect("failures"), 0);
        assert_eq!(led.recent_runs(5).expect("runs").len(), 3);
    }

    #[test]
    fn proposal_rate_limit_query() {
        let (led, _tmp) = ledger();
        led.insert_proposal("prop_1", "my-skill", &[])
            .expect("insert");
        assert_eq!(led.proposals_since(now_ms() - 1000).expect("count"), 1);
        led.set_proposal_status("prop_1", "rejected", "{}")
            .expect("status");
        assert_eq!(
            led.rejected_proposals_since(now_ms() - 1000)
                .expect("rejected"),
            vec!["my-skill".to_string()]
        );
    }

    #[test]
    fn deployment_versions() {
        let (led, _tmp) = ledger();
        assert_eq!(led.latest_deployment_version("sk").expect("v"), None);
        led.record_deployment("sk", 1, "global", "prop_1")
            .expect("dep");
        assert_eq!(led.latest_deployment_version("sk").expect("v"), Some(1));
        led.retire_deployments("sk").expect("retire");
        assert_eq!(led.latest_deployment_version("sk").expect("v"), None);
    }

    #[test]
    fn next_deployment_version_counts_retired_rows() {
        let (led, _tmp) = ledger();
        assert_eq!(led.next_deployment_version("sk").expect("next"), 1);
        led.record_deployment("sk", 1, "global", "prop_1")
            .expect("dep");
        led.retire_deployments("sk").expect("retire");
        // Active-only lookup resets to None, but the next version must not reuse v1.
        assert_eq!(led.latest_deployment_version("sk").expect("v"), None);
        assert_eq!(led.next_deployment_version("sk").expect("next"), 2);
        led.record_deployment("sk", 2, "global", "prop_2")
            .expect("dep");
        assert_eq!(led.latest_deployment_version("sk").expect("v"), Some(2));
        assert_eq!(led.next_deployment_version("sk").expect("next"), 3);
    }

    #[test]
    fn stale_running_runs_are_marked_error_on_open() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("state.db");
        {
            let led = StateLedger::open(&path).expect("ledger");
            let stale_id = led.start_run("maintainer").expect("start");
            led.start_run("maintainer").expect("start");
            // Age the first run past the 2h stale window.
            led.conn
                .execute(
                    "UPDATE runs SET started_at = ?1 WHERE id = ?2",
                    params![now_ms() - 3 * 3_600_000, stale_id],
                )
                .expect("age");
        }
        let led = StateLedger::open(&path).expect("reopen");
        let runs = led.recent_runs(10).expect("runs");
        let stale = runs
            .iter()
            .find(|r| r.error.as_deref() == Some("stale running run (crashed?)"))
            .expect("stale run reaped");
        assert_eq!(stale.status, "error");
        assert!(stale.ended_at.is_some());
        // A run still inside the window is left running.
        let fresh = runs
            .iter()
            .find(|r| r.error.is_none())
            .expect("fresh run untouched");
        assert_eq!(fresh.status, "running");
        assert_eq!(led.consecutive_failures("maintainer").expect("failures"), 1);
    }
}
