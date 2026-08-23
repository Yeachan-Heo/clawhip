//! CLI surface for the GJC SDK control plane (#323). Every command talks to
//! the local daemon and renders public-safe JSON or text.

use std::sync::Arc;

use serde_json::Value;

use crate::Result;
use crate::cli::GjcCommands;
use crate::client::DaemonClient;
use crate::config::AppConfig;
use crate::gjc::model::{ControlRequestEnvelope, GJC_CONTROL_SCHEMA};

fn mutation_payload(
    session: &str,
    mutation: &crate::cli::GjcMutationArgs,
    extra: Value,
) -> Result<Value> {
    let mut payload = serde_json::json!({
        "session": session,
        "idempotency_key": mutation.idempotency_key,
        "expected_session": mutation.expected_session,
        "timeout_ms": mutation
            .timeout_ms
            .unwrap_or(ControlRequestEnvelope::DEFAULT_TIMEOUT_MS),
    });
    if let Value::Object(map) = extra {
        for (key, value) in map {
            payload[key] = value;
        }
    }
    Ok(payload)
}

fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Compact public-safe text rendering used when `--json` is absent.
fn print_text(value: &Value, keys: &[&str]) {
    for key in keys.iter() {
        let label = key.to_uppercase();
        match value.get(*key) {
            Some(Value::Null) | None => println!("{label}: -"),
            Some(Value::String(text)) if text.is_empty() => println!("{label}: -"),
            Some(other) => println!("{label}: {other}"),
        }
    }
}

pub async fn run(config: Arc<AppConfig>, command: GjcCommands) -> Result<()> {
    let client = DaemonClient::from_config(config.as_ref());
    match command {
        GjcCommands::Capabilities { json } => {
            let body = client.gjc_capabilities().await?;
            render(
                &body,
                json,
                &["schema", "transport_implemented", "capabilities"],
            )
        }
        GjcCommands::Session {
            session,
            sections,
            json,
        } => {
            let body = client
                .gjc_session_query(&session, sections.as_deref())
                .await?;
            render(
                &body,
                json,
                &[
                    "metadata",
                    "stats",
                    "model_profile",
                    "turn",
                    "queue",
                    "workflow_gates",
                    "goal_todo",
                ],
            )
        }
        GjcCommands::TurnOutcome {
            session,
            turn_id,
            json,
        } => {
            let body = client.gjc_turn_outcome(&session, &turn_id).await?;
            render(&body, json, &["schema", "outcome"])
        }
        GjcCommands::Prompt {
            session,
            prompt,
            mutation,
        } => {
            let payload =
                mutation_payload(&session, &mutation, serde_json::json!({"prompt": prompt}))?;
            let body = client.gjc_mutation("prompt", payload).await?;
            render_receipt(&body, mutation.json)
        }
        GjcCommands::Steer {
            session,
            message,
            mutation,
        } => {
            let payload =
                mutation_payload(&session, &mutation, serde_json::json!({"message": message}))?;
            let body = client.gjc_mutation("steer", payload).await?;
            render_receipt(&body, mutation.json)
        }
        GjcCommands::AbortAndPrompt {
            session,
            prompt,
            turn_ids,
            mutation,
        } => {
            let payload = mutation_payload(
                &session,
                &mutation,
                serde_json::json!({"prompt": prompt, "turn_ids": turn_ids}),
            )?;
            let body = client.gjc_mutation("abort-and-prompt", payload).await?;
            render_receipt(&body, mutation.json)
        }
        GjcCommands::GateAnswer {
            session,
            gate_id,
            option,
            mutation,
        } => {
            let payload = mutation_payload(
                &session,
                &mutation,
                serde_json::json!({"gate_id": gate_id, "option": option}),
            )?;
            let body = client.gjc_mutation("workflow-gate-answer", payload).await?;
            render_receipt(&body, mutation.json)
        }
        GjcCommands::AskAnswer {
            session,
            ask_id,
            choices,
            mutation,
        } => {
            let payload = mutation_payload(
                &session,
                &mutation,
                serde_json::json!({"ask_id": ask_id, "choices": choices}),
            )?;
            let body = client.gjc_mutation("ask-answer", payload).await?;
            render_receipt(&body, mutation.json)
        }
        GjcCommands::Select {
            session,
            model,
            profile,
            mutation,
        } => {
            let payload = mutation_payload(
                &session,
                &mutation,
                serde_json::json!({"model": model, "profile": profile}),
            )?;
            let body = client.gjc_mutation("model-selection", payload).await?;
            render_receipt(&body, mutation.json)
        }
        GjcCommands::Receipt {
            idempotency_key,
            json,
        } => {
            let body = client.gjc_command_receipt(&idempotency_key).await?;
            render_receipt(&body, json)
        }
    }
}

fn render(body: &Value, json: bool, keys: &[&str]) -> Result<()> {
    if json {
        print_json(body)?;
    } else {
        print_text(body, keys);
    }
    Ok(())
}

fn render_receipt(body: &Value, json: bool) -> Result<()> {
    if json {
        return print_json(body);
    }
    // Receipts always carry the schema marker; keep the output honest even
    // on unexpected shapes.
    if body.get("schema").and_then(Value::as_str) != Some(GJC_CONTROL_SCHEMA) {
        println!("SCHEMA: -");
        return Ok(());
    }
    print_text(
        body,
        &[
            "command_id",
            "kind",
            "session_id",
            "status",
            "turn_id",
            "outcome",
            "created_at",
        ],
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_payload_carries_common_envelope_fields() {
        let mutation = crate::cli::GjcMutationArgs {
            idempotency_key: "idem-key-0001".into(),
            expected_session: Some("sess-1".into()),
            timeout_ms: Some(2_500),
            json: false,
        };
        let payload =
            mutation_payload("sess-1", &mutation, serde_json::json!({"prompt": "hello"})).unwrap();
        assert_eq!(payload["session"], "sess-1");
        assert_eq!(payload["idempotency_key"], "idem-key-0001");
        assert_eq!(payload["expected_session"], "sess-1");
        assert_eq!(payload["timeout_ms"], 2_500);
        assert_eq!(payload["prompt"], "hello");
    }

    #[test]
    fn receipt_rendering_requires_schema_marker() {
        // Text mode never invents fields; a non-receipt body prints a dash.
        // (Rendering writes to stdout; here we only assert no panic.)
        let _ = render_receipt(&serde_json::json!({"unexpected": true}), false);
    }

    #[test]
    fn text_renderer_skips_null_and_empty_fields() {
        // (Rendering writes to stdout; assert no panic on mixed shapes.)
        print_text(
            &serde_json::json!({"a": null, "b": "", "c": "x"}),
            &["a", "b", "c"],
        );
    }
}
