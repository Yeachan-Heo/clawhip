use std::env;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow};

use crate::{Result, plugins};

const GITHUB_REPO: &str = "Yeachan-Heo/clawhip";
const SKIP_STAR_PROMPT_ENV: &str = "CLAWHIP_SKIP_STAR_PROMPT";

pub fn install(systemd: bool, skip_star_prompt: bool) -> Result<()> {
    let repo_root = current_repo_root()?;
    run(Command::new("cargo")
        .arg("install")
        .arg("--path")
        .arg(&repo_root))?;
    ensure_config_dir()?;
    plugins::install_bundled_plugins(&config_dir().join("plugins"))?;
    if systemd {
        install_systemd(&repo_root)?;
    }
    maybe_prompt_to_star_repo(skip_star_prompt)?;
    println!("clawhip install complete");
    Ok(())
}

pub fn update(restart: bool) -> Result<()> {
    let repo_root = current_repo_root()?;
    run(Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .arg("pull")
        .arg("--ff-only"))?;
    run(Command::new("cargo")
        .arg("install")
        .arg("--path")
        .arg(&repo_root)
        .arg("--force"))?;
    ensure_config_dir()?;
    plugins::install_bundled_plugins(&config_dir().join("plugins"))?;
    if restart {
        restart_systemd_if_present()?;
    }
    println!("clawhip update complete");
    Ok(())
}

pub fn uninstall(remove_systemd: bool, remove_config: bool) -> Result<()> {
    stop_systemd_if_present()?;
    let binary_path = cargo_bin_dir().join("clawhip");
    if binary_path.exists() {
        fs::remove_file(&binary_path)?;
        println!("Removed {}", binary_path.display());
    }
    if remove_systemd {
        uninstall_systemd_if_present()?;
    }
    if remove_config {
        let config_dir = config_dir();
        if config_dir.exists() {
            fs::remove_dir_all(&config_dir)?;
            println!("Removed {}", config_dir.display());
        }
    }
    println!("clawhip uninstall complete");
    Ok(())
}

fn current_repo_root() -> Result<PathBuf> {
    let dir = env::current_dir()?;
    if dir.join("Cargo.toml").exists() && dir.join("src").exists() {
        Ok(dir)
    } else {
        Err(anyhow!("run this command from the clawhip git clone root").into())
    }
}

fn ensure_config_dir() -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir)?;
    println!("Ensured config dir {}", dir.display());
    Ok(())
}

fn config_dir() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".to_string())).join(".clawhip")
}

fn cargo_bin_dir() -> PathBuf {
    env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into())).join(".cargo")
        })
        .join("bin")
}

fn maybe_prompt_to_star_repo(skip_star_prompt: bool) -> Result<()> {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let env_skip_star_prompt = env::var(SKIP_STAR_PROMPT_ENV).ok();

    maybe_prompt_to_star_repo_with(
        skip_star_prompt,
        env_skip_star_prompt.as_deref(),
        interactive,
        &mut input,
        &mut output,
        gh_command_succeeds,
    )
}

fn maybe_prompt_to_star_repo_with<R, W, F>(
    skip_star_prompt: bool,
    env_skip_star_prompt: Option<&str>,
    interactive: bool,
    input: &mut R,
    output: &mut W,
    mut gh_command_succeeds: F,
) -> Result<()>
where
    R: BufRead,
    W: Write,
    F: FnMut(&[&str]) -> bool,
{
    if star_prompt_disabled(skip_star_prompt, env_skip_star_prompt) {
        writeln!(
            output,
            "[clawhip] skipping GitHub star prompt (--skip-star-prompt or {SKIP_STAR_PROMPT_ENV})"
        )?;
        return Ok(());
    }

    if !interactive || !gh_command_succeeds(&["auth", "status"]) {
        return Ok(());
    }

    writeln!(
        output,
        "[clawhip] optional: star {GITHUB_REPO} on GitHub to support the project"
    )?;
    write!(
        output,
        "[clawhip] Would you like to star {GITHUB_REPO} on GitHub with gh? [y/N]: "
    )?;
    output.flush()?;

    let mut response = String::new();
    if input.read_line(&mut response)? == 0 {
        return Ok(());
    }

    match response.trim() {
        "y" | "Y" | "yes" | "Yes" | "YES" => {
            if gh_star_repo_succeeds_with(&mut gh_command_succeeds) {
                writeln!(output, "[clawhip] thanks for starring {GITHUB_REPO}")?;
            } else {
                writeln!(
                    output,
                    "[clawhip] unable to star {GITHUB_REPO} with gh; continuing without it"
                )?;
            }
        }
        _ => {
            writeln!(output, "[clawhip] skipping GitHub star step")?;
        }
    }

    Ok(())
}

fn star_prompt_disabled(skip_star_prompt: bool, env_skip_star_prompt: Option<&str>) -> bool {
    skip_star_prompt || env_skip_star_prompt.is_some_and(is_truthy)
}

fn is_truthy(value: &str) -> bool {
    matches!(value, "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
}

fn gh_star_repo_succeeds_with<F>(gh_command_succeeds: &mut F) -> bool
where
    F: FnMut(&[&str]) -> bool,
{
    let endpoint = format!("/user/starred/{GITHUB_REPO}");
    gh_command_succeeds(&["api", "--method", "PUT", endpoint.as_str(), "--silent"])
}

fn gh_command_succeeds(args: &[&str]) -> bool {
    Command::new("gh")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn install_systemd(repo_root: &Path) -> Result<()> {
    let unit_src = repo_root.join("deploy").join("clawhip.service");
    let unit_dest = PathBuf::from("/etc/systemd/system/clawhip.service");
    run(Command::new("sudo")
        .arg("cp")
        .arg(&unit_src)
        .arg(&unit_dest))?;
    run(Command::new("sudo").arg("systemctl").arg("daemon-reload"))?;
    run(Command::new("sudo")
        .arg("systemctl")
        .arg("enable")
        .arg("--now")
        .arg("clawhip"))?;
    Ok(())
}

fn uninstall_systemd_if_present() -> Result<()> {
    let unit_dest = PathBuf::from("/etc/systemd/system/clawhip.service");
    if unit_dest.exists() {
        let _ = run(Command::new("sudo")
            .arg("systemctl")
            .arg("disable")
            .arg("--now")
            .arg("clawhip"));
        let _ = run(Command::new("sudo").arg("rm").arg("-f").arg(&unit_dest));
        let _ = run(Command::new("sudo").arg("systemctl").arg("daemon-reload"));
    }
    Ok(())
}

fn restart_systemd_if_present() -> Result<()> {
    let unit_dest = PathBuf::from("/etc/systemd/system/clawhip.service");
    if unit_dest.exists() {
        let _ = run(Command::new("sudo")
            .arg("systemctl")
            .arg("restart")
            .arg("clawhip"));
    }
    Ok(())
}

fn stop_systemd_if_present() -> Result<()> {
    let unit_dest = PathBuf::from("/etc/systemd/system/clawhip.service");
    if unit_dest.exists() {
        let _ = run(Command::new("sudo")
            .arg("systemctl")
            .arg("stop")
            .arg("clawhip"));
    }
    Ok(())
}

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to run command: {command:?}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("command failed with status {status}: {command:?}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn skip_flag_or_env_disables_star_prompt() {
        let mut output = Vec::new();
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut gh_calls = Vec::<Vec<String>>::new();

        maybe_prompt_to_star_repo_with(true, Some("1"), true, &mut input, &mut output, |args| {
            gh_calls.push(args.iter().map(|arg| (*arg).to_string()).collect());
            true
        })
        .expect("skip should succeed");

        assert!(gh_calls.is_empty());
        let stdout = String::from_utf8(output).expect("utf8 output");
        assert!(stdout.contains("skipping GitHub star prompt"));
    }

    #[test]
    fn skips_star_prompt_when_not_interactive() {
        let mut output = Vec::new();
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut gh_calls = Vec::<Vec<String>>::new();

        maybe_prompt_to_star_repo_with(false, None, false, &mut input, &mut output, |args| {
            gh_calls.push(args.iter().map(|arg| (*arg).to_string()).collect());
            true
        })
        .expect("non-interactive install should succeed");

        assert!(gh_calls.is_empty());
        assert!(output.is_empty());
    }

    #[test]
    fn skips_prompt_when_gh_is_unauthenticated() {
        let mut output = Vec::new();
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut gh_calls = Vec::<Vec<String>>::new();

        maybe_prompt_to_star_repo_with(false, None, true, &mut input, &mut output, |args| {
            gh_calls.push(args.iter().map(|arg| (*arg).to_string()).collect());
            !matches!(args, ["auth", "status"])
        })
        .expect("unauthenticated gh should skip cleanly");

        assert_eq!(
            gh_calls,
            vec![vec![String::from("auth"), String::from("status")]]
        );
        let stdout = String::from_utf8(output).expect("utf8 output");
        assert!(!stdout.contains("Would you like to star"));
    }

    #[test]
    fn stars_repo_only_after_explicit_yes() {
        let mut output = Vec::new();
        let mut input = Cursor::new(b"y\n".to_vec());
        let mut gh_calls = Vec::<Vec<String>>::new();

        maybe_prompt_to_star_repo_with(false, None, true, &mut input, &mut output, |args| {
            gh_calls.push(args.iter().map(|arg| (*arg).to_string()).collect());
            true
        })
        .expect("yes path should succeed");

        assert_eq!(
            gh_calls,
            vec![
                vec![String::from("auth"), String::from("status")],
                vec![
                    String::from("api"),
                    String::from("--method"),
                    String::from("PUT"),
                    format!("/user/starred/{GITHUB_REPO}"),
                    String::from("--silent"),
                ],
            ]
        );
        let stdout = String::from_utf8(output).expect("utf8 output");
        assert!(stdout.contains("Would you like to star"));
        assert!(stdout.contains("thanks for starring"));
    }

    #[test]
    fn star_failure_does_not_fail_the_install() {
        let mut output = Vec::new();
        let mut input = Cursor::new(b"yes\n".to_vec());
        let mut gh_calls = Vec::<Vec<String>>::new();

        maybe_prompt_to_star_repo_with(false, None, true, &mut input, &mut output, |args| {
            gh_calls.push(args.iter().map(|arg| (*arg).to_string()).collect());
            !matches!(
                args,
                ["api", "--method", "PUT", endpoint, "--silent"]
                    if *endpoint == format!("/user/starred/{GITHUB_REPO}")
            )
        })
        .expect("star failure should not fail install");

        assert_eq!(
            gh_calls,
            vec![
                vec![String::from("auth"), String::from("status")],
                vec![
                    String::from("api"),
                    String::from("--method"),
                    String::from("PUT"),
                    format!("/user/starred/{GITHUB_REPO}"),
                    String::from("--silent"),
                ],
            ]
        );
        let stdout = String::from_utf8(output).expect("utf8 output");
        assert!(stdout.contains("continuing without it"));
    }
}
