use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Synthetic scope id for files owned by a workspace root rather than one of
/// its configured submodule scopes.
pub const WORKSPACE_ROOT_SCOPE_ID: &str = "<root>";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IsolationTier {
    #[default]
    Shared,
    Private,
    Isolated,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub overrides: HashMap<String, SubmoduleOverride>,
    #[serde(default)]
    pub autoindex: AutoindexConfig,
    #[serde(default)]
    pub findings: FindingsConfig,
}

/// Prompt-hook indexing policy.
///
/// An empty focus preserves the workspace-wide default. Workspace users with
/// many scopes can list only the scope ids (or relative submodule paths) that
/// should be kept warm by the background prompt hook. Read commands still
/// perform their own freshness check for any scope they actually consume.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AutoindexConfig {
    #[serde(default)]
    pub focus: Vec<String>,
    /// Optional Linux `taskset -c` CPU list for background autoindex workers
    /// (for example, `"16-31"`). This constrains tsift; reserving those CPUs
    /// away from the UI remains an operating-system configuration concern.
    #[serde(default)]
    pub cpu_affinity: Option<String>,
}

/// Findings Graph Layer configuration (#trt1p4). Currently gates the passive
/// auto-capture path; off by default so a user who never opts in sees zero
/// behavior change.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FindingsConfig {
    /// Enable passive harvest of `draft` candidate findings from agent-doc
    /// session archives (`tsift finding harvest`). Off by default — the whole
    /// auto-capture path is fail-closed until explicitly enabled.
    #[serde(default)]
    pub passive_harvest: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_true")]
    pub federation: bool,
    #[serde(default)]
    pub tier: IsolationTier,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            federation: true,
            tier: IsolationTier::Shared,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubmoduleOverride {
    #[serde(default)]
    pub federation: Option<bool>,
    #[serde(default)]
    pub tier: Option<IsolationTier>,
    /// Whether `tsift init --workspace` refreshes this scope's tracked
    /// instruction surface (`#wsinit`). Defaults to on: silence is the wrong
    /// default when the block itself tells an agent to prefer submodule-local
    /// files. Set `instructions = false` for a vendored or read-only scope.
    #[serde(default)]
    pub instructions: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceScope {
    pub id: String,
    pub legacy_name: String,
    pub relative_path: String,
    pub source_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvableWorkspaceScope {
    pub relative_path: String,
    pub source_root: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceDiscovery {
    pub scopes: Vec<WorkspaceScope>,
    pub unresolvable: Vec<UnresolvableWorkspaceScope>,
}

impl WorkspaceScope {
    pub fn matches_selector(&self, selector: &str) -> bool {
        self.id == selector || self.relative_path == selector
    }
}

fn default_true() -> bool {
    true
}

impl Config {
    fn override_for_scope(&self, scope: &WorkspaceScope) -> Option<&SubmoduleOverride> {
        self.overrides
            .get(&scope.id)
            .or_else(|| self.overrides.get(&scope.relative_path))
            .or_else(|| self.overrides.get(&scope.legacy_name))
    }

    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(".tsift/config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn tier_for(&self, submodule: &str) -> IsolationTier {
        self.overrides
            .get(submodule)
            .and_then(|o| o.tier)
            .unwrap_or(self.defaults.tier)
    }

    pub fn tier_for_scope(&self, scope: &WorkspaceScope) -> IsolationTier {
        self.override_for_scope(scope)
            .and_then(|o| o.tier)
            .unwrap_or(self.defaults.tier)
    }

    pub fn federation_for(&self, submodule: &str) -> bool {
        if let Some(ovr) = self.overrides.get(submodule) {
            if let Some(tier) = ovr.tier
                && (tier == IsolationTier::Isolated || tier == IsolationTier::Private)
            {
                return false;
            }
            if let Some(fed) = ovr.federation {
                return fed;
            }
        }
        self.defaults.federation
    }

    /// Whether `tsift init --workspace` should refresh this scope's tracked
    /// instruction files (`#wsinit`).
    pub fn instructions_for_scope(&self, scope: &WorkspaceScope) -> bool {
        self.override_for_scope(scope)
            .and_then(|ovr| ovr.instructions)
            .unwrap_or(true)
    }

    pub fn federation_for_scope(&self, scope: &WorkspaceScope) -> bool {
        if let Some(ovr) = self.override_for_scope(scope) {
            if let Some(tier) = ovr.tier
                && (tier == IsolationTier::Isolated || tier == IsolationTier::Private)
            {
                return false;
            }
            if let Some(fed) = ovr.federation {
                return fed;
            }
        }
        self.defaults.federation
    }

    pub fn db_path_for(&self, root: &Path, submodule: &str) -> PathBuf {
        root.join(".tsift/indexes").join(submodule).join("index.db")
    }

    pub fn available_scope_names(root: &Path) -> Result<Vec<String>> {
        Ok(Self::submodule_dirs(root)?
            .into_iter()
            .map(|scope| scope.id)
            .collect())
    }

    pub fn find_submodule(root: &Path, selector: &str) -> Result<Option<WorkspaceScope>> {
        let scopes = Self::submodule_dirs(root)?;
        if let Some(scope) = scopes
            .iter()
            .find(|scope| scope.matches_selector(selector))
            .cloned()
        {
            return Ok(Some(scope));
        }

        let alias_matches: Vec<WorkspaceScope> = scopes
            .into_iter()
            .filter(|scope| scope.legacy_name == selector)
            .collect();
        match alias_matches.len() {
            0 => Ok(None),
            1 => Ok(alias_matches.into_iter().next()),
            _ => {
                let options = alias_matches
                    .iter()
                    .map(|scope| scope.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("ambiguous scope `{}`. Use one of: {}", selector, options);
            }
        }
    }

    pub fn resolve_submodule(root: &Path, selector: &str) -> Result<WorkspaceScope> {
        if let Some(scope) = Self::find_submodule(root, selector)? {
            return Ok(scope);
        }

        let available_scopes = Self::available_scope_names(root)?;
        if available_scopes.is_empty() {
            bail!(
                "unknown scope `{}`. Workspace {} has no configured submodules.",
                selector,
                root.display()
            );
        }

        bail!(
            "unknown scope `{}`. Available scopes: {}",
            selector,
            available_scopes.join(", ")
        );
    }

    pub fn infer_submodule_from_path(root: &Path, path: &Path) -> Result<Option<WorkspaceScope>> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", path.display()))?;
        let mut scopes = Self::submodule_dirs(root)?;
        scopes.sort_by(|left, right| {
            right
                .source_root
                .components()
                .count()
                .cmp(&left.source_root.components().count())
        });
        Ok(scopes
            .into_iter()
            .find(|scope| canonical.starts_with(&scope.source_root)))
    }

    pub fn submodule_dirs(root: &Path) -> Result<Vec<WorkspaceScope>> {
        Ok(Self::workspace_discovery(root)?.scopes)
    }

    /// Resolve usable workspace scopes while retaining stale `.gitmodules`
    /// declarations as diagnostics.
    ///
    /// An initialized directory is usable even when a fixture or worktree does
    /// not expose it as a gitlink. An absent directory is usable only when the
    /// repository index still owns a `160000` gitlink for it; otherwise the
    /// declaration is stale configuration, not a scope.
    pub fn workspace_discovery(root: &Path) -> Result<WorkspaceDiscovery> {
        let mut paths = Vec::new();
        let mut unresolved_paths = Vec::new();
        let mut seen_paths = HashSet::new();
        collect_workspace_paths(
            root,
            Path::new(""),
            &mut paths,
            &mut unresolved_paths,
            &mut seen_paths,
        )?;
        if paths.is_empty() && unresolved_paths.is_empty() {
            return Ok(WorkspaceDiscovery::default());
        }
        let mut alias_counts: HashMap<String, usize> = HashMap::new();
        for path_val in &paths {
            let legacy_name = Path::new(path_val)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path_val.to_string());
            *alias_counts.entry(legacy_name).or_default() += 1;
        }

        let mut result = Vec::new();
        for path_val in paths {
            let legacy_name = Path::new(&path_val)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path_val.clone());
            let id = if alias_counts.get(&legacy_name).copied().unwrap_or(0) > 1 {
                path_val.clone()
            } else {
                legacy_name.clone()
            };
            result.push(WorkspaceScope {
                id,
                legacy_name,
                relative_path: path_val.clone(),
                source_root: root.join(&path_val),
            });
        }
        let unresolvable = unresolved_paths
            .into_iter()
            .map(|relative_path| UnresolvableWorkspaceScope {
                source_root: root.join(&relative_path),
                relative_path,
            })
            .collect();
        Ok(WorkspaceDiscovery {
            scopes: result,
            unresolvable,
        })
    }
}

/// Discover every initialized nested workspace, not only the root's immediate
/// `.gitmodules` entries. A nested workspace is its own index/federation scope;
/// otherwise a parent scope can silently omit an ignored-but-tracked gitlink
/// while root diagnostics still claim that parent was searched completely.
fn collect_workspace_paths(
    root: &Path,
    base_relative: &Path,
    paths: &mut Vec<String>,
    unresolved_paths: &mut Vec<String>,
    seen_paths: &mut HashSet<String>,
) -> Result<()> {
    let source_root = root.join(base_relative);
    let gitmodules = source_root.join(".gitmodules");
    if !gitmodules.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&gitmodules)
        .with_context(|| format!("reading {}", gitmodules.display()))?;
    let declared = content
        .lines()
        .filter_map(|line| line.trim().strip_prefix("path = "))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let gitlinks = gitlink_paths(&source_root, &declared);

    for local_path in declared {
        let relative = base_relative.join(&local_path);
        let relative_path = relative.to_string_lossy().replace('\\', "/");
        if !seen_paths.insert(relative_path.clone()) {
            continue;
        }
        let nested_root = root.join(&relative);
        if nested_root.is_dir() || gitlinks.contains(&local_path) {
            paths.push(relative_path);
            if nested_root.is_dir() {
                collect_workspace_paths(
                    root,
                    &relative,
                    paths,
                    unresolved_paths,
                    seen_paths,
                )?;
            }
        } else {
            unresolved_paths.push(relative_path);
        }
    }
    Ok(())
}

fn gitlink_paths(root: &Path, paths: &[String]) -> HashSet<String> {
    if paths.is_empty() {
        return HashSet::new();
    }

    let output = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--stage", "-z", "--"])
        .args(paths)
        .output();
    let Ok(output) = output else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            let (metadata, path) = entry.split_once('\t')?;
            metadata.starts_with("160000 ").then(|| path.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_config_shared_federated() {
        let cfg = Config::default();
        assert_eq!(cfg.defaults.tier, IsolationTier::Shared);
        assert!(cfg.defaults.federation);
        assert_eq!(cfg.tier_for("anything"), IsolationTier::Shared);
        assert!(cfg.federation_for("anything"));
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.defaults.tier, IsolationTier::Shared);
        assert!(cfg.defaults.federation);
    }

    #[test]
    fn parse_full_config() {
        let dir = tempfile::tempdir().unwrap();
        let tsift_dir = dir.path().join(".tsift");
        fs::create_dir_all(&tsift_dir).unwrap();
        fs::write(
            tsift_dir.join("config.toml"),
            r#"
[defaults]
federation = true
tier = "shared"

[autoindex]
focus = ["agent-doc", "src/session-share"]
cpu_affinity = "16-31"

[overrides.mail]
federation = false
tier = "private"

[overrides.session-share]
tier = "isolated"

[overrides.agent-doc]
federation = true
"#,
        )
        .unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.tier_for("mail"), IsolationTier::Private);
        assert!(!cfg.federation_for("mail"));
        assert_eq!(cfg.tier_for("session-share"), IsolationTier::Isolated);
        assert!(!cfg.federation_for("session-share"));
        assert_eq!(cfg.tier_for("agent-doc"), IsolationTier::Shared);
        assert!(cfg.federation_for("agent-doc"));
        assert_eq!(cfg.tier_for("unknown"), IsolationTier::Shared);
        assert!(cfg.federation_for("unknown"));
        assert_eq!(
            cfg.autoindex.focus,
            vec!["agent-doc".to_string(), "src/session-share".to_string()]
        );
        assert_eq!(cfg.autoindex.cpu_affinity.as_deref(), Some("16-31"));
    }

    #[test]
    fn federation_false_on_private_even_without_explicit_flag() {
        let dir = tempfile::tempdir().unwrap();
        let tsift_dir = dir.path().join(".tsift");
        fs::create_dir_all(&tsift_dir).unwrap();
        fs::write(
            tsift_dir.join("config.toml"),
            r#"
[overrides.secret]
tier = "private"
"#,
        )
        .unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        assert!(!cfg.federation_for("secret"));
    }

    #[test]
    fn db_path_per_submodule() {
        let cfg = Config::default();
        let root = Path::new("/workspace");
        let path = cfg.db_path_for(root, "agent-doc");
        assert_eq!(
            path,
            PathBuf::from("/workspace/.tsift/indexes/agent-doc/index.db")
        );
    }

    #[test]
    fn parse_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let tsift_dir = dir.path().join(".tsift");
        fs::create_dir_all(&tsift_dir).unwrap();
        fs::write(tsift_dir.join("config.toml"), "").unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.defaults.tier, IsolationTier::Shared);
        assert!(cfg.defaults.federation);
        assert!(cfg.autoindex.focus.is_empty());
        assert!(cfg.autoindex.cpu_affinity.is_none());
    }

    #[test]
    fn override_federation_only() {
        let dir = tempfile::tempdir().unwrap();
        let tsift_dir = dir.path().join(".tsift");
        fs::create_dir_all(&tsift_dir).unwrap();
        fs::write(
            tsift_dir.join("config.toml"),
            r#"
[overrides.special]
federation = false
"#,
        )
        .unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        assert!(!cfg.federation_for("special"));
        assert_eq!(cfg.tier_for("special"), IsolationTier::Shared);
    }

    #[test]
    fn findings_passive_harvest_defaults_off() {
        let cfg = Config::default();
        assert!(!cfg.findings.passive_harvest);
        // Absent [findings] section also defaults off.
        let dir = tempfile::tempdir().unwrap();
        let tsift_dir = dir.path().join(".tsift");
        fs::create_dir_all(&tsift_dir).unwrap();
        fs::write(
            tsift_dir.join("config.toml"),
            "[defaults]\nfederation = true\n",
        )
        .unwrap();
        assert!(!Config::load(dir.path()).unwrap().findings.passive_harvest);
    }

    #[test]
    fn findings_passive_harvest_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let tsift_dir = dir.path().join(".tsift");
        fs::create_dir_all(&tsift_dir).unwrap();
        fs::write(
            tsift_dir.join("config.toml"),
            "[findings]\npassive_harvest = true\n",
        )
        .unwrap();
        assert!(Config::load(dir.path()).unwrap().findings.passive_harvest);
    }

    #[test]
    fn submodule_dirs_no_gitmodules() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Config::submodule_dirs(dir.path()).unwrap();
        assert!(dirs.is_empty());
    }

    #[test]
    fn submodule_dirs_parses_gitmodules() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/agent-doc")).unwrap();
        fs::create_dir_all(dir.path().join("src/corky")).unwrap();
        fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "src/agent-doc"]
	path = src/agent-doc
	url = https://github.com/btakita/agent-doc
[submodule "src/corky"]
	path = src/corky
	url = https://github.com/btakita/corky
"#,
        )
        .unwrap();
        let dirs = Config::submodule_dirs(dir.path()).unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].id, "agent-doc");
        assert_eq!(dirs[0].relative_path, "src/agent-doc");
        assert_eq!(dirs[1].id, "corky");
        assert_eq!(dirs[1].relative_path, "src/corky");
    }

    #[test]
    fn submodule_dirs_use_full_path_when_leaf_names_collide() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("pkg/app/foo")).unwrap();
        fs::create_dir_all(dir.path().join("vendor/foo")).unwrap();
        fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "pkg/app/foo"]
	path = pkg/app/foo
	url = https://example.com/pkg-app-foo
[submodule "vendor/foo"]
	path = vendor/foo
	url = https://example.com/vendor-foo
"#,
        )
        .unwrap();

        let dirs = Config::submodule_dirs(dir.path()).unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].id, "pkg/app/foo");
        assert_eq!(dirs[0].legacy_name, "foo");
        assert_eq!(dirs[1].id, "vendor/foo");
        assert_eq!(dirs[1].legacy_name, "foo");
    }

    #[test]
    fn find_submodule_errors_on_ambiguous_legacy_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("pkg/app/foo")).unwrap();
        fs::create_dir_all(dir.path().join("vendor/foo")).unwrap();
        fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "pkg/app/foo"]
	path = pkg/app/foo
	url = https://example.com/pkg-app-foo
[submodule "vendor/foo"]
	path = vendor/foo
	url = https://example.com/vendor-foo
"#,
        )
        .unwrap();

        let err = Config::find_submodule(dir.path(), "foo").unwrap_err();
        assert!(err.to_string().contains("ambiguous scope `foo`"));
        assert!(err.to_string().contains("pkg/app/foo"));
        assert!(err.to_string().contains("vendor/foo"));
    }

    #[test]
    fn infer_submodule_from_path_uses_matching_nested_scope() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
        )
        .unwrap();
        let nested = dir.path().join("src/alpha/nested");
        fs::create_dir_all(&nested).unwrap();

        let scope = Config::infer_submodule_from_path(dir.path(), &nested)
            .unwrap()
            .expect("expected inferred scope");

        assert_eq!(scope.id, "alpha");
        assert_eq!(scope.source_root, dir.path().join("src/alpha"));
    }

    #[test]
    fn workspace_discovery_reports_absent_non_gitlink_as_unresolvable() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "deploy"]
path = deploy
url = https://example.com/deploy
"#,
        )
        .unwrap();

        let discovery = Config::workspace_discovery(dir.path()).unwrap();

        assert!(discovery.scopes.is_empty());
        assert_eq!(discovery.unresolvable.len(), 1);
        assert_eq!(discovery.unresolvable[0].relative_path, "deploy");
        assert_eq!(
            discovery.unresolvable[0].source_root,
            dir.path().join("deploy")
        );
    }

    #[test]
    fn workspace_discovery_keeps_an_uninitialized_gitlink_scope() {
        let dir = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "deploy"]
path = deploy
url = https://example.com/deploy
"#,
        )
        .unwrap();
        let cacheinfo = Command::new("git")
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,1111111111111111111111111111111111111111,deploy",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(cacheinfo.success());

        let discovery = Config::workspace_discovery(dir.path()).unwrap();

        assert_eq!(discovery.scopes.len(), 1);
        assert_eq!(discovery.scopes[0].id, "deploy");
        assert!(discovery.unresolvable.is_empty());
    }

    #[test]
    fn workspace_discovery_recurses_into_nested_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("src/parent");
        let nested = parent.join("vendor/nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"src/parent\"]\npath = src/parent\nurl = https://example.com/parent\n",
        )
        .unwrap();
        fs::write(
            parent.join(".gitmodules"),
            "[submodule \"vendor/nested\"]\npath = vendor/nested\nurl = https://example.com/nested\n",
        )
        .unwrap();
        // The parent can ignore a path that Git still owns as a gitlink. The
        // nested workspace must remain independently discoverable.
        fs::write(parent.join(".gitignore"), "vendor/nested\n").unwrap();

        let discovery = Config::workspace_discovery(dir.path()).unwrap();

        assert_eq!(
            discovery
                .scopes
                .iter()
                .map(|scope| (scope.id.as_str(), scope.relative_path.as_str()))
                .collect::<Vec<_>>(),
            vec![("parent", "src/parent"), ("nested", "src/parent/vendor/nested")]
        );
        assert!(discovery.unresolvable.is_empty());
    }
}
