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
            let boundary = s
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= max_len)
                .last()
                .unwrap_or(0);
            format!("{}…", &s[..boundary])
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::MessageFormat;

    #[test]
    fn truncate_respects_utf8_char_boundaries() {
        let input = format!("{}🚨", "a".repeat(239));

        let truncated = truncate(Some(&input), 240).expect("truncated value");

        assert_eq!(truncated, format!("{}…", "a".repeat(239)));
    }

    #[tokio::test]
    async fn send_appends_jsonl_record_with_summarized_payload() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("events.jsonl");
        let sink = LocalFileSink;
        let message = SinkMessage {
            event_kind: "session.finished".into(),
            format: MessageFormat::Compact,
            content: "done".into(),
            payload: json!({
                "provider": "codex",
                "prompt": "ship it",
                "event_payload": {
                    "last_assistant_message": "complete"
                }
            }),
            telemetry: None,
        };
        let target = SinkTarget::LocalFile(path.display().to_string());

        sink.send(&target, &message).await.expect("first append");
        sink.send(&target, &message).await.expect("second append");

        let contents = std::fs::read_to_string(&path).expect("jsonl");
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        let record: Value = serde_json::from_str(lines[0]).expect("json record");
        assert_eq!(record["event_kind"], "session.finished");
        assert_eq!(record["format"], "compact");
        assert_eq!(record["content"], "done");
        assert_eq!(record["summary_payload"]["provider"], "codex");
        assert_eq!(record["summary_payload"]["prompt"], "ship it");
        assert_eq!(
            record["summary_payload"]["last_assistant_message"],
            "complete"
        );
    }
}
