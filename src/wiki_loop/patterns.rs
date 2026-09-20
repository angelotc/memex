//! Wiki layer (paper layer 2): pattern pages with frontmatter, **patch-based** merges
//! that preserve accumulated evidence, `index.md` catalog, and the `logs.md` evolution
//! journal. The wiki is never rolled back: edits only add, merge, or supersede.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Corroboration {
    pub source: String,
    pub session_id: String,
    pub ts: i64,
}

/// One maintainer operation on a pattern (from the validated maintainer JSON).
#[derive(Debug, Clone, Deserialize)]
pub struct PatternOp {
    /// "create" | "merge" | "supersede"
    pub action: String,
    /// "failure" (default) | "success": failure modes from error traces, or
    /// strategies extracted from passing traces.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub existing_id: Option<String>,
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub symptom: String,
    #[serde(default)]
    pub root_cause: String,
    #[serde(default)]
    pub fix: String,
    #[serde(default)]
    pub evidence_session_ids: Vec<String>,
    #[serde(default)]
    pub evidence_summary: String,
}

/// Frontmatter of an existing pattern page.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PatternMeta {
    pub id: String,
    pub title: String,
    pub status: String,
    /// "failure" | "success"; pages written before the field existed parse as "failure".
    pub kind: String,
    pub scope: String,
    pub created: String,
    pub updated: String,
    #[serde(skip)]
    pub corroboration: Vec<Corroboration>,
    pub superseded_by: Option<String>,
    pub scrub_pass: String,
    #[serde(skip)]
    pub file: String,
}

pub struct PatternWrite {
    pub file: PathBuf,
    pub action: String,
    pub pattern_id: String,
}

pub struct PatternStore {
    wiki_dir: PathBuf,
}

impl PatternStore {
    pub fn new(wiki_dir: &Path) -> Result<Self> {
        let store = Self {
            wiki_dir: wiki_dir.to_path_buf(),
        };
        std::fs::create_dir_all(store.patterns_dir())?;
        std::fs::create_dir_all(store.quarantine_dir())?;
        Ok(store)
    }

    pub fn patterns_dir(&self) -> PathBuf {
        self.wiki_dir.join("patterns")
    }

    pub fn quarantine_dir(&self) -> PathBuf {
        self.wiki_dir.join("quarantine")
    }

    // ---- frontmatter (hand-rolled key: value + corroboration list, memory.rs style) ----

    pub fn parse_frontmatter(content: &str) -> Option<(PatternMeta, String)> {
        let rest = content.strip_prefix("---\n")?;
        let end = rest.find("\n---")?;
        let fm_text = &rest[..end];
        let body = rest[end + 4..]
            .strip_prefix('\n')
            .unwrap_or(&rest[end + 4..]);

        let mut meta = PatternMeta {
            status: "candidate".into(),
            kind: "failure".into(),
            scope: "global".into(),
            scrub_pass: "v1".into(),
            ..Default::default()
        };
        let mut in_corroboration = false;
        let mut current: Option<Corroboration> = None;
        let mut corroboration: Vec<Corroboration> = Vec::new();

        for line in fm_text.lines() {
            if line.starts_with("  - ") {
                if let Some(c) = current.take() {
                    corroboration.push(c);
                }
                let fields = line.trim_start_matches("  - ");
                let mut c = Corroboration {
                    source: String::new(),
                    session_id: String::new(),
                    ts: 0,
                };
                for part in fields.split(", ") {
                    let Some((k, v)) = part.split_once(": ") else {
                        continue;
                    };
                    match k {
                        "source" => c.source = v.to_string(),
                        "session_id" => c.session_id = v.to_string(),
                        "ts" => c.ts = v.parse().unwrap_or(0),
                        _ => {}
                    }
                }
                current = Some(c);
                in_corroboration = true;
                continue;
            }
            if line.starts_with("    ") && in_corroboration {
                let Some((k, v)) = line.trim().split_once(": ") else {
                    continue;
                };
                if let Some(c) = current.as_mut() {
                    match k {
                        "source" => c.source = v.to_string(),
                        "session_id" => c.session_id = v.to_string(),
                        "ts" => c.ts = v.parse().unwrap_or(0),
                        _ => {}
                    }
                }
                continue;
            }
            if let Some(c) = current.take() {
                corroboration.push(c);
            }
            in_corroboration = false;
            // Tolerate lines without a `key: value` shape (the empty-corroboration
            // marker, stray blanks) instead of discarding the whole page.
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let v = v.trim().trim_matches('"');
            match k.trim() {
                "id" => meta.id = v.to_string(),
                "title" => meta.title = v.to_string(),
                "status" => meta.status = v.to_string(),
                "kind" => meta.kind = normalize_kind(v).to_string(),
                "scope" => meta.scope = v.to_string(),
                "created" => meta.created = v.to_string(),
                "updated" => meta.updated = v.to_string(),
                "superseded_by" => {
                    meta.superseded_by = (!v.is_empty() && v != "null").then(|| v.to_string())
                }
                "scrub_pass" => meta.scrub_pass = v.to_string(),
                _ => {}
            }
        }
        if let Some(c) = current.take() {
            corroboration.push(c);
        }
        meta.corroboration = corroboration;
        meta.file = String::new();
        Some((meta, body.to_string()))
    }

    fn render(
        meta: &PatternMeta,
        symptom: &str,
        root_cause: &str,
        fix: &str,
        evidence_summary: &str,
    ) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("id: {}\n", meta.id));
        // Free text is spliced into hand-rolled frontmatter, so it can never be
        // allowed to start a new line (quote-escaping alone does not prevent that).
        out.push_str(&format!(
            "title: \"{}\"\n",
            collapse_newlines(&meta.title).replace('"', "'")
        ));
        out.push_str(&format!("status: {}\n", meta.status));
        out.push_str(&format!("kind: {}\n", meta.kind));
        out.push_str(&format!("scope: {}\n", meta.scope));
        out.push_str(&format!("created: {}\n", meta.created));
        out.push_str(&format!("updated: {}\n", meta.updated));
        out.push_str("corroboration:\n");
        if meta.corroboration.is_empty() {
            out.push_str("  []\n");
        } else {
            for c in &meta.corroboration {
                out.push_str(&format!(
                    "  - source: {}, session_id: {}, ts: {}\n",
                    c.source, c.session_id, c.ts
                ));
            }
        }
        let superseded_by = meta
            .superseded_by
            .as_deref()
            .map(first_line)
            .unwrap_or_else(|| "null".into());
        out.push_str(&format!(
            "superseded_by: {superseded_by}\nscrub_pass: {}\n---\n",
            meta.scrub_pass
        ));
        out.push_str(&format!(
            "\n## Symptom\n{symptom}\n\n## Root Cause\n{root_cause}\n\n## Fix\n{fix}\n\n## Evidence\n{evidence_summary}\n"
        ));
        out
    }

    fn write_atomic(&self, path: &Path, content: &str) -> Result<()> {
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Map pattern id -> (path, meta) for all active patterns.
    pub fn catalog(&self) -> Result<BTreeMap<String, (PathBuf, PatternMeta)>> {
        let mut map = BTreeMap::new();
        let entries = std::fs::read_dir(self.patterns_dir())
            .with_context(|| format!("reading {}", self.patterns_dir().display()))?;
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let content = std::fs::read_to_string(&path)?;
            if let Some((mut meta, _)) = Self::parse_frontmatter(&content) {
                meta.file = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                map.insert(meta.id.clone(), (path, meta));
            }
        }
        Ok(map)
    }

    /// Apply one maintainer operation. This is the **only** code path that edits the wiki:
    /// creates append new pages; merges patch existing pages preserving corroboration,
    /// created timestamps, and evidence history; supersede flips status without deletion.
    pub fn apply_op(
        &self,
        op: &PatternOp,
        scrubbed: &PatternOp,
        evidence: &[Corroboration],
    ) -> Result<PatternWrite> {
        let Some(slug) = normalize_slug(&op.slug) else {
            bail!("pattern slug `{}` normalizes to nothing", op.slug);
        };
        // Downstream paths are built from the scrubbed copy; pin it to the normalized
        // slug so a divergence between the two can never reach the filesystem.
        let mut scrubbed = scrubbed.clone();
        scrubbed.slug = slug.clone();
        let mut op = op.clone();
        op.slug = slug;
        match op.action.as_str() {
            "create" => self.create(&op, &scrubbed, evidence),
            "merge" => self.merge(&op, &scrubbed, evidence),
            "supersede" => self.supersede(&op, &scrubbed, evidence),
            other => bail!("unknown pattern action `{other}`"),
        }
    }

    fn create(
        &self,
        op: &PatternOp,
        scrubbed: &PatternOp,
        evidence: &[Corroboration],
    ) -> Result<PatternWrite> {
        // A create aimed at a slug that already has a page would clobber its
        // accumulated corroboration with a brand-new id; promote it to a merge
        // against the existing page instead (forks union, they never reset).
        let target = self.patterns_dir().join(format!("{}.md", scrubbed.slug));
        if target.exists() {
            let content = std::fs::read_to_string(&target)?;
            let Some((existing, _)) = Self::parse_frontmatter(&content) else {
                bail!(
                    "`{}` already exists but its frontmatter is unparseable; not overwriting",
                    target.display()
                );
            };
            let mut promoted = op.clone();
            promoted.existing_id = Some(existing.id);
            return self.merge(&promoted, scrubbed, evidence);
        }
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let id = generate_pattern_id();
        let meta = PatternMeta {
            id: id.clone(),
            title: scrubbed.title.clone(),
            status: "candidate".into(),
            kind: normalize_kind(&scrubbed.kind).into(),
            scope: sanitize_scope(scrubbed.scope.as_deref().unwrap_or("global"))?,
            created: now.clone(),
            updated: now,
            corroboration: evidence.to_vec(),
            superseded_by: None,
            scrub_pass: "v1".into(),
            file: String::new(),
        };
        let content = Self::render(
            &meta,
            &scrubbed.symptom,
            &scrubbed.root_cause,
            &scrubbed.fix,
            &evidence_summary(scrubbed),
        );
        let path = self.patterns_dir().join(format!("{}.md", scrubbed.slug));
        self.write_atomic(&path, &content)?;
        Ok(PatternWrite {
            file: path,
            action: "created".into(),
            pattern_id: id,
        })
    }

    fn merge(
        &self,
        op: &PatternOp,
        scrubbed: &PatternOp,
        evidence: &[Corroboration],
    ) -> Result<PatternWrite> {
        let Some(existing_id) = op.existing_id.as_deref() else {
            bail!("merge requires existing_id");
        };
        let catalog = self.catalog()?;
        let Some((path, existing)) = catalog.get(existing_id) else {
            bail!("merge target `{existing_id}` not found in catalog");
        };
        let path = path.clone();
        let existing = existing.clone();

        // Patch semantics: union corroboration, keep the original created timestamp and
        // identity, refresh the analysis sections, and *append* to the evidence history.
        // An empty incoming section preserves the page's current text instead of
        // blanking it: the maintainer refines sections, and must be able to leave
        // one alone.
        let mut meta = existing.clone();
        meta.title = scrubbed.title.clone();
        meta.updated = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        if scrubbed.scope.is_some() {
            meta.scope = sanitize_scope(scrubbed.scope.as_deref().unwrap())?;
        }
        for c in evidence {
            if !meta
                .corroboration
                .iter()
                .any(|e| e.source == c.source && e.session_id == c.session_id)
            {
                meta.corroboration.push(c.clone());
            }
        }

        let existing_content = std::fs::read_to_string(&path)?;
        let prior_symptom = existing_section(&existing_content, "Symptom");
        let prior_root_cause = existing_section(&existing_content, "Root Cause");
        let prior_fix = existing_section(&existing_content, "Fix");
        let prior_evidence = existing_section(&existing_content, "Evidence");
        let mut summary = prior_evidence;
        if !summary.is_empty() {
            summary.push('\n');
        }
        summary.push_str(&evidence_summary(scrubbed));

        let content = Self::render(
            &meta,
            section_text(&scrubbed.symptom, &prior_symptom),
            section_text(&scrubbed.root_cause, &prior_root_cause),
            section_text(&scrubbed.fix, &prior_fix),
            &summary,
        );
        self.write_atomic(&path, &content)?;
        Ok(PatternWrite {
            file: path,
            action: "merged".into(),
            pattern_id: meta.id,
        })
    }

    fn supersede(
        &self,
        op: &PatternOp,
        scrubbed: &PatternOp,
        _evidence: &[Corroboration],
    ) -> Result<PatternWrite> {
        let Some(existing_id) = op.existing_id.as_deref() else {
            bail!("supersede requires existing_id");
        };
        let catalog = self.catalog()?;
        let Some((path, existing)) = catalog.get(existing_id) else {
            bail!("supersede target `{existing_id}` not found in catalog");
        };
        let path = path.clone();
        let mut meta = existing.clone();
        meta.status = "superseded".into();
        // Point at the replacement pattern (by slug→id if it exists, else the raw slug).
        let replacement_id = self
            .catalog()?
            .values()
            .find(|(_, m)| m.file == format!("{}.md", scrubbed.slug))
            .map(|(_, m)| m.id.clone())
            .unwrap_or_else(|| scrubbed.slug.clone());
        meta.superseded_by = Some(replacement_id);
        meta.updated = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let content = Self::render(
            &meta,
            &scrubbed.symptom,
            &scrubbed.root_cause,
            &scrubbed.fix,
            "",
        );
        self.write_atomic(&path, &content)?;
        Ok(PatternWrite {
            file: path,
            action: "superseded".into(),
            pattern_id: meta.id,
        })
    }

    /// Quarantine holds a suspect pattern outside the catalog; it is never indexed or merged.
    pub fn quarantine(&self, scrubbed: &PatternOp, evidence: &[Corroboration]) -> Result<PathBuf> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let meta = PatternMeta {
            id: generate_pattern_id(),
            title: scrubbed.title.clone(),
            status: "quarantined".into(),
            kind: normalize_kind(&scrubbed.kind).into(),
            scope: sanitize_scope(scrubbed.scope.as_deref().unwrap_or("global"))
                .unwrap_or_else(|_| "global".into()),
            created: now.clone(),
            updated: now,
            corroboration: evidence.to_vec(),
            superseded_by: None,
            scrub_pass: "v1".into(),
            file: String::new(),
        };
        let content = Self::render(
            &meta,
            &scrubbed.symptom,
            &scrubbed.root_cause,
            &scrubbed.fix,
            &evidence_summary(scrubbed),
        );
        // Never overwrite an earlier quarantine of the same slug: side-step to a
        // 4-hex-char suffix so both snapshots survive for inspection.
        let mut file_name = format!("{}.md", scrubbed.slug);
        if self.quarantine_dir().join(&file_name).exists() {
            file_name = format!("{}-{}.md", scrubbed.slug, hex(&rand_bytes()[..2]));
        }
        let path = self.quarantine_dir().join(file_name);
        self.write_atomic(&path, &content)?;
        Ok(path)
    }

    /// Render `index.md` (paper: the Maintainer revises the wiki index each iteration).
    pub fn update_index(&self) -> Result<()> {
        let mut lines = vec![
            "# Wiki Pattern Catalog".to_string(),
            String::new(),
            "| ID | Title | Kind | Scope | Status | File |".to_string(),
            "|---|---|---|---|---|---|".to_string(),
        ];
        for (_, (path, meta)) in self.catalog()? {
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let link = format!("patterns/{file}");
            lines.push(format!(
                "| `{}` | {} | {} | `{}` | {} | [{}]({link}) |",
                meta.id, meta.title, meta.kind, meta.scope, meta.status, file
            ));
        }
        let path = self.wiki_dir.join("index.md");
        self.write_atomic(&path, &(lines.join("\n") + "\n"))
    }

    /// Append a run entry to `logs.md` (paper: the evolution journal, one entry per
    /// maintainer iteration; written programmatically by the orchestrator).
    pub fn append_log(&self, entry: &str) -> Result<()> {
        use std::io::Write;
        let path = self.wiki_dir.join("logs.md");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{entry}")?;
        Ok(())
    }
}

fn evidence_summary(op: &PatternOp) -> String {
    if op.evidence_summary.is_empty() {
        format!(
            "Corroborated across sessions: {}",
            op.evidence_session_ids.join(", ")
        )
    } else {
        op.evidence_summary.clone()
    }
}

/// Body of a `## <name>` section from a pattern page (used to preserve section
/// text when an op leaves the incoming field empty).
fn existing_section(content: &str, name: &str) -> String {
    let header = format!("## {name}");
    let mut out = String::new();
    let mut in_section = false;
    for line in content.lines() {
        if line.starts_with("## ") {
            in_section = line.trim() == header;
            if in_section {
                continue;
            }
        }
        if in_section {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

/// Empty or whitespace-only incoming text keeps the page's current section
/// instead of blanking it.
fn section_text<'a>(incoming: &'a str, existing: &'a str) -> &'a str {
    if incoming.trim().is_empty() {
        existing
    } else {
        incoming
    }
}

/// Collapse any run of CR/LF into a single space; a newline inside a frontmatter
/// value would let free text forge additional frontmatter keys.
fn collapse_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c == '\r' || c == '\n' {
            pending_space = true;
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

/// Keep only the first line of a value destined for a single frontmatter line.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

/// Only "success" means success; every other value (including the serde-default
/// empty string) is a failure pattern.
fn normalize_kind(kind: &str) -> &'static str {
    if kind.trim() == "success" {
        "success"
    } else {
        "failure"
    }
}

/// Scope must be exactly `global` or `project:<slug>`; anything else (including
/// embedded newlines or extra frontmatter keys) is rejected.
pub fn sanitize_scope(scope: &str) -> Result<String> {
    let scope = scope.trim();
    if scope == "global" {
        return Ok(scope.to_string());
    }
    if let Some(project) = scope.strip_prefix("project:")
        && is_valid_slug(project)
    {
        return Ok(scope.to_string());
    }
    bail!("invalid pattern scope `{scope}` (expected `global` or `project:<slug>`)");
}

pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 80
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
}

/// Sanitize a model-emitted slug into a valid one: lowercase, dashes for anything
/// else, collapsed and trimmed, truncated to the 80-char cap on a dash boundary.
/// Over-long descriptive slugs are the normal model failure (verbosity, not malice)
/// — truncating keeps the pattern instead of dropping the op. None when nothing
/// usable remains.
pub fn normalize_slug(raw: &str) -> Option<String> {
    let mut slug: String = raw
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    if slug.len() > 80 {
        slug.truncate(80);
        // Cut on a word boundary and never end on a dangling dash.
        if let Some(cut) = slug.rfind('-') {
            slug.truncate(cut);
        }
    }
    let slug = slug.trim_matches('-').to_string();
    (!slug.is_empty()).then_some(slug)
}

pub fn generate_pattern_id() -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let rand: [u8; 5] = rand_bytes();
    format!("pat_{ts:x}{}", hex(&rand))
}

fn rand_bytes() -> [u8; 5] {
    let mut buf = [0u8; 5];
    if getrandom::fill(&mut buf).is_err() {
        // Fallback entropy: pid + subsec nanos mixed per byte. IDs only need uniqueness
        // within a host, and the millisecond timestamp prefix carries most of it.
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (pid >> i) as u8 ^ (nanos >> (i * 5)) as u8;
        }
    }
    buf
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Convenience: scrub all free-text fields of a PatternOp (orchestrator-owned redaction).
pub fn scrub_op(op: &PatternOp) -> (PatternOp, bool) {
    let mut out = op.clone();
    let mut quarantined = false;
    for (field, res) in [
        (&mut out.symptom, super::scrub::scrub_text(&op.symptom)),
        (
            &mut out.root_cause,
            super::scrub::scrub_text(&op.root_cause),
        ),
        (&mut out.fix, super::scrub::scrub_text(&op.fix)),
        (
            &mut out.evidence_summary,
            super::scrub::scrub_text(&op.evidence_summary),
        ),
        (&mut out.title, super::scrub::scrub_text(&op.title)),
    ] {
        *field = res.text;
        quarantined |= res.quarantined;
    }
    (out, quarantined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_slug_cleans_and_truncates() {
        // The live failure mode: a descriptive slug past the 80-char cap.
        let long =
            "gsc-api-query-fails-with-403-forbidden-on-domain-properties-without-sc-domain-prefix";
        let normalized = normalize_slug(long).expect("normalizes");
        assert!(is_valid_slug(&normalized), "{normalized}");
        // Truncation lands on a word boundary, not mid-word or on a dangling dash.
        assert!(normalized.len() <= 80);
        assert!(normalized.starts_with("gsc-api-query-fails-with-403-forbidden"));

        // Dirty input: case, separators, runs, surrounding dashes.
        assert_eq!(
            normalize_slug("  Way Too--Messy_SLUG!! "),
            Some("way-too-messy-slug".into())
        );
        // Already-valid slugs pass through untouched.
        assert_eq!(
            normalize_slug("wrangler-erofs-log-failure").as_deref(),
            Some("wrangler-erofs-log-failure")
        );
        // Nothing usable remains.
        assert_eq!(normalize_slug("---___***---"), None);
        assert_eq!(normalize_slug(""), None);
    }

    fn op(action: &str, slug: &str, sessions: &[&str]) -> PatternOp {
        PatternOp {
            action: action.into(),
            kind: "failure".into(),
            existing_id: None,
            slug: slug.into(),
            title: "SUUMO image URLs need host-aware width".into(),
            scope: Some("project:nipponhomes".into()),
            symptom: "Images 400".into(),
            root_cause: "Width param not accepted by yimg host".into(),
            fix: "Use w=1000 only on SUUMO host".into(),
            evidence_session_ids: sessions.iter().map(|s| s.to_string()).collect(),
            evidence_summary: String::new(),
        }
    }

    fn corroboration(sessions: &[&str]) -> Vec<Corroboration> {
        sessions
            .iter()
            .enumerate()
            .map(|(i, s)| Corroboration {
                source: "claude".into(),
                session_id: s.to_string(),
                ts: 1000 + i as i64,
            })
            .collect()
    }

    #[test]
    fn create_then_merge_preserves_and_unions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");

        let write = store
            .apply_op(
                &op("create", "suumo-image-width", &["s1"]),
                &op("create", "suumo-image-width", &["s1"]),
                &corroboration(&["s1"]),
            )
            .expect("create");
        assert_eq!(write.action, "created");

        let (path, meta) = store
            .catalog()
            .expect("catalog")
            .values()
            .next()
            .expect("one pattern")
            .clone();
        assert_eq!(meta.corroboration.len(), 1);
        let created_ts = meta.created.clone();

        // Second run merges new evidence into the existing pattern (same id).
        let mut merge_op = op("merge", "suumo-image-width", &["s2"]);
        merge_op.existing_id = Some(write.pattern_id.clone());
        merge_op.fix = "Use w=1000 on SUUMO; yimg rejects width entirely".into();
        let write2 = store
            .apply_op(&merge_op, &merge_op, &corroboration(&["s2"]))
            .expect("merge");
        assert_eq!(write2.action, "merged");
        assert_eq!(
            write2.file, path,
            "merge must patch the existing file in place"
        );

        let content = std::fs::read_to_string(&path).expect("read");
        let (merged, _) = PatternStore::parse_frontmatter(&content).expect("frontmatter");
        assert_eq!(merged.created, created_ts, "created must survive merge");
        let ids: Vec<&str> = merged
            .corroboration
            .iter()
            .map(|c| c.session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["s1", "s2"], "corroboration must union, not reset");
        assert!(content.contains("w=1000"), "fix section must be refreshed");
        assert!(
            content.contains("Corroborated across sessions: s1"),
            "prior evidence must survive"
        );
        assert!(
            content.contains("Corroborated across sessions: s2"),
            "new evidence must be appended"
        );
    }

    #[test]
    fn create_for_existing_slug_promotes_to_merge() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");

        let write = store
            .apply_op(
                &op("create", "slug-a", &["s1"]),
                &op("create", "slug-a", &["s1"]),
                &corroboration(&["s1"]),
            )
            .expect("create");
        assert_eq!(write.action, "created");

        let (path, meta) = store
            .catalog()
            .expect("catalog")
            .values()
            .next()
            .expect("one pattern")
            .clone();
        let created_ts = meta.created.clone();

        // The maintainer forks: `create` for a slug that already has a page must be
        // promoted to a merge, not replace the page under a brand-new id.
        let write2 = store
            .apply_op(
                &op("create", "slug-a", &["s2"]),
                &op("create", "slug-a", &["s2"]),
                &corroboration(&["s2"]),
            )
            .expect("promoted merge");
        assert_eq!(write2.action, "merged");
        assert_eq!(
            write2.pattern_id, write.pattern_id,
            "the fork must keep the existing pattern id"
        );
        assert_eq!(
            write2.file, path,
            "the fork patches the existing file in place"
        );

        let content = std::fs::read_to_string(&path).expect("read");
        let (merged, _) = PatternStore::parse_frontmatter(&content).expect("frontmatter");
        assert_eq!(merged.created, created_ts, "created must survive the fork");
        let ids: Vec<&str> = merged
            .corroboration
            .iter()
            .map(|c| c.session_id.as_str())
            .collect();
        assert_eq!(ids, vec!["s1", "s2"], "the fork must union corroboration");
        assert_eq!(store.catalog().expect("catalog").len(), 1);
    }

    #[test]
    fn merge_with_empty_sections_preserves_analysis() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");
        let write = store
            .apply_op(
                &op("create", "blank-guard", &["s1"]),
                &op("create", "blank-guard", &["s1"]),
                &corroboration(&["s1"]),
            )
            .expect("create");

        // An op that omits a section (serde default "") must not blank the page.
        let mut merge_op = op("merge", "blank-guard", &["s2"]);
        merge_op.existing_id = Some(write.pattern_id.clone());
        merge_op.symptom = String::new();
        merge_op.root_cause = "   ".into();
        merge_op.fix = String::new();
        store
            .apply_op(&merge_op, &merge_op, &corroboration(&["s2"]))
            .expect("merge");

        let content =
            std::fs::read_to_string(store.patterns_dir().join("blank-guard.md")).expect("read");
        assert!(
            content.contains("## Symptom\nImages 400"),
            "empty symptom must preserve the existing section"
        );
        assert!(
            content.contains("## Root Cause\nWidth param not accepted"),
            "whitespace-only root_cause must preserve the existing section"
        );
        assert!(
            content.contains("## Fix\nUse w=1000 only on SUUMO host"),
            "empty fix must preserve the existing section"
        );
    }

    #[test]
    fn frontmatter_values_stay_single_line() {
        let meta = PatternMeta {
            title: "line one\nline two\r\nwith key: forged".into(),
            superseded_by: Some("pat_1\nscrub_pass: v999".into()),
            ..Default::default()
        };
        let content = PatternStore::render(&meta, "s", "rc", "f", "e");
        assert!(
            content.contains("title: \"line one line two with key: forged\"\n"),
            "CR/LF in titles must collapse to single spaces"
        );
        assert!(
            content.contains("superseded_by: pat_1\n"),
            "superseded_by must keep only its first line"
        );
        assert!(
            !content.contains("scrub_pass: v999"),
            "no forged frontmatter keys may survive rendering"
        );
    }

    #[test]
    fn sanitize_scope_accepts_only_global_or_project_slug() {
        assert_eq!(sanitize_scope("global").expect("global"), "global");
        assert_eq!(
            sanitize_scope("project:nipponhomes").expect("project"),
            "project:nipponhomes"
        );
        assert!(sanitize_scope("global\nscrub_pass: v9").is_err());
        assert!(sanitize_scope("project:Not A Slug").is_err());
        assert!(sanitize_scope("").is_err());
        assert!(sanitize_scope("team:memex").is_err());

        // At the op level an invalid scope bails instead of writing the page.
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");
        let mut bad = op("create", "scope-guard", &["s1"]);
        bad.scope = Some("global\ninjected: yes".into());
        let err = store
            .apply_op(&bad, &bad, &corroboration(&["s1"]))
            .err()
            .expect("invalid scope must bail");
        assert!(
            err.to_string().contains("invalid pattern scope"),
            "unexpected error: {err:#}"
        );
        assert!(!store.patterns_dir().join("scope-guard.md").exists());
    }

    #[test]
    fn success_kind_round_trips_through_page_and_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");

        let mut create = op("create", "parallel-cargo-jobs", &["s1"]);
        create.kind = "success".into();
        store
            .apply_op(&create, &create, &corroboration(&["s1"]))
            .expect("create");

        let content = std::fs::read_to_string(store.patterns_dir().join("parallel-cargo-jobs.md"))
            .expect("read");
        assert!(content.contains("kind: success"));
        let (meta, _) = PatternStore::parse_frontmatter(&content).expect("parse");
        assert_eq!(meta.kind, "success");

        // Anything that is not exactly "success" is a failure pattern.
        let mut weird = op("create", "weird-kind", &["s1"]);
        weird.kind = "banana".into();
        store
            .apply_op(&weird, &weird, &corroboration(&["s1"]))
            .expect("create");
        let raw =
            std::fs::read_to_string(store.patterns_dir().join("weird-kind.md")).expect("read");
        assert!(
            raw.contains("kind: failure"),
            "unknown kind must fall back to failure"
        );

        store.update_index().expect("index");
        let index = std::fs::read_to_string(tmp.path().join("index.md")).expect("index");
        assert!(index.contains("| Kind |"), "index gains a Kind column");
        assert!(index.contains("| success |"));
        assert!(index.contains("| failure |"));
    }

    #[test]
    fn old_page_without_kind_parses_as_failure() {
        let old_page = "---\nid: pat_old\ntitle: \"Legacy\"\nstatus: candidate\nscope: global\n\
                        created: t1\nupdated: t1\ncorroboration:\n  []\nsuperseded_by: null\n\
                        scrub_pass: v1\n---\n\n## Symptom\nold\n";
        let (meta, _) = PatternStore::parse_frontmatter(old_page).expect("parse");
        assert_eq!(
            meta.kind, "failure",
            "pages without kind default to failure"
        );
    }

    #[test]
    fn quarantine_does_not_overwrite_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");
        let first = store
            .quarantine(&op("create", "leaky", &["s1"]), &corroboration(&["s1"]))
            .expect("first quarantine");
        let second = store
            .quarantine(&op("create", "leaky", &["s2"]), &corroboration(&["s2"]))
            .expect("second quarantine");

        assert_ne!(first, second, "same slug must not overwrite in quarantine");
        assert!(first.exists() && second.exists());
        let first_content = std::fs::read_to_string(&first).expect("read");
        assert!(
            first_content.contains("s1") && !first_content.contains("s2"),
            "the first quarantine snapshot must survive"
        );
        let name = second
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let suffix = name
            .strip_prefix("leaky-")
            .and_then(|s| s.strip_suffix(".md"))
            .expect("suffixed name");
        assert_eq!(suffix.len(), 4, "suffix must be 4 hex chars: {name}");
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "suffix must be hex: {name}"
        );
    }

    #[test]
    fn supersede_marks_without_deleting() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");
        let write = store
            .apply_op(
                &op("create", "old-way", &["s1"]),
                &op("create", "old-way", &["s1"]),
                &corroboration(&["s1"]),
            )
            .expect("create");
        let mut sup = op("supersede", "old-way", &[]);
        sup.existing_id = Some(write.pattern_id.clone());
        store.apply_op(&sup, &sup, &[]).expect("supersede");
        let (_, meta) = store
            .catalog()
            .expect("catalog")
            .values()
            .next()
            .expect("p")
            .clone();
        assert_eq!(meta.status, "superseded");
        assert!(meta.superseded_by.is_some());
    }

    #[test]
    fn slug_validation_rejects_traversal() {
        assert!(is_valid_slug("suumo-image-width"));
        assert!(!is_valid_slug("../etc/passwd"));
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug("-leading"));
    }

    #[test]
    fn frontmatter_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");
        store
            .apply_op(
                &op("create", "some-pattern", &["a", "b"]),
                &op("create", "some-pattern", &["a", "b"]),
                &corroboration(&["a", "b"]),
            )
            .expect("create");
        let content =
            std::fs::read_to_string(store.patterns_dir().join("some-pattern.md")).expect("read");
        let (meta, body) = PatternStore::parse_frontmatter(&content).expect("parse");
        assert_eq!(meta.corroboration.len(), 2);
        assert_eq!(meta.scope, "project:nipponhomes");
        assert!(body.contains("## Symptom"));
    }

    #[test]
    fn pattern_with_no_corroboration_stays_in_catalog() {
        // Regression: the empty-corroboration marker (`  []`) has no `key: value` shape.
        // Aborting the frontmatter parse on it silently dropped the page from the catalog,
        // which would let the maintainer create a duplicate instead of merging into it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");
        let write = store
            .apply_op(
                &op("create", "no-evidence", &[]),
                &op("create", "no-evidence", &[]),
                &[],
            )
            .expect("create");

        let raw = std::fs::read_to_string(&write.file).expect("read");
        assert!(
            raw.contains("corroboration:\n  []"),
            "empty marker expected"
        );

        let catalog = store.catalog().expect("catalog");
        assert_eq!(catalog.len(), 1, "page must stay visible to the maintainer");
        let (_, meta) = catalog.values().next().expect("entry");
        assert_eq!(meta.id, write.pattern_id);
        assert!(meta.corroboration.is_empty());

        store.update_index().expect("index");
        let index = std::fs::read_to_string(tmp.path().join("index.md")).expect("index");
        assert!(index.contains(&write.pattern_id));
    }

    #[test]
    fn quarantine_writes_outside_catalog() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = PatternStore::new(tmp.path()).expect("store");
        let path = store
            .quarantine(&op("create", "leaky", &["s"]), &corroboration(&["s"]))
            .expect("quarantine");
        assert!(path.starts_with(store.quarantine_dir()));
        assert!(store.catalog().expect("catalog").is_empty());
    }
}
