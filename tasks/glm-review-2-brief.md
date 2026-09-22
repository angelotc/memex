# Review brief: wiki-loop UX, docs, and correctness (second pass)

You are reviewing the `wiki-loop` branch of /apps/memex (a Rust CLI/TUI, "memex").
The first review (tasks/glm-review.md, 22 findings) covered the initial port; all of
it was fixed and collapsed into commit `1315e06`. **Your scope is everything after
that: `git diff 1315e06..HEAD`** (current HEAD is `c73d453`).

wiki-loop compiles agent traces into a wiki of failure/success patterns and proposes
versioned skills, implementing the WikiSkill loop (arXiv:2608.27454; local paper text
at /apps/pdf2md/output/wikiskills.md — divergences are documented in docs/wiki-loop.md
and should be *deliberate*, not accidental).

## What landed in your scope

1. **Shared workspace-root stores**: one wiki + one skills base under an optional
   `workspace_root` (e.g. /apps → /apps/wiki, /apps/skills); per-project attribution
   via `project:<name>` scope stamps; diverging pattern scopes widen a proposal to
   `global` under the strictest gates. Per-project store machinery was removed.
2. **Collector sweep** (src/wiki_loop/collect.rs): ended, uncompiled sessions enqueue
   from the analytics store before each maintainer run; `collect_lookback_days`
   bounds it; queue upserts stamp the session's real `last_at`, move forward only,
   and skip identical writes.
3. **`wiki-loop init`**: scaffolds config, creates dirs, installs a marked idempotent
   crontab block (splice logic in cli.rs), runs doctor.
4. **TUI**: a wiki browser (`alt+w` from anywhere in sessions view; plain `w` from
   list focus) and a separate skills screen (`s` from wiki; `w` hops back; Esc exits
   to sessions), sharing a BrowserState; skills render SKILL.md + PURPOSE.md.
5. **Robustness**: `run_role_structured` retries once when a role's output drifts out
   of JSON (agy envelope leaks); `run-maintainer --force` escapes a tripped breaker;
   `normalize_slug` truncates over-long model slugs instead of dropping the op.
6. README.md section + Reference row; docs/wiki-loop.md install/scheduling sections.

## What to review, in priority order

1. **UX of the wiki/skills experience overall.** Walk the surface as a user would:
   `wiki-loop init/status/doctor/run-maintainer --dry-run`, the TUI keybindings and
   screens, empty states, footer hints, terminology consistency (wiki vs patterns vs
   skills vs sessions), discoverability (can a new user find and understand the three
   screens?), friction points, misleading messages. Judge the *flow*, not just
   correctness: install → collecting → browsing wiki → (eventually) reviewing a
   proposal → applying → seeing the skill.
2. **README.md and docs/wiki-loop.md.** Accuracy against the actual code (flags,
   paths, defaults, cron lines, key names), clarity, and gaps a new user would hit.
   Flag anything stale (e.g. per-project-era text) or over-promising.
3. **Correctness of the program.** Read the diff critically: race conditions (queue
   upserts vs claim/ack fingerprints, sweep vs mid-run rewrites), the collector's
   interaction with the processed ledger and `is_processed` semantics, gate logic
   after the store refactor (scope handling, referenced-path checks, judge replay),
   TUI state machine holes (focus modes, return modes, mouse dispatch), and test
   coverage of the new paths. Also sanity-check paper fidelity: where the code
   diverges from WikiSkill, is the divergence documented *and* defensible?

## Rules

- READ-ONLY: do not modify any file. Your output goes to
  **/apps/memex/tasks/glm-review-2.md**.
- Verify every claim against the code before writing it; cite `file:line` for each
  finding. No speculation presented as fact.
- Rank findings P0 (breaks the loop or loses data) → P3 (polish). Start the file with
  a one-paragraph verdict: is this shippable as the wiki/skills experience for this
  workspace?
- You may run read-only commands (git diff/log, grep, cargo metadata) but do NOT run
  cargo build/test (16GB box, builds are coordinated) and do NOT touch the live wiki
  at /apps/wiki or the state db.
