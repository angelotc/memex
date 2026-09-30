//! Read side of the wiki layer: `memex wiki search` / `memex wiki show`.
//!
//! The wiki holds a few hundred pattern pages, so each query builds a throwaway in-RAM
//! tantivy index over them (the `memory_search::LexicalMemoryIndex` idiom) rather than
//! maintaining an on-disk index that could drift from the markdown source of truth.

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    INDEXED, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::{Index, IndexReader, ReloadPolicy, TantivyDocument};

use super::patterns::{PatternMeta, PatternStore};

#[derive(Debug, Clone, Default)]
pub struct WikiSearchOptions {
    /// Maximum hits to return; 0 returns none (the CLI defaults to 5 and rejects 0).
    pub limit: usize,
    /// Keep global patterns plus those scoped to `project:<name>`.
    pub project: Option<String>,
    /// "failure" | "success"
    pub kind: Option<String>,
    pub include_superseded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct WikiHit {
    pub id: String,
    /// File stem, e.g. `psql-missing-root-crt-system-ca`.
    pub slug: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub scope: String,
    /// Number of corroborating sessions.
    pub corroboration: usize,
    pub score: f32,
    /// `## Symptom` section, whitespace-collapsed, untruncated.
    pub symptom: String,
    /// `## Fix` section, whitespace-collapsed, untruncated.
    pub fix: String,
    pub path: PathBuf,
}

struct Page {
    meta: PatternMeta,
    slug: String,
    body: String,
    path: PathBuf,
}

/// BM25 search over `patterns/*.md` (title boosted over slug over body), highest first.
pub fn search(wiki_root: &Path, query: &str, opts: &WikiSearchOptions) -> Result<Vec<WikiHit>> {
    let query = sanitize_query(query)?;
    let pages: Vec<Page> = load_pages(wiki_root)?
        .into_iter()
        .filter(|page| keep(&page.meta, opts))
        .collect();
    if pages.is_empty() || opts.limit == 0 {
        return Ok(Vec::new());
    }

    let mut schema = Schema::builder();
    let key = schema.add_u64_field("key", INDEXED | STORED);
    let stemmed = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("en_stem")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let title = schema.add_text_field("title", stemmed.clone());
    let slug = schema.add_text_field("slug", stemmed.clone());
    let body = schema.add_text_field("body", stemmed);
    let index = Index::create_in_ram(schema.build());
    let mut writer = index.writer(15_000_000)?;
    for (position, page) in pages.iter().enumerate() {
        let mut row = TantivyDocument::default();
        row.add_u64(key, position as u64);
        row.add_text(title, &page.meta.title);
        row.add_text(slug, page.slug.replace(['-', '_'], " "));
        row.add_text(body, &page.body);
        writer.add_document(row)?;
    }
    writer.commit()?;
    let reader: IndexReader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    let searcher = reader.searcher();

    let mut parser = QueryParser::for_index(&index, vec![title, slug, body]);
    parser.set_field_boost(title, 2.0);
    parser.set_field_boost(slug, 1.5);
    let parsed = parser
        .parse_query(&query)
        .with_context(|| format!("parsing wiki query {query:?}"))?;
    let rows = searcher.search(&parsed, &TopDocs::with_limit(opts.limit.min(pages.len())))?;
    rows.into_iter()
        .map(|(score, address)| {
            let row = searcher.doc::<TantivyDocument>(address)?;
            let page = row
                .get_first(key)
                .and_then(|value| value.as_u64())
                .and_then(|position| pages.get(usize::try_from(position).ok()?))
                .ok_or_else(|| anyhow!("wiki index row is missing its page key"))?;
            Ok(to_hit(page, score))
        })
        .collect()
}

/// Resolve a pattern id (`pat_...`), slug (file stem), or file name (`x.md`) to its page.
pub fn resolve(wiki_root: &Path, key: &str) -> Result<PathBuf> {
    let key = key.trim();
    if key.is_empty() || key.contains('/') || key.contains("..") {
        bail!("invalid wiki pattern key '{key}' (expected a pattern id, slug, or file name)");
    }
    let direct =
        patterns_dir(wiki_root)?.join(format!("{}.md", key.strip_suffix(".md").unwrap_or(key)));
    if direct.is_file() {
        return Ok(direct);
    }
    load_pages(wiki_root)?
        .into_iter()
        .find(|page| page.meta.id == key)
        .map(|page| page.path)
        .ok_or_else(|| anyhow!("no wiki pattern '{key}' (try: memex wiki search \"<words>\")"))
}

/// Text cards for `memex wiki search` (the default output format).
pub fn format_text(hits: &[WikiHit]) -> String {
    if hits.is_empty() {
        return "no matching wiki patterns\n".to_string();
    }
    hits.iter()
        .enumerate()
        .map(|(position, hit)| render_card(position + 1, hit))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_card(rank: usize, hit: &WikiHit) -> String {
    let sessions = match hit.corroboration {
        1 => "1 session".to_string(),
        n => format!("{n} sessions"),
    };
    // `--all` can surface superseded pages; flag them so their fix isn't followed blindly.
    let status = if hit.status == "superseded" {
        " · superseded"
    } else {
        ""
    };
    let mut card = format!(
        "{rank}. {}  [{} · {}{status} · {sessions} · {:.1}]\n   {}\n",
        hit.slug,
        hit.kind,
        hit.scope,
        hit.score,
        truncate_chars(&hit.title, 140)
    );
    if !hit.symptom.is_empty() {
        card.push_str(&format!(
            "   Symptom: {}\n",
            truncate_chars(&hit.symptom, 200)
        ));
    }
    if !hit.fix.is_empty() {
        card.push_str(&format!("   Fix: {}\n", truncate_chars(&hit.fix, 400)));
    }
    card.push_str(&format!("   memex wiki show {}\n", hit.slug));
    card
}

/// Read-only lookup: unlike `PatternStore::new`, never creates the wiki directories.
fn patterns_dir(wiki_root: &Path) -> Result<PathBuf> {
    let dir = wiki_root.join("patterns");
    if !dir.is_dir() {
        bail!(
            "no wiki at {} (run `memex wiki-loop init`, or set wiki_root in ~/.memex/wiki-loop.toml)",
            wiki_root.display()
        );
    }
    Ok(dir)
}

/// Every parseable `*.md` page, in file-name order; unreadable or malformed pages are skipped.
fn load_pages(wiki_root: &Path) -> Result<Vec<Page>> {
    let dir = patterns_dir(wiki_root)?;
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let content = std::fs::read_to_string(&path).ok()?;
            let (meta, body) = PatternStore::parse_frontmatter(&content)?;
            let slug = path.file_stem()?.to_string_lossy().into_owned();
            Some(Page {
                meta,
                slug,
                body,
                path,
            })
        })
        .collect())
}

fn keep(meta: &PatternMeta, opts: &WikiSearchOptions) -> bool {
    (opts.include_superseded || !is_superseded(meta))
        && opts.kind.as_deref().is_none_or(|kind| meta.kind == kind)
        && opts.project.as_deref().is_none_or(|project| {
            meta.scope == "global" || meta.scope == format!("project:{project}")
        })
}

fn is_superseded(meta: &PatternMeta) -> bool {
    meta.status == "superseded"
        || meta.superseded_by.as_deref().is_some_and(|by| {
            let by = by.trim();
            !by.is_empty() && by != "null"
        })
}

fn to_hit(page: &Page, score: f32) -> WikiHit {
    WikiHit {
        id: page.meta.id.clone(),
        slug: page.slug.clone(),
        title: page.meta.title.clone(),
        kind: page.meta.kind.clone(),
        status: page.meta.status.clone(),
        scope: page.meta.scope.clone(),
        corroboration: page.meta.corroboration.len(),
        score,
        symptom: section(&page.body, "Symptom"),
        fix: section(&page.body, "Fix"),
        path: page.path.clone(),
    }
}

/// Make raw error text safe for tantivy's query grammar: punctuation (quotes, colons,
/// `-`/`+` prefixes, brackets, `*`) becomes whitespace, and the whole query is lowercased.
/// Lowercasing (rather than only AND/OR/NOT) also defuses the grammar's other reserved
/// bare words (`IN`, `TO`), and the `en_stem` tokenizer lowercases every term anyway,
/// so it changes no match. Terms combine with the parser's default OR.
fn sanitize_query(raw: &str) -> Result<String> {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    let query = cleaned
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if query.is_empty() {
        bail!("empty wiki search query");
    }
    Ok(query)
}

/// Text of the `## {heading}` section up to the next `## ` heading, whitespace-collapsed.
fn section(body: &str, heading: &str) -> String {
    let header = format!("## {heading}");
    let mut lines = body.lines().skip_while(|line| line.trim() != header);
    if lines.next().is_none() {
        return String::new();
    }
    lines
        .take_while(|line| !line.starts_with("## "))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Cut to at most `max` chars (never mid-codepoint), appending `…` when it cuts.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{}…", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        slug: &'static str,
        id: &'static str,
        title: &'static str,
        status: &'static str,
        kind: &'static str,
        scope: &'static str,
        superseded_by: &'static str,
        sessions: usize,
        symptom: &'static str,
        fix: &'static str,
    }

    const PSQL: Fixture = Fixture {
        slug: "psql-missing-root-crt-system-ca",
        id: "pat_psql",
        title: "psql Fails with Missing root.crt on Remote Cloud Database Connections",
        status: "candidate",
        kind: "failure",
        scope: "global",
        superseded_by: "null",
        sessions: 3,
        symptom: "psql: error: connection to server failed: root certificate file\n\"/root/.postgresql/root.crt\" does not exist",
        fix: "Pass sslrootcert=system (libpq 16+) so the system CA bundle is used.",
    };

    fn fixtures() -> Vec<Fixture> {
        vec![
            PSQL,
            Fixture {
                slug: "cargo-test-oom-parallel-linker",
                id: "pat_cargo",
                title: "cargo test OOM-kills the VM when the linker overlaps parallel rustc jobs",
                status: "candidate",
                kind: "failure",
                scope: "global",
                superseded_by: "null",
                sessions: 1,
                symptom: "The box froze during cargo test; dmesg showed the oom-killer.",
                fix: "Cap build jobs at 2 in ~/.cargo/config.toml.",
            },
            Fixture {
                slug: "copy-root-crt-into-home",
                id: "pat_old_psql",
                title: "Copy root.crt into ~/.postgresql so psql can verify the server",
                status: "superseded",
                kind: "failure",
                scope: "global",
                superseded_by: "pat_psql",
                sessions: 2,
                symptom: "root.crt does not exist when psql connects over TLS.",
                fix: "Copy the provider CA file to ~/.postgresql/root.crt.",
            },
            Fixture {
                slug: "pooler-breaks-advisory-locks",
                id: "pat_pooler",
                title: "PlanetScale pooler breaks session advisory locks",
                status: "candidate",
                kind: "failure",
                scope: "project:nipponhomes",
                superseded_by: "null",
                sessions: 4,
                symptom: "The advisory lock is never released through the pooler.",
                fix: "Use flock on the host instead of pg_advisory_lock.",
            },
            Fixture {
                slug: "cargo-check-under-memory-limits",
                id: "pat_check",
                title: "Validate with cargo check under a tight memory budget",
                status: "candidate",
                kind: "success",
                scope: "project:memex",
                superseded_by: "null",
                sessions: 1,
                symptom: "",
                fix: "Run MALLOC_ARENA_MAX=1 cargo check --bin memex instead of cargo build.",
            },
        ]
    }

    fn write_page(dir: &Path, page: &Fixture) {
        let corroboration: String = (0..page.sessions)
            .map(|n| format!("  - source: codex, session_id: s-{}-{n}, ts: 1\n", page.id))
            .collect();
        let content = format!(
            "---\nid: {}\ntitle: \"{}\"\nstatus: {}\nkind: {}\nscope: {}\n\
             created: 2026-09-01T00:00:00Z\nupdated: 2026-09-01T00:00:00Z\n\
             corroboration:\n{corroboration}superseded_by: {}\nscrub_pass: v1\n---\n\n\
             ## Symptom\n{}\n\n## Root Cause\nSee evidence.\n\n## Fix\n{}\n\n\
             ## Evidence\nSession evidence.\n",
            page.id,
            page.title,
            page.status,
            page.kind,
            page.scope,
            page.superseded_by,
            page.symptom,
            page.fix
        );
        std::fs::write(dir.join(format!("{}.md", page.slug)), content).expect("write page");
    }

    fn wiki() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("patterns");
        std::fs::create_dir_all(&dir).expect("patterns dir");
        for page in fixtures() {
            write_page(&dir, &page);
        }
        // Malformed and non-markdown files are skipped, not fatal.
        std::fs::write(dir.join("broken.md"), "no frontmatter here").expect("write");
        std::fs::write(dir.join("notes.txt"), "root.crt psql").expect("write");
        tmp
    }

    fn opts(limit: usize) -> WikiSearchOptions {
        WikiSearchOptions {
            limit,
            ..Default::default()
        }
    }

    fn slugs(hits: &[WikiHit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.slug.as_str()).collect()
    }

    #[test]
    fn raw_error_text_ranks_matching_page_first() {
        let tmp = wiki();
        let hits = search(
            tmp.path(),
            r#"psql: error: connection to server failed: root certificate file "/root/.postgresql/root.crt" does not exist"#,
            &opts(5),
        )
        .expect("search");
        let top = hits.first().expect("a hit");
        assert_eq!(top.slug, PSQL.slug);
        assert_eq!(top.id, "pat_psql");
        assert_eq!(top.title, PSQL.title, "surrounding quotes are stripped");
        assert_eq!(top.corroboration, 3);
        assert!(top.symptom.starts_with("psql: error: connection"));
        assert!(
            !top.symptom.contains('\n'),
            "symptom is whitespace-collapsed"
        );
        assert!(top.fix.starts_with("Pass sslrootcert=system"));
        assert!(hits.windows(2).all(|pair| pair[0].score >= pair[1].score));
        assert_eq!(
            search(tmp.path(), "psql", &opts(1)).expect("search").len(),
            1
        );
        // Reserved grammar words in raw text can never break parsing.
        search(tmp.path(), "SIGN IN AND OR NOT TO", &opts(5)).expect("keywords parse");
    }

    #[test]
    fn sanitize_query_neutralizes_syntax() {
        assert_eq!(
            sanitize_query(r#"psql: error: "root.crt" -- NOT FOUND"#).unwrap(),
            "psql error root crt not found"
        );
        assert_eq!(
            sanitize_query("cargo --release -j2 +nightly").unwrap(),
            "cargo release j2 nightly"
        );
        assert_eq!(sanitize_query("AND OR NOT IN").unwrap(), "and or not in");
        assert_eq!(sanitize_query("title:foo*").unwrap(), "title foo");
        for empty in ["", "   ", r#" :: -- "" [] "#] {
            let err = sanitize_query(empty).unwrap_err();
            assert_eq!(err.to_string(), "empty wiki search query");
        }
    }

    #[test]
    fn superseded_pages_hidden_unless_requested() {
        let tmp = wiki();
        let hits = search(tmp.path(), "copy root crt postgresql", &opts(10)).expect("search");
        assert!(!slugs(&hits).contains(&"copy-root-crt-into-home"));
        let all = WikiSearchOptions {
            include_superseded: true,
            ..opts(10)
        };
        let hits = search(tmp.path(), "copy root crt postgresql", &all).expect("search");
        assert!(slugs(&hits).contains(&"copy-root-crt-into-home"));
    }

    #[test]
    fn project_and_kind_filters() {
        let tmp = wiki();
        let scoped = WikiSearchOptions {
            project: Some("nipponhomes".into()),
            ..opts(10)
        };
        let hits = search(tmp.path(), "evidence", &scoped).expect("search");
        let found = slugs(&hits);
        assert!(found.contains(&"pooler-breaks-advisory-locks"));
        assert!(found.contains(&PSQL.slug));
        assert!(found.contains(&"cargo-test-oom-parallel-linker"));
        assert!(!found.contains(&"cargo-check-under-memory-limits"));

        let success = WikiSearchOptions {
            kind: Some("success".into()),
            ..opts(10)
        };
        let hits = search(tmp.path(), "evidence", &success).expect("search");
        assert_eq!(slugs(&hits), vec!["cargo-check-under-memory-limits"]);
    }

    #[test]
    fn section_and_truncation_are_utf8_safe() {
        let body = "## Symptom\n  ニセコ町  (Niseko Town)\n\tline two\n\n## Fix\nfix it\n### nested\nstill fix\n## Evidence\ne\n";
        assert_eq!(section(body, "Symptom"), "ニセコ町 (Niseko Town) line two");
        assert_eq!(section(body, "Fix"), "fix it ### nested still fix");
        assert_eq!(section(body, "Root Cause"), "");

        assert_eq!(truncate_chars("ニセコ町ニセコ町", 4), "ニセコ町…");
        assert_eq!(truncate_chars("ニセコ町", 4), "ニセコ町");
        assert_eq!(truncate_chars("ab cd", 3), "ab…");
        assert_eq!(truncate_chars("", 0), "");
    }

    #[test]
    fn text_cards_follow_format() {
        assert_eq!(format_text(&[]), "no matching wiki patterns\n");
        let tmp = wiki();
        let hits = search(tmp.path(), "memory budget cargo check", &opts(5)).expect("search");
        let text = format_text(&hits);
        let card = text
            .split("\n\n")
            .find(|card| card.contains("cargo-check-under-memory-limits"))
            .expect("card");
        assert!(card.contains("[success · project:memex · 1 session · "));
        assert!(!card.contains("Symptom:"), "empty sections are omitted");
        assert!(card.contains("   Fix: Run MALLOC_ARENA_MAX=1"));
        assert!(
            card.trim_end()
                .ends_with("   memex wiki show cargo-check-under-memory-limits")
        );
    }

    #[test]
    fn resolve_by_id_slug_and_file_name() {
        let tmp = wiki();
        let expected = tmp
            .path()
            .join("patterns")
            .join(format!("{}.md", PSQL.slug));
        assert_eq!(resolve(tmp.path(), "pat_psql").unwrap(), expected);
        assert_eq!(resolve(tmp.path(), PSQL.slug).unwrap(), expected);
        assert_eq!(
            resolve(tmp.path(), &format!("{}.md", PSQL.slug)).unwrap(),
            expected
        );
        assert!(resolve(tmp.path(), "../x").is_err());
        assert!(resolve(tmp.path(), "patterns/x").is_err());
        let missing = resolve(tmp.path(), "nope").unwrap_err().to_string();
        assert_eq!(
            missing,
            "no wiki pattern 'nope' (try: memex wiki search \"<words>\")"
        );
    }
}
