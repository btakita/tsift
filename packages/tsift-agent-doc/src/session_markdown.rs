use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::io::Read as _;
use std::path::Path;

const FILE_PROBE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentDocSessionDocument {
    pub session_id: Option<String>,
    pub backlog_items: Vec<AgentDocBacklogItem>,
    pub queue_items: Vec<AgentDocQueueItem>,
}

impl AgentDocSessionDocument {
    pub fn parse(content: &str) -> Self {
        let mut backlog_items = Vec::new();
        let mut queue_items = Vec::new();
        let mut in_queue = false;
        for (idx, line) in content.lines().enumerate() {
            let line_number = idx + 1;
            if let Some(backlog_item) = parse_backlog_line(line, line_number) {
                backlog_items.push(backlog_item);
            }

            let trimmed = line.trim();
            if trimmed.starts_with("<!-- agent:queue") {
                in_queue = true;
                continue;
            }
            if trimmed.starts_with("<!-- /agent:queue") {
                in_queue = false;
                continue;
            }
            if in_queue && let Some(queue_item) = parse_queue_line(line, line_number) {
                queue_items.push(queue_item);
            }
        }

        Self {
            session_id: session_id_from_content(content),
            backlog_items,
            queue_items,
        }
    }

    pub fn parse_if_session(content: &str) -> Option<Self> {
        markdown_content_looks_like_agent_doc_session(content).then(|| Self::parse(content))
    }

    pub fn read(path: &Path) -> Result<Option<Self>> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("reading agent-doc session document {}", path.display()))?;
        Ok(Self::parse_if_session(&content))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentDocBacklogItem {
    pub id: String,
    pub text: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentDocQueueItem {
    Dispatch { value: String, line: usize },
    Preset { value: String, line: usize },
    Do { id: String, line: usize },
}

impl AgentDocQueueItem {
    pub fn line(&self) -> usize {
        match self {
            Self::Dispatch { line, .. } | Self::Preset { line, .. } | Self::Do { line, .. } => {
                *line
            }
        }
    }
}

pub fn session_id_from_path(path: &Path) -> Result<Option<String>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading agent-doc session document {}", path.display()))?;
    Ok(session_id_from_content(&content))
}

pub fn session_id_from_content(content: &str) -> Option<String> {
    content.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("agent_doc_session:")
            .map(str::trim)
            .map(|value| value.trim_matches('"').trim_matches('\'').trim())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

pub fn parse_backlog_line(line: &str, line_number: usize) -> Option<AgentDocBacklogItem> {
    let trimmed = line.trim();
    if !trimmed.starts_with("- [") {
        return None;
    }
    let start = trimmed.find("[#")?;
    let after_start = start + 2;
    let rest = &trimmed[after_start..];
    let end = rest.find(']')?;
    let id = rest[..end].trim();
    if id.is_empty() {
        return None;
    }
    Some(AgentDocBacklogItem {
        id: id.to_string(),
        text: rest[end + 1..].trim().to_string(),
        line: line_number,
    })
}

pub fn parse_queue_line(line: &str, line_number: usize) -> Option<AgentDocQueueItem> {
    let trimmed = line.trim();
    if let Some(value) = trimmed
        .strip_prefix("dispatch ")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(AgentDocQueueItem::Dispatch {
            value: value.to_string(),
            line: line_number,
        });
    }
    if let Some(value) = trimmed
        .strip_prefix("preset ")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(AgentDocQueueItem::Preset {
            value: value.to_string(),
            line: line_number,
        });
    }
    let rest = trimmed.strip_prefix("- do [#")?;
    let end = rest.find(']')?;
    let id = rest[..end].trim();
    (!id.is_empty()).then(|| AgentDocQueueItem::Do {
        id: id.to_string(),
        line: line_number,
    })
}

pub fn markdown_content_looks_like_agent_doc_session(content: &str) -> bool {
    session_id_from_content(content).is_some()
        || content.contains("<!-- agent:exchange")
        || content.contains("<!-- agent:backlog")
        || content.contains("<!-- agent:queue")
        || content.lines().any(|line| {
            let trimmed = line.trim();
            trimmed == "## Exchange" || trimmed == "## Backlog"
        })
}

pub fn markdown_file_looks_like_agent_doc_session(path: &Path) -> bool {
    read_file_prefix(path, FILE_PROBE_BYTES)
        .as_deref()
        .is_some_and(markdown_content_looks_like_agent_doc_session)
}

pub fn log_content_looks_like_agent_doc_runtime_log(content: &str) -> bool {
    let mut saw_line = false;
    for line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(8)
    {
        saw_line = true;
        if !(line.starts_with('[') && line.contains("] ")) {
            return false;
        }
    }
    saw_line
}

pub fn log_file_looks_like_agent_doc_runtime_log(path: &Path) -> bool {
    read_file_prefix(path, FILE_PROBE_BYTES)
        .as_deref()
        .is_some_and(log_content_looks_like_agent_doc_runtime_log)
}

fn read_file_prefix(path: &Path, max_bytes: usize) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut buffer = Vec::new();
    file.by_ref()
        .take(max_bytes as u64)
        .read_to_end(&mut buffer)
        .ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_doc_session_backlog_and_queue() {
        let doc = AgentDocSessionDocument::parse(
            r##"---
agent_doc_session: "tsift-v0.1"
---

## Exchange

<!-- agent:queue preset="#spec" go -->
dispatch #spec-test-build-install-commit-push
- do [#x5fw]
<!-- /agent:queue -->

<!-- agent:backlog -->
- [ ] [#x5fw] Move CLI parsing into tsift-agent-doc.
<!-- /agent:backlog -->
"##,
        );

        assert_eq!(doc.session_id.as_deref(), Some("tsift-v0.1"));
        assert_eq!(doc.backlog_items.len(), 1);
        assert_eq!(doc.backlog_items[0].id, "x5fw");
        assert_eq!(
            doc.backlog_items[0].text,
            "Move CLI parsing into tsift-agent-doc."
        );
        assert_eq!(
            doc.queue_items,
            vec![
                AgentDocQueueItem::Dispatch {
                    value: "#spec-test-build-install-commit-push".to_string(),
                    line: 8,
                },
                AgentDocQueueItem::Do {
                    id: "x5fw".to_string(),
                    line: 9,
                },
            ]
        );
    }

    #[test]
    fn detects_agent_doc_markdown_and_runtime_logs() {
        assert!(markdown_content_looks_like_agent_doc_session(
            "---\nagent_doc_session: tsift-v0.1\n---\n"
        ));
        assert!(markdown_content_looks_like_agent_doc_session(
            "## Exchange\n\n<!-- agent:exchange patch=append -->\n"
        ));
        assert!(!markdown_content_looks_like_agent_doc_session(
            "# Product Backlog\n\n- [ ] normal project note\n"
        ));
        assert!(log_content_looks_like_agent_doc_runtime_log(
            "[1776528398] claude_start mode=fresh_restart\n[1776528399] commit ok\n"
        ));
        assert!(!log_content_looks_like_agent_doc_runtime_log(
            "plain text log line\n[1776528399] commit ok\n"
        ));
    }
}
