//! Build-time provenance for deployment-drift detection.
//!
//! `clawhip 0.6.11` is not enough to tell a freshly deployed daemon apart from
//! one running a binary built several merges ago: the crate version only moves
//! on release, so every `dev` build in between reports an identical version
//! string. Operators then have to reconstruct "is the running service actually
//! the code we merged?" by hand.
//!
//! This script stamps the source revision the binary was built from into the
//! binary itself. It is deliberately fail-open: a source tarball, a vendored
//! crates.io build, or a machine without `git` still builds, and simply
//! reports an unknown revision instead of breaking the build.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let (commit, source) = detect_commit();
    let dirty = detect_dirty();

    println!("cargo:rustc-env=CLAWHIP_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=CLAWHIP_BUILD_COMMIT_SOURCE={source}");
    println!(
        "cargo:rustc-env=CLAWHIP_BUILD_COMMIT_DIRTY={}",
        if dirty { "1" } else { "0" }
    );

    // Rebuild when HEAD moves so a stamped binary can never claim an older
    // revision than the tree it was built from.
    // In a linked worktree, .git is a file containing a gitdir pointer,
    // so we need to resolve the actual gitdir location.
    if let Some(gitdir) = resolve_gitdir() {
        if let Ok(head_path) = gitdir.join("HEAD").canonicalize() {
            println!("cargo:rerun-if-changed={}", head_path.display());
        }
        if let Ok(index_path) = gitdir.join("index").canonicalize() {
            println!("cargo:rerun-if-changed={}", index_path.display());
        }
    }
    println!("cargo:rerun-if-env-changed=CLAWHIP_BUILD_COMMIT");
}

/// Resolve the build revision, preferring an explicit override so release and
/// packaging pipelines that build outside a checkout can still stamp a real
/// commit.
fn detect_commit() -> (String, &'static str) {
    if let Some(commit) = sanitized_env("CLAWHIP_BUILD_COMMIT") {
        return (commit, "environment");
    }
    if let Some(commit) = sanitized_env("GITHUB_SHA") {
        return (commit, "environment");
    }
    match git(&["rev-parse", "HEAD"]) {
        Some(commit) if is_hex_commit(&commit) => (commit, "git"),
        _ => ("unknown".to_string(), "unavailable"),
    }
}

fn detect_dirty() -> bool {
    if sanitized_env("CLAWHIP_BUILD_COMMIT").is_some() {
        return false;
    }
    git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|out| !out.is_empty())
}

fn sanitized_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| is_hex_commit(value))
}

/// Only accept plain hex object names. This keeps branch names, refs, ticket
/// text, or anything else operator-supplied out of the stamped value.
fn is_hex_commit(value: &str) -> bool {
    let len = value.len();
    (7..=40).contains(&len) && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolve the gitdir path, handling both regular git directories and linked
/// worktrees where .git is a file containing a gitdir pointer.
fn resolve_gitdir() -> Option<PathBuf> {
    let git_path = Path::new(".git");
    if !git_path.exists() {
        return None;
    }

    // If .git is a directory, return it directly.
    if git_path.is_dir() {
        return Some(git_path.to_path_buf());
    }

    // If .git is a file (linked worktree), read the gitdir pointer.
    let contents = fs::read_to_string(git_path).ok()?;
    for line in contents.lines() {
        if let Some(gitdir) = line.strip_prefix("gitdir:") {
            let gitdir = gitdir.trim();
            let path = if gitdir.starts_with('/') {
                PathBuf::from(gitdir)
            } else {
                // Relative paths are relative to the .git file location
                git_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(gitdir)
            };
            if path.is_dir() {
                return Some(path);
            }
        }
    }
    None
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}
