# wiki-loop: implement glm-review fixes + per-project roots

Review verdict (tasks/glm-review.md): accurate — all 22 findings verified against
src/wiki_loop/ and the paper (arXiv:2608.27454, §3.2.2/§3.2.3/§3.2.4, Alg. 1, App. C,
prompts E.2/E.3). Fix everything P0–P2 plus the cheap P3s, and add per-project
wiki/skills stores under a configurable projects root.

## Wave 1 — parallel subagents (disjoint files)

- [x] A. scrub.rs — F19 secret shapes: Stripe (sk_/rk_/whsec_), unquoted env assigns,
      JWTs, DB connection strings, AWS secret keys (label-gated), Slack webhooks,
      `Authorization: Basic`. Tests per shape + FP sanity.
- [x] B. harness.rs — F20 process-group kill (process_group(0) + kill(-pgid, SIGKILL)).
- [x] C. patterns.rs + prompts/maintainer.md — F4 (create-on-existing-slug promotes to
      merge; quarantine never clobbers), F11 (merge: empty section keeps prior text),
      F18 (frontmatter sanitization: one-line title, scope validation), `kind:
      failure|success` field (F10 substrate), F22 test fixes (slug-fork case).
- [x] D. gates.rs + ledger.rs — F1 (proposal-id validator allowing `_`), F2 (apply
      requires all tier-0 gates, not a nonexistent "tier0" key), F8 (v1 rollback
      restores backup, no remove_dir_all), F9 (next version from MAX over all rows),
      F15 (global-scope fails closed: refs checked against contributing projects, tier1
      judges the union), F16 (judge assessments reconciled against digested sessions),
      F21 (stale `running` runs marked error on open). New gates test module covering
      validate → apply → rollback end to end.

## Wave 2 — core wiring (orchestrator)

- [x] F3 — remove the staging/live split: maintainer writes the live wiki; drop
      `--live` and `wiki_staging` config; fix docs cron.
- [x] F5 — maintainer output: `patterns` required (no serde default); empty patterns +
      empty summary = error, never ack.
- [x] F6 — empty/partial index reads fall back to raw transcript, else nack (retry),
      never silently "ok".
- [x] F7 — ack before mark_processed; `compiled_last_event_at` column so resumed
      sessions recompile their new turns.
- [x] F10 — stratified sampling per paper App. C: ≤5 failing + ≤3 passing sessions per
      run (FIFO); passing digests built for strategy extraction; success patterns.
- [x] F11 — maintainer prompt receives full pattern page bodies (bounded), not a
      one-line catalog.
- [x] F13 — `validate` appends to skill-impact.md on status change.
- [x] F14 — role locks: proposer takes "proposer" + shared "wiki" lock; maintainer
      takes "maintainer" + "wiki"; validate/apply/rollback take "delivery".
- [x] F17 — materialize embedded JSON schemas; pass schema_path where role.json_schema.
- [x] F21 — written counts only successful ops; all-ops-fail run = error (breaker);
      per-pattern provenance (session → patterns it actually evidenced).
- [x] F22 — error markers matched against tool output only (not assistant prose);
      char-consistent truncation.
- [x] F12 — document the one-shot-proposer divergence in docs/wiki-loop.md.

## Per-project stores (user request)

- [x] `projects_root` config (e.g. /apps); sessions resolve to a project via
      git_root/cwd under the root (fallback: repo_project dir exists).
- [x] Wiki per project: `~/.memex/wiki/projects/<name>/`; unresolved sessions keep the
      global store at `~/.memex/wiki/`. Maintainer groups its batch per project and
      runs once per project group.
- [x] Skills per project: applied skills land in
      `<projects_root>/<project>/.claude/skills/` (configurable subdir); global-scope
      skills still go to `~/.agents/skills`. Rollback resolves the dir from the
      deployment's recorded scope.
- [x] Proposer iterates project stores + global (weekly ceiling still global).
- [x] status/doctor show per-project state.

## Wave 3 — verify + ship

- [x] cargo fmt --check, cargo clippy -- -D warnings, cargo test (all green;
      CARGO_TARGET_DIR=/mnt/HC_Volume_106441424/caches/target, jobs capped at 2).
- [x] docs/wiki-loop.md updated (cron without --live, per-project layout, new
      divergences, unredacted-traces-to-models note).
- [x] Commit (no attribution lines, per repo rule).

## Review

All 22 review findings addressed (F1–F22). Verification: `cargo fmt --check` clean,
`cargo clippy --lib -- -D warnings` clean, `cargo test --lib` **1085 passed / 0
failed** (wiki_loop suite grew 32 → 67 tests, including a new gates module covering
validate → apply → rollback → re-apply end to end). Binary smoke-tested
(`memex wiki-loop --help`).

Bonus defects found and fixed beyond the review:

1. **The maintainer prompt had no `{digest}` placeholder at all** — cli.rs's
   `.replace("{digest}", …)` was a no-op, so the model never saw the catalog or the
   trace digest. Every maintenance run ran blind. (Found by the patterns agent; now one
   explicit placeholder + JSON-schema enforcement of the output shape.)
2. `Record` is not `Clone`, so the per-project grouping initially cloned a `&Vec` by
   accident — caught by compile, fixed by consuming the map.

Operational notes:

- To enable per-project stores, set `projects_root = "/apps"` in
  `~/.memex/wiki-loop.toml`; skills then deploy to each project's
  `.claude/skills/` (configurable via `project_skills_subdir`). Unset = previous
  single-store behavior.
- The cache volume filled (31G, 100%) during the final link; cleared
  `target/debug/incremental` (8.3G) — rebuilds are slower once, nothing else affected.
- Known accepted limitations, documented in docs/wiki-loop.md: the proposer remains
  one-shot (no ReAct trace access — declared divergence), merges are section-level
  rather than span-level, and the digest sent to the role models is unredacted by
  design (scrubbing protects the durable files).

