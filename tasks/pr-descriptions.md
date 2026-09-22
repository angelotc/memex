# PR descriptions — resume cwd / antigravity restoration

Two independent branches off `main`. They share only `src/sources/antigravity.rs`,
in disjoint hunks (`profile_roots` in PR 1, cwd fallback in PR 2), so either merge
order works.

- PR 1 branch: `fix/resume-state-store-dirs`
- PR 2 branch: `fix/antigravity-session-cwd`

---

## PR 1 — fix/resume-state-store-dirs

### Title

```
Never resume sessions inside agent state stores
```

### Description

```markdown
## Problem

When a session's working directory can't be resolved, every resume flow falls
back to the transcript's own directory. Transcripts live inside agent state
stores, so the resumed CLI gets launched from (and asked to trust) agent
internals — e.g. an antigravity session opens with:

    Accessing workspace:
    /root/.gemini/antigravity-cli/brain/<uuid>/.system_generated/logs

    Do you trust the contents of this project?

This is not antigravity-specific: the same fallback can `cd` any CLI into
`~/.claude/projects/<slug>`, `~/.codex/sessions/...`, `~/.openclaw/...`, etc.

## Root cause

1. **Cwd resolution was piecemeal.** `tui.rs::resolve_session_cwd` and
   `machine.rs::discover_cwd` were near-duplicate generic JSONL scanners with
   special cases only for Copilot and Bob. Formats that record the cwd
   elsewhere (antigravity tool-call args, SQLite stores) resolved to `None`.
2. **The last resort was the transcript's parent directory**, i.e. the state
   store itself.

## Changes

- **`sources::session_cwd(source, path, session_id)`** — single dispatch point
  for cwd resolution. Sources with dedicated extractors (antigravity, bob,
  copilot's `workspace.yaml` sidecar — moved here from tui.rs) plug in
  directly; everything else shares one generic JSONL scan now living in
  `sources::jsonl.rs::scan_session_cwd`. The TUI and remote-machine paths use
  the same code instead of two drifting copies.
- **`sources::is_state_store_dir(dir)`** — recognizes state stores from each
  source's *own* discovery roots (claude, codex, cursor, opencode, pi, omp,
  openclaw, copilot, grok, hermes, jcode, muse, antigravity profiles, bob), so
  env overrides (`CODEX_HOME`, `ANTIGRAVITY_HOME`, `OPENCLAW_STATE_DIR`, …)
  stay authoritative. A root equal to `$HOME` itself is ignored (pi can be
  configured with `sessionDir = "~"`).
- **`resume::fallback_resume_cwd(source_dir)`** — keeps the transcript's
  directory only when it is not a state store; otherwise falls back to the
  user's home directory. Applied in the TUI resume flow, the CLI
  `session_resume_command` (`memex sessions` output and
  `memex herdr resume[-last]`), and remote `discover_cwd`.

## Behavior

| case                                      | before                   | after      |
| ----------------------------------------- | ------------------------ | ---------- |
| cwd resolvable                            | `cd <cwd>`               | unchanged  |
| cwd unresolvable                          | `cd <transcript parent>` | `cd $HOME` |
| resolved/fallback dir is an agent store   | `cd` into it             | `cd $HOME` |

## Testing

- New tests for the dispatch, the state-store guard, the generic scanner, and
  the fallback — all hermetic: a new `test_support::pin_source_roots` helper
  pins `HOME` plus every source-root env var into a tempdir, so nothing
  depends on the machine running the tests.
- `cargo fmt --check` and `cargo clippy -- -D warnings` clean; full lib suite:
  1023 passed, 0 failed.

## Notes

- No config or on-disk format changes.
- Companion PR (independent, either merge order works):
  `fix/antigravity-session-cwd` resolves the real cwd for antigravity brain
  transcripts from the sibling `conversations/<id>.db`, so those sessions
  resume straight into the project directory instead of falling back to
  `$HOME`.
```

---

## PR 2 — fix/antigravity-session-cwd

### Title

```
Resolve antigravity cwd from sibling conversation stores
```

### Description

```markdown
## Problem

Antigravity brain transcripts only record the working directory in explicit
`Cwd` tool arguments or workspace-mapping system messages. Sessions with
neither resolve no cwd at all:

- analytics stores a NULL repository, so the session groups under **Unfiled**;
- resume flows have no project directory to open, so the session can't be
  restored into the workspace it ran in.

## Root cause

Discovery prefers brain projections over the SQLite store
(`transcript_full.jsonl` > `transcript.jsonl` > `conversations/<id>.db`), so
`session_cwd` only ever looked at the transcript. But the sibling
`conversations/<session_id>.db` still exists on disk and records the project
root as a `file://` URL on its user steps (protobuf payload `19.4.2.*.13`) —
it was simply never consulted for transcript-backed sessions.

## Changes

- **`sibling_db_cwd(path)`** — when a brain transcript or overview projection
  carries no cwd of its own, look up `conversations/<session_id>.db` across
  the profile roots and extract its project root.
- Wired into both consumers:
  - `session_cwd()` — used by analytics fallbacks, transfer, and the resume
    cwd dispatch;
  - `index_transcript_file()` — so indexed records carry the real project
    label and `IndexParseOutput::session_cwd` feeds analytics, letting the
    repository (and the project grouping) resolve instead of landing in
    Unfiled.
- In-band signals still win: an explicit `Cwd` argument or workspace mapping
  keeps precedence; the database is strictly a fallback.

## Behavior

After a re-ingest, antigravity sessions that previously grouped under
**Unfiled** resolve their repository, and resume opens in the actual project
directory (paired with `fix/resume-state-store-dirs`, which stops the fallback
from ever entering the brain logs directory).

## Testing

- New test `transcript_without_inline_cwd_falls_back_to_sibling_db` builds a
  brain transcript with no in-band cwd plus a sibling store fixture and checks
  `session_cwd`, the parser output, and the record project labels; existing
  precedence tests confirm in-band cwd still wins.
- `cargo fmt --check` and `cargo clippy -- -D warnings` clean; antigravity
  suite 18/18; full lib suite: 1019 passed, 0 failed.

## Notes

- Run `memex ingest` once after merging so already-indexed sessions backfill
  their cwd/repository metadata.
- Companion PR (independent, either merge order works):
  `fix/resume-state-store-dirs` adds the general state-store guard so an
  unresolvable cwd falls back to `$HOME` instead of the transcript directory.
```
