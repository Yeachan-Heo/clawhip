use async_trait::async_trait;
use serde_json::{Value, json};
use std::fs::{OpenOptions, create_dir_all};
use std::io::Write;
use std::path::Path;

use crate::Result;

use super::{Sink, SinkMessage, SinkTarget};

#[derive(Clone, Default)]
pub struct LocalFileSink;

fn nested_str<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str()
}

fn truncate(value: Option<&str>, max_len: usize) -> Option<String> {
    value.map(|s| {
        if s.len() <= max_len {
            s.to_string()
        } else {
            format!("{}…", &s[..max_len])
        }
    })
}

fn summarize_payload(payload: &Value) -> Value {
    json!({
        "provider": payload.get("provider").and_then(Value::as_str),
        "session_id": payload.get("session_id").and_then(Value::as_str),
        "repo_name": payload.get("repo_name").and_then(Value::as_str),
        "repo_path": payload.get("repo_path").and_then(Value::as_str),
        "directory": payload.get("directory").and_then(Value::as_str),
        "event_name": payload.get("event_name").and_then(Value::as_str),
        "hook_event_name": payload.get("hook_event_name").and_then(Value::as_str),
        "tool_name": payload.get("tool_name").and_then(Value::as_str),
        "command": nested_str(payload, &["tool_input", "command"]),
        "prompt": truncate(payload.get("prompt").and_then(Value::as_str), 240),
        "summary": truncate(payload.get("summary_text").and_then(Value::as_str), 240),
        "transcript_path": payload.get("transcript_path").and_then(Value::as_str),
        "turn_id": payload.get("turn_id").and_then(Value::as_str),
        "last_assistant_message": truncate(nested_str(payload, &["event_payload", "last_assistant_message"]), 240),
        "tool_response": truncate(payload.get("tool_response").and_then(Value::as_str), 240),
    })
}

#[async_trait]
impl Sink for LocalFileSink {
    async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()> {
        let SinkTarget::LocalFile(path) = target else {
            return Err("localfile sink received non-local target".into());
        };

        let path_ref = Path::new(path);
        if let Some(parent) = path_ref.parent() {
            create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path_ref)?;

        let record = json!({
            "event_kind": message.event_kind,
            "format": message.format.as_str(),
            "content": truncate(Some(&message.content), 240),
            "summary_payload": summarize_payload(&message.payload),
        });
        writeln!(file, "{}", serde_json::to_string(&record)?)?;
        Ok(())
    }
}
