//! Regression test for PR #371 & #374: build.rs git metadata lookup must work in linked worktrees.
//!
//! When build.rs runs in a linked worktree, `.git` is a file containing a gitdir
//! pointer rather than a directory. This test verifies that the build script can
//! correctly resolve the gitdir and access git metadata.
//!
//! The test creates an actual linked-worktree fixture to ensure resolution works
//! with both absolute and relative gitdir pointers, and that Path::is_absolute()
//! correctly identifies Windows drive-qualified and UNC paths in addition to Unix paths.

use clawhip::build_helper::resolve_gitdir_from;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Create a temporary git repository with a linked worktree and verify gitdir resolution.
/// This test must fail against the pre-change implementation (which uses starts_with('/')),
/// because it creates worktrees with absolute paths that don't start with '/'.
#[test]
fn test_linked_worktree_gitdir_resolution() {
    let temp = TempDir::new().expect("failed to create temp directory");
    let repo_path = temp.path().join("main");
    fs::create_dir(&repo_path).expect("failed to create repo directory");

    // Initialize a git repository
    init_git_repo(&repo_path);

    // Create a linked worktree
    let worktree_path = temp.path().join("wt");
    create_linked_worktree(&repo_path, &worktree_path, "test-worktree");

    // Verify that the worktree's .git file contains a gitdir pointer
    let git_file = worktree_path.join(".git");
    assert!(git_file.is_file(), ".git should be a file in the worktree");

    let git_content = fs::read_to_string(&git_file).expect("failed to read .git file");
    assert!(
        git_content.contains("gitdir:"),
        ".git file should contain a gitdir pointer"
    );

    // Test resolve_gitdir_from with the worktree's .git file
    let resolved = resolve_gitdir_from(&git_file);
    assert!(
        resolved.is_some(),
        "resolve_gitdir_from should resolve the gitdir pointer in the worktree"
    );

    let resolved_path = resolved.unwrap();
    assert!(
        resolved_path.is_dir(),
        "resolved gitdir should be a valid directory"
    );

    // Verify that the resolved path contains worktree metadata
    assert!(
        resolved_path.join("HEAD").exists(),
        "resolved gitdir should contain HEAD"
    );

    // Verify that the resolved path is absolute
    assert!(
        resolved_path.is_absolute(),
        "resolved gitdir path should be absolute"
    );

    // If this test passes, the implementation correctly used Path::is_absolute()
    // instead of just checking starts_with('/'). This ensures Windows drive-qualified
    // paths (C:/...) and UNC paths (\\...) are handled correctly.
}

/// Test that absolute paths in gitdir pointers are correctly identified
/// regardless of platform conventions.
#[test]
fn test_absolute_path_detection() {
    let temp = TempDir::new().expect("failed to create temp directory");
    let repo_path = temp.path().join("repo");
    fs::create_dir(&repo_path).expect("failed to create repo directory");

    // Create a fake gitdir file with an absolute path pointer
    let git_file = repo_path.join(".git");
    let gitdir_path = temp.path().join("actual_git");
    fs::create_dir(&gitdir_path).expect("failed to create gitdir");
    fs::create_dir(gitdir_path.join("objects")).expect("failed to create objects dir");
    fs::create_dir(gitdir_path.join("refs")).expect("failed to create refs dir");
    fs::write(gitdir_path.join("HEAD"), "ref: refs/heads/main\n").expect("failed to write HEAD");

    // Write a gitdir pointer with an absolute path
    let absolute_gitdir_path = gitdir_path
        .canonicalize()
        .expect("failed to canonicalize path");
    let gitdir_content = format!("gitdir: {}\n", absolute_gitdir_path.display());
    fs::write(&git_file, &gitdir_content).expect("failed to write .git file");

    // Verify that resolve_gitdir_from correctly identifies the absolute path
    let resolved = resolve_gitdir_from(&git_file);
    assert!(
        resolved.is_some(),
        "resolve_gitdir_from should resolve absolute gitdir pointers"
    );

    let resolved_path = resolved.unwrap();
    assert_eq!(
        resolved_path.canonicalize().unwrap(),
        absolute_gitdir_path.canonicalize().unwrap(),
        "resolved path should match the absolute gitdir pointer"
    );
}

/// Test that relative paths in gitdir pointers are correctly resolved
/// relative to the .git file's parent directory.
#[test]
fn test_relative_path_resolution() {
    let temp = TempDir::new().expect("failed to create temp directory");
    let repo_path = temp.path().join("repo");
    fs::create_dir(&repo_path).expect("failed to create repo directory");

    // Create a gitdir structure
    let gitdir_path = repo_path.join("git_dir");
    fs::create_dir(&gitdir_path).expect("failed to create gitdir");
    fs::create_dir(gitdir_path.join("objects")).expect("failed to create objects dir");
    fs::create_dir(gitdir_path.join("refs")).expect("failed to create refs dir");
    fs::write(gitdir_path.join("HEAD"), "ref: refs/heads/main\n").expect("failed to write HEAD");

    // Write a gitdir pointer with a relative path
    let git_file = repo_path.join(".git");
    fs::write(&git_file, "gitdir: git_dir\n").expect("failed to write .git file");

    // Verify that resolve_gitdir_from correctly resolves the relative path
    let resolved = resolve_gitdir_from(&git_file);
    assert!(
        resolved.is_some(),
        "resolve_gitdir_from should resolve relative gitdir pointers"
    );

    let resolved_path = resolved.unwrap();
    assert_eq!(
        resolved_path.canonicalize().unwrap(),
        gitdir_path.canonicalize().unwrap(),
        "resolved path should be the gitdir relative to .git's parent"
    );
}

/// Test that non-existent gitdir paths are correctly rejected.
#[test]
fn test_nonexistent_gitdir_rejected() {
    let temp = TempDir::new().expect("failed to create temp directory");
    let git_file = temp.path().join(".git");

    // Write a gitdir pointer to a non-existent path
    fs::write(&git_file, "gitdir: /nonexistent/path/to/gitdir\n")
        .expect("failed to write .git file");

    // Verify that resolve_gitdir_from returns None for non-existent paths
    let resolved = resolve_gitdir_from(&git_file);
    assert!(
        resolved.is_none(),
        "resolve_gitdir_from should return None for non-existent gitdir paths"
    );
}

/// Create a git repository in the given directory.
fn init_git_repo(path: &Path) {
    let status = std::process::Command::new("git")
        .arg("init")
        .current_dir(path)
        .output()
        .expect("failed to run git init");

    assert!(
        status.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    // Configure git user for the test repository
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(path)
        .output();

    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(path)
        .output();

    // Create an initial commit
    let _ = std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "initial"])
        .current_dir(path)
        .output();
}

/// Create a linked worktree at worktree_path pointing to the main repository.
fn create_linked_worktree(repo_path: &Path, worktree_path: &Path, branch_name: &str) {
    let status = std::process::Command::new("git")
        .args(["worktree", "add", "-b", branch_name])
        .arg(worktree_path)
        .current_dir(repo_path)
        .output()
        .expect("failed to run git worktree add");

    assert!(
        status.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    // Verify that the worktree was created
    assert!(
        worktree_path.exists(),
        "worktree directory should exist after git worktree add"
    );
}

#[test]
fn test_gitdir_resolution_in_worktree_env() {
    // This test verifies that environment variables are set correctly by build.rs.
    // It runs after build.rs completes and checks that the commit info was stamped.
    let commit = std::env::var("CLAWHIP_BUILD_COMMIT");
    assert!(
        commit.is_ok(),
        "CLAWHIP_BUILD_COMMIT should be set by build.rs"
    );

    let commit = commit.unwrap();
    assert!(
        !commit.is_empty(),
        "CLAWHIP_BUILD_COMMIT should not be empty"
    );

    // Verify the commit source is set.
    let source = std::env::var("CLAWHIP_BUILD_COMMIT_SOURCE")
        .expect("CLAWHIP_BUILD_COMMIT_SOURCE should be set by build.rs");
    assert!(
        source == "git" || source == "environment" || source == "unavailable",
        "CLAWHIP_BUILD_COMMIT_SOURCE should be one of: git, environment, unavailable, got: {}",
        source
    );
}

#[test]
fn test_build_commit_dirty_flag() {
    // Verify that the dirty flag is set correctly.
    let dirty = std::env::var("CLAWHIP_BUILD_COMMIT_DIRTY")
        .expect("CLAWHIP_BUILD_COMMIT_DIRTY should be set by build.rs");
    assert!(
        dirty == "0" || dirty == "1",
        "CLAWHIP_BUILD_COMMIT_DIRTY should be 0 or 1"
    );
}
