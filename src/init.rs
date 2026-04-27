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

const CODEX_HOOK_COMMAND: &str = "tsift index --check --exit-code . >/dev/null 2>&1 || tsift index . >/dev/null 2>&1";
const CODEX_HOOK_STATUS: &str = "tsift auto-reindex";

pub struct InitResult {
    pub updates: Vec<InstructionUpdate>,
    pub gitignore_added: bool,
    pub codex_hooks: Option<CodexHooksResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexHooksResult {
    Created,
    Added,
    AlreadyPresent,
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

pub fn init(dir: &Path, codex: bool) -> Result<InitResult> {
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

    let codex_hooks = if codex {
        Some(ensure_codex_hooks(dir)?)
    } else {
        None
    };

    Ok(InitResult {
        updates,
        gitignore_added,
        codex_hooks,
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

fn ensure_codex_hooks(dir: &Path) -> Result<CodexHooksResult> {
    let codex_dir = dir.join(".codex");
    let hooks_path = codex_dir.join("hooks.json");

    if hooks_path.exists() {
        let content = std::fs::read_to_string(&hooks_path)?;
        let mut doc: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| anyhow::anyhow!("invalid .codex/hooks.json: {}", e))?;

        if content.contains(CODEX_HOOK_STATUS) {
            return Ok(CodexHooksResult::AlreadyPresent);
        }

        let hooks_obj = doc
            .as_object_mut()
            .and_then(|o| o.get_mut("hooks"))
            .and_then(|h| h.as_object_mut());

        if let Some(hooks_obj) = hooks_obj {
            let tsift_hook = serde_json::json!({
                "command": CODEX_HOOK_COMMAND,
                "statusMessage": CODEX_HOOK_STATUS,
                "type": "command"
            });

            let event_key = "UserPromptSubmit";
            if let Some(event_arr) = hooks_obj.get_mut(event_key).and_then(|v| v.as_array_mut()) {
                if let Some(first_group) = event_arr.first_mut().and_then(|g| g.as_object_mut()) {
                    if let Some(hook_list) = first_group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                        hook_list.push(tsift_hook);
                    } else {
                        first_group.insert("hooks".to_string(), serde_json::json!([tsift_hook]));
                    }
                } else {
                    event_arr.push(serde_json::json!({"hooks": [tsift_hook]}));
                }
            } else {
                hooks_obj.insert(
                    event_key.to_string(),
                    serde_json::json!([{"hooks": [tsift_hook]}]),
                );
            }
        } else {
            bail!(".codex/hooks.json has unexpected structure (missing \"hooks\" object)");
        }

        let formatted = serde_json::to_string_pretty(&doc)?;
        std::fs::write(&hooks_path, format!("{}\n", formatted))?;
        Ok(CodexHooksResult::Added)
    } else {
        std::fs::create_dir_all(&codex_dir)?;
        let doc = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [{
                        "command": CODEX_HOOK_COMMAND,
                        "statusMessage": CODEX_HOOK_STATUS,
                        "type": "command"
                    }]
                }]
            }
        });
        let formatted = serde_json::to_string_pretty(&doc)?;
        std::fs::write(&hooks_path, format!("{}\n", formatted))?;
        Ok(CodexHooksResult::Created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_creates_agents_md_when_none_exists() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), false).unwrap();
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
        let result = init(dir.path(), false).unwrap();
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
        let result = init(dir.path(), false).unwrap();
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
        let result = init(dir.path(), false).unwrap();
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

        let r1 = init(dir.path(), false).unwrap();
        assert!(matches!(r1.updates[0].action, InitAction::Created));
        let content_after_first = std::fs::read_to_string(&agents).unwrap();

        let r2 = init(dir.path(), false).unwrap();
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

        let result = init(dir.path(), false).unwrap();
        assert!(matches!(result.updates[0].action, InitAction::Updated));
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains("tsift search"));
        assert!(!content.contains("Old content here."));
        assert_eq!(content.matches(SECTION_MARKER).count(), 1);
    }

    #[test]
    fn init_creates_gitignore_with_tsift_entry() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), false).unwrap();
        assert!(result.gitignore_added);
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(".tsift/"));
    }

    #[test]
    fn init_appends_to_existing_gitignore() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "/target\n").unwrap();
        let result = init(dir.path(), false).unwrap();
        assert!(result.gitignore_added);
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains("/target"));
        assert!(content.contains(".tsift/"));
    }

    #[test]
    fn init_skips_gitignore_when_already_present() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "/target\n.tsift/\n").unwrap();
        let result = init(dir.path(), false).unwrap();
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
        init(dir.path(), false).unwrap();
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains("# Header"));
        assert!(content.contains("Before content."));
        assert!(content.contains("## Footer"));
        assert!(content.contains("After content."));
        assert!(content.contains(SECTION_MARKER));
    }

    #[test]
    fn init_codex_creates_hooks_json() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), true).unwrap();
        assert_eq!(result.codex_hooks, Some(CodexHooksResult::Created));
        let hooks_path = dir.path().join(".codex/hooks.json");
        assert!(hooks_path.exists());
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let hooks = &doc["hooks"]["UserPromptSubmit"][0]["hooks"];
        assert_eq!(hooks[0]["statusMessage"], CODEX_HOOK_STATUS);
        assert!(hooks[0]["command"].as_str().unwrap().contains("tsift index"));
    }

    #[test]
    fn init_codex_merges_into_existing_hooks() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let existing = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [{
                        "command": "agent-doc hook codex-user-prompt-submit",
                        "statusMessage": "Tracking active agent-doc session",
                        "type": "command"
                    }]
                }]
            }
        });
        std::fs::write(
            codex_dir.join("hooks.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let result = init(dir.path(), true).unwrap();
        assert_eq!(result.codex_hooks, Some(CodexHooksResult::Added));
        let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let hooks = doc["hooks"]["UserPromptSubmit"][0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0]["statusMessage"], "Tracking active agent-doc session");
        assert_eq!(hooks[1]["statusMessage"], CODEX_HOOK_STATUS);
    }

    #[test]
    fn init_codex_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let r1 = init(dir.path(), true).unwrap();
        assert_eq!(r1.codex_hooks, Some(CodexHooksResult::Created));

        let r2 = init(dir.path(), true).unwrap();
        assert_eq!(r2.codex_hooks, Some(CodexHooksResult::AlreadyPresent));

        let content = std::fs::read_to_string(dir.path().join(".codex/hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let hooks = doc["hooks"]["UserPromptSubmit"][0]["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
    }

    #[test]
    fn init_codex_adds_to_stop_only_hooks() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let existing = serde_json::json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{
                        "command": "agent-doc hook codex-stop",
                        "statusMessage": "Checking agent-doc completion boundary",
                        "type": "command"
                    }]
                }]
            }
        });
        std::fs::write(
            codex_dir.join("hooks.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let result = init(dir.path(), true).unwrap();
        assert_eq!(result.codex_hooks, Some(CodexHooksResult::Added));
        let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(doc["hooks"]["Stop"].is_array());
        let ups_hooks = doc["hooks"]["UserPromptSubmit"][0]["hooks"].as_array().unwrap();
        assert_eq!(ups_hooks.len(), 1);
        assert_eq!(ups_hooks[0]["statusMessage"], CODEX_HOOK_STATUS);
    }

    #[test]
    fn init_without_codex_flag_skips_hooks() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), false).unwrap();
        assert!(result.codex_hooks.is_none());
        assert!(!dir.path().join(".codex/hooks.json").exists());
    }
}
