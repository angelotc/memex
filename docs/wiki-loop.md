# wiki-loop

`memex wiki-loop` compiles agent execution traces into persistent, compounding knowledge and
staged skill updates. It implements the three-layer loop from
[WikiSkill: Compiling Agent Experience into Persistent Knowledge for Skill Evolution](https://arxiv.org/abs/2608.27454)
(Tang et al., 2026) on top of memex's read-only corpus.

## The three layers

| Layer | Where | Mutability |
| --- | --- | --- |
| **Raw** — execution traces | memex index + `~/.memex/state/analytics.sqlite` | read-only, immutable |
| **Wiki** — compounding knowledge | one shared wiki (default `~/.memex/wiki/`, or `<workspace_root>/wiki/`): `patterns/*.md`, `index.md`, `logs.md`, `skill-impact.md` | append and patch; **never rolled back** |
| **Skills** — procedural instructions | one shared skills root (default `~/.agents/skills/`, or `<workspace_root>/skills/`) | versioned, rollback-able |

memex itself is never written to. wiki-loop reads sessions from the analytics store and loads
turn records through `SearchIndex::records_by_session_id`, falling back to re-parsing the raw
transcript when the index lags ingest.

## Workspace root

Set `workspace_root` (e.g. `/apps`) and the wiki and skills base defaults become
`<root>/wiki` and `<root>/skills` — one shared knowledge base covering every project
under the root. There are no per-project stores: attribution travels in `project:<name>`
scope stamps on patterns and proposals, and cross-project evidence is exactly what the
fail-closed gates are for (a proposal whose motivating patterns span projects widens to
`global`, where referenced paths and judge evidence must resolve across every
contributing repo). Explicit `wiki_root` / `skills_root` keys still override the
workspace defaults; with neither set, the loop uses `~/.memex/wiki` and `~/.agents/skills`.

## Loop order

The stages follow the paper's Algorithm 1:

1. **Collect** — before each run, a sweep enqueues every session that has ended (been
   quiet for the quiet window), is inside the lookback (`collect_lookback_days`), and is
   not yet compiled through its current `last_at`. memex already centralizes transcripts,
   so collection needs no per-harness hooks; `memex wiki-loop enqueue` remains for
   session-end hooks that want sub-cron latency.
2. **Sample** — the maintainer claims a context-fit batch: sessions that have **ended** and
   cleared the quiet window, stratified per Appendix C into up to **5 failing** traces
   (root-cause analysis) and up to **3 passing** traces (successful-strategy extraction,
   regression prevention), oldest first. Overflow sessions stay queued for the next run.
3. **Maintain** — the Wiki Maintainer sees up to the **30 most recently updated pattern
   pages** (each capped at 2,000 characters, with a 40 KiB total page budget) plus the
   stratified digest, consolidates failure *and* success patterns, revises `index.md`, and
   appends to `logs.md`. Merges preserve prior sections the model leaves empty, union
   corroboration, and append evidence. Older pages can be absent from a run's prompt.
4. **Propose** — the Skill Proposer reads the wiki index and the
   `skill-impact.md` audit trail **first** (so rejected interventions are never re-proposed),
   then corroborated patterns and active skills, and emits at most one **atomic** single-skill
   proposal. A global weekly ceiling bounds proposal fatigue; only proposals that
   survive gating to human review count against it — gate rejections never reached
   the human, so they must not spend the budget.
5. **Gate** — Tier 0 static hygiene (secret rescan, slug, scope stamp, dedup,
   recently-rejected, referenced-path existence — checked against the contributing projects,
   failing closed), then a Tier 1 counterfactual judge over up to `tier1_sessions` complete
   sessions (default 5), within half of `max_chars_per_batch` bytes. Replay starts with
   corroborating sessions from the motivating patterns, then tops up with recent sessions
   from the contributing project(s). The resulting evidence can mix failing and passing
   sessions; Tier 1 does not force Appendix C's maintainer 5/3 split. Passing-session
   strategies are judged too; having no error turns is not itself a pass. Missing, partial,
   unreadable, or otherwise unusable evidence fails closed instead of skipping the judge.
   The judge's assessments are reconciled one-to-one against sessions actually digested.
6. **Apply / roll back** — a human applies a validated proposal; every decision (including
   `validate` rejections) is appended to `skill-impact.md`. Skills revert to the prior
   version — pre-existing hand-authored content is restored from backup, never deleted; the
   wiki does not revert.

### Divergences from the paper (deliberate)

The paper evolves skills against benchmark tasks with trusted rollouts and a single automatic
validation gate. Production traces are untrusted and real tasks are not replayable, so:

- **Human gate.** The paper has no human in the loop. Compiling instructions out of traces
  inverts memex's trust model (traces are evidence, not instructions), so proposals stage for
  human approval rather than auto-applying.
- **Counterfactual judge instead of validation rollouts.** A `SKILL.md` is instructions, not
  code — a repo test suite cannot grade it. Tier 1 asks a judge whether historical sessions
  that hit the failure mode would plausibly have gone better under the candidate skill.
- **One-shot proposer instead of ReAct.** The paper's proposer is a multi-turn ReAct agent
  that reads pattern pages and raw traces on demand via `read_file`. Cost and harness
  complexity argue for a single call with the full wiki index and impact audit, up to 10
  corroborated pattern pages (within a 60 KiB limit), and active skill contents pre-stuffed.
  The proposer cannot inspect raw traces and may not see older wiki pages outside its
  candidate set. On-demand retrieval remains a fidelity gap and a future design direction.
- **Section-level merges instead of span patches.** The paper's maintainer edits pattern
  pages with append/replace/insert_after span operations. Merges here replace whole
  Symptom/Root Cause/Fix sections (the model sees the current page bodies first) and keep
  any section the model leaves empty; evidence always appends.
- **Orchestrator-owned writes.** Agents emit JSON only; wiki-loop validates, scrubs, and
  performs every filesystem write. A compromised maintainer cannot run shell or touch the wiki.
- **Secret scrubbing and quarantine.** The wiki is durable and indexed; traces routinely carry
  credentials. High-risk hits divert the whole pattern to `quarantine/`. Note the boundary:
  scrubbing protects the **files**; the digest piped to the maintainer/proposer/judge models
  is unredacted, because the model must see the failure — run the roles against providers you
  are comfortable sending trace content to.
- **Scope stamping.** memex is multi-project; every pattern and skill carries `project:<name>`
  or `global` so a lesson states where it applies even inside the shared wiki. Diverging
  evidence widens a proposal to `global`, which gets the strictest gating: referenced paths
  and judge evidence must resolve across every contributing project, failing closed when
  they cannot. (The paper's wiki compounds per benchmark; this loop compounds across all
  projects under the workspace root, with scopes as the attribution layer.)

## Correctness and operations

These behaviors follow the paper's persistent-wiki and gating requirements (§§3.2.2–3.2.4).
Appendix C's 5-failing/3-passing stratification applies to maintainer sampling; Tier 1 uses
its own total-session and byte budgets:

- Tier 1 loads up to `tier1_sessions` sessions (default 5) into a digest capped at half of
  `max_chars_per_batch`. It replays corroborating sessions for the motivating patterns
  first, then tops up from recent project history. The selected set may contain a mix of
  failing and passing sessions; unlike maintainer sampling, it has no forced 5/3 split.
  Missing, incomplete, unreadable, or empty evidence fails closed. Passing sessions are
  judged as successful-strategy evidence; having no error turns is not itself a pass.
- If a maintainer pattern operation fails, sessions that supplied that operation's evidence
  remain queued for retry. If provenance is missing, the maintainer retains the whole
  selected batch. Superseding a pattern changes its status and replacement metadata while
  preserving its complete page body and accumulated evidence.
- Proposer runs appear in the run ledger and status output and have their own three-failure
  breaker. `run-proposer --force` permits one attempt after the cause is fixed; a failed
  forced attempt leaves the breaker tripped. The doctor checks the configured executable
  for each maintainer, proposer, and judge role.
- Validation releases the delivery lock while the Tier 1 judge runs, then reacquires it and
  checks that the proposal, deployed skill, and motivating pattern pages are still current before saving a
  verdict. This keeps a long judge call from blocking apply or rollback while preventing a
  stale validation result from being committed.

The proposer receives the full `index.md` and full `skill-impact.md` audit trail on each
run; neither is currently compacted or bounded, so both can grow the prompt over time. The
maintainer sees only the freshest 30 pattern pages, while the paper's proposer can inspect
wiki pages and raw traces on demand (§3.2.3). These are current context limits, not a claim
that history compaction or on-demand retrieval has been implemented; faithful retrieval is
future work.

## Install

The loop is optional behavior layered on top of memex; it never touches the index or
analytics store except read-only. One command scaffolds everything (no model calls):

```bash
memex wiki-loop init --workspace /apps --install-cron
```

That writes `~/.memex/wiki-loop.toml` when absent (everything in it is optional —
defaults live in the binary), creates the wiki and skills directories, installs the
marked crontab block (maintainer every 5 min, proposer every 6 h, and an incremental
`memex index` every 10 min so the analytics store stays fresh even when no memex
command runs — the loop only reads it, something has to feed it), running the exact
binary `init` was invoked through. Re-running is
idempotent; `--install-cron` replaces its own marked block without touching the rest of
the crontab. Then compile what already happened and watch it go:

```bash
memex wiki-loop run-maintainer   # sweep + compile ended sessions
memex wiki-loop status           # queue depth, wiki growth, health
```

In the TUI, press `alt+w` for the wiki browser (plain `w` once the results list has
focus) and `s` from the wiki screen for skills. The skills screen is the proposal
review surface: staged proposals (pending/validated) are listed above applied skills
with a status badge and full preview (metadata, SKILL.md, diff, purpose) — `a` applies
a validated proposal (same Tier-3 path as `memex wiki-loop apply`), `d` denies it
(status → rejected, audited in `skill-impact.md`, no files deployed).

Applying also **propagates** the skill: `skills_root` stays the canonical store, and
the applied skill is symlinked into every harness discovery root (`~/.claude/skills`,
`~/.codex/skills`, `~/.gemini/skills`, `~/.gemini/config/skills`, `~/.agents/skills`,
`~/.config/opencode/skills` — override with the `harness_skill_roots` key). A root
that doesn't exist is created; a hand-authored skill of the same name is never
clobbered (reported as skipped). Rollback removes the links it created.

## Commands

```bash
memex wiki-loop init [--workspace <dir>] [--install-cron] [--force]
memex wiki-loop enqueue <source> <session_id> [--project <p>] [--ended]
memex wiki-loop run-maintainer [--dry-run] [--force]
memex wiki-loop run-proposer [--dry-run] [--force]
memex wiki-loop validate <proposal_id> [--skip-tier1]
memex wiki-loop apply <proposal_id>
memex wiki-loop rollback <skill_name>
memex wiki-loop dead-letters
memex wiki-loop requeue <source> <session_id>
memex wiki-loop status
memex wiki-loop doctor
```

The maintainer writes the live wiki directly. There is no staging split: the wiki is
append-only by construction and the human gate sits at skill delivery, where mistakes are
reversible.

## Searching the wiki

`memex wiki` is the read side: agents look up known failure modes and fixes before
re-debugging them.

```bash
memex wiki search "psql root.crt no such file" [--limit 5] [--project <p>] [--kind failure|success] [--all] [--format text|jsonl|json]
memex wiki show psql-missing-root-crt-system-ca   # by slug, file name, or pat_ id
```

`search` ranks `patterns/*.md` by BM25 over title, slug, and body. Raw error text is
safe to paste; punctuation and operators are neutralized. Superseded pages are hidden
unless `--all` is passed, and `--project` keeps global patterns plus those scoped to
that project. `show` prints the whole page, frontmatter included.

## Configuration

Optional, at `~/.memex/wiki-loop.toml`. Defaults apply when the file is absent.
The complete annotated reference — every parameter with its default, plus a
1M-context profile — is [wiki-loop.example.toml](wiki-loop.example.toml); a unit
test asserts it stays in sync with the binary's defaults. The essentials:

```toml
# Paths
workspace_root       = "/apps"                  # optional: wiki + skills base (→ /apps/wiki, /apps/skills)
wiki_root            = "~/.memex/wiki"          # default without workspace_root; explicit key always wins
skills_root          = "~/.agents/skills"       # default without workspace_root; explicit key always wins
queue_dir            = "~/.local/state/wiki-loop/queue"
state_db             = "~/.local/state/wiki-loop/state.db"
proposals_dir        = "~/.local/state/wiki-loop/proposals"

# Sampling
quiet_minutes         = 20   # a session must be quiet this long after ending
collect_lookback_days = 7    # sweep horizon for ended sessions (0 = all history)
min_turns             = 3    # skip trivial sessions
max_batch_size = 10   # sessions claimed per maintainer run, before stratification

# Stratification (paper Appendix C: up to 8 traces = 5 failing + 3 passing)
max_failing_sessions = 5
max_passing_sessions = 3

# Budgets and ceilings
max_chars_per_session     = 24576
max_chars_per_batch       = 163840
subprocess_timeout_secs   = 1200
max_proposals_per_week    = 3   # precision over recall: a quiet proposer is a trusted one
min_pattern_corroboration = 2   # distinct sessions before a pattern can motivate a skill
tier1_sessions            = 5

[maintainer]
command = ["agy", "--output-format", "json", "--model", "{model}", "--effort", "{effort}"]
model = "gemini-3.8-flash"
effort = "low"
json_schema = true

[proposer]
command = ["claude", "-p", "--output-format", "json", "--model", "{model}"]
model = "opus"
```

Prompts are piped on stdin; `{model}`, `{effort}`, and `{schema}` are substituted per argument.
Output schemas are embedded in the binary and materialized per run; roles with
`json_schema = true` are invoked with `--json-schema <path>` so malformed output is rejected
by the harness, not silently treated as "nothing to record".

## Scheduling

`init --install-cron` owns the schedule. To manage crontabs by hand instead (absolute
paths — cron has a minimal PATH), the equivalent block is:

```cron
# BEGIN memex wiki-loop
*/5 * * * * /usr/local/bin/memex --no-update-check --non-interactive wiki-loop run-maintainer >> ~/.local/state/wiki-loop/maintainer.log 2>&1
17 */6 * * * /usr/local/bin/memex --no-update-check --non-interactive wiki-loop run-proposer >> ~/.local/state/wiki-loop/proposer.log 2>&1
*/10 * * * * /usr/local/bin/memex --no-update-check --non-interactive index >> ~/.local/state/wiki-loop/index.log 2>&1
# END memex wiki-loop
```

Every mutating subcommand takes a `flock`-based role lock, so overlapping runs exit cleanly.
The maintainer and proposer additionally share a wiki lock, so the proposer never reads a
wiki mid-write. Three consecutive failures trip each role's circuit breaker. `--force`
allows one recovery attempt, and scheduled runs resume automatically after a successful run.

## Operational notes

- **Sessions are compiled once, when complete.** `enqueue` defaults to not-ended; only
  `--ended` plus the quiet window makes a session eligible. A queue entry updated mid-run
  (e.g. a resumed session) is never acked, and a session whose transcript grew after
  compilation is re-compiled for the new turns — the processed marker records the
  `last_event_at` it covers.
- **At-least-once processing.** A session is acked only after its updates persist, only if the
  entry did not change mid-run, and only after the model returned structured output. An
  empty/unparseable maintainer response fails the run and leaves the batch queued.
- **Partial maintainer failures.** A failed pattern operation leaves its evidence sessions
  queued even when other operations in that run succeeded; missing evidence provenance
  leaves the full batch queued for retry.
- **Ingest lag is expected.** A session not yet in analytics, or whose index read returns
  fewer records than the session's message count, is retried with exponential backoff and
  dead-lettered after five attempts rather than dropped.
- **Dead-letter recovery.** `dead-letters` lists parked sessions and their last errors.
  After fixing the cause, `requeue <source> <session_id>` restores one entry to the live
  queue, resets its attempt count and retry delay, and removes its dead-letter record.
- **Wiki discovery.** memex only indexes memory markdown under a source's memory root
  (`~/.claude/projects/<project>/memory/**/*.md`); the wiki directory is **not** auto-indexed.
  The TUI's wiki browser reads it directly (press `r` in it to refresh after a run); symlink it into a memory root as well if
  you also want full-text search over it.
