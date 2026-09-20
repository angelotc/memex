//! CLI surface for `memex wiki-loop <subcommand>`.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::path::Path;

use super::config::WikiLoopConfig;
use super::digest;
use super::gates::{Proposal, apply as apply_proposal, rollback as rollback_skill};
use super::harness;
use super::ingest;
use super::ledger::StateLedger;
use super::lock::RoleLock;
use super::patterns::{PatternStore, scrub_op};
use super::proposer;
use super::queue::{QueueManager, now_ms};

#[derive(Debug, Subcommand)]
pub enum WikiLoopCommand {
    /// Enqueue a session event for compilation (call from session-end hooks; <10ms)
    Enqueue {
        /// Harness storage label as it appears in analytics (claude, codex, antigravity, ...)
        source: String,
        session_id: String,
        /// Optional project hint attached to the queue entry
        #[arg(long)]
        project: Option<String>,
        /// Mark the session as ended (per-turn hooks should omit this)
        #[arg(long)]
        ended: bool,
    },
    /// Run the Wiki Maintainer: drain the queue, update patterns, index, and logs
    RunMaintainer {
        /// Build the prompt and digest but do not invoke the maintainer model or write
        #[arg(long)]
        dry_run: bool,
        /// Write to the live wiki (~/.memex/wiki) instead of the staging wiki
        #[arg(long, conflicts_with = "dry_run")]
        live: bool,
    },
    /// Run the Skill Proposer: stage at most one atomic skill proposal
    RunProposer {
        #[arg(long)]
        dry_run: bool,
    },
    /// Re-run the validation gates on a staged proposal
    Validate {
        proposal_id: String,
        /// Skip the Tier-1 counterfactual judge
        #[arg(long)]
        skip_tier1: bool,
    },
    /// Apply a validated proposal to the live skills root (the human-approved step)
    Apply { proposal_id: String },
    /// Roll a skill back to its previous deployed version
    Rollback { skill_name: String },
    /// Show queue, DLQ, ledger, and wiki statistics
    Status,
    /// Check prerequisites (binaries, paths, stores)
    Doctor,
}

pub fn run(command: WikiLoopCommand) -> Result<()> {
    match command {
        WikiLoopCommand::Enqueue {
            source,
            session_id,
            project,
            ended,
        } => {
            let cfg = WikiLoopConfig::load(None)?;
            let queue = QueueManager::new(&cfg.queue_dir)?;
            let path = queue.enqueue(&source, &session_id, project.as_deref(), ended)?;
            println!("enqueued {}:{} -> {}", source, session_id, path.display());
            Ok(())
        }
        WikiLoopCommand::RunMaintainer { dry_run, live } => run_maintainer(dry_run, live),
        WikiLoopCommand::RunProposer { dry_run } => {
            let cfg = WikiLoopConfig::load(None)?;
            let ledger = StateLedger::open(&cfg.state_db)?;
            println!("{}", proposer::run_proposer(&cfg, &ledger, dry_run)?);
            Ok(())
        }
        WikiLoopCommand::Validate {
            proposal_id,
            skip_tier1,
        } => {
            let cfg = WikiLoopConfig::load(None)?;
            let ledger = StateLedger::open(&cfg.state_db)?;
            run_validate(&cfg, &ledger, &proposal_id, skip_tier1)
        }
        WikiLoopCommand::Apply { proposal_id } => {
            let cfg = WikiLoopConfig::load(None)?;
            let ledger = StateLedger::open(&cfg.state_db)?;
            println!("{}", apply_proposal(&cfg, &ledger, &proposal_id)?);
            Ok(())
        }
        WikiLoopCommand::Rollback { skill_name } => {
            let cfg = WikiLoopConfig::load(None)?;
            let ledger = StateLedger::open(&cfg.state_db)?;
            println!("{}", rollback_skill(&cfg, &ledger, &skill_name)?);
            Ok(())
        }
        WikiLoopCommand::Status => {
            let cfg = WikiLoopConfig::load(None)?;
            run_status(&cfg)
        }
        WikiLoopCommand::Doctor => {
            let cfg = WikiLoopConfig::load(None)?;
            run_doctor(&cfg)
        }
    }
}

fn run_maintainer(dry_run: bool, live: bool) -> Result<()> {
    let cfg = WikiLoopConfig::load(None)?;
    let lock_dir = cfg
        .state_db
        .parent()
        .map(|p| p.join("locks"))
        .context("state db has no parent directory")?;

    // Single-instance per role; cron overlap exits cleanly.
    let _lock = match RoleLock::try_acquire(&lock_dir, "maintainer") {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("note: {e}; exiting");
            return Ok(());
        }
    };

    let ledger = StateLedger::open(&cfg.state_db)?;

    // Circuit breaker: after 3 consecutive maintainer failures, stay quiet and notify
    // instead of burning model budget every cron tick.
    let failures = ledger.consecutive_failures("maintainer")?;
    if failures >= 3 && !dry_run {
        let msg = format!(
            "maintainer disabled after {failures} consecutive failures; inspect `memex wiki-loop status`"
        );
        if cfg.notify_on_failure {
            super::notify::notify("Wiki-Loop maintainer failing", Some(&msg)).ok();
        }
        bail!("{msg}");
    }

    let run_id = ledger.start_run("maintainer")?;
    let result = execute_maintainer(&cfg, &ledger, run_id, dry_run, live);
    if let Err(e) = &result {
        let _ = ledger.finish_run(run_id, "error", 0, 0, Some(&format!("{e:#}")));
        if cfg.notify_on_failure {
            super::notify::notify(
                "Wiki-Loop maintainer failed",
                Some(&format!("{e:#}").chars().take(180).collect::<String>()),
            )
            .ok();
        }
    }
    result
}

fn execute_maintainer(
    cfg: &WikiLoopConfig,
    ledger: &StateLedger,
    run_id: i64,
    dry_run: bool,
    live: bool,
) -> Result<()> {
    let queue = QueueManager::new(&cfg.queue_dir)?;
    let claimed = queue.claim(cfg.max_batch_size, cfg.quiet_minutes)?;
    if claimed.is_empty() {
        ledger.finish_run(run_id, "ok", 0, 0, None)?;
        println!("queue empty or no sessions cleared the quiet window");
        return Ok(());
    }
    println!("claimed {} session(s)", claimed.len());

    let analytics_db = cfg.analytics_db();
    let index_dir = cfg.index_dir()?;
    let mut batch: Vec<(super::queue::QueueEntry, u64, ingest::SessionMeta, String)> = Vec::new();

    for (entry, fingerprint) in &claimed {
        if ledger.is_processed(&entry.source, &entry.session_id) {
            let _ = queue.ack_if_unchanged(entry, *fingerprint);
            continue;
        }
        let meta = match ingest::get_session_meta(&analytics_db, &entry.source, &entry.session_id)?
        {
            Some(meta) => meta,
            None => {
                // Ingest lag: retry with backoff, dead-letter after 5 attempts.
                queue.nack(entry, "session not yet in analytics (ingest lag)", 5)?;
                continue;
            }
        };
        if meta.message_count < cfg.min_turns {
            println!(
                "session {}:{} has {} turns (< {}); skipping as trivial",
                meta.source, meta.session_id, meta.message_count, cfg.min_turns
            );
            ledger.mark_processed(&entry.source, &entry.session_id, "skipped_trivial", &[])?;
            let _ = queue.ack_if_unchanged(entry, *fingerprint);
            continue;
        }
        let records = match ingest::load_records(&index_dir, &meta) {
            Ok(records) => records,
            Err(e) => {
                queue.nack(entry, &format!("{e:#}"), 5)?;
                continue;
            }
        };
        let indices = digest::extract_error_turn_indices(&records);
        if indices.is_empty() {
            ledger.mark_processed(&entry.source, &entry.session_id, "ok", &[])?;
            let _ = queue.ack_if_unchanged(entry, *fingerprint);
            continue;
        }
        let summary = digest::build_session_summary(cfg, &meta, &records, &indices);
        batch.push((entry.clone(), *fingerprint, meta, summary));
    }

    if batch.is_empty() {
        ledger.finish_run(run_id, "ok", 0, 0, None)?;
        println!("no sessions required pattern analysis");
        return Ok(());
    }

    let wiki_dir = cfg.wiki_dir(live);
    let store = PatternStore::new(&wiki_dir)?;

    let summaries: Vec<String> = batch.iter().map(|(_, _, _, s)| s.clone()).collect();
    let batch_digest = digest::build_batch_digest(cfg, &summaries);

    let catalog: Vec<super::patterns::PatternMeta> = store
        .catalog()?
        .into_values()
        .map(|(_, meta)| meta)
        .collect();
    let catalog_summary = if catalog.is_empty() {
        "None yet.".to_string()
    } else {
        let mut lines = vec!["id | slug | title | scope | status".to_string()];
        for meta in &catalog {
            lines.push(format!(
                "{} | {} | {} | {} | {}",
                meta.id,
                meta.file.trim_end_matches(".md"),
                meta.title,
                meta.scope,
                meta.status
            ));
        }
        lines.join("\n")
    };

    let prompt = super::prompts::MAINTAINER
        .replace("{digest}", &format!(
            "### Existing Pattern Catalog\n\n{catalog_summary}\n\n### Error Trace Digest\n\n{batch_digest}"
        ));

    if dry_run {
        ledger.finish_run(run_id, "ok", batch.len() as i64, 0, None)?;
        println!(
            "dry run: maintainer prompt built ({} chars) from {} session(s); queue left untouched",
            prompt.len(),
            batch.len()
        );
        println!("--- prompt preview ---");
        println!("{}", prompt.chars().take(1200).collect::<String>());
        return Ok(());
    }

    // ---- Maintain (paper step 10). The model only emits JSON; the orchestrator owns
    // every write to the wiki.
    let raw = harness::run_role(
        &cfg.maintainer,
        &prompt,
        None,
        std::time::Duration::from_secs(cfg.subprocess_timeout_secs),
    )?;
    let value = harness::extract_json_object(&raw)?;
    #[derive(serde::Deserialize)]
    struct MaintainerOutput {
        #[serde(default)]
        patterns: Vec<super::patterns::PatternOp>,
        #[serde(default)]
        summary: String,
    }
    let output: MaintainerOutput =
        serde_json::from_value(value).context("parsing maintainer JSON output")?;

    let mut written = 0i64;
    let mut pattern_ids: Vec<String> = Vec::new();
    let mut log_lines = vec![format!(
        "## {} — maintainer run {} over {} session(s)",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        run_id,
        batch.len()
    )];

    for op in &output.patterns {
        let (scrubbed, quarantined) = scrub_op(op);
        // Evidence correlation: only cite sessions that were actually in this digest.
        let known: std::collections::HashSet<&str> = batch
            .iter()
            .map(|(_, _, m, _)| m.session_id.as_str())
            .collect();
        let evidence: Vec<super::patterns::Corroboration> = scrubbed
            .evidence_session_ids
            .iter()
            .filter(|sid| known.contains(sid.as_str()))
            .filter_map(|sid| {
                batch
                    .iter()
                    .find(|(_, _, m, _)| m.session_id == *sid)
                    .map(|(_, _, m, _)| super::patterns::Corroboration {
                        source: m.source.clone(),
                        session_id: sid.clone(),
                        ts: m.last_at,
                    })
            })
            .collect();

        let outcome = if quarantined {
            let path = store.quarantine(&scrubbed, &evidence)?;
            println!("quarantined suspect pattern -> {}", path.display());
            format!("quarantined {}", scrubbed.slug)
        } else {
            match store.apply_op(op, &scrubbed, &evidence) {
                Ok(write) => {
                    pattern_ids.push(write.pattern_id.clone());
                    println!(
                        "{} pattern {} ({}) -> {}",
                        write.action,
                        scrubbed.slug,
                        write.pattern_id,
                        write.file.display()
                    );
                    format!("{} {} ({})", write.action, scrubbed.slug, write.pattern_id)
                }
                Err(e) => {
                    eprintln!("warning: pattern op `{}` failed: {e:#}", op.action);
                    format!("failed {} {}: {e}", op.action, scrubbed.slug)
                }
            }
        };
        log_lines.push(format!("- {outcome}"));
        written += 1;
    }
    log_lines.push(format!(
        "- summary: {}",
        if output.summary.is_empty() {
            "(none)"
        } else {
            &output.summary
        }
    ));
    store.append_log(&log_lines.join("\n"))?;
    store.append_log("")?;
    store.update_index()?;

    // ---- Ack only entries that did not change mid-run; mark processed with the
    // patterns each session contributed (provenance join).
    for (entry, fingerprint, _, _) in &batch {
        let contributed: Vec<String> = pattern_ids
            .iter()
            .filter(|_| {
                output
                    .patterns
                    .iter()
                    .any(|op| op.evidence_session_ids.contains(&entry.session_id))
            })
            .cloned()
            .collect();
        ledger.mark_processed(&entry.source, &entry.session_id, "ok", &contributed)?;
        let acked = queue.ack_if_unchanged(entry, *fingerprint)?;
        if !acked {
            eprintln!(
                "note: {}:{} was updated mid-run; left queued for the next pass",
                entry.source, entry.session_id
            );
        }
    }

    ledger.finish_run(run_id, "ok", batch.len() as i64, written, None)?;
    println!(
        "maintainer finished: {} session(s), {} pattern op(s) -> {}",
        batch.len(),
        written,
        wiki_dir.display()
    );
    Ok(())
}

fn run_validate(
    cfg: &WikiLoopConfig,
    ledger: &StateLedger,
    proposal_id: &str,
    skip_tier1: bool,
) -> Result<()> {
    let mut proposal = Proposal::load(&cfg.proposals_dir, proposal_id)?;
    let skill_md = proposal.skill_markdown(&cfg.proposals_dir)?;
    let existing =
        std::fs::read_to_string(cfg.skills_root.join(&proposal.skill_name).join("SKILL.md")).ok();

    let mut all_passed = true;
    let mut results = super::gates::tier0(cfg, ledger, &proposal, &skill_md, existing.as_deref())?;
    for (name, result) in results.drain(..) {
        all_passed &= result.passed;
        println!(
            "{name}: {} — {}",
            if result.passed { "pass" } else { "FAIL" },
            result.detail
        );
        proposal.gates.insert(name, result);
    }

    if all_passed && !skip_tier1 {
        match super::gates::tier1(cfg, &proposal, &skill_md) {
            Ok(Some(result)) => {
                all_passed &= result.passed;
                println!(
                    "tier1: {} — {}",
                    if result.passed { "pass" } else { "FAIL" },
                    result.detail
                );
                proposal.gates.insert("tier1".into(), result);
            }
            Ok(None) => {}
            Err(e) => {
                all_passed = false;
                println!("tier1: FAIL — judge errored: {e:#}");
                proposal.gates.insert(
                    "tier1".into(),
                    super::gates::GateResult {
                        passed: false,
                        detail: format!("judge errored: {e:#}"),
                    },
                );
            }
        }
    }

    proposal.status = if all_passed { "validated" } else { "rejected" }.into();
    proposal.save(&cfg.proposals_dir)?;
    ledger.set_proposal_status(
        proposal_id,
        &proposal.status,
        &serde_json::to_string(&proposal.gates)?,
    )?;
    println!("proposal {proposal_id}: {}", proposal.status);
    Ok(())
}

fn run_status(cfg: &WikiLoopConfig) -> Result<()> {
    let queue = QueueManager::new(&cfg.queue_dir)?;
    let (pending, dlq) = queue.counts()?;
    let ledger = StateLedger::open(&cfg.state_db)?;
    let staging_patterns = count_patterns(&cfg.wiki_dir(false));
    let live_patterns = count_patterns(&cfg.wiki_dir(true));

    println!("=== wiki-loop status ===");
    println!(
        "queue:      {pending} pending, {dlq} dead-lettered ({})",
        cfg.queue_dir.display()
    );
    println!(
        "wiki:       {live_patterns} live pattern(s) in {}, {staging_patterns} staged in {}",
        cfg.wiki_root.display(),
        cfg.wiki_staging.display()
    );
    println!("skills:     {}", cfg.skills_root.display());
    println!("proposals:  {}", cfg.proposals_dir.display());
    let week_ago = now_ms() - 7 * 24 * 3_600_000;
    println!(
        "proposals:  {}/week used (ceiling {})",
        ledger.proposals_since(week_ago)?,
        cfg.max_proposals_per_week
    );
    let failures = ledger.consecutive_failures("maintainer")?;
    if failures > 0 {
        println!("health:     {failures} consecutive maintainer failure(s)");
    } else {
        println!("health:     ok");
    }
    println!("\nrecent runs:");
    let runs = ledger.recent_runs(5)?;
    if runs.is_empty() {
        println!("  (none)");
    }
    for r in runs {
        println!(
            "  #{} {} {}: read={} written={}{}",
            r.id,
            r.role,
            r.status,
            r.sessions_read,
            r.patterns_written,
            r.error
                .as_ref()
                .map(|e| format!(" error={}", e.chars().take(80).collect::<String>()))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn count_patterns(wiki_dir: &Path) -> usize {
    std::fs::read_dir(wiki_dir.join("patterns"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
                .count()
        })
        .unwrap_or(0)
}

fn run_doctor(cfg: &WikiLoopConfig) -> Result<()> {
    let mut ok = true;
    println!("=== wiki-loop doctor ===");

    let analytics = cfg.analytics_db();
    if analytics.exists() {
        println!("[ok] analytics store: {}", analytics.display());
    } else {
        println!(
            "[warn] analytics store missing: {} (run `memex index` first)",
            analytics.display()
        );
    }

    match cfg.index_dir() {
        Ok(index_dir) if index_dir.exists() => {
            println!("[ok] search index: {}", index_dir.display())
        }
        _ => {
            println!("[warn] search index missing; maintainer will rely on raw transcript fallback")
        }
    }

    for dir in [
        &cfg.queue_dir,
        &cfg.state_db
            .parent()
            .unwrap_or(Path::new("/tmp"))
            .to_path_buf(),
        &cfg.proposals_dir,
        &cfg.wiki_staging,
    ] {
        match std::fs::create_dir_all(dir) {
            Ok(()) => println!("[ok] writable: {}", dir.display()),
            Err(e) => {
                ok = false;
                println!("[fail] cannot create {}: {e}", dir.display());
            }
        }
    }

    for bin in ["agy", "claude", "herdr"] {
        match super::notify::which_bin(bin) {
            Some(path) => println!("[ok] binary `{bin}`: {}", path.display()),
            None => println!("[warn] binary `{bin}` not on PATH"),
        }
    }

    if !ok {
        bail!("doctor found blocking issues");
    }
    Ok(())
}
