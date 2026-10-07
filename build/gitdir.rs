//! Gitdir resolution for linked worktrees, shared by build.rs and its regression test.

use std::fs;
use std::path::{Path, PathBuf};

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
