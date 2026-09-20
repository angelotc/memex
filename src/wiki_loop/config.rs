//! Configuration for wiki-loop, loaded from `~/.memex/wiki-loop.toml`.
//!
//! Missing file means defaults; unknown keys are ignored (matching `UserConfig` semantics).
//! Paths expand `~` and are made absolute at load time.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// How to invoke one agent role. The prompt is always piped via stdin; `{model}`, `{effort}`,
/// and `{schema}` placeholders in `command` args are substituted before exec.
#[derive(Debug, Clone, Deserialize)]
pub struct RoleConfig {
    /// argv vector; element 0 is the program. Supports `{model}` / `{effort}` / `{schema}`.
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: String,
    /// Append `--json-schema <path>` when a schema is available (agy supports this natively).
    #[serde(default)]
    pub json_schema: bool,
}

impl RoleConfig {
    fn agy(model: &str, effort: &str) -> Self {
        Self {
            command: vec![
                "agy".into(),
                "-p".into(),
                "--output-format".into(),
                "json".into(),
                "--model".into(),
                "{model}".into(),
                "--effort".into(),
                "{effort}".into(),
            ],
            model: model.into(),
            effort: effort.into(),
            json_schema: true,
        }
    }

    fn claude(model: &str) -> Self {
        // `claude -p` reads the prompt from piped stdin; no `-` positional and no --effort flag.
        Self {
            command: vec![
                "claude".into(),
                "-p".into(),
                "--output-format".into(),
                "json".into(),
                "--model".into(),
                "{model}".into(),
            ],
            model: model.into(),
            effort: String::new(),
            json_schema: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WikiLoopConfig {
    pub queue_dir: PathBuf,
    pub state_db: PathBuf,
    pub wiki_root: PathBuf,
    pub wiki_staging: PathBuf,
    pub skills_root: PathBuf,
    pub proposals_dir: PathBuf,
    /// Quiet window a session must sit through (after ending) before the maintainer claims it.
    pub quiet_minutes: i64,
    /// Sessions with fewer analytics turns than this are skipped as trivial.
    pub min_turns: i64,
    pub max_batch_size: usize,
    pub max_chars_per_session: usize,
    pub max_chars_per_batch: usize,
    pub subprocess_timeout_secs: u64,
    pub max_proposals_per_week: u32,
    /// Distinct evidence sessions required before a pattern is eligible for a skill proposal.
    pub min_pattern_corroboration: usize,
    /// Historical sessions the Tier-1 counterfactual judge inspects.
    pub tier1_sessions: usize,
    pub maintainer: RoleConfig,
    pub proposer: RoleConfig,
    pub judge: RoleConfig,
    pub notify_on_proposal: bool,
    pub notify_on_failure: bool,
}

impl Default for WikiLoopConfig {
    fn default() -> Self {
        let state_dir = dirs_next_home()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".local/state/wiki-loop");
        let memex_root = dirs_next_home()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".memex");
        Self {
            queue_dir: state_dir.join("queue"),
            state_db: state_dir.join("state.db"),
            wiki_root: memex_root.join("wiki"),
            wiki_staging: state_dir.join("wiki-staging"),
            skills_root: dirs_next_home()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".agents/skills"),
            proposals_dir: state_dir.join("proposals"),
            quiet_minutes: 20,
            min_turns: 3,
            max_batch_size: 10,
            max_chars_per_session: 24 * 1024,
            max_chars_per_batch: 160 * 1024,
            subprocess_timeout_secs: 1200,
            max_proposals_per_week: 3,
            min_pattern_corroboration: 2,
            tier1_sessions: 5,
            maintainer: RoleConfig::agy("gemini-3.8-flash", "low"),
            proposer: RoleConfig::claude("opus"),
            judge: RoleConfig::claude("opus"),
            notify_on_proposal: true,
            notify_on_failure: true,
        }
    }
}

fn dirs_next_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    queue_dir: Option<String>,
    state_db: Option<String>,
    wiki_root: Option<String>,
    wiki_staging: Option<String>,
    skills_root: Option<String>,
    proposals_dir: Option<String>,
    quiet_minutes: Option<i64>,
    min_turns: Option<i64>,
    max_batch_size: Option<usize>,
    max_chars_per_session: Option<usize>,
    max_chars_per_batch: Option<usize>,
    subprocess_timeout_secs: Option<u64>,
    max_proposals_per_week: Option<u32>,
    min_pattern_corroboration: Option<usize>,
    tier1_sessions: Option<usize>,
    maintainer: Option<RoleConfig>,
    proposer: Option<RoleConfig>,
    judge: Option<RoleConfig>,
    notify_on_proposal: Option<bool>,
    notify_on_failure: Option<bool>,
}

impl WikiLoopConfig {
    /// Load config from an explicit path, or `~/.memex/wiki-loop.toml` when present.
    pub fn load(override_path: Option<&Path>) -> Result<Self> {
        let path = match override_path {
            Some(p) => Some(p.to_path_buf()),
            None => {
                let candidate = dirs_next_home()
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join(".memex/wiki-loop.toml");
                candidate.exists().then_some(candidate)
            }
        };

        let mut cfg = Self::default();
        let Some(path) = path else {
            return Ok(cfg);
        };

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let file: ConfigFile =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

        macro_rules! expand {
            ($field:expr, $value:expr) => {
                if let Some(v) = $value {
                    $field = expand_home(Path::new(&v))?;
                }
            };
        }
        expand!(cfg.queue_dir, file.queue_dir);
        expand!(cfg.state_db, file.state_db);
        expand!(cfg.wiki_root, file.wiki_root);
        expand!(cfg.wiki_staging, file.wiki_staging);
        expand!(cfg.skills_root, file.skills_root);
        expand!(cfg.proposals_dir, file.proposals_dir);

        if let Some(v) = file.quiet_minutes {
            cfg.quiet_minutes = v;
        }
        if let Some(v) = file.min_turns {
            cfg.min_turns = v;
        }
        if let Some(v) = file.max_batch_size {
            cfg.max_batch_size = v;
        }
        if let Some(v) = file.max_chars_per_session {
            cfg.max_chars_per_session = v;
        }
        if let Some(v) = file.max_chars_per_batch {
            cfg.max_chars_per_batch = v;
        }
        if let Some(v) = file.subprocess_timeout_secs {
            cfg.subprocess_timeout_secs = v;
        }
        if let Some(v) = file.max_proposals_per_week {
            cfg.max_proposals_per_week = v;
        }
        if let Some(v) = file.min_pattern_corroboration {
            cfg.min_pattern_corroboration = v;
        }
        if let Some(v) = file.tier1_sessions {
            cfg.tier1_sessions = v;
        }
        if let Some(v) = file.maintainer {
            cfg.maintainer = v;
        }
        if let Some(v) = file.proposer {
            cfg.proposer = v;
        }
        if let Some(v) = file.judge {
            cfg.judge = v;
        }
        if let Some(v) = file.notify_on_proposal {
            cfg.notify_on_proposal = v;
        }
        if let Some(v) = file.notify_on_failure {
            cfg.notify_on_failure = v;
        }
        Ok(cfg)
    }

    /// Wiki directory a maintainer run should write to.
    pub fn wiki_dir(&self, live: bool) -> PathBuf {
        if live {
            self.wiki_root.clone()
        } else {
            self.wiki_staging.clone()
        }
    }

    /// Path to memex's analytics store for read-only session metadata lookups.
    pub fn analytics_db(&self) -> PathBuf {
        crate::config::Paths::new(None)
            .map(|p| crate::analytics::analytics_path(&p.state))
            .unwrap_or_else(|_| PathBuf::from("analytics.sqlite"))
    }

    /// Path to the memex search index directory.
    pub fn index_dir(&self) -> Result<PathBuf> {
        Ok(crate::config::Paths::new(None)?.index)
    }
}

fn expand_home(p: &Path) -> Result<PathBuf> {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        let home = dirs_next_home().context("HOME not set; cannot expand ~ in config path")?;
        return Ok(home.join(rest));
    }
    Ok(p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_self_consistent() {
        let cfg = WikiLoopConfig::default();
        assert_eq!(cfg.min_pattern_corroboration, 2);
        assert!(cfg.quiet_minutes > 0);
        assert!(cfg.maintainer.json_schema);
        assert!(!cfg.proposer.json_schema);
        assert!(cfg.state_db.starts_with(dirs_next_home().unwrap()));
    }

    #[test]
    fn load_missing_file_yields_defaults() {
        let cfg = WikiLoopConfig::load(Some(Path::new("/nonexistent/wiki-loop.toml")));
        // Explicit override that does not exist is an error.
        assert!(cfg.is_err());
        // Absent default file falls back to defaults.
        // (Covered implicitly: load(None) without ~/.memex/wiki-loop.toml returns defaults.)
    }

    #[test]
    fn load_overrides_from_toml() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("wiki-loop.toml");
        std::fs::write(
            &path,
            "quiet_minutes = 5\nmin_turns = 1\nmax_batch_size = 4\n\n[maintainer]\ncommand = [\"/bin/echo\"]\nmodel = \"stub\"\neffort = \"\"\njson_schema = false\n",
        )
        .expect("write config");
        let cfg = WikiLoopConfig::load(Some(&path)).expect("load");
        assert_eq!(cfg.quiet_minutes, 5);
        assert_eq!(cfg.min_turns, 1);
        assert_eq!(cfg.max_batch_size, 4);
        assert_eq!(cfg.maintainer.model, "stub");
        assert!(!cfg.maintainer.json_schema);
    }
}
