use serde::Serialize;
use std::collections::BTreeMap;

const MAX_RESTART_CHURN: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestartChurnSummary {
    pub family: String,
    pub occurrences: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_restart_count: Option<usize>,
    pub sample: String,
}

#[derive(Debug, Default)]
pub struct RestartChurnState {
    counts: BTreeMap<String, usize>,
    max_restart_counts: BTreeMap<String, usize>,
    samples: BTreeMap<String, String>,
}

impl RestartChurnState {
    pub fn observe(&mut self, event_name: &str, detail: &str) {
        let restart_count = extract_field(detail, "restart_count").and_then(parse_usize);
        let sample = truncate_detail(detail, 140);
        for family in classify_restart_churn(event_name, detail) {
            *self.counts.entry(family.clone()).or_default() += 1;
            if let Some(count) = restart_count {
                let entry = self.max_restart_counts.entry(family.clone()).or_default();
                *entry = (*entry).max(count);
            }
            self.samples.entry(family).or_insert_with(|| sample.clone());
        }
    }

    pub fn groups(&self) -> usize {
        self.counts.len()
    }

    pub fn summaries(&self) -> Vec<RestartChurnSummary> {
        let mut summaries = self
            .counts
            .iter()
            .map(|(family, occurrences)| RestartChurnSummary {
                family: family.clone(),
                occurrences: *occurrences,
                max_restart_count: self.max_restart_counts.get(family).copied(),
                sample: self.samples.get(family).cloned().unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| {
            right
                .occurrences
                .cmp(&left.occurrences)
                .then(left.family.cmp(&right.family))
        });
        summaries.truncate(MAX_RESTART_CHURN);
        summaries
    }
}

fn classify_restart_churn(event_name: &str, detail: &str) -> Vec<String> {
    let mut families = Vec::new();

    if is_fresh_restart(event_name, detail) {
        families.push("fresh_restart".to_string());
    }
    if event_name == "auto_trigger_timeout" {
        families.push("auto_trigger_timeout".to_string());
    }
    if is_ctrl_d_restart_loop(event_name) {
        families.push("ctrl_d_restart_loop".to_string());
    }
    if is_quit_after_eof(event_name, detail) {
        families.push("quit_after_eof".to_string());
    }

    families
}

fn is_fresh_restart(event_name: &str, detail: &str) -> bool {
    if matches!(
        event_name,
        "claude_start" | "codex_start" | "claude_restart" | "codex_restart"
    ) {
        return extract_field(detail, "mode") == Some("fresh_restart");
    }

    if event_name == "ipc_restart" {
        return extract_field(detail, "mode") == Some("fresh");
    }

    matches!(
        event_name,
        "fresh_restart_before_prompt"
            | "ctrl_d_restart_fresh"
            | "ctrl_d_before_prompt_restart_fresh"
            | "ctrl_d_committed_cycle_restart_fresh"
    )
}

fn is_ctrl_d_restart_loop(event_name: &str) -> bool {
    matches!(
        event_name,
        "ctrl_d_restart_fresh"
            | "ctrl_d_before_prompt_restart_fresh"
            | "ctrl_d_committed_cycle_restart_fresh"
    )
}

fn is_quit_after_eof(event_name: &str, detail: &str) -> bool {
    if matches!(event_name, "user_quit_after_eof" | "user_quit_after_ctrl_d") {
        return true;
    }

    event_name == "supervisor_exit"
        && extract_field(detail, "reason")
            .is_some_and(|reason| reason.starts_with("user_quit_after_"))
}

fn extract_field<'a>(detail: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    let start = detail.find(&needle)? + needle.len();
    let remainder = &detail[start..];
    let end = remainder
        .find(char::is_whitespace)
        .unwrap_or(remainder.len());
    Some(remainder[..end].trim_matches('"'))
}

fn parse_usize(raw: &str) -> Option<usize> {
    raw.parse::<usize>().ok()
}

fn truncate_detail(detail: &str, limit: usize) -> String {
    let normalized = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        return normalized;
    }

    let mut truncated = String::new();
    for ch in normalized.chars().take(limit.saturating_sub(1)) {
        truncated.push(ch);
    }
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_churn_detects_requested_families() {
        let mut state = RestartChurnState::default();
        state.observe(
            "codex_start",
            "codex_start mode=fresh_restart restart_count=2",
        );
        state.observe(
            "auto_trigger_timeout",
            "auto_trigger_timeout harness=codex reason=no_prompt_after_30s",
        );
        state.observe(
            "ctrl_d_restart_fresh",
            "ctrl_d_restart_fresh restart_count=3",
        );
        state.observe("user_quit_after_ctrl_d", "user_quit_after_ctrl_d");
        state.observe(
            "supervisor_exit",
            "supervisor_exit reason=user_quit_after_ctrl_d pane=%26 restart_count=0",
        );

        let summaries = state.summaries();
        assert_eq!(state.groups(), 4);
        assert!(summaries.iter().any(|entry| entry.family == "fresh_restart"
            && entry.occurrences == 2
            && entry.max_restart_count == Some(3)));
        assert!(
            summaries
                .iter()
                .any(|entry| entry.family == "auto_trigger_timeout" && entry.occurrences == 1)
        );
        assert!(
            summaries
                .iter()
                .any(|entry| entry.family == "ctrl_d_restart_loop"
                    && entry.occurrences == 1
                    && entry.max_restart_count == Some(3))
        );
        assert!(
            summaries
                .iter()
                .any(|entry| entry.family == "quit_after_eof" && entry.occurrences == 2)
        );
    }
}
