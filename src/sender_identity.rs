//! Fail-closed Discord sender-identity verification.
//!
//! Transport success (`discord_send_success`, `token_source=config`) proves the
//! token is valid and the message was delivered — it does not prove *which*
//! bot sent it. During a 2026-08-21 recovery, a wrong-but-valid token
//! delivered messages as the wrong bot while every transport check stayed
//! green. This module closes that gap: when the operator configures
//! `expected_bot_id`, the effective token must resolve to exactly that stable
//! ID via the Discord `/users/@me` identity endpoint before any readiness,
//! smoke, or health claim of a verified sender identity is made.
//!
//! Verification is fail-closed: any outcome other than an exact observed ==
//! expected match is a non-healthy verdict. Outputs are public-safe — they
//! carry expected/observed bot IDs and a failure mode, never the token,
//! response bodies, or other credential material.

use std::fmt;

use crate::discord::{DiscordClient, SelfLookup};

/// Whether an operator-configured expected bot ID is present at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderIdentityExpectation {
    /// `expected_bot_id` is configured; identity must match it exactly.
    Expected { bot_id: String },
    /// No expectation configured. Identity verification is a no-op and
    /// explicitly *not* claimed as passed; transport-only behavior is
    /// preserved for existing deployments.
    Absent,
}

/// Fail-closed verdict for the sender-identity contract.
///
/// `Verified` is the only healthy outcome, and it requires an exact
/// observed == expected stable bot ID match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderIdentityVerdict {
    /// `/users/@me` resolved to exactly the expected stable bot ID.
    Verified {
        /// The stable bot ID both expected and observed.
        bot_id: String,
    },
    /// `/users/@me` resolved to a different stable bot ID. This is the
    /// wrong-but-valid-token case: transport works, the sender is wrong.
    Mismatch {
        expected_bot_id: String,
        observed_bot_id: String,
    },
    /// `expected_bot_id` is not configured, so identity is unverified by
    /// operator choice. Not healthy, not an error — absent expectation.
    NotConfigured,
    /// No bot token is effective, so no identity can be resolved.
    NoToken,
    /// Discord rejected the credential: invalid or revoked token.
    InvalidCredential,
    /// Credential accepted but the identity endpoint was forbidden.
    Forbidden,
    /// Rate limited before identity could be resolved.
    RateLimited,
    /// The identity endpoint answered but the payload did not yield a stable
    /// bot ID. Identity is unverified; fail closed rather than guess.
    MalformedResponse,
    /// Network or unexpected HTTP failure. Identity is unverified.
    TransportFailure,
}

impl SenderIdentityVerdict {
    /// The only healthy outcome: an exact observed == expected match.
    pub fn is_verified(&self) -> bool {
        matches!(self, SenderIdentityVerdict::Verified { .. })
    }

    /// Stable, machine-readable reason code safe for JSON output.
    pub fn reason_code(&self) -> &'static str {
        match self {
            SenderIdentityVerdict::Verified { .. } => "sender_identity_verified",
            SenderIdentityVerdict::Mismatch { .. } => "sender_identity_mismatch",
            SenderIdentityVerdict::NotConfigured => "sender_identity_not_configured",
            SenderIdentityVerdict::NoToken => "sender_identity_no_token",
            SenderIdentityVerdict::InvalidCredential => "sender_identity_invalid_credential",
            SenderIdentityVerdict::Forbidden => "sender_identity_forbidden",
            SenderIdentityVerdict::RateLimited => "sender_identity_rate_limited",
            SenderIdentityVerdict::MalformedResponse => "sender_identity_malformed_response",
            SenderIdentityVerdict::TransportFailure => "sender_identity_transport_failure",
        }
    }

    /// The stable bot ID this verdict proves (or expected, for mismatches).
    pub fn observed_bot_id(&self) -> Option<&str> {
        match self {
            SenderIdentityVerdict::Verified { bot_id } => Some(bot_id),
            SenderIdentityVerdict::Mismatch {
                observed_bot_id, ..
            } => Some(observed_bot_id),
            _ => None,
        }
    }
}

impl fmt::Display for SenderIdentityVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SenderIdentityVerdict::Verified { bot_id } => {
                write!(f, "sender identity VERIFIED (bot id {bot_id})")
            }
            SenderIdentityVerdict::Mismatch {
                expected_bot_id,
                observed_bot_id,
            } => write!(
                f,
                "sender identity MISMATCH: expected Discord bot id {expected_bot_id} but the \
                 effective token resolves to bot id {observed_bot_id}; transport may still \
                 succeed while messages are sent by the wrong bot"
            ),
            SenderIdentityVerdict::NotConfigured => write!(
                f,
                "sender identity NOT CONFIGURED: set providers.discord.expected_bot_id to \
                 enable fail-closed verification; transport checks alone cannot prove sender \
                 identity"
            ),
            SenderIdentityVerdict::NoToken => write!(
                f,
                "sender identity UNVERIFIED: no effective Discord bot token is configured"
            ),
            SenderIdentityVerdict::InvalidCredential => write!(
                f,
                "sender identity UNVERIFIED: Discord rejected the bot token (unauthorized); \
                 the credential is invalid or revoked"
            ),
            SenderIdentityVerdict::Forbidden => write!(
                f,
                "sender identity UNVERIFIED: Discord answered forbidden for the identity \
                 endpoint"
            ),
            SenderIdentityVerdict::RateLimited => write!(
                f,
                "sender identity UNVERIFIED: Discord rate limited the identity check; retry"
            ),
            SenderIdentityVerdict::MalformedResponse => write!(
                f,
                "sender identity UNVERIFIED: Discord identity response did not contain a \
                 stable bot id"
            ),
            SenderIdentityVerdict::TransportFailure => write!(
                f,
                "sender identity UNVERIFIED: Discord identity request failed (transport)"
            ),
        }
    }
}

/// Resolve the operator expectation from the effective (post-legacy-migration)
/// config, tolerating surrounding whitespace and empty strings.
pub fn sender_identity_expectation(expected: Option<&str>) -> SenderIdentityExpectation {
    match expected.map(str::trim).filter(|value| !value.is_empty()) {
        Some(bot_id) => SenderIdentityExpectation::Expected {
            bot_id: bot_id.to_string(),
        },
        None => SenderIdentityExpectation::Absent,
    }
}

/// Run the fail-closed sender-identity check.
///
/// Queries Discord `/users/@me` with the effective token — a bounded,
/// non-mutating read — and compares the observed stable bot ID against the
/// operator-configured expectation. Every non-match outcome (including
/// success-shaped but malformed responses) maps to a non-healthy verdict so a
/// wrong-but-valid token can never be reported healthy merely because
/// transport works.
pub async fn verify_sender_identity(
    client: &DiscordClient,
    expectation: &SenderIdentityExpectation,
) -> SenderIdentityVerdict {
    match expectation {
        SenderIdentityExpectation::Absent => SenderIdentityVerdict::NotConfigured,
        SenderIdentityExpectation::Expected { bot_id } => match client.lookup_self().await {
            SelfLookup::Bot { id } => {
                if id == *bot_id {
                    SenderIdentityVerdict::Verified { bot_id: id }
                } else {
                    SenderIdentityVerdict::Mismatch {
                        expected_bot_id: bot_id.clone(),
                        observed_bot_id: id,
                    }
                }
            }
            SelfLookup::NoToken => SenderIdentityVerdict::NoToken,
            SelfLookup::Unauthorized => SenderIdentityVerdict::InvalidCredential,
            SelfLookup::Forbidden => SenderIdentityVerdict::Forbidden,
            SelfLookup::RateLimited => SenderIdentityVerdict::RateLimited,
            SelfLookup::MalformedSuccess => SenderIdentityVerdict::MalformedResponse,
            SelfLookup::Transport => SenderIdentityVerdict::TransportFailure,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expectation_absent_for_none_empty_and_whitespace() {
        assert_eq!(
            sender_identity_expectation(None),
            SenderIdentityExpectation::Absent
        );
        assert_eq!(
            sender_identity_expectation(Some("")),
            SenderIdentityExpectation::Absent
        );
        assert_eq!(
            sender_identity_expectation(Some("   ")),
            SenderIdentityExpectation::Absent
        );
    }

    #[test]
    fn expectation_trims_configured_bot_id() {
        assert_eq!(
            sender_identity_expectation(Some(" 1471139513983307916 ")),
            SenderIdentityExpectation::Expected {
                bot_id: "1471139513983307916".to_string()
            }
        );
    }

    #[test]
    fn verified_is_the_only_healthy_verdict() {
        let mismatch = SenderIdentityVerdict::Mismatch {
            expected_bot_id: "1471139513983307916".into(),
            observed_bot_id: "1465264645320474637".into(),
        };
        // Independent enumeration of the healthy set: only Verified may be
        // healthy; every other verdict must fail closed. Written per-variant
        // so a new variant added without an explicit assertion stands out.
        assert!(
            SenderIdentityVerdict::Verified {
                bot_id: "1471139513983307916".into()
            }
            .is_verified()
        );
        assert!(!mismatch.clone().is_verified());
        assert!(!SenderIdentityVerdict::NotConfigured.is_verified());
        assert!(!SenderIdentityVerdict::NoToken.is_verified());
        assert!(!SenderIdentityVerdict::InvalidCredential.is_verified());
        assert!(!SenderIdentityVerdict::Forbidden.is_verified());
        assert!(!SenderIdentityVerdict::RateLimited.is_verified());
        assert!(!SenderIdentityVerdict::MalformedResponse.is_verified());
        assert!(!SenderIdentityVerdict::TransportFailure.is_verified());
        // Wrong-but-valid token: the exact regression this contract closes.
        assert!(!mismatch.is_verified());
    }

    #[test]
    fn mismatch_display_carries_both_ids_and_no_secret_material() {
        let verdict = SenderIdentityVerdict::Mismatch {
            expected_bot_id: "1471139513983307916".into(),
            observed_bot_id: "1465264645320474637".into(),
        };
        let text = verdict.to_string();
        assert!(text.contains("expected Discord bot id 1471139513983307916"));
        assert!(text.contains("bot id 1465264645320474637"));
        // Redaction: no credential material may appear. Discord bot tokens
        // always contain two dots separating three base64 segments; the
        // diagnosis text contains none.
        assert!(!text.contains("Bot "));
        assert_eq!(
            text.split('.').count(),
            1,
            "diagnosis must contain no dotted token-like material: {text}"
        );
    }

    #[test]
    fn every_verdict_has_a_stable_reason_code() {
        let verdicts = [
            SenderIdentityVerdict::Verified { bot_id: "1".into() },
            SenderIdentityVerdict::Mismatch {
                expected_bot_id: "1".into(),
                observed_bot_id: "2".into(),
            },
            SenderIdentityVerdict::NotConfigured,
            SenderIdentityVerdict::NoToken,
            SenderIdentityVerdict::InvalidCredential,
            SenderIdentityVerdict::Forbidden,
            SenderIdentityVerdict::RateLimited,
            SenderIdentityVerdict::MalformedResponse,
            SenderIdentityVerdict::TransportFailure,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for verdict in &verdicts {
            assert!(
                seen.insert(verdict.reason_code()),
                "duplicate reason code for {verdict:?}"
            );
            assert!(verdict.reason_code().starts_with("sender_identity_"));
        }
    }
}
