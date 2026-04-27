use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

const SECTION_MARKER: &str = "<!-- tsift:code-navigation -->";
const SECTION_END_MARKER: &str = "<!-- /tsift:code-navigation -->";

const DEFAULT_SECTION: &str = r#"<!-- tsift:code-navigation -->
## Code Navigation

Before reading source files for understanding, check tsift:
- `tsift search <query>` — AST-aware hybrid search (prefer over grep/rg)
- `tsift summarize <symbol>` — cached summary (~50 tokens vs ~2000 for file read)
- `tsift explain <symbol>` — callers, callees, community context
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->"#;

pub struct InitResult {
    pub file: PathBuf,
    pub action: InitAction,
}

pub enum InitAction {
    Created,
    Updated,
    AlreadyPresent,
}

impl std::fmt::Display for InitAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitAction::Created => write!(f, "created"),
            InitAction::Updated => write!(f, "updated"),
            InitAction::AlreadyPresent => write!(f, "already present"),
        }
    }
}

pub fn find_instruction_file(dir: &Path) -> Option<PathBuf> {
    let agents = dir.join("AGENTS.md");
    if agents.exists() {
        return Some(agents);
    }
    let claude = dir.join("CLAUDE.md");
    if claude.exists() {
        return Some(claude);
    }
    None
}

pub fn init(dir: &Path) -> Result<InitResult> {
    let file = match find_instruction_file(dir) {
        Some(f) => f,
        None => {
            let agents = dir.join("AGENTS.md");
            std::fs::write(&agents, format!("{}\n", DEFAULT_SECTION))?;
            return Ok(InitResult {
                file: agents,
                action: InitAction::Created,
            });
        }
    };

    let content = std::fs::read_to_string(&file)?;

    if content.contains(SECTION_MARKER) {
        let start = content.find(SECTION_MARKER).unwrap();
        if let Some(end_rel) = content[start..].find(SECTION_END_MARKER) {
            let end = start + end_rel + SECTION_END_MARKER.len();
            let before = &content[..start];
            let after = &content[end..];
            let new_content = format!("{}{}{}", before, DEFAULT_SECTION, after);
            if new_content == content {
                return Ok(InitResult {
                    file,
                    action: InitAction::AlreadyPresent,
                });
            }
            std::fs::write(&file, new_content)?;
            return Ok(InitResult {
                file,
                action: InitAction::Updated,
            });
        } else {
            bail!(
                "Found {} in {} but no matching {} — fix manually",
                SECTION_MARKER,
                file.display(),
                SECTION_END_MARKER
            );
        }
    }

    let mut new_content = content.clone();
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push('\n');
    new_content.push_str(DEFAULT_SECTION);
    new_content.push('\n');
    std::fs::write(&file, new_content)?;

    Ok(InitResult {
        file,
        action: InitAction::Created,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_creates_agents_md_when_none_exists() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path()).unwrap();
        assert!(matches!(result.action, InitAction::Created));
        assert_eq!(result.file.file_name().unwrap(), "AGENTS.md");
        let content = std::fs::read_to_string(&result.file).unwrap();
        assert!(content.contains(SECTION_MARKER));
        assert!(content.contains("tsift search"));
    }

    #[test]
    fn init_appends_to_existing_agents_md() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "# My Project\n\nSome instructions.\n").unwrap();
        let result = init(dir.path()).unwrap();
        assert!(matches!(result.action, InitAction::Created));
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.starts_with("# My Project"));
        assert!(content.contains(SECTION_MARKER));
    }

    #[test]
    fn init_prefers_agents_md_over_claude_md() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agents\n").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# Claude\n").unwrap();
        let result = init(dir.path()).unwrap();
        assert_eq!(result.file.file_name().unwrap(), "AGENTS.md");
    }

    #[test]
    fn init_uses_claude_md_when_no_agents_md() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# Claude\n").unwrap();
        let result = init(dir.path()).unwrap();
        assert_eq!(result.file.file_name().unwrap(), "CLAUDE.md");
    }

    #[test]
    fn init_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "# Project\n").unwrap();

        let r1 = init(dir.path()).unwrap();
        assert!(matches!(r1.action, InitAction::Created));
        let content_after_first = std::fs::read_to_string(&agents).unwrap();

        let r2 = init(dir.path()).unwrap();
        assert!(matches!(r2.action, InitAction::AlreadyPresent));
        let content_after_second = std::fs::read_to_string(&agents).unwrap();
        assert_eq!(content_after_first, content_after_second);
    }

    #[test]
    fn init_updates_stale_section() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        let old_section = format!(
            "{}\n## Code Navigation\n\nOld content here.\n{}",
            SECTION_MARKER, SECTION_END_MARKER
        );
        std::fs::write(&agents, format!("# Project\n\n{}\n", old_section)).unwrap();

        let result = init(dir.path()).unwrap();
        assert!(matches!(result.action, InitAction::Updated));
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains("tsift search"));
        assert!(!content.contains("Old content here."));
        assert_eq!(content.matches(SECTION_MARKER).count(), 1);
    }

    #[test]
    fn init_preserves_surrounding_content() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "# Header\n\nBefore content.\n\n## Footer\n\nAfter content.\n").unwrap();
        init(dir.path()).unwrap();
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains("# Header"));
        assert!(content.contains("Before content."));
        assert!(content.contains("## Footer"));
        assert!(content.contains("After content."));
        assert!(content.contains(SECTION_MARKER));
    }
}
