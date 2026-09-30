# wiki-loop: proposals reviewable in TUI + dedup fix + ingest cron

## Why (session 2026-09-22)
- Bug 1: tier0 `dedup` only compares against the live skills root; a validated-but-
  unapplied proposal is invisible, so the proposer re-proposed
  `python-inline-quote-escaping` twice (prop_1a0c5b727ed, prop_1a0c67b3572), burning
  weekly ceiling slots and duplicating review work.
- Review UX: approving/denying proposals requires CLI (`memex wiki-loop apply`); user
  wants see/approve/deny in the TUI skills screen.
- Raw-layer starvation: nothing schedules `memex index`; analytics.sqlite went stale
  Sep 21 23:21 → Sep 22 05:40 (overnight sessions invisible to the loop) because
  ingest only ran when a memex command happened to run.

## Changes
- [x] ledger.rs: `open_proposal_for_skill(skill, exclude_id)` → newest
      pending/validated proposal id+status for a skill name.
- [x] gates.rs tier0 dedup: fail when an open (pending/validated) proposal already
      stages the same skill; detail points at its id ("apply or deny it first").
- [x] gates.rs: `pub fn deny(cfg, ledger, id)` — pending/validated → rejected, stamps
      a `human_review` gate note, appends skill-impact.md audit entry (mirror of apply).
- [x] gates.rs apply(): mirrors "accepted" into the ledger (was proposal.json-only —
      the review surface kept applied proposals staged forever, and dedup would have
      blocked legitimate v2 re-proposals of an applied skill).
- [x] tui.rs: `WikiEntryKind::Proposal` (+id/status fields on WikiEntry);
      `load_skill_entries` appends staged proposals (ledger) above applied skills;
      proposal list rows show status badge; preview shows meta header + SKILL.md +
      skill.diff + PURPOSE.md; `a` approve (validated only) / `d` deny in
      `handle_skills_key` with delivery lock for approve; footer hints; empty-note
      text; App caches `wiki_loop_config` for test injectability.
- [x] cli.rs `install_cron_schedule`: maintainer cadence matches live */5; added
      `*/10 * * * * {exe} --no-update-check --non-interactive index` (index.log).
- [x] harness.rs `unwrap_envelope`: probe `structured_output` (schema-enforced
      object) before scraping prose `response` — the live breaker-tripper was agy
      filling structured_output while response drifted brace-free.
- [x] docs: wiki-loop.md (ingest cron, TUI review keys, cron block example).
- [x] Tests: ledger open-proposal lookup; dedup fails on staged duplicate + clears
      after deny; deny() semantics; TUI list/apply/deny/pending-guard key paths;
      harness structured_output unwrap (object wins over prose; null keeps old error).
- [x] Verify: fmt ✅, clippy -D warnings ✅, full lib suite 1113 passed / 0 failed.
- [x] Deploy: rebuilt, installed to /root/.local/bin/memex-wiki-loop (backup
      .pre-tui-review.bak), `wiki-loop init --install-cron` refreshed the live block;
      ingest cron proven live (06:30 + 06:40 ticks in index.log; collector swept 16
      sessions into the queue).

## Review
- All three asks shipped: dedup no longer blind to staged proposals; the skills
  screen lists proposals with a status badge + full preview and a=approve / d=deny
  (delivery lock, audit trail, auto-refresh); ingest cron every 10 min keeps the
  analytics store fresh without a memex command having to run.
- Extra fixes surfaced by testing: apply() ledger mirror, harness structured_output
  unwrap (the "model output carried no JSON" breaker-tripper).
- VM died ~06:27 after the first build: the proposer's 06:17 agy run spiked on top
  of post-build pressure (see lessons.md — watchdog floor back at 1800MB, build
  windows picked around proposer ticks, MALLOC_ARENA_MAX=1 for link-heavy steps).
- BLOCKED (environment): agy individual quota exhausted (429, resets ~02:41 UTC
  Sep 23). 17 sessions queued; breaker tripped. After the quota resets, run
  `memex-wiki-loop wiki-loop run-maintainer --force` once — on success the loop
  resumes on its own 5-min ticks. TUI proposal review needs no model and works now:
  two validated `python-inline-quote-escaping` proposals are staged — deny the older
  prop_1a0c5b727ed and keep prop_1a0c67b3572 (or approve it directly).
- Committed as one commit (the earlier tier-1 replay + ceiling rework and this
  session's review surface + cron + harness fix share hunks in gates/ledger/cli;
  splitting inside hunks risked the validated tree) and pushed to origin/wiki-loop.
- Model switch (2026-09-22, user request): all three roles now run
  opencode + zai-coding-plan/glm-5.3-flash#high (`run - --format default --auto`,
  json_schema=false — no --json-schema flag in opencode; prompts demand JSON,
  harness scrapes). agy+gemini wiring kept as commented example in
  ~/.memex/wiki-loop.toml (backup .pre-glm.bak). Digest budgets rescaled from the
  gemini-1M sizing (96KB/800KB) to 24KB/160KB per stratum. First live GLM run:
  11 sessions → 9 pattern ops in 7m43s, breaker cleared, health ok. TUI review
  proven by the user: prop_1a0c67b3572 accepted (first skill deployed to
  /apps/skills/python-inline-quote-escaping), duplicate prop_1a0c5b727ed denied.
---

# wiki-loop: unblock skill creation (fixes A + B) + live config

## Why
Three proposals (Sep 20–21) were all auto-rejected; the weekly ceiling then counted
those rejections and muted the proposer until Sep 27. Root causes fact-checked
against the WikiSkill paper and docs/wiki-loop.md:

- **A (root cause):** `gates.rs::tier1` feeds the judge the most-*recent* sessions of
  contributing projects, not the failure-mode sessions named in the proposal's
  patterns. Judge contract (prompts/judge.md, docs/wiki-loop.md:69) says "sessions
  that hit the failure mode" → both tier1 rejections judged irrelevant sessions.
- **B:** `ledger.rs::proposals_since` counts rejected proposals toward
  `max_proposals_per_week`; rejections never reach the human, so the ceiling burns
  budget with no approval and starves the paper's reject→retry loop.
- **Config:** live proposer runs gemini-3.8-flash @ medium in `~/.memex/wiki-loop.toml`
  (user's edit went to the dead Python rewrite config).

## Changes
- [x] A: `gates.rs` — replay set = corroborating sessions of `purpose_patterns`
      (PatternStore catalog → corroboration {source, session_id} →
      `ingest::get_session_meta`), newest-first, then top up with
      `recent_sessions_for_project` to `tier1_sessions`. Empty-union fail-closed
      details preserved. Project scope keeps its top-up source.
- [x] A tests: corroborating sessions ordered ahead of recent top-up; unknown
      patterns fall back to recent sessions. (`tier1_replay_prefers_corroborating_sessions`)
- [x] B: `ledger.rs` — `proposals_since` → `reviewable_proposals_since`
      (`status != 'rejected'`); call sites `proposer.rs:41`, `cli.rs` status.
      `rejected_proposals_since` untouched (recently_rejected gate).
- [x] B test: rejected proposal drops out of the weekly count but stays in
      `rejected_proposals_since`; validated still counts.
- [x] Config: `~/.memex/wiki-loop.toml` `[proposer] effort = "high"`.
- [x] Docs: wiki-loop.md (ceiling counting + tier1 replay prose) and example.toml
      comment updated; example-sync test only checks values → safe.
- [x] Verify: `cargo fmt --check` ✅, `cargo clippy -- -D warnings` ✅,
      `cargo test wiki_loop` ✅ (79 passed, incl. both new tests) — under watchdog.
- [x] Rebuild + install: `target` lives on the mount (`CARGO_TARGET_DIR=
      /mnt/HC_Volume_106441424/caches/target`), built `-j 1` in 1m50s, installed to
      `/root/.local/bin/memex-wiki-loop` (old binary backed up as
      `.v0.22.0.bak`). `wiki-loop status`: `proposals: 0/week reviewable (ceiling 3)`.

## Review
- Root-cause fixes shipped: tier1 judge now replays failure-mode (corroborating)
  sessions first with recent top-up; weekly ceiling counts only reviewable
  (non-rejected) proposals; live proposer raised to gemini-3.8-flash @ high.
- End-to-end proof (manual `run-proposer`, post-install): first proposal ever to
  pass ALL gates — `python-inline-quote-escaping` (global, corroborated across 4
  sessions) — `validated`, staged for human apply:
  `memex wiki-loop apply prop_1a0c5b727ed_1a0c5b727edd2a260a3dc`
- Cron ticks (proposer :17 every 6h) now operate unblocked.

## Ops note (VM crashes)
Two VM restarts during `cargo test` builds (full dep rebuild after target/ wipes).
All cargo runs now: `-j 1`, `CARGO_PROFILE_TEST_DEBUG=0`, memory watchdog
(/tmp/opencode/test-watchdog.sh, kills build under 1.8GB MemAvailable).

## Review (2026-09-22 16:33 — post-switch run audit)
- ~10h on GLM-5.3-flash: 15 busy maintainer runs, 45 sessions → 31 patterns,
  wiki 51 → 64, ZERO errors since the switch (all 4 morning errors were pre-switch
  agy: 3× structured_output parse + 1× quota).
- Proposer's first GLM fire (12:20) produced a genuine corrigendum: two new
  corroborations show the shipped \\x27 hex-escape technique FAILS — but it was
  muted 7 days by recently_rejected (the morning's denied duplicate).
- Fixed + deployed (0ebeee2): recently_rejected exempts skills with a live
  deployment (patch path open; dedup still blocks identical content). Pinned by
  recently_rejected_does_not_mute_a_deployed_skill. Next proposer tick 18:17
  should re-propose the corrigendum and give the judge its first live GLM fire;
  it will appear in the TUI skills screen if validated.
- Loop quiet since ~10:40 (queue 0, all ticks idle-ok) — expected: ingest cron
  keeps analytics fresh; sessions will sweep in as work happens.
- Outcome of next proposer run post-install.

# Wiki-loop findings verification — 2026-09-22

Scope: audit the supplied P0–P4 report against current source and available local
runtime evidence; no implementation, deployment, or live model invocations.

- [x] Read repository instructions, lessons, and current worktree state.
- [x] Verify P0 control flow and reproduce isolated failure conditions safely.
- [x] Check P1 runtime claims and distinguish historic evidence from current state.
- [x] Verify P2–P4 design/operability/test claims with source references.
- [x] Independently spot-check findings and write tasks/wiki-loop-verification.md.

## Review
Completed against b9a6460 and a read-only live snapshot. All six P0 mechanisms
confirmed with qualified impact; corrected stale P1/P2/P4 claims. See
[tasks/wiki-loop-verification.md](wiki-loop-verification.md) for evidence, isolated
reproductions, limitations, and implementation sequence. No production changes.

# Wiki-loop correctness fixes within WikiSkill — 2026-09-22

Paper: /apps/pdf2md/papers/wikiskills.pdf, §§3.1–3.2.4, Algorithm 1,
Appendix C, and prompts E.2–E.3. Preserve immutable raw traces, persistent wiki
history, incremental consolidation, atomic single-skill proposals, and gated
skill delivery. Memex's counterfactual validation is an existing adaptation,
not the paper's held-out benchmark experiment; do not weaken its threshold.

- [x] Read paper and map fixes to its contracts.
- [x] Fix evidence-free Tier 1 passes and include successful trace evidence.
- [x] Preserve failed-session retry eligibility and scrub maintainer summaries.
- [x] Preserve superseded page history and nonempty merge titles.
- [x] Make proposer truncation UTF-8 safe and scope validation to cited patterns.
- [x] Add proposer observability/breaker, configured-harness doctor checks, and
      quiet cron flags; reduce long validation lock scope safely.
- [x] Address the broken Python example through an auditable correction.
- [x] Add regression coverage for changed orchestration; run memory-guarded
      focused tests, formatting, and clippy.
- [x] Review paper alignment, document remaining design work and verification.

Design boundary: do not delete or truncate archival wiki/audit history to solve
prompt growth. The paper explicitly gives the maintainer full wiki context and
the proposer access to history and on-demand reads. A retrieval/compaction change
requires preserving those semantics, not simply dropping old pages or decisions.


## Implementation review

- Added DLQ listing/requeue with queue mutation locking and preserved newer events.
- Validation now releases delivery/wiki locks while judging, then rechecks the
  proposal, live skill, staged diff, and motivating pattern pages before committing.
- Focused suite: 108 passed, 0 failed. `cargo fmt --check` and
  `cargo clippy -- -D warnings` passed. One pre-existing lib-test warning remains
  in src/watch.rs (unused crossbeam_channel::unbounded import).
- Builds use global jobs=2, MALLOC_ARENA_MAX=1, test debug=0, one process-group
  watchdog with a 1800 MB MemAvailable floor, and the live wiki lock to exclude
  competing scheduled model jobs. No watchdog kill or VM crash occurred.
- One-line Python correction staged as
  prop_1a0cb3d097a_8f2fcc6f2a2542c29fe8; before fails, after passes in Bash and Zsh.
  Tier 0 passed all six gates; Tier 1 passed (2/2 historical sessions would
  improve). Applied through normal delivery as v2; deployed example passes in
  Bash and Zsh, proposal is accepted, and skill-impact.md records the exact diff.
- Remaining design work: full-context/on-demand wiki access and bounded prompt
  views preserving complete archives; semantic consolidation; calibrated judge
  relevance; schema-aware corrective retries. These were not silently substituted
  for the paper's history or validation semantics.

- Installed tested executable to /root/.local/bin/memex-wiki-loop; previous binary
  backed up as memex-wiki-loop.pre-correctness-1790116348.bak. Refreshed cron
  with quiet flags; live doctor passes for all configured opencode roles.
- Installed proposer dry-run succeeded without a model call; ledger now contains
  a completed proposer/dry row. Live cron entries checked for quiet flags.
- Final live checks: Python v2 matches the reviewed one-line correction; latest
  non-retired deployment is v2 (ledger keeps earlier unretired history rows and
  defines current via MAX(version)); accepted proposal and audit entry agree.

# Connect the wiki to working agents (scope, 2026-09-30)

Goal: agents on this box get the relevant wiki pattern at the moment it applies, we
can measure use and effect, and the loop's own evidence doesn't get contaminated.

## Evidence (2026-09-30)
- Pipeline is healthy (1014 sessions → 350 patterns), but agents use none of it.
  The 2 skills are symlinked into all 6 harness skill dirs
  (`link_skill_into_harnesses`, gates.rs:667) and appear in skill listings. Organic
  invocations in 8 days: 0.
- The wiki has no discovery path: no CLAUDE.md/AGENTS.md pointer, no hook, no MCP. No
  harness registers memex MCP.
- Retrieval mechanics are fine. BM25 over pattern text ranked the right page #1 on
  6/6 real error strings. The content is the problem:
  - 349/350 pages are `candidate`, and 202 have a single corroborating session.
  - Median title is 652 chars, and 283 titles exceed 150 chars, with session UUIDs
    embedded.
  - Duplicate families: psql root.crt ×5, python quoting ×5-7, zsh nomatch ×5.
- Paper §5.1/Table 3: giving the inference agent wiki access during *training
  rollouts* cut avg 63.7→60.9, because the knowledge comes from the wiki instead of
  the skills and the traces become less informative. This does not argue against
  deployed use, but injected sessions must be tagged before they feed back into the
  loop. The paper also leaves skill/wiki retrieval and pruning unsolved.

## Approach (decided)
Push via hooks plus pull via CLI, both backed by a new `memex wiki-loop lookup` inside
src/wiki_loop.
- Not skill-only: passive skills already failed (0 uses).
- Not a memex memory source: memory documents are keyed by `provider: SourceKind`
  (a harness enum with about 30 match sites) and ignore frontmatter, so they can't
  filter status/corroboration/scope. `memex search --content memories` also takes
  about 1.05 s.
- `PatternStore::parse_frontmatter`/`catalog()` (patterns.rs:93,240) already give
  status, kind, scope and corroboration. tantivy 0.22 is already a dependency, and
  a 350-doc in-RAM BM25 index takes milliseconds.

## Phase 0: prerequisites
- [ ] Commit the uncommitted 2026-09-22 correctness work. It is 11 files,
      +1411/−267 (gates/patterns/proposer/judge.md/cli/ledger/queue…). The cron
      binary already runs it, but the tree doesn't record it. New work touches the
      same files. **Needs user OK.**
- [ ] Replace the 660 MB debug `memex-wiki-loop` (0.22.0) with a release build of the
      branch, because hook latency matters. Follow lessons.md build safety: jobs=2,
      MALLOC_ARENA_MAX=1, ≥4 GB free, watchdog, no concurrent build or proposer.

## Phase 1: gate what agents see (no archive deletion, per design boundary above)
- [ ] patterns.rs write path: titles must be ≤100 chars with no `(Instance: …)` or
      session metadata. Trim or reject on write, and update the maintainer prompt.
- [ ] One-off retitle backfill for the existing 350 pages. The history and
      corroboration stay intact.
- [ ] Eligibility for lookup (computed at lookup time; no status rewrite):
      - not superseded;
      - corroboration ≥2, OR kind=failure with an exact error-token match;
      - `project:<x>` pages only when cwd resolves to project x.
- [ ] Known v1 limitation: duplicate families can both surface. Real consolidation
      stays in "remaining design work".

## Phase 2: `memex wiki-loop lookup` (+ `show`)
- [ ] Command:
      `lookup <query> [--cwd] [--limit 3] [--min-score] [--session-id] [--harness]
      [--event prompt|tool-failure] [--format text|json]`
      - BM25 over title + Symptom + Fix of eligible pages.
      - Each hit gives: short title, a one-line symptom, the fix, the id, and a
        `wiki-loop show <id>` pointer.
      - Total output is capped at about 600 tokens. It is **empty when nothing clears
        min-score**, which should be the common case.
- [ ] `wiki-loop show <id>` prints the full page (if it doesn't already exist).
- [ ] Ledger table `injections(ts, session_id, harness, event, query_sha, pattern_ids,
      withheld)`. Every lookup that returns hits writes a row. This is the usage
      metric.
- [ ] Holdout using the existing unused `holdout` table: a deterministic
      hash(session_id) puts about 20% of sessions in holdout. For those, lookup is
      computed and logged with withheld=1 but not emitted.
- [ ] Tests: eligibility, scope filter, min-score empty path, output cap, holdout
      determinism. Latency target: p95 <150 ms on the release build.

## Phase 3a: hooks for Claude Code 2.1.285 + Codex 0.159 (verified hook APIs)
- [ ] `memex wiki-loop hook <claude|codex|agy> <prompt|tool-failure>` reads the
      harness stdin JSON, extracts the query (prompt text, or the stderr/error tail of
      a failed tool), calls lookup, and prints the harness's `additionalContext` JSON.
      It **always exits 0 and fails open**: on timeout or any error it prints nothing.
- [ ] Claude Code:
      - `PostToolUse` on Bash, emitting only when exit≠0 or the result is an error.
        First verify whether 2.1.285 has a separate failure event.
      - `UserPromptSubmit` with a higher min-score.
- [ ] Codex:
      - `UserPromptSubmit`, plus `PostToolUse` (verify it fires on non-zero exit).
      - `[features] hooks = true` is already on. Handle the hook trust hash.
- [ ] `wiki-loop init --install-hooks`: an idempotent merge into ~/.claude/settings.json
      and ~/.codex/hooks.json. It preserves the existing herdr SessionStart hooks and
      writes a backup first. **User confirms before touching harness config.**

## Phase 3b: agy 1.2.12 + opencode v2.0.16 (partly unverified APIs)
- [ ] agy: `PreInvocation` → `additionalContext`. The query comes from `transcript_path`
      (last user message or tool error), and the first call needs dedupe. Hooks live
      in ~/.gemini/config/hooks.json, alongside the herdr entry.
- [ ] opencode: a V2 plugin in ~/.config/opencode/plugins/ using
      `ctx.session.hook("context")` and shelling out to the same `hook` command.

## Phase 4: pull fallbacks (cheap)
- [ ] Add a 5-line block to skills/memex-search/SKILL.md: "on an unfamiliar error run
      `memex wiki-loop lookup "<error text>"` first".
- [ ] Add a one-line pointer in /apps/CLAUDE.md and ~/.codex/AGENTS.md, and create
      ~/.config/opencode/AGENTS.md and GEMINI.md. **User-owned files: confirm.**
- [ ] Deferred: a `wiki_lookup` tool in src/mcp.rs, and registering `memex mcp` in the
      harnesses (none do today).

## Phase 5: close the loop
- [ ] Collector/digest: tag sessions that have rows in `injections`, and mark them in
      proposer/judge evidence so wiki-fed wins aren't credited to skills (paper §5.1).
- [ ] `wiki-loop status` adds:
      - injections per week by harness and event, and the top patterns;
      - the withheld count;
      - recurrence of the targeted failure later in the same session, injected vs
        holdout;
      - Skill-tool invocations of /apps/skills skills, from transcripts.

## Success criteria (2 weeks after 3a)
- Injections happen daily, and at least 70% of 20 spot-checked injections are
  relevant.
- Under 5% of prompts get an injection, hook p95 is under 200 ms, and no session is
  blocked or broken by a hook.
- Directional result: targeted-failure recurrence is lower for injected sessions than
  for holdout.

## Open decisions
1. Phase 0 commit of the Sep 22 work: OK?
2. Roll out Claude Code + Codex first (3a), then agy/opencode (3b)?
3. Eligibility: corroboration ≥2 only (148 pages), or also single-session failure
   pages on exact error matches (recommended)?
4. Holdout rate of 20%?

## Build now: `memex wiki search` / `memex wiki show` (user-approved 2026-09-30)
The user named the command `memex wiki search "query"`: a top-level `wiki`, not a
`wiki-loop` subcommand. This slice is pull only. Hooks, the injections ledger and
the holdout stay in the phases above.
- [x] `src/wiki_loop/search.rs`:
      - Build an in-RAM tantivy index (en_stem, the memory_search.rs:889 idiom) over
        each pattern's title (boost 2.0), slug words (boost 1.5) and body.
      - Sanitize the query: non-alphanumerics become spaces, and bare AND/OR/NOT are
        lowercased, so raw error text never fails to parse.
      - Exclude superseded pages unless `--all`.
      - `--project X` keeps pages scoped `global` or `project:X`. `--kind` filters
        failure or success.
- [x] `memex wiki search <QUERY> [--limit 5] [--project] [--kind] [--all]
      [--format text|jsonl|json]`. The default is compact text cards: slug, badges,
      title truncated to 140 chars, Symptom/Fix snippets, and a `show` pointer. JSON
      carries the full fields.
- [x] `memex wiki show <ID|SLUG>` prints the page.
- [x] Wiring: top-level `Commands::Wiki` in src/cli.rs, a help-template line, and
      tests in search.rs.
- [x] Gates (orchestrator only): fmt, clippy -D warnings, focused tests, release
      build. Install to /root/.local/bin/memex after backing it up. Probe with real
      error strings and check latency.
- [x] Point /apps/CLAUDE.md and the Codex AGENTS.md at `memex wiki search`.

### Review (2026-09-30)
- Deviations from spec, all accepted:
  - The whole query is lowercased. tantivy also reserves bare `IN` and `TO`, and
    en_stem lowercases anyway.
  - Superseded hits get a marker under `--all`.
  - `--limit 0` is rejected.
- The first version created directories on a read (via `PatternStore::new`). I
  replaced that with a read-only `wiki_root.join("patterns")`, the same way
  cli.rs:993 and tui.rs:7139 read the wiki.
- Gates:
  - `cargo fmt --check` OK. `cargo clippy -- -D warnings` OK.
  - `cargo test --lib wiki_loop::` 115/115 (7 new).
  - Release build took 2m27s.
  - All ran under a process-group watchdog with an 1800 MB floor. MemAvailable
    never went below 5.9 GB.
- Live probes on the 350-page wiki: 5/5 real error strings rank a correct page #1
  (psql root.crt, python f-string, wrangler EROFS, zsh nomatch, helius ws). Each
  query takes about 0.15 s and 33 MB RSS. Duplicate families show up as #1–#3
  (curation is Phase 1).
- Installed the release build to /root/.local/bin/memex. The previous binary is at
  memex.pre-wiki-search.bak. The cron `memex-wiki-loop` (debug, Sep 22) is untouched.
- Pointers added to /apps/CLAUDE.md, /apps/AGENTS.md (identical copies) and
  ~/.codex/AGENTS.md. /apps is not a git repo, so Codex in a sub-repo never reads
  /apps/AGENTS.md; the global file covers that case.
- Still unwired: opencode (~/.config/opencode/AGENTS.md) and agy (GEMINI.md).
  Hooks (push) remain Phase 3.
