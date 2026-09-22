//! Gating (paper step: validate → accept/revert) and staged delivery.
//!
//! Tier 0 — static hygiene: secret rescan, slug check, dedup, recently-rejected check,
//! referenced-path existence, scope stamp. Zero LLM cost.
//! Tier 1 — counterfactual LLM judge over K historical sessions that hit the failure mode
//! (the production adaptation of the paper's validation rollouts; repo tests cannot grade
//! an instruction change).
//!
//! The wiki is **never** modified here. Only skill deployment, the ledger, and the
//! `skill-impact.md` audit trail (paper: appended programmatically after every decision).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::config::WikiLoopConfig;
use super::ingest;
use super::ledger::StateLedger;
use super::patterns::is_valid_slug;
use super::queue::now_ms;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResult {
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub created_at: i64,
    pub skill_name: String,
    pub description: String,
    pub status: String,
    pub scope: String,
    pub purpose_patterns: Vec<String>,
    pub rationale: String,
    #[serde(default)]
    pub gates: BTreeMap<String, GateResult>,
}

/// Proposal ids are minted as `prop_{hex}_{hex}` (see the proposer) and, unlike skill
/// slugs, may contain `_`. The two validators must not be shared: applying `is_valid_slug`
/// here rejects every generated id and dead-ends validate/apply.
pub fn is_valid_proposal_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !id.starts_with('-')
        && !id.ends_with('-')
        && !id.starts_with('_')
        && !id.ends_with('_')
}

impl Proposal {
    pub fn load(dir: &Path, proposal_id: &str) -> Result<Self> {
        if !is_valid_proposal_id(proposal_id) {
            bail!("invalid proposal id `{proposal_id}`");
        }
        let path = dir.join(proposal_id).join("proposal.json");
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(&self.id).join("proposal.json");
        std::fs::create_dir_all(path.parent().expect("proposal dir"))?;
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn skill_markdown(&self, dir: &Path) -> Result<String> {
        Ok(std::fs::read_to_string(
            dir.join(&self.id).join("SKILL.md"),
        )?)
    }

    pub fn diff(&self, dir: &Path) -> Result<String> {
        Ok(std::fs::read_to_string(
            dir.join(&self.id).join("skill.diff"),
        )?)
    }
}

/// Tier 0 — static hygiene gates. `existing_skill` is the current SKILL.md content when
/// the skill already exists (patch proposals), else None. `contributing_scopes` are the
/// scopes of the wiki patterns that motivated the proposal: a global proposal that
/// references repo-relative paths is only checkable against the repos of those
/// contributing patterns — without them the gate fails closed.
pub fn tier0(
    cfg: &WikiLoopConfig,
    ledger: &StateLedger,
    proposal: &Proposal,
    skill_markdown: &str,
    existing_skill: Option<&str>,
    contributing_scopes: &[String],
) -> Result<Vec<(String, GateResult)>> {
    let mut results = Vec::new();

    // 1. Secret rescan (release gate, not a nicety — the wiki/skills are durable stores).
    results.push((
        "secret_scan".into(),
        match super::scrub::contains_secret(skill_markdown) {
            false => GateResult {
                passed: true,
                detail: "no secrets detected".into(),
            },
            true => GateResult {
                passed: false,
                detail: "secret-like material present in SKILL.md".into(),
            },
        },
    ));

    // 2. Name hygiene.
    results.push((
        "name_slug".into(),
        GateResult {
            passed: is_valid_slug(&proposal.skill_name),
            detail: format!("skill name `{}`", proposal.skill_name),
        },
    ));

    // 3. Scope stamp (paper layer: project-scoped knowledge must not bleed globally).
    let scoped_ok = proposal.scope == "global"
        || proposal
            .scope
            .strip_prefix("project:")
            .is_some_and(|p| !p.is_empty());
    results.push((
        "scope_stamp".into(),
        GateResult {
            passed: scoped_ok,
            detail: format!("scope `{}`", proposal.scope),
        },
    ));

    // 4. Dedup: identical content already live, or the same skill already staged.
    //    A validated-but-unapplied proposal does not exist as a skill yet, so an
    //    active-skills scan alone would let the proposer mint a duplicate — the
    //    staged one must be applied or denied before a re-proposal is reviewable.
    let staged_duplicate = ledger.open_proposal_for_skill(&proposal.skill_name, &proposal.id)?;
    results.push((
        "dedup".into(),
        if let Some(existing) = existing_skill
            && existing.trim() == skill_markdown.trim()
        {
            GateResult {
                passed: false,
                detail: "proposal duplicates the already-active skill".into(),
            }
        } else if let Some((staged_id, staged_status)) = staged_duplicate {
            GateResult {
                passed: false,
                detail: format!(
                    "`{}` is already staged by {staged_id} ({staged_status}); \
                     apply or deny it first",
                    proposal.skill_name
                ),
            }
        } else {
            GateResult {
                passed: true,
                detail: "not a duplicate of the active skill or a staged proposal".into(),
            }
        },
    ));

    // 5. Recently-rejected proposals are not re-proposed (paper: the impact tracker prevents
    //    re-proposing rejected interventions; enforced here programmatically as a backstop).
    let week_ago = now_ms() - 7 * 24 * 3_600_000;
    let rejected = ledger.rejected_proposals_since(week_ago)?;
    let re_rejected = rejected.contains(&proposal.skill_name);
    results.push((
        "recently_rejected".into(),
        GateResult {
            passed: !re_rejected,
            detail: if re_rejected {
                format!(
                    "`{}` was rejected within the last 7 days; re-proposal blocked",
                    proposal.skill_name
                )
            } else {
                "no recent rejection for this skill".into()
            },
        },
    ));

    // 6. Referenced relative paths must exist in the target repo (staleness guard).
    results.push(referenced_paths_gate(
        cfg,
        proposal,
        skill_markdown,
        contributing_scopes,
    )?);

    Ok(results)
}

fn referenced_paths_gate(
    cfg: &WikiLoopConfig,
    proposal: &Proposal,
    skill_markdown: &str,
    contributing_scopes: &[String],
) -> Result<(String, GateResult)> {
    // Candidate references: backticked tokens that look like repo-relative paths.
    let mut refs: Vec<String> = Vec::new();
    for caps in MARKDOWN_CODE.captures_iter(skill_markdown) {
        let token = caps[1].trim();
        if token.contains('/')
            && !token.starts_with('/')
            && !token.contains(' ')
            && token
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '-' | '_'))
            && !token.ends_with(".md")
        {
            refs.push(token.to_string());
        }
    }
    refs.sort();
    refs.dedup();
    refs.truncate(20);

    if refs.is_empty() {
        return Ok((
            "referenced_paths".into(),
            GateResult {
                passed: true,
                detail: "no repo-relative paths referenced".into(),
            },
        ));
    }

    // Repo root(s) the references are checked against: the proposal's own project
    // scope, or — for a global proposal — every contributing project scope. "Verify
    // manually" is not a gate: a global proposal with no resolvable repo fails closed.
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(project) = proposal.scope.strip_prefix("project:") {
        let Some(session) = ingest::recent_sessions_for_project(&cfg.analytics_db(), project, 1)?
            .into_iter()
            .next()
        else {
            return Ok((
                "referenced_paths".into(),
                GateResult {
                    passed: false,
                    detail: format!("scope project `{project}` has no known repo root"),
                },
            ));
        };
        let root = session
            .git_root
            .or_else(|| session.cwd.clone())
            .unwrap_or_default();
        let root = PathBuf::from(root);
        if !root.exists() {
            return Ok((
                "referenced_paths".into(),
                GateResult {
                    passed: false,
                    detail: format!("repo root `{}` no longer exists", root.display()),
                },
            ));
        }
        roots.push(root);
    } else {
        let mut projects: Vec<&str> = contributing_scopes
            .iter()
            .filter_map(|s| s.strip_prefix("project:"))
            .collect();
        projects.sort_unstable();
        projects.dedup();
        for project in &projects {
            if let Some(root) = repo_root_for_project(cfg, project)? {
                roots.push(root);
            }
        }
        if roots.is_empty() {
            let why = if projects.is_empty() {
                "it has no contributing project scopes".to_string()
            } else {
                format!(
                    "contributing project scope(s) {} resolve to no live repo root",
                    projects.join(", ")
                )
            };
            return Ok((
                "referenced_paths".into(),
                GateResult {
                    passed: false,
                    detail: format!(
                        "global skill references paths ({}) but {why}; give the proposal a \
                         `project:` scope or re-propose it from patterns observed in a live repo",
                        refs.join(", ")
                    ),
                },
            ));
        }
    }
    let missing: Vec<&String> = refs
        .iter()
        .filter(|r| !roots.iter().any(|root| root.join(r.as_str()).exists()))
        .collect();
    Ok((
        "referenced_paths".into(),
        if missing.is_empty() {
            GateResult {
                passed: true,
                detail: format!(
                    "{} referenced path(s) exist under {}",
                    refs.len(),
                    roots
                        .iter()
                        .map(|r| r.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        } else {
            GateResult {
                passed: false,
                detail: format!(
                    "missing in {}: {}",
                    roots
                        .iter()
                        .map(|r| r.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        },
    ))
}

/// Repo root for a contributing project, from its most recent analytics session (git
/// root, falling back to cwd). None when the project is unknown or its root is gone;
/// contributing roots are advisory, so the reason stays in the gate result.
fn repo_root_for_project(cfg: &WikiLoopConfig, project: &str) -> Result<Option<PathBuf>> {
    let Some(session) = ingest::recent_sessions_for_project(&cfg.analytics_db(), project, 1)?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let root = session
        .git_root
        .or_else(|| session.cwd.clone())
        .unwrap_or_default();
    let root = PathBuf::from(root);
    Ok(root.exists().then_some(root))
}

static MARKDOWN_CODE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new("`([^`\\n]+)`").expect("static regex"));

/// The sessions the tier-1 judge replays: the corroborating (failure-mode) sessions
/// stamped on the patterns that motivated the proposal come first — the judge's
/// contract is "historical sessions that hit the failure mode" (prompts/judge.md) —
/// then the most recent sessions of the relevant project(s) top the set up to
/// `budget`. Recency alone puts mostly unrelated work in front of the judge, which
/// fails global proposals by arithmetic rather than by judgment.
fn replay_sessions(
    analytics_db: &Path,
    catalog: &BTreeMap<String, (PathBuf, super::patterns::PatternMeta)>,
    proposal: &Proposal,
    contributing_scopes: &[String],
    budget: usize,
) -> Result<Vec<ingest::SessionMeta>> {
    // Failure-mode sessions: pattern page -> corroboration stamps -> session metas.
    // Unknown pattern ids and sessions missing from the store fall through to the
    // top-up below.
    let mut sessions: Vec<ingest::SessionMeta> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for pattern_id in &proposal.purpose_patterns {
        let Some((_, meta)) = catalog.get(pattern_id) else {
            continue;
        };
        for c in &meta.corroboration {
            if seen.insert((c.source.clone(), c.session_id.clone()))
                && let Some(session) =
                    ingest::get_session_meta(analytics_db, &c.source, &c.session_id)?
            {
                sessions.push(session);
            }
        }
    }
    sessions.sort_by_key(|meta| std::cmp::Reverse(meta.last_at));
    sessions.truncate(budget);

    // Recent top-up: the proposal's own project, or — for a global proposal — the
    // union across the projects that contributed the motivating patterns.
    let top_up: Vec<ingest::SessionMeta> = match proposal.scope.strip_prefix("project:") {
        Some(project) => ingest::recent_sessions_for_project(analytics_db, project, budget)?,
        None => {
            let mut projects: Vec<&str> = contributing_scopes
                .iter()
                .filter_map(|s| s.strip_prefix("project:"))
                .collect();
            projects.sort_unstable();
            projects.dedup();
            let mut union = Vec::new();
            for project in &projects {
                union.extend(ingest::recent_sessions_for_project(
                    analytics_db,
                    project,
                    budget,
                )?);
            }
            // Each project's sessions are most-recent-first, the union is not; re-sort
            // before the budget truncates the replay set.
            union.sort_by_key(|meta| std::cmp::Reverse(meta.last_at));
            union.truncate(budget);
            union
        }
    };
    for meta in top_up {
        if sessions.len() >= budget {
            break;
        }
        if seen.insert((meta.source.clone(), meta.session_id.clone())) {
            sessions.push(meta);
        }
    }
    Ok(sessions)
}

/// Tier 1 — counterfactual LLM judge over historical sessions that hit the failure
/// mode (see [`replay_sessions`]). A proposal with neither corroborating sessions nor
/// resolvable project history fails closed: it would otherwise skip both real gates.
pub fn tier1(
    cfg: &WikiLoopConfig,
    proposal: &Proposal,
    skill_markdown: &str,
    contributing_scopes: &[String],
) -> Result<Option<GateResult>> {
    let catalog = super::patterns::PatternStore::new(&cfg.wiki_root)?.catalog()?;
    let sessions = replay_sessions(
        &cfg.analytics_db(),
        &catalog,
        proposal,
        contributing_scopes,
        cfg.tier1_sessions,
    )?;
    if sessions.is_empty() {
        return Ok(Some(match proposal.scope.strip_prefix("project:") {
            Some(project) => GateResult {
                passed: false,
                detail: format!("no historical sessions found for project `{project}`"),
            },
            None => {
                let projects: Vec<&str> = contributing_scopes
                    .iter()
                    .filter_map(|s| s.strip_prefix("project:"))
                    .collect();
                GateResult {
                    passed: false,
                    detail: if projects.is_empty() {
                        "global scope with no contributing project patterns: set a `project:` \
                         scope or re-propose from patterns observed in a live repo"
                            .into()
                    } else {
                        format!(
                            "global scope: no historical sessions found for contributing \
                             project(s) {}",
                            projects.join(", ")
                        )
                    },
                }
            }
        }));
    }

    let index_dir = cfg.index_dir()?;
    let mut digest = String::new();
    let mut used = 0usize;
    let mut digested_ids: Vec<String> = Vec::new();
    for meta in &sessions {
        let records = match ingest::load_records(&index_dir, meta) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let indices = super::digest::extract_error_turn_indices(&records);
        if indices.is_empty() {
            continue;
        }
        let summary = super::digest::build_session_summary(cfg, meta, &records, &indices);
        digest.push_str(&summary);
        digest.push_str("\n\n---\n\n");
        digested_ids.push(meta.session_id.clone());
        used += 1;
        if used >= cfg.tier1_sessions || digest.len() > cfg.max_chars_per_batch / 2 {
            break;
        }
    }
    if used == 0 {
        return Ok(Some(GateResult {
            passed: true,
            detail: "no historical error turns available to judge against; skipped".into(),
        }));
    }

    let prompt = format!(
        "{}\n\n## Candidate Skill\n\n```markdown\n{}\n```\n\n## Historical Sessions\n\n{}",
        super::prompts::JUDGE,
        skill_markdown,
        digest
    );
    let schema = if cfg.judge.json_schema {
        Some(cfg.schema_path("judge", super::prompts::JUDGE_SCHEMA)?)
    } else {
        None
    };
    let value = super::harness::run_role_structured(
        &cfg.judge,
        &prompt,
        schema.as_deref(),
        std::time::Duration::from_secs(cfg.subprocess_timeout_secs),
    )?;
    if value
        .get("assessments")
        .and_then(|v| v.as_array())
        .is_none()
    {
        bail!("judge output missing `assessments`");
    }
    Ok(Some(judge_verdict(&value, &digested_ids)))
}

/// Tally the judge's per-session assessments against the sessions that were actually
/// digested into the judge prompt. Expected sessions the judge did not assess count as
/// NOT improved; assessments for sessions the judge was never shown are ignored (noted
/// in the detail). Passes only when strictly more than half of the expected sessions
/// improved, so a single optimistic entry cannot carry a proposal.
fn judge_verdict(value: &serde_json::Value, expected_ids: &[String]) -> GateResult {
    if expected_ids.is_empty() {
        return GateResult {
            passed: false,
            detail: "judge verdict requested with no digested sessions; refusing to pass".into(),
        };
    }
    let assessments = value
        .get("assessments")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut improved_ids: Vec<&str> = Vec::new();
    let mut unknown_ids: Vec<&str> = Vec::new();
    let mut objections: Vec<String> = Vec::new();
    for a in assessments {
        let Some(session_id) = a.get("session_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !expected_ids.iter().any(|e| e == session_id) {
            unknown_ids.push(session_id);
            continue;
        }
        if a.get("would_improve").and_then(|v| v.as_bool()) == Some(true) {
            if !improved_ids.contains(&session_id) {
                improved_ids.push(session_id);
            }
        } else if objections.len() < 3
            && let Some(reason) = a.get("reason").and_then(|v| v.as_str())
        {
            objections.push(reason.to_string());
        }
    }
    let improved = improved_ids.len();
    let total = expected_ids.len();
    let passed = improved * 2 > total;
    let mut detail = format!("judge: {improved}/{total} historical sessions would improve");
    if !objections.is_empty() {
        detail.push_str(&format!("; objections: {}", objections.join(" | ")));
    }
    if !unknown_ids.is_empty() {
        detail.push_str(&format!(
            "; ignored unrequested session(s): {}",
            unknown_ids.join(", ")
        ));
    }
    GateResult { passed, detail }
}

// ---- delivery: apply / rollback / impact audit ----

fn skills_backup_dir(cfg: &WikiLoopConfig) -> PathBuf {
    cfg.proposals_dir
        .parent()
        .unwrap_or(Path::new("/tmp"))
        .join("skills_backup")
}

/// Paper step 16: the audit trail is appended programmatically after every decision.
pub fn append_skill_impact(wiki_root: &Path, entry: &str) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(wiki_root)?;
    let path = wiki_root.join("skill-impact.md");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{entry}")?;
    Ok(())
}

/// Apply a validated proposal to the live skills root (Tier 3 human-approved step).
pub fn apply(cfg: &WikiLoopConfig, ledger: &StateLedger, proposal_id: &str) -> Result<String> {
    let proposal = Proposal::load(&cfg.proposals_dir, proposal_id)?;
    if proposal.id != proposal_id {
        bail!("proposal id mismatch");
    }
    // Tier-0 results are stored per gate (secret_scan, name_slug, scope_stamp, ...),
    // never under a single `tier0` key: require at least one Tier-0 entry and all of
    // them passing before anything is written.
    let tier0_gates: Vec<(&String, &GateResult)> = proposal
        .gates
        .iter()
        .filter(|(name, _)| name.as_str() != "tier1")
        .collect();
    if tier0_gates.is_empty() {
        bail!("proposal has no Tier-0 gate results; run `wiki-loop validate {proposal_id}` first");
    }
    let failed: Vec<&str> = tier0_gates
        .iter()
        .filter(|(_, g)| !g.passed)
        .map(|(name, _)| name.as_str())
        .collect();
    if !failed.is_empty() {
        bail!(
            "proposal failed Tier-0 gate(s) {}; re-run `wiki-loop validate {proposal_id}` before applying",
            failed.join(", ")
        );
    }
    if let Some(tier1) = proposal.gates.get("tier1")
        && !tier1.passed
    {
        bail!("proposal failed Tier 1; not applying");
    }

    let skill_dir = cfg.skills_root.join(&proposal.skill_name);
    let skill_path = skill_dir.join("SKILL.md");
    let new_content = proposal.skill_markdown(&cfg.proposals_dir)?;
    // Version numbers are immutable history ((skill_id, version) is the PK): the next
    // version counts retired rows too, so a re-apply after rollback never collides.
    let version = ledger.next_deployment_version(&proposal.skill_name)?;

    // Back up the currently-deployed version for rollback.
    if let Some(existing) = read_if_exists(&skill_path) {
        let backup_dir = skills_backup_dir(cfg).join(&proposal.skill_name);
        std::fs::create_dir_all(&backup_dir)?;
        std::fs::write(backup_dir.join(format!("v{version}.md")), existing)?;
    }

    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(&skill_path, &new_content)?;
    let purpose = format!(
        "# Purpose\n\nSkill `{}` was compiled from wiki patterns: {}\n\nRationale: {}\n",
        proposal.skill_name,
        proposal.purpose_patterns.join(", "),
        proposal.rationale
    );
    std::fs::write(skill_dir.join("PURPOSE.md"), purpose)?;

    ledger.record_deployment(&proposal.skill_name, version, &proposal.scope, &proposal.id)?;
    let mut proposal = proposal;
    proposal.status = "accepted".into();
    proposal.save(&cfg.proposals_dir)?;
    // Mirror the decision into the ledger: the staged-proposals review surface
    // (status output, TUI skills screen) reads statuses from state.db, and a
    // still-`validated` row would keep an applied proposal queued for review —
    // and make the dedup gate treat the skill as merely staged.
    ledger.set_proposal_status(
        &proposal.id,
        "accepted",
        &serde_json::to_string(&proposal.gates)?,
    )?;

    append_skill_impact(
        &cfg.wiki_root,
        &format!(
            "## {} — {} — {} (v{})\n- decision: accepted\n- patterns: {}\n- scope: {}\n\n```diff\n{}\n```\n",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            proposal.id,
            proposal.skill_name,
            version,
            proposal.purpose_patterns.join(", "),
            proposal.scope,
            proposal.diff(&cfg.proposals_dir)?
        ),
    )?;
    Ok(format!(
        "applied `{}` v{version} to {}",
        proposal.skill_name,
        skill_path.display()
    ))
}

/// Deny an open proposal from the review surface (TUI skills screen or CLI): the
/// human looked at it and said no. Flips pending/validated → rejected — which the
/// recently-rejected gate reads to mute re-proposals for its window — and records
/// the decision in the audit trail. Nothing is written to the skills root.
pub fn deny(cfg: &WikiLoopConfig, ledger: &StateLedger, proposal_id: &str) -> Result<String> {
    let mut proposal = Proposal::load(&cfg.proposals_dir, proposal_id)?;
    if proposal.id != proposal_id {
        bail!("proposal id mismatch");
    }
    if proposal.status != "pending" && proposal.status != "validated" {
        bail!(
            "proposal is `{}`; only pending or validated proposals can be denied",
            proposal.status
        );
    }
    let previous_status = proposal.status.clone();
    proposal.status = "rejected".into();
    proposal.gates.insert(
        "human_review".into(),
        GateResult {
            passed: false,
            detail: format!(
                "denied by human review at {} (was {previous_status})",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
        },
    );
    proposal.save(&cfg.proposals_dir)?;
    ledger.set_proposal_status(
        proposal_id,
        "rejected",
        &serde_json::to_string(&proposal.gates)?,
    )?;
    append_skill_impact(
        &cfg.wiki_root,
        &format!(
            "## {} — {} — {} (denied in review)\n- decision: rejected\n- was: {}\n- patterns: {}\n",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            proposal.id,
            proposal.skill_name,
            previous_status,
            proposal.purpose_patterns.join(", ")
        ),
    )?;
    Ok(format!("denied `{}` ({proposal_id})", proposal.skill_name))
}

/// Roll a skill back to its previous deployed version (rollback primacy; the wiki and
/// `skill-impact.md` history are never rolled back). Skills live in one shared
/// skills root; the recorded scope is attribution, not a location.
pub fn rollback(cfg: &WikiLoopConfig, ledger: &StateLedger, skill_name: &str) -> Result<String> {
    if !is_valid_slug(skill_name) {
        bail!("invalid skill name");
    }
    let Some(version) = ledger.latest_deployment_version(skill_name)? else {
        bail!("skill `{skill_name}` has no active deployment");
    };

    let skill_dir = cfg.skills_root.join(skill_name);
    let skill_path = skill_dir.join("SKILL.md");
    let backup = skills_backup_dir(cfg)
        .join(skill_name)
        .join(format!("v{version}.md"));
    if backup.exists() {
        // Restore the backed-up predecessor: at v1 that is the hand-authored SKILL.md
        // wiki-loop overwrote. Other files in the skill dir are left untouched.
        let content = std::fs::read_to_string(&backup)
            .with_context(|| format!("reading backup {}", backup.display()))?;
        std::fs::write(&skill_path, content)?;
    } else if version > 1 {
        bail!(
            "backup {} is missing; refusing to roll `{skill_name}` back blind",
            backup.display()
        );
    } else {
        // v1 with no backup: wiki-loop created the skill fresh, so only the files
        // wiki-loop itself wrote are removed — never anything else in the directory.
        let _ = std::fs::remove_file(&skill_path);
        let _ = std::fs::remove_file(skill_dir.join("PURPOSE.md"));
        // Best-effort cleanup of the now-maybe-empty directory.
        let _ = std::fs::remove_dir(&skill_dir);
    }
    ledger.retire_deployments(skill_name)?;

    append_skill_impact(
        &cfg.wiki_root,
        &format!(
            "## {} — manual rollback — {} (was v{})\n- decision: reverted\n",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            skill_name,
            version
        ),
    )?;
    Ok(format!("rolled back `{skill_name}` (was v{version})"))
}

fn read_if_exists(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(tmp: &tempfile::TempDir) -> WikiLoopConfig {
        WikiLoopConfig {
            queue_dir: tmp.path().join("queue"),
            state_db: tmp.path().join("state.db"),
            wiki_root: tmp.path().join("wiki"),
            skills_root: tmp.path().join("skills"),
            proposals_dir: tmp.path().join("proposals"),
            ..WikiLoopConfig::default()
        }
    }

    fn stage_proposal(
        cfg: &WikiLoopConfig,
        id: &str,
        skill_name: &str,
        markdown: &str,
    ) -> Proposal {
        let dir = cfg.proposals_dir.join(id);
        std::fs::create_dir_all(&dir).expect("proposal dir");
        std::fs::write(dir.join("SKILL.md"), markdown).expect("staged SKILL.md");
        std::fs::write(dir.join("skill.diff"), "--- a\n+++ b\n").expect("staged skill.diff");
        let proposal = Proposal {
            id: id.to_string(),
            created_at: now_ms(),
            skill_name: skill_name.to_string(),
            description: "staged for tests".into(),
            status: "pending".into(),
            scope: "global".into(),
            purpose_patterns: vec!["pat_test_1".into()],
            rationale: "test rationale".into(),
            gates: Default::default(),
        };
        proposal.save(&cfg.proposals_dir).expect("save proposal");
        proposal
    }

    /// Validate-equivalent (`--skip-tier1` mode — tier1 invokes an LLM harness): run
    /// tier0, record the results, and flip the status per the overall outcome.
    fn validate_tier0(cfg: &WikiLoopConfig, ledger: &StateLedger, proposal: &mut Proposal) -> bool {
        let skill_md = proposal
            .skill_markdown(&cfg.proposals_dir)
            .expect("staged skill markdown");
        let existing = read_if_exists(&cfg.skills_root.join(&proposal.skill_name).join("SKILL.md"));
        let results =
            tier0(cfg, ledger, proposal, &skill_md, existing.as_deref(), &[]).expect("tier0 runs");
        let mut all_passed = true;
        for (name, result) in results {
            all_passed &= result.passed;
            proposal.gates.insert(name, result);
        }
        proposal.status = if all_passed { "validated" } else { "rejected" }.into();
        proposal.save(&cfg.proposals_dir).expect("save proposal");
        all_passed
    }

    #[test]
    fn tier0_dedup_fails_while_same_skill_is_staged() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_cfg(&tmp);
        let ledger = StateLedger::open(&cfg.state_db).expect("ledger");

        // A validated-but-unapplied proposal is already staged for this skill —
        // invisible to an active-skills scan, but a duplicate nonetheless.
        let p1 = stage_proposal(&cfg, "prop_d_1", "dup-skill", "# dup-skill\n\nv1.\n");
        ledger
            .insert_proposal("prop_d_1", "dup-skill", &[])
            .expect("insert");
        ledger
            .set_proposal_status("prop_d_1", "validated", "{}")
            .expect("status");

        // A second proposal for the same skill fails dedup even with new content.
        let p2 = stage_proposal(&cfg, "prop_d_2", "dup-skill", "# dup-skill\n\nv2.\n");
        let skill_md = p2
            .skill_markdown(&cfg.proposals_dir)
            .expect("skill markdown");
        let results = tier0(&cfg, &ledger, &p2, &skill_md, None, &[]).expect("tier0 runs");
        let (_, dedup) = results
            .iter()
            .find(|(name, _)| name == "dedup")
            .expect("dedup gate ran");
        assert!(!dedup.passed, "staged duplicate must fail dedup");
        assert!(
            dedup.detail.contains("prop_d_1"),
            "detail should point at the staged proposal: {}",
            dedup.detail
        );

        // Re-validating the staged proposal itself must not trip its own gate.
        let skill_md = p1
            .skill_markdown(&cfg.proposals_dir)
            .expect("skill markdown");
        let results = tier0(&cfg, &ledger, &p1, &skill_md, None, &[]).expect("tier0 runs");
        let (_, dedup) = results
            .iter()
            .find(|(name, _)| name == "dedup")
            .expect("dedup gate ran");
        assert!(dedup.passed, "{}", dedup.detail);

        // Once the staged one is denied, a fresh proposal is reviewable again.
        deny(&cfg, &ledger, "prop_d_1").expect("deny staged");
        let skill_md = p2
            .skill_markdown(&cfg.proposals_dir)
            .expect("skill markdown");
        let results = tier0(&cfg, &ledger, &p2, &skill_md, None, &[]).expect("tier0 runs");
        let (_, dedup) = results
            .iter()
            .find(|(name, _)| name == "dedup")
            .expect("dedup gate ran");
        assert!(dedup.passed, "{}", dedup.detail);
    }

    #[test]
    fn deny_rejects_open_proposal_and_audits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_cfg(&tmp);
        let ledger = StateLedger::open(&cfg.state_db).expect("ledger");

        stage_proposal(&cfg, "prop_n_1", "deny-skill", "# deny-skill\n\nnope.\n");
        ledger
            .insert_proposal("prop_n_1", "deny-skill", &[])
            .expect("insert");
        ledger
            .set_proposal_status("prop_n_1", "validated", "{}")
            .expect("status");

        let msg = deny(&cfg, &ledger, "prop_n_1").expect("deny");
        assert!(msg.contains("deny-skill"), "{msg}");

        // Out of the review surface, in both stores.
        assert!(ledger.staged_proposals(10).expect("staged").is_empty());
        let saved = Proposal::load(&cfg.proposals_dir, "prop_n_1").expect("reload");
        assert_eq!(saved.status, "rejected");
        let review = saved
            .gates
            .get("human_review")
            .expect("human_review gate stamped");
        assert!(review.detail.contains("denied by human review"));

        // Nothing was deployed...
        assert!(!cfg.skills_root.join("deny-skill/SKILL.md").exists());
        // ...but the decision is audited.
        let impact =
            std::fs::read_to_string(cfg.wiki_root.join("skill-impact.md")).expect("impact");
        assert!(impact.contains("denied in review"));

        // A closed proposal cannot be denied again.
        assert!(deny(&cfg, &ledger, "prop_n_1").is_err());
    }

    #[test]
    fn proposal_id_validation() {
        assert!(is_valid_proposal_id(
            "prop_19a7afd1e00_19a2f3c0de1122334455"
        ));
        assert!(!is_valid_proposal_id("../etc"));
        assert!(!is_valid_proposal_id("UPPER"));
        assert!(!is_valid_proposal_id(""));
        assert!(!is_valid_proposal_id("-leading"));
        assert!(!is_valid_proposal_id("trailing_"));
        assert!(!is_valid_proposal_id(&"x".repeat(101)));
    }

    #[test]
    fn apply_rollback_reapply_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_cfg(&tmp);
        let ledger = StateLedger::open(&cfg.state_db).expect("ledger");

        // v1: fresh skill.
        let mut p1 = stage_proposal(&cfg, "prop_a_1", "demo-skill", "# demo-skill\n\nv1 body.\n");
        assert!(validate_tier0(&cfg, &ledger, &mut p1), "tier0 must pass");
        let msg = apply(&cfg, &ledger, "prop_a_1").expect("apply v1");
        assert!(msg.contains("v1"), "{msg}");
        assert_eq!(
            ledger.latest_deployment_version("demo-skill").expect("v"),
            Some(1)
        );
        let skill_path = cfg.skills_root.join("demo-skill/SKILL.md");
        assert_eq!(
            std::fs::read_to_string(&skill_path).expect("deployed SKILL.md"),
            "# demo-skill\n\nv1 body.\n"
        );
        assert!(cfg.skills_root.join("demo-skill/PURPOSE.md").exists());

        // v2: patch over the live v1.
        let mut p2 = stage_proposal(&cfg, "prop_a_2", "demo-skill", "# demo-skill\n\nv2 body.\n");
        assert!(validate_tier0(&cfg, &ledger, &mut p2));
        apply(&cfg, &ledger, "prop_a_2").expect("apply v2");
        assert_eq!(
            ledger.latest_deployment_version("demo-skill").expect("v"),
            Some(2)
        );
        let backup = skills_backup_dir(&cfg).join("demo-skill/v2.md");
        assert_eq!(
            std::fs::read_to_string(&backup).expect("v2 backup holds the v1 content"),
            "# demo-skill\n\nv1 body.\n"
        );

        // Rollback restores the v1 content.
        rollback(&cfg, &ledger, "demo-skill").expect("rollback");
        assert_eq!(
            std::fs::read_to_string(&skill_path).expect("restored SKILL.md"),
            "# demo-skill\n\nv1 body.\n"
        );
        assert_eq!(
            ledger.latest_deployment_version("demo-skill").expect("v"),
            None
        );

        // F9 regression: re-apply after rollback lands on v3, not a colliding v1.
        let mut p3 = stage_proposal(&cfg, "prop_a_3", "demo-skill", "# demo-skill\n\nv3 body.\n");
        assert!(validate_tier0(&cfg, &ledger, &mut p3));
        let msg = apply(&cfg, &ledger, "prop_a_3").expect("apply v3 after rollback");
        assert!(msg.contains("v3"), "{msg}");
        assert_eq!(
            ledger.latest_deployment_version("demo-skill").expect("v"),
            Some(3)
        );
    }

    #[test]
    fn rollback_v1_preserves_hand_authored_skill_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_cfg(&tmp);
        let ledger = StateLedger::open(&cfg.state_db).expect("ledger");

        // A hand-authored skill wiki-loop overwrites at v1.
        let skill_dir = cfg.skills_root.join("authored-skill");
        std::fs::create_dir_all(skill_dir.join("scripts")).expect("scripts dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "original hand-authored content\n",
        )
        .expect("authored SKILL.md");
        std::fs::write(skill_dir.join("scripts/run.sh"), "#!/bin/sh\nexit 0\n").expect("run.sh");

        let mut p = stage_proposal(
            &cfg,
            "prop_b_1",
            "authored-skill",
            "# authored-skill\n\nwiki-loop says hi.\n",
        );
        assert!(validate_tier0(&cfg, &ledger, &mut p));
        apply(&cfg, &ledger, "prop_b_1").expect("apply v1");
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("SKILL.md")).expect("overwritten SKILL.md"),
            "# authored-skill\n\nwiki-loop says hi.\n"
        );

        rollback(&cfg, &ledger, "authored-skill").expect("rollback");
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("SKILL.md")).expect("restored SKILL.md"),
            "original hand-authored content\n"
        );
        assert!(
            skill_dir.join("scripts/run.sh").exists(),
            "hand-authored extras survive rollback"
        );
    }

    #[test]
    fn apply_requires_passing_tier0_gates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_cfg(&tmp);
        let ledger = StateLedger::open(&cfg.state_db).expect("ledger");

        // No gates recorded at all.
        stage_proposal(&cfg, "prop_c_1", "gateless-skill", "# gateless\n\nbody.\n");
        let err = apply(&cfg, &ledger, "prop_c_1").expect_err("empty gates must bail");
        assert!(
            format!("{err:#}").contains("no Tier-0 gate results"),
            "{err:#}"
        );

        // One failing Tier-0 gate, named in the error.
        let mut p2 = stage_proposal(&cfg, "prop_c_2", "gateless-skill", "# gateless\n\nbody.\n");
        assert!(validate_tier0(&cfg, &ledger, &mut p2));
        p2.gates.insert(
            "secret_scan".into(),
            GateResult {
                passed: false,
                detail: "test".into(),
            },
        );
        p2.save(&cfg.proposals_dir).expect("save proposal");
        let err = apply(&cfg, &ledger, "prop_c_2").expect_err("failing gate must bail");
        assert!(format!("{err:#}").contains("secret_scan"), "{err:#}");
    }

    fn assessment(id: &str, improves: bool, reason: &str) -> serde_json::Value {
        serde_json::json!({"session_id": id, "would_improve": improves, "reason": reason})
    }

    #[test]
    fn judge_verdict_requires_majority_of_expected_sessions() {
        let expected: Vec<String> = ["s1", "s2", "s3", "s4", "s5"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // One optimistic entry against five digested sessions: fail (F16's 1*2 > 1).
        let v = serde_json::json!({"assessments": [assessment("s1", true, "")]});
        let r = judge_verdict(&v, &expected);
        assert!(!r.passed);
        assert!(r.detail.contains("1/5"), "{}", r.detail);

        // 3 of 5 — strictly more than half: pass, objections surface.
        let v = serde_json::json!({"assessments": [
            assessment("s1", true, ""),
            assessment("s2", true, ""),
            assessment("s3", true, ""),
            assessment("s4", false, "already handled"),
            assessment("s5", false, "unrelated failure"),
        ]});
        let r = judge_verdict(&v, &expected);
        assert!(r.passed);
        assert!(r.detail.contains("3/5"), "{}", r.detail);
        assert!(r.detail.contains("already handled"), "{}", r.detail);

        // Assessments for sessions the judge was never shown are ignored, not tallied.
        let v = serde_json::json!({"assessments": [
            assessment("s1", true, ""),
            assessment("s2", true, ""),
            assessment("s3", true, ""),
            assessment("hallucinated", true, ""),
        ]});
        let r = judge_verdict(&v, &expected);
        assert!(r.passed, "{}", r.detail);
        assert!(r.detail.contains("hallucinated"), "{}", r.detail);

        // Expected sessions the judge skipped count as not improved (2/5).
        let v = serde_json::json!({"assessments": [
            assessment("s1", true, ""),
            assessment("s2", true, ""),
        ]});
        let r = judge_verdict(&v, &expected);
        assert!(!r.passed);
        assert!(r.detail.contains("2/5"), "{}", r.detail);

        // No expected sessions at all: fail closed.
        let r = judge_verdict(&v, &[]);
        assert!(!r.passed);
    }

    #[test]
    fn tier1_replay_prefers_corroborating_sessions() {
        let tmp = tempfile::tempdir().expect("tempdir");

        // Analytics store: one corroborated (failure-mode) session and one newer,
        // unrelated session in the contributing project.
        let db = tmp.path().join("analytics.sqlite");
        let conn = rusqlite::Connection::open(&db).expect("analytics db");
        conn.execute_batch(
            "CREATE TABLE sessions (
                 source TEXT, session_id TEXT, source_path TEXT, project TEXT,
                 cwd TEXT, git_root TEXT, repo_project TEXT,
                 started_at INTEGER, last_at INTEGER, message_count INTEGER,
                 resolution_status TEXT
             );
             INSERT INTO sessions VALUES
                 ('claude', 'sess_old', '', 'demo', '', '', 'demo', 900, 1000, 5, 'failing'),
                 ('agy',    'sess_new', '', 'demo', '', '', 'demo', 1900, 2000, 9, 'passing');",
        )
        .expect("seed sessions");

        // Wiki pattern page whose corroboration cites the failure-mode session.
        let wiki = tmp.path().join("wiki");
        std::fs::create_dir_all(wiki.join("patterns")).expect("patterns dir");
        std::fs::write(
            wiki.join("patterns/demo-failure.md"),
            "---\nid: pat_demo\ntitle: \"Demo Failure\"\nstatus: candidate\nkind: failure\n\
             scope: project:demo\ncreated: 2026-09-21T00:00:00Z\nupdated: 2026-09-21T00:00:00Z\n\
             corroboration:\n  - source: claude, session_id: sess_old, ts: 1000\n\
             superseded_by: null\nscrub_pass: v1\n---\n\n## Symptom\nDemo.\n",
        )
        .expect("pattern page");
        let catalog = super::super::patterns::PatternStore::new(&wiki)
            .expect("store")
            .catalog()
            .expect("catalog");
        assert!(catalog.contains_key("pat_demo"), "pattern page must parse");

        let proposal = Proposal {
            id: "prop_r_1".into(),
            created_at: now_ms(),
            skill_name: "replay-skill".into(),
            description: "d".into(),
            status: "pending".into(),
            scope: "global".into(),
            purpose_patterns: vec!["pat_demo".into()],
            rationale: "r".into(),
            gates: Default::default(),
        };
        let scopes = vec!["project:demo".to_string()];

        // Corroborating session leads even though the top-up session is more recent.
        let replay = replay_sessions(&db, &catalog, &proposal, &scopes, 5).expect("replay set");
        let ids: Vec<&str> = replay.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["sess_old", "sess_new"],
            "failure-mode session first"
        );

        // Budget of one keeps the corroborated session and drops the top-up.
        let replay = replay_sessions(&db, &catalog, &proposal, &scopes, 1).expect("replay set");
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].session_id, "sess_old");

        // A pattern citing both sessions dedupes against the top-up entirely.
        std::fs::write(
            wiki.join("patterns/demo-failure.md"),
            "---\nid: pat_demo\ntitle: \"Demo Failure\"\nstatus: candidate\nkind: failure\n\
             scope: project:demo\ncreated: 2026-09-21T00:00:00Z\nupdated: 2026-09-21T00:00:00Z\n\
             corroboration:\n  - source: claude, session_id: sess_old, ts: 1000\n  \
             - source: agy, session_id: sess_new, ts: 2000\n\
             superseded_by: null\nscrub_pass: v1\n---\n\n## Symptom\nDemo.\n",
        )
        .expect("pattern page");
        let catalog = super::super::patterns::PatternStore::new(&wiki)
            .expect("store")
            .catalog()
            .expect("catalog");
        let replay = replay_sessions(&db, &catalog, &proposal, &scopes, 5).expect("replay set");
        assert_eq!(
            replay.len(),
            2,
            "no duplicate when corroborated also recurs in top-up"
        );
    }

    #[test]
    fn tier1_global_without_contributing_scopes_fails_closed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_cfg(&tmp);
        let proposal = Proposal {
            id: "prop_d_1".into(),
            created_at: now_ms(),
            skill_name: "global-skill".into(),
            description: "d".into(),
            status: "pending".into(),
            scope: "global".into(),
            purpose_patterns: vec![],
            rationale: "r".into(),
            gates: Default::default(),
        };
        // No contributing scopes: no analytics lookup, no model call — fail closed.
        let result = tier1(&cfg, &proposal, "# global-skill\n\nbody.\n", &[])
            .expect("tier1 runs")
            .expect("Some");
        assert!(!result.passed);
        assert!(
            result.detail.contains("no contributing project patterns"),
            "{}",
            result.detail
        );
    }

    #[test]
    fn tier0_global_with_referenced_paths_fails_closed_without_scopes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = test_cfg(&tmp);
        let ledger = StateLedger::open(&cfg.state_db).expect("ledger");
        let p = stage_proposal(
            &cfg,
            "prop_e_1",
            "ref-skill",
            "# ref-skill\n\nRun `src/tools/frob.py` first.\n",
        );
        let skill_md = p
            .skill_markdown(&cfg.proposals_dir)
            .expect("skill markdown");
        let results = tier0(&cfg, &ledger, &p, &skill_md, None, &[]).expect("tier0 runs");
        let (_, referenced) = results
            .iter()
            .find(|(name, _)| name == "referenced_paths")
            .expect("referenced_paths gate ran");
        assert!(!referenced.passed);
        assert!(
            referenced.detail.contains("no contributing project scopes"),
            "{}",
            referenced.detail
        );
    }
}
