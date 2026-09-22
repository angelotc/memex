# Review 2: wiki-loop UX, docs, and correctness (`1315e06..c73d453`)

**Verdict.** Shippable as the wiki/skills experience for this workspace — it already runs
live here, and I found no data-loss or wiki-corruption path in the diff. The install story
(`init --workspace --install-cron`), the collector sweep, the idempotent queue upserts,
and the two TUI browser screens are solid and well-tested where it matters most. Two P2
semantics bugs deserve near-term fixes: a `--dry-run` silently resets the maintainer
circuit breaker (re-arming a failing cron), and the collector sweep resurrects
dead-lettered sessions with a fresh retry counter, defeating the DLQ. Everything else is
P3 polish: a `w`-keybinding story that is wrong in the exact state a new user first sees,
a few stale doc lines, and some ordering/UX roughness in the queue and browsers.

Scope reviewed: full diff of `1315e06..HEAD` (14 files, +1764/−86), with full-file reads
of cli.rs, queue.rs, collect.rs, config.rs, ledger.rs, ingest.rs, harness.rs, the TUI
browser code, README.md, docs/wiki-loop.md, and the local paper text. Read-only; no
builds run.

---

## P2 — real bugs, bounded blast radius

### P2-1. `--dry-run` resets the maintainer circuit breaker

`run_maintainer` skips the breaker check when `dry_run` (src/wiki_loop/cli.rs:181), then
unconditionally records a run (start_run, cli.rs:191). The dry-run exit path finishes
that run as `"ok"` (cli.rs:356-362), and `consecutive_failures` stops counting at the
first `"ok"` row (src/wiki_loop/ledger.rs:213-216). The empty-queue early return writes
`"ok"` the same way (cli.rs:230-233).

So the natural operator reflex when the breaker is tripped — run `--dry-run` to see what
is wrong — silently un-trips it. The next three cron ticks then invoke the still-broken
maintainer (exactly the model spend the breaker exists to cap) before it re-trips. A dry
run proves the prompt builds; it proves nothing about the failing model call. A forced run
resetting the breaker on success is fine (explicit operator intent); a dry run doing so is
not.

Fix: record dry runs under a distinct status that `consecutive_failures` treats as
transparent (or skip `start_run`/`finish_run` for dry runs entirely).

### P2-2. The collector sweep defeats the DLQ

A session that nacks five times is moved to `.dlq` and leaves the queue
(src/wiki_loop/queue.rs:219-226). The sweep, however, consults only
`ledger.is_processed` (src/wiki_loop/collect.rs:41) — and a dead-lettered session was
never marked processed (`mark_processed` is only reached on ack paths, cli.rs:264 and
cli.rs:501; nack paths never touch the ledger). On the next sweep (≤30 min later),
`enqueue_event` finds no existing entry and writes a fresh one with `attempts = 0`
(queue.rs:102, 118).

Consequences: a permanently-poison session (e.g. a partial trace that never indexes
completely) cycles retry×5 → DLQ → resurrect indefinitely within the lookback window;
the DLQ stops being a terminal, human-visible signal and `status`'s dead-letter count
under-reports the problem (the session also sits in pending forever). Cost is bounded —
nacks happen pre-model, and resurrection stops once the session ages past
`collect_lookback_days` — but docs/wiki-loop.md:211-213 ("dead-lettered after five
attempts rather than dropped") now overstates the guarantee. Fix: have the sweep skip
sessions with a DLQ entry (or have `enqueue_event` refuse/prefer-DLQ when one exists).

---

## P3 — polish, accuracy, and UX friction

### P3-1. `--dry-run` is not actually side-effect-free, and says it is

The `run-maintainer` help promises "do not invoke the maintainer model **or write**"
(cli.rs:48-50), and the dry-run output says "queue left untouched" (cli.rs:359). But
`execute_maintainer` runs the collector sweep — which writes up to 500 queue entries —
*before* the dry-run branch (cli.rs:225 vs 356), and the ledger run rows are always
written (cli.rs:191, 358). On a machine where the queue was empty, a dry run visibly
populates it. The writes are idempotent and harmless; the contract is what's wrong.
Either skip the sweep when `dry_run`, or reword both strings.

### P3-2. The `w` guidance is wrong in the exact state a new user first sees

The TUI starts on Home with `focus: Query` (src/tui.rs:1086). There, plain `w` is query
text (tui.rs:3391-3394); only `alt+w` opens the wiki from the search box (tui.rs:3367).
Plain `w` works from list/browse focus (tui.rs:3269, 3447) — and the in-TUI footers
advertise it correctly per state ("alt+w wiki" on home, "w wiki" in browse). But the
`init` next-steps hint (cli.rs:879: "memex tui, then `w`"), README.md:215, and
docs/wiki-loop.md:117 all tell the user to press `w` — which, followed literally from a
fresh `memex tui`, types a `w` into the search box. Say "alt+w" (or "↓ then w").

### P3-3. `alt+w` is not honored in sessions-view query/find boxes

baad427 made the chord "safe from the search box," but only on Home. In Split/List/Detail
views the Query and Find handlers match `Char(ch)` with any non-CONTROL modifier
(tui.rs:3100-3111 and 3144-3147), so `alt+w` while typing inserts a literal `w`. The
advertised escape hatch works on one screen and corrupts text on the others. Add the ALT
arm to both text-entry branches, or filter ALT-modified chars from text insertion.

### P3-4. Claim order is filename order, not "oldest first"

`claim` sorts entry paths lexicographically and takes the first eligible
`max_batch_size` (queue.rs:161-189, `paths.sort()` at 170); the stratified selection
sorts by `enqueued_at` only *within* the claimed batch (cli.rs:307-308). The comment
"oldest first; overflow … next run's head of the line" (cli.rs:305-306) and docs step 2
(wiki-loop.md:40-43) promise more than the code delivers: which 10 sessions get compiled
first is effectively filename-arbitrary. Eventually everything drains, so this is
comment/doc accuracy plus mild fairness — sort claimed paths by `last_event_at` if the
oldest-first property is meant.

### P3-5. `SWEEP_LIMIT` applies before the `min_turns` filter

The SQL caps discovery at `LIMIT 500` with only `message_count >= 1`
(src/wiki_loop/ingest.rs:117-118); the `< min_turns` skip happens afterwards in Rust
(collect.rs:36-40). Trivial sessions (plentiful on a box that runs one-shot `agy` calls)
consume sweep slots, shrinking the effective discovery window. Combined with the lookback:
sessions beyond the cap are discovered only as older ones compile out, and any session
still undiscovered when it ages past `collect_lookback_days` is silently never compiled —
consistent with the config's stated intent of bounding the compile bill
(config.rs:80-83), but the interaction is non-obvious. Push `message_count >= ?` into the
SQL so the limit counts only compilable sessions.

### P3-6. Stale docs

- wiki-loop.md:124: the Commands block still shows `run-maintainer [--dry-run]`; `[--force]`
  exists (cli.rs:51-54) and is the breaker-escape story — it should be advertised.
- wiki-loop.md:216: "The TUI's wiki browser **(below)**" — nothing about the browser
  follows; the file ends there.
- cli.rs:851: init template comment "Roles default to `agy`" — only the maintainer does;
  proposer and judge default to `claude`/opus (config.rs:136-138).

### P3-7. `init --workspace` is silently ignored when the config exists

cli.rs:832-833 prints "[ok] config already present" and drops the requested workspace
root — the flag only takes effect on first write or with `--force` (which rewrites the
whole file). A user running `init --workspace /apps` against an existing default config
gets a wiki at `~/.memex/wiki` while believing it lives at `/apps/wiki`. Also cosmetic:
"[ok] created:" prints for directories that already existed (cli.rs:867-870).

### P3-8. The proposal-review leg has no listing surface

Discovery depends entirely on the notification, which is good when seen — it carries the
id and the exact `validate`/`apply` commands (proposer.rs:258-267). But `status` prints
only the proposals directory and the weekly count (cli.rs:713-720); no subcommand lists
staged proposal ids or their gate states, and the TUI skills screen shows only applied
skills. A missed notification means `ls` on a state dir. Cheapest fix: have `status` list
pending/validated proposals from the ledger.

### P3-9. `splice_cron_block` can swallow user lines from a malformed crontab

If a crontab ever contains a BEGIN marker without its END (manual edit, interrupted
write), the append path (cli.rs:944-951) adds a second block; a later `init` then splices
from the first BEGIN to the *first* END (cli.rs:936-943), deleting everything in between —
including unrelated user entries. Low likelihood, but the failure mode is destroying the
user's crontab content. Only splice when the markers form a well-formed block, else bail
and tell the user to fix the markers.

### P3-10. Browser polish

- Entries load only on entry/hop (`populate_browser` from `enter_wiki`/`hop_*`,
  tui.rs:2700-2728); there is no refresh key, so sitting in the screen while a maintainer
  runs elsewhere shows stale content (the undocumented w→s→w dance reloads).
- The wiki list is filename-ordered with no updated-at or count signal; "what changed
  since yesterday" means opening `logs.md`.
- `status` prints the "proposals:" label twice in a row (cli.rs:713, 716-720).

---

## What's good (verified)

- **Queue upsert semantics are right.** Forward-only `last_event_at`, sticky `ended`,
  preserved retry bookkeeping, and the fingerprint no-op skip (queue.rs:93-131) with tests
  pinning each property (queue.rs:301-356). The resumed-session → re-compile flow matches
  the ledger's compiled-through-`last_event_at` contract (ledger.rs:110-125).
- **Optimistic concurrency holds where it matters.** `claim` is read-only;
  `ack_if_unchanged` fingerprints the mutable fields (queue.rs:196-209, 249-257), so a
  hook enqueued mid-run blocks the ack and the session re-compiles. The remaining
  sweep-vs-hook last-writer-wins window self-heals via the next sweep's `max()` stamp.
- **`run_role_structured` retry is well-aimed.** One retry for shape drift only; harness
  errors (spawn/timeout/exit) are not retried; the final error names the envelope status
  and keys (harness.rs:42-77), and `is_leaked_envelope` matches exactly the shapes
  `unwrap_envelope` can pass through (harness.rs:223-237). Tests cover the retry, the
  persistent-failure message, and both envelope shapes.
- **`normalize_slug`** sanitizes and truncates on a word boundary with a
  collision-safe bail, and both the op and the scrubbed copy are pinned to the
  normalized slug before any path is built (patterns.rs:267-287).
- **The browsers are consistent citizens.** Focus/mouse behavior mirrors the sessions
  view (including wheel-steals-focus), the return-mode chain across w↔s hops is correct
  and tested, narrow terminals stack, content is capped at 200k chars with a visible
  truncation note, and empty states name the exact command to run next.
  Quarantined patterns stay hidden consistently (subdir excluded from both the TUI
  loader and `status`'s count).
- **Docs are largely accurate.** The workspace-root rebasing, explicit-key precedence,
  defaults, cron lines, and log paths all check out against config.rs and
  `install_cron_schedule`; the divergence list is honest (including the
  unredacted-digest boundary).

## Test coverage of the new paths

Good: queue upsert semantics, cron splice, envelope retry, slug normalization, TUI
loaders/render/return-modes/hops. Gaps: the dry-run/breaker interaction has no test (it
would have caught P2-1); browser mouse dispatch and the tab pane-toggle are untested;
the sweep's SQL-vs-filter interplay (P3-5) is pinned only by the lookback-math test
(collect.rs:59-74 — the code comment acknowledges the live path is exercised by the
deployed cron instead).

## Paper fidelity

Every divergence I could check is documented *and* defensible: collect-as-sweep instead
of per-task harness hooks (collect.rs:1-9, docs §Loop order 1), scope stamps replacing
per-benchmark wikis with diverging-evidence widening to `global` under fail-closed gates
(docs §Divergences, proposer.rs:270-288, gates.rs:355-403), plus the previously
documented human gate, counterfactual judge, one-shot proposer, and section-level
merges. Caveat: the local paper extraction (/apps/pdf2md/output/wikiskills.md — 66
lines, mostly figure captions) is too thin to verify the "Algorithm 1 step 9/10/11" or
"Appendix C" citations; those predate this diff, and the one new citation ("paper step
1", collect.rs:1) is framed as a documented divergence rather than a claim of fidelity.
