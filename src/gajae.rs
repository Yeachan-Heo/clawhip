use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

const GAJAE_ENV: &str = "GAJAE_BIN";
const GAJAE_PATH_NAME: &str = "gajae";
const PROFILE_INSTALL_ARGS: &[&str] = &["clawhip", "profile", "install"];

#[derive(Debug, Clone, Copy)]
pub enum GajaeCommand {
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandExit {
    pub success: bool,
    pub code: Option<i32>,
}

pub trait CommandRunner {
    fn output(&mut self, program: &Path, args: &[&str]) -> io::Result<CommandOutput>;
    fn status_inherited_output(&mut self, program: &Path, args: &[&str])
    -> io::Result<CommandExit>;
}

#[derive(Debug, Default)]
pub struct StdCommandRunner;

impl CommandRunner for StdCommandRunner {
    fn output(&mut self, program: &Path, args: &[&str]) -> io::Result<CommandOutput> {
        let output = Command::new(program).args(args).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn status_inherited_output(
        &mut self,
        program: &Path,
        args: &[&str],
    ) -> io::Result<CommandExit> {
        let status = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        Ok(CommandExit {
            success: status.success(),
            code: status.code(),
        })
    }
}

pub fn run(command: GajaeCommand) -> Result<()> {
    let mut runner = StdCommandRunner;
    match command {
        GajaeCommand::Status => run_status_with(&mut runner, |name| std::env::var_os(name)),
    }
}

fn discover_gajae_with(env_var: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    env_var(GAJAE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(GAJAE_PATH_NAME))
}

fn run_status_with(
    runner: &mut impl CommandRunner,
    env_var: impl Fn(&str) -> Option<OsString>,
) -> Result<()> {
    let bin = discover_gajae_with(env_var);
    match runner.output(&bin, &["--help"]) {
        Ok(output) if output.success => {
            println!("gajae available: {}", bin.display());
            Ok(())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "gajae found at {} but `--help` failed{}",
                bin.display(),
                concise_detail(&stderr)
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("gajae unavailable: set {GAJAE_ENV} or install `{GAJAE_PATH_NAME}` on PATH")
        }
        Err(error) => Err(error).with_context(|| format!("failed to run {} --help", bin.display())),
    }
}

pub fn run_profile_install() -> Result<CommandExit> {
    let mut runner = StdCommandRunner;
    run_profile_install_with(&mut runner, |name| std::env::var_os(name))
}

fn run_profile_install_with(
    runner: &mut impl CommandRunner,
    env_var: impl Fn(&str) -> Option<OsString>,
) -> Result<CommandExit> {
    let bin = discover_gajae_with(env_var);
    let status = runner
        .status_inherited_output(&bin, PROFILE_INSTALL_ARGS)
        .with_context(|| {
            format!(
                "failed to run {} {}",
                bin.display(),
                PROFILE_INSTALL_ARGS.join(" ")
            )
        })?;

    Ok(status)
}

pub fn profile_install_failure_message(status: CommandExit) -> String {
    format!(
        "gajae clawhip profile install failed{}",
        status
            .code
            .map(|code| format!(" with exit code {code}"))
            .unwrap_or_else(|| " without an exit code".to_string())
    )
}

fn concise_detail(stderr: &str) -> String {
    stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| format!(": {}", line.trim()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        program: PathBuf,
        args: Vec<String>,
        inherits_stdout_stderr: bool,
        stdin_null: bool,
    }

    #[derive(Debug)]
    struct MockRunner {
        calls: Vec<Call>,
        output_result: io::Result<CommandOutput>,
        status_result: io::Result<CommandExit>,
    }

    impl MockRunner {
        fn available() -> Self {
            Self {
                calls: Vec::new(),
                output_result: Ok(CommandOutput {
                    success: true,
                    stdout: b"help".to_vec(),
                    stderr: Vec::new(),
                }),
                status_result: Ok(CommandExit {
                    success: true,
                    code: Some(0),
                }),
            }
        }

        fn failing_status(code: i32) -> Self {
            Self {
                status_result: Ok(CommandExit {
                    success: false,
                    code: Some(code),
                }),
                ..Self::available()
            }
        }
    }

    impl CommandRunner for MockRunner {
        fn output(&mut self, program: &Path, args: &[&str]) -> io::Result<CommandOutput> {
            self.calls.push(Call {
                program: program.to_path_buf(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                inherits_stdout_stderr: false,
                stdin_null: false,
            });
            self.output_result
                .as_ref()
                .map(Clone::clone)
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }

        fn status_inherited_output(
            &mut self,
            program: &Path,
            args: &[&str],
        ) -> io::Result<CommandExit> {
            self.calls.push(Call {
                program: program.to_path_buf(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                inherits_stdout_stderr: true,
                stdin_null: true,
            });
            self.status_result
                .as_ref()
                .copied()
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }
    }

    #[test]
    fn gajae_status_prefers_gajae_bin_env_override() {
        let mut runner = MockRunner::available();
        run_status_with(&mut runner, |name| {
            (name == GAJAE_ENV).then(|| OsString::from("/custom/gajae"))
        })
        .expect("status should pass");

        assert_eq!(
            runner.calls,
            vec![Call {
                program: PathBuf::from("/custom/gajae"),
                args: vec!["--help".into()],
                inherits_stdout_stderr: false,
                stdin_null: false,
            }]
        );
    }

    #[test]
    fn gajae_status_uses_path_name_when_env_is_absent() {
        let mut runner = MockRunner::available();
        run_status_with(&mut runner, |_| None).expect("status should pass");

        assert_eq!(runner.calls[0].program, PathBuf::from("gajae"));
    }

    #[test]
    fn gajae_status_fails_when_help_exits_nonzero() {
        let mut runner = MockRunner {
            output_result: Ok(CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"usage unavailable".to_vec(),
            }),
            ..MockRunner::available()
        };

        let error = run_status_with(&mut runner, |_| None).expect_err("nonzero help should fail");

        assert!(
            error.to_string().contains("usage unavailable"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn profile_install_constructs_expected_args_inherits_output_and_closes_stdin() {
        let mut runner = MockRunner::available();
        let status = run_profile_install_with(&mut runner, |_| None).expect("install should run");
        assert!(status.success);

        assert_eq!(
            runner.calls,
            vec![Call {
                program: PathBuf::from("gajae"),
                args: PROFILE_INSTALL_ARGS
                    .iter()
                    .map(|arg| (*arg).to_string())
                    .collect(),
                inherits_stdout_stderr: true,
                stdin_null: true,
            }]
        );
    }

    #[test]
    fn profile_install_fails_on_nonzero_status() {
        let mut runner = MockRunner::failing_status(17);
        let status =
            run_profile_install_with(&mut runner, |_| None).expect("nonzero still reports status");
        assert_eq!(status.code, Some(17));

        let message = profile_install_failure_message(status);
        assert!(
            message.contains("exit code 17"),
            "unexpected message: {message}"
        );
    }
}
