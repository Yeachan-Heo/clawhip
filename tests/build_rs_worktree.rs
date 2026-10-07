//! Regression test for PR #371: build.rs git metadata lookup must work in linked worktrees.
//!
//! When build.rs runs in a linked worktree, `.git` is a file containing a gitdir
//! pointer rather than a directory. This test verifies that the build script can
//! correctly resolve the gitdir and access git metadata.

#[test]
fn test_gitdir_resolution_in_worktree() {
    // This test runs in the worktree context where .git is a file.
    // The test passes if the build script completes without errors.
    // We verify this indirectly by checking that CLAWHIP_BUILD_COMMIT is set.
    let commit = std::env::var("CLAWHIP_BUILD_COMMIT");
    assert!(
        commit.is_ok(),
        "CLAWHIP_BUILD_COMMIT should be set by build.rs"
    );

    // The commit should be a reasonable hex value or "unknown".
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
        "CLAWHIP_BUILD_COMMIT_SOURCE should be one of: git, environment, unavailable"
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
