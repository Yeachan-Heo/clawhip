//! Gitdir resolution for linked worktrees, shared by build.rs and its regression test.

#[cfg(unix)]
use std::ffi::OsStr;
use std::fs;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
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
    // Read as bytes to avoid requiring UTF-8 encoding.
    let contents = fs::read(git_path).ok()?;

    // Process line by line, looking for the gitdir: pointer.
    // We need to handle lines that might not be valid UTF-8 on Unix systems.
    for line_bytes in contents.split(|&b| b == b'\n') {
        if line_bytes.is_empty() {
            continue;
        }

        // Try to find the "gitdir:" prefix (ASCII, safe to check in bytes)
        if line_bytes.starts_with(b"gitdir:") {
            // Extract the path part after "gitdir:" and strip whitespace
            let path_bytes = &line_bytes[7..]; // Skip "gitdir:"
            let path_bytes = path_bytes.trim_ascii();

            #[cfg(unix)]
            {
                // On Unix, paths are arbitrary bytes; use OsStr
                let path_osstr = OsStr::from_bytes(path_bytes);
                let path = if Path::new(path_osstr).is_absolute() {
                    PathBuf::from(path_osstr)
                } else {
                    git_path
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(path_osstr)
                };
                if path.is_dir() {
                    return Some(path);
                }
            }

            #[cfg(not(unix))]
            {
                // On non-Unix (e.g., Windows), decode to string; paths should be UTF-8
                if let Ok(path_str) = String::from_utf8(path_bytes.to_vec()) {
                    let path = if Path::new(&path_str).is_absolute() {
                        PathBuf::from(&path_str)
                    } else {
                        git_path
                            .parent()
                            .unwrap_or_else(|| Path::new("."))
                            .join(&path_str)
                    };
                    if path.is_dir() {
                        return Some(path);
                    }
                }
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
