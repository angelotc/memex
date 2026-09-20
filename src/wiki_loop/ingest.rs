//! Raw-layer reads (paper layer 1): session metadata from the analytics store
//! (read-only SQLite) and full turn records from the memex index, with a
//! per-source transcript parser fallback when the index lags ingest.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use std::path::Path;
use std::sync::atomic::AtomicU64;

use crate::types::Record;

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub source: String,
    pub session_id: String,
    pub source_path: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub git_root: Option<String>,
    pub repo_project: Option<String>,
    #[allow(dead_code)]
    pub started_at: i64,
    pub last_at: i64,
    pub message_count: i64,
    pub resolution_status: Option<String>,
}

/// Look up one session in `analytics.sqlite` in read-only mode (same pragmas as
/// `AnalyticsStore::open_read_only`). Returns `None` when the session is not yet ingested.
pub fn get_session_meta(
    analytics_db: &Path,
    source: &str,
    session_id: &str,
) -> Result<Option<SessionMeta>> {
    if !analytics_db.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        analytics_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening analytics db {}", analytics_db.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.pragma_update(None, "query_only", true)?;

    let mut stmt = conn.prepare(
        "SELECT source, session_id, source_path, project, cwd, git_root, repo_project,
                started_at, last_at, message_count, resolution_status
         FROM sessions WHERE source = ? AND session_id = ?",
    )?;
    let mut rows = stmt.query(params![source, session_id])?;
    match rows.next()? {
        None => Ok(None),
        Some(row) => Ok(Some(SessionMeta {
            source: row.get(0)?,
            session_id: row.get(1)?,
            source_path: row.get(2)?,
            project: row.get(3)?,
            cwd: row.get(4)?,
            git_root: row.get(5)?,
            repo_project: row.get(6)?,
            started_at: row.get(7)?,
            last_at: row.get(8)?,
            message_count: row.get(9)?,
            resolution_status: row.get(10)?,
        })),
    }
}

/// Sessions for a repo project (most recent first) — used by the Tier-1 counterfactual
/// judge to find historical sessions that likely hit the target failure mode.
pub fn recent_sessions_for_project(
    analytics_db: &Path,
    repo_project: &str,
    limit: usize,
) -> Result<Vec<SessionMeta>> {
    if !analytics_db.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(
        analytics_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.pragma_update(None, "query_only", true)?;
    let mut stmt = conn.prepare(
        "SELECT source, session_id, source_path, project, cwd, git_root, repo_project,
                started_at, last_at, message_count, resolution_status
         FROM sessions WHERE repo_project = ? ORDER BY last_at DESC LIMIT ?",
    )?;
    let rows = stmt.query_map(params![repo_project, limit as i64], map_session_row)?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Sessions whose last activity falls in `[since_ms, quiet_before_ms]` — i.e. ended (they
/// have been quiet for at least the quiet window) and within the collector's lookback —
/// with at least `min_message_count` turns, oldest first, so the sweep drains history in
/// the order it happened and the limit counts only compilable sessions.
pub fn ended_sessions_since(
    analytics_db: &Path,
    quiet_before_ms: i64,
    since_ms: i64,
    min_message_count: i64,
    limit: usize,
) -> Result<Vec<SessionMeta>> {
    if !analytics_db.exists() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(
        analytics_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.pragma_update(None, "query_only", true)?;
    let mut stmt = conn.prepare(
        "SELECT source, session_id, source_path, project, cwd, git_root, repo_project,
                started_at, last_at, message_count, resolution_status
         FROM sessions
         WHERE last_at <= ? AND last_at >= ? AND message_count >= ?
         ORDER BY last_at ASC LIMIT ?",
    )?;
    let rows = stmt.query_map(
        params![quiet_before_ms, since_ms, min_message_count, limit as i64],
        map_session_row,
    )?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    Ok(SessionMeta {
        source: row.get(0)?,
        session_id: row.get(1)?,
        source_path: row.get(2)?,
        project: row.get(3)?,
        cwd: row.get(4)?,
        git_root: row.get(5)?,
        repo_project: row.get(6)?,
        started_at: row.get(7)?,
        last_at: row.get(8)?,
        message_count: row.get(9)?,
        resolution_status: row.get(10)?,
    })
}

/// A session's records are complete when they cover at least the message count the
/// analytics store already attributes to the session. The index commits in batches, so
/// a lagging index legitimately returns fewer — callers must treat that as "not ready",
/// never as "no errors".
pub fn records_complete(records: &[Record], meta: &SessionMeta) -> bool {
    !records.is_empty() && records.len() as i64 >= meta.message_count
}

/// Load a session's turn records: primary path is memex's own index
/// (`SearchIndex::records_by_session_id`); fallback re-parses the raw transcript file
/// for sources with JSONL parsers. An index read that returns a partial (or empty)
/// set for a session the analytics store knows about also triggers the fallback —
/// `records_by_session_id` cannot distinguish "not yet committed" from "no records".
/// Records are sorted into causal order.
pub fn load_records(index_dir: &Path, meta: &SessionMeta) -> Result<Vec<Record>> {
    let mut records = match crate::index::SearchIndex::open_or_create(index_dir) {
        Ok(idx) => match idx.records_by_session_id(&meta.session_id) {
            Ok(records) => records,
            Err(e) => {
                eprintln!(
                    "warning: index read failed for {}: {e}; falling back to raw parse",
                    meta.session_id
                );
                parse_raw_transcript(meta)?
            }
        },
        Err(e) => {
            eprintln!(
                "warning: index unavailable ({}); falling back to raw parse",
                e
            );
            parse_raw_transcript(meta)?
        }
    };
    if !records_complete(&records, meta) {
        // Partial index read: the raw transcript is the authoritative source for an
        // ended session. Keep whichever yields more records.
        if let Ok(raw) = parse_raw_transcript(meta)
            && raw.len() > records.len()
        {
            records = raw;
        }
    }
    records.sort_by_key(|r| (r.ts, r.turn_id));
    Ok(records)
}

/// Re-parse the raw transcript for the sources with push-style JSONL parsers.
/// Sources without one (SQLite-backed) return an error; the caller nacks for retry.
fn parse_raw_transcript(meta: &SessionMeta) -> Result<Vec<Record>> {
    use crate::sources::IndexParseState;

    let Some(path_str) = meta.source_path.as_deref() else {
        anyhow::bail!(
            "session {} has no source_path; cannot parse raw transcript",
            meta.session_id
        );
    };
    let path = Path::new(path_str);
    if !path.exists() {
        anyhow::bail!("raw transcript missing: {}", path.display());
    }

    let next_doc_id = AtomicU64::new(1);
    let mut records = Vec::new();
    let state = IndexParseState::default();
    match meta.source.as_str() {
        "claude" => {
            let _ = crate::sources::claude::parse_index_records_with_background(
                path,
                state,
                false,
                None,
                0,
                &next_doc_id,
                |r| {
                    records.push(r);
                    Ok(())
                },
            )?;
        }
        "codex" | "codex-session" | "codex-history" => {
            crate::sources::codex::parse_index_records_with_metadata_offsets(
                path,
                state,
                false,
                &next_doc_id,
                None,
                |r| {
                    records.push(r);
                    Ok(())
                },
            )?;
        }
        "antigravity" => {
            crate::sources::antigravity::parse_index_records(
                path,
                state,
                false,
                &next_doc_id,
                |r| {
                    records.push(r);
                    Ok(())
                },
            )?;
        }
        other => {
            anyhow::bail!("no raw-transcript fallback for source `{other}`; wait for index ingest")
        }
    }
    Ok(records
        .into_iter()
        .filter(|r| r.session_id == meta.session_id)
        .collect())
}
