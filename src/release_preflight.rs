//! Release consistency preflight.
//!
//! Guards the release path against the kind of drift that produced the
//! `v0.6.4` dogfood failure (fresh GitHub Release + broken
//! `cargo publish --dry-run --locked` because `Cargo.lock` was stale). The
//! checks here are intentionally cheap so they can run locally *and* in CI
//! before a tag is pushed:
//!
//! 1. `Cargo.toml` `[package].version` matches the intended release version.
//! 2. `Cargo.lock` already records that version for the `clawhip` package
//!    (i.e. the lockfile is fresh for the bump).
//! 3. `CHANGELOG.md` has a concrete release entry for that version — not
//!    still `Unreleased`.
//!
//! All logic is kept pure over file contents so it is easy to unit-test
//! without touching the filesystem. `run` handles I/O and reporting.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Outcome of a single preflight check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

impl CheckResult {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: true,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            passed: false,
            detail: detail.into(),
        }
    }
}

/// Full report for all preflight checks.
#[derive(Debug, Clone)]
pub struct PreflightReport {
    pub version: String,
    pub checks: Vec<CheckResult>,
}

impl PreflightReport {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|check| check.passed)
    }

    pub fn render(&self) -> String {
        let mut out = format!("release preflight for v{}\n", self.version);
        for check in &self.checks {
            let marker = if check.passed { "ok  " } else { "FAIL" };
            out.push_str(&format!("  [{marker}] {}: {}\n", check.name, check.detail));
        }
        if self.ok() {
            out.push_str("\nall release consistency checks passed.\n");
        } else {
            out.push_str("\nrelease consistency checks FAILED — resolve before tagging.\n");
        }
        out
    }
}

/// Normalize a user-supplied version/tag string into a bare semver.
///
/// Accepts `1.2.3`, `v1.2.3`, `clawhip-v1.2.3`, and `refs/tags/v1.2.3` —
/// matching the tag shapes the release workflow and cargo-dist recognize.
pub fn normalize_version(input: &str) -> String {
    let trimmed = input.trim();
    let without_ref = trimmed.strip_prefix("refs/tags/").unwrap_or(trimmed);
    let after_slash = without_ref.rsplit('/').next().unwrap_or(without_ref);
    let after_dash = after_slash.rsplit('-').next().unwrap_or(after_slash);
    after_dash
        .strip_prefix('v')
        .unwrap_or(after_dash)
        .to_string()
}

#[derive(Debug, Deserialize)]
struct CargoToml {
    package: CargoPackage,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
}

/// Parse `Cargo.toml` and return `(package_name, version)`.
pub fn parse_cargo_toml(contents: &str) -> Result<(String, String), String> {
    let parsed: CargoToml =
        toml::from_str(contents).map_err(|error| format!("failed to parse Cargo.toml: {error}"))?;
    Ok((parsed.package.name, parsed.package.version))
}

/// Check that `Cargo.toml` version matches the intended release version.
pub fn check_cargo_toml(contents: &str, expected_version: &str) -> CheckResult {
    match parse_cargo_toml(contents) {
        Ok((name, version)) if version == expected_version => {
            CheckResult::pass("Cargo.toml version", format!("{name} = {version}"))
        }
        Ok((name, version)) => CheckResult::fail(
            "Cargo.toml version",
            format!(
                "{name} is {version}, expected {expected_version} — bump Cargo.toml before tagging"
            ),
        ),
        Err(error) => CheckResult::fail("Cargo.toml version", error),
    }
}

/// Check that `Cargo.lock` records `expected_version` for `package_name`.
///
/// This catches the exact `v0.6.4` failure mode: `Cargo.toml` gets bumped,
/// but `Cargo.lock` still pins the old version, which makes
/// `cargo publish --dry-run --locked` fail downstream.
pub fn check_cargo_lock(contents: &str, package_name: &str, expected_version: &str) -> CheckResult {
    // Cargo.lock is line-oriented TOML: each `[[package]]` block has a
    // `name = "..."` and `version = "..."` on successive lines. We scan for
    // the block that matches `package_name` and compare its version. Doing
    // this without a full TOML deserialization keeps the check resilient to
    // lockfile format churn across cargo versions.
    let name_needle = format!("name = \"{package_name}\"");
    let mut found_package = false;
    let mut lines = contents.lines().peekable();

    while let Some(line) = lines.next() {
        if line.trim() != name_needle {
            continue;
        }
        found_package = true;
        for lookahead in lines.by_ref().take(4) {
            let trimmed = lookahead.trim();
            if let Some(version) = trimmed
                .strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
            {
                return if version == expected_version {
                    CheckResult::pass(
                        "Cargo.lock freshness",
                        format!("{package_name} = {version}"),
                    )
                } else {
                    CheckResult::fail(
                        "Cargo.lock freshness",
                        format!(
                            "{package_name} is {version}, expected {expected_version} — run `cargo update -p {package_name}` (or a full `cargo build`) and commit Cargo.lock"
                        ),
                    )
                };
            }
        }
        break;
    }

    if found_package {
        CheckResult::fail(
            "Cargo.lock freshness",
            format!(
                "found {package_name} package block but no version line — Cargo.lock looks malformed"
            ),
        )
    } else {
        CheckResult::fail(
            "Cargo.lock freshness",
            format!("no [[package]] entry for {package_name} in Cargo.lock"),
        )
    }
}

/// Check that `CHANGELOG.md` has a concrete release entry for
/// `expected_version` (not still `Unreleased`).
///
/// We accept any heading that contains the version and is not flagged as
/// `Unreleased`. The historical shape is `## 0.6.4 - 2026-04-10`, but we
/// stay lenient to survive small stylistic drift.
pub fn check_changelog(contents: &str, expected_version: &str) -> CheckResult {
    let mut saw_version_heading = false;
    let mut saw_unreleased_for_version = false;

    for line in contents.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('#') {
            continue;
        }
        let heading = trimmed.trim_start_matches('#').trim();
        let contains_version = heading_contains_version(heading, expected_version);
        let is_unreleased = heading.to_ascii_lowercase().contains("unreleased");

        if contains_version && !is_unreleased {
            saw_version_heading = true;
            break;
        }
        if contains_version && is_unreleased {
            saw_unreleased_for_version = true;
        }
    }

    if saw_version_heading {
        CheckResult::pass(
            "CHANGELOG.md entry",
            format!("found concrete heading for {expected_version}"),
        )
    } else if saw_unreleased_for_version {
        CheckResult::fail(
            "CHANGELOG.md entry",
            format!(
                "version {expected_version} is still marked Unreleased — promote the heading before tagging"
            ),
        )
    } else {
        CheckResult::fail(
            "CHANGELOG.md entry",
            format!(
                "no heading found for {expected_version} — add a CHANGELOG entry before tagging"
            ),
        )
    }
}

fn heading_contains_version(heading: &str, version: &str) -> bool {
    // Match either `version` or `vversion` as a delimited token, so `0.6.4`
    // does not accidentally match `10.6.4` or a stray `0.6.40`.
    heading
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
        .any(|token| {
            let normalized = token.strip_prefix('v').unwrap_or(token);
            normalized == version
        })
}

/// Run all preflight checks against files rooted at `repo_root`.
pub fn run_preflight(repo_root: &Path, expected_version: &str) -> Result<PreflightReport, String> {
    let cargo_toml_path = repo_root.join("Cargo.toml");
    let cargo_lock_path = repo_root.join("Cargo.lock");
    let changelog_path = repo_root.join("CHANGELOG.md");

    let cargo_toml = read_required(&cargo_toml_path)?;
    let cargo_lock = read_required(&cargo_lock_path)?;
    let changelog = read_required(&changelog_path)?;

    let (package_name, _) = parse_cargo_toml(&cargo_toml)
        .map_err(|error| format!("{}: {error}", cargo_toml_path.display()))?;

    let checks = vec![
        check_cargo_toml(&cargo_toml, expected_version),
        check_cargo_lock(&cargo_lock, &package_name, expected_version),
        check_changelog(&changelog, expected_version),
    ];

    Ok(PreflightReport {
        version: expected_version.to_string(),
        checks,
    })
}

fn read_required(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("failed to read {}: {error}", path.display()))
}

/// CLI entry point: resolve the version (either user-supplied or inferred
/// from `Cargo.toml`), run all checks, print the report, and return a
/// non-zero status on failure by propagating an error.
pub fn run(repo_root: Option<PathBuf>, version: Option<String>) -> crate::Result<()> {
    let repo_root = match repo_root {
        Some(path) => path,
        None => std::env::current_dir()
            .map_err(|error| format!("failed to resolve current directory: {error}"))?,
    };

    let expected_version = match version {
        Some(raw) => normalize_version(&raw),
        None => {
            let cargo_toml_path = repo_root.join("Cargo.toml");
            let contents = read_required(&cargo_toml_path)?;
            let (_, version) = parse_cargo_toml(&contents)?;
            version
        }
    };

    let report = run_preflight(&repo_root, &expected_version)?;
    print!("{}", report.render());

    if report.ok() {
        Ok(())
    } else {
        Err("release preflight failed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARGO_TOML_SAMPLE: &str = r#"
[package]
name = "clawhip"
version = "0.6.5"
edition = "2024"

[dependencies]
anyhow = "1"
"#;

    const CARGO_LOCK_SAMPLE: &str = r#"
# This file is automatically @generated by Cargo.
version = 4

[[package]]
name = "anyhow"
version = "1.0.99"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "clawhip"
version = "0.6.5"
dependencies = [
 "anyhow",
]

[[package]]
name = "serde"
version = "1.0.219"
"#;

    const CHANGELOG_SAMPLE: &str = r#"
# Changelog

## 0.6.5 - 2026-04-12

### Highlights

- release preflight

## 0.6.4 - 2026-04-10
"#;

    #[test]
    fn normalize_version_accepts_common_tag_shapes() {
        assert_eq!(normalize_version("0.6.5"), "0.6.5");
        assert_eq!(normalize_version("v0.6.5"), "0.6.5");
        assert_eq!(normalize_version("  v0.6.5  "), "0.6.5");
        assert_eq!(normalize_version("refs/tags/v0.6.5"), "0.6.5");
        assert_eq!(normalize_version("clawhip-v0.6.5"), "0.6.5");
        assert_eq!(normalize_version("clawhip/v0.6.5"), "0.6.5");
    }

    #[test]
    fn parse_cargo_toml_extracts_name_and_version() {
        let (name, version) = parse_cargo_toml(CARGO_TOML_SAMPLE).unwrap();
        assert_eq!(name, "clawhip");
        assert_eq!(version, "0.6.5");
    }

    #[test]
    fn check_cargo_toml_passes_on_match() {
        let result = check_cargo_toml(CARGO_TOML_SAMPLE, "0.6.5");
        assert!(result.passed, "detail = {}", result.detail);
    }

    #[test]
    fn check_cargo_toml_fails_on_mismatch() {
        let result = check_cargo_toml(CARGO_TOML_SAMPLE, "0.6.6");
        assert!(!result.passed);
        assert!(result.detail.contains("0.6.5"));
        assert!(result.detail.contains("0.6.6"));
    }

    #[test]
    fn check_cargo_lock_passes_when_package_version_matches() {
        let result = check_cargo_lock(CARGO_LOCK_SAMPLE, "clawhip", "0.6.5");
        assert!(result.passed, "detail = {}", result.detail);
    }

    #[test]
    fn check_cargo_lock_fails_when_lock_is_stale() {
        let stale_lock = CARGO_LOCK_SAMPLE.replace("version = \"0.6.5\"", "version = \"0.6.4\"");
        let result = check_cargo_lock(&stale_lock, "clawhip", "0.6.5");
        assert!(!result.passed);
        assert!(result.detail.contains("0.6.4"));
        assert!(result.detail.contains("cargo update"));
    }

    #[test]
    fn check_cargo_lock_fails_when_package_missing() {
        let result = check_cargo_lock("# empty", "clawhip", "0.6.5");
        assert!(!result.passed);
        assert!(result.detail.contains("no [[package]] entry"));
    }

    #[test]
    fn check_cargo_lock_ignores_other_packages_with_matching_version() {
        let lock = r#"
[[package]]
name = "serde"
version = "0.6.5"

[[package]]
name = "clawhip"
version = "0.6.4"
"#;
        let result = check_cargo_lock(lock, "clawhip", "0.6.5");
        assert!(!result.passed, "should not match on serde's version");
        assert!(result.detail.contains("0.6.4"));
    }

    #[test]
    fn check_changelog_passes_on_concrete_entry() {
        let result = check_changelog(CHANGELOG_SAMPLE, "0.6.5");
        assert!(result.passed, "detail = {}", result.detail);
    }

    #[test]
    fn check_changelog_fails_when_still_unreleased() {
        let changelog = r#"
# Changelog

## 0.6.5 - Unreleased

- wip
"#;
        let result = check_changelog(changelog, "0.6.5");
        assert!(!result.passed);
        assert!(result.detail.contains("Unreleased"));
    }

    #[test]
    fn check_changelog_fails_when_version_missing() {
        let result = check_changelog(CHANGELOG_SAMPLE, "0.7.0");
        assert!(!result.passed);
        assert!(result.detail.contains("no heading"));
    }

    #[test]
    fn check_changelog_does_not_match_on_substring_versions() {
        let changelog = r#"
# Changelog

## 10.6.4 - 2026-04-10

- decoy
"#;
        let result = check_changelog(changelog, "0.6.4");
        assert!(!result.passed, "0.6.4 must not match 10.6.4");
    }

    #[test]
    fn run_preflight_reports_all_checks_for_matching_repo() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), CARGO_TOML_SAMPLE).unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), CARGO_LOCK_SAMPLE).unwrap();
        std::fs::write(dir.path().join("CHANGELOG.md"), CHANGELOG_SAMPLE).unwrap();

        let report = run_preflight(dir.path(), "0.6.5").unwrap();
        assert!(report.ok(), "report = {}", report.render());
        assert_eq!(report.checks.len(), 3);
    }

    #[test]
    fn run_preflight_fails_when_lock_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), CARGO_TOML_SAMPLE).unwrap();
        std::fs::write(
            dir.path().join("Cargo.lock"),
            CARGO_LOCK_SAMPLE.replace("version = \"0.6.5\"", "version = \"0.6.4\""),
        )
        .unwrap();
        std::fs::write(dir.path().join("CHANGELOG.md"), CHANGELOG_SAMPLE).unwrap();

        let report = run_preflight(dir.path(), "0.6.5").unwrap();
        assert!(!report.ok());
        let rendered = report.render();
        assert!(rendered.contains("FAIL"));
        assert!(rendered.contains("Cargo.lock freshness"));
    }

    #[test]
    fn run_preflight_surfaces_missing_files_as_error() {
        let dir = tempfile::tempdir().unwrap();
        // Only write Cargo.toml; other files missing.
        std::fs::write(dir.path().join("Cargo.toml"), CARGO_TOML_SAMPLE).unwrap();
        let err = run_preflight(dir.path(), "0.6.5").unwrap_err();
        assert!(err.contains("failed to read"));
    }

    #[test]
    fn preflight_report_render_lists_pass_and_fail_markers() {
        let report = PreflightReport {
            version: "0.6.5".into(),
            checks: vec![CheckResult::pass("a", "ok"), CheckResult::fail("b", "nope")],
        };
        let rendered = report.render();
        assert!(rendered.contains("[ok  ] a"));
        assert!(rendered.contains("[FAIL] b"));
        assert!(rendered.contains("FAILED"));
    }
}
