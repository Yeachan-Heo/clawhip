use std::collections::HashSet;
use std::time::{Duration, Instant};

const LAUNCHER_NOISE_PATTERNS: &[&str] = &[
    "clawhip emit agent.started",
    "clawhip emit agent.finished",
    "clawhip emit agent.failed",
    "function else>",
    "registered_at=",
    "parent_pid=",
    "parent_name=",
    "--error \"exit $exit_code\"",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordHit {
    pub keyword: String,
    pub line: String,
    pub provenance: Option<KeywordMatchProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordMatchProvenance {
    pub pane_id: String,
    pub pane_name: String,
    pub cursor: Option<usize>,
    pub source: KeywordMatchSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordMatchSource {
    FreshOutput,
}

#[derive(Debug, Clone)]
pub struct PendingKeywordHits {
    started_at: Instant,
    hits: Vec<KeywordHit>,
    seen: HashSet<(String, String)>,
}

impl PendingKeywordHits {
    pub fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            hits: Vec::new(),
            seen: HashSet::new(),
        }
    }

    pub fn push(&mut self, hits: Vec<KeywordHit>) {
        for hit in hits {
            let key = (hit.keyword.clone(), hit.line.clone());
            if self.seen.insert(key) {
                self.hits.push(hit);
            }
        }
    }

    pub fn ready_to_flush(&self, now: Instant, window: Duration) -> bool {
        now.duration_since(self.started_at) >= window
    }

    pub fn into_hits(self) -> Vec<KeywordHit> {
        self.hits
    }
}

#[cfg(test)]
pub fn collect_keyword_hits(previous: &str, current: &str, keywords: &[String]) -> Vec<KeywordHit> {
    collect_keyword_hits_from_lines(
        appended_lines_with_cursors(previous, current)
            .into_iter()
            .map(|(_, line)| (None, line))
            .collect(),
        keywords,
        None,
    )
}

pub fn collect_keyword_hits_with_provenance(
    previous: &str,
    current: &str,
    keywords: &[String],
    provenance: KeywordMatchProvenance,
) -> Vec<KeywordHit> {
    collect_keyword_hits_from_lines(
        appended_lines_with_cursors(previous, current)
            .into_iter()
            .map(|(cursor, line)| (Some(cursor), line))
            .collect(),
        keywords,
        Some(provenance),
    )
}

fn collect_keyword_hits_from_lines(
    lines: Vec<(Option<usize>, &str)>,
    keywords: &[String],
    provenance: Option<KeywordMatchProvenance>,
) -> Vec<KeywordHit> {
    if keywords.is_empty() {
        return Vec::new();
    }

    let normalized_keywords = keywords
        .iter()
        .map(|keyword| (keyword.clone(), keyword.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    let mut hits = Vec::new();

    for (line_cursor, line) in lines {
        if should_ignore_launcher_line(line) {
            continue;
        }

        let lower_line = line.to_ascii_lowercase();
        for (keyword, lower_keyword) in &normalized_keywords {
            if lower_line.contains(lower_keyword) {
                if is_negated_default_failure_match(lower_keyword, &lower_line)
                    || is_instruction_or_search_review_marker_prose(lower_keyword, line)
                {
                    continue;
                }

                let key = (keyword.clone(), line.to_string());
                if seen.insert(key.clone()) {
                    hits.push(KeywordHit {
                        keyword: key.0,
                        line: key.1,
                        provenance: provenance.clone().map(|mut provenance| {
                            if let Some(cursor) = line_cursor {
                                provenance.cursor = Some(cursor);
                            }
                            provenance
                        }),
                    });
                }
            }
        }
    }

    hits
}

fn is_negated_default_failure_match(lower_keyword: &str, lower_line: &str) -> bool {
    match lower_keyword {
        "error" | "errors" => contains_any(
            lower_line,
            &[
                "0 error",
                "0 errors",
                "zero error",
                "zero errors",
                "no error",
                "no errors",
                "without error",
                "without errors",
            ],
        ),
        "fail" | "fails" | "failed" | "failure" | "failures" => contains_any(
            lower_line,
            &[
                "0 fail",
                "0 fails",
                "0 failure",
                "0 failures",
                "zero fail",
                "zero fails",
                "zero failure",
                "zero failures",
                "no fail",
                "no fails",
                "no failure",
                "no failures",
                "without fail",
                "without fails",
                "without failure",
                "without failures",
            ],
        ),
        _ => false,
    }
}

fn is_instruction_or_search_review_marker_prose(lower_keyword: &str, line: &str) -> bool {
    if !matches!(lower_keyword, "blocker" | "request_changes" | "approve") {
        return false;
    }

    let normalized = line.trim().to_ascii_lowercase();
    if normalized == lower_keyword {
        return false;
    }

    // Only suppress obvious instruction/search prose that mentions the review
    // marker as text to look for. Fresh verdict prose such as
    // "Final verdict APPROVE with evidence" or "I found a BLOCKER..." must
    // still alert; stale prompt/search scrollback is handled by the appended
    // output boundary before this filter runs.
    normalized.contains("end with")
        || normalized.contains("search ")
        || normalized.contains("query ")
        || normalized.contains("keywords")
        || normalized.contains("using ralph until")
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles
        .iter()
        .any(|needle| contains_bounded(haystack, needle))
}

fn contains_bounded(haystack: &str, needle: &str) -> bool {
    let mut search_start = 0;
    while let Some(relative_start) = haystack[search_start..].find(needle) {
        let start = search_start + relative_start;
        let end = start + needle.len();
        let before_is_word = haystack[..start]
            .chars()
            .next_back()
            .map(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            .unwrap_or(false);
        let after_is_word = haystack[end..]
            .chars()
            .next()
            .map(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            .unwrap_or(false);
        if !before_is_word && !after_is_word {
            return true;
        }
        search_start = end;
    }
    false
}

fn should_ignore_launcher_line(line: &str) -> bool {
    let trimmed = line.trim();
    LAUNCHER_NOISE_PATTERNS
        .iter()
        .any(|pattern| trimmed.contains(pattern))
}

fn appended_lines_with_cursors<'a>(previous: &'a str, current: &'a str) -> Vec<(usize, &'a str)> {
    let previous_lines = previous.lines().collect::<Vec<_>>();
    let current_lines = current.lines().collect::<Vec<_>>();
    let overlap = overlapping_suffix_prefix_len(&previous_lines, &current_lines);

    current_lines
        .into_iter()
        .enumerate()
        .skip(overlap)
        .map(|(index, line)| (index + 1, line))
        .collect()
}

fn overlapping_suffix_prefix_len(previous: &[&str], current: &[&str]) -> usize {
    let max_overlap = previous.len().min(current.len());

    for overlap in (0..=max_overlap).rev() {
        if previous[previous.len().saturating_sub(overlap)..] == current[..overlap] {
            return overlap;
        }
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_keyword_hits_dedups_same_keyword_and_line() {
        let hits = collect_keyword_hits(
            "done",
            "done\nerror: failed\nerror: failed\nERROR: FAILED",
            &["error".into()],
        );

        assert_eq!(
            hits,
            vec![
                KeywordHit {
                    keyword: "error".into(),
                    line: "error: failed".into(),
                    provenance: None,
                },
                KeywordHit {
                    keyword: "error".into(),
                    line: "ERROR: FAILED".into(),
                    provenance: None,
                },
            ]
        );
    }

    #[test]
    fn collect_keyword_hits_detects_reappended_identical_lines() {
        let hits = collect_keyword_hits(
            "done\nerror: failed",
            "done\nerror: failed\nerror: failed",
            &["error".into()],
        );

        assert_eq!(
            hits,
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: failed".into(),
                provenance: None,
            }]
        );
    }

    #[test]
    fn collect_keyword_hits_uses_snapshot_overlap_for_scrolling_history() {
        let hits = collect_keyword_hits(
            "one\ntwo\nthree",
            "two\nthree\nerror: failed",
            &["error".into()],
        );

        assert_eq!(
            hits,
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: failed".into(),
                provenance: None,
            }]
        );
    }

    #[test]
    fn collect_keyword_hits_ignores_wrapper_lifecycle_emit_lines() {
        let hits = collect_keyword_hits(
            "boot",
            "boot\nfunction else>     clawhip emit agent.failed --agent omx --session omx-pr-1340-review --project oh-my-codex --elapsed \"$elapsed\" --error \"exit $exit_code\" --mention '<@1465264645320474637>' || true\nerror: real failure",
            &["error".into(), "FAILED".into()],
        );

        assert_eq!(
            hits,
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: real failure".into(),
                provenance: None,
            }]
        );
    }

    #[test]
    fn collect_keyword_hits_ignores_tmux_wrapper_audit_lines() {
        let hits = collect_keyword_hits(
            "boot",
            "boot\nclawhip tmux cli-new start session=issue-166 channel=ops keywords=error mention=- stale_minutes=30 format=- registered_at=2026-04-07T09:58:00Z parent_pid=4242 parent_name=codex\nerror: real failure",
            &["error".into()],
        );

        assert_eq!(
            hits,
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: real failure".into(),
                provenance: None,
            }]
        );
    }

    #[test]
    fn collect_keyword_hits_ignores_wrapped_exit_error_boilerplate() {
        let hits = collect_keyword_hits(
            "boot",
            "boot\n  --error \"exit $exit_code\" \\\nFAILED: actual application failure",
            &["error".into(), "FAILED".into()],
        );

        assert_eq!(
            hits,
            vec![KeywordHit {
                keyword: "FAILED".into(),
                line: "FAILED: actual application failure".into(),
                provenance: None,
            }]
        );
    }

    #[test]
    fn collect_keyword_hits_suppresses_negated_default_failure_phrases() {
        let hits = collect_keyword_hits(
            "boot",
            "boot
0 errors, 0 warnings
completed without failure
no errors found
error: real failure",
            &["error".into()],
        );

        assert_eq!(
            hits,
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: real failure".into(),
                provenance: None,
            }]
        );
    }

    #[test]
    fn negated_failure_suppression_requires_phrase_boundaries() {
        let hits = collect_keyword_hits(
            "boot",
            "boot
10 errors remain
20 failures remain",
            &["error".into(), "failure".into()],
        );

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line, "10 errors remain");
        assert_eq!(hits[1].line, "20 failures remain");
    }

    #[test]
    fn collect_keyword_hits_ignores_startup_prompt_boundary() {
        let startup = "Fix issue #220
End with PR URL or concrete BLOCKER
ISSUE2843_PR_READY";

        assert!(
            collect_keyword_hits(
                startup,
                startup,
                &["BLOCKER".into(), "ISSUE2843_PR_READY".into()]
            )
            .is_empty()
        );
    }

    #[test]
    fn collect_keyword_hits_suppresses_instruction_marker_prose_but_keeps_custom_markers() {
        let hits = collect_keyword_hits(
            "armed",
            "armed
• Using ralph until PR/ blocker
End with PR URL or concrete BLOCKER
ISSUE2843_PR_READY",
            &["BLOCKER".into(), "ISSUE2843_PR_READY".into()],
        );

        assert_eq!(
            hits,
            vec![KeywordHit {
                keyword: "ISSUE2843_PR_READY".into(),
                line: "ISSUE2843_PR_READY".into(),
                provenance: None,
            }]
        );
    }

    #[test]
    fn collect_keyword_hits_alerts_on_fresh_review_verdict_prose() {
        let hits = collect_keyword_hits_with_provenance(
            "armed",
            "armed
Final verdict APPROVE with evidence
REQUEST_CHANGES with evidence
I found a BLOCKER in tmux cursor handling",
            &["APPROVE".into(), "REQUEST_CHANGES".into(), "BLOCKER".into()],
            KeywordMatchProvenance {
                pane_id: "%11".into(),
                pane_name: "0.0".into(),
                cursor: None,
                source: KeywordMatchSource::FreshOutput,
            },
        );

        assert_eq!(
            hits.iter().map(|hit| hit.line.as_str()).collect::<Vec<_>>(),
            vec![
                "Final verdict APPROVE with evidence",
                "REQUEST_CHANGES with evidence",
                "I found a BLOCKER in tmux cursor handling",
            ]
        );
        assert_eq!(hits[0].keyword, "APPROVE");
        assert_eq!(hits[0].provenance.as_ref().unwrap().cursor, Some(2));
        assert_eq!(hits[1].keyword, "REQUEST_CHANGES");
        assert_eq!(hits[1].provenance.as_ref().unwrap().cursor, Some(3));
        assert_eq!(hits[2].keyword, "BLOCKER");
        assert_eq!(hits[2].provenance.as_ref().unwrap().cursor, Some(4));
    }

    #[test]
    fn collect_keyword_hits_treats_existing_prompt_and_search_markers_as_existing_buffer() {
        // Regression for #220 / Discord message 1502008605518594172:
        // markers present in the user's initial prompt and search/query text
        // are registration-time scrollback, not fresh model output.
        let previous = "Welcome
End with PR_READY #220 and summary
Search keywords.*...PR_READY";
        let current = "Welcome
End with PR_READY #220 and summary
Search keywords.*...PR_READY
still running";

        let hits = collect_keyword_hits(previous, current, &["PR_READY".into()]);

        assert!(hits.is_empty());
    }

    #[test]
    fn collect_keyword_hits_suppresses_existing_prompt_search_but_keeps_fresh_custom_marker() {
        let previous = "Welcome
End with PR_READY #220 and summary
Search keywords.*...PR_READY";
        let current = "Welcome
End with PR_READY #220 and summary
Search keywords.*...PR_READY
still running
PR_READY #220";

        let hits = collect_keyword_hits_with_provenance(
            previous,
            current,
            &["PR_READY".into()],
            KeywordMatchProvenance {
                pane_id: "%9".into(),
                pane_name: "0.0".into(),
                cursor: None,
                source: KeywordMatchSource::FreshOutput,
            },
        );

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, "PR_READY #220");
        assert_eq!(hits[0].provenance.as_ref().unwrap().cursor, Some(5));
    }

    #[test]
    fn collect_keyword_hits_keeps_exact_cursor_for_fresh_custom_marker() {
        let hits = collect_keyword_hits_with_provenance(
            "boot",
            "boot
working
ISSUE220_PR_READY",
            &["ISSUE220_PR_READY".into()],
            KeywordMatchProvenance {
                pane_id: "%7".into(),
                pane_name: "0.0".into(),
                cursor: None,
                source: KeywordMatchSource::FreshOutput,
            },
        );

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].keyword, "ISSUE220_PR_READY");
        assert_eq!(hits[0].line, "ISSUE220_PR_READY");
        assert_eq!(hits[0].provenance.as_ref().unwrap().cursor, Some(3));
        assert_eq!(
            hits[0].provenance.as_ref().unwrap().source,
            KeywordMatchSource::FreshOutput
        );
    }

    #[test]
    fn pending_keyword_hits_dedups_across_window_additions() {
        let start = Instant::now();
        let mut pending = PendingKeywordHits::new(start);
        pending.push(vec![KeywordHit {
            keyword: "error".into(),
            line: "error: failed".into(),
            provenance: None,
        }]);
        pending.push(vec![
            KeywordHit {
                keyword: "error".into(),
                line: "error: failed".into(),
                provenance: None,
            },
            KeywordHit {
                keyword: "complete".into(),
                line: "complete".into(),
                provenance: None,
            },
        ]);

        assert_eq!(
            pending.into_hits(),
            vec![
                KeywordHit {
                    keyword: "error".into(),
                    line: "error: failed".into(),
                    provenance: None,
                },
                KeywordHit {
                    keyword: "complete".into(),
                    line: "complete".into(),
                    provenance: None,
                },
            ]
        );
    }

    #[test]
    fn pending_keyword_hits_flush_when_window_expires() {
        let start = Instant::now();
        let pending = PendingKeywordHits::new(start);

        assert!(!pending.ready_to_flush(start + Duration::from_secs(29), Duration::from_secs(30)));
        assert!(pending.ready_to_flush(start + Duration::from_secs(30), Duration::from_secs(30)));
    }
}
