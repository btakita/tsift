use crate::config;
use anyhow::{Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

const SECTION_MARKER_PREFIX: &str = "<!-- tsift:code-navigation";
const SECTION_END_MARKER: &str = "<!-- /tsift:code-navigation -->";
pub const TSIFT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn versioned_section() -> String {
    format!(
        r#"<!-- tsift:code-navigation v={version} -->
## Code Navigation

Run `tsift status` at session start from the owning repo root. If the task or file lives under a git submodule (for example `src/tsift/...`), switch to that submodule root first so the harness loads the narrower local instructions and repo state instead of the superproject root.

Use the commands listed in its `use:` output:
- `tsift search <query>` — AST-aware hybrid search (prefer over grep/rg)
- `tsift explain <symbol>` — callers, callees, community context
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)

Prefer bounded digest commands over raw transcript, diff, and verbose-log reads:
- `tsift session-digest <file>` / `tsift session-review <path>` instead of replaying long session docs, JSONL transcripts, or agent-doc runtime logs with `cat`, `tail`, or `sed`.
- `tsift diff-digest [path]` (`--cached`, `--revision <rev>`) instead of `git diff`, `git show`, or patch-style `git log`.
- `tsift test-digest --path .` / `tsift log-digest --path .` for noisy test/build/install output, or let the rewrite/hooks wrap `cargo test`, `pytest`, and verbose cargo commands for you.
- If your harness does not support Claude-style `PreToolUse` hooks, run `tsift rewrite --run '<command>'` to execute the same digest-first/bounded tsift equivalent manually.

Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->"#,
        version = TSIFT_VERSION
    )
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state")]
pub enum InstructionStatus {
    #[serde(rename = "current")]
    Current { version: String },
    #[serde(rename = "stale")]
    Stale {
        found: Option<String>,
        expected: String,
    },
    #[serde(rename = "missing")]
    Missing,
}

const GITIGNORE_ENTRY: &str = ".tsift/";
const CODEX_HOOK_STATUS: &str = "tsift auto-reindex";

pub struct InitResult {
    pub updates: Vec<InstructionUpdate>,
    pub gitignore_added: bool,
    pub codex_hooks: Option<CodexHooksResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodexHooksResult {
    pub action: CodexHookAction,
    pub scope: CodexHookScope,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexHookAction {
    Created,
    Added,
    Updated,
    AlreadyPresent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexHookScope {
    Project,
    Workspace,
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
    let dir = input_dir(path)?;

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

pub fn resolve_workspace_dir(path: &Path) -> Result<PathBuf> {
    let dir = input_dir(path)?;

    let output = Command::new("git")
        .args([
            "-C",
            &dir.to_string_lossy(),
            "rev-parse",
            "--show-superproject-working-tree",
        ])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let root = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !root.is_empty() {
                return Ok(PathBuf::from(root));
            }
        }
        _ => {}
    }

    resolve_project_dir(path)
}

pub fn has_submodules(dir: &Path) -> Result<bool> {
    Ok(!config::Config::submodule_dirs(dir)?.is_empty())
}

pub fn init(dir: &Path, codex: bool, codex_workspace: bool) -> Result<InitResult> {
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
        let scope = if codex_workspace {
            if !has_submodules(dir)? {
                bail!("no submodules found in {}", dir.display());
            }
            CodexHookScope::Workspace
        } else {
            CodexHookScope::Project
        };
        Some(ensure_codex_hooks(dir, scope)?)
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
    let section = versioned_section();
    if !file.exists() {
        std::fs::write(file, format!("{}\n", section))?;
        return Ok(InitAction::Created);
    }

    let content = std::fs::read_to_string(file)?;

    if content.contains(SECTION_MARKER_PREFIX) {
        let start = content.find(SECTION_MARKER_PREFIX).unwrap();
        if let Some(end_rel) = content[start..].find(SECTION_END_MARKER) {
            let end = start + end_rel + SECTION_END_MARKER.len();
            let before = &content[..start];
            let after = &content[end..];
            let new_content = format!("{}{}{}", before, section, after);
            if new_content == content {
                return Ok(InitAction::AlreadyPresent);
            }
            std::fs::write(file, new_content)?;
            return Ok(InitAction::Updated);
        } else {
            bail!(
                "Found {} in {} but no matching {} — fix manually",
                SECTION_MARKER_PREFIX,
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
    new_content.push_str(&section);
    new_content.push('\n');
    std::fs::write(file, new_content)?;

    Ok(InitAction::Created)
}

fn ensure_codex_hooks(dir: &Path, scope: CodexHookScope) -> Result<CodexHooksResult> {
    let codex_dir = dir.join(".codex");
    let hooks_path = codex_dir.join("hooks.json");
    let tsift_hook = codex_hook_json(dir, scope);

    if hooks_path.exists() {
        let content = std::fs::read_to_string(&hooks_path)?;
        let mut doc: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| anyhow::anyhow!("invalid .codex/hooks.json: {}", e))?;

        let hooks_obj = doc
            .as_object_mut()
            .and_then(|o| o.get_mut("hooks"))
            .and_then(|h| h.as_object_mut());

        let Some(hooks_obj) = hooks_obj else {
            bail!(".codex/hooks.json has unexpected structure (missing \"hooks\" object)");
        };

        let event_key = "UserPromptSubmit";
        let mut action = CodexHookAction::Added;

        if let Some(event_arr) = hooks_obj.get_mut(event_key).and_then(|v| v.as_array_mut()) {
            let mut matched = false;
            for group in event_arr.iter_mut() {
                if let Some(hook_list) = group
                    .as_object_mut()
                    .and_then(|g| g.get_mut("hooks"))
                    .and_then(|h| h.as_array_mut())
                {
                    let mut i = 0;
                    while i < hook_list.len() {
                        let is_tsift = hook_list[i].get("statusMessage").and_then(|v| v.as_str())
                            == Some(CODEX_HOOK_STATUS);
                        if !is_tsift {
                            i += 1;
                            continue;
                        }

                        if !matched {
                            matched = true;
                            if hook_list[i] == tsift_hook {
                                action = CodexHookAction::AlreadyPresent;
                                i += 1;
                                continue;
                            }
                            hook_list[i] = tsift_hook.clone();
                            action = CodexHookAction::Updated;
                            i += 1;
                        } else {
                            hook_list.remove(i);
                            action = CodexHookAction::Updated;
                        }
                    }
                }
            }

            if !matched {
                if let Some(first_group) = event_arr.first_mut().and_then(|g| g.as_object_mut()) {
                    if let Some(hook_list) =
                        first_group.get_mut("hooks").and_then(|h| h.as_array_mut())
                    {
                        hook_list.push(tsift_hook);
                    } else {
                        first_group.insert("hooks".to_string(), serde_json::json!([tsift_hook]));
                    }
                } else {
                    event_arr.push(serde_json::json!({"hooks": [tsift_hook]}));
                }
            }
        } else {
            hooks_obj.insert(
                event_key.to_string(),
                serde_json::json!([{"hooks": [tsift_hook]}]),
            );
        }

        if action == CodexHookAction::AlreadyPresent {
            return Ok(CodexHooksResult { action, scope });
        }

        let formatted = serde_json::to_string_pretty(&doc)?;
        std::fs::write(&hooks_path, format!("{}\n", formatted))?;
        Ok(CodexHooksResult { action, scope })
    } else {
        std::fs::create_dir_all(&codex_dir)?;
        let doc = serde_json::json!({"hooks": {"UserPromptSubmit": [{"hooks": [tsift_hook]}]}});
        let formatted = serde_json::to_string_pretty(&doc)?;
        std::fs::write(&hooks_path, format!("{}\n", formatted))?;
        Ok(CodexHooksResult {
            action: CodexHookAction::Created,
            scope,
        })
    }
}

fn input_dir(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("cannot determine parent of {}", path.display()))
            .map(Path::to_path_buf)
    } else {
        Ok(path.to_path_buf())
    }
}

fn codex_hook_json(dir: &Path, scope: CodexHookScope) -> serde_json::Value {
    serde_json::json!({
        "command": codex_hook_command(dir, scope),
        "statusMessage": CODEX_HOOK_STATUS,
        "type": "command"
    })
}

fn codex_hook_command(dir: &Path, scope: CodexHookScope) -> String {
    let quoted_dir = shell_quote(&dir.display().to_string());
    match scope {
        CodexHookScope::Project => format!(
            "tsift index --check --exit-code {} >/dev/null 2>&1 || tsift index {} >/dev/null 2>&1",
            quoted_dir, quoted_dir
        ),
        CodexHookScope::Workspace => format!(
            "tsift index --check --exit-code --workspace {} >/dev/null 2>&1 || tsift index --workspace {} >/dev/null 2>&1",
            quoted_dir, quoted_dir
        ),
    }
}

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        format!("\"{}\"", s)
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

pub fn extract_instruction_version(content: &str) -> Option<String> {
    let start = content.find(SECTION_MARKER_PREFIX)?;
    let rest = &content[start + SECTION_MARKER_PREFIX.len()..];
    let close = rest.find("-->")?;
    let tag_content = rest[..close].trim();
    tag_content.strip_prefix("v=").map(|v| v.to_string())
}

pub fn check_instruction_version(dir: &Path) -> InstructionStatus {
    let agents = dir.join("AGENTS.md");
    let file = if agents.exists() {
        agents
    } else {
        let claude = dir.join("CLAUDE.md");
        if claude.exists() {
            claude
        } else {
            return InstructionStatus::Missing;
        }
    };
    let content = match std::fs::read_to_string(&file) {
        Ok(c) => c,
        Err(_) => return InstructionStatus::Missing,
    };
    if !content.contains(SECTION_MARKER_PREFIX) {
        return InstructionStatus::Missing;
    }
    match extract_instruction_version(&content) {
        Some(v) if v == TSIFT_VERSION => InstructionStatus::Current { version: v },
        Some(v) => InstructionStatus::Stale {
            found: Some(v),
            expected: TSIFT_VERSION.to_string(),
        },
        None => InstructionStatus::Stale {
            found: None,
            expected: TSIFT_VERSION.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn init_creates_agents_md_when_none_exists() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(result.updates.len(), 1);
        assert!(matches!(result.updates[0].action, InitAction::Created));
        assert_eq!(result.updates[0].file.file_name().unwrap(), "AGENTS.md");
        let content = std::fs::read_to_string(&result.updates[0].file).unwrap();
        assert!(content.contains(SECTION_MARKER_PREFIX));
        assert!(content.contains("tsift search"));
    }

    #[test]
    fn init_appends_to_existing_agents_md() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "# My Project\n\nSome instructions.\n").unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(result.updates.len(), 1);
        assert!(matches!(result.updates[0].action, InitAction::Created));
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.starts_with("# My Project"));
        assert!(content.contains(SECTION_MARKER_PREFIX));
    }

    #[test]
    fn init_updates_agents_and_claude_when_both_exist() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agents\n").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# Claude\n").unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(result.updates.len(), 2);
        assert_eq!(result.updates[0].file.file_name().unwrap(), "AGENTS.md");
        assert_eq!(result.updates[1].file.file_name().unwrap(), "CLAUDE.md");
        assert!(
            std::fs::read_to_string(dir.path().join("AGENTS.md"))
                .unwrap()
                .contains(SECTION_MARKER_PREFIX)
        );
        assert!(
            std::fs::read_to_string(dir.path().join("CLAUDE.md"))
                .unwrap()
                .contains(SECTION_MARKER_PREFIX)
        );
    }

    #[test]
    fn init_creates_agents_and_updates_claude_when_only_claude_exists() {
        let dir = TempDir::new().unwrap();
        let claude = dir.path().join("CLAUDE.md");
        std::fs::write(&claude, "# Claude\n").unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(result.updates.len(), 2);
        assert_eq!(result.updates[0].file.file_name().unwrap(), "AGENTS.md");
        assert!(dir.path().join("AGENTS.md").exists());
        assert!(matches!(result.updates[0].action, InitAction::Created));
        assert!(matches!(result.updates[1].action, InitAction::Created));
        assert!(
            std::fs::read_to_string(claude)
                .unwrap()
                .contains(SECTION_MARKER_PREFIX)
        );
    }

    #[test]
    fn init_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "# Project\n").unwrap();

        let r1 = init(dir.path(), false, false).unwrap();
        assert!(matches!(r1.updates[0].action, InitAction::Created));
        let content_after_first = std::fs::read_to_string(&agents).unwrap();

        let r2 = init(dir.path(), false, false).unwrap();
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
            "<!-- tsift:code-navigation v=0.0.1 -->", SECTION_END_MARKER
        );
        std::fs::write(&agents, format!("# Project\n\n{}\n", old_section)).unwrap();

        let result = init(dir.path(), false, false).unwrap();
        assert!(matches!(result.updates[0].action, InitAction::Updated));
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains("tsift search"));
        assert!(!content.contains("Old content here."));
        assert_eq!(content.matches(SECTION_MARKER_PREFIX).count(), 1);
    }

    #[test]
    fn init_creates_gitignore_with_tsift_entry() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert!(result.gitignore_added);
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains(".tsift/"));
    }

    #[test]
    fn init_appends_to_existing_gitignore() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "/target\n").unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert!(result.gitignore_added);
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(content.contains("/target"));
        assert!(content.contains(".tsift/"));
    }

    #[test]
    fn init_skips_gitignore_when_already_present() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "/target\n.tsift/\n").unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert!(!result.gitignore_added);
        let content = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(content.matches(".tsift/").count(), 1);
    }

    #[test]
    fn resolve_project_dir_returns_dir_for_directory() {
        let dir = TempDir::new().unwrap();
        let resolved = resolve_project_dir(dir.path()).unwrap();
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn resolve_project_dir_returns_parent_for_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.md");
        std::fs::write(&file, "content").unwrap();
        let resolved = resolve_project_dir(&file).unwrap();
        assert_eq!(resolved, dir.path());
    }

    #[test]
    fn resolve_project_dir_finds_git_root() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub").join("deep");
        std::fs::create_dir_all(&sub).unwrap();
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
    fn resolve_workspace_dir_falls_back_to_project_dir() {
        let dir = TempDir::new().unwrap();
        Command::new("git")
            .args(["init", &dir.path().to_string_lossy()])
            .output()
            .unwrap();
        let resolved = resolve_workspace_dir(dir.path()).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn has_submodules_reads_gitmodules() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"src/foo\"]\n\tpath = src/foo\n",
        )
        .unwrap();
        assert!(has_submodules(dir.path()).unwrap());
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
        init(dir.path(), false, false).unwrap();
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains("# Header"));
        assert!(content.contains("Before content."));
        assert!(content.contains("## Footer"));
        assert!(content.contains("After content."));
        assert!(content.contains(SECTION_MARKER_PREFIX));
    }

    #[test]
    fn init_codex_creates_hooks_json() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), true, false).unwrap();
        assert_eq!(
            result.codex_hooks,
            Some(CodexHooksResult {
                action: CodexHookAction::Created,
                scope: CodexHookScope::Project,
            })
        );
        let hooks_path = dir.path().join(".codex/hooks.json");
        assert!(hooks_path.exists());
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let hooks = &doc["hooks"]["UserPromptSubmit"][0]["hooks"];
        let command = hooks[0]["command"].as_str().unwrap();
        assert_eq!(hooks[0]["statusMessage"], CODEX_HOOK_STATUS);
        assert!(command.contains("tsift index --check --exit-code"));
        assert!(command.contains(&dir.path().display().to_string()));
        assert!(!command.contains("--workspace"));
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

        let result = init(dir.path(), true, false).unwrap();
        assert_eq!(
            result.codex_hooks,
            Some(CodexHooksResult {
                action: CodexHookAction::Added,
                scope: CodexHookScope::Project,
            })
        );
        let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let hooks = doc["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap();
        assert_eq!(hooks.len(), 2);
        assert_eq!(
            hooks[0]["statusMessage"],
            "Tracking active agent-doc session"
        );
        assert_eq!(hooks[1]["statusMessage"], CODEX_HOOK_STATUS);
    }

    #[test]
    fn init_codex_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let r1 = init(dir.path(), true, false).unwrap();
        assert_eq!(
            r1.codex_hooks,
            Some(CodexHooksResult {
                action: CodexHookAction::Created,
                scope: CodexHookScope::Project,
            })
        );

        let r2 = init(dir.path(), true, false).unwrap();
        assert_eq!(
            r2.codex_hooks,
            Some(CodexHooksResult {
                action: CodexHookAction::AlreadyPresent,
                scope: CodexHookScope::Project,
            })
        );

        let content = std::fs::read_to_string(dir.path().join(".codex/hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let hooks = doc["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap();
        assert_eq!(hooks.len(), 1);
    }

    #[test]
    fn init_codex_updates_existing_project_hook_command() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let existing = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [{
                        "command": "tsift index --check --exit-code . >/dev/null 2>&1 || tsift index . >/dev/null 2>&1",
                        "statusMessage": CODEX_HOOK_STATUS,
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

        let result = init(dir.path(), true, false).unwrap();
        assert_eq!(
            result.codex_hooks,
            Some(CodexHooksResult {
                action: CodexHookAction::Updated,
                scope: CodexHookScope::Project,
            })
        );
        let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let command = doc["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains(&dir.path().display().to_string()));
        assert!(!command.contains("--workspace"));
    }

    #[test]
    fn init_codex_deduplicates_existing_tsift_hooks() {
        let dir = TempDir::new().unwrap();
        let codex_dir = dir.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let existing = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [
                        {
                            "command": "tsift index --check --exit-code . >/dev/null 2>&1 || tsift index . >/dev/null 2>&1",
                            "statusMessage": CODEX_HOOK_STATUS,
                            "type": "command"
                        },
                        {
                            "command": "tsift index --check --exit-code . >/dev/null 2>&1 || tsift index . >/dev/null 2>&1",
                            "statusMessage": CODEX_HOOK_STATUS,
                            "type": "command"
                        }
                    ]
                }]
            }
        });
        std::fs::write(
            codex_dir.join("hooks.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let result = init(dir.path(), true, false).unwrap();
        assert_eq!(
            result.codex_hooks,
            Some(CodexHooksResult {
                action: CodexHookAction::Updated,
                scope: CodexHookScope::Project,
            })
        );
        let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let hooks = doc["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap();
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

        let result = init(dir.path(), true, false).unwrap();
        assert_eq!(
            result.codex_hooks,
            Some(CodexHooksResult {
                action: CodexHookAction::Added,
                scope: CodexHookScope::Project,
            })
        );
        let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(doc["hooks"]["Stop"].is_array());
        let ups_hooks = doc["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap();
        assert_eq!(ups_hooks.len(), 1);
        assert_eq!(ups_hooks[0]["statusMessage"], CODEX_HOOK_STATUS);
    }

    #[test]
    fn init_codex_creates_workspace_hook_for_submodules() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"src/alpha\"]\n\tpath = src/alpha\n",
        )
        .unwrap();

        let result = init(dir.path(), true, true).unwrap();
        assert_eq!(
            result.codex_hooks,
            Some(CodexHooksResult {
                action: CodexHookAction::Created,
                scope: CodexHookScope::Workspace,
            })
        );
        let content = std::fs::read_to_string(dir.path().join(".codex/hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let command = doc["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("--workspace"));
        assert!(command.contains(&dir.path().display().to_string()));
    }

    #[test]
    fn init_codex_updates_project_hook_to_workspace_hook() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"src/alpha\"]\n\tpath = src/alpha\n",
        )
        .unwrap();
        let codex_dir = dir.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let existing = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [{
                        "command": "tsift index --check --exit-code . >/dev/null 2>&1 || tsift index . >/dev/null 2>&1",
                        "statusMessage": CODEX_HOOK_STATUS,
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

        let result = init(dir.path(), true, true).unwrap();
        assert_eq!(
            result.codex_hooks,
            Some(CodexHooksResult {
                action: CodexHookAction::Updated,
                scope: CodexHookScope::Workspace,
            })
        );
        let content = std::fs::read_to_string(codex_dir.join("hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&content).unwrap();
        let command = doc["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("--workspace"));
    }

    #[test]
    fn init_workspace_hook_requires_submodules() {
        let dir = TempDir::new().unwrap();
        let err = init(dir.path(), true, true).err().unwrap().to_string();
        assert!(err.contains("no submodules found"));
    }

    #[test]
    fn init_without_codex_flag_skips_hooks() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert!(result.codex_hooks.is_none());
        assert!(!dir.path().join(".codex/hooks.json").exists());
    }

    #[test]
    fn init_embeds_version_in_marker() {
        let dir = TempDir::new().unwrap();
        init(dir.path(), false, false).unwrap();
        let content = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        let expected_marker = format!("<!-- tsift:code-navigation v={} -->", TSIFT_VERSION);
        assert!(content.contains(&expected_marker));
        assert!(content.contains("tsift session-digest <file>"));
        assert!(content.contains("tsift diff-digest [path]"));
        assert!(content.contains("tsift test-digest --path ."));
        assert!(content.contains("tsift log-digest --path ."));
    }

    #[test]
    fn extract_version_from_versioned_marker() {
        let content = "# Project\n\n<!-- tsift:code-navigation v=1.2.3 -->\n## Code Navigation\n<!-- /tsift:code-navigation -->\n";
        assert_eq!(
            extract_instruction_version(content),
            Some("1.2.3".to_string())
        );
    }

    #[test]
    fn extract_version_returns_none_for_old_format() {
        let content =
            "<!-- tsift:code-navigation -->\n## Code Navigation\n<!-- /tsift:code-navigation -->\n";
        assert_eq!(extract_instruction_version(content), None);
    }

    #[test]
    fn extract_version_returns_none_when_no_section() {
        let content = "# Just a project\n\nNo tsift here.\n";
        assert_eq!(extract_instruction_version(content), None);
    }

    #[test]
    fn check_version_current_after_init() {
        let dir = TempDir::new().unwrap();
        init(dir.path(), false, false).unwrap();
        let status = check_instruction_version(dir.path());
        assert_eq!(
            status,
            InstructionStatus::Current {
                version: TSIFT_VERSION.to_string()
            }
        );
    }

    #[test]
    fn check_version_stale_for_older_version() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(
            &agents,
            "<!-- tsift:code-navigation v=0.0.1 -->\n## Code Navigation\nOld.\n<!-- /tsift:code-navigation -->\n",
        )
        .unwrap();
        let status = check_instruction_version(dir.path());
        assert_eq!(
            status,
            InstructionStatus::Stale {
                found: Some("0.0.1".to_string()),
                expected: TSIFT_VERSION.to_string(),
            }
        );
    }

    #[test]
    fn check_version_stale_for_pre_versioned() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(
            &agents,
            "<!-- tsift:code-navigation -->\n## Code Navigation\nOld.\n<!-- /tsift:code-navigation -->\n",
        )
        .unwrap();
        let status = check_instruction_version(dir.path());
        assert_eq!(
            status,
            InstructionStatus::Stale {
                found: None,
                expected: TSIFT_VERSION.to_string(),
            }
        );
    }

    #[test]
    fn check_version_missing_when_no_files() {
        let dir = TempDir::new().unwrap();
        let status = check_instruction_version(dir.path());
        assert_eq!(status, InstructionStatus::Missing);
    }

    #[test]
    fn check_version_missing_when_no_section() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Project\n").unwrap();
        let status = check_instruction_version(dir.path());
        assert_eq!(status, InstructionStatus::Missing);
    }

    #[test]
    fn init_upgrades_pre_versioned_section() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(
            &agents,
            "# Project\n\n<!-- tsift:code-navigation -->\n## Code Navigation\n\nOld content.\n<!-- /tsift:code-navigation -->\n",
        )
        .unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert!(matches!(result.updates[0].action, InitAction::Updated));
        let content = std::fs::read_to_string(&agents).unwrap();
        let expected_marker = format!("<!-- tsift:code-navigation v={} -->", TSIFT_VERSION);
        assert!(content.contains(&expected_marker));
        assert!(!content.contains("Old content."));
    }

    #[test]
    fn check_version_prefers_agents_over_claude() {
        let dir = TempDir::new().unwrap();
        init(dir.path(), false, false).unwrap();
        std::fs::write(
            dir.path().join("CLAUDE.md"),
            "<!-- tsift:code-navigation v=0.0.1 -->\n## Code Navigation\nOld.\n<!-- /tsift:code-navigation -->\n",
        )
        .unwrap();
        let status = check_instruction_version(dir.path());
        assert_eq!(
            status,
            InstructionStatus::Current {
                version: TSIFT_VERSION.to_string()
            }
        );
    }
}
