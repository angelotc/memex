//! wiki-loop: compile agent experience into persistent wiki knowledge and staged skills.
//!
//! Implements the WikiSkill three-layer loop (arXiv:2608.27454) downstream of memex's
//! read-only corpus:
//!
//! 1. **Raw layer** — immutable execution traces, read in-process from the memex index and
//!    analytics store (`SearchIndex::records_by_session_id`, `analytics.sqlite`).
//! 2. **Wiki layer** — a compounding markdown store (`patterns/*.md`, `index.md`, `logs.md`,
//!    `skill-impact.md`). Wiki content is never rolled back; edits are patch-based merges.
//! 3. **Skills layer** — staged proposals validated through tier gates before a human applies
//!    them to the live skills directory.
//!
//! Loop order mirrors the paper's Algorithm 1: collect traces (enqueue hooks) → sample a
//! context-fit batch (queue claim + digest budgets) → Wiki Maintainer consolidates → Skill
//! Proposer proposes one atomic skill update (reading the wiki index and skill-impact audit
//! trail first) → gates validate → apply/revert with the outcome appended to `skill-impact.md`.

pub mod cli;

pub use cli::WikiLoopCommand;
mod config;
mod digest;
mod gates;
mod harness;
mod ingest;
mod ledger;
mod lock;
mod notify;
mod patterns;
mod proposer;
mod queue;
mod scrub;

/// Prompt templates, embedded at compile time (house style: `include_str!`).
pub mod prompts {
    pub const MAINTAINER: &str = include_str!("prompts/maintainer.md");
    pub const PROPOSER: &str = include_str!("prompts/proposer.md");
    pub const JUDGE: &str = include_str!("prompts/judge.md");
}
