# wiki-loop — Rust port into memex (branch: wiki-loop)

Extend memex with a `memex wiki-loop` subcommand implementing the WikiSkill
(arXiv:2608.27454) three-layer loop in Rust. Fixes every deviation found reviewing the
earlier Python prototype (/tmp/glm_review.md addendum).

## Plan

- [x] 1. Recon memex internals (CLI plumbing, config/Paths, Record parsing, analytics read
      API, flock precedent, house style)
- [x] 2. Module skeleton: src/wiki_loop/{mod,cli,config,queue,digest,scrub,harness,patterns,
      ledger,lock,notify,proposer,gates}.rs + prompts via include_str!
- [x] 3. Config: ~/.memex/wiki-loop.toml, corrected paths (analytics.sqlite, wiki root,
      staging), limits (budgets, timeout, max_proposals_per_week)
- [x] 4. Queue: atomic upsert enqueue (ended defaults FALSE), claim = ended AND quiet,
      nack/backoff/DLQ, ack-only-if-unchanged
- [x] 5. Maintainer: in-process trace read (analytics RO + index + raw JSONL fallback),
      error-turn digest w/ budgets, existing catalog, orchestrator-owned writes
- [x] 6. Patterns: PATCH-BASED merge (union corroboration, keep created, preserve evidence),
      index.md render, logs.md append per run, quarantine dir, superseded_by
- [x] 7. Ledger: state.db (processed/runs/proposals/deployment/holdout), pattern_ids recorded
- [x] 8. Proposer: inputs = index.md + skill-impact.md FIRST, then corroborated patterns +
      skills; atomic single-skill proposal dir (proposal.json, SKILL.md, PURPOSE.md, skill.diff)
- [x] 9. Gates: Tier 0 (secret rescan, name/scope, dedup, recently-rejected, referenced-path
      existence) + Tier 1 counterfactual judge; apply/rollback write skill-impact.md;
      wiki NEVER rolled back
- [x] 10. notify (herdr notification show), flock role lock, status/doctor, circuit breaker
- [x] 11. docs/wiki-loop.md
- [ ] 12. Tests green; cargo fmt --check + clippy -D warnings
- [ ] 13. Commit on wiki-loop branch
- [ ] 14. GLM 5.3 herdr pane (ccs glm) thorough review vs paper; address findings

## Paper-fidelity checklist (Algorithm 1)

- [x] order: traces → sample → maintainer(W_{k-1}) → proposer → gate → apply/revert
- [x] maintainer patches the wiki incrementally (create / merge / supersede), never overwrites
- [x] maintainer revises index.md and appends logs.md each run
- [x] proposer reads index + skill-impact.md first; never re-proposes a rejected intervention
- [x] proposal is atomic: exactly one skill created or patched
- [x] accept/reject decision appended to skill-impact.md programmatically
- [x] wiki is never rolled back; only skills revert

## Deliberate divergences from the paper (documented in docs/wiki-loop.md)

- human approval gate (paper is fully automatic) — traces are untrusted in production
- Tier-1 counterfactual judge replaces validation rollouts — real tasks are not replayable
- orchestrator owns all writes; agents emit JSON only — prompt-injection containment
- secret scrubbing + quarantine; scope stamping — durable multi-project store
