//! Enqueue-only session queue with atomic upsert, quiet-window claiming, retry backoff,
//! and a dead-letter queue.
//!
//! Hooks call `enqueue` on session events; the maintainer claims only sessions that have
//! **ended** **and** sat out the quiet window, so mid-session turns are never compiled from
//! partial traces. Acknowledgement deletes an entry only if it was not updated while the
//! maintainer was running.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub schema: u8,
    pub source: String,
    pub session_id: String,
    #[serde(default)]
    pub project_hint: Option<String>,
    pub enqueued_at: i64,
    pub last_event_at: i64,
    #[serde(default)]
    pub ended: bool,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub next_retry_at: i64,
    #[serde(default)]
    pub last_error: Option<String>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn sanitize(component: &str) -> String {
    component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub struct QueueManager {
    queue_dir: PathBuf,
    dlq_dir: PathBuf,
}

impl QueueManager {
    pub fn new(queue_dir: &Path) -> Result<Self> {
        let queue_dir = queue_dir.to_path_buf();
        std::fs::create_dir_all(&queue_dir)?;
        let dlq_dir = queue_dir.join(".dlq");
        std::fs::create_dir_all(&dlq_dir)?;
        Ok(Self { queue_dir, dlq_dir })
    }

    fn entry_path(&self, source: &str, session_id: &str) -> PathBuf {
        self.queue_dir.join(format!(
            "{}--{}.json",
            sanitize(source),
            sanitize(session_id)
        ))
    }

    /// Whether a session has a dead-letter entry. The collector sweep consults this so
    /// a poison session stays parked in the DLQ instead of being re-enqueued with a
    /// fresh retry counter every cycle — the DLQ is the terminal, human-visible signal.
    pub fn is_dead_lettered(&self, source: &str, session_id: &str) -> bool {
        self.dlq_dir
            .join(
                self.entry_path(source, session_id)
                    .file_name()
                    .unwrap_or_default(),
            )
            .exists()
    }

    /// Atomically enqueue or upsert a session event, stamping the event at the current
    /// time (hook calls; see `enqueue_event` for the collector's analytics-time stamp).
    pub fn enqueue(
        &self,
        source: &str,
        session_id: &str,
        project_hint: Option<&str>,
        ended: bool,
    ) -> Result<PathBuf> {
        self.enqueue_event(source, session_id, project_hint, ended, now_ms())
    }

    /// Enqueue with an explicit event timestamp (the session's real `last_at` from
    /// analytics). `last_event_at` only ever moves forward across upserts, so a sweep
    /// cannot extend an entry's quiet window backwards, and `ended` is sticky. Re-enqueue
    /// preserves retry bookkeeping (idempotent upsert).
    pub fn enqueue_event(
        &self,
        source: &str,
        session_id: &str,
        project_hint: Option<&str>,
        ended: bool,
        last_event_at: i64,
    ) -> Result<PathBuf> {
        let path = self.entry_path(source, session_id);
        let existing: Option<QueueEntry> = self.read_entry(&path);

        let now = now_ms();
        let entry = QueueEntry {
            schema: SCHEMA_VERSION,
            source: source.to_string(),
            session_id: session_id.to_string(),
            project_hint: project_hint
                .map(str::to_string)
                .or_else(|| existing.as_ref().and_then(|e| e.project_hint.clone())),
            enqueued_at: existing.as_ref().map(|e| e.enqueued_at).unwrap_or(now),
            last_event_at: existing
                .as_ref()
                .map(|e| e.last_event_at.max(last_event_at))
                .unwrap_or(last_event_at),
            ended: ended || existing.as_ref().is_some_and(|e| e.ended),
            attempts: existing.as_ref().map(|e| e.attempts).unwrap_or(0),
            next_retry_at: existing.as_ref().map(|e| e.next_retry_at).unwrap_or(0),
            last_error: existing.as_ref().and_then(|e| e.last_error.clone()),
        };
        // The collector sweep re-visits every uncompiled session each run; when nothing
        // observable changed (same fingerprint), skip the disk churn entirely.
        if let Some(existing) = &existing
            && entry_fingerprint(&entry) == entry_fingerprint(existing)
        {
            return Ok(path);
        }
        self.write_atomic(&path, &entry)?;
        Ok(path)
    }

    fn read_entry(&self, path: &Path) -> Option<QueueEntry> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn write_atomic(&self, path: &Path, entry: &QueueEntry) -> Result<()> {
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        {
            let mut f = std::fs::File::create(&tmp)?;
            serde_json::to_writer_pretty(&mut f, entry)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Claim up to `max_batch_size` sessions that have ended **and** cleared the quiet
    /// window, oldest activity first (paper step 9: sample a context-fit subset of
    /// completed traces). Returns entries with a fingerprint of the on-disk bytes so
    /// `ack_if_unchanged` can detect concurrent updates.
    pub fn claim(
        &self,
        max_batch_size: usize,
        quiet_minutes: i64,
    ) -> Result<Vec<(QueueEntry, u64)>> {
        let now = now_ms();
        let quiet_ms = quiet_minutes * 60 * 1000;
        let mut eligible: Vec<QueueEntry> = Vec::new();
        for path in std::fs::read_dir(&self.queue_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().is_some_and(|e| e == "json")
                    && !p
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
        {
            let Some(entry) = self.read_entry(&path) else {
                continue;
            };
            if entry.next_retry_at > now {
                continue;
            }
            // Only complete sessions that have been quiet for the full window.
            if !entry.ended || now - entry.last_event_at < quiet_ms {
                continue;
            }
            eligible.push(entry);
        }
        // Oldest activity first — filename order is arbitrary, and the drain order is
        // user-visible in the wiki's evidence timeline.
        eligible.sort_by_key(|e| e.last_event_at);
        eligible.truncate(max_batch_size);
        Ok(eligible
            .into_iter()
            .map(|e| {
                let fp = entry_fingerprint(&e);
                (e, fp)
            })
            .collect())
    }

    /// Delete the entry after successful processing, but only if it was not updated while
    /// we worked on it. Returns `false` when the entry changed (caller must not mark the
    /// session processed; the next run will re-visit it).
    pub fn ack_if_unchanged(&self, entry: &QueueEntry, fingerprint: u64) -> Result<bool> {
        let path = self.entry_path(&entry.source, &entry.session_id);
        match self.read_entry(&path) {
            None => Ok(true), // already gone (e.g. DLQ'd concurrently): nothing to do
            Some(current) => {
                if entry_fingerprint(&current) == fingerprint {
                    std::fs::remove_file(&path)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    /// Record a processing failure: exponential backoff (15min · 2^(n−1)), then dead-letter.
    pub fn nack(&self, entry: &QueueEntry, error: &str, max_attempts: u32) -> Result<()> {
        let path = self.entry_path(&entry.source, &entry.session_id);
        let Some(mut current) = self.read_entry(&path) else {
            return Ok(()); // entry vanished; nothing to nack
        };
        current.attempts += 1;
        let now = now_ms();
        if current.attempts >= max_attempts {
            current.last_error = Some(error.to_string());
            let dlq_path = self.dlq_dir.join(
                path.file_name()
                    .ok_or_else(|| anyhow::anyhow!("queue entry has no filename"))?,
            );
            self.write_atomic(&dlq_path, &current)?;
            let _ = std::fs::remove_file(&path);
        } else {
            current.next_retry_at = now + 15 * 60 * 1000 * (1 << (current.attempts - 1).min(20));
            current.last_error = Some(error.to_string());
            self.write_atomic(&path, &current)?;
        }
        Ok(())
    }

    pub fn counts(&self) -> Result<(usize, usize)> {
        let pending = std::fs::read_dir(&self.queue_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count();
        let dlq = std::fs::read_dir(&self.dlq_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count();
        Ok((pending, dlq))
    }
}

/// Stable per-content fingerprint used for optimistic concurrency on ack.
fn entry_fingerprint(entry: &QueueEntry) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    entry.last_event_at.hash(&mut h);
    entry.ended.hash(&mut h);
    entry.attempts.hash(&mut h);
    entry.next_retry_at.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (QueueManager, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let qm = QueueManager::new(&tmp.path().join("queue")).expect("queue");
        (qm, tmp)
    }

    fn ended_entry(source: &str, id: &str) -> QueueEntry {
        QueueEntry {
            schema: 1,
            source: source.into(),
            session_id: id.into(),
            project_hint: None,
            enqueued_at: now_ms() - 3_600_000,
            last_event_at: now_ms() - 3_600_000,
            ended: true,
            attempts: 0,
            next_retry_at: 0,
            last_error: None,
        }
    }

    #[test]
    fn enqueue_upsert_is_atomic_and_preserves_bookkeeping() {
        let (qm, _tmp) = manager();
        qm.enqueue("claude", "s1", Some("proj"), false)
            .expect("enqueue");
        qm.enqueue("claude", "s1", None, false).expect("re-enqueue");
        let path = qm.entry_path("claude", "s1");
        let e = qm.read_entry(&path).expect("entry");
        assert!(e.project_hint.as_deref() == Some("proj"));
        assert!(!e.ended);
        // ended flag is sticky once any event marks the session ended
        qm.enqueue("claude", "s1", None, true).expect("end");
        let e = qm.read_entry(&path).expect("entry");
        assert!(e.ended);
    }

    #[test]
    fn enqueue_event_moves_last_event_at_forward_only() {
        let (qm, _tmp) = manager();
        // A hook stamped the session at wall-clock T; the sweep knows the session's
        // analytics last_at is slightly earlier — the earlier stamp must not win.
        let now = now_ms();
        qm.enqueue_event("claude", "s1", None, true, now)
            .expect("enqueue at hook time");
        qm.enqueue_event("claude", "s1", None, true, now - 5_000)
            .expect("sweep with analytics last_at");
        let path = qm.entry_path("claude", "s1");
        let e = qm.read_entry(&path).expect("entry");
        assert_eq!(e.last_event_at, now);

        // A resumed session pushes the stamp forward and re-opens compilation.
        qm.enqueue_event("claude", "s1", None, true, now + 60_000)
            .expect("resume");
        let e = qm.read_entry(&path).expect("entry");
        assert_eq!(e.last_event_at, now + 60_000);
    }

    #[test]
    fn identical_upsert_skips_the_write() {
        let (qm, _tmp) = manager();
        let now = now_ms();
        qm.enqueue_event("claude", "s1", None, true, now)
            .expect("enqueue");
        let path = qm.entry_path("claude", "s1");
        let mtime = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");

        // The sweep revisits the same session next run: nothing observable changed,
        // so the file must not be rewritten.
        qm.enqueue_event("claude", "s1", None, true, now)
            .expect("sweep no-op");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("stat")
                .modified()
                .expect("mtime"),
            mtime
        );

        // A newer event still writes.
        std::thread::sleep(std::time::Duration::from_millis(5));
        qm.enqueue_event("claude", "s1", None, true, now + 1)
            .expect("sweep with new event");
        assert_ne!(
            std::fs::metadata(&path)
                .expect("stat")
                .modified()
                .expect("mtime"),
            mtime
        );
    }

    #[test]
    fn claim_requires_ended_and_quiet() {
        let (qm, _tmp) = manager();
        qm.enqueue("claude", "active", None, false)
            .expect("enqueue");
        qm.enqueue("claude", "fresh-end", None, true)
            .expect("enqueue");
        // force quiet window to have elapsed for one entry
        let path = qm.entry_path("claude", "fresh-end");
        let mut e = qm.read_entry(&path).expect("entry");
        e.last_event_at = now_ms() - 3_600_000;
        qm.write_atomic(&path, &e).expect("write");

        let claimed = qm.claim(10, 20).expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].0.session_id, "fresh-end");
    }

    #[test]
    fn nack_backs_off_then_dead_letters() {
        let (qm, tmp) = manager();
        qm.enqueue("claude", "bad", None, true).expect("enqueue");
        let path = qm.entry_path("claude", "bad");
        let e = qm.read_entry(&path).expect("entry");
        qm.nack(&e, "fail 1", 2).expect("nack");
        let e = qm.read_entry(&path).expect("entry after nack");
        assert_eq!(e.attempts, 1);
        assert!(e.next_retry_at > now_ms());
        qm.nack(&e, "fail 2", 2).expect("nack");
        assert!(!path.exists(), "entry should be dead-lettered");
        let dlq = tmp.path().join("queue/.dlq/claude--bad.json");
        assert!(dlq.exists());
    }

    #[test]
    fn ack_detects_concurrent_update() {
        let (qm, _tmp) = manager();
        qm.enqueue("claude", "race", None, true).expect("enqueue");
        let path = qm.entry_path("claude", "race");
        let e = qm.read_entry(&path).expect("entry");
        let fp = entry_fingerprint(&e);

        // concurrent update before ack
        qm.enqueue("claude", "race", None, false).expect("update");
        let acked = qm.ack_if_unchanged(&e, fp).expect("ack");
        assert!(!acked, "must not ack an entry updated mid-run");
        assert!(path.exists());

        // unchanged entry acks cleanly
        let e2 = qm.read_entry(&path).expect("entry");
        let fp2 = entry_fingerprint(&e2);
        let acked = qm.ack_if_unchanged(&e2, fp2).expect("ack");
        assert!(acked);
        assert!(!path.exists());
    }
}
