pub mod tmux;

use std::collections::HashMap;

use async_trait::async_trait;

use crate::config::ActionSpec;
use crate::events::IncomingEvent;

#[derive(Debug, Clone)]
pub enum ActionOutcome {
    Success(String),
    Failed(String),
}

#[async_trait]
pub trait Action: Send + Sync {
    #[allow(dead_code)]
    fn name(&self) -> &str;
    async fn execute(&self, spec: &ActionSpec, event: &IncomingEvent) -> ActionOutcome;
}

pub fn default_actions() -> HashMap<String, Box<dyn Action>> {
    let mut actions: HashMap<String, Box<dyn Action>> = HashMap::new();
    actions.insert("tmux.send-keys".into(), Box::new(tmux::TmuxSendKeysAction));
    actions
}
