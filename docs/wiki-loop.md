# wiki-loop

`memex wiki-loop` compiles agent execution traces into persistent, compounding knowledge and
staged skill updates. It implements the three-layer loop from
[WikiSkill: Compiling Agent Experience into Persistent Knowledge for Skill Evolution](https://arxiv.org/abs/2608.27454)
(Tang et al., 2026) on top of memex's read-only corpus.

## The three layers

| Layer | Where | Mutability |
| --- | --- | --- |
| **Raw** — execution traces | memex index + `~/.memex/state/analytics.sqlite` | read-only, immutable |
| **Wiki** — compounding knowledge | `~/.memex/wiki/` (`patterns/*.md`, `index.md`, `logs.md`, `skill-impact.md`) | append and patch; **never rolled back** |
| **Skills** — procedural instructions | `~/.agents/skills/<name>/SKILL.md` | versioned, rollback-able |

memex itself is never written to. wiki-loop reads sessions from the analytics store and loads
turn records through `SearchIndex::records_by_session_id`, falling back to re-parsing the raw
transcript when the index lags ingest.

## Loop order

The stages follow the paper's Algorithm 1:

1. **Collect** — session hooks call `memex wiki-loop enqueue` (enqueue-only, no model calls).
2. **Sample** — the maintainer claims a context-fit batch: sessions that have **ended** and
   cleared the quiet window, capped by `max_batch_size` and the digest char budgets.
3. **Maintain** — the Wiki Maintainer consolidates error traces into pattern pages, revises
   `index.md`, and appends to `logs.md`.
4. **Propose** — the Skill Proposer reads the wiki index and the `skill-impact.md` audit trail
   **first** (so rejected interventions are never re-proposed), then corroborated patterns and
   active skills, and emits at most one **atomic** single-skill proposal.
5. **Gate** — Tier 0 static hygiene, then a Tier 1 counterfactual judge.
6. **Apply / roll back** — a human applies a validated proposal; every decision is appended to
   `skill-impact.md`. Skills revert; the wiki does not.

### Divergences from the paper (deliberate)

The paper evolves skills against benchmark tasks with trusted rollouts and a single automatic
validation gate. Production traces are untrusted and real tasks are not replayable, so:

- **Human gate.** The paper has no human in the loop. Compiling instructions out of traces
  inverts memex's trust model (traces are evidence, not instructions), so proposals stage for
  human approval rather than auto-applying.
- **Counterfactual judge instead of validation rollouts.** A `SKILL.md` is instructions, not
  code — a repo test suite cannot grade it. Tier 1 asks a judge whether historical sessions
  that hit the failure mode would plausibly have gone better under the candidate skill.
- **Orchestrator-owned writes.** Agents emit JSON only; wiki-loop validates, scrubs, and
  performs every filesystem write. A compromised maintainer cannot run shell or touch the wiki.
- **Secret scrubbing and quarantine.** The wiki is durable and indexed; traces routinely carry
  credentials. High-risk hits divert the whole pattern to `quarantine/`.
- **Scope stamping.** memex is multi-project; every pattern and skill carries
  `project:<name>` or `global` so lessons from one repo do not surface in another.

## Commands

```bash
memex wiki-loop enqueue <source> <session_id> [--project <p>] [--ended]
memex wiki-loop run-maintainer [--dry-run] [--live]
memex wiki-loop run-proposer [--dry-run]
memex wiki-loop validate <proposal_id> [--skip-tier1]
memex wiki-loop apply <proposal_id>
memex wiki-loop rollback <skill_name>
memex wiki-loop status
memex wiki-loop doctor
```

`run-maintainer` writes to a staging wiki by default; pass `--live` to write
`~/.memex/wiki` once you trust the output.

## Configuration

Optional, at `~/.memex/wiki-loop.toml`. Defaults apply when the file is absent.

```toml
# Paths
wiki_root     = "~/.memex/wiki"
wiki_staging  = "~/.local/state/wiki-loop/wiki-staging"
skills_root   = "~/.agents/skills"
queue_dir     = "~/.local/state/wiki-loop/queue"
state_db      = "~/.local/state/wiki-loop/state.db"
proposals_dir = "~/.local/state/wiki-loop/proposals"

# Sampling
quiet_minutes  = 20   # a session must be quiet this long after ending
min_turns      = 3    # skip trivial sessions
max_batch_size = 10

# Budgets and ceilings
max_chars_per_session     = 24576
max_chars_per_batch       = 163840
subprocess_timeout_secs   = 1200
max_proposals_per_week    = 3   # precision over recall: a quiet proposer is a trusted one
min_pattern_corroboration = 2   # distinct sessions before a pattern can motivate a skill
tier1_sessions            = 5

[maintainer]
command = ["agy", "-p", "--output-format", "json", "--model", "{model}", "--effort", "{effort}"]
model = "gemini-3.8-flash"
effort = "low"
json_schema = true

[proposer]
command = ["claude", "-p", "--output-format", "json", "--model", "{model}"]
model = "opus"
```

Prompts are piped on stdin; `{model}`, `{effort}`, and `{schema}` are substituted per argument.

## Scheduling

Hooks enqueue; cron runs the maintainer. Use absolute paths — cron has a minimal PATH.

```cron
*/30 * * * * /usr/local/bin/memex wiki-loop run-maintainer >> ~/.local/state/wiki-loop/maintainer.log 2>&1
17 */6 * * * /usr/local/bin/memex wiki-loop run-proposer   >> ~/.local/state/wiki-loop/proposer.log 2>&1
```

A `flock`-based role lock makes overlapping runs exit cleanly, and three consecutive maintainer
failures trip a circuit breaker that notifies instead of burning model budget every tick.

## Operational notes

- **Sessions are compiled once, when complete.** `enqueue` defaults to not-ended; only
  `--ended` plus the quiet window makes a session eligible, so partial traces are never
  compiled and then permanently skipped.
- **At-least-once processing.** The queue is acked only after a successful run, and only if
  the entry did not change mid-run.
- **Ingest lag is expected.** A session not yet in analytics is retried with exponential
  backoff and dead-lettered after five attempts rather than dropped.
- **Wiki discovery.** memex only indexes memory markdown under a source's memory root
  (`~/.claude/projects/<project>/memory/**/*.md`); `~/.memex/wiki` is **not** auto-indexed.
  Symlink it into a memory root if you want the wiki searchable from the TUI.
