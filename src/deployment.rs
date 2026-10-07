//! Deployment-drift detection.
//!
//! Reporting the build revision (see [`crate::build_info`]) made drift
//! *visible*, but an operator still had to notice it: compare the running
//! binary's revision against the source revision by hand, every time. That
//! manual step is exactly the one that keeps being skipped, which is how a
//! merged fix can keep looking broken in production while the repository
//! backlog is green.
//!
//! This module closes the loop. When the daemon knows where its source
//! checkout lives (`[update].repo_root`, already required for self-update), it
//! periodically compares that checkout's `HEAD` against the revision its own
//! binary was built from and raises one alert per newly observed drift pair.
//!
//! Design constraints:
//!
//! - **Never guess.** With no configured checkout, no readable `HEAD`, or a
//!   binary that carries no stamped revision, the state is `unknown` and no
//!   alert is emitted. An unknown state is reported, not alerted.
//! - **No repeat spam.** An alert fires once per `(binary, source)` revision
//!   pair; the pair only changes when someone redeploys or moves the checkout.
//! - **Public-safe.** Only commit object names are exposed. The checkout path
//!   never appears in an event payload or health surface.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use crate::build_info::{self, BuildStamp};
use crate::config::AppConfig;
use crate::events::{IncomingEvent, MessageFormat};

/// Lower bound for the drift poll, mirroring the update checker's floor.
const MIN_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Default drift poll interval.
const CHECK_INTERVAL: Duration = Duration::from_secs(300);
/// Characters used when abbreviating a revision for humans.
const SHORT_LEN: usize = 12;

/// Whether the running binary matches the source checkout it was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftState {
    /// Binary revision equals the checkout `HEAD`.
    Match,
    /// Binary revision differs from the checkout `HEAD`.
    Drift,
    /// Not enough information to compare; never alerted on.
    Unknown,
}

impl DriftState {
    pub fn as_str(self) -> &'static str {
        match self {
            DriftState::Match => "match",
            DriftState::Drift => "drift",
            DriftState::Unknown => "unknown",
        }
    }
}

/// Result of one drift comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DriftReport {
    pub state: DriftState,
    /// Revision the running binary was built from, if stamped.
    pub binary_commit: Option<String>,
    /// Revision the source checkout currently points at, if readable.
    pub source_commit: Option<String>,
    /// Why the comparison could not be made, when `state` is `unknown`.
    pub reason: Option<&'static str>,
}

impl DriftReport {
    fn unknown(reason: &'static str, binary: Option<String>, source: Option<String>) -> Self {
        Self {
            state: DriftState::Unknown,
            binary_commit: binary,
            source_commit: source,
            reason: Some(reason),
        }
    }

    /// Public-safe JSON projection. Contains revisions only, never paths.
    pub fn payload(&self) -> Value {
        json!({
            "state": self.state.as_str(),
            "binary_commit": self.binary_commit,
            "source_commit": self.source_commit,
            "reason": self.reason,
        })
    }

    /// Alert text for a drifted deployment.
    fn alert_message(&self) -> Option<String> {
        if self.state != DriftState::Drift {
            return None;
        }
        let binary = short(self.binary_commit.as_deref()?);
        let source = short(self.source_commit.as_deref()?);
        Some(format!(
            "clawhip deployment drift: running binary was built from {binary}, \
             but the source checkout is at {source}.\n\
             The running service is not the code in the checkout; rebuild and restart to deploy it."
        ))
    }

    /// Identity of this drift observation, used to alert only once per pair.
    fn pair(&self) -> Option<(String, String)> {
        Some((self.binary_commit.clone()?, self.source_commit.clone()?))
    }
}

/// Compare a build stamp against a source revision.
///
/// Pure so the decision table is testable without a checkout: both revisions
/// must be known before a verdict is possible, and comparison is
/// prefix-tolerant so an abbreviated stamp (from a packaging override) still
/// matches a full `HEAD`.
pub fn compare(stamp: &BuildStamp, source_commit: Option<&str>) -> DriftReport {
    let binary = stamp.commit.map(str::to_string);
    let source = source_commit.map(str::to_string);

    let (Some(binary_commit), Some(source_commit)) = (binary.clone(), source.clone()) else {
        let reason = if binary.is_none() {
            "binary carries no build revision"
        } else {
            "source revision unavailable"
        };
        return DriftReport::unknown(reason, binary, source);
    };

    // A dirty build tree cannot be attributed to a revision: the binary is
    // provably not exactly `binary_commit`, so refuse to call it a match.
    if stamp.dirty {
        return DriftReport::unknown(
            "binary was built from a modified tree",
            Some(binary_commit),
            Some(source_commit),
        );
    }

    let matched =
        binary_commit.starts_with(&source_commit) || source_commit.starts_with(&binary_commit);
    DriftReport {
        state: if matched {
            DriftState::Match
        } else {
            DriftState::Drift
        },
        binary_commit: Some(binary_commit),
        source_commit: Some(source_commit),
        reason: None,
    }
}

/// Latest drift observation, shared with the health surface.
pub type SharedDriftReport = Arc<RwLock<Option<DriftReport>>>;

/// Process-global drift observation.
///
/// Mirrors the shared-handle pattern used for GitHub monitor auth status, so
/// the health surface can read the latest observation without threading a new
/// field through `AppState` and every one of its construction sites.
pub fn shared_drift_report() -> SharedDriftReport {
    static REPORT: OnceLock<SharedDriftReport> = OnceLock::new();
    REPORT.get_or_init(|| Arc::new(RwLock::new(None))).clone()
}

/// Independent handle, for tests that must not observe global state.
#[cfg(test)]
pub fn new_shared_drift_report() -> SharedDriftReport {
    Arc::new(RwLock::new(None))
}

/// Read `HEAD` of the source checkout, if one is configured and readable.
async fn source_commit(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (commit.len() >= 7 && commit.chars().all(|c| c.is_ascii_hexdigit())).then_some(commit)
}

/// Observe drift once, publishing the result for the health surface.
pub async fn observe(repo_root: Option<&Path>, report: &SharedDriftReport) -> DriftReport {
    let stamp = build_info::stamp();
    let observed = match repo_root {
        Some(root) => compare(&stamp, source_commit(root).await.as_deref()),
        None => DriftReport::unknown(
            "no source checkout configured",
            stamp.commit.map(str::to_string),
            None,
        ),
    };
    *report.write().await = Some(observed.clone());
    observed
}

/// Periodically compare the running binary against its source checkout and
/// alert once per newly observed drift pair.
pub async fn run_checker(
    config: Arc<AppConfig>,
    tx: mpsc::Sender<IncomingEvent>,
    report: SharedDriftReport,
) {
    let Some(repo_root) = config.effective_update_repo_root().map(PathBuf::from) else {
        // Nothing to compare against; record why and stay quiet.
        observe(None, &report).await;
        return;
    };

    let mut tick = interval(CHECK_INTERVAL.max(MIN_CHECK_INTERVAL));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut alerted: Option<(String, String)> = None;

    loop {
        tick.tick().await;
        let observed = observe(Some(&repo_root), &report).await;

        let Some(message) = observed.alert_message() else {
            continue;
        };
        let pair = observed.pair();
        if pair == alerted {
            continue;
        }

        let event = IncomingEvent::custom(config.update.channel.clone(), message)
            .with_format(Some(MessageFormat::Alert));
        if let Err(error) = tx.send(event).await {
            eprintln!("clawhip deployment drift: failed to send notification: {error}");
            continue;
        }
        alerted = pair;
    }
}

fn short(commit: &str) -> &str {
    &commit[..SHORT_LEN.min(commit.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(commit: Option<&'static str>, dirty: bool) -> BuildStamp {
        BuildStamp {
            version: "9.9.9",
            commit,
            short_commit: commit.map(|commit| &commit[..SHORT_LEN.min(commit.len())]),
            dirty,
            commit_source: if commit.is_some() {
                "git"
            } else {
                "unavailable"
            },
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn identical_revisions_are_a_match_and_never_alert() {
        let report = compare(&stamp(Some(A), false), Some(A));
        assert_eq!(report.state, DriftState::Match);
        assert_eq!(report.reason, None);
        assert!(report.alert_message().is_none());
    }

    #[test]
    fn differing_revisions_drift_and_alert_with_both_revisions() {
        let report = compare(&stamp(Some(A), false), Some(B));
        assert_eq!(report.state, DriftState::Drift);
        let message = report.alert_message().expect("drift must alert");
        assert!(message.contains(&A[..SHORT_LEN]));
        assert!(message.contains(&B[..SHORT_LEN]));
        assert!(message.contains("rebuild and restart"));
    }

    #[test]
    fn abbreviated_stamps_still_match_a_full_head() {
        // Packaging pipelines may stamp an abbreviated revision.
        let report = compare(&stamp(Some("aaaaaaaaaaaa"), false), Some(A));
        assert_eq!(report.state, DriftState::Match);
    }

    #[test]
    fn missing_information_is_unknown_rather_than_drift() {
        let cases = [
            (
                compare(&stamp(None, false), Some(A)),
                "binary carries no build revision",
            ),
            (
                compare(&stamp(Some(A), false), None),
                "source revision unavailable",
            ),
            (
                compare(&stamp(Some(A), true), Some(B)),
                "binary was built from a modified tree",
            ),
        ];
        for (report, expected_reason) in cases {
            assert_eq!(report.state, DriftState::Unknown);
            assert_eq!(report.reason, Some(expected_reason));
            assert!(
                report.alert_message().is_none(),
                "unknown state must never alert"
            );
        }
    }

    #[test]
    fn payload_is_public_safe_and_reports_state() {
        let report = compare(&stamp(Some(A), false), Some(B));
        let payload = report.payload();
        assert_eq!(payload["state"], Value::from("drift"));
        assert_eq!(payload["binary_commit"], Value::from(A));
        assert_eq!(payload["source_commit"], Value::from(B));
        assert_eq!(payload["reason"], Value::Null);

        let rendered = payload.to_string();
        for forbidden in ['/', '\\'] {
            assert!(
                !rendered.contains(forbidden),
                "drift payload leaked a path-like value: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn observe_without_a_checkout_records_unknown_and_stays_quiet() {
        let report = new_shared_drift_report();
        let observed = observe(None, &report).await;
        assert_eq!(observed.state, DriftState::Unknown);
        assert_eq!(observed.reason, Some("no source checkout configured"));
        assert_eq!(
            report.read().await.as_ref().map(|report| report.state),
            Some(DriftState::Unknown)
        );
    }

    /// Minimal on-disk checkout whose `HEAD` resolves to `commit`.
    ///
    /// Written directly instead of shelling out to `git init`/`git commit`:
    /// the daemon only ever reads `HEAD`, and spawning extra child processes
    /// during the shared test run starves timing-sensitive adapter tests.
    fn checkout_at(root: &Path, commit: &str) {
        let git_dir = root.join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::create_dir_all(git_dir.join("refs").join("heads")).unwrap();
        std::fs::write(git_dir.join("HEAD"), format!("{commit}\n")).unwrap();
    }

    #[tokio::test]
    async fn observe_reads_head_from_a_checkout_and_reports_drift() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        checkout_at(root, B);

        let head = source_commit(root).await.expect("HEAD must be readable");
        assert_eq!(head, B);

        let report = new_shared_drift_report();
        let observed = observe(Some(root), &report).await;
        assert_eq!(observed.source_commit.as_deref(), Some(B));
        // The test binary's own stamp decides match/drift, so only assert the
        // parts that do not depend on how this test run was built.
        assert_ne!(observed.state, DriftState::Match);
    }

    #[tokio::test]
    async fn repo_root_alone_is_sufficient_for_health_comparison() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        std::fs::create_dir(&root).unwrap();
        checkout_at(&root, B);
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"clawhip\"\n").unwrap();
        let config_path = dir.path().join("config.toml");
        crate::source_checkout::persist(&config_path, &root).unwrap();
        let config = AppConfig::load_or_default(&config_path).unwrap();
        assert!(!config.update.enabled);
        assert!(config.update.channel.is_none());

        let report = new_shared_drift_report();
        let observed = observe(config.effective_update_repo_root().map(Path::new), &report).await;

        assert_eq!(observed.source_commit.as_deref(), Some(B));
        assert_ne!(observed.reason, Some("no source checkout configured"));
    }

    #[tokio::test]
    async fn unreadable_checkout_is_unknown_not_drift() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_repo = dir.path().join("nope");
        std::fs::create_dir_all(&not_a_repo).unwrap();
        assert!(source_commit(&not_a_repo).await.is_none());

        let report = new_shared_drift_report();
        let observed = observe(Some(&not_a_repo), &report).await;
        assert_eq!(observed.state, DriftState::Unknown);
        assert!(observed.alert_message().is_none());
    }
}
