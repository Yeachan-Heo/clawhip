use tokio::process::Command;

use crate::Result;
use crate::source::tmux::tmux_bin;

/// Send literal text to a tmux session/pane target via `send-keys -l`.
pub async fn send_literal_keys(target: &str, text: &str) -> Result<()> {
    let output = Command::new(tmux_bin())
        .arg("send-keys")
        .arg("-t")
        .arg(target)
        .arg("-l")
        .arg(text)
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }
    Ok(())
}

/// Send a named key (e.g. "Enter") to a tmux session/pane target.
pub async fn send_key(target: &str, key: &str) -> Result<()> {
    let output = Command::new(tmux_bin())
        .arg("send-keys")
        .arg("-t")
        .arg(target)
        .arg(key)
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }
    Ok(())
}

fn tmux_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}
