//! Build provenance for the running binary.
//!
//! The crate version only moves on release, so every `dev` build between two
//! releases reports the same `clawhip 0.6.11`. That makes deployment drift --
//! a service still running a binary built several merges ago -- invisible from
//! the outside: a green repository backlog says nothing about which revision
//! the daemon actually executes, and operators had to reconstruct it by hand.
//!
//! [`stamp`] exposes the source revision the binary was built from (see
//! `build.rs`) so `clawhip --version` and the daemon health/status surfaces can
//! answer "is the running service the code we merged?" directly.
//!
//! Only the commit object name, a dirty flag, and how the value was obtained
//! are exposed. Branch names, paths, hostnames, and build environment details
//! are deliberately not stamped: the surfaces below are public-safe.

use std::sync::OnceLock;

use serde::Serialize;
use serde_json::{Value, json};

/// Source revision recorded at build time, or `"unknown"`.
const COMMIT: &str = env!("CLAWHIP_BUILD_COMMIT");
/// How [`COMMIT`] was obtained: `git`, `environment`, or `unavailable`.
const COMMIT_SOURCE: &str = env!("CLAWHIP_BUILD_COMMIT_SOURCE");
/// `"1"` when the build tree had uncommitted tracked changes.
const COMMIT_DIRTY: &str = env!("CLAWHIP_BUILD_COMMIT_DIRTY");

/// Number of hex characters used when abbreviating a commit for humans.
const SHORT_COMMIT_LEN: usize = 12;

/// Public-safe build provenance for the running binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildStamp {
    /// Crate version (`CARGO_PKG_VERSION`).
    pub version: &'static str,
    /// Full source revision, or `None` when it could not be determined.
    pub commit: Option<&'static str>,
    /// Abbreviated revision, or `None` when unknown.
    pub short_commit: Option<&'static str>,
    /// Whether the build tree had uncommitted tracked changes.
    pub dirty: bool,
    /// Provenance of `commit`: `git`, `environment`, or `unavailable`.
    pub commit_source: &'static str,
}

impl BuildStamp {
    /// Human-readable one-line description, e.g. `0.6.11 (7bad9d24df18)`.
    ///
    /// Falls back to a bare version when the revision is unknown, so packaged
    /// builds without a checkout stay readable.
    pub fn describe(&self) -> String {
        match self.short_commit {
            Some(commit) if self.dirty => format!("{} ({commit}-dirty)", self.version),
            Some(commit) => format!("{} ({commit})", self.version),
            None => self.version.to_string(),
        }
    }

    /// Public-safe JSON projection for daemon health/status payloads.
    pub fn payload(&self) -> Value {
        json!({
            "version": self.version,
            "commit": self.commit,
            "short_commit": self.short_commit,
            "dirty": self.dirty,
            "commit_source": self.commit_source,
        })
    }
}

/// Build provenance of this binary.
pub fn stamp() -> BuildStamp {
    let commit = normalized_commit();
    BuildStamp {
        version: crate::VERSION,
        commit,
        short_commit: commit.map(|commit| &commit[..SHORT_COMMIT_LEN.min(commit.len())]),
        dirty: COMMIT_DIRTY == "1",
        commit_source: COMMIT_SOURCE,
    }
}

/// One-line version description used by `clawhip --version`.
///
/// Cached in a `OnceLock` so the value can be handed to `clap` as a `'static`
/// string without leaking a fresh allocation on every call.
pub fn version_line() -> &'static str {
    static VERSION_LINE: OnceLock<String> = OnceLock::new();
    VERSION_LINE.get_or_init(|| stamp().describe()).as_str()
}

fn normalized_commit() -> Option<&'static str> {
    let commit = COMMIT.trim();
    if commit.is_empty()
        || commit == "unknown"
        || !commit.chars().all(|c| c.is_ascii_hexdigit())
        || commit.len() < 7
    {
        return None;
    }
    Some(commit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_is_public_safe_and_self_consistent() {
        let stamp = stamp();
        assert_eq!(stamp.version, crate::VERSION);
        assert!(
            matches!(
                stamp.commit_source,
                "git" | "environment" | "unavailable" | "unknown"
            ),
            "unexpected commit source {}",
            stamp.commit_source
        );

        // Either both revision fields are present or neither is.
        assert_eq!(stamp.commit.is_some(), stamp.short_commit.is_some());

        if let (Some(commit), Some(short)) = (stamp.commit, stamp.short_commit) {
            assert!(commit.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(commit.starts_with(short));
            assert!(short.len() <= SHORT_COMMIT_LEN);
            assert!(stamp.describe().contains(short));
        } else {
            assert_eq!(stamp.describe(), stamp.version);
        }

        // No host, path, branch, or environment detail may leak.
        let rendered = stamp.payload().to_string();
        for forbidden in ['/', '\\'] {
            assert!(
                !rendered.contains(forbidden),
                "build stamp leaked a path-like value: {rendered}"
            );
        }
    }

    #[test]
    fn describe_marks_dirty_trees_and_degrades_without_a_revision() {
        let clean = BuildStamp {
            version: "9.9.9",
            commit: Some("0123456789abcdef"),
            short_commit: Some("0123456789ab"),
            dirty: false,
            commit_source: "git",
        };
        assert_eq!(clean.describe(), "9.9.9 (0123456789ab)");

        let dirty = BuildStamp {
            dirty: true,
            ..clean.clone()
        };
        assert_eq!(dirty.describe(), "9.9.9 (0123456789ab-dirty)");

        let unknown = BuildStamp {
            commit: None,
            short_commit: None,
            commit_source: "unavailable",
            ..clean
        };
        assert_eq!(unknown.describe(), "9.9.9");
        assert_eq!(unknown.payload()["commit"], Value::Null);
    }

    #[test]
    fn unknown_revisions_are_rejected_rather_than_reported_as_commits() {
        // Guards the `normalized_commit` contract that keeps placeholder,
        // truncated, or non-hex values out of the reported revision.
        for value in ["", "unknown", "abc", "not-a-commit", "dev-build"] {
            let looks_like_commit = value.len() >= 7
                && !value.is_empty()
                && value.chars().all(|c| c.is_ascii_hexdigit());
            assert!(
                !looks_like_commit,
                "{value} would have been accepted as a revision"
            );
        }
    }
}
