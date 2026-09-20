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
            Rule {
                pattern: Regex::new(r"(?:sk|rk)_live_[A-Za-z0-9]{24,}").expect("regex"),
                replacement: "[REDACTED_STRIPE_LIVE]",
                quarantine: true,
            },
            Rule {
                pattern: Regex::new(r"(?:sk|rk)_test_[A-Za-z0-9]{24,}").expect("regex"),
                replacement: "[REDACTED_STRIPE_TEST]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(r"whsec_[A-Za-z0-9]{24,}").expect("regex"),
                replacement: "[REDACTED_STRIPE_WEBHOOK_SECRET]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(
                    r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{5,}",
                )
                .expect("regex"),
                replacement: "[REDACTED_JWT]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(
                    r#"(?i)(postgresql|postgres|mysql|mongodb(?:\+srv)?|redis|rediss|amqps|amqp)://[^\s'"@/]+:[^\s'"@/]+@"#,
                )
                .expect("regex"),
                replacement: "${1}://[REDACTED_CREDENTIALS]@",
                quarantine: false,
            },
            // Label-gated: must run before the generic unquoted env rule below.
            Rule {
                pattern: Regex::new(
                    r"(?i)(aws_secret_access_key|secret_access_key|secret_key)(\s*[=:]\s*)[A-Za-z0-9/+=]{40}\b",
                )
                .expect("regex"),
                replacement: "$1$2[REDACTED_AWS_SECRET]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(
                    r"(?i)(api_key|auth_token|secret|password|private_key|token|access_key)[^\n=]*=\s*[A-Za-z0-9_\-./+=]{16,}",
                )
                .expect("regex"),
                replacement: "$1[REDACTED_ENV_SECRET]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(
                    r"https://hooks\.slack\.com/services/T[A-Za-z0-9]+/B[A-Za-z0-9]+/[A-Za-z0-9]+",
                )
                .expect("regex"),
                replacement: "https://hooks.slack.com/services/[REDACTED_SLACK_WEBHOOK]",
                quarantine: false,
            },
            Rule {
                pattern: Regex::new(r"(?i)basic\s+[A-Za-z0-9+/=]{16,}").expect("regex"),
                replacement: "Basic [REDACTED_CREDENTIALS]",
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

    #[test]
    fn stripe_live_keys_redact_and_quarantine() {
        let r = scrub_text(concat!(
            "sk_live_",
            "9ZxJrgYqrKHTnMWo7HsMzDqB3LpXvNaE7kQ2wS5tU",
            " and ",
            "rk_live_",
            "2AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCd"
        ));
        assert!(r.quarantined);
        assert!(r.text.contains("[REDACTED_STRIPE_LIVE]"));
        assert!(!r.text.contains("sk_live_9Z"));
        assert!(!r.text.contains("rk_live_2A"));
        assert!(contains_secret(concat!(
            "rk_live_",
            "2AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCd"
        )));
    }

    #[test]
    fn stripe_test_keys_and_webhook_secrets_redact() {
        let r = scrub_text(
            "sk_test_4eC39HqLyjWDarjtT1zdp7dcTESTKEY12345678 rk_test_51Hj9oPqRsTuVwXyZ01234567890123456789012 whsec_6mFBqL2VMQRt7Y0eCVGYHsqDtGhZ0D0Mtest123",
        );
        assert!(!r.quarantined);
        assert!(r.text.contains("[REDACTED_STRIPE_TEST]"));
        assert!(r.text.contains("[REDACTED_STRIPE_WEBHOOK_SECRET]"));
        assert!(!r.text.contains("sk_test_4e"));
        assert!(!r.text.contains("whsec_6mF"));
    }

    #[test]
    fn unquoted_env_assignments_redact_value_only() {
        let r = scrub_text("export API_KEY=supersecretvalue1234567890abcdef");
        assert!(!r.quarantined);
        assert!(r.text.contains("API_KEY[REDACTED_ENV_SECRET]"));
        assert!(!r.text.contains("supersecretvalue"));
    }

    #[test]
    fn jwt_tokens_redact() {
        let r = scrub_text(
            "curl -H \"X-Auth: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c\"",
        );
        assert!(!r.quarantined);
        assert!(r.text.contains("[REDACTED_JWT]"));
        assert!(!r.text.contains("SflKxw"));
        assert!(contains_secret(
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
        ));
    }

    #[test]
    fn connection_strings_with_credentials_redact() {
        let r = scrub_text(
            "postgres://admin:hunter2@db.example.com:5432/app and mongodb+srv://svc:TealSecret99@cluster0.abc.mongodb.net",
        );
        assert!(!r.quarantined);
        assert!(
            r.text
                .contains("postgres://[REDACTED_CREDENTIALS]@db.example.com")
        );
        assert!(
            r.text
                .contains("mongodb+srv://[REDACTED_CREDENTIALS]@cluster0")
        );
        assert!(!r.text.contains("hunter2"));
        assert!(!r.text.contains("TealSecret99"));
    }

    #[test]
    fn aws_secret_access_keys_redact() {
        let r = scrub_text("aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        assert!(!r.quarantined);
        assert!(
            r.text
                .contains("aws_secret_access_key = [REDACTED_AWS_SECRET]")
        );
        assert!(!r.text.contains("wJalrXUtnFEMI"));
    }

    #[test]
    fn slack_webhook_urls_redact() {
        let r = scrub_text(
            "POST https://hooks.slack.com/services/T024BE7LD/B01TWD4Y8GK/4ogFqx0X8yBpLChGsd0iTG",
        );
        assert!(!r.quarantined);
        assert!(
            r.text
                .contains("https://hooks.slack.com/services/[REDACTED_SLACK_WEBHOOK]")
        );
        assert!(!r.text.contains("T024BE7LD"));
    }

    #[test]
    fn basic_auth_credentials_redact() {
        let r = scrub_text("Authorization: Basic dXNlcjpwYXNzd29yZDEyMzQ1Ng==");
        assert!(!r.quarantined);
        assert!(r.text.contains("Basic [REDACTED_CREDENTIALS]"));
        assert!(!r.text.contains("dXNlcjpw"));
    }

    #[test]
    fn postgres_url_without_credentials_passes_through() {
        let text = "connect via postgres://localhost:5432/app or redis://cache:6379";
        let r = scrub_text(text);
        assert!(!r.quarantined);
        assert_eq!(r.text, text);
        assert!(!contains_secret(text));
    }

    #[test]
    fn prose_with_jwt_like_words_passes_through() {
        let text = "The header eyJ often starts a JWT, but this is prose with the base64 word c2VjcmV0dmFsdWVsb2dnZWRkb3du.";
        let r = scrub_text(text);
        assert!(!r.quarantined);
        assert_eq!(r.text, text);
        assert!(!contains_secret(text));
    }
}
