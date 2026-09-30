# Wiki-loop findings verification

Reviewed 2026-09-22 against source commit `b9a6460`. Runtime snapshot approximately
22:11 UTC; the scheduled maintainer was active, so counts are not immutable.
Scope: verification only. No production code, configuration, queue, proposal,
deployment, or skill was changed. No live model jobs were invoked.

## Verdict

All six P0 mechanisms exist in current source. Their consequences need narrower
wording: Tier 1 can validate without evidence but does not deploy automatically;
failed operations lose automatic retry eligibility, not the underlying traces;
a UTF-8 panic does not permanently retain OS locks; supersede removes the old
body/evidence prose but retains metadata and corroboration. The six findings do
not all merit identical incident priority: evidence loss and the scrub bypass
are more urgent than an empty title.

## P0 — source-verified defects

| Finding | Evidence and exact consequence | Verification/fix criterion |
|---|---|---|
| Tier 1 passes with no usable evidence | `src/wiki_loop/gates.rs:503-525`: record-load errors and sessions without error turns are skipped; `used == 0` returns `passed: true`. An entirely empty replay set fails closed earlier at line 470. | A nonempty replay set with all unreadable records must not pass. Passing-session evidence needs an explicit evaluation path for success patterns. |
| Failed operations consume sessions | `src/wiki_loop/cli.rs:490-513` acknowledges every unchanged selected entry and records outcome `ok`, before the all-operations-failed error at line 528. The same problem occurs for a session represented only by failed operations in a partially successful batch. `collect.rs:43` and `ledger.rs:113-123` then suppress recollection through that event timestamp. | Test all-failed and mixed-result batches: evidence associated with failed operations remains retryable; successful writes remain idempotent on replay. |
| Maintainer summary bypasses scrubbing | `cli.rs:478-486` writes `output.summary` directly. `patterns.rs:504-511` appends verbatim; digest construction includes trace text and tool input/output (`digest.rs:81-99`, `144-159`) without a scrub pass. | Synthetic secret in summary must be redacted before persistence. This confirms an exposure path, not an observed real secret leak. |
| UTF-8 truncation panic | `proposer.rs:89-92` calls `String::truncate(61440)` without checking a character boundary. | Isolated Rust reproduction: ASCII and aligned CJK inputs succeed; 61,439 ASCII bytes followed by `日本` panic. Apply a UTF-8-safe byte bound. |
| Supersede overwrites historical body | `patterns.rs:429-436` renders the old page using incoming sections and an explicitly empty evidence section. Metadata is cloned at line 417, so corroboration survives while original prose does not. | Preserve old sections and evidence when retiring a page. Existing test at line 1042 checks retirement metadata only. Empty sections are permitted; the schema does not require them to be empty. |
| Merge can erase title | `patterns.rs:362` assigns the incoming title unconditionally, unlike section preservation at lines 390-392. | Preserve an existing title on an empty incoming title; test whitespace policy. Live `git-show-stage-specifiers-fail-on-clean-index-read-refs-instead.md:3` has `title: ""`. This proves current bad state, not which historic operation caused it. |

Additional qualifications:

- Human apply remains a separate step (`gates.rs:691`). “Broken index auto-approves
  skills” should read “unreadable replay records can bypass Tier 1.” Other index
  failures can still produce errors or an empty replay set and fail closed.
- Error-free corroborating sessions are excluded regardless of pattern kind.
  A success pattern can still be judged against other sessions containing errors;
  “success patterns are never judged” is too categorical.
- Raw sessions remain indexed. A later event timestamp can make a session eligible
  again, or an operator can explicitly recover/requeue it.
- Proposer holds wiki/proposer locks at the panic point (`cli.rs:112`), but these
  are file locks (`lock.rs:16-18,38`), released on unwind/process exit.

## P1 — deployed skill and judge calibration

The deployed `/apps/skills/python-inline-quote-escaping/SKILL.md:29` still contains
the broken literal `\x27` command. Executing that exact example in Bash returned
exit 1 with `SyntaxError: unexpected character after line continuation character`.
The alternate example at line 33 returned exit 0 and `name=Alice`.

The 12:20 corrigendum proposal
`prop_1a0c90f2f03_1a0c90f2f0381c4102d09` was rejected by `recently_rejected` before
commit `0ebeee2` at 16:33 UTC. Current `gates.rs:177-201` exempts a live deployment,
and a regression test exists (`recently_rejected_does_not_mute_a_deployed_skill`).
This historic rejection does not show that exemption is broken.

The claim that the 18:17 tick produced nothing is contradicted by `proposer.log`
and proposal artifacts: it produced `git-push-sandbox-preflight`, proposal
`prop_1a0ca57edfe_1a0ca57edfe303fcace25`, and Tier 1 rejected it. No subsequent
Python correction was found. Why it selected a different skill is not established.

The mute uses exact skill-name equality (`gates.rs:181-184`, `ledger.rs:259-267`),
so a renamed `-v2` proposal evades this particular gate. Other gates still apply.

Current state has one accepted proposal and six rejected proposals, three of
which have failed Tier 1 results: psycopg2 SQL escaping (1/3), PG environment
loading (1/3), and Git push sandbox preflight (1/5). These figures justify an
assessment/evidence review, but do not establish false rejections. Review whether
each sampled session actually exercises the proposed behavior before changing
the strict-majority threshold (`gates.rs:593-606`).

## P2 — wiki growth and consolidation

- **Unbounded proposer context is confirmed for index and impact history.**
  `proposer.rs:56-58,103-109` reads and includes both files in full. Impact history
  appends diffs (`gates.rs:763-774`); index includes all catalog rows, including
  superseded pages (`patterns.rs:480-497`). Snapshot sizes: index 20,268 bytes;
  skill-impact 19,105 bytes.
- **Logs grow, but are not prompt input.** `logs.md` was 67,998 bytes; append is
  unbounded (`patterns.rs:504`). No prompt reader was found. Its size is a
  storage/browser concern, not the cited proposer context problem. The claimed
  22 KB/day growth rate was not independently measured.
- **Freshest-30 visibility is confirmed.** `cli.rs:549-561` takes only the 30 most
  recently updated non-quarantined pages, with further byte bounds. Older
  counterparts can be absent from a merge decision. This supports fragmentation
  risk, not proof of permanent starvation or of causation for every duplicate.
- **Some duplication and narrative drift are real.** The psycopg2 unescaped-percent
  and LIKE-query pages describe overlapping cause/remedy. The sandbox DNS page
  literally begins a section with “Adds a fourth client flavor” at line 18.
  Wrangler log EROFS and Corepack SQLite failures are distinct remedies even when
  they share a session; shared provenance alone does not prove duplication.
  The claimed four semantic clusters were not all independently established.
- **Current counts differ slightly:** 70 pages, 69 candidate and one superseded;
  40/70 have one corroboration (57.1%); 13 titles exceed 120 characters, maximum
  210; one empty title. The source creates candidate pages and has no automatic
  promotion to a mature status (`patterns.rs:314,418`). Proposer eligibility uses
  corroboration, not a promoted status (`proposer.rs:63-73`), so candidate status
  alone does not demonstrate proposer starvation.

## P3 — operability

| Claim | Verdict |
|---|---|
| Proposer has no run rows/breaker | Confirmed. Only maintainer calls `start_run` (`cli.rs:191`). Proposer wrapper at line 110 lacks run tracking and breaker checks. Current DB has zero proposer run rows. |
| 10 of 12 maintainer errors are JSON drift | Confirmed from read-only ledger snapshot: 10 parse/shape/missing-field errors, one quota error, one other. Snapshot totals: 565 ok, 12 error, one dry, one running = 579 rows. |
| Retry but no repair | Precisely: `harness.rs:49-73` retries extraction/envelope failures once with the same prompt. It does not feed validation errors back into a corrective prompt; syntactically valid JSON failing later domain deserialization is not retried (`cli.rs:392`, `proposer.rs:132`). Saying no retry/recovery exists would be wrong. |
| Doctor checks obsolete harnesses | Confirmed: hardcoded agy/claude/herdr checks (`cli.rs:829`), while all live roles use `/root/.opencode/bin/opencode`, schema enforcement disabled. |
| Update notices spam role logs | Confirmed in source cron template (`cli.rs:934`) and live cron: only index has `--no-update-check`. Snapshot logs contain 594 maintainer and 10 proposer `update: memex` notices. |
| Breaker repeatedly notifies | Confirmed notification attempt on every tripped invocation when notifications are enabled (`cli.rs:180-188`), with no cooldown/latch. External delivery itself was not tested. |
| DLQ lacks recovery interface | Confirmed: CLI supplies five attempts (`cli.rs:258`); `queue.rs:227` moves exhausted entries into DLQ; collector skips them (`collect.rs:46`). Status counts DLQ entries, but no list/requeue command exists. Manual recovery is possible, so “black hole” is operational shorthand. |

## P4 — tests, orchestration, and locking

No direct orchestration tests were found for `execute_maintainer`, `run_proposer`,
`run_validate`, `run_status`, or `run_doctor`. Helper-level tests exist, including
queue, ledger, patterns, gates, harness, and cron handling. “Zero tests” should be
scoped to these flows, not the entire module.

Gate policy is centralized. The evaluation sequence is duplicated between
`proposer.rs:199-228` and `cli.rs:598-639`. `gates.rs::apply` checks saved results;
it is not a third execution of the gate sequence. There is real input drift:
proposer uses all supplied candidate scopes (`proposer.rs:195`), while manual
validation derives scopes from the proposal's motivating patterns (`cli.rs:587`).

Validation acquires delivery lock before judging (`cli.rs:129,617`), blocking
apply/rollback while the judge runs. Live subprocess timeout is 2,400 seconds
per invocation. A shape/extraction retry permits two invocations, so the lock
can span roughly 80 minutes, not merely 40, plus surrounding work.

## Prior-review nits

Filename sorting in the wiki browser remains (`src/tui.rs:7099`). Normal
`superseded_by: null` and quoted `"null"` both parse as `None`
(`patterns.rs:161,170-171`) and render back to null. The claimed null-roundtrip
nit is not an operational bug for ordinary IDs; only the literal replacement
identifier `null` would collide with that sentinel.

The blanket claim that every other finding in both prior reviews was remediated
was not re-audited item by item in this pass.

## Recommended implementation sequence

1. Fix acknowledgement/retry semantics, summary scrubbing, Tier 1 zero-evidence
   behavior, and supersede history preservation with focused regression tests.
   Include UTF-8-safe truncation and title preservation in the same bounded pass.
2. Correct the deployed Python example through the reviewed update path; evaluate
   judge evidence relevance separately from its acceptance threshold. The existing
   deployed-skill exemption does not need another speculative fix.
3. Add proposer run tracking, configured-harness doctor checks, and quiet cron
   flags. These improve the evidence needed for later tuning.
4. Design bounded index/impact prompt views and relevant-page retrieval for the
   maintainer while retaining archival history. Avoid treating log deletion as
   the context-size fix.

Verification performed: source/control-flow inspection, existing test inspection,
read-only SQLite/log/config/cron/artifact checks, a tiny standalone Rust boundary
reproduction, and execution of the two isolated Python examples. No cargo suite
or end-to-end model run was performed; the remaining defects are source-verified,
not claimed as newly executed integration regressions.
