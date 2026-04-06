use std::env;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::cli::OmcArgs;
use crate::config::AppConfig;
use crate::tmux_wrapper;

/// Default keywords monitored for OMC sessions.
const DEFAULT_OMC_KEYWORDS: &[&str] = &[
    "error",
    "Error",
    "FAILED",
    "PR created",
    "panic",
    "complete",
];

/// Default stale timeout in minutes for OMC sessions.
const DEFAULT_OMC_STALE_MINUTES: u64 = 30;

pub async fn run(args: OmcArgs, config: &AppConfig) -> Result<()> {
    let workdir = resolve_workdir(&args.workdir)?;
    let session_name = resolve_session_name(&args.session, &workdir);
    let project = detect_project(&workdir);

    // Check if hooks are installed
    if !hooks_installed(&workdir) {
        eprintln!("clawhip hooks not found in this workspace.");
        eprintln!("Install with: clawhip hooks install --all");
        if !args.skip_hook_check {
            return Err("hooks not installed — pass --skip-hook-check to launch anyway".into());
        }
        eprintln!("Continuing without hooks (--skip-hook-check)...");
    }

    eprintln!(
        "clawhip omc session={session_name} workdir={} project={project}",
        workdir.display()
    );

    let omc_flags = env::var("CLAWHIP_OMC_FLAGS").unwrap_or_else(|_| {
        args.omc_flags
            .clone()
            .unwrap_or_else(|| "--openclaw --madmax".into())
    });

    let omc_command = build_omc_shell_command(&session_name, &project, &workdir, &omc_flags, &args);

    // Build keywords
    let keywords: Vec<String> = if args.keywords.is_empty() {
        env::var("CLAWHIP_OMC_KEYWORDS")
            .map(|v| v.split(',').map(String::from).collect())
            .unwrap_or_else(|_| DEFAULT_OMC_KEYWORDS.iter().map(|s| s.to_string()).collect())
    } else {
        args.keywords.clone()
    };

    let stale_minutes = args.stale_minutes.unwrap_or_else(|| {
        env::var("CLAWHIP_OMC_STALE_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_OMC_STALE_MINUTES)
    });

    // Schedule prompt delivery as a background task (10s delay for TUI init)
    if let Some(prompt) = args.prompt.clone() {
        let target_session = session_name.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            if let Err(e) = deliver_prompt(&target_session, &prompt).await {
                eprintln!("clawhip omc: prompt delivery failed: {e}");
            }
        });
    }

    // Launch via clawhip tmux new
    let tmux_args = crate::cli::TmuxNewArgs {
        session: session_name.clone(),
        window_name: None,
        cwd: Some(workdir.to_string_lossy().into_owned()),
        channel: args.channel.clone(),
        mention: args.mention.clone(),
        keywords,
        stale_minutes,
        format: None,
        attach: args.attach,
        retry_enter: true,
        retry_enter_count: crate::cli::DEFAULT_RETRY_ENTER_COUNT,
        retry_enter_delay_ms: crate::cli::DEFAULT_RETRY_ENTER_DELAY_MS,
        shell: None,
        command: vec![omc_command],
    };

    tmux_wrapper::run(tmux_args, config).await?;

    Ok(())
}

/// Deliver a prompt to a tmux session via send-keys.
async fn deliver_prompt(session: &str, prompt: &str) -> Result<()> {
    use crate::source::tmux::tmux_bin;
    use tokio::process::Command;

    let literal_output = Command::new(tmux_bin())
        .arg("send-keys")
        .arg("-t")
        .arg(session)
        .arg("-l")
        .arg(prompt)
        .output()
        .await?;
    if !literal_output.status.success() {
        let stderr = String::from_utf8_lossy(&literal_output.stderr);
        return Err(format!("tmux send-keys failed: {}", stderr.trim()).into());
    }

    let enter_output = Command::new(tmux_bin())
        .arg("send-keys")
        .arg("-t")
        .arg(session)
        .arg("Enter")
        .output()
        .await?;
    if !enter_output.status.success() {
        let stderr = String::from_utf8_lossy(&enter_output.stderr);
        return Err(format!("tmux send-keys Enter failed: {}", stderr.trim()).into());
    }

    eprintln!("clawhip omc: prompt delivered to {session}");
    Ok(())
}

/// Build the shell command that runs inside the tmux session, including
/// lifecycle event emission and the actual `omc` invocation.
fn build_omc_shell_command(
    session: &str,
    project: &str,
    workdir: &Path,
    omc_flags: &str,
    args: &OmcArgs,
) -> String {
    let mut emit_suffix = String::new();
    if let Some(channel) = &args.channel {
        emit_suffix.push_str(&format!(" --channel {}", shell_escape(channel)));
    }
    if let Some(mention) = &args.mention {
        emit_suffix.push_str(&format!(" --mention {}", shell_escape(mention)));
    }

    let omc_env = env::var("CLAWHIP_OMC_ENV").unwrap_or_default();
    let env_prefix = if omc_env.is_empty() {
        String::new()
    } else {
        format!("{omc_env} ")
    };

    format!(
        r#"source ~/.zshrc
START_TS=$(date +%s)
cleanup() {{
  local exit_code=$?
  local elapsed=$(( $(date +%s) - START_TS ))
  if [ "$exit_code" -eq 0 ]; then
    clawhip emit agent.finished --agent omc --session {session_esc} --project {project_esc} --elapsed "$elapsed"{emit_suffix} || true
  else
    clawhip emit agent.failed --agent omc --session {session_esc} --project {project_esc} --elapsed "$elapsed" --error "exit $exit_code"{emit_suffix} || true
  fi
}}
trap cleanup EXIT
trap 'exit 130' INT TERM
clawhip emit agent.started --agent omc --session {session_esc} --project {project_esc}{emit_suffix} || true
cd {workdir_esc}
{env_prefix}omc {omc_flags}"#,
        session_esc = shell_escape(session),
        project_esc = shell_escape(project),
        workdir_esc = shell_escape(&workdir.to_string_lossy()),
    )
}

/// Resolve working directory: explicit arg, or auto-detect git worktree from CWD.
fn resolve_workdir(explicit: &Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        if dir.is_dir() {
            return Ok(dir.canonicalize()?);
        }
        return Err(format!("directory not found: {}", dir.display()).into());
    }

    let cwd = env::current_dir()?;

    // Check if we're already in a git worktree
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let toplevel = String::from_utf8(out.stdout)?.trim().to_string();
            Ok(PathBuf::from(toplevel))
        }
        _ => Ok(cwd),
    }
}

/// Derive a session name from the worktree path if not explicitly provided.
fn resolve_session_name(explicit: &Option<String>, workdir: &Path) -> String {
    if let Some(name) = explicit {
        return name.clone();
    }

    // Use the directory basename, sanitized for tmux
    let basename = workdir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "omc".into());

    sanitize_tmux_session_name(&basename)
}

/// Detect the project name from the git common dir.
fn detect_project(workdir: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(workdir)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let common_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Path::new(&common_dir)
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir_basename(workdir))
        }
        _ => dir_basename(workdir),
    }
}

fn dir_basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into())
}

/// Check if clawhip hooks or repo-local native hook config are installed in this workspace.
fn hooks_installed(workdir: &Path) -> bool {
    if crate::native_hooks::native_hooks_installed(workdir) {
        return true;
    }
    // Check for OMX hook bridge
    let omx_hook = workdir.join(".omx/hooks/clawhip.mjs");
    if omx_hook.is_file() {
        return true;
    }

    // Check for Claude Code settings.json with clawhip hooks
    let claude_settings = workdir.join(".claude/settings.json");
    if claude_settings.is_file()
        && let Ok(content) = std::fs::read_to_string(&claude_settings)
        && content.contains("clawhip")
    {
        return true;
    }

    // Check global Claude Code settings
    let home = env::var("HOME").unwrap_or_default();
    let global_settings = PathBuf::from(&home).join(".claude/settings.json");
    if global_settings.is_file()
        && let Ok(content) = std::fs::read_to_string(&global_settings)
        && content.contains("clawhip")
    {
        return true;
    }

    false
}

/// Sanitize a string into a valid tmux session name.
fn sanitize_tmux_session_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn shell_escape(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_@%+=:,./-".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_tmux_session_name_replaces_special_chars() {
        assert_eq!(sanitize_tmux_session_name("issue/123"), "issue-123");
        assert_eq!(sanitize_tmux_session_name("feat+omc"), "feat-omc");
        assert_eq!(sanitize_tmux_session_name("simple"), "simple");
        assert_eq!(sanitize_tmux_session_name("a.b.c"), "a-b-c");
    }

    #[test]
    fn resolve_session_name_uses_explicit_when_provided() {
        assert_eq!(
            resolve_session_name(&Some("my-session".into()), Path::new("/tmp/project")),
            "my-session"
        );
    }

    #[test]
    fn resolve_session_name_falls_back_to_dir_basename() {
        assert_eq!(
            resolve_session_name(&None, Path::new("/home/user/projects/my-app")),
            "my-app"
        );
    }

    #[test]
    fn detect_project_falls_back_to_basename() {
        // In a non-git directory, should fall back to basename
        let tmpdir = std::env::temp_dir();
        let project = detect_project(&tmpdir);
        assert!(!project.is_empty());
    }

    #[test]
    fn shell_escape_leaves_safe_strings_untouched() {
        assert_eq!(shell_escape("hello-world"), "hello-world");
        assert_eq!(shell_escape("path/to/file"), "path/to/file");
    }

    #[test]
    fn shell_escape_quotes_strings_with_special_chars() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn build_omc_shell_command_launches_from_workdir_without_worktree_flag() {
        let args = OmcArgs {
            prompt: Some("fix it".into()),
            session: None,
            workdir: None,
            channel: Some("alerts".into()),
            mention: Some("@ops".into()),
            keywords: Vec::new(),
            stale_minutes: None,
            omc_flags: Some("--openclaw --madmax --foo".into()),
            attach: false,
            skip_hook_check: false,
        };
        let command = build_omc_shell_command(
            "issue-9001",
            "clawhip",
            Path::new("/tmp/native launcher"),
            "--openclaw --madmax --foo",
            &args,
        );

        assert!(command.contains("cd '/tmp/native launcher'"));
        assert!(command.contains("omc --openclaw --madmax --foo"));
        assert!(!command.contains("--worktree"));
        assert!(!command.contains("fix it"));
    }

    #[test]
    fn hooks_installed_returns_false_for_empty_dir() {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        assert!(!hooks_installed(tmpdir.path()));
    }

    #[test]
    fn hooks_installed_detects_omx_bridge() {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let hook_dir = tmpdir.path().join(".omx/hooks");
        std::fs::create_dir_all(&hook_dir).expect("create hook dir");
        std::fs::write(hook_dir.join("clawhip.mjs"), "// hook").expect("write hook");
        assert!(hooks_installed(tmpdir.path()));
    }

    #[test]
    fn hooks_installed_detects_claude_settings() {
        let tmpdir = tempfile::tempdir().expect("tempdir");
        let claude_dir = tmpdir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).expect("create claude dir");
        std::fs::write(
            claude_dir.join("settings.json"),
            r#"{"hooks": {"clawhip omx hook": true}}"#,
        )
        .expect("write settings");
        assert!(hooks_installed(tmpdir.path()));
    }
}
