# Review brief: wiki-loop implementation vs. the WikiSkill paper

You are reviewing a Rust implementation for **fidelity to the source paper** and for
**correctness**. Be adversarial and specific. Cite `file:line`. Do not be agreeable —
your value here is finding what is wrong or misaligned.

## Materials

- **Paper**: `/apps/pdf2md/papers/wikiskills.pdf` (WikiSkill: Compiling Agent Experience
  into Persistent Knowledge for Skill Evolution, arXiv:2608.27454, Tang et al. 2026).
  A digested summary is at `/apps/pdf2md/output/PAPERS_SUMMARY.md` (search for "WikiSkill").
  **Read the paper itself** — especially Section 3 (the framework) and Algorithm 1 in the
  appendix. Do not rely only on the summary.
- **Implementation**: branch `wiki-loop` in `/apps/memex`, all under `src/wiki_loop/`
  (13 modules + `prompts/`). Entry point is `src/wiki_loop/cli.rs`.
- **Design doc**: `/apps/memex/docs/wiki-loop.md` — states the intended divergences.
- **Prior review of an earlier Python prototype**: `/tmp/glm_review.md` (the addendum at
  the end lists deviations this Rust version was written to fix — verify they *are* fixed).

## What to check

### 1. Algorithm 1 order of operations
Walk the paper's loop step by step and map each step onto the code. Confirm or refute:
- Maintainer runs **before** Proposer, consuming W_{k-1} and a sampled trace subset.
- Maintainer edits the wiki **incrementally / patch-based**, and revises `index.md` and
  appends `logs.md` each iteration.
- Proposer reads the **wiki index and `skill-impact.md` first**, and never re-proposes a
  rejected intervention.
- A proposal is **atomic**: exactly one skill created or patched.
- The accept/reject decision is appended to `skill-impact.md` programmatically.
- **The wiki is never rolled back**, regardless of the skill decision. Only skills revert.

For each: state MATCHES / DEVIATES / MISSING with file:line evidence.

### 2. Which divergences are legitimate vs. accidental
`docs/wiki-loop.md` claims four deliberate divergences (human gate, counterfactual judge
instead of validation rollouts, orchestrator-owned writes, scrubbing + scope stamping).
For each: is it actually justified for production traces, and is it *implemented* the way
the doc claims? Separately, flag any divergence that is **not** documented — i.e. places
where the code silently departs from the paper.

### 3. Correctness review of the Rust
Focus where bugs would be silent and costly:
- `patterns.rs` — the merge path. Does a merge genuinely preserve prior corroboration,
  `created`, and prior evidence? Can a slug/id mismatch fork a duplicate page? Is
  `parse_frontmatter` robust to what `render` writes (round-trip), including edge cases?
- `queue.rs` — can a session be compiled from a partial trace? Can an entry be lost or
  processed twice? Is the ack-if-unchanged fingerprint sound?
- `gates.rs` / `proposer.rs` — can an unvalidated proposal reach `apply`? Is rollback
  correct at version 1 and at version N? Any path where the wiki gets mutated by gating?
- `harness.rs` — subprocess timeout, pipe deadlock, JSON extraction from model envelopes.
- `scrub.rs` — what realistic secret shapes slip through into a durable, indexed store?
- Concurrency: the flock role lock, and cron overlap.

### 4. The prompts
`src/wiki_loop/prompts/*.md` are the actual contract with the models. Do they faithfully
instruct the paper's behavior? Would a mediocre model produce output that the Rust side
mis-parses or that violates an invariant?

## Output

Write your review to `/apps/memex/tasks/glm-review.md`, structured as:

1. **Verdict** (2-4 sentences): is this a faithful implementation, and is it safe to run?
2. **Paper fidelity table**: each Algorithm 1 step → MATCHES / DEVIATES / MISSING + evidence.
3. **Findings**, ordered by severity. For each: file:line, what breaks, and a concrete
   failure scenario. Separate *correctness bugs* from *paper-fidelity gaps* from *nits*.
4. **What you could not verify** and why.

Be concrete. "Looks good" without evidence is not useful. If you think a design decision
is wrong, argue it, do not hedge.
