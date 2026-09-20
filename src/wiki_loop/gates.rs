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

impl Proposal {
    pub fn load(dir: &Path, proposal_id: &str) -> Result<Self> {
        if !is_valid_slug(proposal_id) {
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
/// the skill already exists (patch proposals), else None.
pub fn tier0(
    cfg: &WikiLoopConfig,
    ledger: &StateLedger,
    proposal: &Proposal,
    skill_markdown: &str,
    existing_skill: Option<&str>,
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

    // 4. Dedup: identical content already live.
    results.push((
        "dedup".into(),
        match existing_skill {
            Some(existing) if existing.trim() == skill_markdown.trim() => GateResult {
                passed: false,
                detail: "proposal duplicates the already-active skill".into(),
            },
            _ => GateResult {
                passed: true,
                detail: "not a duplicate of the active skill".into(),
            },
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
    results.push(referenced_paths_gate(cfg, proposal, skill_markdown)?);

    Ok(results)
}

fn referenced_paths_gate(
    cfg: &WikiLoopConfig,
    proposal: &Proposal,
    skill_markdown: &str,
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

    // Resolve the target repo root from the most recent session of that project.
    let project = proposal.scope.strip_prefix("project:");
    let Some(project) = project else {
        return Ok((
            "referenced_paths".into(),
            GateResult {
                passed: true,
                detail: format!(
                    "global skill references paths ({}) — verify manually",
                    refs.join(", ")
                ),
            },
        ));
    };
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
    let missing: Vec<&String> = refs
        .iter()
        .filter(|r| !root.join(r.as_str()).exists())
        .collect();
    Ok((
        "referenced_paths".into(),
        if missing.is_empty() {
            GateResult {
                passed: true,
                detail: format!(
                    "{} referenced path(s) exist under {}",
                    refs.len(),
                    root.display()
                ),
            }
        } else {
            GateResult {
                passed: false,
                detail: format!(
                    "missing in {}: {}",
                    root.display(),
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

static MARKDOWN_CODE: once_cell::sync::Lazy<regex::Regex> =
    once_cell::sync::Lazy::new(|| regex::Regex::new("`([^`\\n]+)`").expect("static regex"));

/// Tier 1 — counterfactual LLM judge over K recent sessions of the proposal's project.
/// Returns None when the judge is not applicable (global scope, no historical sessions,
/// or no judge configured output).
pub fn tier1(
    cfg: &WikiLoopConfig,
    proposal: &Proposal,
    skill_markdown: &str,
) -> Result<Option<GateResult>> {
    let Some(project) = proposal.scope.strip_prefix("project:") else {
        return Ok(Some(GateResult {
            passed: true,
            detail: "global scope: no project-specific replay set; skipped".into(),
        }));
    };
    let sessions =
        ingest::recent_sessions_for_project(&cfg.analytics_db(), project, cfg.tier1_sessions)?;
    if sessions.is_empty() {
        return Ok(Some(GateResult {
            passed: false,
            detail: format!("no historical sessions found for project `{project}`"),
        }));
    }

    let index_dir = cfg.index_dir()?;
    let mut digest = String::new();
    let mut used = 0usize;
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
    let raw = super::harness::run_role(
        &cfg.judge,
        &prompt,
        None,
        std::time::Duration::from_secs(cfg.subprocess_timeout_secs),
    )?;
    let value = super::harness::extract_json_object(&raw)?;
    let Some(assessments) = value.get("assessments").and_then(|v| v.as_array()) else {
        bail!("judge output missing `assessments`");
    };
    let total = assessments.len();
    let improved = assessments
        .iter()
        .filter(|a| a.get("would_improve").and_then(|v| v.as_bool()) == Some(true))
        .count();
    let reasons: Vec<String> = assessments
        .iter()
        .filter(|a| a.get("would_improve").and_then(|v| v.as_bool()) != Some(true))
        .filter_map(|a| a.get("reason").and_then(|v| v.as_str()).map(str::to_string))
        .take(3)
        .collect();
    let passed = total > 0 && improved * 2 > total;
    Ok(Some(GateResult {
        passed,
        detail: format!(
            "judge: {improved}/{total} historical sessions would improve{}",
            if reasons.is_empty() {
                String::new()
            } else {
                format!("; objections: {}", reasons.join(" | "))
            }
        ),
    }))
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
    let tier0 = proposal.gates.get("tier0");
    match tier0 {
        Some(g) if g.passed => {}
        _ => bail!("proposal has not passed Tier 0; run `wiki-loop validate {proposal_id}` first"),
    }
    if let Some(tier1) = proposal.gates.get("tier1")
        && !tier1.passed
    {
        bail!("proposal failed Tier 1; not applying");
    }

    let skill_dir = cfg.skills_root.join(&proposal.skill_name);
    let skill_path = skill_dir.join("SKILL.md");
    let new_content = proposal.skill_markdown(&cfg.proposals_dir)?;
    let version = ledger
        .latest_deployment_version(&proposal.skill_name)?
        .unwrap_or(0)
        + 1;

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

/// Roll a skill back to its previous deployed version (rollback primacy; the wiki and
/// `skill-impact.md` history are never rolled back).
pub fn rollback(cfg: &WikiLoopConfig, ledger: &StateLedger, skill_name: &str) -> Result<String> {
    if !is_valid_slug(skill_name) {
        bail!("invalid skill name");
    }
    let Some(version) = ledger.latest_deployment_version(skill_name)? else {
        bail!("skill `{skill_name}` has no active deployment");
    };

    let skill_dir = cfg.skills_root.join(skill_name);
    let skill_path = skill_dir.join("SKILL.md");
    let previous = if version > 1 {
        let backup = skills_backup_dir(cfg)
            .join(skill_name)
            .join(format!("v{version}.md"));
        Some(
            std::fs::read_to_string(&backup)
                .with_context(|| format!("reading backup {}", backup.display()))?,
        )
    } else {
        None
    };

    match previous {
        Some(content) => std::fs::write(&skill_path, content)?,
        None => {
            let _ = std::fs::remove_dir_all(&skill_dir);
        }
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
