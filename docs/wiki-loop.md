# wiki-loop

`memex wiki-loop` compiles agent execution traces into persistent, compounding knowledge and
staged skill updates. It implements the three-layer loop from
[WikiSkill: Compiling Agent Experience into Persistent Knowledge for Skill Evolution](https://arxiv.org/abs/2608.27454)
(Tang et al., 2026) on top of memex's read-only corpus.

## The three layers

| Layer | Where | Mutability |
| --- | --- | --- |
| **Raw** — execution traces | memex index + `~/.memex/state/analytics.sqlite` | read-only, immutable |
| **Wiki** — compounding knowledge | `~/.memex/wiki/` (global) and `~/.memex/wiki/projects/<name>/` (per project): `patterns/*.md`, `index.md`, `logs.md`, `skill-impact.md` | append and patch; **never rolled back** |
| **Skills** — procedural instructions | `~/.agents/skills/<name>/` (global) and `<projects_root>/<project>/.claude/skills/<name>/` (per project) | versioned, rollback-able |

memex itself is never written to. wiki-loop reads sessions from the analytics store and loads
turn records through `SearchIndex::records_by_session_id`, falling back to re-parsing the raw
transcript when the index lags ingest.

## Per-project stores

Set `projects_root` (e.g. `/apps`) and wiki-loop partitions the loop per project found under
it. Sessions resolve to a project via their `git_root`/`cwd` falling under the root (fallback:
a `repo_project` whose directory exists under the root). Each project gets its own wiki
(`~/.memex/wiki/projects/<name>/`) and its own skills directory
(`<projects_root>/<name>/.claude/skills/`), so knowledge compounds per repo the way the
paper's wiki compounds per benchmark — lessons from one codebase never surface in another.
Sessions that resolve to no project land in the global store at `~/.memex/wiki/`, and
global-scope skills still deploy to `~/.agents/skills/`. With `projects_root` unset, the
entire loop uses the global store.

## Loop order

The stages follow the paper's Algorithm 1:

1. **Collect** — session hooks call `memex wiki-loop enqueue` (enqueue-only, no model calls).
2. **Sample** — the maintainer claims a context-fit batch: sessions that have **ended** and
   cleared the quiet window, stratified per Appendix C into up to **5 failing** traces
   (root-cause analysis) and up to **3 passing** traces (successful-strategy extraction,
   regression prevention), oldest first. Overflow sessions stay queued for the next run.
3. **Maintain** — per project, the Wiki Maintainer sees the **full text of existing pattern
   pages** plus the stratified digest, consolidates failure *and* success patterns, revises
   `index.md`, and appends to `logs.md`. Merges preserve prior sections the model leaves
   empty, union corroboration, and append evidence.
4. **Propose** — per project wiki, the Skill Proposer reads the wiki index and the
   `skill-impact.md` audit trail **first** (so rejected interventions are never re-proposed),
   then corroborated patterns and active skills, and emits at most one **atomic** single-skill
   proposal. A global weekly ceiling bounds proposal fatigue.
5. **Gate** — Tier 0 static hygiene (secret rescan, slug, scope stamp, dedup,
   recently-rejected, referenced-path existence — checked against the contributing projects,
   failing closed), then a Tier 1 counterfactual judge whose assessments are reconciled
   one-to-one against the sessions actually digested.
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
  complexity argue for a single call with the wiki index, the full audit trail, up to 10
  corroborated pattern pages, and the active skill list pre-stuffed; the proposer therefore
  cannot inspect raw traces, only the maintainer's compiled evidence. This is the main
  fidelity gap — revisit if proposals feel uninformed.
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
  or `global` so lessons from one repo do not surface in another. Global-scope proposals get
  the strictest gating (their referenced paths and judge evidence resolve across all
  contributing projects, failing closed when they cannot).

## Commands

```bash
memex wiki-loop enqueue <source> <session_id> [--project <p>] [--ended]
memex wiki-loop run-maintainer [--dry-run]
memex wiki-loop run-proposer [--dry-run]
memex wiki-loop validate <proposal_id> [--skip-tier1]
memex wiki-loop apply <proposal_id>
memex wiki-loop rollback <skill_name>
memex wiki-loop status
memex wiki-loop doctor
```

The maintainer writes the live wiki directly (per-project stores, the global store for
unresolved sessions). There is no staging split: the wiki is append-only by construction and
the human gate sits at skill delivery, where mistakes are reversible.

## Configuration

Optional, at `~/.memex/wiki-loop.toml`. Defaults apply when the file is absent.

```toml
# Paths
wiki_root            = "~/.memex/wiki"
skills_root          = "~/.agents/skills"       # global-scope skills land here
projects_root        = "/apps"                  # optional: per-project wikis + skills
project_skills_subdir = ".claude/skills"        # per-project skills dir, relative to each project
queue_dir            = "~/.local/state/wiki-loop/queue"
state_db             = "~/.local/state/wiki-loop/state.db"
proposals_dir        = "~/.local/state/wiki-loop/proposals"

# Sampling
quiet_minutes  = 20   # a session must be quiet this long after ending
min_turns      = 3    # skip trivial sessions
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

Hooks enqueue; cron runs the loop. Use absolute paths — cron has a minimal PATH.

```cron
*/30 * * * * /usr/local/bin/memex wiki-loop run-maintainer >> ~/.local/state/wiki-loop/maintainer.log 2>&1
17 */6 * * * /usr/local/bin/memex wiki-loop run-proposer   >> ~/.local/state/wiki-loop/proposer.log 2>&1
```

Every mutating subcommand takes a `flock`-based role lock, so overlapping runs exit cleanly.
The maintainer and proposer additionally share a wiki lock, so the proposer never reads a
wiki mid-write. Three consecutive maintainer failures trip a circuit breaker that notifies
instead of burning model budget every tick.

## Operational notes

- **Sessions are compiled once, when complete.** `enqueue` defaults to not-ended; only
  `--ended` plus the quiet window makes a session eligible. A queue entry updated mid-run
  (e.g. a resumed session) is never acked, and a session whose transcript grew after
  compilation is re-compiled for the new turns — the processed marker records the
  `last_event_at` it covers.
- **At-least-once processing.** The queue is acked only after a successful run, only if the
  entry did not change mid-run, and only after the model returned structured output. An
  empty/unparseable maintainer response fails the run and leaves the batch queued.
- **Ingest lag is expected.** A session not yet in analytics, or whose index read returns
  fewer records than the session's message count, is retried with exponential backoff and
  dead-lettered after five attempts rather than dropped.
- **Wiki discovery.** memex only indexes memory markdown under a source's memory root
  (`~/.claude/projects/<project>/memory/**/*.md`); `~/.memex/wiki` is **not** auto-indexed.
  Symlink it into a memory root if you want the wiki searchable from the TUI.
