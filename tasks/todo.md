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

## Review (fill after done)
- Outcome of next proposer run post-install.
