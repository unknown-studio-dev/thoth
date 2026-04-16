//! PII / secret redaction.
//!
//! Runs inside every adapter **and** as a final pass in the sync engine.
//! The philosophy is: **fail loud** — if a rule matches a high-risk
//! pattern, reject the whole rule rather than silently redacting. The
//! user can then look at the source, strip the offending field, and re-sync.
//!
//! This is intentionally a small, auditable set of regex-style string
//! checks. For production use, teams should layer on their own DLP.

use crate::error::{DomainError, Result};
use crate::types::RemoteRule;

/// Check a rule for disallowed content. On match, returns `Err(Redacted)`.
///
/// Current checks:
///
/// - Bearer / API-key-looking tokens (`sk-*`, `xoxb-*`, `ghp_*`, JWTs).
/// - 16-digit card-number patterns.
/// - AWS access-key-looking strings (`AKIA[0-9A-Z]{16}`).
pub fn scan(rule: &RemoteRule) -> Result<()> {
    let haystack = format!("{}\n{}", rule.title, rule.body);

    if contains_jwt(&haystack) {
        return Err(DomainError::Redacted(
            rule.id.clone(),
            "JWT-like token found".into(),
        ));
    }
    if contains_provider_token(&haystack) {
        return Err(DomainError::Redacted(
            rule.id.clone(),
            "provider API key pattern found".into(),
        ));
    }
    if contains_card_number(&haystack) {
        return Err(DomainError::Redacted(
            rule.id.clone(),
            "16-digit number that looks like a card".into(),
        ));
    }
    if contains_aws_key(&haystack) {
        return Err(DomainError::Redacted(
            rule.id.clone(),
            "AWS access key pattern found".into(),
        ));
    }
    Ok(())
}

fn contains_jwt(s: &str) -> bool {
    // Rough JWT shape: three base64url segments separated by dots, first
    // segment starts with `eyJ` (the `{"` of a JWT header).
    s.split_whitespace().any(|tok| {
        tok.starts_with("eyJ") && tok.chars().filter(|c| *c == '.').count() == 2 && tok.len() >= 20
    })
}

fn contains_provider_token(s: &str) -> bool {
    // A few common, visually distinctive prefixes. Intentionally narrow
    // to avoid false positives on prose like "skip this step".
    const PREFIXES: &[&str] = &["sk-", "xoxb-", "xoxp-", "ghp_", "ghs_", "gho_", "glpat-"];
    s.split_whitespace().any(|tok| {
        PREFIXES
            .iter()
            .any(|p| tok.starts_with(p) && tok.len() >= p.len() + 10)
    })
}

fn contains_card_number(s: &str) -> bool {
    // Look for runs of exactly 16 digits, possibly dash/space-separated
    // in 4-4-4-4 form. We don't Luhn-check — we prefer false positives
    // that force a human review.
    let bytes = s.as_bytes();
    let mut digits = 0usize;
    let mut groups = 0usize;
    let mut group_len = 0usize;
    let mut last_was_digit = false;
    for &b in bytes {
        if b.is_ascii_digit() {
            digits += 1;
            group_len += 1;
            last_was_digit = true;
        } else {
            if last_was_digit {
                if group_len == 4 {
                    groups += 1;
                    if groups == 4 {
                        return true;
                    }
                } else if group_len != 0 {
                    groups = 0;
                }
                group_len = 0;
            }
            if b != b' ' && b != b'-' {
                digits = 0;
                groups = 0;
            }
            last_was_digit = false;
        }
    }
    if last_was_digit && group_len == 4 && groups == 3 {
        return true;
    }
    // Fallback: any substring of exactly 16 consecutive digits.
    digits >= 16
        && bytes
            .windows(16)
            .any(|w| w.iter().all(|b| b.is_ascii_digit()))
}

fn contains_aws_key(s: &str) -> bool {
    s.split_whitespace().any(|tok| {
        tok.len() == 20
            && tok.starts_with("AKIA")
            && tok
                .chars()
                .skip(4)
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RemoteRule, RuleKind};
    use time::OffsetDateTime;

    fn make_rule(body: &str) -> RemoteRule {
        RemoteRule {
            id: "r1".into(),
            source: "test".into(),
            source_uri: "test://r1".into(),
            context: "billing".into(),
            kind: RuleKind::Invariant,
            title: "t".into(),
            body: body.into(),
            updated_at: OffsetDateTime::now_utc(),
            tags: vec![],
        }
    }

    #[test]
    fn clean_rule_passes() {
        assert!(scan(&make_rule("refunds over $500 require approval")).is_ok());
    }

    #[test]
    fn jwt_blocked() {
        let jwt = "eyJhbGciOi.eyJzdWIiOiJhYmMi.signaturehere123";
        assert!(scan(&make_rule(&format!("token: {jwt}"))).is_err());
    }

    #[test]
    fn provider_token_blocked() {
        assert!(scan(&make_rule("key: sk-abcdefghijklmnop")).is_err());
    }

    #[test]
    fn card_number_blocked() {
        assert!(scan(&make_rule("pay with 4111-1111-1111-1111 please")).is_err());
    }

    #[test]
    fn aws_key_blocked() {
        assert!(scan(&make_rule("cred AKIAIOSFODNN7EXAMPLE")).is_err());
    }

    #[test]
    fn skip_word_not_token() {
        // "skip" starts with "sk" but not "sk-", so must not flag.
        assert!(scan(&make_rule("skip this step if unsure")).is_ok());
    }
}
