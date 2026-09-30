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
    content: String,
}

pub fn run_proposer(cfg: &WikiLoopConfig, ledger: &StateLedger, dry_run: bool) -> Result<String> {
    // Weekly proposal ceiling: approval fatigue defeats the human gate. Only
    // reviewable proposals (not gate-rejected ones) spend it — see the ledger.
    if !dry_run
        && ledger.reviewable_proposals_since(now_ms() - 7 * 24 * 3_600_000)?
            >= cfg.max_proposals_per_week
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
    let mut candidates: Vec<(String, String)> = Vec::new();
    let catalog = store.catalog()?;
    let catalog_scopes: BTreeMap<String, String> = catalog
        .iter()
        .map(|(id, (_, meta))| (id.clone(), meta.scope.clone()))
        .collect();
    for (id, (path, meta)) in &catalog {
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
        if let Ok(content) = std::fs::read_to_string(path) {
            candidates.push((id.clone(), content));
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
    let pattern_block = candidates
        .iter()
        .map(|(id, body)| format!("## Pattern {id}\n\n{body}"))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    let pattern_block = truncate_utf8(&pattern_block, 60 * 1024);

    let skills = list_existing_skills(&cfg.skills_root)?;
    let skills_block = serde_json::to_string_pretty(&skills)?;

    let prompt = build_proposer_prompt(&index_md, &impact_md, &pattern_block, &skills_block);

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
    let value = super::harness::run_role_structured(
        &cfg.proposer,
        &prompt,
        schema.as_deref(),
        std::time::Duration::from_secs(cfg.subprocess_timeout_secs),
    )?;
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

    // Scope gates to the evidence this proposal names, not every pattern shown in
    // the prompt. Unrelated projects must not influence its counterfactual judge.
    let Some(contributing_scopes) = motivating_scopes(&out.purpose_patterns, &catalog_scopes)
    else {
        bail!("proposer must cite existing pattern IDs for its motivating evidence");
    };
    let scope = proposal_scope_for(&contributing_scopes);

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

fn build_proposer_prompt(index: &str, impact: &str, patterns: &str, skills: &str) -> String {
    format!(
        "{}\n\nFor an existing active skill, prefer an incremental patch to that skill over creating a duplicate. Emit the complete resulting SKILL.md for the single target skill. Use the full skill-impact audit below to avoid repeating rejected or reverted interventions.\n\n## 1. Wiki Index\n\n{}\n\n## 2. Skill-Impact Audit Trail\n\n{}\n\n## 3. Corroborated Pattern Pages\n\n{}\n\n## 4. Existing Active Skills\n\n{}",
        super::prompts::PROPOSER,
        index,
        impact,
        patterns,
        skills
    )
}

fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let mut end = max_bytes;
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... [patterns truncated]", &input[..end])
}

fn motivating_scopes(
    pattern_ids: &[String],
    available: &BTreeMap<String, String>,
) -> Option<Vec<String>> {
    if pattern_ids.is_empty() || pattern_ids.iter().any(|id| !available.contains_key(id)) {
        return None;
    }
    let mut scopes: Vec<String> = pattern_ids.iter().map(|id| available[id].clone()).collect();
    scopes.sort();
    scopes.dedup();
    Some(scopes)
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
        out.push(ExistingSkillSummary {
            name,
            description,
            content,
        });
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

    #[test]
    fn pattern_truncation_preserves_utf8_at_boundary() {
        let max_bytes = 60 * 1024;
        let input = format!("{}日", "a".repeat(max_bytes - 1));
        let truncated = truncate_utf8(&input, max_bytes);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(truncated.starts_with(&"a".repeat(max_bytes - 1)));
        assert!(truncated.ends_with("... [patterns truncated]"));
    }

    #[test]
    fn proposal_scopes_follow_only_named_motivating_patterns() {
        let available = BTreeMap::from([
            ("pat_a".to_string(), "project:a".to_string()),
            ("pat_b".to_string(), "project:b".to_string()),
            ("pat_unselected".to_string(), "project:c".to_string()),
        ]);
        let scopes = motivating_scopes(&["pat_a".into(), "pat_a".into()], &available)
            .expect("known motivating pattern");
        assert_eq!(scopes, vec!["project:a"]);

        let mixed = motivating_scopes(&["pat_a".into(), "pat_b".into()], &available)
            .expect("known motivating patterns");
        assert_eq!(proposal_scope_for(&mixed), "global");
        assert!(motivating_scopes(&[], &available).is_none());
        assert!(motivating_scopes(&["pat_a".into(), "missing".into()], &available).is_none());
        // Valid catalog entries remain valid even if they were outside the 10 prompt pages.
        assert!(motivating_scopes(&["pat_unselected".into()], &available).is_some());
    }

    #[test]
    fn proposer_prompt_keeps_complete_audit_tail_and_all_sections_ordered() {
        let audit = format!("historical-entry\n{}\nlatest-entry", "x".repeat(70 * 1024));
        let prompt = build_proposer_prompt("index", &audit, "patterns", "skills");
        let index_at = prompt.find("## 1. Wiki Index").unwrap();
        let audit_at = prompt.find("## 2. Skill-Impact Audit Trail").unwrap();
        let patterns_at = prompt.find("## 3. Corroborated Pattern Pages").unwrap();
        let skills_at = prompt.find("## 4. Existing Active Skills").unwrap();
        assert!(index_at < audit_at && audit_at < patterns_at && patterns_at < skills_at);
        assert!(prompt.contains(&audit));
        assert!(prompt.ends_with("skills"));
        assert!(prompt.contains("prefer an incremental patch"));
    }
}
