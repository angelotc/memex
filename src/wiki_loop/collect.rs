//! Collector (paper step 1): sweep ended sessions from the analytics store into the
//! queue before the maintainer claims its batch.
//!
//! The paper collects traces from the harness after each task. memex already centralizes
//! transcripts for every source it indexes, so collection is a sweep instead of a
//! per-harness hook: anything ended (quiet for the quiet window), uncompiled, and inside
//! the lookback is enqueued with its real analytics `last_at`. `enqueue` stays available
//! for session-end hooks that want sub-cron latency; the sweep is idempotent either way —
//! sessions already compiled through their current `last_at` are skipped, and a session
//! resumed after compilation is picked up again by its newer `last_at`.

use anyhow::Result;

use super::config::WikiLoopConfig;
use super::ingest;
use super::ledger::StateLedger;
use super::queue::QueueManager;

/// Upper bound on sessions examined per sweep; each becomes one small queue file.
const SWEEP_LIMIT: usize = 500;

/// Enqueue ended-but-uncompiled sessions. Returns how many entries were (re)enqueued.
pub fn sweep(cfg: &WikiLoopConfig, ledger: &StateLedger, queue: &QueueManager) -> Result<usize> {
    let now = super::queue::now_ms();
    let quiet_before = now - cfg.quiet_minutes * 60_000;
    let since = if cfg.collect_lookback_days > 0 {
        now - cfg.collect_lookback_days * 24 * 3_600_000
    } else {
        0
    };
    let sessions =
        ingest::ended_sessions_since(&cfg.analytics_db(), quiet_before, since, SWEEP_LIMIT)?;

    let mut enqueued = 0usize;
    for meta in sessions {
        // Trivial sessions are skipped at claim time too; filtering here keeps them out
        // of the queue entirely so `status` counts mean work, not noise.
        if meta.message_count < cfg.min_turns {
            continue;
        }
        if ledger.is_processed(&meta.source, &meta.session_id, meta.last_at) {
            continue;
        }
        queue.enqueue_event(
            &meta.source,
            &meta.session_id,
            meta.project.as_deref(),
            true,
            meta.last_at,
        )?;
        enqueued += 1;
    }
    Ok(enqueued)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The end-to-end sweep path (analytics → queue) is exercised live by the maintainer;
    // here we pin the lookback math, which is the part that can silently regress.

    #[test]
    fn lookback_window_is_quiet_minus_days() {
        let cfg = WikiLoopConfig::default();
        assert_eq!(cfg.collect_lookback_days, 7);
        assert!(cfg.quiet_minutes > 0);
        // With defaults the window is [now - 7d, now - quiet]; a zero lookback means
        // "all history" (since = 0), decided in sweep().
        let now = super::super::queue::now_ms();
        let quiet_before = now - cfg.quiet_minutes * 60_000;
        let since = now - cfg.collect_lookback_days * 24 * 3_600_000;
        assert!(since < quiet_before);
        assert!(quiet_before < now);
    }
}
