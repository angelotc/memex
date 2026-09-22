# Lessons

- Write tests that are canonical, not machine-specific: never assert on absolute
  paths from this VM (`/apps/memex`, real `$HOME`). Pin every env-driven root into
  a tempdir (`test_support::pin_source_roots`) and assert only on paths the test
  creates. Remember indirect path derivations: `bob::roots()` returns the *parent*
  of the configured db, so `MEMEX_BOB_DB` must be pinned file-shaped.
- When an edit's `newString` is a near-copy of a neighboring function, re-read the
  region before saving: a copy-paste slip silently replaced a `#[cfg(test)] mod`
  opener and broke the file. Read-after-edit for structural blocks.
- Split PRs along seam lines the user names ("fix directories" vs "fix antigravity
  restoration"): keep shared-file hunks disjoint (profile_roots vs cwd fallback in
  antigravity.rs) so branches off main merge independently.
- Commit and push incrementally on a fix branch as milestones go green; keep cargo
  at the global jobs=2 cap and use CARGO_PROFILE_TEST_DEBUG=0 for test builds.
- The global jobs=2 cap is NOT enough for full dep rebuilds of this crate on the
  16GB/no-swap VM: two `cargo test` runs (target/ wiped → full rebuild) each
  hard-crashed the box. Use the MemAvailable watchdog that kills cargo/rustc
  before the OOM takes the VM (see /tmp/opencode/test-watchdog.sh pattern).
- USER RULE (2026-09-22): builds and tests run at `-j 2` — the global
  ~/.cargo/config.toml cap already enforces this, so plain `cargo build` /
  `cargo test` is correct. Do NOT pass `-j 1`: it gets stuck on this box. Only
  add the memory watchdog, never lower parallelism.
- VM DIED (2026-09-22 ~06:27) minutes after a clean build finished: the wiki-loop
  proposer's 06:17 cron tick (agy on a big prompt) spiked on top of post-build
  memory pressure. Builds are not done when cargo exits — the loop's model runs
  (maintainer every 5m, proposer at :17 of every 6th hour) add multi-GB spikes.
  Rules: (1) keep the watchdog floor at 1500-1800MB, not lower, even if it aborts
  the build — a killed build is cheaper than a dead VM; (2) before building check
  `memex-wiki-loop wiki-loop status` + the proposer window; prefer building right
  after a proposer run ends, not while one is live; (3) after any heavy build,
  leave headroom for the loop before starting another heavy task.
- glibc arena blowout in bfd: `MALLOC_ARENA_MAX=1` cut the lib-test link from
  OOM-kill territory to fitting comfortably. Use it for every link-heavy cargo
  invocation on this box.
