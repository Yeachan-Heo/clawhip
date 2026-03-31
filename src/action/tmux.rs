use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::config::ActionSpec;
use crate::events::IncomingEvent;
use crate::tmux_ops;

use super::{Action, ActionOutcome};

pub struct TmuxSendKeysAction;

#[async_trait]
impl Action for TmuxSendKeysAction {
    fn name(&self) -> &str {
        "tmux.send-keys"
    }

    async fn execute(&self, spec: &ActionSpec, event: &IncomingEvent) -> ActionOutcome {
        let context = event.template_context();

        let target = spec
            .target
            .as_deref()
            .map(|t| resolve_template(t, &context))
            .or_else(|| {
                event
                    .payload
                    .get("session")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
            });

        let Some(target) = target.filter(|t| !t.is_empty()) else {
            return ActionOutcome::Failed("no target session for tmux.send-keys".into());
        };

        let keys = spec.keys.as_deref().unwrap_or("continue");

        if let Err(error) = tmux_ops::send_literal_keys(&target, keys).await {
            return ActionOutcome::Failed(format!("send-keys literal to '{target}': {error}"));
        }

        if let Err(error) = tmux_ops::send_key(&target, "Enter").await {
            return ActionOutcome::Failed(format!("send-keys Enter to '{target}': {error}"));
        }

        ActionOutcome::Success(format!("sent '{keys}' + Enter to tmux session '{target}'"))
    }
}

fn resolve_template(template: &str, context: &BTreeMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in context {
        result = result.replace(&format!("{{{key}}}"), value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_template_substitutes_known_keys() {
        let mut context = BTreeMap::new();
        context.insert("session".into(), "issue-24".into());
        context.insert("pane".into(), "0.0".into());

        assert_eq!(
            resolve_template("{session}:{pane}", &context),
            "issue-24:0.0"
        );
    }

    #[test]
    fn resolve_template_leaves_unknown_keys_as_is() {
        let context = BTreeMap::new();
        assert_eq!(resolve_template("{unknown}", &context), "{unknown}");
    }

    #[test]
    fn resolve_template_handles_empty_template() {
        let context = BTreeMap::new();
        assert_eq!(resolve_template("", &context), "");
    }
}
