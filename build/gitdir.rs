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

/// Resolve the common git directory for a worktree.
/// For regular repos, returns the gitdir itself.
/// For linked worktrees, returns the common directory that holds refs/heads, packed-refs, etc.
#[allow(dead_code)]
pub fn resolve_common_dir(gitdir: &Path) -> Option<PathBuf> {
    // The commondir file, if it exists, points to the shared git directory.
    let common_dir_file = gitdir.join("commondir");
    if let Ok(contents) = fs::read_to_string(&common_dir_file) {
        let path_str = contents.trim();
        if !path_str.is_empty() {
            let path = if Path::new(path_str).is_absolute() {
                PathBuf::from(path_str)
            } else {
                gitdir.join(path_str)
            };
            if path.is_dir() {
                return Some(path);
            }
        }
    }
    // If no commondir file, return the gitdir itself (regular repo).
    Some(gitdir.to_path_buf())
}

/// Read HEAD file as bytes to preserve non-UTF-8 encoding.
/// Returns the full content including the newline if present.
#[allow(dead_code)]
pub fn read_head_bytes(gitdir: &Path) -> Option<Vec<u8>> {
    let head_path = gitdir.join("HEAD");
    fs::read(&head_path).ok()
}

/// Parse HEAD content to get the symbolic ref if on a branch.
/// HEAD content is like "ref: refs/heads/main\n" for branches,
/// or a commit hash for detached HEAD.
/// Returns the path to the branch ref (e.g., "refs/heads/main") if on a branch.
#[allow(dead_code)]
pub fn parse_head_symref(head_bytes: &[u8]) -> Option<Vec<u8>> {
    const REF_PREFIX: &[u8] = b"ref: ";

    if !head_bytes.starts_with(REF_PREFIX) {
        return None; // Detached HEAD
    }

    // Skip "ref: " prefix and trim trailing whitespace/newline
    let ref_path = &head_bytes[REF_PREFIX.len()..];
    let ref_path = ref_path.trim_ascii();

    if ref_path.is_empty() {
        return None;
    }

    Some(ref_path.to_vec())
}

/// Construct a safe rerun-if-changed directive for a path.
/// If the path is valid UTF-8, use the display form.
/// If it contains non-UTF-8 bytes, emit a warning and use a coarser watch.
#[allow(dead_code)]
pub fn rerun_path_safe(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        match String::from_utf8(bytes.to_vec()) {
            Ok(s) => s,
            Err(_) => {
                // Path contains non-UTF-8 bytes; watch the parent directory instead.
                // This is a safe coarser watch.
                eprintln!(
                    "warning: path contains non-UTF-8 bytes, watching parent directory instead: {:?}",
                    path.parent()
                );
                path.parent()
                    .and_then(|p| p.to_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| ".".to_string())
            }
        }
    }
    #[cfg(not(unix))]
    {
        path.display().to_string()
    }
}
