//! Structured provenance for event routing decisions.
//!
//! A [`Provenance`] record explains *why* an event was (or was not) delivered
//! to a given sink. Operators can ask for provenance via `clawhip explain` to
//! answer questions like "what emitted this message?" and "why did this route
//! fire?" without having to read the config by hand.

use std::fmt;

use serde::Serialize;

/// Complete provenance trace for a routing decision on a single event.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Provenance {
    /// Original event kind as submitted by the caller.
    pub event_kind: String,
    /// Canonical kind after normalization (what the router actually matches on).
    pub canonical_kind: String,
    /// Candidate route keys tried in order.
    pub route_candidates: Vec<String>,
    /// One entry per configured route describing why it matched or not.
    pub routes: Vec<RouteExplanation>,
    /// The concrete deliveries this event would produce, in dispatch order.
    pub deliveries: Vec<DeliveryExplanation>,
}

/// Outcome of evaluating a single [`RouteRule`](crate::config::RouteRule) against an event.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteExplanation {
    /// Zero-based index in `config.routes`.
    pub route_index: usize,
    /// The event pattern from the route rule (as written in config).
    pub event_pattern: String,
    /// Whether every condition (pattern + all filters) passed.
    pub matched: bool,
    /// Whether the event pattern alone matched the canonical kind.
    pub pattern_matched: bool,
    /// Per-filter match details. Empty when the route has no filters.
    pub filter_results: Vec<FilterResult>,
}

/// Per-filter match detail.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FilterResult {
    /// Filter key (e.g. `repo_name`, `branch`).
    pub key: String,
    /// Expected glob pattern from the route config.
    pub pattern: String,
    /// Actual value observed on the event, if any.
    pub actual: Option<String>,
    /// Whether the actual value matched the pattern.
    pub matched: bool,
}

/// A delivery that would be produced by the dispatcher.
///
/// `matched_route_index == None` means the delivery came from the default
/// fallback (no configured route matched the event).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeliveryExplanation {
    pub sink: String,
    pub target: String,
    pub channel: Option<String>,
    pub format: String,
    pub mention: Option<String>,
    pub template: Option<String>,
    pub matched_route_index: Option<usize>,
}

impl fmt::Display for Provenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "event: {}", self.event_kind)?;
        if self.canonical_kind != self.event_kind {
            writeln!(f, "canonical: {}", self.canonical_kind)?;
        }
        writeln!(f, "route candidates: {}", self.route_candidates.join(", "))?;

        if self.routes.is_empty() {
            writeln!(f, "routes: (none configured)")?;
        } else {
            writeln!(f, "routes:")?;
            for route in &self.routes {
                writeln!(f, "  {}", route)?;
            }
        }

        if self.deliveries.is_empty() {
            writeln!(f, "deliveries: (none - event would be dropped)")?;
        } else {
            writeln!(f, "deliveries:")?;
            for delivery in &self.deliveries {
                writeln!(f, "  {}", delivery)?;
            }
        }

        Ok(())
    }
}

impl fmt::Display for RouteExplanation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = if self.matched { "MATCH" } else { "skip " };
        write!(
            f,
            "[{status}] #{idx} event={pattern:?}",
            idx = self.route_index,
            pattern = self.event_pattern,
        )?;
        if !self.pattern_matched {
            write!(f, " (pattern mismatch)")?;
        }
        for filter in &self.filter_results {
            write!(f, " {filter}")?;
        }
        Ok(())
    }
}

impl fmt::Display for FilterResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let marker = if self.matched { "✓" } else { "✗" };
        let actual = self
            .actual
            .as_deref()
            .map(|v| format!("{v:?}"))
            .unwrap_or_else(|| "<missing>".to_string());
        write!(
            f,
            "{marker}{key}:{pattern:?}→{actual}",
            key = self.key,
            pattern = self.pattern,
        )
    }
}

impl fmt::Display for DeliveryExplanation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let route_tag = match self.matched_route_index {
            Some(idx) => format!("route #{idx}"),
            None => "default".to_string(),
        };
        write!(
            f,
            "[{route_tag}] {sink} → {target} (format={format}",
            sink = self.sink,
            target = self.target,
            format = self.format,
        )?;
        if let Some(mention) = self.mention.as_deref() {
            write!(f, ", mention={mention:?}")?;
        }
        if self.template.is_some() {
            write!(f, ", template=custom")?;
        }
        write!(f, ")")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_provenance_renders_matched_and_skipped_routes() {
        let provenance = Provenance {
            event_kind: "git.commit".into(),
            canonical_kind: "git.commit".into(),
            route_candidates: vec!["git.commit".into(), "github.commit".into()],
            routes: vec![
                RouteExplanation {
                    route_index: 0,
                    event_pattern: "git.commit".into(),
                    matched: true,
                    pattern_matched: true,
                    filter_results: vec![FilterResult {
                        key: "repo_name".into(),
                        pattern: "clawhip".into(),
                        actual: Some("clawhip".into()),
                        matched: true,
                    }],
                },
                RouteExplanation {
                    route_index: 1,
                    event_pattern: "git.commit".into(),
                    matched: false,
                    pattern_matched: true,
                    filter_results: vec![FilterResult {
                        key: "branch".into(),
                        pattern: "main".into(),
                        actual: Some("feature".into()),
                        matched: false,
                    }],
                },
            ],
            deliveries: vec![DeliveryExplanation {
                sink: "discord".into(),
                target: "DiscordChannel(\"commits\")".into(),
                channel: Some("commits".into()),
                format: "compact".into(),
                mention: Some("@devs".into()),
                template: None,
                matched_route_index: Some(0),
            }],
        };

        let output = provenance.to_string();
        assert!(output.contains("event: git.commit"));
        assert!(output.contains("route candidates: git.commit, github.commit"));
        assert!(output.contains("[MATCH] #0"));
        assert!(output.contains("[skip ] #1"));
        assert!(output.contains("✓repo_name"));
        assert!(output.contains("✗branch"));
        assert!(output.contains("[route #0] discord"));
        assert!(output.contains("@devs"));
    }

    #[test]
    fn display_notes_when_no_deliveries_would_fire() {
        let provenance = Provenance {
            event_kind: "custom".into(),
            canonical_kind: "custom".into(),
            route_candidates: vec!["custom".into()],
            routes: vec![],
            deliveries: vec![],
        };

        let output = provenance.to_string();
        assert!(output.contains("routes: (none configured)"));
        assert!(output.contains("deliveries: (none - event would be dropped)"));
    }

    #[test]
    fn display_marks_default_fallback_delivery() {
        let provenance = Provenance {
            event_kind: "custom".into(),
            canonical_kind: "custom".into(),
            route_candidates: vec!["custom".into()],
            routes: vec![RouteExplanation {
                route_index: 0,
                event_pattern: "github.*".into(),
                matched: false,
                pattern_matched: false,
                filter_results: vec![],
            }],
            deliveries: vec![DeliveryExplanation {
                sink: "discord".into(),
                target: "DiscordChannel(\"fallback\")".into(),
                channel: Some("fallback".into()),
                format: "alert".into(),
                mention: None,
                template: None,
                matched_route_index: None,
            }],
        };

        let output = provenance.to_string();
        assert!(output.contains("(pattern mismatch)"));
        assert!(output.contains("[default] discord"));
    }
}
