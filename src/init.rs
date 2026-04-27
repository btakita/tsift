use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

const SECTION_MARKER: &str = "<!-- tsift:code-navigation -->";
const SECTION_END_MARKER: &str = "<!-- /tsift:code-navigation -->";

const DEFAULT_SECTION: &str = r#"<!-- tsift:code-navigation -->
## Code Navigation

Run `tsift status` at session start. Use the commands listed in its `use:` output:
- `tsift search <query>` — AST-aware hybrid search (prefer over grep/rg)
- `tsift explain <symbol>` — callers, callees, community context
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->"#;

const GITIGNORE_ENTRY: &str = ".tsift/";

pub struct InitResult {
    pub updates: Vec<InstructionUpdate>,
    pub gitignore_added: bool,
}

pub struct InstructionUpdate {
    pub file: PathBuf,
    pub action: InitAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

fn ensure_gitignore(dir: &Path) -> Result<bool> {
    let gitignore = dir.join(".gitignore");
    if gitignore.exists() {
        let content = std::fs::read_to_string(&gitignore)?;
        if content.lines().any(|line| line.trim() == GITIGNORE_ENTRY) {
            return Ok(false);
        }
        let mut new_content = content;
        if !new_content.ends_with('\n') && !new_content.is_empty() {
            new_content.push('\n');
        }
        new_content.push_str(GITIGNORE_ENTRY);
        new_content.push('\n');
        std::fs::write(&gitignore, new_content)?;
    } else {
        std::fs::write(&gitignore, format!("{}\n", GITIGNORE_ENTRY))?;
    }
    Ok(true)
}

pub fn resolve_project_dir(path: &Path) -> Result<PathBuf> {
    let dir = if path.is_file() {
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("cannot determine parent of {}", path.display()))?
            .to_path_buf()
    } else {
        path.to_path_buf()
    };

    let output = Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "rev-parse", "--show-toplevel"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let root = String::from_utf8_lossy(&o.stdout).trim().to_string();
            Ok(PathBuf::from(root))
        }
        _ => Ok(dir),
    }
}

pub fn init(dir: &Path) -> Result<InitResult> {
    let gitignore_added = ensure_gitignore(dir)?;
    let mut updates = Vec::new();

    let agents = dir.join("AGENTS.md");
    updates.push(InstructionUpdate {
        file: agents.clone(),
        action: ensure_instruction_file(&agents)?,
    });

    let claude = dir.join("CLAUDE.md");
    if claude.exists() {
        updates.push(InstructionUpdate {
            file: claude.clone(),
            action: ensure_instruction_file(&claude)?,
        });
    }

    Ok(InitResult {
        updates,
        gitignore_added,
    })
}

fn ensure_instruction_file(file: &Path) -> Result<InitAction> {
    if !file.exists() {
        std::fs::write(file, format!("{}\n", DEFAULT_SECTION))?;
        return Ok(InitAction::Created);
    }

    let content = std::fs::read_to_string(file)?;

    if content.contains(SECTION_MARKER) {
        let start = content.find(SECTION_MARKER).unwrap();
        if let Some(end_rel) = content[start..].find(SECTION_END_MARKER) {
            let end = start + end_rel + SECTION_END_MARKER.len();
            let before = &content[..start];
            let after = &content[end..];
            let new_content = format!("{}{}{}", before, DEFAULT_SECTION, after);
            if new_content == content {
                return Ok(InitAction::AlreadyPresent);
            }
            std::fs::write(file, new_content)?;
            return Ok(InitAction::Updated);
        } else {
            bail!(
                "Found {} in {} but no matching {} — fix manually",
                SECTION_MARKER,
                file.display(),
                SECTION_END_MARKER
            );
        }
    }

    let mut new_content = content;
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push('\n');
    new_content.push_str(DEFAULT_SECTION);
    new_content.push('\n');
    std::fs::write(file, new_content)?;

    Ok(InitAction::Created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_creates_agents_md_when_none_exists() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path()).unwrap();
        assert_eq!(result.updates.len(), 1);
        assert!(matches!(result.updates[0].action, InitAction::Created));
        assert_eq!(result.updates[0].file.file_name().unwrap(), "AGENTS.md");
        let content = std::fs::read_to_string(&result.updates[0].file).unwrap();
        assert!(content.contains(SECTION_MARKER));
        assert!(content.contains("tsift search"));
    }

    #[test]
    fn init_appends_to_existing_agents_md() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "# My Project\n\nSome instructions.\n").unwrap();
        let result = init(dir.path()).unwrap();
        assert_eq!(result.updates.len(), 1);
        assert!(matches!(result.updates[0].action, InitAction::Created));
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.starts_with("# My Project"));
        assert!(content.contains(SECTION_MARKER));
    }

    #[test]
    fn init_updates_agents_and_claude_when_both_exist() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agents\n").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# Claude\n").unwrap();
        let result = init(dir.path()).unwrap();
        assert_eq!(result.updates.len(), 2);
        assert_eq!(result.updates[0].file.file_name().unwrap(), "AGENTS.md");
        assert_eq!(result.updates[1].file.file_name().unwrap(), "CLAUDE.md");
        assert!(
            std::fs::read_to_string(dir.path().join("AGENTS.md"))
                .unwrap()
                .contains(SECTION_MARKER)
        );
        assert!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md"))
                .unwrap()
                .contains(SECTION_MARKER)
        );
    }

    #[test]
    fn init_creates_agents_and_updates_claude_when_only_claude_exists() {
        let dir = TempDir::new().unwrap();
        let claude = dir.path().join("CLAUDE.md");
        std::fs::write(&claude, "# Claude\n").unwrap();
        let result = init(dir.path()).unwrap();
        assert_eq!(result.updates.len(), 2);
        assert_eq!(result.updates[0].file.file_name().unwrap(), "AGENTS.md");
        assert!(dir.path().join("AGENTS.md").exists());
        assert!(matches!(result.updates[0].action, InitAction::Created));
        assert!(matches!(result.updates[1].action, InitAction::Created));
        assert!(
            std::fs::read_to_string(claude)
                .unwrap()
                .contains(SECTION_MARKER)
        );
    }

    #[test]
    fn init_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "# Project\n").unwrap();

        let r1 = init(dir.path()).unwrap();
        assert!(matches!(r1.updates[0].action, InitAction::Created));
        let content_after_first = std::fs::read_to_string(&agents).unwrap();

        let r2 = init(dir.path()).unwrap();
        assert!(matches!(r2.updates[0].action, InitAction::AlreadyPresent));
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
        assert!(matches!(result.updates[0].action, InitAction::Updated));
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains("tsift search"));
        assert!(!content.contains("Old content here."));
        assert_eq!(content.matches(SECTION_MARKER).count(), 1);
    }

    #[test]
    fn init_creates_gitignore_with_tsift_entry() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path()).unwrap();
        assert!(result.gitignore_added);
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(".tsift/"));
    }

    #[test]
    fn init_appends_to_existing_gitignore() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "/target\n").unwrap();
        let result = init(dir.path()).unwrap();
        assert!(result.gitignore_added);
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains("/target"));
        assert!(content.contains(".tsift/"));
    }

    #[test]
    fn init_skips_gitignore_when_already_present() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "/target\n.tsift/\n").unwrap();
        let result = init(dir.path()).unwrap();
        assert!(!result.gitignore_added);
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(content.matches(".tsift/").count(), 1);
    }

    #[test]
    fn resolve_project_dir_returns_dir_for_directory() {
        let dir = TempDir::new().unwrap();
        let resolved = resolve_project_dir(dir.path()).unwrap();
        // No git repo, so falls back to the directory itself
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn resolve_project_dir_returns_parent_for_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.md");
        std::fs::write(&file, "content").unwrap();
        let resolved = resolve_project_dir(&file).unwrap();
        // No git repo, falls back to parent dir
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn resolve_project_dir_finds_git_root() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
        // Init a git repo at the top
        Command::new("git")
            .args(["init", &dir.path().to_string_lossy()])
            .output()
            .unwrap();
        let resolved = resolve_project_dir(&sub).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_project_dir_finds_git_root_from_file() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("tasks");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("plan.md");
        std::fs::write(&file, "content").unwrap();
        Command::new("git")
            .args(["init", &dir.path().to_string_lossy()])
            .output()
            .unwrap();
        let resolved = resolve_project_dir(&file).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn init_preserves_surrounding_content() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(
            &agents,
            "# Header\n\nBefore content.\n\n## Footer\n\nAfter content.\n",
        )
        .unwrap();
        init(dir.path()).unwrap();
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains("# Header"));
        assert!(content.contains("Before content."));
        assert!(content.contains("## Footer"));
        assert!(content.contains("After content."));
        assert!(content.contains(SECTION_MARKER));
    }
}
