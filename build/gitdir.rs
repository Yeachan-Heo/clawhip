//! Gitdir resolution for linked worktrees, shared by build.rs and its regression test.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve the gitdir path, handling both regular git directories and linked
/// worktrees where .git is a file containing a gitdir pointer.
#[allow(dead_code)]
pub fn resolve_gitdir() -> Option<PathBuf> {
    resolve_gitdir_from(Path::new(".git"))
}

/// Resolve the gitdir path from an explicit .git path location.
/// Used for testing with custom .git locations.
pub fn resolve_gitdir_from(git_path: &Path) -> Option<PathBuf> {
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
            let path = if Path::new(gitdir).is_absolute() {
                // Handle Unix absolute paths (/...), Windows drive-qualified (C:/...), and UNC paths (\\...)
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

/// Parse an absolute path from a gitdir pointer line.
/// This is a pure function that doesn't depend on the filesystem,
/// allowing platform-independent tests of path parsing logic.
#[allow(dead_code)]
pub fn parse_gitdir_pointer(line: &str) -> Option<&str> {
    line.strip_prefix("gitdir:").map(|s| s.trim())
}

/// Check if a path string is absolute using platform-aware logic.
/// This wraps Path::is_absolute() for testability.
#[allow(dead_code)]
pub fn is_absolute_path(path: &str) -> bool {
    Path::new(path).is_absolute()
}

/// Run a git command and return stdout if successful.
/// Used for dirty detection in build.rs and testable from build_rs_worktree.
#[allow(dead_code)]
pub fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Detect if the current working directory has uncommitted changes.
/// This is the logic used by build.rs detect_dirty().
#[allow(dead_code)]
pub fn detect_dirty_status() -> Option<bool> {
    run_git(&["status", "--porcelain", "--untracked-files=no"]).map(|output| !output.is_empty())
}
