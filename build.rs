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

#[path = "build/gitdir.rs"]
mod gitdir;

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
    if let Some(gitdir) = gitdir::resolve_gitdir() {
        // Watch HEAD itself for detached HEAD changes
        if let Ok(head_path) = gitdir.join("HEAD").canonicalize() {
            println!(
                "cargo:rerun-if-changed={}",
                gitdir::rerun_path_safe(&head_path)
            );
        }

        // If on a branch, also watch the branch ref file for commits without index changes.
        // Register the loose ref path even when it does not yet exist; a subsequent commit
        // can create refs/heads/<branch> without changing other watched files.
        if let Some(head_bytes) = gitdir::read_head_bytes(&gitdir)
            && let Some(ref_path_bytes) = gitdir::parse_head_symref(&head_bytes)
            && let Some(common_dir) = gitdir::resolve_common_dir(&gitdir)
        {
            // ref_path_bytes is like b"refs/heads/main"
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                let ref_osstr = std::ffi::OsStr::from_bytes(&ref_path_bytes);
                let ref_full = common_dir.join(ref_osstr);
                // Try to canonicalize if it exists; otherwise use the path as-is.
                let watch_path = if ref_full.exists() {
                    ref_full.canonicalize().ok()
                } else {
                    Some(ref_full)
                };
                if let Some(path) = watch_path {
                    println!(
                        "cargo:rerun-if-changed={}",
                        gitdir::rerun_path_safe(&path)
                    );
                }
            }
            #[cfg(not(unix))]
            {
                if let Ok(ref_path_str) = std::str::from_utf8(&ref_path_bytes) {
                    let ref_full = common_dir.join(ref_path_str);
                    // Try to canonicalize if it exists; otherwise use the path as-is.
                    let watch_path = if ref_full.exists() {
                        ref_full.canonicalize().ok()
                    } else {
                        Some(ref_full)
                    };
                    if let Some(path) = watch_path {
                        println!(
                            "cargo:rerun-if-changed={}",
                            gitdir::rerun_path_safe(&path)
                        );
                    }
                }
            }
        }

        // Watch packed-refs for efficiency (batch ref updates).
        // Register even if it does not yet exist (it might be created on first gc).
        if let Some(common_dir) = gitdir::resolve_common_dir(&gitdir) {
            let packed_refs = common_dir.join("packed-refs");
            let watch_path = if packed_refs.exists() {
                packed_refs.canonicalize().ok()
            } else {
                Some(packed_refs)
            };
            if let Some(path) = watch_path {
                println!(
                    "cargo:rerun-if-changed={}",
                    gitdir::rerun_path_safe(&path)
                );
            }
        }

        // Watch the index for working tree changes
        if let Ok(index_path) = gitdir.join("index").canonicalize() {
            println!(
                "cargo:rerun-if-changed={}",
                gitdir::rerun_path_safe(&index_path)
            );
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
    gitdir::detect_dirty_status().unwrap_or(false)
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

fn git(args: &[&str]) -> Option<String> {
    gitdir::run_git(args)
}
