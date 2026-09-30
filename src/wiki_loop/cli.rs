//! CLI surface for `memex wiki-loop <subcommand>` and the read-only `memex wiki <subcommand>`.

use anyhow::{Context, Result, bail};
use clap::{Subcommand, ValueEnum};
use std::path::PathBuf;

use super::config::WikiLoopConfig;
use super::digest;
use super::gates::{Proposal, apply as apply_proposal, rollback as rollback_skill};
use super::harness;
use super::ingest;
use super::ledger::StateLedger;
use super::lock::RoleLock;
use super::patterns::{PatternStore, scrub_op};
use super::proposer;
use super::queue::{QueueEntry, QueueManager, now_ms};
use super::search::{self, WikiSearchOptions};
use crate::types::Record;

#[derive(Debug, Subcommand)]
pub enum WikiLoopCommand {
    /// Enqueue a session event for compilation (optional; the maintainer's collector
    /// sweep also picks up ended sessions from analytics — hooks only buy sub-cron latency)
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
    /// Scaffold the config, create the wiki/skills directories, optionally install cron
    Init {
        /// Workspace root backing the loop: wiki at <root>/wiki, skills at <root>/skills
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Overwrite an existing config file
        #[arg(long)]
        force: bool,
        /// Install (or replace) the marked crontab block that runs the loop
        #[arg(long)]
        install_cron: bool,
    },
    /// Run the Wiki Maintainer: drain the queue, update patterns, index, and logs
    RunMaintainer {
        /// Build the prompt and digest but do not invoke the maintainer model or write
        #[arg(long)]
        dry_run: bool,
        /// Attempt one run even while the circuit breaker is tripped (after fixing the
        /// cause). A failed forced run leaves the breaker tripped.
        #[arg(long)]
        force: bool,
    },
    /// Run the Skill Proposer: stage at most one atomic skill proposal per run
    RunProposer {
        #[arg(long)]
        dry_run: bool,
        /// Attempt one run after fixing a tripped proposer circuit breaker
        #[arg(long)]
        force: bool,
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
    /// List dead-lettered sessions and their last processing error
    DeadLetters,
    /// Retry one dead-lettered session after correcting its cause
    Requeue { source: String, session_id: String },
    /// Show queue, DLQ, ledger, and wiki statistics
    Status,
    /// Check prerequisites (binaries, paths, stores)
    Doctor,
}

#[derive(Debug, Subcommand)]
pub enum WikiCommand {
    /// Search wiki patterns (failure modes + fixes) compiled from past agent sessions
    Search {
        /// Keywords or raw error text
        query: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Only global patterns plus those scoped to this project
        #[arg(long)]
        project: Option<String>,
        /// Only `failure` or `success` patterns
        #[arg(long, value_parser = ["failure", "success"])]
        kind: Option<String>,
        /// Include superseded patterns
        #[arg(long)]
        all: bool,
        /// Output encoding
        #[arg(long, value_enum, default_value = "text")]
        format: WikiFormat,
    },
    /// Print a wiki pattern page by id or slug
    Show { key: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WikiFormat {
    /// One card per hit, fields truncated
    Text,
    /// One untruncated hit per line
    Jsonl,
    /// A pretty-printed array of untruncated hits
    Json,
}

pub fn run_wiki(command: WikiCommand) -> Result<()> {
    let cfg = WikiLoopConfig::load(None)?;
    match command {
        WikiCommand::Search {
            query,
            limit,
            project,
            kind,
            all,
            format,
        } => {
            if limit == 0 {
                bail!("limit must be at least 1");
            }
            let opts = WikiSearchOptions {
                limit,
                project,
                kind,
                include_superseded: all,
            };
            let hits = search::search(&cfg.wiki_root, &query, &opts)?;
            match format {
                WikiFormat::Text => print!("{}", search::format_text(&hits)),
                WikiFormat::Jsonl => {
                    for hit in &hits {
                        println!("{}", serde_json::to_string(hit)?);
                    }
                }
                WikiFormat::Json => println!("{}", serde_json::to_string_pretty(&hits)?),
            }
            Ok(())
        }
        WikiCommand::Show { key } => {
            let path = search::resolve(&cfg.wiki_root, &key)?;
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            print!("{content}");
            if !content.ends_with('\n') {
                println!();
            }
            Ok(())
        }
    }
}

pub fn run(command: WikiLoopCommand) -> Result<()> {
    match command {
        WikiLoopCommand::Init {
            workspace,
            force,
            install_cron,
        } => run_init(workspace, force, install_cron),
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
        WikiLoopCommand::RunMaintainer { dry_run, force } => {
            let cfg = WikiLoopConfig::load(None)?;
            // The wiki lock serializes against the proposer so it never reads a wiki
            // mid-write; the role lock prevents overlapping maintainer crons.
            let _locks = match acquire_locks(&cfg, &["wiki", "maintainer"]) {
                Ok(locks) => locks,
                Err(e) => {
                    eprintln!("note: {e}; exiting");
                    return Ok(());
                }
            };
            run_maintainer(&cfg, dry_run, force)
        }
        WikiLoopCommand::RunProposer { dry_run, force } => {
            let cfg = WikiLoopConfig::load(None)?;
            let _locks = match acquire_locks(&cfg, &["wiki", "proposer"]) {
                Ok(locks) => locks,
                Err(e) => {
                    eprintln!("note: {e}; exiting");
                    return Ok(());
                }
            };
            run_proposer(&cfg, dry_run, force)
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
            let _locks = acquire_locks(&cfg, &["delivery"])?;
            println!("{}", apply_proposal(&cfg, &ledger, &proposal_id)?);
            Ok(())
        }
        WikiLoopCommand::Rollback { skill_name } => {
            let cfg = WikiLoopConfig::load(None)?;
            let ledger = StateLedger::open(&cfg.state_db)?;
            let _locks = acquire_locks(&cfg, &["delivery"])?;
            println!("{}", rollback_skill(&cfg, &ledger, &skill_name)?);
            Ok(())
        }
        WikiLoopCommand::DeadLetters => {
            let cfg = WikiLoopConfig::load(None)?;
            for entry in QueueManager::new(&cfg.queue_dir)?.dead_letters()? {
                println!(
                    "{}:{} attempts={} error={}",
                    entry.source,
                    entry.session_id,
                    entry.attempts,
                    entry.last_error.as_deref().unwrap_or("unknown")
                );
            }
            Ok(())
        }
        WikiLoopCommand::Requeue { source, session_id } => {
            let cfg = WikiLoopConfig::load(None)?;
            let _locks = acquire_locks(&cfg, &["wiki", "maintainer"])?;
            let path =
                QueueManager::new(&cfg.queue_dir)?.requeue_dead_letter(&source, &session_id)?;
            println!("requeued {source}:{session_id} -> {}", path.display());
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

fn lock_dir(cfg: &WikiLoopConfig) -> Result<PathBuf> {
    cfg.state_db
        .parent()
        .map(|p| p.join("locks"))
        .context("state db has no parent directory")
}

/// Acquire role locks in the given (fixed) order; the caller keeps the returned guards
/// alive for the duration of its mutation.
fn acquire_locks(cfg: &WikiLoopConfig, roles: &[&str]) -> Result<Vec<RoleLock>> {
    let dir = lock_dir(cfg)?;
    roles
        .iter()
        .map(|role| RoleLock::try_acquire(&dir, role))
        .collect()
}

fn run_maintainer(cfg: &WikiLoopConfig, dry_run: bool, force: bool) -> Result<()> {
    let ledger = StateLedger::open(&cfg.state_db)?;

    // Circuit breaker: after 3 consecutive maintainer failures, stay quiet and notify
    // instead of burning model budget every cron tick. `--force` buys exactly one
    // attempt after the operator has fixed the cause; a failure keeps it tripped.
    let failures = ledger.consecutive_failures("maintainer")?;
    if failures >= 3 && !dry_run && !force {
        let msg = format!(
            "maintainer disabled after {failures} consecutive failures; inspect `memex wiki-loop status`"
        );
        bail!("{msg}");
    }

    let run_id = ledger.start_run("maintainer")?;
    let result = execute_maintainer(cfg, &ledger, run_id, dry_run);
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

fn run_proposer(cfg: &WikiLoopConfig, dry_run: bool, force: bool) -> Result<()> {
    let ledger = StateLedger::open(&cfg.state_db)?;
    let failures = ledger.consecutive_failures("proposer")?;
    if failures >= 3 && !dry_run && !force {
        bail!(
            "proposer disabled after {failures} consecutive failures; inspect `memex wiki-loop status`"
        );
    }
    let run_id = ledger.start_run("proposer")?;
    match proposer::run_proposer(cfg, &ledger, dry_run) {
        Ok(message) => {
            ledger.finish_run(run_id, if dry_run { "dry" } else { "ok" }, 0, 0, None)?;
            println!("{message}");
            Ok(())
        }
        Err(error) => {
            ledger.finish_run(run_id, "error", 0, 0, Some(&format!("{error:#}")))?;
            if cfg.notify_on_failure {
                super::notify::notify(
                    "Wiki-Loop proposer failed",
                    Some(&format!("{error:#}").chars().take(180).collect::<String>()),
                )
                .ok();
            }
            Err(error)
        }
    }
}

/// One claimed session, classified by outcome.
struct Candidate {
    entry: QueueEntry,
    fingerprint: u64,
    meta: ingest::SessionMeta,
    records: Vec<Record>,
    failing: bool,
}

fn execute_maintainer(
    cfg: &WikiLoopConfig,
    ledger: &StateLedger,
    run_id: i64,
    dry_run: bool,
) -> Result<()> {
    // Dry runs record under a status the circuit breaker ignores, and skip the
    // collector sweep — "no model call, no write" must include queue files.
    let ok_status = if dry_run { "dry" } else { "ok" };

    let queue = QueueManager::new(&cfg.queue_dir)?;
    // Collect first (paper step 1): sweep ended-but-uncompiled sessions from analytics
    // into the queue so the loop needs no per-harness hooks. Idempotent — sessions
    // already compiled through their current last_at are skipped.
    if !dry_run {
        let swept = super::collect::sweep(cfg, ledger, &queue)?;
        if swept > 0 {
            println!("collector swept {swept} ended session(s) into the queue");
        }
    }
    let claimed = queue.claim(cfg.max_batch_size, cfg.quiet_minutes)?;
    if claimed.is_empty() {
        ledger.finish_run(run_id, ok_status, 0, 0, None)?;
        println!("queue empty or no sessions cleared the quiet window");
        return Ok(());
    }
    println!("claimed {} session(s)", claimed.len());

    let analytics_db = cfg.analytics_db();
    let index_dir = cfg.index_dir()?;
    let mut candidates: Vec<Candidate> = Vec::new();

    for (entry, fingerprint) in &claimed {
        // Processed only through the last_event_at this entry now carries: a session
        // resumed since its last compile owes a re-compile of the new turns.
        if ledger.is_processed(&entry.source, &entry.session_id, entry.last_event_at) {
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
            let acked = queue.ack_if_unchanged(entry, *fingerprint)?;
            if acked {
                ledger.mark_processed(
                    &entry.source,
                    &entry.session_id,
                    "skipped_trivial",
                    &[],
                    entry.last_event_at,
                )?;
            }
            continue;
        }
        let records = match ingest::load_records(&index_dir, &meta) {
            Ok(records) => records,
            Err(e) => {
                queue.nack(entry, &format!("{e:#}"), 5)?;
                continue;
            }
        };
        // An empty or short read is an index that has not caught up, not a session
        // without errors — compiling it would permanently drop real traces.
        if !ingest::records_complete(&records, &meta) {
            queue.nack(
                entry,
                &format!(
                    "partial trace: {} of {} message(s) indexed; retrying for the full session",
                    records.len(),
                    meta.message_count
                ),
                5,
            )?;
            continue;
        }
        let failing = !digest::extract_error_turn_indices(&records).is_empty();
        candidates.push(Candidate {
            entry: entry.clone(),
            fingerprint: *fingerprint,
            meta,
            records,
            failing,
        });
    }

    // Stratified sampling (paper Appendix C): up to N failing + M passing sessions,
    // oldest first; overflow stays queued and is the next run's head of the line.
    let mut ordered = candidates;
    ordered.sort_by_key(|c| c.entry.enqueued_at);
    let mut failing: Vec<&Candidate> = Vec::new();
    let mut passing: Vec<&Candidate> = Vec::new();
    for candidate in &ordered {
        if candidate.failing {
            if failing.len() < cfg.max_failing_sessions {
                failing.push(candidate);
            }
        } else if passing.len() < cfg.max_passing_sessions {
            passing.push(candidate);
        }
    }
    let selected: Vec<&Candidate> = failing.iter().chain(passing.iter()).copied().collect();
    if selected.is_empty() {
        ledger.finish_run(run_id, ok_status, 0, 0, None)?;
        println!("no new sessions to compile");
        return Ok(());
    }
    let selected_total = selected.len();

    let (ok_ops, quarantined_ops);
    {
        let failing_summaries: Vec<String> = failing
            .iter()
            .map(|c| {
                let indices = digest::extract_error_turn_indices(&c.records);
                digest::build_session_summary(cfg, &c.meta, &c.records, &indices)
            })
            .collect();
        let passing_summaries: Vec<String> = passing
            .iter()
            .map(|c| digest::build_success_summary(cfg, &c.meta, &c.records))
            .collect();

        let store = PatternStore::new(&cfg.wiki_root)?;

        let prompt = super::prompts::MAINTAINER.replace(
            "{digest}",
            &format!(
                "### Existing Pattern Pages\n\n{}\n\n### Trace Digest\n\n{}",
                pattern_page_bodies(&store)?,
                digest::build_stratified_digest(cfg, &failing_summaries, &passing_summaries)
            ),
        );

        if dry_run {
            let preview = prompt.chars().take(1200).collect::<String>();
            ledger.finish_run(run_id, ok_status, selected_total as i64, 0, None)?;
            println!("dry run: {selected_total} session(s) selected; queue left untouched");
            println!("--- prompt preview ---");
            println!("{preview}");
            return Ok(());
        }

        // ---- Maintain (paper step 10). The model only emits JSON; the orchestrator
        // owns every write to the wiki.
        let schema = if cfg.maintainer.json_schema {
            Some(cfg.schema_path("maintainer", super::prompts::MAINTAINER_SCHEMA)?)
        } else {
            None
        };
        let value = harness::run_role_structured(
            &cfg.maintainer,
            &prompt,
            schema.as_deref(),
            std::time::Duration::from_secs(cfg.subprocess_timeout_secs),
        )?;
        let output: MaintainerOutput =
            serde_json::from_value(value.clone()).with_context(|| {
                // Malformed-output failures recur flakily with frontier models; naming
                // the keys we actually got (an unwrapped envelope shows up here) makes
                // the circuit-breaker error diagnosable from `status` alone.
                let keys = value
                    .as_object()
                    .map(|o| o.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                format!("parsing maintainer JSON output (keys: {keys:?})")
            })?;
        if output.patterns.is_empty() && output.summary.trim().is_empty() {
            bail!(
                "maintainer produced no structured output (0 patterns, empty summary); \
                 leaving {} session(s) queued",
                selected.len()
            );
        }

        (ok_ops, quarantined_ops) =
            persist_maintainer_output(&store, &queue, ledger, run_id, &selected, &output)?;

        println!(
            "wiki: {} session(s) ({} failing / {} passing) -> {}",
            selected.len(),
            failing.len(),
            passing.len(),
            cfg.wiki_root.display()
        );
    }

    ledger.finish_run(
        run_id,
        "ok",
        selected_total as i64,
        (ok_ops + quarantined_ops) as i64,
        None,
    )?;
    println!(
        "maintainer finished: {} session(s), {} pattern op(s) ok, {} quarantined",
        selected_total, ok_ops, quarantined_ops
    );
    Ok(())
}

#[derive(serde::Deserialize)]
struct MaintainerOutput {
    // Missing patterns is malformed output, not a successful empty batch.
    patterns: Vec<super::patterns::PatternOp>,
    #[serde(default)]
    summary: String,
}

/// Persist the incremental wiki updates before acknowledging their source traces.
fn persist_maintainer_output(
    store: &PatternStore,
    queue: &QueueManager,
    ledger: &StateLedger,
    run_id: i64,
    selected: &[&Candidate],
    output: &MaintainerOutput,
) -> Result<(usize, usize)> {
    let mut ok_ops = 0usize;
    let mut quarantined_ops = 0usize;
    let mut failed_ops = 0usize;
    let mut retry_sessions = std::collections::HashSet::new();
    let mut retry_all = false;
    let mut log_lines = vec![format!(
        "## {} — maintainer run {} over {} session(s)",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        run_id,
        selected.len()
    )];
    // Successful writes with the sessions that evidence them (provenance join).
    let mut written_patterns: Vec<(String, Vec<String>)> = Vec::new();

    for op in &output.patterns {
        let (scrubbed, quarantined) = scrub_op(op);
        // Evidence correlation: only cite sessions that were actually in this digest.
        let known: std::collections::HashSet<&str> = selected
            .iter()
            .map(|c| c.meta.session_id.as_str())
            .collect();
        let evidence: Vec<super::patterns::Corroboration> = scrubbed
            .evidence_session_ids
            .iter()
            .filter(|sid| known.contains(sid.as_str()))
            .filter_map(|sid| {
                selected
                    .iter()
                    .find(|c| c.meta.session_id == *sid)
                    .map(|c| super::patterns::Corroboration {
                        source: c.meta.source.clone(),
                        session_id: sid.clone(),
                        ts: c.meta.last_at,
                    })
            })
            .collect();

        if quarantined {
            let path = store.quarantine(&scrubbed, &evidence)?;
            println!("quarantined suspect pattern -> {}", path.display());
            log_lines.push(format!("- quarantined {}", scrubbed.slug));
            quarantined_ops += 1;
            written_patterns.push((scrubbed.slug.clone(), scrubbed.evidence_session_ids.clone()));
            continue;
        }
        match store.apply_op(op, &scrubbed, &evidence) {
            Ok(write) => {
                println!(
                    "{} pattern {} ({}) -> {}",
                    write.action,
                    scrubbed.slug,
                    write.pattern_id,
                    write.file.display()
                );
                log_lines.push(format!(
                    "{} {} ({})",
                    write.action, scrubbed.slug, write.pattern_id
                ));
                ok_ops += 1;
                written_patterns.push((
                    write.pattern_id.clone(),
                    scrubbed.evidence_session_ids.clone(),
                ));
            }
            Err(e) => {
                eprintln!("warning: pattern op `{}` failed: {e:#}", op.action);
                log_lines.push(format!("failed {} {}: {e}", op.action, scrubbed.slug));
                failed_ops += 1;
                let affected: Vec<_> = scrubbed
                    .evidence_session_ids
                    .iter()
                    .filter(|sid| known.contains(sid.as_str()))
                    .cloned()
                    .collect();
                // Missing provenance cannot justify discarding any selected trace.
                retry_all |= affected.is_empty();
                retry_sessions.extend(affected);
            }
        }
    }
    log_lines.push(format!(
        "- summary: {}",
        if output.summary.is_empty() {
            "(none)"
        } else {
            &output.summary
        }
    ));
    let log = super::scrub::scrub_text(&log_lines.join("\n"));
    store.append_log(&log.text)?;
    store.append_log("")?;
    store.update_index()?;

    // ---- Ack only entries that did not change mid-run, and mark processed only
    // after the ack (paper: at-least-once; a changed entry re-compiles next run).
    for candidate in selected {
        let entry = &candidate.entry;
        if retry_all || retry_sessions.contains(&entry.session_id) {
            continue;
        }
        let acked = queue.ack_if_unchanged(entry, candidate.fingerprint)?;
        if !acked {
            eprintln!(
                "note: {}:{} was updated mid-run; left queued to recompile the new turns",
                entry.source, entry.session_id
            );
            continue;
        }
        let contributed: Vec<String> = written_patterns
            .iter()
            .filter(|(_, sessions)| sessions.contains(&entry.session_id))
            .map(|(pattern_id, _)| pattern_id.clone())
            .collect();
        ledger.mark_processed(
            &entry.source,
            &entry.session_id,
            "ok",
            &contributed,
            entry.last_event_at,
        )?;
    }

    if failed_ops > 0 {
        bail!("{failed_ops} pattern op(s) failed; affected sessions remain queued");
    }
    Ok((ok_ops, quarantined_ops))
}

/// Full text of the existing pattern pages for the maintainer prompt (paper §3.2.2: the
/// maintainer receives the wiki, not a one-line catalog). Most recently updated pages
/// first, page- and total-bounded so a large wiki cannot blow the context.
fn pattern_page_bodies(store: &PatternStore) -> Result<String> {
    let mut pages: Vec<(String, String)> = Vec::new();
    for (_, (path, meta)) in store.catalog()? {
        if meta.status == "quarantined" {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            pages.push((meta.updated.clone(), content));
        }
    }
    // RFC 3339 timestamps at fixed precision sort lexicographically.
    pages.sort_by(|a, b| b.0.cmp(&a.0));
    pages.truncate(30);

    let mut out = String::new();
    let mut total = 0usize;
    for (_, content) in pages {
        let mut body: String = content.chars().take(2000).collect();
        if body.chars().count() == 2000 {
            body.push_str("\n... [page truncated]");
        }
        if total + body.len() > 40 * 1024 {
            out.push_str("... [more pattern pages truncated]\n");
            break;
        }
        total += body.len();
        out.push_str(&body);
        out.push_str("\n\n---\n\n");
    }
    Ok(if out.is_empty() {
        "None yet.".to_string()
    } else {
        out
    })
}

/// The judge evaluates a snapshot; a concurrent apply/deny/edit invalidates it.
struct ValidationSnapshot {
    proposal: Proposal,
    skill_md: String,
    existing_skill: Option<String>,
    diff: String,
    pattern_pages: std::collections::BTreeMap<String, Option<String>>,
}

impl ValidationSnapshot {
    fn load(cfg: &WikiLoopConfig, id: &str) -> Result<Self> {
        let proposal = Proposal::load(&cfg.proposals_dir, id)?;
        let skill_md = proposal.skill_markdown(&cfg.proposals_dir)?;
        let diff = proposal.diff(&cfg.proposals_dir)?;
        let path = cfg.skills_root.join(&proposal.skill_name).join("SKILL.md");
        let existing_skill = match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let catalog = PatternStore::new(&cfg.wiki_root)?.catalog()?;
        let mut pattern_pages = std::collections::BTreeMap::new();
        for id in &proposal.purpose_patterns {
            let content = catalog
                .get(id)
                .map(|(path, _)| std::fs::read_to_string(path))
                .transpose()?;
            pattern_pages.insert(id.clone(), content);
        }
        Ok(Self {
            proposal,
            skill_md,
            existing_skill,
            diff,
            pattern_pages,
        })
    }

    fn ensure_unchanged(&self, cfg: &WikiLoopConfig, id: &str) -> Result<()> {
        let current = Self::load(cfg, id)?;
        if serde_json::to_value(&self.proposal)? != serde_json::to_value(&current.proposal)?
            || self.skill_md != current.skill_md
            || self.existing_skill != current.existing_skill
            || self.diff != current.diff
            || self.pattern_pages != current.pattern_pages
        {
            bail!(
                "proposal, deployed skill, or motivating evidence changed during validation; retry validation"
            );
        }
        Ok(())
    }
}

fn run_validate(
    cfg: &WikiLoopConfig,
    ledger: &StateLedger,
    proposal_id: &str,
    skip_tier1: bool,
) -> Result<()> {
    if !super::gates::is_valid_proposal_id(proposal_id) {
        bail!("invalid proposal id `{proposal_id}`");
    }
    let validation_role = format!("validate-{proposal_id}");
    let _validation_lock = acquire_locks(cfg, &[&validation_role])?;
    let snapshot = {
        let _snapshot_locks = acquire_locks(cfg, &["wiki", "delivery"])?;
        ValidationSnapshot::load(cfg, proposal_id)?
    };
    let mut proposal = snapshot.proposal.clone();
    if proposal.status == "accepted" {
        bail!("proposal is already accepted; stage a new proposal to change a deployed skill");
    }
    let skill_md = &snapshot.skill_md;
    let existing = &snapshot.existing_skill;
    let scopes = contributing_scopes(cfg, &proposal);

    proposal.gates.clear();
    let mut all_passed = true;
    let mut results = super::gates::tier0(
        cfg,
        ledger,
        &proposal,
        skill_md,
        existing.as_deref(),
        &scopes,
    )?;
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
        match super::gates::tier1(cfg, &proposal, skill_md, &scopes) {
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

    let _commit_locks = acquire_locks(cfg, &["wiki", "delivery"])?;
    snapshot.ensure_unchanged(cfg, proposal_id)?;
    if contributing_scopes(cfg, &proposal) != scopes {
        bail!("motivating pattern scopes changed during validation; retry validation");
    }
    let previous_status = proposal.status.clone();
    proposal.status = if all_passed { "validated" } else { "rejected" }.into();
    proposal.save(&cfg.proposals_dir)?;
    ledger.set_proposal_status(
        proposal_id,
        &proposal.status,
        &serde_json::to_string(&proposal.gates)?,
    )?;

    // Paper §3.2.4: after each validation evaluation the harness appends to
    // `skill-impact.md` — the audit trail the proposer reads to avoid repeating rejected
    // interventions. A re-validation that flips the status is exactly such a decision.
    if proposal.status != previous_status {
        let diff = proposal.diff(&cfg.proposals_dir).unwrap_or_default();
        super::gates::append_skill_impact(
            &cfg.wiki_root,
            &format!(
                "## {} — validation — {} — {}\n- decision: {}\n- gates: {}\n- patterns: {}\n\n```diff\n{}\n```\n",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                proposal.id,
                proposal.skill_name,
                proposal.status,
                proposal
                    .gates
                    .iter()
                    .map(|(k, g)| format!("{k}={}", if g.passed { "pass" } else { "FAIL" }))
                    .collect::<Vec<_>>()
                    .join(" "),
                proposal.purpose_patterns.join(", "),
                diff
            ),
        )?;
    }
    println!("proposal {proposal_id}: {}", proposal.status);
    Ok(())
}

/// Scopes of the wiki patterns that motivated a proposal — what the fail-closed
/// Tier-0/Tier-1 gates check global proposals against.
fn contributing_scopes(cfg: &WikiLoopConfig, proposal: &Proposal) -> Vec<String> {
    let mut scopes = Vec::new();
    if cfg.wiki_root.exists()
        && let Ok(store) = PatternStore::new(&cfg.wiki_root)
        && let Ok(catalog) = store.catalog()
    {
        for id in &proposal.purpose_patterns {
            if let Some((_, meta)) = catalog.get(id) {
                scopes.push(meta.scope.clone());
            }
        }
    }
    if let Some(project) = proposal.scope.strip_prefix("project:") {
        scopes.push(format!("project:{project}"));
    }
    scopes.sort();
    scopes.dedup();
    scopes
}

fn run_status(cfg: &WikiLoopConfig) -> Result<()> {
    let queue = QueueManager::new(&cfg.queue_dir)?;
    let (pending, dlq) = queue.counts()?;
    let ledger = StateLedger::open(&cfg.state_db)?;

    println!("=== wiki-loop status ===");
    println!(
        "queue:      {pending} pending, {dlq} dead-lettered ({})",
        cfg.queue_dir.display()
    );
    println!(
        "wiki:       {} pattern(s) in {}",
        count_patterns(&cfg.wiki_root),
        cfg.wiki_root.display()
    );
    if let Some(root) = &cfg.workspace_root {
        println!("workspace:  {} (wiki + skills base)", root.display());
    }
    println!("skills:     {}", cfg.skills_root.display());
    let week_ago = now_ms() - 7 * 24 * 3_600_000;
    println!(
        "proposals:  {}/week reviewable (ceiling {}), staged at {}",
        ledger.reviewable_proposals_since(week_ago)?,
        cfg.max_proposals_per_week,
        cfg.proposals_dir.display()
    );
    // The review surface: a missed notification must not mean `ls` on a state dir.
    let staged = ledger.staged_proposals(10)?;
    if staged.is_empty() {
        println!("staged:     (no open proposals)");
    } else {
        println!("staged:");
        for (id, skill_name, status) in staged {
            println!("  {id} {status:<9} {skill_name}");
        }
        println!("  review with: memex wiki-loop validate <id> && memex wiki-loop apply <id>");
    }
    for role in ["maintainer", "proposer"] {
        let failures = ledger.consecutive_failures(role)?;
        println!("health:     {role}: {failures} consecutive failure(s)");
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

fn count_patterns(wiki_dir: &std::path::Path) -> usize {
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

    match &cfg.workspace_root {
        Some(root) if root.exists() => {
            println!("[ok] workspace root: {}", root.display());
        }
        Some(root) => {
            ok = false;
            println!("[fail] workspace root missing: {}", root.display());
        }
        None => {}
    }

    for dir in [
        &cfg.queue_dir,
        &cfg.state_db
            .parent()
            .unwrap_or(std::path::Path::new("/tmp"))
            .to_path_buf(),
        &cfg.proposals_dir,
        &cfg.wiki_root,
    ] {
        match std::fs::create_dir_all(dir) {
            Ok(()) => println!("[ok] writable: {}", dir.display()),
            Err(e) => {
                ok = false;
                println!("[fail] cannot create {}: {e}", dir.display());
            }
        }
    }

    for (name, role) in [
        ("maintainer", &cfg.maintainer),
        ("proposer", &cfg.proposer),
        ("judge", &cfg.judge),
    ] {
        let binary = role.command.first().map(String::as_str).unwrap_or("");
        match super::notify::which_bin(binary) {
            Some(path) => println!("[ok] {name} binary: {}", path.display()),
            None => {
                ok = false;
                println!("[fail] {name} configured binary `{binary}` is unavailable");
            }
        }
    }

    if !ok {
        bail!("doctor found blocking issues");
    }
    Ok(())
}

/// Scaffold `~/.memex/wiki-loop.toml` (only when absent, unless `--force`), create the
/// wiki/skills directories, optionally install the cron schedule, then run doctor.
/// No model calls — safe to run at any time and idempotent.
fn run_init(workspace: Option<PathBuf>, force: bool, install_cron: bool) -> Result<()> {
    println!("=== wiki-loop init ===");

    let config_path = super::config::default_config_path()?;
    if config_path.exists() && !force {
        println!("[ok] config already present: {}", config_path.display());
        // A requested workspace that an existing config does not carry is silently
        // dropped — say so loudly instead of letting the wiki land somewhere else.
        if let Some(w) = &workspace {
            let existing = WikiLoopConfig::load(None)?;
            if existing.workspace_root.as_deref() != Some(w.as_path()) {
                println!(
                    "[note] --workspace {} ignored: the existing config does not set it \
                     (wiki stays at {}). Add `workspace_root = \"{}\"` to {} or re-run \
                     with --force.",
                    w.display(),
                    existing.wiki_root.display(),
                    w.display(),
                    config_path.display()
                );
            }
        }
    } else {
        let workspace_line = workspace
            .as_ref()
            .map(|w| format!("workspace_root = \"{}\"\n", w.display()))
            .unwrap_or_default();
        let template = format!(
            "# wiki-loop configuration. Everything here is optional — defaults live in\n\
             # the binary; this file only overrides. Every parameter is documented with\n\
             # its default in docs/wiki-loop.example.toml; docs: docs/wiki-loop.md.\n\
             {workspace_line}\
             # Sampling and budgets (defaults shown):\n\
             # quiet_minutes          = 20    # quiet window before a session is compiled\n\
             # collect_lookback_days  = 7     # sweep horizon for ended sessions (0 = all)\n\
             # min_turns              = 3     # skip trivial sessions\n\
             # max_batch_size         = 10    # sessions claimed per maintainer run\n\
             # max_failing_sessions   = 5     # paper Appendix C stratification\n\
             # max_passing_sessions   = 3\n\
             # max_proposals_per_week = 3     # precision over recall\n\n\
             # Roles: the maintainer defaults to `agy`; the proposer and judge default to\n\
             # `claude` (opus). See docs for full examples:\n\
             # [maintainer]\n\
             # command = [\"agy\", \"--output-format\", \"json\", \"--model\", \"{{model}}\", \"--effort\", \"{{effort}}\"]\n\
             # model = \"gemini-3.8-flash\"\n\
             # effort = \"low\"\n\
             # json_schema = true\n"
        );
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&config_path, template)
            .with_context(|| format!("writing {}", config_path.display()))?;
        println!("[ok] wrote config: {}", config_path.display());
    }

    let cfg = WikiLoopConfig::load(None)?;
    for dir in [&cfg.wiki_root, &cfg.skills_root] {
        if dir.exists() {
            println!("[ok] present: {}", dir.display());
            continue;
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        println!("[ok] created: {}", dir.display());
    }

    if install_cron {
        install_cron_schedule(&cfg)?;
    }

    println!("\nnext steps:");
    println!("  memex wiki-loop run-maintainer   # compile ended sessions now");
    println!("  memex wiki-loop status           # queue, wiki, staged skills, health");
    println!("  memex tui, then alt+w            # wiki browser; `s` from there for skills");
    run_doctor(&cfg)
}

/// Install (or replace) the marked crontab block for the loop. The binary that runs is
/// the one `init` was invoked through (`current_exe`), so cron always runs the exact
/// build the user just used — including a `memex-wiki-loop` copy pinned for the loop.
fn install_cron_schedule(cfg: &WikiLoopConfig) -> Result<()> {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "memex".into());
    let state_dir = cfg
        .state_db
        .parent()
        .context("state db has no parent directory")?;
    let block = format!(
        "# BEGIN memex wiki-loop (added by `memex wiki-loop init --install-cron`)\n\
         */5 * * * * {exe} --no-update-check --non-interactive wiki-loop run-maintainer >> {m} 2>&1\n\
         17 */6 * * * {exe} --no-update-check --non-interactive wiki-loop run-proposer >> {p} 2>&1\n\
         */10 * * * * {exe} --no-update-check --non-interactive index >> {i} 2>&1\n\
         # END memex wiki-loop\n",
        m = state_dir.join("maintainer.log").display(),
        p = state_dir.join("proposer.log").display(),
        i = state_dir.join("index.log").display(),
    );

    // `crontab -l` fails when no crontab exists yet — that is an empty starting point.
    let existing = std::process::Command::new("crontab")
        .arg("-l")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    let merged = splice_cron_block(&existing, &block)?;
    let mut child = std::process::Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("spawning `crontab -` (is cron installed?)")?;
    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(merged.as_bytes())?;
    }
    let status = child.wait().context("waiting for crontab")?;
    if !status.success() {
        bail!("crontab rejected the new schedule (exit {status})");
    }
    println!("[ok] cron installed: maintainer every 5m, proposer every 6h, index every 10m");
    println!(
        "     logs under {} — add a logrotate drop-in if they grow (see docs)",
        state_dir.display()
    );
    Ok(())
}

/// Replace the existing marked block (whole lines between the BEGIN/END markers,
/// inclusive) or append a fresh one after the current crontab. Idempotent. Refuses
/// malformed marker layouts (a BEGIN without its END, duplicated markers): splicing
/// those could swallow unrelated user entries, so the caller is told to fix them.
fn splice_cron_block(existing: &str, block: &str) -> Result<String> {
    const BEGIN: &str = "# BEGIN memex wiki-loop";
    const END: &str = "# END memex wiki-loop";
    let begins = existing
        .lines()
        .enumerate()
        .filter_map(|(i, l)| l.starts_with(BEGIN).then_some(i))
        .collect::<Vec<_>>();
    let ends = existing
        .lines()
        .enumerate()
        .filter_map(|(i, l)| l.starts_with(END).then_some(i))
        .collect::<Vec<_>>();
    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => {
            let trimmed = existing.trim_end();
            if trimmed.is_empty() {
                Ok(block.to_string())
            } else {
                Ok(format!("{trimmed}\n\n{block}"))
            }
        }
        ([begin], [end]) if begin < end => {
            let mut out: Vec<&str> = existing.lines().collect();
            out.splice(*begin..=*end, [block.trim_end()]);
            Ok(out.join("\n") + "\n")
        }
        _ => bail!(
            "crontab has malformed wiki-loop markers ({} BEGIN, {} END); fix or remove \
             them by hand and re-run `init --install-cron`",
            begins.len(),
            ends.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(tmp: &tempfile::TempDir) -> WikiLoopConfig {
        WikiLoopConfig {
            wiki_root: tmp.path().join("wiki"),
            skills_root: tmp.path().join("skills"),
            state_db: tmp.path().join("state.db"),
            queue_dir: tmp.path().join("queue"),
            proposals_dir: tmp.path().join("proposals"),
            notify_on_failure: false,
            ..WikiLoopConfig::default()
        }
    }

    fn candidate(queue: &QueueManager, id: &str) -> Candidate {
        queue.enqueue_event("test", id, None, true, 1).unwrap();
        let (entry, fingerprint) = queue
            .claim(100, 0)
            .unwrap()
            .into_iter()
            .find(|(entry, _)| entry.session_id == id)
            .unwrap();
        Candidate {
            meta: ingest::SessionMeta {
                source: "test".into(),
                session_id: id.into(),
                source_path: None,
                project: None,
                cwd: None,
                git_root: None,
                repo_project: None,
                started_at: 0,
                last_at: 1,
                message_count: 3,
                resolution_status: None,
            },
            entry,
            fingerprint,
            records: vec![],
            failing: false,
        }
    }

    fn operation(action: &str, slug: &str, sessions: &[&str]) -> super::super::patterns::PatternOp {
        serde_json::from_value(serde_json::json!({
            "action": action, "slug": slug, "existing_id": "missing-pattern",
            "title": "A useful pattern", "symptom": "Symptom", "root_cause": "Cause",
            "fix": "Fix", "evidence_session_ids": sessions,
        }))
        .unwrap()
    }

    #[test]
    fn failed_ops_preserve_affected_sessions_including_mixed_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(&tmp);
        let store = PatternStore::new(&cfg.wiki_root).unwrap();
        let queue = QueueManager::new(&cfg.queue_dir).unwrap();
        let ledger = StateLedger::open(&cfg.state_db).unwrap();
        let good = candidate(&queue, "good");
        let bad = candidate(&queue, "bad");
        let both = candidate(&queue, "both");
        let output = MaintainerOutput {
            patterns: vec![
                operation("create", "good", &["good", "both"]),
                operation("merge", "bad", &["bad", "both"]),
            ],
            summary: "One write failed".into(),
        };
        assert!(
            persist_maintainer_output(&store, &queue, &ledger, 1, &[&good, &bad, &both], &output)
                .is_err()
        );
        assert!(ledger.is_processed("test", "good", 1));
        for id in ["bad", "both"] {
            assert!(!ledger.is_processed("test", id, 1));
        }
        assert_eq!(queue.counts().unwrap(), (2, 0));
        // Incremental writes are kept, even when another operation fails.
        assert_eq!(store.catalog().unwrap().len(), 1);
    }

    #[test]
    fn all_failed_and_unattributed_ops_do_not_ack() {
        for sessions in [vec!["session"], vec![], vec!["unknown"]] {
            let tmp = tempfile::tempdir().unwrap();
            let cfg = config(&tmp);
            let store = PatternStore::new(&cfg.wiki_root).unwrap();
            let queue = QueueManager::new(&cfg.queue_dir).unwrap();
            let ledger = StateLedger::open(&cfg.state_db).unwrap();
            let c = candidate(&queue, "session");
            let output = MaintainerOutput {
                patterns: vec![operation("merge", "missing", &sessions)],
                summary: "failed".into(),
            };
            assert!(persist_maintainer_output(&store, &queue, &ledger, 1, &[&c], &output).is_err());
            assert_eq!(queue.counts().unwrap(), (1, 0));
            assert!(!ledger.is_processed("test", "session", 1));
        }
    }

    #[test]
    fn maintainer_summary_is_scrubbed_and_noop_is_acknowledged() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(&tmp);
        let store = PatternStore::new(&cfg.wiki_root).unwrap();
        let queue = QueueManager::new(&cfg.queue_dir).unwrap();
        let ledger = StateLedger::open(&cfg.state_db).unwrap();
        let c = candidate(&queue, "session");
        let secret = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let output = MaintainerOutput {
            patterns: vec![],
            summary: format!("trace includes {secret}"),
        };
        persist_maintainer_output(&store, &queue, &ledger, 1, &[&c], &output).unwrap();
        let log = std::fs::read_to_string(cfg.wiki_root.join("logs.md")).unwrap();
        assert!(!log.contains(secret));
        assert!(log.contains("[REDACTED_GH_TOKEN]"));
        assert_eq!(queue.counts().unwrap(), (0, 0));
        assert!(ledger.is_processed("test", "session", 1));
    }

    #[test]
    fn proposer_records_errors_and_honors_breaker_and_force() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(&tmp);
        std::fs::create_dir_all(&cfg.wiki_root).unwrap();
        // Fail before any model is invoked.
        std::fs::write(cfg.wiki_root.join("patterns"), "not a directory").unwrap();
        for _ in 0..3 {
            assert!(run_proposer(&cfg, false, false).is_err());
        }
        let ledger = StateLedger::open(&cfg.state_db).unwrap();
        assert_eq!(ledger.consecutive_failures("proposer").unwrap(), 3);
        assert!(
            run_proposer(&cfg, false, false)
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
        assert_eq!(ledger.recent_runs(10).unwrap().len(), 3);
        assert!(run_proposer(&cfg, false, true).is_err());
        assert_eq!(ledger.recent_runs(10).unwrap().len(), 4);
        std::fs::remove_file(cfg.wiki_root.join("patterns")).unwrap();
        run_proposer(&cfg, false, true).unwrap();
        assert_eq!(ledger.consecutive_failures("proposer").unwrap(), 0);
        let rows = ledger.recent_runs(10).unwrap();
        assert!(
            rows.iter()
                .all(|r| r.role == "proposer" && r.ended_at.is_some())
        );
    }

    #[test]
    fn proposer_dry_runs_do_not_clear_failure_history() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(&tmp);
        let ledger = StateLedger::open(&cfg.state_db).unwrap();
        for _ in 0..3 {
            let id = ledger.start_run("proposer").unwrap();
            ledger.finish_run(id, "error", 0, 0, Some("quota")).unwrap();
        }
        run_proposer(&cfg, true, false).unwrap();
        assert_eq!(ledger.consecutive_failures("proposer").unwrap(), 3);
        assert!(
            ledger
                .recent_runs(10)
                .unwrap()
                .iter()
                .any(|r| r.status == "dry")
        );
    }

    #[test]
    fn doctor_checks_configured_role_programs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(&tmp);
        cfg.maintainer.command = vec!["/bin/sh".into()];
        cfg.proposer.command = vec!["/bin/sh".into()];
        cfg.judge.command = vec!["/bin/sh".into()];
        run_doctor(&cfg).unwrap();
        cfg.judge.command = vec![tmp.path().join("absent-judge").display().to_string()];
        assert!(run_doctor(&cfg).is_err());
        cfg.judge.command.clear();
        assert!(run_doctor(&cfg).is_err());
    }

    fn stage_for_validation(cfg: &WikiLoopConfig) -> Proposal {
        let proposal = Proposal {
            id: "prop_review".into(),
            created_at: 1,
            skill_name: "review-skill".into(),
            description: "review".into(),
            status: "pending".into(),
            scope: "global".into(),
            purpose_patterns: vec![],
            rationale: "test".into(),
            gates: Default::default(),
        };
        proposal.save(&cfg.proposals_dir).unwrap();
        let dir = cfg.proposals_dir.join(&proposal.id);
        std::fs::write(dir.join("SKILL.md"), "# Review skill\n").unwrap();
        std::fs::write(dir.join("skill.diff"), "+# Review skill\n").unwrap();
        proposal
    }

    #[test]
    fn validation_snapshot_rejects_concurrent_deny_or_content_changes() {
        for change in ["deny", "staged", "live", "diff"] {
            let tmp = tempfile::tempdir().unwrap();
            let cfg = config(&tmp);
            let mut proposal = stage_for_validation(&cfg);
            let snapshot = ValidationSnapshot::load(&cfg, &proposal.id).unwrap();
            snapshot.ensure_unchanged(&cfg, &proposal.id).unwrap();
            let dir = cfg.proposals_dir.join(&proposal.id);
            match change {
                "deny" => {
                    proposal.status = "rejected".into();
                    proposal.save(&cfg.proposals_dir).unwrap();
                }
                "staged" => std::fs::write(dir.join("SKILL.md"), "changed").unwrap(),
                "diff" => std::fs::write(dir.join("skill.diff"), "changed").unwrap(),
                "live" => {
                    let live = cfg.skills_root.join(&proposal.skill_name);
                    std::fs::create_dir_all(&live).unwrap();
                    std::fs::write(live.join("SKILL.md"), "changed").unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                snapshot.ensure_unchanged(&cfg, &proposal.id).is_err(),
                "{change}"
            );
        }
    }

    #[test]
    fn validation_snapshot_rejects_changed_pattern_evidence_with_same_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(&tmp);
        let mut proposal = stage_for_validation(&cfg);
        let store = PatternStore::new(&cfg.wiki_root).unwrap();
        let op = operation("create", "evidence", &["session"]);
        let written = store.apply_op(&op, &op, &[]).unwrap();
        proposal.purpose_patterns = vec![written.pattern_id];
        proposal.save(&cfg.proposals_dir).unwrap();
        let snapshot = ValidationSnapshot::load(&cfg, &proposal.id).unwrap();
        snapshot.ensure_unchanged(&cfg, &proposal.id).unwrap();
        let content = std::fs::read_to_string(&written.file).unwrap();
        std::fs::write(
            &written.file,
            format!("{content}\nAdditional corroborating evidence.\n"),
        )
        .unwrap();
        assert!(snapshot.ensure_unchanged(&cfg, &proposal.id).is_err());
    }

    #[test]
    fn validate_rejects_terminal_accepted_proposal_without_overwriting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(&tmp);
        let mut proposal = stage_for_validation(&cfg);
        proposal.status = "accepted".into();
        proposal.save(&cfg.proposals_dir).unwrap();
        let ledger = StateLedger::open(&cfg.state_db).unwrap();
        assert!(run_validate(&cfg, &ledger, &proposal.id, true).is_err());
        assert_eq!(
            Proposal::load(&cfg.proposals_dir, &proposal.id)
                .unwrap()
                .status,
            "accepted"
        );
        // Both locks are released on the error path.
        acquire_locks(&cfg, &["delivery", "validate-prop_review"]).unwrap();
    }

    const BLOCK: &str = "# BEGIN memex wiki-loop (added by `memex wiki-loop init --install-cron`)\n*/30 * * * * memex wiki-loop run-maintainer >> /tmp/m.log 2>&1\n# END memex wiki-loop\n";

    #[test]
    fn splice_appends_to_unmarked_crontab_and_starts_fresh() {
        let merged = splice_cron_block("0 5 * * * backup\n", BLOCK).expect("splice");
        assert!(merged.starts_with("0 5 * * * backup\n\n# BEGIN memex wiki-loop"));
        assert!(merged.ends_with("# END memex wiki-loop\n"));
        // The other entries survive untouched.
        assert!(merged.contains("0 5 * * * backup"));

        let fresh = splice_cron_block("", BLOCK).expect("splice");
        assert_eq!(fresh, BLOCK);

        // Only-noise input (crontab -l failure fallback) is treated as empty.
        assert_eq!(splice_cron_block("\n", BLOCK).expect("splice"), BLOCK);
    }

    #[test]
    fn splice_replaces_existing_block_in_place_and_exactly_once() {
        let with_old = "0 5 * * * backup\n# BEGIN memex wiki-loop (old)\nold line\n# END memex wiki-loop\n9 9 * * * other\n";
        let merged = splice_cron_block(with_old, BLOCK).expect("splice");
        assert!(!merged.contains("old line"), "{}", merged);
        assert!(!merged.contains("(old)"));
        assert_eq!(merged.matches("# BEGIN memex wiki-loop").count(), 1);
        // Position preserved: backup before the block, other after.
        assert!(
            merged.find("backup").unwrap() < merged.find("# BEGIN").unwrap()
                && merged.find("# END").unwrap() < merged.find("other").unwrap(),
            "{}",
            merged
        );
    }

    #[test]
    fn splice_refuses_malformed_markers_instead_of_swallowing_lines() {
        // BEGIN without END (truncated manual edit): splicing would eat everything
        // after BEGIN up to a later END — refuse.
        let orphan_begin = "0 5 * * * backup\n# BEGIN memex wiki-loop\nold line\n";
        let err = splice_cron_block(orphan_begin, BLOCK).expect_err("must refuse");
        assert!(format!("{err:#}").contains("malformed"), "{err:#}");

        // Duplicated markers (an earlier append created two blocks): also refuse.
        let doubled = format!("{BLOCK}0 0 * * * mine\n{BLOCK}");
        let err = splice_cron_block(&doubled, BLOCK).expect_err("must refuse");
        assert!(format!("{err:#}").contains("malformed"), "{err:#}");
        // Nothing is silently produced.
        assert!(splice_cron_block(&doubled, BLOCK).is_err());
    }
}
