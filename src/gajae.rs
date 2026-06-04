use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::events::IncomingEvent;

const GAJAE_ENV: &str = "GAJAE_BIN";
const GAJAE_PATH_NAME: &str = "gajae";
const PROFILE_INSTALL_ARGS: &[&str] = &["clawhip", "profile", "install"];
const SUMMARY_LIMIT: usize = 240;
const RECEIPT_STDIN_LIMIT: usize = 1_048_576;

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
    fn output_with_stdin(
        &mut self,
        program: &Path,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> io::Result<CommandOutput>;
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
    fn output_with_stdin(
        &mut self,
        program: &Path,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> io::Result<CommandOutput> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = stdin
            && let Some(mut child_stdin) = child.stdin.take()
        {
            child_stdin.write_all(input)?;
        }
        let output = child.wait_with_output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptSource {
    File(PathBuf),
    Stdin(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptIngestRequest {
    pub family: String,
    pub source: ReceiptSource,
    pub channel: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReceiptIngestResult {
    pub event: IncomingEvent,
}

pub fn read_receipt_stdin(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    reader
        .take((RECEIPT_STDIN_LIMIT + 1) as u64)
        .read_to_end(&mut input)
        .context("failed to read receipt from stdin")?;
    if input.len() > RECEIPT_STDIN_LIMIT {
        bail!("receipt stdin exceeds {RECEIPT_STDIN_LIMIT} byte limit");
    }
    Ok(input)
}

pub fn ingest_receipt(request: ReceiptIngestRequest) -> Result<ReceiptIngestResult> {
    let mut runner = StdCommandRunner;
    ingest_receipt_with(&mut runner, |name| std::env::var_os(name), request)
}

fn ingest_receipt_with(
    runner: &mut impl CommandRunner,
    env_var: impl Fn(&str) -> Option<OsString>,
    request: ReceiptIngestRequest,
) -> Result<ReceiptIngestResult> {
    let family = sanitize_family(&request.family)?;
    let bin = discover_gajae_with(env_var);
    let temp;
    let file_path = match &request.source {
        ReceiptSource::File(path) => path.as_path(),
        ReceiptSource::Stdin(input) => {
            temp = write_receipt_tempfile(input)?;
            temp.path()
        }
    };
    let file_arg = file_path
        .to_str()
        .ok_or_else(|| anyhow!("receipt file path is not valid UTF-8"))?;
    let args = [family.as_str(), "validate", "--file", file_arg];
    let output = runner
        .output_with_stdin(&bin, &args, None)
        .with_context(|| format!("failed to run {} {} validate", bin.display(), family))?;
    if !output.success {
        bail!(
            "gajae receipt validation failed for family {}{}",
            family,
            validation_detail(&output)
        );
    }

    let validation = parse_validation_output(&output)?;
    Ok(ReceiptIngestResult {
        event: receipt_event(&family, validation, request.channel),
    })
}

fn write_receipt_tempfile(input: &[u8]) -> Result<TempReceiptFile> {
    let path = std::env::temp_dir().join(format!(
        "clawhip-gajae-receipt-{}.json",
        uuid::Uuid::new_v4()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .context("failed to create temporary receipt file")?;
    file.write_all(input)
        .context("failed to write temporary receipt file")?;
    file.sync_all()
        .context("failed to sync temporary receipt file")?;
    Ok(TempReceiptFile { path })
}

#[derive(Debug)]
struct TempReceiptFile {
    path: PathBuf,
}

impl TempReceiptFile {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempReceiptFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn parse_validation_output(output: &CommandOutput) -> Result<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout)
        && value.is_object()
    {
        return Ok(value);
    }
    if let Ok(value) = serde_json::from_slice::<Value>(&output.stderr)
        && value.is_object()
    {
        return Ok(value);
    }
    Ok(json!({}))
}

fn receipt_event(family: &str, validation: Value, channel: Option<String>) -> IncomingEvent {
    let mut payload = Map::new();
    payload.insert("family".into(), json!(family));
    payload.insert("status".into(), json!("validated"));
    insert_safe_string(
        &mut payload,
        "receipt_id",
        first_string(&validation, &["receipt_id", "id"]),
    );
    insert_safe_string(
        &mut payload,
        "subject",
        first_string(&validation, &["subject", "target"]),
    );
    insert_safe_string(
        &mut payload,
        "verdict",
        first_string(&validation, &["verdict", "decision", "outcome"]),
    );
    insert_safe_string(
        &mut payload,
        "summary",
        first_string(&validation, &["summary", "reason"]),
    );

    IncomingEvent::workspace(
        event_kind_for_family(family).to_string(),
        Value::Object(payload),
        channel,
    )
}

fn event_kind_for_family(family: &str) -> &'static str {
    match family {
        "review-verdict-evidence" => "gajae.review.verdict",
        "merge-hold-decision" => "gajae.merge.hold",
        "zero-backlog-checkpoint" => "gajae.backlog.zero",
        family if family.contains("release-hold") => "gajae.release.hold",
        _ => "gajae.receipt.validated",
    }
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .map(bounded_public_string)
        .filter(|value| !value.is_empty())
}

fn insert_safe_string(object: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        object.insert(key.to_string(), json!(value));
    }
}

fn sanitize_family(family: &str) -> Result<String> {
    let family = family.trim();
    if family.is_empty() {
        bail!("receipt family is required");
    }
    if !family
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("receipt family must contain only lowercase letters, digits, and '-' characters");
    }
    Ok(family.to_string())
}

fn validation_detail(_output: &CommandOutput) -> String {
    ": validator rejected receipt".to_string()
}

fn bounded_public_string(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        let safe = match ch {
            '\n' | '\r' | '\t' => ' ',
            '/' | '\\' => ' ',
            ch if ch.is_control() => ' ',
            ch => ch,
        };
        if out.len() + safe.len_utf8() > SUMMARY_LIMIT {
            break;
        }
        out.push(safe);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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
    use serde_json::json;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        program: PathBuf,
        args: Vec<String>,
        inherits_stdout_stderr: bool,
        stdin_null: bool,
        stdin_piped: bool,
    }

    #[derive(Debug)]
    struct MockRunner {
        calls: Vec<Call>,
        output_result: io::Result<CommandOutput>,
        status_result: io::Result<CommandExit>,
        output_with_stdin_result: io::Result<CommandOutput>,
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
                output_with_stdin_result: Ok(CommandOutput {
                    success: true,
                    stdout: br#"{"receipt_id":"r1","verdict":"approve","summary":"safe summary"}"#
                        .to_vec(),
                    stderr: Vec::new(),
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
                stdin_piped: false,
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
                stdin_piped: false,
            });
            self.status_result
                .as_ref()
                .copied()
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }

        fn output_with_stdin(
            &mut self,
            program: &Path,
            args: &[&str],
            stdin: Option<&[u8]>,
        ) -> io::Result<CommandOutput> {
            self.calls.push(Call {
                program: program.to_path_buf(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                inherits_stdout_stderr: false,
                stdin_null: stdin.is_none(),
                stdin_piped: stdin.is_some(),
            });
            self.output_with_stdin_result
                .as_ref()
                .map(Clone::clone)
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
                stdin_piped: false,
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
                stdin_piped: false,
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

    #[test]
    fn receipt_ingest_invokes_family_validator_and_maps_safe_event() {
        let mut runner = MockRunner::available();
        let result = ingest_receipt_with(
            &mut runner,
            |_| Some(OsString::from("/custom/gajae")),
            ReceiptIngestRequest {
                family: "review-verdict-evidence".into(),
                source: ReceiptSource::File(PathBuf::from("receipt.json")),
                channel: Some("ops".into()),
            },
        )
        .expect("receipt should validate");

        assert_eq!(result.event.kind, "gajae.review.verdict");
        assert_eq!(result.event.channel.as_deref(), Some("ops"));
        assert_eq!(
            result.event.payload["family"],
            json!("review-verdict-evidence")
        );
        assert_eq!(result.event.payload["receipt_id"], json!("r1"));
        assert_eq!(result.event.payload["verdict"], json!("approve"));
        assert_eq!(result.event.payload["summary"], json!("safe summary"));
        assert_eq!(
            runner.calls,
            vec![Call {
                program: PathBuf::from("/custom/gajae"),
                args: vec![
                    "review-verdict-evidence".into(),
                    "validate".into(),
                    "--file".into(),
                    "receipt.json".into(),
                ],
                inherits_stdout_stderr: false,
                stdin_null: true,
                stdin_piped: false,
            }]
        );
    }

    #[test]
    fn receipt_ingest_rejects_invalid_receipt_with_bounded_public_diagnostic() {
        let mut runner = MockRunner {
            output_with_stdin_result: Ok(CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: format!("/secret/path/token {}", "x".repeat(400)).into_bytes(),
            }),
            ..MockRunner::available()
        };

        let error = ingest_receipt_with(
            &mut runner,
            |_| None,
            ReceiptIngestRequest {
                family: "runtime-followup-receipt".into(),
                source: ReceiptSource::File(PathBuf::from("receipt.json")),
                channel: None,
            },
        )
        .expect_err("invalid receipt should fail");
        let message = error.to_string();
        assert!(message.contains("gajae receipt validation failed"));
        assert!(!message.contains("/secret/path"), "message={message}");
        assert!(message.len() < 360, "message too long: {}", message.len());
    }
}
