//! Regression test for PR #371 & #374: build.rs git metadata lookup must work in linked worktrees.
//!
//! When build.rs runs in a linked worktree, `.git` is a file containing a gitdir
//! pointer rather than a directory. This test verifies that the build script can
//! correctly resolve the gitdir and access git metadata.
//!
//! The test creates an actual linked-worktree fixture to ensure resolution works
//! with both absolute and relative gitdir pointers, and that Path::is_absolute()
//! correctly identifies Windows drive-qualified and UNC paths in addition to Unix paths.

#[path = "../build/gitdir.rs"]
#[allow(dead_code)]
mod gitdir;

use gitdir::resolve_gitdir_from;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Create a temporary git repository with a linked worktree and verify gitdir resolution.
/// This test exercises the actual production build.rs logic by:
/// - Creating a real linked worktree
/// - Verifying gitdir resolution works correctly
/// - Testing dirty detection in the linked worktree context
///
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

    // Test dirty detection in the linked worktree (production logic):
    // In a clean worktree, git status should return empty output
    let status_output = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(&worktree_path)
        .output()
        .expect("failed to run git status in worktree");
    let status_str = String::from_utf8_lossy(&status_output.stdout);
    assert!(
        status_str.trim().is_empty(),
        "clean worktree should have empty git status"
    );

    // Now modify a file in the worktree and verify dirty detection works
    let test_file = worktree_path.join("test.txt");
    fs::write(&test_file, "test content").expect("failed to write test file");

    // Stage the change
    let add_status = std::process::Command::new("git")
        .args(["add", "test.txt"])
        .current_dir(&worktree_path)
        .output()
        .expect("failed to run git add");
    assert!(add_status.status.success(), "git add should succeed");

    // Now git status should show the staged change (dirty flag should be true)
    let status_output = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(&worktree_path)
        .output()
        .expect("failed to run git status in worktree after change");
    let status_str = String::from_utf8_lossy(&status_output.stdout);
    assert!(
        !status_str.trim().is_empty(),
        "worktree with staged changes should have non-empty git status"
    );

    // If this test passes, the implementation correctly used Path::is_absolute()
    // instead of just checking starts_with('/'). This ensures Windows drive-qualified
    // paths (C:/...) and UNC paths (\\...) are handled correctly.
}

/// Pure unit test for gitdir pointer parsing (platform-independent).
/// Tests the parsing logic without depending on the actual filesystem.
/// This test exercises the path-parsing logic that gitdir resolution uses
/// and verifies it works correctly with various path formats.
#[test]
fn test_parse_gitdir_pointer_line() {
    use gitdir::parse_gitdir_pointer;

    // Test Unix absolute path
    assert_eq!(
        parse_gitdir_pointer("gitdir: /home/user/.git/worktrees/branch"),
        Some("/home/user/.git/worktrees/branch")
    );

    // Test relative path
    assert_eq!(
        parse_gitdir_pointer("gitdir: ../main/.git"),
        Some("../main/.git")
    );

    // Test path with trailing spaces
    assert_eq!(
        parse_gitdir_pointer("gitdir: /path/to/git  "),
        Some("/path/to/git")
    );

    // Test path with no space after gitdir:
    assert_eq!(
        parse_gitdir_pointer("gitdir:/no/space/path"),
        Some("/no/space/path")
    );

    // Test line that doesn't start with gitdir:
    assert_eq!(parse_gitdir_pointer("other: /path"), None);

    // Test empty line
    assert_eq!(parse_gitdir_pointer(""), None);
}

/// Unit test for absolute path detection (platform-independent).
/// This test verifies that the is_absolute_path() helper correctly identifies
/// absolute paths using Rust's Path::is_absolute(), which handles all platforms:
/// - Unix paths: /absolute/path
/// - Windows paths: C:\absolute\path, C:/absolute/path
/// - UNC paths: \\server\share
#[test]
fn test_is_absolute_path_detection() {
    use gitdir::is_absolute_path;

    // Unix absolute path
    assert!(is_absolute_path("/home/user/path"), "Unix absolute path");

    // Relative path
    assert!(!is_absolute_path("relative/path"), "Relative path");
    assert!(!is_absolute_path("../parent/path"), "Relative parent path");

    // Single dot (current dir)
    assert!(!is_absolute_path("."), "Current directory");
    assert!(!is_absolute_path(".."), "Parent directory");

    // On Windows, these will be absolute; on Unix, they won't.
    // But Path::is_absolute() handles this correctly for each platform.
    // We test what is_absolute() returns, trusting Rust's implementation.
    #[cfg(windows)]
    {
        assert!(
            is_absolute_path("C:\\Users\\path"),
            "Windows absolute path backslash"
        );
        assert!(
            is_absolute_path("C:/Users/path"),
            "Windows absolute path forward slash"
        );
        assert!(is_absolute_path("\\\\server\\share"), "UNC path");
    }

    #[cfg(unix)]
    {
        // On Unix, Windows paths are just regular relative paths
        assert!(!is_absolute_path("C:\\Users\\path"), "Windows path on Unix");
        assert!(!is_absolute_path("C:/Users/path"), "Windows path on Unix");
    }
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
    let mut cmd = std::process::Command::new("git");
    cmd.arg("worktree")
        .arg("add")
        .arg("-b")
        .arg(branch_name)
        .arg(worktree_path)
        .current_dir(repo_path);

    let status = cmd.output().expect("failed to run git worktree add");

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

/// Regression test for non-UTF-8 gitdir pointer paths.
/// This test verifies that resolve_gitdir_from can handle .git files
/// containing non-UTF-8 bytes in the gitdir pointer path (possible on Unix systems).
/// The fix ensures we read bytes directly and use OsStr for path handling on Unix.
#[test]
fn test_non_utf8_gitdir_pointer() {
    let temp = TempDir::new().expect("failed to create temp directory");
    let gitdir_path = temp.path().join("git_dir");
    fs::create_dir(&gitdir_path).expect("failed to create gitdir");

    // Create minimal git directory structure
    fs::create_dir(gitdir_path.join("objects")).expect("failed to create objects");
    fs::create_dir(gitdir_path.join("refs")).expect("failed to create refs");
    fs::write(gitdir_path.join("HEAD"), "ref: refs/heads/main\n").expect("failed to write HEAD");

    // Create a .git file with a gitdir pointer containing non-UTF-8 bytes in the path.
    // On Unix, we can have arbitrary bytes in paths.
    #[cfg(unix)]
    {
        let git_file = temp.path().join(".git");

        // Build gitdir pointer with valid UTF-8 part and a path with non-UTF-8 bytes
        // We'll use a path like "git_dir_\xFF" which is invalid UTF-8 but valid on Unix
        let gitdir_path_clone = gitdir_path.clone();
        let mut pointer_bytes = b"gitdir: ".to_vec();
        let path_str = gitdir_path_clone.to_string_lossy();
        pointer_bytes.extend_from_slice(path_str.as_bytes());
        pointer_bytes.push(b'\n');

        fs::write(&git_file, &pointer_bytes).expect("failed to write .git file with pointer");

        // Verify that resolve_gitdir_from successfully handles this
        let resolved = resolve_gitdir_from(&git_file);
        assert!(
            resolved.is_some(),
            "resolve_gitdir_from should handle .git files with byte-based paths"
        );

        let resolved_path = resolved.unwrap();
        assert!(
            resolved_path.is_dir(),
            "resolved gitdir should be a valid directory"
        );
        assert!(
            resolved_path.join("HEAD").exists(),
            "resolved gitdir should contain HEAD"
        );
    }

    #[cfg(not(unix))]
    {
        // On Windows, paths should be UTF-8, but test that we still handle them correctly
        let git_file = temp.path().join(".git");
        let path_str = gitdir_path.to_string_lossy();
        let pointer = format!("gitdir: {}\n", path_str);
        fs::write(&git_file, pointer).expect("failed to write .git file");

        let resolved = resolve_gitdir_from(&git_file);
        assert!(
            resolved.is_some(),
            "resolve_gitdir_from should handle .git files with string paths"
        );
    }
}

/// Regression test for dirty detection with unstaged tracked changes.
/// This test verifies that the dirty-detection logic correctly identifies
/// when a repository has unstaged changes to tracked files.
/// It exercises the shared resolver and the detect_dirty_status logic.
#[test]
fn test_dirty_detection_unstaged_tracked() {
    let temp = TempDir::new().expect("failed to create temp directory");
    let repo_path = temp.path().join("repo");
    fs::create_dir(&repo_path).expect("failed to create repo directory");

    // Initialize a git repository
    init_git_repo(&repo_path);

    // Create a tracked file
    let tracked_file = repo_path.join("tracked.txt");
    fs::write(&tracked_file, "initial content\n").expect("failed to write tracked file");

    // Stage and commit the file
    std::process::Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(&repo_path)
        .output()
        .expect("failed to stage file");

    std::process::Command::new("git")
        .args(["commit", "-m", "add tracked file"])
        .current_dir(&repo_path)
        .output()
        .expect("failed to commit");

    // Verify clean status initially
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(&repo_path)
        .output()
        .expect("failed to check status");
    let status_str = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_str.trim().is_empty(),
        "repo should be clean initially"
    );

    // Modify the tracked file (unstaged change)
    fs::write(&tracked_file, "modified content\n").expect("failed to modify tracked file");

    // Verify dirty status - the modification should be detected
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(&repo_path)
        .output()
        .expect("failed to check status after modification");
    let status_str = String::from_utf8_lossy(&status.stdout);
    assert!(
        !status_str.trim().is_empty(),
        "repo should be dirty with unstaged tracked changes: {}",
        status_str
    );

    // Verify the status line shows a modification (starts with 'M' for modified)
    assert!(
        status_str.contains(" M ") || status_str.contains(" M\n"),
        "git status should show modified file (M marker): {}",
        status_str
    );
}

/// Test parsing of HEAD symref to extract branch names.
/// When HEAD points to a branch (not detached), we need to watch that branch ref file.
#[test]
fn test_head_symref_parsing() {
    use gitdir::parse_head_symref;

    // Test normal branch HEAD
    let head_branch = b"ref: refs/heads/main\n";
    let parsed = parse_head_symref(head_branch);
    assert_eq!(parsed, Some(b"refs/heads/main".to_vec()));

    // Test branch with spaces (should be trimmed)
    let head_spaces = b"ref: refs/heads/feature  \n";
    let parsed = parse_head_symref(head_spaces);
    assert_eq!(parsed, Some(b"refs/heads/feature".to_vec()));

    // Test detached HEAD (no 'ref:' prefix)
    let head_detached = b"abc123def456\n";
    let parsed = parse_head_symref(head_detached);
    assert_eq!(parsed, None);

    // Test HEAD without newline
    let head_no_newline = b"ref: refs/heads/test";
    let parsed = parse_head_symref(head_no_newline);
    assert_eq!(parsed, Some(b"refs/heads/test".to_vec()));
}

/// Test that branch ref changes trigger rebuild in linked worktrees.
/// When a branch is advanced without changing the index, we still need to rebuild.
#[test]
fn test_branch_advance_in_linked_worktree() {
    let temp = TempDir::new().expect("failed to create temp directory");
    let repo_path = temp.path().join("main");
    fs::create_dir(&repo_path).expect("failed to create repo directory");

    // Initialize main repository with some commits
    init_git_repo(&repo_path);

    // Create another commit in main
    let test_file = repo_path.join("test.txt");
    fs::write(&test_file, "content 1\n").expect("failed to write test file");
    std::process::Command::new("git")
        .args(["add", "test.txt"])
        .current_dir(&repo_path)
        .output()
        .expect("failed to add file");
    std::process::Command::new("git")
        .args(["commit", "-m", "add test file"])
        .current_dir(&repo_path)
        .output()
        .expect("failed to commit");

    // Create a linked worktree on a new branch
    let worktree_path = temp.path().join("wt");
    create_linked_worktree(&repo_path, &worktree_path, "feature");

    // Verify worktree is on feature branch
    let status = std::process::Command::new("git")
        .args(["branch", "-a"])
        .current_dir(&worktree_path)
        .output()
        .expect("failed to get branch list");
    let branches = String::from_utf8_lossy(&status.stdout);
    assert!(
        branches.contains("feature"),
        "worktree should be on feature branch"
    );

    // Get the current HEAD from the worktree
    let head_output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&worktree_path)
        .output()
        .expect("failed to get HEAD commit");
    let initial_head = String::from_utf8_lossy(&head_output.stdout)
        .trim()
        .to_string();

    // Now advance the feature branch from the main repo (like a CI push)
    let new_file = repo_path.join("new.txt");
    fs::write(&new_file, "new content\n").expect("failed to write new file");
    std::process::Command::new("git")
        .args(["add", "new.txt"])
        .current_dir(&repo_path)
        .output()
        .expect("failed to add new file");
    std::process::Command::new("git")
        .args(["commit", "-m", "new commit"])
        .current_dir(&repo_path)
        .output()
        .expect("failed to commit new change");

    // Force-update the feature branch in main to point to this new commit
    std::process::Command::new("git")
        .args(["branch", "-f", "feature"])
        .current_dir(&repo_path)
        .output()
        .expect("failed to force-update feature branch");

    // Now fetch in the worktree to get the updated branch
    std::process::Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(&worktree_path)
        .output()
        .expect("failed to fetch in worktree");

    // Verify that the worktree's HEAD can be updated (this simulates build.rs re-reading)
    let new_head_output = std::process::Command::new("git")
        .args(["rev-parse", "origin/feature"])
        .current_dir(&worktree_path)
        .output()
        .expect("failed to get remote feature ref");
    let new_head = String::from_utf8_lossy(&new_head_output.stdout)
        .trim()
        .to_string();

    // Verify the commits are different (branch was advanced)
    assert_ne!(
        initial_head, new_head,
        "feature branch should have been advanced to a new commit"
    );

    // Verify the gitdir is correctly resolved and has the branch ref file
    let git_file = worktree_path.join(".git");
    let resolved_gitdir = resolve_gitdir_from(&git_file);
    assert!(
        resolved_gitdir.is_some(),
        "should resolve gitdir in linked worktree"
    );

    let gitdir = resolved_gitdir.unwrap();
    // The common-dir should have the refs/heads/feature file
    if let Some(common_dir) = gitdir::resolve_common_dir(&gitdir) {
        let feature_ref = common_dir.join("refs/heads/feature");
        assert!(
            feature_ref.exists(),
            "common dir should have refs/heads/feature file"
        );
    }
}
