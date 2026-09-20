//! Skill Proposer (paper step 11): reads the wiki index and the skill-impact audit trail
//! **first**, then corroborated pattern pages and active skills, and emits at most one
//! atomic single-skill proposal as a staged directory + unified diff.
//!
//! One shared wiki backs every project under the workspace root, so one proposal
//! opportunity per run; the weekly ceiling bounds the human's approval load.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use super::config::WikiLoopConfig;
use super::gates::{GateResult, Proposal};
use super::ledger::StateLedger;
use super::patterns::is_valid_slug;
use super::queue::now_ms;

#[derive(Debug, Deserialize)]
struct ProposerOutput {
    skill_name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    skill_markdown: String,
    #[serde(default)]
    purpose_patterns: Vec<String>,
    #[serde(default)]
    rationale: String,
}

#[derive(Debug, Serialize)]
struct ExistingSkillSummary {
    name: String,
    description: String,
}

pub fn run_proposer(cfg: &WikiLoopConfig, ledger: &StateLedger, dry_run: bool) -> Result<String> {
    // Weekly proposal ceiling: approval fatigue defeats the human gate.
    if !dry_run
        && ledger.proposals_since(now_ms() - 7 * 24 * 3_600_000)? >= cfg.max_proposals_per_week
    {
        return Ok(format!(
            "proposal ceiling reached (≥ {}/week); proposer quiet until next week",
            cfg.max_proposals_per_week
        ));
    }

    // ---- Inputs, in the paper's order: index → skill-impact → corroborated patterns → skills.

    if !cfg.wiki_root.exists() {
        return Ok("nothing to propose: the wiki does not exist yet".into());
    }
    let index_md = std::fs::read_to_string(cfg.wiki_root.join("index.md")).unwrap_or_default();
    let impact_md =
        std::fs::read_to_string(cfg.wiki_root.join("skill-impact.md")).unwrap_or_default();

    let store = super::patterns::PatternStore::new(&cfg.wiki_root)?;
    let mut candidates: Vec<String> = Vec::new();
    let mut candidate_scopes: Vec<String> = Vec::new();
    for (_, (path, meta)) in store.catalog()? {
        if meta.status == "superseded" || meta.status == "quarantined" {
            continue;
        }
        let distinct: std::collections::HashSet<_> = meta
            .corroboration
            .iter()
            .map(|c| c.session_id.as_str())
            .collect();
        if distinct.len() < cfg.min_pattern_corroboration {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            candidates.push(content);
            candidate_scopes.push(meta.scope.clone());
        }
        if candidates.len() >= 10 {
            break;
        }
    }
    if candidates.is_empty() {
        return Ok(format!(
            "nothing to propose: no patterns with ≥{} corroborating sessions yet",
            cfg.min_pattern_corroboration
        ));
    }
    let mut pattern_block = candidates.join("\n\n---\n\n");
    if pattern_block.len() > 60 * 1024 {
        pattern_block.truncate(60 * 1024);
        pattern_block.push_str("\n... [patterns truncated]");
    }

    // ---- Scope for a new proposal: the unanimous scope of the motivating patterns.
    // Diverging evidence widens to `global` — which is exactly when the fail-closed
    // Tier-0/Tier-1 gates get to veto it; it must not silently widen.
    let scope = proposal_scope_for(&candidate_scopes);

    let skills = list_existing_skills(&cfg.skills_root)?;
    let skills_block = serde_json::to_string_pretty(&skills)?;

    let prompt = format!(
        "{}\n\n## 1. Wiki Index\n\n{}\n\n## 2. Skill-Impact Audit Trail\n\n{}\n\n## 3. Corroborated Pattern Pages\n\n{}\n\n## 4. Existing Active Skills\n\n{}",
        super::prompts::PROPOSER,
        index_md,
        impact_md,
        pattern_block,
        skills_block
    );

    if dry_run {
        return Ok(format!(
            "dry run: proposer prompt built ({} chars); {} corroborated pattern(s), {} active skill(s)",
            prompt.len(),
            candidates.len(),
            skills.len()
        ));
    }

    let schema = if cfg.proposer.json_schema {
        Some(cfg.schema_path("proposer", super::prompts::PROPOSER_SCHEMA)?)
    } else {
        None
    };
    let raw = super::harness::run_role(
        &cfg.proposer,
        &prompt,
        schema.as_deref(),
        std::time::Duration::from_secs(cfg.subprocess_timeout_secs),
    )?;
    let value = super::harness::extract_json_object(&raw)?;
    let out: ProposerOutput = serde_json::from_value(value).context("parsing proposer JSON")?;

    let Some(skill_name) = out.skill_name else {
        return Ok(format!(
            "proposer declined to propose: {}",
            if out.rationale.is_empty() {
                "no rationale given"
            } else {
                &out.rationale
            }
        ));
    };
    if !is_valid_slug(&skill_name) {
        bail!("proposer emitted invalid skill name `{skill_name}`");
    }
    if out.skill_markdown.trim().is_empty() {
        bail!("proposer emitted empty skill_markdown");
    }

    // ---- Stage the proposal directory.
    let id = format!(
        "prop_{:x}_{}",
        now_ms(),
        super::patterns::generate_pattern_id().trim_start_matches("pat_")
    );
    let dir = cfg.proposals_dir.join(&id);
    std::fs::create_dir_all(&dir)?;

    let existing_skill = read_existing_skill(&cfg.skills_root, &skill_name);
    let diff = unified_diff(
        existing_skill.as_deref().unwrap_or(""),
        &out.skill_markdown,
        &format!("a/{skill_name}/SKILL.md"),
        &format!("b/{skill_name}/SKILL.md"),
    );

    let mut proposal = Proposal {
        id: id.clone(),
        created_at: now_ms(),
        skill_name: skill_name.clone(),
        description: out.description.clone(),
        status: "pending".into(),
        scope: scope.clone(),
        purpose_patterns: out.purpose_patterns.clone(),
        rationale: out.rationale.clone(),
        gates: Default::default(),
    };
    proposal.save(&cfg.proposals_dir)?;
    std::fs::write(dir.join("SKILL.md"), &out.skill_markdown)?;
    std::fs::write(
        dir.join("PURPOSE.md"),
        format!(
            "# Purpose\n\nMaps skill `{skill_name}` to wiki patterns: {}\n\n{}\n",
            out.purpose_patterns.join(", "),
            out.rationale
        ),
    )?;
    std::fs::write(dir.join("skill.diff"), &diff)?;
    ledger.insert_proposal(&id, &skill_name, &out.purpose_patterns)?;

    // ---- Gate: Tier 0 always; Tier 1 counterfactual judge, fed the contributing
    // pattern scopes so global-scope proposals replay evidence from every project
    // that motivated them (and fail closed when none resolves).
    let mut contributing_scopes = candidate_scopes.clone();
    contributing_scopes.sort();
    contributing_scopes.dedup();

    let tier0_results = super::gates::tier0(
        cfg,
        ledger,
        &proposal,
        &out.skill_markdown,
        existing_skill.as_deref(),
        &contributing_scopes,
    )?;
    let mut all_passed = true;
    for (name, result) in &tier0_results {
        proposal.gates.insert(name.clone(), result.clone());
        all_passed &= result.passed;
    }
    if all_passed {
        match super::gates::tier1(cfg, &proposal, &out.skill_markdown, &contributing_scopes) {
            Ok(Some(result)) => {
                all_passed &= result.passed;
                proposal.gates.insert("tier1".into(), result);
            }
            Ok(None) => {}
            Err(e) => {
                proposal.gates.insert(
                    "tier1".into(),
                    GateResult {
                        passed: false,
                        detail: format!("judge errored: {e}"),
                    },
                );
                all_passed = false;
            }
        }
    }

    if all_passed {
        proposal.status = "validated".into();
    } else {
        proposal.status = "rejected".into();
    }
    proposal.save(&cfg.proposals_dir)?;
    ledger.set_proposal_status(
        &id,
        &proposal.status,
        &serde_json::to_string(&proposal.gates)?,
    )?;

    if proposal.status == "rejected" {
        super::gates::append_skill_impact(
            &cfg.wiki_root,
            &format!(
                "## {} — {} — {} (auto-rejected at gates)\n- decision: rejected\n- gates: {}\n- patterns: {}\n\n```diff\n{}\n```\n",
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                id,
                skill_name,
                summarize_gates(&proposal.gates),
                out.purpose_patterns.join(", "),
                diff
            ),
        )?;
    }

    let summary = summarize_gates(&proposal.gates);
    if cfg.notify_on_proposal && proposal.status == "validated" {
        let _ = super::notify::notify(
            "Wiki-Loop",
            Some(&format!("Skill proposal ready: {skill_name} ({id})")),
        );
    }
    Ok(format!(
        "proposal {id} for `{skill_name}`: {} ({summary})\n  review with: memex wiki-loop validate {id} && memex wiki-loop apply {id}",
        proposal.status
    ))
}

/// Scope for a proposal: the unanimous scope of its motivating patterns when they agree;
/// diverging evidence is cross-project by definition, so it widens to `global` — where
/// the strictest gates apply — rather than pinning to whichever project sorted first.
fn proposal_scope_for(candidate_scopes: &[String]) -> String {
    if let Some(first) = candidate_scopes.first()
        && candidate_scopes.iter().all(|s| s == first)
    {
        return first.clone();
    }
    "global".into()
}

fn summarize_gates(gates: &BTreeMap<String, GateResult>) -> String {
    gates
        .iter()
        .map(|(k, v)| format!("{k}={}", if v.passed { "pass" } else { "FAIL" }))
        .collect::<Vec<_>>()
        .join(" ")
}

fn list_existing_skills(skills_root: &Path) -> Result<Vec<ExistingSkillSummary>> {
    let mut out = Vec::new();
    if !skills_root.exists() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(skills_root)? {
        let path = entry?.path();
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&skill_md)?;
        let (name, description) = parse_skill_frontmatter(&content).unwrap_or_else(|| {
            (
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                String::new(),
            )
        });
        out.push(ExistingSkillSummary { name, description });
    }
    Ok(out)
}

fn read_existing_skill(skills_root: &Path, skill_name: &str) -> Option<String> {
    std::fs::read_to_string(skills_root.join(skill_name).join("SKILL.md")).ok()
}

/// Extract `name` / `description` from a SKILL.md frontmatter (single-level key: value).
fn parse_skill_frontmatter(content: &str) -> Option<(String, String)> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let mut name = None;
    let mut description = None;
    for line in rest[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "name" => name = Some(v.trim().to_string()),
                "description" => description = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    Some((name?, description.unwrap_or_default()))
}

/// Minimal unified diff over lines (LCS-based; skill files are small).
pub fn unified_diff(old: &str, new: &str, a: &str, b: &str) -> String {
    let old_lines: Vec<&str> = if old.is_empty() {
        Vec::new()
    } else {
        old.lines().collect()
    };
    let new_lines: Vec<&str> = new.lines().collect();
    let n = old_lines.len();
    let m = new_lines.len();
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old_lines[i] == new_lines[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out = format!("--- {a}\n+++ {b}\n");
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            out.push_str(&format!(" {}\n", old_lines[i]));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push_str(&format!("-{}\n", old_lines[i]));
            i += 1;
        } else {
            out.push_str(&format!("+{}\n", new_lines[j]));
            j += 1;
        }
    }
    while i < n {
        out.push_str(&format!("-{}\n", old_lines[i]));
        i += 1;
    }
    while j < m {
        out.push_str(&format!("+{}\n", new_lines[j]));
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_marks_add_remove_and_context() {
        let diff = unified_diff("a\nb\nc\n", "a\nB\nc\n", "a/f", "b/f");
        assert!(diff.starts_with("--- a/f\n+++ b/f\n"));
        assert!(diff.contains("-b\n"));
        assert!(diff.contains("+B\n"));
        assert!(diff.contains(" a\n"));

        let pure_add = unified_diff("", "x\ny\n", "a", "b");
        let added: Vec<&str> = pure_add
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .collect();
        assert_eq!(added, vec!["+x", "+y"]);
    }

    #[test]
    fn frontmatter_extraction() {
        let (name, desc) =
            parse_skill_frontmatter("---\nname: my-skill\ndescription: Does things.\n---\n\nbody")
                .expect("fm");
        assert_eq!(name, "my-skill");
        assert_eq!(desc, "Does things.");
    }

    #[test]
    fn scope_selection() {
        // Unanimous patterns keep their scope, project or global.
        assert_eq!(
            proposal_scope_for(&["project:memex".into(), "project:memex".into()]),
            "project:memex"
        );
        assert_eq!(proposal_scope_for(&["global".into()]), "global");
        // Diverging evidence is cross-project by definition: widen to global, where the
        // strictest gates apply, instead of pinning whichever project sorted first.
        assert_eq!(
            proposal_scope_for(&["project:memex".into(), "global".into()]),
            "global"
        );
        assert_eq!(
            proposal_scope_for(&["project:a".into(), "project:b".into()]),
            "global"
        );
        assert_eq!(proposal_scope_for(&[]), "global");
    }
}
