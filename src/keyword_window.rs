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
    // filter them out itself (issue #342 review blocker 1). The filtered list
    // feeds every consumer, including the line-level argv filter, so no
    // suppression path ever sees a raw empty keyword.
    let normalized_keywords = keywords
        .iter()
        .filter(|keyword| !keyword.trim().is_empty())
        .map(|keyword| (keyword.clone(), keyword.to_ascii_lowercase()))
        .collect::<Vec<_>>();
    if normalized_keywords.is_empty() {
        return Vec::new();
    }
    let match_keywords = normalized_keywords
        .iter()
        .map(|(keyword, _)| keyword.as_str())
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    let mut hits = Vec::new();

    let mut previous_line: Option<&str> = None;
    let mut monitor_argv = MonitorArgvState::default();

    for (line_cursor, line) in lines {
        // Cursor 0 is the overlap-boundary context line (already-seen output
        // prepended for wrapped-predecessor classification); it never hits.
        let is_context_line = line_cursor == Some(0);
        monitor_argv = advance_monitor_argv(monitor_argv, line);
        let line_monitor_argv = monitor_argv;
        if should_ignore_launcher_line(line, &match_keywords) {
            previous_line = Some(line);
            continue;
        }

        let lower_line = line.to_ascii_lowercase();
        for (keyword, lower_keyword) in &normalized_keywords {
            if lower_line.contains(lower_keyword) && !is_context_line {
                if !line_is_structured_runtime(line)
                    && (is_negated_default_failure_match(lower_keyword, &lower_line)
                        || is_instruction_or_search_review_marker_prose(lower_keyword, line))
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
                            line_monitor_argv,
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

fn should_ignore_launcher_line(line: &str, keywords: &[&str]) -> bool {
    let trimmed = strip_pane_frame_chrome(line);
    if line_is_structured_runtime(line) {
        return false;
    }
    LAUNCHER_NOISE_PATTERNS
        .iter()
        .any(|pattern| trimmed.contains(pattern))
        || is_monitor_argv_line(trimmed, keywords)
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

fn strip_shell_prompt(line: &str) -> &str {
    line.strip_prefix("$ ")
        .or_else(|| line.strip_prefix("% "))
        .or_else(|| line.strip_prefix("> "))
        .unwrap_or(line)
}

const MONITOR_FLAGS: &[(&str, bool)] = &[
    ("--session", true),
    ("-s", true),
    ("--keywords", true),
    ("--keyword", true),
    ("--line", true),
    ("--channel", true),
    ("--mention", true),
    ("--stale-minutes", true),
    ("--format", true),
    ("--window-name", true),
    ("--cwd", true),
    ("--shell", true),
    ("--thread", true),
    ("--kickoff", true),
    ("--retry-enter-count", true),
    ("--retry-enter-delay-ms", true),
    ("--attach", false),
    ("--follow", false),
    ("--retry-enter", true),
    ("--json", false),
    ("-n", true),
    ("-c", true),
    ("session", true),
    ("keywords", true),
    ("channel", true),
    ("mention", true),
    ("stale_minutes", true),
    ("format", true),
    ("registered_at", true),
    ("parent_pid", true),
    ("parent_name", true),
];

#[derive(Debug, Clone, Copy)]
enum EchoKind {
    Monitor,
    Keyword,
}

#[derive(Debug, Clone, Copy)]
struct MonitorArgvState {
    active: bool,
    complete: bool,
    pending_value: bool,
    pending_flag: Option<&'static str>,
    remaining: u8,
    kind: EchoKind,
    cli_new: bool,
    seen_session: bool,
    seen_keyword: bool,
    seen_line: bool,
    quote: Option<char>,
}

impl Default for MonitorArgvState {
    fn default() -> Self {
        Self {
            active: false,
            complete: false,
            pending_value: false,
            pending_flag: None,
            remaining: 0,
            kind: EchoKind::Monitor,
            cli_new: false,
            seen_session: false,
            seen_keyword: false,
            seen_line: false,
            quote: None,
        }
    }
}

fn monitor_head(line: &str) -> Option<&str> {
    let command = strip_shell_prompt(strip_pane_frame_chrome(line));
    [
        "clawhip tmux watch",
        "clawhip tmux new",
        "clawhip tmux cli-new",
    ]
    .into_iter()
    .find(|head| {
        command == *head
            || command
                .strip_prefix(head)
                .is_some_and(|tail| tail.starts_with(' '))
    })
}

fn keyword_head(line: &str) -> Option<&str> {
    let command = strip_shell_prompt(strip_pane_frame_chrome(line));
    (command == "clawhip tmux keyword" || command.starts_with("clawhip tmux keyword "))
        .then_some("clawhip tmux keyword")
}

fn monitor_flag(token: &str) -> Option<(&str, bool)> {
    if token == "--" {
        return Some(("--", false));
    }
    let name = token.split_once('=').map_or(token, |(name, _)| name);
    MONITOR_FLAGS
        .iter()
        .find(|(flag, _)| *flag == name)
        .copied()
}

fn tokenize_monitor_argv(line: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    for ch in line.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => token.push(c),
            (None, '\'' | '"') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
            }
            (None, c) => token.push(c),
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    (!tokens.is_empty()).then_some(tokens)
}

fn monitor_argv_tokens_valid(tokens: &[String]) -> bool {
    if tokens.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index] == "--" {
            return true;
        }
        if tokens[index] == "start" {
            index += 1;
            continue;
        }
        let Some((_, takes_value)) = monitor_flag(&tokens[index]) else {
            return false;
        };
        if tokens[index].split_once('=').is_some_and(|(_, value)| {
            tokens[index].starts_with("--retry-enter=") && !matches!(value, "true" | "false")
        }) {
            return false;
        }
        if takes_value && !tokens[index].contains('=') {
            let Some(_) = tokens.get(index + 1) else {
                return false;
            };
            if tokens[index].split_once('=').is_none()
                && tokens[index] == "--retry-enter"
                && !matches!(tokens[index + 1].as_str(), "true" | "false")
            {
                return false;
            }
            index += 1;
        }
        index += 1;
    }
    true
}

fn is_monitor_argv_line(line: &str, keywords: &[&str]) -> bool {
    let command = strip_shell_prompt(strip_pane_frame_chrome(line));
    let Some(head) = monitor_head(command).or_else(|| keyword_head(command)) else {
        return false;
    };
    let tail = command[head.len()..].trim_start();
    let Some(tokens) = tokenize_monitor_argv(tail) else {
        return false;
    };
    let parsed = scan_monitor_argv(
        MonitorArgvState {
            active: true,
            kind: if head.ends_with("keyword") {
                EchoKind::Keyword
            } else {
                EchoKind::Monitor
            },
            cli_new: head == "clawhip tmux cli-new",
            ..Default::default()
        },
        tail,
    );
    if !parsed.complete {
        return false;
    }
    let needs_keyword = if head.ends_with("keyword") {
        &["--keyword"][..]
    } else {
        &["--keywords", "keywords"][..]
    };
    tokens
        .iter()
        .enumerate()
        .take_while(|(_, token)| token.as_str() != "--")
        .any(|(index, token)| {
            let name = token
                .split_once('=')
                .map_or(token.as_str(), |(name, _)| name);
            let value = token
                .split_once('=')
                .map(|(_, value)| value)
                .or_else(|| tokens.get(index + 1).map(String::as_str));
            needs_keyword.contains(&name)
                && value.is_some_and(|value| {
                    keywords.iter().any(|keyword| {
                        value
                            .split(',')
                            .any(|part| part.eq_ignore_ascii_case(keyword))
                    })
                })
        })
}

fn advance_monitor_argv(mut state: MonitorArgvState, line: &str) -> MonitorArgvState {
    let command = strip_shell_prompt(strip_pane_frame_chrome(line));
    if let Some(head) = monitor_head(command).or_else(|| keyword_head(command)) {
        let tail = command[head.len()..].trim_start();
        if tail.is_empty() {
            return MonitorArgvState {
                active: true,
                remaining: 4,
                kind: if head.ends_with("keyword") {
                    EchoKind::Keyword
                } else {
                    EchoKind::Monitor
                },
                cli_new: head == "clawhip tmux cli-new",
                ..Default::default()
            };
        }
        return scan_monitor_argv(
            MonitorArgvState {
                active: true,
                remaining: 4,
                kind: if head.ends_with("keyword") {
                    EchoKind::Keyword
                } else {
                    EchoKind::Monitor
                },
                cli_new: head == "clawhip tmux cli-new",
                ..Default::default()
            },
            tail,
        );
    }
    if state.complete {
        state = MonitorArgvState::default();
    }
    if !state.active || state.remaining == 0 {
        return MonitorArgvState::default();
    }
    if let Some(quote) = state.quote.filter(|_| {
        matches!(state.kind, EchoKind::Keyword)
            || (matches!(state.kind, EchoKind::Monitor) && state.pending_flag.is_some())
    }) {
        let mut next = state;
        next.quote = (!command.contains(quote)).then_some(quote);
        if next.quote.is_none() {
            let tail = command
                .find(quote)
                .map(|close| command[close + quote.len_utf8()..].trim_start())
                .unwrap_or("");
            if !tail.is_empty() {
                let mut scanned = scan_monitor_argv(next, tail);
                scanned.remaining = scanned.remaining.saturating_sub(1);
                return scanned;
            }
            let complete = match next.kind {
                EchoKind::Monitor => next.seen_session && next.seen_keyword,
                EchoKind::Keyword => next.seen_session && next.seen_keyword && next.seen_line,
            };
            if complete {
                next.complete = true;
                return next;
            }
        }
        next.remaining = next.remaining.saturating_sub(1);
        return if next.remaining == 0 {
            MonitorArgvState::default()
        } else {
            next
        };
    }
    if state.pending_value {
        let mut next = state;
        let origin_flag = state.pending_flag;
        next.pending_value = false;
        if let Some((_, rest)) = command.split_once(char::is_whitespace) {
            let mut scanned = scan_monitor_argv(next, rest.trim_start());
            // The remainder can discover a newer pending state (e.g. a
            // wrapped `--keywords` flag at end of row). Never clobber that
            // newer discovery; retain the origin flag only where the scan
            // created no newer pending state and it remains semantically
            // needed (live quote span, completed argv, or carried origin)
            // for downstream continuation gating (issue #345).
            if scanned.pending_flag.is_none()
                && !scanned.pending_value
                && (scanned.quote.is_some() || scanned.complete || origin_flag.is_some())
            {
                scanned.pending_flag = origin_flag;
            }
            scanned.remaining = scanned.remaining.saturating_sub(1);
            return scanned;
        }
        next.complete = match next.kind {
            EchoKind::Monitor => next.seen_session && next.seen_keyword,
            EchoKind::Keyword => next.seen_session && next.seen_keyword && next.seen_line,
        };
        next.pending_flag = origin_flag;
        next.remaining = next.remaining.saturating_sub(1);
        return next;
    }
    let mut next = scan_monitor_argv(state, command);
    next.remaining = next.remaining.saturating_sub(1);
    if next.remaining == 0 {
        MonitorArgvState::default()
    } else {
        next
    }
}

fn scan_monitor_argv(mut state: MonitorArgvState, line: &str) -> MonitorArgvState {
    let Some(tokens) = tokenize_monitor_argv(line) else {
        return MonitorArgvState::default();
    };
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index] == "--" {
            if state.seen_session
                && state.seen_keyword
                && (matches!(state.kind, EchoKind::Monitor) || state.seen_line)
            {
                state.complete = true;
                return state;
            }
            return MonitorArgvState::default();
        }
        if tokens[index] == "start" && matches!(state.kind, EchoKind::Monitor) && state.cli_new {
            index += 1;
            continue;
        }
        let Some((_, takes_value)) = monitor_flag(&tokens[index]) else {
            return MonitorArgvState::default();
        };
        let name = tokens[index]
            .split_once('=')
            .map_or(tokens[index].as_str(), |(name, _)| name);
        if let Some((_, value)) = tokens[index].split_once('=')
            && (value.is_empty() || (name == "--retry-enter" && !matches!(value, "true" | "false")))
        {
            return MonitorArgvState::default();
        }
        state.seen_session |= matches!(name, "--session" | "-s" | "session");
        state.seen_keyword |= matches!(name, "--keywords" | "--keyword" | "keywords" | "keyword");
        state.seen_line |= matches!(name, "--line" | "line");
        state.quote = unclosed_quote(line);
        if state.quote.is_some() {
            state.pending_flag = match name {
                "--keywords" | "keywords" => Some("--keywords"),
                "--keyword" | "keyword" => Some("--keyword"),
                "--line" | "line" => Some("--line"),
                _ if takes_value => Some("--quoted-value"),
                _ => state.pending_flag,
            };
        }
        if takes_value && !tokens[index].contains('=') {
            if index + 1 == tokens.len() {
                state.pending_value = true;
                state.pending_flag = match name {
                    "--keyword" | "keyword" => Some("--keyword"),
                    "--keywords" | "keywords" => Some("--keywords"),
                    "--line" | "line" => Some("--line"),
                    _ => None,
                };
                return state;
            }
            if name == "--retry-enter" && !matches!(tokens[index + 1].as_str(), "true" | "false") {
                return MonitorArgvState::default();
            }
            index += 1;
        }
        state.pending_value = false;
        if state.quote.is_none() {
            state.pending_flag = None;
        }
        index += 1;
    }
    state.pending_value = false;
    let complete = match state.kind {
        EchoKind::Monitor => state.seen_session && state.seen_keyword,
        EchoKind::Keyword => state.seen_session && state.seen_keyword && state.seen_line,
    };
    if complete && state.quote.is_none() {
        state.complete = true;
        state
    } else {
        state
    }
}

fn unclosed_quote(line: &str) -> Option<char> {
    let mut quote = None;
    for ch in line.chars() {
        match (quote, ch) {
            (Some(q), c) if q == c => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            _ => {}
        }
    }
    quote
}

/// Prose cues that introduce a keyword as a mentioned example rather than a
/// runtime event. The cue must be a structural label, not a substring in a
/// machine-readable line.
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
    monitor_argv: MonitorArgvState,
) -> bool {
    if line_is_structured_runtime(line) {
        return false;
    }
    let occurrence = &line[occurrence_start..occurrence_end];
    let structural_prose = strip_pane_frame_chrome(line).to_ascii_lowercase();
    if structural_prose.contains("at pane") && structural_prose.contains("fresh-output cursor")
        || structural_prose
            .trim_start()
            .starts_with("- command prose:")
        || structural_prose
            .trim_start()
            .starts_with("- diagnostic explanation prose:")
        || structural_prose
            .trim_start()
            .starts_with("2. a genuinely newly emitted")
    {
        return true;
    }
    let keyword_echo_matches = !matches!(monitor_argv.kind, EchoKind::Keyword)
        || keyword_echo_mentions(line, previous_line, occurrence);
    if monitor_argv.active {
        if matches!(monitor_argv.kind, EchoKind::Keyword)
            && monitor_argv.seen_session
            && monitor_argv.seen_keyword
            && monitor_argv.seen_line
            && monitor_line_contains_keyword_value(line, occurrence)
        {
            return true;
        }
        if (monitor_argv.quote.is_some() || monitor_argv.complete)
            && keyword_echo_matches
            && (matches!(monitor_argv.kind, EchoKind::Keyword)
                || (matches!(monitor_argv.kind, EchoKind::Monitor)
                    && monitor_argv.pending_flag == Some("--keywords")))
        {
            let dechromed = strip_shell_prompt(strip_pane_frame_chrome(line));
            if matches!(monitor_argv.kind, EchoKind::Monitor)
                && monitor_argv.pending_flag == Some("--quoted-value")
            {
                return true;
            }
            let frame_offset = line.find(dechromed).unwrap_or(0);
            let relative_occurrence = occurrence_start.saturating_sub(frame_offset);
            let starts_dash_payload =
                dechromed.starts_with('-') && matches!(relative_occurrence, 1 | 2);
            let runtime_shaped = line[occurrence_end..].starts_with(": ")
                && (lower_line.contains("runtime")
                    || lower_line.contains("failure")
                    || lower_line.contains("warning")
                    || lower_line.contains("timeout"));
            let closes_quote = dechromed.contains('"') || dechromed.contains('\'');
            let verified_quoted_echo = monitor_argv.complete
                || (monitor_argv.quote.is_some()
                    && unclosed_quote(dechromed).is_none()
                    && monitor_argv.seen_session
                    && monitor_argv.seen_keyword
                    && monitor_argv.seen_line);
            if closes_quote
                && (!dechromed.starts_with('-') || starts_dash_payload)
                && (verified_quoted_echo || !runtime_shaped)
            {
                return true;
            }
        }
        let tokens = tokenize_monitor_argv(strip_shell_prompt(strip_pane_frame_chrome(line)))
            .unwrap_or_default();
        if monitor_argv_tokens_valid(&tokens)
            && ((matches!(monitor_argv.kind, EchoKind::Keyword)
                && keyword_echo_matches
                && !monitor_argv.pending_value
                && tokens
                    .first()
                    .is_some_and(|token| token == "--line" || token.starts_with("--line=")))
                || (matches!(monitor_argv.kind, EchoKind::Monitor)
                    && monitor_line_contains_keyword_value(line, occurrence)))
        {
            return true;
        }
    }
    // A keyword flag value is self-echo only when the complete line is a
    // recognized command argv. This prevents application prose or flags from
    // forging monitor state.
    if monitor_head(line).or_else(|| keyword_head(line)).is_some()
        && is_monitor_argv_line(line, &[occurrence])
    {
        return true;
    }

    // Quoted mention, only in proven prose/narration context: the occurrence
    // is wrapped in backticks or matched quotes AND the surrounding line is
    // prose about the keyword (a narration cue or the live-watch summary
    // vocabulary), not a structured runtime field. Structured quoting is
    // field syntax — the char before the opening quote is `=`/`{`/`,`/`:`
    // (JSON `"k":"v"`, logfmt `k="v"`) — and never counts as a mention,
    // even when a narration cue is adjacent on this or the previous line
    // (issue #342 review blocker 2 and re-review finding F2).
    let before = line[..occurrence_start].chars().next_back();
    let after = line[occurrence_end..].chars().next();
    let before_quote = before
        .and_then(|open| line[..occurrence_start].strip_suffix(open))
        .and_then(|stripped| stripped.chars().next_back());
    if let (Some(open), Some(close)) = (before, after)
        && matches!(open, '`' | '\'' | '"')
        && open == close
        && !matches!(before_quote, Some('=') | Some('{') | Some(',') | Some(':'))
        && (is_prose_about_keyword(line, occurrence_start)
            || previous_line_is_narration_context(previous_line))
    {
        return true;
    }
    if has_narration_label(&line[..occurrence_start]) {
        return true;
    }
    // Monitor status summary: `… <kw> — N live watch …` (em-dash directly
    // after the keyword plus the live-watch vocabulary on the same line).
    // `lower_line` comes from the caller, avoiding a per-occurrence
    // allocation in the hot path.
    let rest = line[occurrence_end..].trim_start_matches(' ');
    if rest.starts_with('—') && is_live_watch_summary(rest, lower_line) {
        return true;
    }

    if monitor_argv.complete
        && line[..occurrence_start].trim().is_empty()
        && monitor_continuation_is_valid(line, occurrence, monitor_argv)
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

            if monitor_argv.active
                && monitor_argv.pending_value
                && line_starts_with_occurrence
                && monitor_continuation_is_valid(line, occurrence, monitor_argv)
            {
                return true;
            }
        }
    }

    false
}

fn monitor_continuation_is_valid(line: &str, occurrence: &str, state: MonitorArgvState) -> bool {
    let command = strip_shell_prompt(strip_pane_frame_chrome(line));
    let Some(tokens) = tokenize_monitor_argv(command) else {
        return false;
    };
    tokens
        .first()
        .is_some_and(|token| token.eq_ignore_ascii_case(occurrence))
        && (tokens.len() > 1 && monitor_argv_tokens_valid(&tokens[1..]) || tokens.len() == 1)
        && match state.kind {
            EchoKind::Monitor => {
                state.seen_session
                    && state.seen_keyword
                    && matches!(state.pending_flag, Some("--keywords"))
            }
            EchoKind::Keyword => {
                state.seen_session
                    && state.seen_keyword
                    && matches!(state.pending_flag, Some("--keyword" | "--line"))
            }
        }
}

fn monitor_line_contains_keyword_value(line: &str, occurrence: &str) -> bool {
    let Some(tokens) = tokenize_monitor_argv(strip_shell_prompt(strip_pane_frame_chrome(line)))
    else {
        return false;
    };
    tokens
        .iter()
        .enumerate()
        .take_while(|(_, token)| token.as_str() != "--")
        .any(|(index, token)| {
            let name = token
                .split_once('=')
                .map_or(token.as_str(), |(name, _)| name);
            if !matches!(name, "--keywords" | "--keyword" | "keywords" | "keyword") {
                return false;
            }
            let value = token
                .split_once('=')
                .map(|(_, value)| value)
                .or_else(|| tokens.get(index + 1).map(String::as_str));
            value.is_some_and(|value| {
                value
                    .split(',')
                    .any(|part| part.eq_ignore_ascii_case(occurrence))
            })
        })
}

fn keyword_echo_mentions(line: &str, previous: Option<&str>, occurrence: &str) -> bool {
    fn has_value(line: &str, occurrence: &str) -> bool {
        let Some(tokens) = tokenize_monitor_argv(strip_shell_prompt(strip_pane_frame_chrome(line)))
        else {
            return false;
        };
        tokens
            .iter()
            .enumerate()
            .take_while(|(_, token)| token.as_str() != "--")
            .any(|(i, token)| {
                let name = token
                    .split_once('=')
                    .map_or(token.as_str(), |(name, _)| name);
                if name != "--keyword" && name != "keyword" {
                    return false;
                }
                let value = token
                    .split_once('=')
                    .map(|(_, value)| value)
                    .or_else(|| tokens.get(i + 1).map(String::as_str));
                value.is_some_and(|value| {
                    value
                        .split(',')
                        .any(|part| part.eq_ignore_ascii_case(occurrence))
                })
            })
    }
    has_value(line, occurrence)
        || previous.is_some_and(|previous| has_value(previous, occurrence))
        || previous
            .map(strip_pane_frame_chrome)
            .is_some_and(|previous| {
                previous.trim_end().ends_with("--keyword")
                    && strip_shell_prompt(strip_pane_frame_chrome(line))
                        .split_whitespace()
                        .next()
                        .is_some_and(|token| token.eq_ignore_ascii_case(occurrence))
            })
}

fn line_is_structured_runtime(line: &str) -> bool {
    let trimmed = strip_shell_prompt(strip_pane_frame_chrome(line));
    trimmed.starts_with('{')
        || trimmed.starts_with('[')
        || trimmed
            .split_whitespace()
            .next()
            .is_some_and(|token| token.contains('=') && !token.starts_with('-'))
}

fn is_live_watch_summary(rest: &str, lower_line: &str) -> bool {
    let Some(after_dash) = rest.strip_prefix('—').map(str::trim_start) else {
        return false;
    };
    let Some((count, tail)) = after_dash.split_once(' ') else {
        return false;
    };
    let phrase = tail.trim_start().to_ascii_lowercase();
    let valid_phrase = phrase.strip_prefix("live watch").is_some_and(|rest| {
        rest.is_empty()
            || rest.starts_with(' ')
            || rest.starts_with("es")
                && (rest[2..].is_empty()
                    || rest[2..]
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '.' | ',' | ';')))
            || rest.starts_with(';')
    });
    count.chars().all(|ch| ch.is_ascii_digit())
        && valid_phrase
        && contains_bounded(lower_line, "live watch")
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
fn is_prose_about_keyword(line: &str, occurrence_start: usize) -> bool {
    let lower_before = line[..occurrence_start].to_ascii_lowercase();
    has_narration_label(&line[..occurrence_start])
        || lower_before.ends_with("see the example ")
        || lower_before.ends_with("see the example `")
        || lower_before.ends_with("example: ")
}

fn has_narration_label(prefix: &str) -> bool {
    let lower = prefix.to_ascii_lowercase();
    KEYWORD_NARRATION_CUES.iter().any(|cue| {
        lower.rfind(cue).is_some_and(|at| {
            let suffix = &lower[at + cue.len()..];
            suffix.contains(':')
                && suffix
                    .chars()
                    .all(|ch| ch.is_whitespace() || matches!(ch, ':' | ';' | ',' | '.' | '-' | '/'))
        })
    })
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
            let expected_hits = if variant.contains("live watch")
                || variant.contains("evidence prose")
                || variant.contains("requirement prose")
                || variant.starts_with('`')
                || variant.contains("runtime owner failed;")
                || variant.starts_with("clawhip tmux")
                || variant.contains("at pane")
                || variant.starts_with("- command prose:")
            {
                0
            } else {
                1
            };
            assert_eq!(
                hits.len(),
                expected_hits,
                "unexpected exact-provenance result for {variant:?}: {hits:?}"
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

        assert_eq!(hits.len(), 1, "prompt self-echo hits: {hits:?}");
        assert_eq!(
            hits[0].line,
            "owner-endpoint-unreachable: runtime owner failed"
        );
        let genuine = &hits[0];
        assert_eq!(genuine.provenance.as_ref().unwrap().pane_id, "%466");
        assert_eq!(genuine.provenance.as_ref().unwrap().cursor, Some(16));
        assert_eq!(
            genuine.provenance.as_ref().unwrap().source,
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

    /// Issue #345: a wrapped argv row that ends with a value-taking flag
    /// (`--session`) enters the pending-value branch on the next row; that
    /// row's remainder can itself discover a *newer* pending flag
    /// (`--keywords`). The old origin flag must not overwrite the newer
    /// discovery, or the row after it (the wrapped keyword value) loses its
    /// verified-continuation suppression and alerts on self-echo.
    #[test]
    fn collect_keyword_hits_preserves_newly_pending_flag_across_wrapped_argv_rows() {
        let keyword = "panic";

        // Exact three-row wrapped argv: head row ends with the value-taking
        // `--session`, the second row both supplies that value and discovers
        // `--keywords`, the third row is the wrapped keyword value. All three
        // rows are the monitor's own command echo: zero hits.
        let wrapped_three_rows = "boot
clawhip tmux watch --session
worker --keywords
panic";
        assert!(
            collect_keyword_hits("boot", wrapped_three_rows, &[keyword.into()]).is_empty(),
            "wrapped argv rows must not self-alert"
        );

        // The same three rows followed by a genuine application panic after
        // the completed command: exactly one hit, on the application line.
        let with_genuine_panic = "boot
clawhip tmux watch --session
worker --keywords
panic
panic: application failure";
        let hits = collect_keyword_hits("boot", with_genuine_panic, &[keyword.into()]);
        assert_eq!(hits.len(), 1, "genuine panic must be kept: {hits:?}");
        assert_eq!(hits[0].line, "panic: application failure");
        assert_eq!(hits[0].keyword, "panic");
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

        // The argv-fragment path (is_keyword_flag_value) must terminate too:
        // a wrapped monitor echo containing a real keyword alongside an
        // empty one must neither hang nor emit (re-review finding F1).
        let wrapped_echo_current =
            "boot\n│ --stale-minutes 60 --format compact --keywords owner-endpoint-unreachable │";
        let echo_hits = collect_keyword_hits(
            "boot",
            wrapped_echo_current,
            &["owner-endpoint-unreachable".into(), "".into()],
        );
        assert_eq!(
            echo_hits.len(),
            1,
            "headless flags must not forge monitor state: {echo_hits:?}"
        );
    }

    /// Re-review finding F2: cue-adjacent structured output must still alert
    /// — the field-syntax gate keeps quote suppression off JSON/logfmt values
    /// even when a narration cue precedes on the previous line or shares the
    /// line.
    #[test]
    fn collect_keyword_hits_keeps_structured_output_adjacent_to_narration_cues() {
        // Cue on the previous line, JSON failure on this line.
        let json_after_cue = collect_keyword_hits(
            "boot\nfailures such as:",
            "failures such as:\n{\"error\":\"owner-endpoint-unreachable\"}",
            &["owner-endpoint-unreachable".into()],
        );
        assert_eq!(
            json_after_cue.len(),
            1,
            "cue-adjacent JSON must alert: got {json_after_cue:?}"
        );

        // Cue on the previous line, logfmt failure on this line.
        let logfmt_after_cue = collect_keyword_hits(
            "boot\nerrors were logged, such as",
            "errors were logged, such as\nmsg=\"owner-endpoint-unreachable: runtime owner failed\"",
            &["owner-endpoint-unreachable".into()],
        );
        assert_eq!(
            logfmt_after_cue.len(),
            1,
            "cue-adjacent logfmt must alert: got {logfmt_after_cue:?}"
        );

        // Prose backtick mention after the same cue stays suppressed.
        let mention_after_cue = collect_keyword_hits(
            "boot\nfailures such as:",
            "failures such as:\nsee `owner-endpoint-unreachable` above",
            &["owner-endpoint-unreachable".into()],
        );
        assert!(mention_after_cue.is_empty(), "got {mention_after_cue:?}");
    }

    /// Re-review finding F3: the mono notify path's own echo
    /// (`clawhip tmux keyword ... --line "<kw text>"`) is a self-match.
    #[test]
    fn collect_keyword_hits_ignores_mono_keyword_notify_echo() {
        let current = "boot
clawhip tmux keyword --session omx --keyword panic --line \"panic: runtime panic in worker\"
panic: genuine application panic";

        let hits = collect_keyword_hits("boot", current, &["panic".into()]);

        assert_eq!(hits.len(), 1, "got {hits:?}");
        assert_eq!(hits[0].line, "panic: genuine application panic");
    }

    #[test]
    fn collect_keyword_hits_covers_structured_and_provenance_signed_review_cases() {
        let keyword = "owner-endpoint-unreachable";
        let structured = [
            r#"{"message":"such as owner-endpoint-unreachable","level":"error","live-watch":"approve"}"#,
            r#"level=error msg="newly emitted owner-endpoint-unreachable" note="approve""#,
            r#"{"message":"clawhip emit agent.failed owner-endpoint-unreachable"}"#,
            r#"level=error parent_pid=42 message="owner-endpoint-unreachable: runtime failure""#,
        ];
        for line in structured {
            assert_eq!(
                collect_keyword_hits("boot", &format!("boot\n{line}"), &[keyword.into()]).len(),
                1,
                "structured runtime must alert: {line}"
            );
        }

        let prose_runtime = "boot
live watch worker returned \"owner-endpoint-unreachable: error\"";
        assert_eq!(
            collect_keyword_hits("boot", prose_runtime, &[keyword.into()]).len(),
            1,
            "non-summary live-watch prose must alert"
        );

        let verified_runtime = "boot
clawhip tmux keyword --session s --keyword owner-endpoint-unreachable --line \"owner-endpoint-unreachable: runtime failure\"";
        assert!(collect_keyword_hits("boot", verified_runtime, &[keyword.into()]).is_empty());

        let incomplete = "boot
clawhip tmux watch --keywords owner-endpoint-unreachable";
        assert_eq!(
            collect_keyword_hits("boot", incomplete, &[keyword.into()]).len(),
            1
        );

        let retry_boolean = "boot
clawhip tmux watch --session s --keywords owner-endpoint-unreachable --retry-enter true";
        assert!(collect_keyword_hits("boot", retry_boolean, &[keyword.into()]).is_empty());

        let empty_watch_head = "boot
clawhip tmux watch
--session s --keywords owner-endpoint-unreachable
owner-endpoint-unreachable: runtime failure";
        assert_eq!(
            collect_keyword_hits("boot", empty_watch_head, &[keyword.into()]).len(),
            1
        );

        let empty_keyword_head = "boot
clawhip tmux keyword
--session s --keyword owner-endpoint-unreachable --line 'owner-endpoint-unreachable: echoed'
owner-endpoint-unreachable: runtime failure";
        assert_eq!(
            collect_keyword_hits("boot", empty_keyword_head, &[keyword.into()]).len(),
            1
        );

        let wrapped_shell = "boot
clawhip tmux new --session s --shell \"bash
-c\"
--keywords owner-endpoint-unreachable";
        assert!(collect_keyword_hits("boot", wrapped_shell, &[keyword.into()]).is_empty());

        let wrapped_kickoff = "boot
clawhip tmux new --session s --kickoff \"start
worker\"
--keywords owner-endpoint-unreachable";
        assert!(collect_keyword_hits("boot", wrapped_kickoff, &[keyword.into()]).is_empty());

        let forged_after_unverified = "boot
application printed \"shell\"
--keywords owner-endpoint-unreachable";
        assert_eq!(
            collect_keyword_hits("boot", forged_after_unverified, &[keyword.into()]).len(),
            1
        );

        let wrapped_command = "boot
$ clawhip tmux watch --session s --format compact
--keywords owner-endpoint-unreachable --channel 1
owner-endpoint-unreachable: runtime failure";
        let wrapped_hits = collect_keyword_hits("boot", wrapped_command, &[keyword.into()]);
        assert_eq!(wrapped_hits.len(), 1);
        assert_eq!(
            wrapped_hits[0].line,
            "owner-endpoint-unreachable: runtime failure"
        );

        let forged_state = "boot
application logged --keywords
owner-endpoint-unreachable: runtime failure";
        assert_eq!(
            collect_keyword_hits("boot", forged_state, &[keyword.into()]).len(),
            1
        );

        let forms = [
            "clawhip tmux keyword --session s --keyword panic --line \"panic: echoed\"",
            "clawhip tmux keyword --line='panic: echoed' --keyword=panic --session=s",
            "clawhip tmux keyword --keyword panic --line 'panic: echoed' --session s",
        ];
        for command in forms {
            assert!(
                collect_keyword_hits("boot", &format!("boot\n{command}"), &["panic".into()])
                    .is_empty(),
                "shipped keyword command form must suppress: {command}"
            );
        }

        let wrapped_keyword = "boot
clawhip tmux keyword --session=s --keyword panic
--line='panic: echoed'
panic: application failure";
        let hits = collect_keyword_hits("boot", wrapped_keyword, &["panic".into()]);
        assert!(
            hits.iter()
                .any(|hit| hit.line == "panic: application failure"),
            "incomplete keyword head must fail open: {hits:?}"
        );

        let malformed_quote = "boot
clawhip tmux keyword --session s --line \"panic
panic: info";
        let hits = collect_keyword_hits("boot", malformed_quote, &["panic".into()]);
        assert!(
            hits.iter().any(|hit| hit.line == "panic: info"),
            "malformed quote must not hide runtime output: {hits:?}"
        );

        let wrapped_quote = "boot
clawhip tmux keyword --session s --keyword panic --line \"panic: echoed
continued panic: echoed\"
panic: application failure";
        let hits = collect_keyword_hits("boot", wrapped_quote, &["panic".into()]);
        assert_eq!(hits.len(), 1, "wrapped quote hits: {hits:?}");
        assert_eq!(hits[0].line, "panic: application failure");

        let framed_quote = "boot
│ clawhip tmux keyword --session s --keyword panic --line \"panic: echoed
│ --panic\"
panic: application failure";
        let hits = collect_keyword_hits("boot", framed_quote, &["panic".into()]);
        assert_eq!(hits.len(), 1, "framed quote hits: {hits:?}");
        assert_eq!(hits[0].line, "panic: application failure");

        let value_only = "boot
clawhip tmux keyword --session s --keyword panic --line
panic
panic: application failure";
        let hits = collect_keyword_hits("boot", value_only, &["panic".into()]);
        assert_eq!(hits.len(), 1, "value-only hits: {hits:?}");
        assert_eq!(hits[0].line, "panic: application failure");

        let application_flags = "boot
clawhip tmux keyword --session s --keyword panic --line 'panic: echoed'
--channel panic";
        assert_eq!(
            collect_keyword_hits("boot", application_flags, &["panic".into()]).len(),
            1,
            "application output after a complete command must alert"
        );

        let audit = "boot
clawhip tmux cli-new start session=s keywords=panic channel=1";
        assert!(
            collect_keyword_hits("boot", audit, &["panic".into()]).is_empty(),
            "cli-new audit hits"
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

        // The same prose predecessor must NOT suppress any non-argv
        // continuation either — including a flag-shaped echo line, because
        // "application logged option" is not verified monitor argv.
        let wrapped_echo = collect_keyword_hits(
            "boot",
            "boot
application logged option --keywords
owner-endpoint-unreachable --channel 1508831529856663612",
            &["owner-endpoint-unreachable".into()],
        );
        assert_eq!(
            wrapped_echo.len(),
            1,
            "unrelated prior cue must never suppress: got {wrapped_echo:?}"
        );

        // Structured (non-colon) failure after the unrelated cue also alerts.
        let json_after_cue = collect_keyword_hits(
            "boot",
            "boot
application logged option --keywords
{\"error\":\"owner-endpoint-unreachable\",\"message\":\"runtime owner failed\"}",
            &["owner-endpoint-unreachable".into()],
        );
        assert_eq!(
            json_after_cue.len(),
            1,
            "structured failure after unrelated cue must alert: got {json_after_cue:?}"
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
        assert_eq!(
            monitor_argv_echo.len(),
            1,
            "headless continuation must alert, got {monitor_argv_echo:?}"
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
    /// Snapshot-boundary safety: identical snapshots, full overlap, no
    /// overlap, and empty snapshots must neither emit nor panic.
    #[test]
    fn collect_keyword_hits_handles_snapshot_boundaries() {
        let kw = &["owner-endpoint-unreachable".into()];

        // current == previous: nothing appended.
        assert!(collect_keyword_hits("a\nb\nc", "a\nb\nc", kw).is_empty());

        // Full overlap: current is a strict scroll of previous.
        assert!(collect_keyword_hits("x\na\nb\nc", "a\nb\nc", kw).is_empty());

        // No overlap at all: everything is fresh.
        let fresh = collect_keyword_hits(
            "old",
            "owner-endpoint-unreachable: runtime owner failed",
            kw,
        );
        assert_eq!(fresh.len(), 1);

        // Empty previous snapshot: everything is fresh, context line absent.
        let from_empty = collect_keyword_hits(
            "",
            "boot\nowner-endpoint-unreachable: runtime owner failed",
            kw,
        );
        assert_eq!(from_empty.len(), 1);
        assert_eq!(
            from_empty[0].line,
            "owner-endpoint-unreachable: runtime owner failed"
        );

        // Empty current snapshot: no hits, no panic.
        assert!(collect_keyword_hits("boot\nkw line", "", kw).is_empty());

        // Provenance cursor numbering starts at the first appended line and
        // the overlap context line cannot itself emit (cursor 0 excluded).
        let hits = collect_keyword_hits_with_provenance(
            "keep\nplain line",
            "keep\nplain line\nowner-endpoint-unreachable: runtime owner failed",
            kw,
            KeywordMatchProvenance {
                pane_id: "%2".into(),
                pane_name: "0.1".into(),
                cursor: None,
                source: KeywordMatchSource::FreshOutput,
            },
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provenance.as_ref().unwrap().pane_id, "%2");
        assert_eq!(hits[0].provenance.as_ref().unwrap().pane_name, "0.1");
        assert_eq!(hits[0].provenance.as_ref().unwrap().cursor, Some(3));
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
        assert_eq!(
            collect_keyword_hits(
                "boot",
                &format!("boot\n{list}"),
                &["owner-endpoint-unreachable".into()]
            )
            .len(),
            1,
            "headless keyword flags must alert"
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
