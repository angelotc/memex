//! Error-turn extraction and char-budgeted session digests (paper step 9: sample traces
//! to fit the Maintainer's context).

use super::config::WikiLoopConfig;
use crate::types::Record;

/// Indices of records that look like tool failures, with one turn of surrounding context.
pub fn extract_error_turn_indices(records: &[Record]) -> Vec<usize> {
    let mut error_indices = Vec::new();
    for (idx, rec) in records.iter().enumerate() {
        if rec.links.tool_result_is_error == Some(true) {
            error_indices.push(idx);
            continue;
        }
        let content = rec.tool_output.as_deref().unwrap_or(&rec.text);
        let lower = content.to_lowercase();
        if [
            "command failed",
            "traceback (most recent",
            "error: ",
            "fatal: ",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            error_indices.push(idx);
        }
    }
    if error_indices.is_empty() {
        return Vec::new();
    }

    let mut selected: Vec<usize> = error_indices
        .into_iter()
        .flat_map(|idx| [idx.wrapping_sub(1), idx, idx + 1])
        .filter(|&idx| idx < records.len())
        .collect();
    selected.sort_unstable();
    selected.dedup();
    selected
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Format one session's error turns into a bounded markdown summary.
pub fn build_session_summary(
    cfg: &WikiLoopConfig,
    meta: &super::ingest::SessionMeta,
    records: &[Record],
    error_indices: &[usize],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "### Session `{}` (Source: {}, Project: {})\n",
        meta.session_id,
        meta.source,
        meta.repo_project
            .clone()
            .or_else(|| meta.project.clone())
            .unwrap_or_else(|| "unknown".into())
    ));
    out.push_str(&format!(
        "- Working Directory: `{}`\n- Message Count: {}, Resolution: {}\n- Error turns:\n",
        meta.cwd.as_deref().unwrap_or("?"),
        meta.message_count,
        meta.resolution_status.as_deref().unwrap_or("?")
    ));

    for &idx in error_indices {
        let rec = &records[idx];
        let is_err = rec.links.tool_result_is_error == Some(true);
        out.push_str(&format!(
            "- **Turn {} ({}){}**\n",
            rec.turn_id,
            rec.role,
            if is_err { " [TOOL ERROR]" } else { "" }
        ));
        if let Some(tool) = &rec.tool_name {
            out.push_str(&format!(
                "  - Tool Call: `{}`: {}\n",
                tool,
                truncate(rec.tool_input.as_deref().unwrap_or(""), 500)
            ));
        }
        let content = rec
            .tool_output
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&rec.text);
        if !content.is_empty() {
            let trimmed = truncate(content, 1500);
            let suffix = if content.len() > trimmed.len() {
                " ... [truncated]"
            } else {
                ""
            };
            out.push_str(&format!("  - Content: {trimmed}{suffix}\n"));
        }
    }
    if out.len() > cfg.max_chars_per_session {
        out = truncate(&out, cfg.max_chars_per_session).to_string();
        out.push_str("\n... [Session Digest Truncated]\n");
    }
    out
}

/// Combine session summaries into one batch digest within the total budget.
/// Sessions are taken in the order given until the budget is exhausted.
pub fn build_batch_digest(cfg: &WikiLoopConfig, summaries: &[String]) -> String {
    let mut out = String::new();
    for (i, summary) in summaries.iter().enumerate() {
        if out.len() + summary.len() > cfg.max_chars_per_batch {
            out.push_str("\n\n... [Batch Digest Truncated at Char Limit]");
            break;
        }
        if i > 0 {
            out.push_str("\n\n---\n\n");
        }
        out.push_str(summary);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Record, RecordLinks};

    fn record(turn: u32, role: &str, text: &str, tool_error: Option<bool>) -> Record {
        Record {
            source: crate::types::SourceKind::Claude,
            doc_id: turn as u64,
            ts: 1_000 + turn as u64,
            project: "p".into(),
            session_id: "s".into(),
            turn_id: turn,
            role: role.into(),
            text: text.into(),
            tool_name: if tool_error.is_some() {
                Some("bash".into())
            } else {
                None
            },
            tool_input: if tool_error.is_some() {
                Some("npm run build".into())
            } else {
                None
            },
            tool_output: if tool_error.is_some() {
                Some("Error: missing module foo".into())
            } else {
                None
            },
            links: RecordLinks {
                tool_result_is_error: tool_error,
                ..Default::default()
            },
            source_path: "/tmp/x.jsonl".into(),
        }
    }

    #[test]
    fn extracts_error_turn_with_context() {
        let records = vec![
            record(1, "user", "run the build", None),
            record(2, "assistant", "", Some(true)),
            record(3, "assistant", "installing module foo", None),
            record(4, "user", "looks good", None),
        ];
        let idx = extract_error_turn_indices(&records);
        assert_eq!(idx, vec![0, 1, 2]);
    }

    #[test]
    fn no_errors_yields_empty() {
        let records = vec![record(1, "user", "hello", None)];
        assert!(extract_error_turn_indices(&records).is_empty());
    }

    #[test]
    fn budgets_are_respected() {
        let cfg = WikiLoopConfig::default();
        let mut cfg = cfg.clone();
        cfg.max_chars_per_session = 5000;
        cfg.max_chars_per_batch = 100;
        let meta = super::super::ingest::SessionMeta {
            source: "claude".into(),
            session_id: "s1".into(),
            source_path: None,
            project: Some("p".into()),
            cwd: Some("/app".into()),
            git_root: None,
            repo_project: Some("p".into()),
            started_at: 0,
            last_at: 0,
            message_count: 4,
            resolution_status: Some("done".into()),
        };
        let records = vec![record(1, "user", "hello", None)];
        let s = build_session_summary(&cfg, &meta, &records, &[0]);
        assert!(s.contains("Session `s1`"));
        let batch = build_batch_digest(&cfg, &[s.clone(), "y".repeat(500)]);
        assert!(batch.len() < 250, "batch budget must truncate");
    }
}
