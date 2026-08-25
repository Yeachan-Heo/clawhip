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
            .map(|(cursor, line)| (Some(cursor), line))
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

    // Empty or whitespace-only keywords match every line at offset 0 and can
    // never advance a scan cursor; config accepts them, so the matcher must
    // filter them out itself (issue #342 review blocker 1).
    let normalized_keywords = keywords
        .iter()
        .filter(|keyword| !keyword.trim().is_empty())
        .map(|keyword| (keyword.clone(), keyword.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    if normalized_keywords.is_empty() {
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut hits = Vec::new();

    let mut previous_line: Option<&str> = None;

    for (line_cursor, line) in lines {
        // Cursor 0 is the overlap-boundary context line (already-seen output
        // prepended for wrapped-predecessor classification); it never hits.
        let is_context_line = line_cursor == Some(0);
        if should_ignore_launcher_line(line, keywords) {
            previous_line = Some(line);
            continue;
        }

        let lower_line = line.to_ascii_lowercase();
        for (keyword, lower_keyword) in &normalized_keywords {
            if lower_line.contains(lower_keyword) && !is_context_line {
                if is_negated_default_failure_match(lower_keyword, &lower_line)
                    || is_instruction_or_search_review_marker_prose(lower_keyword, line)
                {
                    continue;
                }

                if keyword_occurrences(&lower_line, lower_keyword)
                    .iter()
                    .all(|&(start, end)| {
                        is_keyword_self_match_occurrence(
                            line,
                            &lower_line,
                            start,
                            end,
                            previous_line,
                        )
                    })
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

        previous_line = Some(line);
    }

    hits
}

/// Byte offsets of every occurrence of `lower_keyword` in `lower_line` (the
/// caller's already-lowercased line). ASCII-only lowering keeps byte
/// positions identical to the original line, so offsets slice both safely.
/// An empty needle matches at every offset without ever advancing the scan
/// cursor, so it is rejected outright rather than looping.
fn keyword_occurrences(lower_line: &str, lower_keyword: &str) -> Vec<(usize, usize)> {
    if lower_keyword.is_empty() {
        return Vec::new();
    }

    let mut occurrences = Vec::new();
    let mut search_start = 0;
    while let Some(relative_start) = lower_line[search_start..].find(lower_keyword) {
        let start = search_start + relative_start;
        let end = start + lower_keyword.len();
        occurrences.push((start, end));
        search_start = end;
    }
    occurrences
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

fn should_ignore_launcher_line(line: &str, keywords: &[String]) -> bool {
    let trimmed = strip_pane_frame_chrome(line);
    LAUNCHER_NOISE_PATTERNS
        .iter()
        .any(|pattern| trimmed.contains(pattern))
        || is_tmux_watch_command_echo(trimmed)
        || is_wrapped_monitor_command_fragment(trimmed, keywords)
}

/// Trim tmux/GUI frame chrome (box-drawing rails, soft-wrap markers) plus
/// surrounding whitespace so a wrapped pane line can be inspected as the bare
/// text it carries.
fn strip_pane_frame_chrome(line: &str) -> &str {
    line.trim_matches(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                '│' | '┃' | '║' | '|' | '╎' | '▏' | '▕' | '┆' | '┊' | '·' | '┄' | '┈'
            )
    })
}

fn is_tmux_watch_command_echo(line: &str) -> bool {
    let command = strip_shell_prompt(line);
    (command.starts_with("clawhip tmux watch ")
        || command.starts_with("clawhip tmux new ")
        || command.starts_with("clawhip tmux cli-new "))
        && (command.contains(" --session ")
            || command.contains(" -s ")
            || command.contains("session="))
        && (command.contains(" --keywords ") || command.contains("keywords="))
}

fn strip_shell_prompt(line: &str) -> &str {
    line.strip_prefix("$ ")
        .or_else(|| line.strip_prefix("% "))
        .or_else(|| line.strip_prefix("> "))
        .unwrap_or(line)
}

/// Flags a `clawhip tmux watch`/`new` wrapper can emit. Wrapped continuation
/// lines of the monitor's own command start with one of these instead of the
/// program name.
const MONITOR_ARGV_FLAGS: &[&str] = &[
    "--keywords",
    "--keyword",
    "--session",
    "--stale-minutes",
    "--format",
    "--channel",
    "--mention",
    "--attach",
    "--follow",
    "--retry-enter",
    "--retry-enter-count",
    "--retry-enter-delay-ms",
    "--window-name",
    "--cwd",
    "-s",
];

/// Continuation fragment of a self-generated monitor command that was
/// hard-wrapped by a narrow pane, e.g.
/// `│ --stale-minutes 60 --format compact --keywords owner-endpoint-unreachable  │`.
/// The head line (`clawhip tmux watch ...`) is caught by
/// `is_tmux_watch_command_echo`; the tail lines start mid-argv with one of the
/// wrapper's own flags. A line only qualifies when it is pure argv AND one of
/// the monitored keywords is carried as a keyword-flag value, so a runtime
/// failure that merely starts with a flag token (`--format compact: <kw>`)
/// still alerts.
fn is_wrapped_monitor_command_fragment(line: &str, keywords: &[String]) -> bool {
    let fragment = strip_shell_prompt(line);
    let first_token = fragment.split_whitespace().next().unwrap_or("");
    if !MONITOR_ARGV_FLAGS.contains(&first_token) {
        return false;
    }
    if fragment.split_whitespace().count() < 2 {
        return false;
    }
    if !fragment.chars().all(is_monitor_argv_char) {
        return false;
    }
    let lower_fragment = fragment.to_ascii_lowercase();
    keywords
        .iter()
        .any(|keyword| is_keyword_flag_value(&lower_fragment, &keyword.to_ascii_lowercase()))
}

/// True when `lower_keyword` occurs in `lower_fragment` directly as the value
/// of a keyword flag (`--keywords <kw>[,…]`, `keywords=<kw>[,…]`).
fn is_keyword_flag_value(lower_fragment: &str, lower_keyword: &str) -> bool {
    let mut search_start = 0;
    while let Some(relative_start) = lower_fragment[search_start..].find(lower_keyword) {
        let start = search_start + relative_start;
        let before = &lower_fragment[..start];
        if before.ends_with("--keywords ")
            || before.ends_with("--keyword ")
            || before.ends_with("keywords=")
            || before.ends_with("keyword=")
        {
            return true;
        }
        search_start = start + lower_keyword.len();
    }
    false
}

fn is_monitor_argv_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
        || matches!(
            ch,
            '_' | '@' | '#' | '<' | '>' | '/' | '=' | '+' | '.' | ',' | ':' | '-' | ' '
        )
}

/// Prose cues that introduce a keyword as a mentioned example rather than a
/// runtime event. Each phrase was taken from an observed false-positive line;
/// the bounded list mirrors the existing review-marker prose filter.
const KEYWORD_NARRATION_CUES: &[&str] = &["such as", "newly emitted", "evidence prose"];

/// Decides whether one keyword occurrence in a fresh pane line is a
/// self-match: text about the monitor or about the keyword itself, not a
/// runtime event. All rules are structural (flag positions, quote spans,
/// summary dashes, narration cues); none suppress a bare `keyword: message`
/// runtime line.
fn is_keyword_self_match_occurrence(
    line: &str,
    lower_line: &str,
    occurrence_start: usize,
    occurrence_end: usize,
    previous_line: Option<&str>,
) -> bool {
    // Monitor flag value on the same line: `--keywords <kw>`, `keywords=<kw>`,
    // `--keyword <kw>`, `keyword=<kw>` (the occurrence may be one element of a
    // comma-separated value).
    let value_start = line[..occurrence_start]
        .rfind(|ch: char| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | ',' | '.' | '='))
        })
        .map(|index| index + 1)
        .unwrap_or(0);
    let before_value = &line[..value_start];
    if before_value.ends_with("--keywords ")
        || before_value.ends_with("--keyword ")
        || before_value.ends_with("keywords=")
        || before_value.ends_with("keyword=")
    {
        return true;
    }

    // Quoted mention, only in proven prose/narration context: the occurrence
    // is wrapped in backticks or matched quotes AND the surrounding line is
    // prose about the keyword (a narration cue or the live-watch summary
    // vocabulary), not a structured runtime field. Bare JSON/logfmt values
    // like `{"error":"<kw>"}` stay alertable (issue #342 review blocker 2).
    let before = line[..occurrence_start].chars().next_back();
    let after = line[occurrence_end..].chars().next();
    if let (Some(open), Some(close)) = (before, after)
        && matches!(open, '`' | '\'' | '"')
        && open == close
        && (is_prose_about_keyword(line, lower_line, occurrence_start)
            || previous_line_is_narration_context(previous_line))
    {
        return true;
    }

    // Monitor status summary: `… <kw> — N live watch …` (em-dash directly
    // after the keyword plus the live-watch vocabulary on the same line).
    // `lower_line` comes from the caller, avoiding a per-occurrence
    // allocation in the hot path.
    let rest = line[occurrence_end..].trim_start_matches(' ');
    if rest.starts_with('—') && contains_bounded(lower_line, "live watch") {
        return true;
    }

    // Narration prose on the same line, before the occurrence.
    let lower_before = line[..occurrence_start].to_ascii_lowercase();
    if KEYWORD_NARRATION_CUES
        .iter()
        .any(|cue| lower_before.contains(cue))
    {
        return true;
    }

    // Wrapped prose continuation: the occurrence starts the line and the
    // previous pane line ends with a narration cue, so this line is the
    // quoted example that cue introduced (`… preserve real lines such as`
    // / `owner-endpoint-unreachable: runtime owner failed;`).
    if let Some(previous) = previous_line {
        let line_starts_with_occurrence = line[..occurrence_start]
            .chars()
            .all(|ch| ch.is_whitespace() || matches!(ch, '│' | '┃' | '║' | ' '));
        if line_starts_with_occurrence {
            let dechromed_previous = strip_pane_frame_chrome(previous);
            let lower_previous = dechromed_previous.to_ascii_lowercase();
            let lower_previous = lower_previous.trim_end_matches(|ch: char| {
                ch.is_whitespace() || matches!(ch, '.' | ',' | ';' | ':')
            });
            if KEYWORD_NARRATION_CUES
                .iter()
                .any(|cue| lower_previous.ends_with(cue))
            {
                return true;
            }

            // Wrapped `--keywords <value>` continuation: the previous line
            // ends with the `--keywords`/`--keyword` flag itself and this
            // line begins with that flag's value. Verified monitor argv is
            // sufficient; otherwise (an application's own `logged option
            // --keywords` log) the continuation is accepted only when this
            // line is NOT failure-shaped — the keyword must not be followed
            // by `: message`, so a genuine runtime failure still alerts
            // (issue #342 review blocker 3).
            let previous_tokens = dechromed_previous.split_whitespace().collect::<Vec<_>>();
            let previous_is_monitor_argv = previous_tokens
                .first()
                .is_some_and(|token| MONITOR_ARGV_FLAGS.contains(token))
                && dechromed_previous.chars().all(is_monitor_argv_char);
            let line_is_failure_shaped = line[occurrence_end..]
                .chars()
                .next()
                .is_some_and(|ch| ch == ':');
            if (lower_previous.ends_with("--keywords") || lower_previous.ends_with("--keyword"))
                && (previous_is_monitor_argv || !line_is_failure_shaped)
            {
                return true;
            }
        }
    }

    false
}

/// True when the wrapped-predecessor line is narration prose that introduces
/// a mentioned keyword (ends with a narration cue), so a quoted occurrence at
/// the start of the next line is a mention continuation, not a runtime event.
fn previous_line_is_narration_context(previous_line: Option<&str>) -> bool {
    previous_line.is_some_and(|previous| {
        let lower_previous = strip_pane_frame_chrome(previous)
            .to_ascii_lowercase()
            .trim_end_matches(|ch: char| ch.is_whitespace() || matches!(ch, '.' | ',' | ';' | ':'))
            .to_string();
        KEYWORD_NARRATION_CUES
            .iter()
            .any(|cue| lower_previous.ends_with(cue))
    })
}

/// True when the line around a quoted keyword occurrence is prose about the
/// keyword or the monitor (narration cues, live-watch summary vocabulary, or
/// quoting/backticking a command example), as opposed to structured runtime
/// output whose quoting is field syntax. Used to keep quote-suppression from
/// eating valid JSON/logfmt failures.
fn is_prose_about_keyword(line: &str, lower_line: &str, occurrence_start: usize) -> bool {
    let lower_before = line[..occurrence_start].to_ascii_lowercase();
    KEYWORD_NARRATION_CUES
        .iter()
        .any(|cue| lower_before.contains(cue))
        || contains_bounded(lower_line, "live watch")
        || lower_before.contains("example")
        || lower_before.contains("e.g.")
        || lower_before.contains("quoted")
        || lower_before.contains("mention")
}

/// Fresh (appended) lines of `current` relative to the `previous` snapshot,
/// with 1-based cursor positions into `current`. The line immediately before
/// the first appended line — even when it is an *overlapped* line carried
/// over from the previous snapshot — is prepended as line 0 so per-occurrence
/// classification can see its wrapped-predecessor context across the
/// snapshot-overlap boundary (issue #342 review blocker 4). Line 0 never
/// produces a hit: it is already-seen output by construction.
fn appended_lines_with_cursors<'a>(previous: &'a str, current: &'a str) -> Vec<(usize, &'a str)> {
    let previous_lines = previous.lines().collect::<Vec<_>>();
    let current_lines = current.lines().collect::<Vec<_>>();
    let overlap = overlapping_suffix_prefix_len(&previous_lines, &current_lines);

    let mut lines = Vec::with_capacity(current_lines.len().saturating_sub(overlap) + 1);
    if overlap > 0 && overlap < current_lines.len() {
        lines.push((0, current_lines[overlap - 1]));
    }
    lines.extend(
        current_lines
            .into_iter()
            .enumerate()
            .skip(overlap)
            .map(|(index, line)| (index + 1, line)),
    );
    lines
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
    fn collect_keyword_hits_ignores_detached_watch_command_but_keeps_owner_failure() {
        let hits = collect_keyword_hits_with_provenance(
            "boot",
            "boot
$ clawhip tmux watch --session clawhip-issue-299 --stale-minutes 60 --keywords owner-endpoint-unreachable
owner-endpoint-unreachable: runtime owner failed
good output",
            &["owner-endpoint-unreachable".into()],
            KeywordMatchProvenance {
                pane_id: "%29".into(),
                pane_name: "0.0".into(),
                cursor: None,
                source: KeywordMatchSource::FreshOutput,
            },
        );

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].keyword, "owner-endpoint-unreachable");
        assert_eq!(
            hits[0].line,
            "owner-endpoint-unreachable: runtime owner failed"
        );
        assert_eq!(
            hits[0].provenance.as_ref().and_then(|value| value.cursor),
            Some(3)
        );
    }
    /// Issue #342: exact observed false-positive variants from live pane
    /// captures (%460 fresh-output cursors 205/179 and %466's 8 self-matches)
    /// must produce zero alerts, while a genuinely new runtime failure line
    /// after baseline still alerts with full provenance.
    #[test]
    fn collect_keyword_hits_suppresses_observed_wrapped_monitor_self_matches() {
        // Exact observed wrapped continuation fragment at pane width 80
        // (cursor 205 / 179 class), including the trailing frame rail.
        let observed_variants = [
            // (a) wrapped continuation of the monitor's own command, framed
            "│ --stale-minutes 60 --format compact --keywords owner-endpoint-unreachable  │",
            // (b) same fragment as the second wrap segment
            "owner-endpoint-unreachable │ at pane %460/0.0, fresh-output cursor 205 and",
            // (c) full command wrapped across lines (all three segments)
            "clawhip tmux watch --session project-pr-4935-auth-gateway-scope",
            "--keywords owner-endpoint-unreachable --channel 1508831529856663612",
            // (d) summary/echo prose (cursor 105 class)
            "watch --session ... --keywords owner-endpoint-unreachable — 4 live watch",
            "owner-endpoint-unreachable — 4 live watch at cursor 105.",
            // (e) bare quoted/mentioned keyword in evidence prose
            "- bare quoted/mentioned keyword in evidence prose: owner-endpoint-unreachable;",
            "`owner-endpoint-unreachable`",
            // (f) command prose
            "- command prose: watch ... --keywords owner-endpoint-unreachable, but wrapped;",
            // (g) requirement bullet quoting the expected real line
            "owner-endpoint-unreachable: runtime owner failed;",
            // (h) diagnostic narration quoting the flag
            "--keywords owner-endpoint-unreachable │ bypass filtering.",
            "- diagnostic explanation prose: --keywords owner-endpoint-unreachable │ bypass",
        ];

        for variant in observed_variants {
            // `boot` anchors both the cue and the variant inside one fresh
            // window; without it the overlap logic treats the cue line as
            // already-seen scrollback and drops the pairing context.
            let cue = match variant {
                // (b) is the second wrap segment of the framed fragment (a)
                "owner-endpoint-unreachable │ at pane %460/0.0, fresh-output cursor 205 and" => {
                    Some("│ --stale-minutes 60 --format compact --keywords")
                }
                // (g) is the wrapped continuation of the requirement bullet
                // that names it an example
                "owner-endpoint-unreachable: runtime owner failed;" => {
                    Some("- requirement prose/bullet: preserve real lines such as")
                }
                // (e) bare quoted continuation lands alone after the cue
                "`owner-endpoint-unreachable`" => {
                    Some("- bare quoted/mentioned keyword in evidence prose:")
                }
                // (h) second line is the wrapped continuation of the
                // diagnostic narration line above it; already covered by the
                // full line variant.
                "- diagnostic explanation prose: --keywords owner-endpoint-unreachable │ bypass" =>
                {
                    continue;
                }
                _ => None,
            };
            let previous = "boot";
            let current = match cue {
                Some(cue) => format!("{previous}\n{cue}\n{variant}"),
                None => format!("{previous}\n{variant}"),
            };
            let hits =
                collect_keyword_hits(previous, &current, &["owner-endpoint-unreachable".into()]);
            assert!(
                hits.is_empty(),
                "expected no self-match alert for {variant:?}, got {hits:?}"
            );
        }
    }

    #[test]
    fn collect_keyword_hits_suppresses_full_observed_prompt_window_but_keeps_new_failure() {
        // The exact observed fresh window that produced the 8 self-matches
        // (pane %466, user prompt text), followed by a genuine runtime
        // failure emitted after the prompt.
        let baseline = "prior work";
        let current = "prior work
 1. False alert line: │ --stale-minutes 60 --format compact --keywords
 owner-endpoint-unreachable │ at pane %460/0.0, fresh-output cursor 205 and
 again cursor 179.
 2. Additional false summary/echo line: watch --session ... --keywords
 owner-endpoint-unreachable — 4 live watch at cursor 105.
 - bare quoted/mentioned keyword in evidence prose: owner-endpoint-unreachable;
 - summary/echo: owner-endpoint-unreachable — 4 live watch;
 - command prose: watch ... --keywords owner-endpoint-unreachable, but wrapped;
 - requirement prose/bullet: preserve real lines such as
 owner-endpoint-unreachable: runtime owner failed;
 - diagnostic explanation prose: --keywords owner-endpoint-unreachable │ bypass
 filtering.
 2. a genuinely newly emitted owner-endpoint-unreachable: runtime owner failed
 after baseline still produces one alert with correct provenance.
owner-endpoint-unreachable: runtime owner failed";

        let hits = collect_keyword_hits_with_provenance(
            baseline,
            current,
            &["owner-endpoint-unreachable".into()],
            KeywordMatchProvenance {
                pane_id: "%466".into(),
                pane_name: "0.0".into(),
                cursor: None,
                source: KeywordMatchSource::FreshOutput,
            },
        );

        assert_eq!(hits.len(), 1, "got {hits:?}");
        assert_eq!(
            hits[0].line,
            "owner-endpoint-unreachable: runtime owner failed"
        );
        assert_eq!(hits[0].provenance.as_ref().unwrap().pane_id, "%466");
        assert_eq!(hits[0].provenance.as_ref().unwrap().cursor, Some(16));
        assert_eq!(
            hits[0].provenance.as_ref().unwrap().source,
            KeywordMatchSource::FreshOutput
        );
    }

    #[test]
    fn collect_keyword_hits_suppresses_wrapped_command_keyword_value_on_own_line() {
        // Narrow pane wraps the flag and its value onto separate lines.
        let previous = "boot";
        let current = "boot
$ clawhip tmux watch --session x --stale-minutes 60 --format compact
 --keywords
 owner-endpoint-unreachable --channel 1508831529856663612 --mention <@1>
owner-endpoint-unreachable: runtime owner failed";

        let hits = collect_keyword_hits(previous, current, &["owner-endpoint-unreachable".into()]);

        assert_eq!(
            hits,
            vec![KeywordHit {
                keyword: "owner-endpoint-unreachable".into(),
                line: "owner-endpoint-unreachable: runtime owner failed".into(),
                provenance: None,
            }]
        );
    }

    #[test]
    fn collect_keyword_hits_keeps_mid_line_application_occurrence_without_narration() {
        // A genuine application line mentioning the keyword mid-line with no
        // monitor/narration context must still alert.
        let hits = collect_keyword_hits(
            "boot",
            "boot\nretrying after owner-endpoint-unreachable (attempt 3/5)",
            &["owner-endpoint-unreachable".into()],
        );

        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].line,
            "retrying after owner-endpoint-unreachable (attempt 3/5)"
        );
    }

    #[test]
    fn collect_keyword_hits_keeps_bare_quoted_keyword_standalone_marker() {
        // Quoted-mention suppression must not eat a real marker-style keyword
        // line that a runtime prints verbatim (no quotes, no cues).
        let hits = collect_keyword_hits(
            "boot",
            "boot\nowner-endpoint-unreachable",
            &["owner-endpoint-unreachable".into()],
        );

        assert_eq!(hits.len(), 1);
    }

    /// Review blocker 1: empty/whitespace keywords are accepted by config;
    /// the matcher must filter them instead of spinning on `find("")` at a
    /// never-advancing cursor.
    #[test]
    fn collect_keyword_hits_ignores_empty_and_whitespace_keywords() {
        let current = "boot\nowner-endpoint-unreachable: runtime owner failed";

        // Empty keyword alone: no hits, and crucially it terminates.
        assert!(
            collect_keyword_hits("boot", current, &["".into()]).is_empty(),
            "empty keyword must be filtered before matching"
        );
        // Whitespace-only keyword: same.
        assert!(
            collect_keyword_hits("boot", current, &["   ".into(), "\t".into()]).is_empty(),
            "whitespace-only keywords must be filtered before matching"
        );
        // Mixed list: the empty entries must not disable the real keyword.
        let hits = collect_keyword_hits_with_provenance(
            "boot",
            current,
            &["".into(), "owner-endpoint-unreachable".into(), " ".into()],
            KeywordMatchProvenance {
                pane_id: "%1".into(),
                pane_name: "0.0".into(),
                cursor: None,
                source: KeywordMatchSource::FreshOutput,
            },
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].line,
            "owner-endpoint-unreachable: runtime owner failed"
        );
    }

    /// Review blocker 2: quoted structured runtime output (JSON/logfmt) is a
    /// valid failure surface; quote suppression applies only to prose about
    /// the keyword.
    #[test]
    fn collect_keyword_hits_keeps_quoted_structured_runtime_fields() {
        let must_alert = [
            r#"{"error":"owner-endpoint-unreachable","message":"runtime owner failed"}"#,
            r#"level=error msg="owner-endpoint-unreachable: runtime owner failed""#,
            r#"{"event":"failure","detail":"owner-endpoint-unreachable"}"#,
            // logfmt with the keyword as a bare value
            r#"state=owner-endpoint-unreachable retries=3"#,
            // quoted keyword at field position with surrounding JSON braces
            r#"{"keyword":"owner-endpoint-unreachable"}"#,
        ];
        for line in must_alert {
            let hits = collect_keyword_hits(
                "boot",
                &format!("boot\n{line}"),
                &["owner-endpoint-unreachable".into()],
            );
            assert_eq!(
                hits.len(),
                1,
                "structured runtime line was suppressed: {line:?}"
            );
            assert_eq!(hits[0].line, line);
        }

        // Prose quoting still suppressed: backticked example in narration.
        let suppressed = collect_keyword_hits(
            "boot",
            "boot\nsee the example `owner-endpoint-unreachable` in the docs",
            &["owner-endpoint-unreachable".into()],
        );
        assert!(suppressed.is_empty(), "got {suppressed:?}");
    }

    /// Review blocker 3: a prior line merely ending in `--keywords` (an
    /// application's own option log) must not suppress the next line's
    /// genuine failure.
    #[test]
    fn collect_keyword_hits_prior_keywords_cue_does_not_leak_into_failures() {
        let previous = "boot";
        let current = "boot
application logged option --keywords
owner-endpoint-unreachable: runtime owner failed";

        let hits = collect_keyword_hits(previous, current, &["owner-endpoint-unreachable".into()]);

        assert_eq!(hits.len(), 1, "got {hits:?}");
        assert_eq!(
            hits[0].line,
            "owner-endpoint-unreachable: runtime owner failed"
        );

        // The same prose predecessor still suppresses a non-failure-shaped
        // flag-value continuation (wrapped echo fragment).
        let wrapped_echo = collect_keyword_hits(
            "boot",
            "boot
application logged option --keywords
owner-endpoint-unreachable --channel 1508831529856663612",
            &["owner-endpoint-unreachable".into()],
        );
        assert!(
            wrapped_echo.is_empty(),
            "non-failure continuation should stay suppressed, got {wrapped_echo:?}"
        );

        // A verified monitor argv predecessor ending in `--keywords` keeps
        // suppressing even a colon-shaped continuation, because it is the
        // monitor's own wrapped echo.
        let monitor_argv_echo = collect_keyword_hits(
            "boot",
            "boot
--stale-minutes 60 --format compact --keywords
owner-endpoint-unreachable --channel 1508831529856663612",
            &["owner-endpoint-unreachable".into()],
        );
        assert!(
            monitor_argv_echo.is_empty(),
            "verified monitor argv continuation should stay suppressed, got {monitor_argv_echo:?}"
        );
    }

    /// Review blocker 4: the snapshot-overlap boundary must not discard the
    /// narration predecessor of the first appended line.
    #[test]
    fn collect_keyword_hits_overlap_boundary_keeps_narration_context() {
        // previous ends with the cue; current scrolls it up but repeats it as
        // the overlapped predecessor of the appended example line.
        let previous = "boot\nsuch as";
        let current = "such as\nowner-endpoint-unreachable: runtime owner failed";

        let hits = collect_keyword_hits(previous, current, &["owner-endpoint-unreachable".into()]);
        assert!(
            hits.is_empty(),
            "overlap boundary lost narration context, got {hits:?}"
        );

        // The overlap context line itself must never produce a hit even when
        // it contains the keyword (it is already-seen output).
        let previous_kw = "boot\nsuch as owner-endpoint-unreachable";
        let current_kw = "such as owner-endpoint-unreachable\nstill running";
        assert!(
            collect_keyword_hits(
                previous_kw,
                current_kw,
                &["owner-endpoint-unreachable".into()]
            )
            .is_empty()
        );

        // Counter-case: overlap context must NOT suppress a genuine failure
        // that merely follows an overlapped unrelated line.
        let previous_plain = "boot\nplain overlap line";
        let current_plain = "plain overlap line\nowner-endpoint-unreachable: runtime owner failed";
        let kept = collect_keyword_hits(
            previous_plain,
            current_plain,
            &["owner-endpoint-unreachable".into()],
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].line,
            "owner-endpoint-unreachable: runtime owner failed"
        );
    }

    /// Unicode/non-ASCII safety: occurrence offsets are computed on an
    /// ASCII-lowercased copy and sliced into the original line; non-ASCII
    /// before/after the keyword must neither panic nor mis-slice, and
    /// multi-occurrence lines must classify each occurrence independently.
    #[test]
    fn collect_keyword_hits_handles_non_ascii_and_multiple_occurrences() {
        let must_alert = [
            "日本語 owner-endpoint-unreachable: failed",
            "emoji 🦞 owner-endpoint-unreachable: failed",
            "café — owner-endpoint-unreachable",
            "owner-endpoint-unreachable: 中文失败",
        ];
        for line in must_alert {
            let hits = collect_keyword_hits(
                "boot",
                &format!("boot\n{line}"),
                &["owner-endpoint-unreachable".into()],
            );
            assert_eq!(
                hits.len(),
                1,
                "non-ASCII line misclassified: {line:?} -> {hits:?}"
            );
            assert_eq!(hits[0].line, line);
        }

        // One flag-value occurrence + one genuine failure on the same line:
        // the `.all()` gate must NOT suppress the line, because the failure
        // occurrence is not a self-match.
        let mixed = "watch --keywords owner-endpoint-unreachable; owner-endpoint-unreachable: runtime owner failed";
        let mixed_hits = collect_keyword_hits(
            "boot",
            &format!("boot\n{mixed}"),
            &["owner-endpoint-unreachable".into()],
        );
        assert_eq!(mixed_hits.len(), 1, "got {mixed_hits:?}");
        assert_eq!(mixed_hits[0].line, mixed);

        // All occurrences are flag values (comma-separated list): suppressed.
        let list = "--keywords owner-endpoint-unreachable,owner-endpoint-unreachable";
        assert!(
            collect_keyword_hits(
                "boot",
                &format!("boot\n{list}"),
                &["owner-endpoint-unreachable".into()]
            )
            .is_empty()
        );
    }
    /// Adversarial boundary cases: each line shares a token with a suppressed
    /// class but is a genuine runtime event, so it MUST still alert. These pin
    /// the suppression rules against broad-suppression drift.
    #[test]
    fn collect_keyword_hits_keeps_near_positive_runtime_lines() {
        let must_alert = [
            // `error` is a flag-ish token but this is a real failure message.
            "owner-endpoint-unreachable: runtime owner failed",
            // Colon-summary shape an app may emit; not an em-dash summary.
            "owner-endpoint-unreachable: retries exhausted",
            // Mid-line mention with no narration cue.
            "dial failed: owner-endpoint-unreachable after 3 attempts",
            // Em-dash present but no `live watch` vocabulary — not a summary.
            "owner-endpoint-unreachable — connection dropped",
            // Mention of the monitor command inside a genuine failure log.
            "FATAL: owner-endpoint-unreachable while starting watch",
            // Failure line that itself contains the word `keyword`.
            "keyword watch failed: owner-endpoint-unreachable",
            // Runtime echo of an unrelated flag before the failure token.
            "--format compact: owner-endpoint-unreachable",
            // Uppercase failure line (case-insensitive matching must hold).
            "OWNER-ENDPOINT-UNREACHABLE: runtime owner failed",
            // Keyword preceded by an unmatched quote pair (narration rule
            // requires matched quotes directly around the occurrence).
            "error 'unterminated: owner-endpoint-unreachable at 04:50",
        ];

        for line in must_alert {
            let hits = collect_keyword_hits(
                "boot",
                &format!("boot\n{line}"),
                &["owner-endpoint-unreachable".into()],
            );
            assert_eq!(
                hits.len(),
                1,
                "genuine runtime line was suppressed: {line:?}"
            );
            assert_eq!(hits[0].line, line);
        }
    }

    #[test]
    fn collect_keyword_hits_suppresses_flag_value_only_when_directly_flagged() {
        // The occurrence must be the flag's value, not merely later in a line
        // that mentions the flag elsewhere.
        let suppressed = collect_keyword_hits(
            "boot",
            "boot\nclawhip tmux watch --keywords owner-endpoint-unreachable --session s",
            &["owner-endpoint-unreachable".into()],
        );
        assert!(suppressed.is_empty());

        let kept = collect_keyword_hits(
            "boot",
            "boot\n--keywords ignored-value; failure owner-endpoint-unreachable",
            &["owner-endpoint-unreachable".into()],
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].line,
            "--keywords ignored-value; failure owner-endpoint-unreachable"
        );
    }

    #[test]
    fn collect_keyword_hits_wrapped_prose_continuation_requires_cue_immediately_before() {
        // A keyword-initial line after an unrelated line still alerts; only a
        // previous line ending in a narration cue (or the --keywords flag) is
        // treated as a wrapped continuation.
        let kept = collect_keyword_hits(
            "boot",
            "boot\nsome unrelated prior line\nowner-endpoint-unreachable: runtime owner failed",
            &["owner-endpoint-unreachable".into()],
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].line,
            "owner-endpoint-unreachable: runtime owner failed"
        );
    }

    #[test]
    fn collect_keyword_hits_ignores_short_form_detached_watch_command() {
        let hits = collect_keyword_hits(
            "boot",
            "boot
clawhip tmux watch -s clawhip-issue-299 --keywords owner-endpoint-unreachable
owner-endpoint-unreachable: runtime owner failed",
            &["owner-endpoint-unreachable".into()],
        );

        assert_eq!(
            hits,
            vec![KeywordHit {
                keyword: "owner-endpoint-unreachable".into(),
                line: "owner-endpoint-unreachable: runtime owner failed".into(),
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
