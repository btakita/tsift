use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(".tsift/config.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&content)
            .with_context(|| format!("parsing {}", path.display()))
    }

    pub fn tier_for(&self, submodule: &str) -> IsolationTier {
        self.overrides.get(submodule)
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

    pub fn db_path_for(&self, root: &Path, submodule: &str) -> PathBuf {
        root.join(".tsift/indexes").join(submodule).join("index.db")
    }

    pub fn submodule_dirs(root: &Path) -> Result<Vec<(String, PathBuf)>> {
        let gitmodules = root.join(".gitmodules");
        if !gitmodules.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&gitmodules)
            .with_context(|| "reading .gitmodules")?;
        let mut result = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(path_val) = trimmed.strip_prefix("path = ") {
                let path_val = path_val.trim();
                let name = Path::new(path_val)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path_val.to_string());
                result.push((name, root.join(path_val)));
            }
        }
        Ok(result)
    }
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
        fs::write(tsift_dir.join("config.toml"), r#"
[defaults]
federation = true
tier = "shared"

[overrides.mail]
federation = false
tier = "private"

[overrides.session-share]
tier = "isolated"

[overrides.agent-doc]
federation = true
"#).unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.tier_for("mail"), IsolationTier::Private);
        assert!(!cfg.federation_for("mail"));
        assert_eq!(cfg.tier_for("session-share"), IsolationTier::Isolated);
        assert!(!cfg.federation_for("session-share"));
        assert_eq!(cfg.tier_for("agent-doc"), IsolationTier::Shared);
        assert!(cfg.federation_for("agent-doc"));
        assert_eq!(cfg.tier_for("unknown"), IsolationTier::Shared);
        assert!(cfg.federation_for("unknown"));
    }

    #[test]
    fn federation_false_on_private_even_without_explicit_flag() {
        let dir = tempfile::tempdir().unwrap();
        let tsift_dir = dir.path().join(".tsift");
        fs::create_dir_all(&tsift_dir).unwrap();
        fs::write(tsift_dir.join("config.toml"), r#"
[overrides.secret]
tier = "private"
"#).unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        assert!(!cfg.federation_for("secret"));
    }

    #[test]
    fn db_path_per_submodule() {
        let cfg = Config::default();
        let root = Path::new("/workspace");
        let path = cfg.db_path_for(root, "agent-doc");
        assert_eq!(path, PathBuf::from("/workspace/.tsift/indexes/agent-doc/index.db"));
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
    }

    #[test]
    fn override_federation_only() {
        let dir = tempfile::tempdir().unwrap();
        let tsift_dir = dir.path().join(".tsift");
        fs::create_dir_all(&tsift_dir).unwrap();
        fs::write(tsift_dir.join("config.toml"), r#"
[overrides.special]
federation = false
"#).unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        assert!(!cfg.federation_for("special"));
        assert_eq!(cfg.tier_for("special"), IsolationTier::Shared);
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
        fs::write(dir.path().join(".gitmodules"), r#"[submodule "src/agent-doc"]
	path = src/agent-doc
	url = https://github.com/btakita/agent-doc
[submodule "src/corky"]
	path = src/corky
	url = https://github.com/btakita/corky
"#).unwrap();
        let dirs = Config::submodule_dirs(dir.path()).unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].0, "agent-doc");
        assert_eq!(dirs[1].0, "corky");
    }
}
