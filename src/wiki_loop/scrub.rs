//! Secret/PII scrubbing for wiki content. Redaction is unconditional; a hit on a
//! high-risk pattern additionally quarantines the whole pattern (fail-closed).

use regex::Regex;
use std::sync::OnceLock;

struct Rule {
    pattern: Regex,
    replacement: &'static str,
    /// When true, a match quarantines the pattern instead of just redacting.
    quarantine: bool,
}

fn rules() -> &'static [Rule] {
    static RULES: OnceLock<Vec<Rule>> = OnceLock::new();
    RULES.get_or_init(|| {
        vec![
            Rule {
                pattern: Regex::new(r"ghp_[A-Za-z0-9]{36}").expect("regex"),
                replacement: "[REDACTED_GH_TOKEN]",
                quarantine: true,
            },
            Rule {
                pattern: Regex::new(r"github_pat_[A-Za-z0-9_]{82}").expect("regex"),
                replacement: "[REDACTED_GH_PAT]",
                quarantine: true,
            },
            Rule {
                pattern: Regex::new(r"gho_[A-Za-z0-9]{36}").expect("regex"),
                replacement: "[REDACTED_GH_OAUTH]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(r"glpat-[A-Za-z0-9\-_]{20,}").expect("regex"),
                replacement: "[REDACTED_GL_PAT]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(r"xox[baprs]-[A-Za-z0-9\-]{24,}").expect("regex"),
                replacement: "[REDACTED_SLACK_TOKEN]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(r"sk-[A-Za-z0-9\-_]{32,}").expect("regex"),
                replacement: "[REDACTED_API_KEY]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(r"AIza[0-9A-Za-z\-_]{35}").expect("regex"),
                replacement: "[REDACTED_GOOGLE_KEY]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(r"AKIA[0-9A-Z]{16}").expect("regex"),
                replacement: "[REDACTED_AWS_KEY]",
                quarantine: true,
            },
            Rule {
                pattern: Regex::new(r"(?i)bearer\s+[A-Za-z0-9\-_.]{10,}").expect("regex"),
                replacement: "Bearer [REDACTED_TOKEN]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                )
                .expect("regex"),
                replacement: "[REDACTED_PRIVATE_KEY]",
                quarantine: true,
            },
            Rule {
                pattern: Regex::new(
                    r#"(?i)(API_KEY|AUTH_TOKEN|SECRET|PASSWORD|PRIVATE_KEY)\s*=\s*['\"][^'\"]{8,}['\"]"#,
                )
                .expect("regex"),
                replacement: "[REDACTED_ENV_SECRET]",
                quarantine: false,
            },
        ]
    })
}

pub struct ScrubResult {
    pub text: String,
    pub quarantined: bool,
}

/// Redact secrets from `text`; report whether a high-risk pattern was present.
pub fn scrub_text(text: &str) -> ScrubResult {
    let mut out = text.to_string();
    let mut quarantined = false;
    for rule in rules() {
        if rule.pattern.is_match(&out) {
            if rule.quarantine {
                quarantined = true;
            }
            out = rule
                .pattern
                .replace_all(&out, rule.replacement)
                .into_owned();
        }
    }
    ScrubResult {
        text: out,
        quarantined,
    }
}

/// Re-scan already-scrubbed text (e.g. a proposed SKILL.md) before it leaves the machine.
pub fn contains_secret(text: &str) -> bool {
    rules().iter().any(|r| r.pattern.is_match(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_passes_through() {
        let r = scrub_text("Everything is working as expected.");
        assert!(!r.quarantined);
        assert_eq!(r.text, "Everything is working as expected.");
        assert!(!contains_secret("no secrets here"));
    }

    #[test]
    fn github_token_redacts_and_quarantines() {
        let r = scrub_text("key ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 leaked");
        assert!(r.quarantined);
        assert!(r.text.contains("[REDACTED_GH_TOKEN]"));
        assert!(!r.text.contains("ghp_ABCD"));
        assert!(contains_secret(
            "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        ));
    }

    #[test]
    fn env_secrets_redact_without_quarantine() {
        let r = scrub_text("API_KEY = \"super-secret-value-123\"");
        assert!(!r.quarantined);
        assert!(r.text.contains("[REDACTED_ENV_SECRET]"));
    }

    #[test]
    fn private_keys_redact_and_quarantine() {
        let r =
            scrub_text("-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----");
        assert!(r.quarantined);
        assert!(r.text.contains("[REDACTED_PRIVATE_KEY]"));
    }
}
