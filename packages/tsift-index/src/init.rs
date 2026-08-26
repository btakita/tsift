use crate::config;
use anyhow::{Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

// Trailing space is load-bearing: without it this prefix also matches
// `<!-- tsift:code-navigation-runbook ... -->`, and the block logic would
// claim the runbook's marker as its own.
const SECTION_MARKER_PREFIX: &str = "<!-- tsift:code-navigation ";
const SECTION_END_MARKER: &str = "<!-- /tsift:code-navigation -->";
const RUNBOOK_MARKER_PREFIX: &str = "<!-- tsift:code-navigation-runbook ";
const RUNBOOK_END_MARKER: &str = "<!-- /tsift:code-navigation-runbook -->";
pub const RUNBOOK_RELATIVE_PATH: &str = ".agent/runbooks/code-navigation.md";
const LEGACY_RUNBOOK_RELATIVE_PATH: &str = "runbooks/code-navigation.md";
pub const TSIFT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn versioned_section(dir: &Path) -> String {
    let verification = verification_paragraph(dir);
    format!(
        r#"<!-- tsift:code-navigation v={version} -->
## Code Navigation

Run `tsift status` at session start from the owning repo root. If the task or file lives under a git submodule (for example `src/tsift/...`), switch to that submodule root first so the harness loads the narrower local instructions and repo state instead of the superproject root. `tsift status` repairs the `.tsift/` index state it owns and never rewrites tracked files (`--no-fix` skips even that). If status reports stale or missing instructions, run `tsift init` to refresh the tracked Code Navigation block and runbook; it names every tracked file it rewrites or moves. When the harness cannot perform write commands, ask the user to run the printed `run:` command instead.

Prefer tsift envelopes over raw reads:
- `tsift --envelope search <query>` instead of `grep`/`rg`
- `tsift --envelope source-read <file>` / `tsift --envelope symbol-read <symbol>` instead of raw `cat`/`head`/`tail`/`sed`/`less` source reads
- `tsift --envelope explain <symbol>` and `tsift graph <symbol> --callers` / `--callees` for call graphs
- `tsift diff-digest [path]` (`--pathspec <pathspec>` to preserve scoped reviews) instead of `git diff`, commit-form `git show`, or patch-style `git log`; blob-form `git show <rev>:<path>` stays a raw object read
- `tsift --envelope session-review <path>` / `tsift --envelope context-pack <path>` instead of replaying long session docs or transcripts
- raw-read rewrites route recognized session docs/transcripts to `tsift session-digest --input <path>` and captured logs to `tsift log-digest --input <path>`
- `tsift --envelope digest-runner --kind test|log --path . --shell-command '<command>'` instead of raw test/build output

Command detail lives in [`{runbook}`]({runbook}) — budgets, `tsift workflow search`, `report.scale_guard` handling, the harness rewrite path for `PreToolUse`-less harnesses, and Codex/OpenCode integration. `tsift init` writes and versions that runbook alongside this block, so it is present in every initialized checkout; read it before broad exploration instead of expanding this block. A repository that also ships a current `.claude/skills/tsift/SKILL.md` should use that skill as the deeper source.

{verification}
Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation -->"#,
        version = TSIFT_VERSION,
        runbook = RUNBOOK_RELATIVE_PATH,
        verification = verification,
    )
}

fn versioned_runbook_section(dir: &Path) -> String {
    let verification = verification_runbook_section(dir);
    format!(
        r#"<!-- tsift:code-navigation-runbook v={version} -->
# Code Navigation

Managed by `tsift init` (versioned markers) — do not hand-edit between the markers; re-run `tsift init` to refresh. Text outside the markers is preserved.

This runbook is the detail behind the `Code Navigation` block in `AGENTS.md`. That block carries the hot path; everything below is the full command surface.

## Session start

Run `tsift status` from the owning repo root. If the task or file lives under a git submodule (for example `src/tsift/...`), switch to that submodule root first so the harness loads the narrower local instructions and repo state instead of the superproject root. `tsift status` repairs the `.tsift/` index state it owns and never rewrites tracked files (`--no-fix` skips even that). If status reports stale or missing instructions, run `tsift init` to refresh the tracked Code Navigation block and runbook; it names every tracked file it rewrites or moves. When the harness cannot perform write commands, ask the user to run the printed `run:` command instead.

Codex projects can install a prompt-time auto-reindex hook with `tsift init --codex`; OpenCode projects can install per-project tsift command shortcuts with `tsift init --opencode`.

## Search, read, and graph

Use the commands listed in `tsift status`'s `use:` output:

- `tsift --envelope source-read <file> --budget normal` — AST-symbol projection with span metadata and source-window expansion commands (prefer over raw cat/head/tail/sed/less reads for source files)
- `tsift --envelope symbol-read <symbol> --budget normal` — token-budgeted symbol body, AST span metadata, child refs, and graph/source expansion commands
- `tsift --envelope search <query> --budget normal` — AST-aware hybrid search preview (prefer over grep/rg)
- `tsift --envelope explain <symbol> --budget normal` — callers, callees, community preview
- `tsift graph <symbol> --callers` / `--callees` — call graph navigation
- `tsift summarize <symbol>` — cached summary (only when listed in `use:`)
- `tsift workflow search` — ordered exact/search/explain/summarize/digest recipe that preserves result handles across expansions

When a search envelope includes `report.scale_guard`, run one of its `narrow_commands` before dispatching parallel agents. The guard means the original result set or corpus is broad enough that fan-out should start from a narrower cited handle, path, or exact query.

## Bounded digests instead of raw output

Prefer bounded digest commands over raw transcript, diff, and verbose-log reads:

- `tsift --envelope session-review <path> --next-context` / `tsift --envelope context-pack <path>` when a resumable handoff is the goal
- raw-read rewrites route recognized agent-doc/JSONL sessions to `tsift session-digest --input <path>` and captured logs to `tsift log-digest --input <path>` instead of replaying them with `cat`, `head`, `tail`, `sed`, or `less`
- `tsift diff-digest [path]` (`--cached`, `--revision <rev>`, repeatable `--pathspec <pathspec>`) instead of `git diff`, commit-form `git show`, or patch-style `git log`. Blob-form `git show <rev>:<path>` is an object read and is deliberately not rewritten.
- `tsift --envelope digest-runner --kind test --path . --shell-command '<test command>'` / `tsift --envelope digest-runner --kind log --path . --shell-command '<build command>'` for noisy test/build/install output, or let the rewrite/hooks create those artifact-backed envelopes for `cargo test`, `pytest`, and verbose cargo commands.
- If RTK is installed, digest-runner delegates supported generic command families through `rtk rewrite` and records the chosen compact filter in `report.filter` while preserving tsift artifact handles.

## Harnesses without `PreToolUse` hooks

Codex, OpenCode, and other harnesses without Claude-style `PreToolUse` hooks should run `tsift rewrite --run '<command>'` before broad `rg`/recursive grep, raw transcript/session/log reads, `git diff`/`git show`/single-patch `git log`, `cargo test`/`pytest`, and cargo build/check/clippy/install commands so the same search, session-digest, diff-digest, and digest-runner rewrites apply manually. OpenCode can install this path as `/tsift-rewrite-run` with `tsift init --opencode`.

{verification}
Only read full source files when tsift results are insufficient.
<!-- /tsift:code-navigation-runbook -->"#,
        version = TSIFT_VERSION,
        verification = verification,
    )
}

fn verification_paragraph(dir: &Path) -> String {
    verification_guidance(dir)
        .map(|guidance| format!("{guidance}\n"))
        .unwrap_or_default()
}

fn verification_runbook_section(dir: &Path) -> String {
    verification_guidance(dir)
        .map(|guidance| format!("## Verification\n\n{guidance}\n"))
        .unwrap_or_default()
}

fn verification_guidance(dir: &Path) -> Option<String> {
    verification_guidance_with(dir, command_on_path)
}

fn verification_guidance_with(dir: &Path, command_exists: impl Fn(&str) -> bool) -> Option<String> {
    let local = if has_make_check_target(dir) {
        Some("For local verification, run `make check` before committing.".to_string())
    } else {
        detected_just_recipe(dir)
            .map(|recipe| format!("For local verification, run `just {recipe}` before committing."))
    };

    let ci = if dir.join(".github/workflows").is_dir() && command_exists("gh") {
        Some("After local changes, check the latest GitHub Actions CI run with `gh run list --limit 1` and fix any failing tests before calling the work complete.".to_string())
    } else if dir.join(".gitlab-ci.yml").is_file() && command_exists("glab") {
        Some("After local changes, check the latest GitLab CI pipeline with `glab ci status` and fix any failing tests before calling the work complete.".to_string())
    } else {
        None
    };

    match (local, ci) {
        (Some(local), Some(ci)) => Some(format!("{local} {ci}")),
        (Some(local), None) => Some(local),
        (None, Some(ci)) => Some(ci),
        (None, None) => None,
    }
}

fn has_make_check_target(dir: &Path) -> bool {
    ["GNUmakefile", "Makefile", "makefile"]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|content| recipe_exists(&content, "check"))
}

fn detected_just_recipe(dir: &Path) -> Option<&'static str> {
    let content = ["justfile", "Justfile", ".justfile"]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
        .and_then(|path| std::fs::read_to_string(path).ok())?;
    ["check", "test", "verify"]
        .into_iter()
        .find(|recipe| recipe_exists(&content, recipe))
}

fn recipe_exists(content: &str, recipe: &str) -> bool {
    content.lines().any(|line| {
        if line.chars().next().is_some_and(char::is_whitespace) {
            return false;
        }
        let Some((targets, _)) = line.split_once(':') else {
            return false;
        };
        targets.split_whitespace().any(|target| target == recipe)
    })
}

fn command_on_path(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| executable_candidate_exists(&dir.join(command)))
}

fn executable_candidate_exists(candidate: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(candidate)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(windows)]
    {
        if candidate.is_file() {
            return true;
        }
        ["exe", "cmd", "bat", "com"]
            .into_iter()
            .any(|extension| candidate.with_extension(extension).is_file())
    }

    #[cfg(not(any(unix, windows)))]
    {
        candidate.is_file()
    }
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
const GITIGNORE_PROBE: &str = ".tsift/.tsift-ignore-probe";
const CODEX_HOOK_STATUS: &str = "tsift auto-reindex";
const CODEX_AUTOINDEX_HELPER: &str = "tsift-autoindex.sh";
const CODEX_AUTOINDEX_HELPER_VERSION: u32 = 3;
const OPENCODE_COMMAND_MARKER_PREFIX: &str = "<!-- tsift:opencode-command";

pub struct InitResult {
    pub updates: Vec<InstructionUpdate>,
    /// The legacy runbook path this run relocated, when a migration happened.
    /// Reported so a tracked-file move is never a silent diff.
    pub migrated_runbook: Option<RunbookMigration>,
    pub gitignore_added: bool,
    /// The effective Git exclude source that already ignored `.tsift/`, when
    /// initialization intentionally left the tracked `.gitignore` untouched.
    pub gitignore_ignore_source: Option<String>,
    pub codex_hooks: Option<CodexHooksResult>,
    pub opencode_commands: Option<Vec<OpenCodeCommandUpdate>>,
}

/// A one-time relocation of the managed code-navigation runbook. Both paths are
/// relative to the initialized project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunbookMigration {
    pub from: &'static str,
    pub to: &'static str,
}

/// Flags tsift has deprecated but that must never appear in generated
/// instruction surfaces. A release that deprecates a flag adds it here, and the
/// template gate below fails until the templates stop teaching it.
pub const DEPRECATED_FLAG_USAGES: &[&str] = &["tsift status --fix"];

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeCommandUpdate {
    pub file: PathBuf,
    pub command_name: &'static str,
    pub action: InitAction,
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
    /// A duplicate managed section was found in a file that already defers to
    /// `AGENTS.md`, and was removed rather than refreshed.
    Removed,
    /// The file defers to `AGENTS.md`, so no managed section was injected.
    Deferred,
}

impl std::fmt::Display for InitAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitAction::Created => write!(f, "created"),
            InitAction::Updated => write!(f, "updated"),
            InitAction::AlreadyPresent => write!(f, "already present"),
            InitAction::Removed => write!(f, "removed"),
            InitAction::Deferred => write!(f, "deferred"),
        }
    }
}

fn effective_gitignore_source(dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "check-ignore",
            "--verbose",
            "--no-index",
            "--",
            GITIGNORE_PROBE,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let line = std::str::from_utf8(&output.stdout).ok()?.trim();
    let metadata = line.split_once('\t').map_or(line, |(metadata, _)| metadata);
    let mut fields = metadata.rsplitn(3, ':');
    let _pattern = fields.next()?;
    let _line_number = fields.next()?;
    let source = fields.next()?.trim();
    (!source.is_empty()).then(|| source.to_string())
}

fn ensure_gitignore(dir: &Path) -> Result<(bool, Option<String>)> {
    let gitignore = dir.join(".gitignore");
    if gitignore.exists() {
        let content = std::fs::read_to_string(&gitignore)?;
        if content.lines().any(|line| line.trim() == GITIGNORE_ENTRY) {
            return Ok((false, None));
        }
    }

    if let Some(source) = effective_gitignore_source(dir) {
        return Ok((false, Some(source)));
    }

    if gitignore.exists() {
        let content = std::fs::read_to_string(&gitignore)?;
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
    Ok((true, None))
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
    resolve_project_dir(path)
}

pub fn has_submodules(dir: &Path) -> Result<bool> {
    Ok(!config::Config::submodule_dirs(dir)?.is_empty())
}

pub fn init(dir: &Path, codex: bool, codex_workspace: bool) -> Result<InitResult> {
    init_with_integrations(dir, codex, codex_workspace, false)
}

pub fn init_with_integrations(
    dir: &Path,
    codex: bool,
    codex_workspace: bool,
    opencode: bool,
) -> Result<InitResult> {
    let (gitignore_added, gitignore_ignore_source) = ensure_gitignore(dir)?;
    let mut updates = Vec::new();
    let runbook_migrated = migrate_legacy_runbook(dir)?;

    let agents = dir.join("AGENTS.md");
    updates.push(InstructionUpdate {
        file: agents.clone(),
        action: ensure_instruction_file(&agents, dir)?,
    });

    let runbook = dir.join(RUNBOOK_RELATIVE_PATH);
    let runbook_action = ensure_runbook_file(&runbook, dir)?;
    updates.push(InstructionUpdate {
        file: runbook.clone(),
        action: if runbook_migrated.is_some() && runbook_action == InitAction::AlreadyPresent {
            InitAction::Updated
        } else {
            runbook_action
        },
    });

    let claude = dir.join("CLAUDE.md");
    if claude.exists() {
        let action = match claude_deference(&claude, &agents)? {
            // A symlink to AGENTS.md is the same bytes; rewriting through it
            // would strip the canonical section out of AGENTS.md itself.
            Some(Deference::SameFile) => InitAction::Deferred,
            Some(Deference::Import) => remove_instruction_section(&claude)?,
            None => ensure_instruction_file(&claude, dir)?,
        };
        updates.push(InstructionUpdate {
            file: claude.clone(),
            action,
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
        let policy = resolve_codex_autoindex_policy(dir, scope)?;
        Some(ensure_codex_hooks(dir, scope, &policy)?)
    } else {
        None
    };

    let opencode_commands = if opencode {
        Some(ensure_opencode_commands(dir)?)
    } else {
        None
    };

    Ok(InitResult {
        updates,
        migrated_runbook: runbook_migrated,
        gitignore_added,
        gitignore_ignore_source,
        codex_hooks,
        opencode_commands,
    })
}

fn migrate_legacy_runbook(dir: &Path) -> Result<Option<RunbookMigration>> {
    let legacy = dir.join(LEGACY_RUNBOOK_RELATIVE_PATH);
    let canonical = dir.join(RUNBOOK_RELATIVE_PATH);
    if !legacy.exists() || canonical.exists() {
        return Ok(None);
    }
    if let Some(parent) = canonical.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&legacy, &canonical)?;
    if let Some(parent) = legacy.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(Some(RunbookMigration {
        from: LEGACY_RUNBOOK_RELATIVE_PATH,
        to: RUNBOOK_RELATIVE_PATH,
    }))
}

/// How `CLAUDE.md` defers to `AGENTS.md`, when it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Deference {
    /// `CLAUDE.md` and `AGENTS.md` resolve to the same file (symlink or hardlink).
    SameFile,
    /// `CLAUDE.md` pulls `AGENTS.md` in with a Claude Code `@AGENTS.md` import.
    Import,
}

fn claude_deference(claude: &Path, agents: &Path) -> Result<Option<Deference>> {
    if let (Ok(a), Ok(c)) = (std::fs::canonicalize(agents), std::fs::canonicalize(claude))
        && a == c
    {
        return Ok(Some(Deference::SameFile));
    }

    let content = std::fs::read_to_string(claude)?;
    let imports_agents = content.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == "@AGENTS.md" || trimmed == "@./AGENTS.md"
    });
    Ok(imports_agents.then_some(Deference::Import))
}

/// Strip a managed Code Navigation section out of a file that already inherits
/// it from `AGENTS.md`, so the same instructions are not repeated in both.
fn remove_instruction_section(file: &Path) -> Result<InitAction> {
    let content = std::fs::read_to_string(file)?;
    let Some(start) = content.find(SECTION_MARKER_PREFIX) else {
        return Ok(InitAction::Deferred);
    };
    let Some(end_rel) = content[start..].find(SECTION_END_MARKER) else {
        bail!(
            "Found {} in {} but no matching {} — fix manually",
            SECTION_MARKER_PREFIX,
            file.display(),
            SECTION_END_MARKER
        );
    };
    let end = start + end_rel + SECTION_END_MARKER.len();
    let before = content[..start].trim_end();
    let after = content[end..].trim_start_matches('\n');

    let mut new_content = String::with_capacity(content.len());
    new_content.push_str(before);
    if !before.is_empty() {
        new_content.push('\n');
    }
    if !after.is_empty() {
        if !before.is_empty() {
            new_content.push('\n');
        }
        new_content.push_str(after);
    }
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    std::fs::write(file, new_content)?;
    Ok(InitAction::Removed)
}

fn ensure_runbook_file(file: &Path, dir: &Path) -> Result<InitAction> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let section = versioned_runbook_section(dir);
    if !file.exists() {
        std::fs::write(file, format!("{}\n", section))?;
        return Ok(InitAction::Created);
    }

    let content = std::fs::read_to_string(file)?;
    let Some(start) = content.find(RUNBOOK_MARKER_PREFIX) else {
        let mut new_content = content;
        if !new_content.ends_with('\n') {
            new_content.push('\n');
        }
        new_content.push('\n');
        new_content.push_str(&section);
        new_content.push('\n');
        std::fs::write(file, new_content)?;
        return Ok(InitAction::Created);
    };
    let Some(end_rel) = content[start..].find(RUNBOOK_END_MARKER) else {
        bail!(
            "Found {} in {} but no matching {} — fix manually",
            RUNBOOK_MARKER_PREFIX,
            file.display(),
            RUNBOOK_END_MARKER
        );
    };
    let end = start + end_rel + RUNBOOK_END_MARKER.len();
    let new_content = format!("{}{}{}", &content[..start], section, &content[end..]);
    if new_content == content {
        return Ok(InitAction::AlreadyPresent);
    }
    std::fs::write(file, new_content)?;
    Ok(InitAction::Updated)
}

fn ensure_instruction_file(file: &Path, dir: &Path) -> Result<InitAction> {
    let section = versioned_section(dir);
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

fn ensure_codex_hooks(
    dir: &Path,
    scope: CodexHookScope,
    policy: &CodexAutoindexPolicy,
) -> Result<CodexHooksResult> {
    let codex_dir = dir.join(".codex");
    std::fs::create_dir_all(&codex_dir)?;
    let helper_changed = ensure_codex_autoindex_helper(dir, scope, policy)?;
    let hooks_path = codex_dir.join("hooks.json");
    let tsift_hook = codex_hook_json(dir);

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

        if action == CodexHookAction::AlreadyPresent && !helper_changed {
            return Ok(CodexHooksResult { action, scope });
        }
        if action == CodexHookAction::AlreadyPresent {
            action = CodexHookAction::Updated;
        }

        let formatted = serde_json::to_string_pretty(&doc)?;
        std::fs::write(&hooks_path, format!("{}\n", formatted))?;
        Ok(CodexHooksResult { action, scope })
    } else {
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

fn codex_hook_json(dir: &Path) -> serde_json::Value {
    serde_json::json!({
        "command": codex_hook_command(dir),
        "statusMessage": CODEX_HOOK_STATUS,
        "type": "command"
    })
}

fn codex_hook_command(dir: &Path) -> String {
    shell_quote(
        &dir.join(".codex")
            .join(CODEX_AUTOINDEX_HELPER)
            .display()
            .to_string(),
    )
}

#[derive(Debug, Default)]
struct CodexAutoindexPolicy {
    focus: Vec<String>,
    cpu_affinity: Option<String>,
}

fn resolve_codex_autoindex_policy(
    dir: &Path,
    scope: CodexHookScope,
) -> Result<CodexAutoindexPolicy> {
    let configured = config::Config::load(dir)?.autoindex;
    let cpu_affinity = configured
        .cpu_affinity
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(value) = cpu_affinity.as_deref()
        && !value
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, ',' | '-'))
    {
        bail!(
            "invalid autoindex.cpu_affinity {value:?}; expected a taskset CPU list such as \"16-31\""
        );
    }

    if scope != CodexHookScope::Workspace {
        return Ok(CodexAutoindexPolicy {
            focus: Vec::new(),
            cpu_affinity,
        });
    }

    let mut resolved = Vec::new();
    for selector in configured.focus {
        let scope = config::Config::resolve_submodule(dir, &selector)?;
        if !resolved.contains(&scope.id) {
            resolved.push(scope.id);
        }
    }
    Ok(CodexAutoindexPolicy {
        focus: resolved,
        cpu_affinity,
    })
}

fn ensure_codex_autoindex_helper(
    dir: &Path,
    scope: CodexHookScope,
    policy: &CodexAutoindexPolicy,
) -> Result<bool> {
    let path = dir.join(".codex").join(CODEX_AUTOINDEX_HELPER);
    let expected = codex_autoindex_helper(dir, scope, policy);
    let mut changed = !path.exists() || std::fs::read_to_string(&path)? != expected;
    if changed {
        std::fs::write(&path, expected)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = std::fs::metadata(&path)?;
        let mut permissions = metadata.permissions();
        if permissions.mode() & 0o777 != 0o755 {
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions)?;
            changed = true;
        }
    }

    Ok(changed)
}

fn codex_autoindex_helper(
    dir: &Path,
    scope: CodexHookScope,
    policy: &CodexAutoindexPolicy,
) -> String {
    let root = shell_quote(&dir.display().to_string());
    let cpu_affinity = shell_quote(policy.cpu_affinity.as_deref().unwrap_or(""));
    let refresh_commands = match scope {
        CodexHookScope::Project => vec![
            "  run_tsift index --check --exit-code \"$root\" || run_tsift index \"$root\""
                .to_string(),
        ],
        CodexHookScope::Workspace if policy.focus.is_empty() => vec![
            "  run_tsift index --check --exit-code --workspace \"$root\" || run_tsift index --workspace \"$root\""
                .to_string(),
        ],
        CodexHookScope::Workspace => policy
            .focus
            .iter()
            .map(|scope| {
                let scope = shell_quote(scope);
                format!(
                    "  run_tsift index --check --exit-code --submodule {scope} \"$root\" || run_tsift index --submodule {scope} \"$root\""
                )
            })
            .collect(),
    }
    .join("\n");

    format!(
        r#"#!/usr/bin/env bash
# tsift-autoindex-hook-version: {version}
# The UI hook only starts this helper. The tsift binary runs after re-exec in a
# detached, debounced, low-priority, workspace-single-flight worker.

if [ "${{TSIFT_AUTOINDEX_WORKER:-0}}" != "1" ]; then
  TSIFT_AUTOINDEX_WORKER=1 nohup "$0" </dev/null >/dev/null 2>&1 &
  exit 0
fi

command -v tsift >/dev/null 2>&1 || exit 0
root={root}
cpu_affinity={cpu_affinity}
max_runtime_seconds="${{TSIFT_AUTOINDEX_MAX_SECONDS:-120}}"
case "$max_runtime_seconds" in
''|*[!0-9]*) max_runtime_seconds=120 ;;
esac
worker_started_seconds=$SECONDS
mkdir -p "$root/.tsift" || exit 0

# Coalesce simultaneous prompts from multiple UI windows. On platforms without
# flock, tsift's native index.lock still prevents competing heavy writers.
if command -v flock >/dev/null 2>&1; then
  exec 9>"$root/.tsift/autoindex-hook.lock"
  flock -n 9 || exit 0
fi

sleep "${{TSIFT_AUTOINDEX_DEBOUNCE_SECONDS:-0.25}}"

run_tsift() {{
local -a runner=()
local remaining_seconds="$max_runtime_seconds"
if command -v nice >/dev/null 2>&1; then
runner+=(nice -n 10)
fi
if [ "$max_runtime_seconds" != "0" ] && command -v timeout >/dev/null 2>&1; then
remaining_seconds=$((max_runtime_seconds - (SECONDS - worker_started_seconds)))
[ "$remaining_seconds" -gt 0 ] || return 124
runner+=(timeout --signal=TERM --kill-after=5 "${{remaining_seconds}}s")
fi
if [ -n "$cpu_affinity" ] && command -v taskset >/dev/null 2>&1; then
runner+=(taskset -c "$cpu_affinity")
fi
command "${{runner[@]}}" tsift "$@"
}}

refresh_indexes() {{
{refresh_commands}
}}

refresh_indexes
"#,
        version = CODEX_AUTOINDEX_HELPER_VERSION,
    )
}

struct OpenCodeCommandSpec {
    name: &'static str,
    description: &'static str,
    body: &'static str,
}

const OPENCODE_COMMANDS: &[OpenCodeCommandSpec] = &[
    OpenCodeCommandSpec {
        name: "tsift-status",
        description: "Refresh and summarize tsift index status",
        body: r#"Run `tsift status` from the project root, then summarize index freshness, instruction freshness, summary-cache state, and any recommended `use:` or `run:` commands. Stop and report the exact failure if the command fails."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-session-review",
        description: "Summarize bounded agent session context",
        body: r#"Run `tsift --envelope session-review <target> --next-context --budget normal`, where `<target>` is `$ARGUMENTS` or `.` when no argument is provided. Summarize prompt targets, unresolved failures, touched files/symbols, and next digest commands. Do not replay raw transcripts."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-context-pack",
        description: "Build a bounded tsift context pack",
        body: r#"Run `tsift --envelope context-pack <target> --budget normal`, where `<target>` is `$ARGUMENTS` or `.` when no argument is provided. Use source handles and expansion commands from the packet before reading whole files."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-memory-status",
        description: "Inspect first-party tsift memory readiness",
        body: r#"Run `tsift memory status <target> --json`, where `<target>` is `$ARGUMENTS` or `.` when no argument is provided. Summarize schema initialization, agent-doc hook contract, graph-db retrieval readiness, the claude-mem retirement gate, and rollback commands. Do not import data unless the user explicitly asks for `--apply`."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-memory-search",
        description: "Search first-party tsift memory graph",
        body: r#"Run `tsift graph-db --path . --json related '<query>'`, where `<query>` is `$ARGUMENTS`; ask for a query if `$ARGUMENTS` is empty. Summarize semantic readiness, useful memory/source hits, and any refresh/import fallback commands. Prefer tsift-memory/graph-db retrieval and do not call direct claude-mem or `/mem-search`; claude-mem remains only a fallback import source through `tsift memory import-claude-mem` when graph memory is missing."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-memory-guard",
        description: "Guard a memory or tool payload before model handoff",
        body: r#"Run `tsift memory budget-guard --file <target> --json` when `$ARGUMENTS` names a file, or `tsift memory budget-guard --text '<payload>' --json` for inline payload text. Summarize whether the payload is allowed, the estimated token count, replacement digest/context commands, and retryable chunk commands; file retry commands may include `--byte-start` / `--byte-end`. Do not send the raw payload to a model when the guard returns `blocked_split_required`."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-diff-digest",
        description: "Digest current or requested git diff",
        body: r#"Run `tsift diff-digest <target>`, where `<target>` is `$ARGUMENTS` or `.` when no argument is provided. Summarize changed paths, high-signal hunks, and any follow-up expansion commands instead of pasting the raw diff."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-test-digest",
        description: "Run tests through the bounded digest runner",
        body: r#"Run a bounded test digest. If `$ARGUMENTS` names a test command, run `tsift --envelope digest-runner --kind test --path . --shell-command '<command>'`; otherwise choose the project test command from the local instructions and wrap it the same way. Summarize failing tests, failure lines, and artifact handles."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-log-digest",
        description: "Run a verbose command through the bounded log digest",
        body: r#"Run a bounded log digest. If `$ARGUMENTS` names a build, install, or verification command, run `tsift --envelope digest-runner --kind log --path . --shell-command '<command>'`; otherwise ask for the command before running. Summarize compact output, failures, and artifact handles."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-rewrite-run",
        description: "Run a shell command through tsift rewrite",
        body: r#"Run the shell command named by `$ARGUMENTS` through `tsift rewrite --run '<command>'`. Use this for broad `rg`/recursive `grep`, raw transcript/session/log reads, `git diff`/`git show`/single-patch `git log`, `cargo test`/`pytest`, and cargo build/check/clippy/install commands so Codex/OpenCode get the same bounded search, session-digest, diff-digest, and digest-runner path as the Claude hook. If tsift reports no rewrite, do not retry automatically; summarize the reason and run the original command only when the user still needs exact raw output."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-source-read",
        description: "AST-aware source code reading via tsift source-read",
        body: r#"Read source code using `tsift --envelope source-read <file> --budget normal`, where `<file>` is `$ARGUMENTS` or the file the user wants to inspect. Prefer this over the raw Read tool for source code files (Rust, TypeScript, JavaScript, Python, Markdown, and other indexed languages). The envelope returns an AST-symbol projection with stable span metadata, `symbol-read` expansion commands for bodies, and `expand.window` commands for literal line previews. When `$ARGUMENTS` includes a line range, pass it as `--start <n> --lines <n>` to bound the AST projection. Add `--style window` only when the user needs numbered source lines. Fall back to the raw Read tool only for non-indexed files or binary assets."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-search",
        description: "AST-aware content search via tsift search",
        body: r#"Search code using `tsift --envelope search '<query>' --budget normal`, where `<query>` is `$ARGUMENTS`. Prefer this over grep/rg for content search in indexed projects. The envelope returns ranked search hits with symbol families, file previews, AST-aware scoring, and expansion commands. When the report includes a `scale_guard`, run one of its `narrow_commands` before dispatching parallel agents. Use `tsift workflow search` for the ordered exact/search/explain/summarize/digest recipe that preserves result handles across expansions. Fall back to grep/rg only when the project is not indexed or for non-code file patterns (e.g. glob-only searches)."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-explain",
        description: "Explain a symbol via callers, callees, and community preview",
        body: r#"Explain the symbol named by `$ARGUMENTS` using `tsift --envelope explain '<symbol>' --budget normal`. Prefer this when you need callers, callees, or community context for a function, struct, or type. The envelope returns ranked caller/callee lists with file locations, community membership, and expansion commands for graph traversal. When the report includes a `scale_guard`, run one of its `narrow_commands` before dispatching parallel agents. Use `tsift graph '<symbol>' --callers` or `--callees` for full call-graph navigation. Fall back to `tsift --envelope search '<symbol>' --budget normal` when the symbol is not found in the index."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-symbol-read",
        description: "Read symbol body with AST metadata via tsift symbol-read",
        body: r#"Read the symbol named by `$ARGUMENTS` using `tsift --envelope symbol-read '<symbol>' --budget normal`. Prefer this over reading entire source files when you need a specific function, struct, or type definition. The envelope returns the symbol body, AST span metadata, child references, and expansion commands for graph/source navigation. When `$ARGUMENTS` includes a file hint, pass it as `--file '<path>'` to disambiguate duplicate names. Use the returned `expand` commands to inspect callers, callees, or the full source file. Fall back to `tsift --envelope source-read '<file>' --budget normal` when the symbol is not found, or add `--style window --start <n> --lines <n>` only when you need raw numbered source lines."#,
    },
    OpenCodeCommandSpec {
        name: "tsift-graph",
        description: "Call graph navigation via tsift graph",
        body: r#"Navigate the call graph for the symbol named by `$ARGUMENTS` using `tsift graph '<symbol>' --callers` or `tsift graph '<symbol>' --callees`. Use `--callers` to find who calls the symbol, `--callees` to find what the symbol calls. The output lists edges with file locations, edge kinds, and navigation hints. Adjust `--limit` (default 20) to cap edges per direction. For a broader overview including community membership, prefer `tsift --envelope explain '<symbol>' --budget normal`. Fall back to `tsift --envelope search '<symbol>' --budget normal` when the symbol is not found in the index."#,
    },
];

fn ensure_opencode_commands(dir: &Path) -> Result<Vec<OpenCodeCommandUpdate>> {
    let commands_dir = dir.join(".opencode").join("commands");
    std::fs::create_dir_all(&commands_dir)?;

    let mut updates = Vec::new();
    for spec in OPENCODE_COMMANDS {
        let file = commands_dir.join(format!("{}.md", spec.name));
        let action = ensure_opencode_command_file(&file, spec)?;
        updates.push(OpenCodeCommandUpdate {
            file,
            command_name: spec.name,
            action,
        });
    }
    Ok(updates)
}

fn ensure_opencode_command_file(file: &Path, spec: &OpenCodeCommandSpec) -> Result<InitAction> {
    let content = opencode_command_content(spec);
    if file.exists() {
        let existing = std::fs::read_to_string(file)?;
        if existing == content {
            return Ok(InitAction::AlreadyPresent);
        }
        if !existing.contains(OPENCODE_COMMAND_MARKER_PREFIX) {
            bail!(
                "{} already exists and is not managed by tsift; move it or add the tsift marker before rerunning --opencode",
                file.display()
            );
        }
        std::fs::write(file, content)?;
        Ok(InitAction::Updated)
    } else {
        std::fs::write(file, content)?;
        Ok(InitAction::Created)
    }
}

fn opencode_command_content(spec: &OpenCodeCommandSpec) -> String {
    format!(
        r#"<!-- tsift:opencode-command v={version} name={name} -->
---
description: {description}
---

{body}
"#,
        version = TSIFT_VERSION,
        name = spec.name,
        description = spec.description,
        body = spec.body
    )
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

pub fn extract_runbook_version(content: &str) -> Option<String> {
    let start = content.find(RUNBOOK_MARKER_PREFIX)?;
    let rest = &content[start + RUNBOOK_MARKER_PREFIX.len()..];
    let close = rest.find("-->")?;
    let tag_content = rest[..close].trim();
    tag_content.strip_prefix("v=").map(|v| v.to_string())
}

/// The instruction block points at the generated runbook, so a missing or
/// out-of-date runbook makes the instruction surface stale even when the
/// `AGENTS.md` marker itself is current.
fn runbook_is_current(dir: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(dir.join(RUNBOOK_RELATIVE_PATH)) else {
        return false;
    };
    extract_runbook_version(&content).is_some_and(|v| v == TSIFT_VERSION)
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
        Some(v) if v == TSIFT_VERSION => {
            if runbook_is_current(dir) {
                InstructionStatus::Current { version: v }
            } else {
                InstructionStatus::Stale {
                    found: Some(v),
                    expected: TSIFT_VERSION.to_string(),
                }
            }
        }
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

    /// The action `init` recorded for a file, by path suffix.
    fn action_for(result: &InitResult, suffix: &str) -> Option<InitAction> {
        result
            .updates
            .iter()
            .find(|u| {
                u.file
                    .to_string_lossy()
                    .replace('\\', "/")
                    .ends_with(suffix)
            })
            .map(|u| u.action)
    }

    #[test]
    fn init_creates_agents_md_when_none_exists() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(result.updates.len(), 2);
        assert!(matches!(result.updates[0].action, InitAction::Created));
        assert_eq!(result.updates[0].file.file_name().unwrap(), "AGENTS.md");
        let content = std::fs::read_to_string(&result.updates[0].file).unwrap();
        assert!(content.contains(SECTION_MARKER_PREFIX));
        assert!(content.contains(RUNBOOK_RELATIVE_PATH));
        assert!(content.contains("tsift --envelope search"));
        assert!(content.contains("`tsift init` to refresh the tracked Code Navigation block"));
        assert!(!content.contains("make check"));
        assert!(!content.contains("gh run list"));
    }

    #[test]
    fn init_migrates_the_legacy_runbook_to_the_canonical_agent_directory() {
        let dir = TempDir::new().unwrap();
        let legacy = dir.path().join(LEGACY_RUNBOOK_RELATIVE_PATH);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy,
            format!(
                "# Local preamble\n\n{}v={} -->\nOld generated detail.\n{}\n\nLocal trailer.\n",
                RUNBOOK_MARKER_PREFIX, TSIFT_VERSION, RUNBOOK_END_MARKER
            ),
        )
        .unwrap();

        let result = init(dir.path(), false, false).unwrap();
        let canonical = dir.path().join(RUNBOOK_RELATIVE_PATH);

        assert!(!legacy.exists());
        assert_eq!(
            action_for(&result, RUNBOOK_RELATIVE_PATH),
            Some(InitAction::Updated)
        );
        let runbook = std::fs::read_to_string(&canonical).unwrap();
        assert!(runbook.starts_with("# Local preamble"));
        assert!(runbook.contains("Local trailer."));
        assert!(!runbook.contains("Old generated detail."));
        let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert!(agents.contains(RUNBOOK_RELATIVE_PATH));
        assert!(!agents.contains("](runbooks/code-navigation.md)"));
        assert_eq!(
            result.migrated_runbook,
            Some(RunbookMigration {
                from: LEGACY_RUNBOOK_RELATIVE_PATH,
                to: RUNBOOK_RELATIVE_PATH,
            }),
            "the tracked-file move must be reportable, not silent"
        );
    }

    #[test]
    fn init_reports_no_runbook_migration_when_there_is_nothing_to_move() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(result.migrated_runbook, None);

        let again = init(dir.path(), false, false).unwrap();
        assert_eq!(again.migrated_runbook, None);
    }

    #[test]
    fn generated_instruction_surfaces_never_teach_a_deprecated_flag() {
        let dir = TempDir::new().unwrap();
        let mut surfaces = vec![
            ("AGENTS.md block".to_string(), versioned_section(dir.path())),
            (
                "code-navigation runbook".to_string(),
                versioned_runbook_section(dir.path()),
            ),
        ];
        for command in OPENCODE_COMMANDS {
            surfaces.push((
                format!("opencode command {}", command.name),
                command.body.to_string(),
            ));
        }

        for (label, body) in surfaces {
            for deprecated in DEPRECATED_FLAG_USAGES {
                assert!(
                    !body.contains(deprecated),
                    "{label} still instructs the deprecated `{deprecated}`"
                );
            }
        }
    }

    #[test]
    fn verification_guidance_uses_only_detected_repo_tools_and_available_ci_clients() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("justfile"), "test:\n\t@cargo test\n").unwrap();
        std::fs::write(dir.path().join(".gitlab-ci.yml"), "test: {}\n").unwrap();

        let without_glab = verification_guidance_with(dir.path(), |_| false).unwrap();
        assert_eq!(
            without_glab,
            "For local verification, run `just test` before committing."
        );

        let with_glab = verification_guidance_with(dir.path(), |command| command == "glab")
            .expect("just + GitLab guidance");
        assert!(with_glab.contains("just test"));
        assert!(with_glab.contains("glab ci status"));
        assert!(!with_glab.contains("make check"));
        assert!(!with_glab.contains("gh run list"));
    }

    #[test]
    fn verification_guidance_detects_make_check_and_github_actions() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            ".PHONY: check\ncheck:\n\tcargo test\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();

        let guidance = verification_guidance_with(dir.path(), |command| command == "gh")
            .expect("make + GitHub guidance");
        assert!(guidance.contains("make check"));
        assert!(guidance.contains("gh run list --limit 1"));
    }

    #[test]
    fn init_writes_the_code_navigation_runbook_with_the_detail_the_block_defers_to() {
        let dir = TempDir::new().unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(
            action_for(&result, RUNBOOK_RELATIVE_PATH),
            Some(InitAction::Created)
        );

        let runbook = std::fs::read_to_string(dir.path().join(RUNBOOK_RELATIVE_PATH)).unwrap();
        assert!(runbook.contains(RUNBOOK_MARKER_PREFIX));
        assert!(runbook.contains(&format!("v={}", TSIFT_VERSION)));

        // The detail the lean block explicitly hands off must actually be here,
        // or the pointer sends agents to a file that answers nothing.
        for detail in [
            "tsift workflow search",
            "report.scale_guard",
            "tsift rewrite --run",
            "tsift init --codex",
            "tsift init --opencode",
            "rtk rewrite",
            "--budget normal",
            "tsift summarize",
        ] {
            assert!(runbook.contains(detail), "runbook is missing {detail}");
        }

        // ...and the block must not still carry that detail inline. It may name
        // a topic to say where it went; it must not restate the instruction.
        let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        for moved in [
            "narrow_commands",
            "tsift rewrite --run",
            "rtk rewrite",
            "--budget normal",
            "tsift summarize",
            "tsift init --codex",
        ] {
            assert!(
                !agents.contains(moved),
                "AGENTS.md still duplicates runbook detail: {moved}"
            );
        }
        assert!(
            agents.len() < runbook.len(),
            "the block should be the hot path, not the bigger of the two \
             (block {} bytes, runbook {} bytes)",
            agents.len(),
            runbook.len()
        );
    }

    /// `<!-- tsift:code-navigation-runbook` begins with the block marker's
    /// name. The block logic must not claim it.
    #[test]
    fn the_runbook_marker_is_not_mistaken_for_the_block_marker() {
        let runbook_only = format!(
            "{}v=1.2.3 -->\n# Code Navigation\n{}\n",
            RUNBOOK_MARKER_PREFIX, RUNBOOK_END_MARKER
        );
        assert!(!runbook_only.contains(SECTION_MARKER_PREFIX));
        assert_eq!(extract_instruction_version(&runbook_only), None);
        assert_eq!(
            extract_runbook_version(&runbook_only),
            Some("1.2.3".to_string())
        );

        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), &runbook_only).unwrap();
        assert!(matches!(
            check_instruction_version(dir.path()),
            InstructionStatus::Missing
        ));

        // The generated pair round-trips: each marker resolves to its own version.
        let block = versioned_section(dir.path());
        assert_eq!(
            extract_instruction_version(&block),
            Some(TSIFT_VERSION.to_string())
        );
        assert_eq!(extract_runbook_version(&block), None);
        assert_eq!(
            extract_runbook_version(&versioned_runbook_section(dir.path())),
            Some(TSIFT_VERSION.to_string())
        );
    }

    #[test]
    fn init_refreshes_a_stale_runbook_in_place_and_keeps_surrounding_text() {
        let dir = TempDir::new().unwrap();
        let runbook = dir.path().join(RUNBOOK_RELATIVE_PATH);
        std::fs::create_dir_all(runbook.parent().unwrap()).unwrap();
        std::fs::write(
            &runbook,
            format!(
                "# Local preamble\n\n{} v=0.0.1 -->\nOld.\n{}\n\nLocal trailer.\n",
                RUNBOOK_MARKER_PREFIX, RUNBOOK_END_MARKER
            ),
        )
        .unwrap();

        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(
            action_for(&result, RUNBOOK_RELATIVE_PATH),
            Some(InitAction::Updated)
        );
        let content = std::fs::read_to_string(&runbook).unwrap();
        assert!(content.starts_with("# Local preamble"));
        assert!(content.contains("Local trailer."));
        assert!(!content.contains("Old."));
        assert!(content.contains(&format!("{}v={} -->", RUNBOOK_MARKER_PREFIX, TSIFT_VERSION)));

        let again = init(dir.path(), false, false).unwrap();
        assert_eq!(
            action_for(&again, RUNBOOK_RELATIVE_PATH),
            Some(InitAction::AlreadyPresent)
        );
    }

    #[test]
    fn init_appends_to_existing_agents_md() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "# My Project\n\nSome instructions.\n").unwrap();
        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(action_for(&result, "AGENTS.md"), Some(InitAction::Created));
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
        assert_eq!(result.updates.len(), 3);
        assert_eq!(result.updates[0].file.file_name().unwrap(), "AGENTS.md");
        assert_eq!(result.updates[2].file.file_name().unwrap(), "CLAUDE.md");
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
        assert_eq!(result.updates.len(), 3);
        assert_eq!(result.updates[0].file.file_name().unwrap(), "AGENTS.md");
        assert!(dir.path().join("AGENTS.md").exists());
        assert_eq!(action_for(&result, "AGENTS.md"), Some(InitAction::Created));
        assert_eq!(action_for(&result, "CLAUDE.md"), Some(InitAction::Created));
        assert!(
            std::fs::read_to_string(claude)
                .unwrap()
                .contains(SECTION_MARKER_PREFIX)
        );
    }

    #[test]
    fn init_does_not_inject_into_a_claude_md_that_imports_agents_md() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agents\n").unwrap();
        let claude = dir.path().join("CLAUDE.md");
        std::fs::write(&claude, "@AGENTS.md\n\n# Claude extras\n").unwrap();

        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(action_for(&result, "CLAUDE.md"), Some(InitAction::Deferred));

        let content = std::fs::read_to_string(&claude).unwrap();
        assert!(
            !content.contains(SECTION_MARKER_PREFIX),
            "CLAUDE.md must not repeat instructions it already imports"
        );
        assert!(content.contains("# Claude extras"));
        // AGENTS.md, the canonical file, still gets it.
        assert!(
            std::fs::read_to_string(dir.path().join("AGENTS.md"))
                .unwrap()
                .contains(SECTION_MARKER_PREFIX)
        );
    }

    #[test]
    fn init_removes_a_duplicate_section_from_a_claude_md_that_imports_agents_md() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agents\n").unwrap();
        let claude = dir.path().join("CLAUDE.md");
        std::fs::write(
            &claude,
            format!(
                "@AGENTS.md\n\n{}v=0.0.1 -->\n## Code Navigation\nOld duplicate.\n{}\n\n## Claude extras\n",
                SECTION_MARKER_PREFIX, SECTION_END_MARKER
            ),
        )
        .unwrap();

        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(action_for(&result, "CLAUDE.md"), Some(InitAction::Removed));

        let content = std::fs::read_to_string(&claude).unwrap();
        assert!(!content.contains(SECTION_MARKER_PREFIX));
        assert!(!content.contains("Old duplicate."));
        assert!(content.starts_with("@AGENTS.md"));
        assert!(content.contains("## Claude extras"));

        // Second run has nothing left to remove.
        let again = init(dir.path(), false, false).unwrap();
        assert_eq!(action_for(&again, "CLAUDE.md"), Some(InitAction::Deferred));
    }

    #[test]
    #[cfg(unix)]
    fn init_leaves_a_claude_md_symlinked_to_agents_md_alone() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("AGENTS.md");
        std::fs::write(&agents, "# Agents\n").unwrap();
        let claude = dir.path().join("CLAUDE.md");
        std::os::unix::fs::symlink("AGENTS.md", &claude).unwrap();

        let result = init(dir.path(), false, false).unwrap();
        assert_eq!(action_for(&result, "CLAUDE.md"), Some(InitAction::Deferred));

        // Rewriting through the symlink would have stripped the section out of
        // the canonical file it points at.
        let content = std::fs::read_to_string(&agents).unwrap();
        assert!(content.contains(SECTION_MARKER_PREFIX));
        assert!(content.starts_with("# Agents"));
        assert_eq!(
            content.matches(SECTION_MARKER_PREFIX).count(),
            1,
            "the section must appear exactly once"
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
        assert!(content.contains("tsift --envelope search"));
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
    fn init_respects_git_info_exclude_without_touching_gitignore() {
        let dir = TempDir::new().unwrap();
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(dir.path().join(".git/info/exclude"), ".tsift/\n").unwrap();

        let result = init(dir.path(), false, false).unwrap();

        assert!(!result.gitignore_added);
        assert!(
            result
                .gitignore_ignore_source
                .as_deref()
                .is_some_and(|source| source.ends_with(".git/info/exclude")),
            "unexpected ignore source: {:?}",
            result.gitignore_ignore_source
        );
        assert!(!dir.path().join(".gitignore").exists());
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
    fn resolve_workspace_dir_uses_current_project_dir() {
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
    fn resolve_workspace_dir_does_not_escape_a_git_submodule() {
        let dir = TempDir::new().unwrap();
        let child_source = TempDir::new().unwrap();
        Command::new("git")
            .args(["init", &child_source.path().to_string_lossy()])
            .output()
            .unwrap();
        std::fs::write(child_source.path().join("tracked.txt"), "child\n").unwrap();
        Command::new("git")
            .args([
                "-C",
                &child_source.path().to_string_lossy(),
                "-c",
                "user.name=tsift test",
                "-c",
                "user.email=tsift@example.invalid",
                "add",
                ".",
            ])
            .output()
            .unwrap();
        let commit = Command::new("git")
            .args([
                "-C",
                &child_source.path().to_string_lossy(),
                "-c",
                "user.name=tsift test",
                "-c",
                "user.email=tsift@example.invalid",
                "commit",
                "-m",
                "fixture",
            ])
            .output()
            .unwrap();
        assert!(commit.status.success());

        Command::new("git")
            .args(["init", &dir.path().to_string_lossy()])
            .output()
            .unwrap();
        let child = dir.path().join("nested");
        let add = Command::new("git")
            .args([
                "-C",
                &dir.path().to_string_lossy(),
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                &child_source.path().to_string_lossy(),
                "nested",
            ])
            .output()
            .unwrap();
        assert!(add.status.success());

        let resolved = resolve_workspace_dir(&child).unwrap();
        assert_eq!(
            resolved.canonicalize().unwrap(),
            child.canonicalize().unwrap()
        );
    }

    #[test]
    fn has_submodules_reads_gitmodules() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src/foo")).unwrap();
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
        assert!(command.contains(CODEX_AUTOINDEX_HELPER));
        assert!(!command.contains("tsift index"));
        let helper = std::fs::read_to_string(dir.path().join(".codex/tsift-autoindex.sh")).unwrap();
        assert!(helper.contains("TSIFT_AUTOINDEX_WORKER=1 nohup"));
        assert!(helper.contains("flock -n 9"));
        assert!(helper.contains("runner+=(nice -n 10)"));
        assert!(helper.contains("TSIFT_AUTOINDEX_MAX_SECONDS:-120"));
        assert!(helper.contains("worker_started_seconds=$SECONDS"));
        assert!(helper.contains("max_runtime_seconds - (SECONDS - worker_started_seconds)"));
        assert!(helper.contains("timeout --signal=TERM --kill-after=5"));
        assert!(helper.contains("tsift index --check --exit-code"));
        assert!(helper.contains(&dir.path().display().to_string()));
        assert!(!helper.contains("--workspace"));
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
        assert!(command.contains(CODEX_AUTOINDEX_HELPER));
        assert!(!command.contains("tsift index"));
        let helper = std::fs::read_to_string(dir.path().join(".codex/tsift-autoindex.sh")).unwrap();
        assert!(helper.contains(&dir.path().display().to_string()));
        assert!(!helper.contains("--workspace"));
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
        std::fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
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
        assert!(command.contains(CODEX_AUTOINDEX_HELPER));
        assert!(!command.contains("tsift index"));
        let helper = std::fs::read_to_string(dir.path().join(".codex/tsift-autoindex.sh")).unwrap();
        assert!(helper.contains("--workspace"));
        assert!(helper.contains(&dir.path().display().to_string()));
    }

    #[test]
    fn init_codex_updates_project_hook_to_workspace_hook() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
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
        assert!(command.contains(CODEX_AUTOINDEX_HELPER));
        let helper = std::fs::read_to_string(dir.path().join(".codex/tsift-autoindex.sh")).unwrap();
        assert!(helper.contains("--workspace"));
    }

    #[test]
    fn init_codex_workspace_hook_honors_autoindex_focus() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/beta")).unwrap();
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"src/alpha\"]\n\tpath = src/alpha\n\n[submodule \"src/beta\"]\n\tpath = src/beta\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".tsift")).unwrap();
        std::fs::write(
            dir.path().join(".tsift/config.toml"),
            "[autoindex]\nfocus = [\"alpha\"]\ncpu_affinity = \"4-7\"\n",
        )
        .unwrap();

        init(dir.path(), true, true).unwrap();

        let helper = std::fs::read_to_string(dir.path().join(".codex/tsift-autoindex.sh")).unwrap();
        assert!(helper.contains("--submodule \"alpha\""));
        assert!(!helper.contains("--submodule \"beta\""));
        assert!(!helper.contains("--workspace"));
        assert!(helper.contains("cpu_affinity=\"4-7\""));
        assert!(helper.contains("runner+=(taskset -c \"$cpu_affinity\")"));
    }

    #[test]
    fn init_codex_rejects_unsafe_autoindex_cpu_affinity() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".tsift")).unwrap();
        std::fs::write(
            dir.path().join(".tsift/config.toml"),
            "[autoindex]\ncpu_affinity = \"0; shutdown\"\n",
        )
        .unwrap();

        let error = init(dir.path(), true, false)
            .err()
            .expect("unsafe affinity should fail")
            .to_string();
        assert!(error.contains("invalid autoindex.cpu_affinity"));
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
        assert!(result.opencode_commands.is_none());
        assert!(!dir.path().join(".codex/hooks.json").exists());
        assert!(!dir.path().join(".opencode/commands").exists());
    }

    #[test]
    fn init_opencode_creates_command_shortcuts() {
        let dir = TempDir::new().unwrap();
        let result = init_with_integrations(dir.path(), false, false, true).unwrap();
        let commands = result
            .opencode_commands
            .expect("opencode command updates should be present");
        assert_eq!(commands.len(), OPENCODE_COMMANDS.len());
        assert!(
            commands
                .iter()
                .all(|update| matches!(update.action, InitAction::Created))
        );

        let status =
            std::fs::read_to_string(dir.path().join(".opencode/commands/tsift-status.md")).unwrap();
        assert!(status.contains(OPENCODE_COMMAND_MARKER_PREFIX));
        assert!(status.contains("description: Refresh and summarize tsift index status"));
        assert!(status.contains("Run `tsift status` from the project root"));

        let session_review = std::fs::read_to_string(
            dir.path()
                .join(".opencode/commands/tsift-session-review.md"),
        )
        .unwrap();
        assert!(session_review.contains("$ARGUMENTS"));
        assert!(session_review.contains("tsift --envelope session-review"));
        assert!(session_review.contains("--next-context --budget normal"));

        let test_digest =
            std::fs::read_to_string(dir.path().join(".opencode/commands/tsift-test-digest.md"))
                .unwrap();
        assert!(test_digest.contains("digest-runner"));
        assert!(!test_digest.contains("__digest-runner"));
        assert!(test_digest.contains("--kind test"));

        let rewrite_run =
            std::fs::read_to_string(dir.path().join(".opencode/commands/tsift-rewrite-run.md"))
                .unwrap();
        assert!(rewrite_run.contains("tsift rewrite --run"));
        assert!(rewrite_run.contains("broad `rg`/recursive `grep`"));
        assert!(rewrite_run.contains("digest-runner"));

        let memory_status =
            std::fs::read_to_string(dir.path().join(".opencode/commands/tsift-memory-status.md"))
                .unwrap();
        assert!(memory_status.contains("tsift memory status"));
        assert!(memory_status.contains("graph-db retrieval readiness"));
        assert!(memory_status.contains("claude-mem retirement gate"));
        assert!(memory_status.contains("rollback commands"));

        let memory_search =
            std::fs::read_to_string(dir.path().join(".opencode/commands/tsift-memory-search.md"))
                .unwrap();
        assert!(memory_search.contains("tsift graph-db --path . --json related"));
        assert!(memory_search.contains("do not call direct claude-mem or `/mem-search`"));

        let memory_guard =
            std::fs::read_to_string(dir.path().join(".opencode/commands/tsift-memory-guard.md"))
                .unwrap();
        assert!(memory_guard.contains("tsift memory budget-guard"));
        assert!(memory_guard.contains("blocked_split_required"));
    }

    #[test]
    fn init_opencode_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let first = init_with_integrations(dir.path(), false, false, true).unwrap();
        assert!(
            first
                .opencode_commands
                .unwrap()
                .iter()
                .all(|update| matches!(update.action, InitAction::Created))
        );

        let second = init_with_integrations(dir.path(), false, false, true).unwrap();
        assert!(
            second
                .opencode_commands
                .unwrap()
                .iter()
                .all(|update| matches!(update.action, InitAction::AlreadyPresent))
        );
    }

    #[test]
    fn opencode_npm_package_matches_init_command_shortcuts() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let package_dir = manifest_dir.join("../opencode-tsift");
        let package_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(package_dir.join("package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(package_json["version"], TSIFT_VERSION);

        for spec in OPENCODE_COMMANDS {
            let packaged = std::fs::read_to_string(
                package_dir
                    .join("commands")
                    .join(format!("{}.md", spec.name)),
            )
            .unwrap_or_else(|_| panic!("missing packaged OpenCode command {}", spec.name));
            assert_eq!(packaged, opencode_command_content(spec));
        }
    }

    #[test]
    fn init_opencode_refuses_unmanaged_command_conflict() {
        let dir = TempDir::new().unwrap();
        let commands_dir = dir.path().join(".opencode/commands");
        std::fs::create_dir_all(&commands_dir).unwrap();
        std::fs::write(
            commands_dir.join("tsift-status.md"),
            "---\ndescription: user command\n---\n\nDo something else.\n",
        )
        .unwrap();

        let err = init_with_integrations(dir.path(), false, false, true)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("not managed by tsift"), "{err}");
    }

    #[test]
    fn init_embeds_version_in_marker() {
        let dir = TempDir::new().unwrap();
        init(dir.path(), false, false).unwrap();
        let content = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        let expected_marker = format!("<!-- tsift:code-navigation v={} -->", TSIFT_VERSION);
        assert!(content.contains(&expected_marker));
        assert!(content.contains("tsift --envelope session-review <path>"));
        assert!(content.contains("tsift --envelope context-pack <path>"));
        assert!(content.contains("tsift session-digest --input <path>"));
        assert!(content.contains("tsift log-digest --input <path>"));
        assert!(content.contains("tsift diff-digest [path]"));
        assert!(content.contains("tsift --envelope digest-runner --kind test|log"));

        // Codex/OpenCode integration detail lives in the runbook now.
        let runbook = std::fs::read_to_string(dir.path().join(RUNBOOK_RELATIVE_PATH)).unwrap();
        assert!(runbook.contains("tsift init --opencode"));
        assert!(runbook.contains("tsift --envelope session-review <path> --next-context"));
        assert!(runbook.contains("tsift --envelope digest-runner --kind test --path ."));
        assert!(runbook.contains("tsift --envelope digest-runner --kind log --path ."));
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
