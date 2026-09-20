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
        out.push_str(&format!("title: \"{}\"\n", meta.title.replace('"', "'")));
        out.push_str(&format!("status: {}\n", meta.status));
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
        out.push_str(&format!(
            "superseded_by: {}\nscrub_pass: {}\n---\n",
            meta.superseded_by.as_deref().unwrap_or("null"),
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
        if !is_valid_slug(&op.slug) {
            bail!("invalid pattern slug `{}`", op.slug);
        }
        match op.action.as_str() {
            "create" => self.create(scrubbed, evidence),
            "merge" => self.merge(op, scrubbed, evidence),
            "supersede" => self.supersede(op, scrubbed, evidence),
            other => bail!("unknown pattern action `{other}`"),
        }
    }

    fn create(&self, scrubbed: &PatternOp, evidence: &[Corroboration]) -> Result<PatternWrite> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let id = generate_pattern_id();
        let meta = PatternMeta {
            id: id.clone(),
            title: scrubbed.title.clone(),
            status: "candidate".into(),
            scope: scrubbed.scope.clone().unwrap_or_else(|| "global".into()),
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
        let mut meta = existing.clone();
        meta.title = scrubbed.title.clone();
        meta.updated = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        if scrubbed.scope.is_some() {
            meta.scope = scrubbed.scope.clone().unwrap();
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
        let prior_evidence = existing_evidence_section(&existing_content);
        let mut summary = prior_evidence;
        if !summary.is_empty() {
            summary.push('\n');
        }
        summary.push_str(&evidence_summary(scrubbed));

        let content = Self::render(
            &meta,
            &scrubbed.symptom,
            &scrubbed.root_cause,
            &scrubbed.fix,
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
            scope: scrubbed.scope.clone().unwrap_or_else(|| "global".into()),
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
        let path = self.quarantine_dir().join(format!("{}.md", scrubbed.slug));
        self.write_atomic(&path, &content)?;
        Ok(path)
    }

    /// Render `index.md` (paper: the Maintainer revises the wiki index each iteration).
    pub fn update_index(&self) -> Result<()> {
        let mut lines = vec![
            "# Wiki Pattern Catalog".to_string(),
            String::new(),
            "| ID | Title | Scope | Status | File |".to_string(),
            "|---|---|---|---|---|".to_string(),
        ];
        for (_, (path, meta)) in self.catalog()? {
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let link = format!("patterns/{file}");
            lines.push(format!(
                "| `{}` | {} | `{}` | {} | [{}]({link}) |",
                meta.id, meta.title, meta.scope, meta.status, file
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

fn existing_evidence_section(content: &str) -> String {
    let mut out = String::new();
    let mut in_section = false;
    for line in content.lines() {
        if line.starts_with("## ") {
            in_section = line.trim() == "## Evidence";
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

pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 80
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
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

    fn op(action: &str, slug: &str, sessions: &[&str]) -> PatternOp {
        PatternOp {
            action: action.into(),
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

        // Second run merges new evidence with a different slug spelling (same id).
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
