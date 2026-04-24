use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct DlqEntry {
    pub original_topic: String,
    pub retry_count: u32,
    pub last_error: String,
    pub target: String,
    pub event_kind: String,
    pub format: String,
    pub content: String,
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_bytes: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct Dlq {
    entries: Vec<DlqEntry>,
}

impl Dlq {
    pub fn push(&mut self, entry: DlqEntry) {
        self.entries.push(entry);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    #[must_use]
    pub fn entries(&self) -> &[DlqEntry] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stores_full_context() {
        let mut dlq = Dlq::default();
        dlq.push(DlqEntry {
            original_topic: "github.ci-failed".into(),
            retry_count: 3,
            last_error: "boom".into(),
            target: "discord:alerts".into(),
            event_kind: "github.ci-failed".into(),
            format: "compact".into(),
            content: "msg".into(),
            payload: json!({"repo":"clawhip"}),
            correlation_id: Some("corr-1".into()),
            content_bytes: Some(3),
            payload_bytes: Some(18),
        });
        assert_eq!(dlq.entries().len(), 1);
        assert_eq!(dlq.entries()[0].payload["repo"], "clawhip");
        assert_eq!(dlq.entries()[0].correlation_id.as_deref(), Some("corr-1"));
    }
}
