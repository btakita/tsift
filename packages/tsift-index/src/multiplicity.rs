use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MultiplicityLayer {
    RepositoryRoot,
    GitSubmodule,
    CargoWorkspace,
    CargoPackage,
    LanguageWorkspace,
    GeneratedRuntime,
    AgentDocSession,
}

pub fn multiplicity_precedence() -> Vec<MultiplicityLayer> {
    vec![
        MultiplicityLayer::RepositoryRoot,
        MultiplicityLayer::GitSubmodule,
        MultiplicityLayer::CargoWorkspace,
        MultiplicityLayer::CargoPackage,
        MultiplicityLayer::LanguageWorkspace,
        MultiplicityLayer::GeneratedRuntime,
        MultiplicityLayer::AgentDocSession,
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CargoWorkspaceInfo {
    pub id: String,
    pub manifest_path: PathBuf,
    pub workspace_root: PathBuf,
    pub relative_manifest_path: String,
    pub relative_root: String,
    pub members: Vec<String>,
    pub default_members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CargoDependencyInfo {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CargoPackageInfo {
    pub name: String,
    pub normalized_name: String,
    pub scope_id: String,
    pub manifest_path: PathBuf,
    pub package_root: PathBuf,
    pub workspace_root: PathBuf,
    pub relative_manifest_path: String,
    pub relative_root: String,
    pub relative_workspace_root: String,
    pub features: Vec<String>,
    pub targets: Vec<String>,
    pub dependencies: Vec<CargoDependencyInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CargoInventory {
    pub workspaces: Vec<CargoWorkspaceInfo>,
    pub packages: Vec<CargoPackageInfo>,
}

#[derive(Debug, Deserialize)]
struct CargoManifest {
    package: Option<CargoManifestPackage>,
    workspace: Option<CargoManifestWorkspace>,
    lib: Option<CargoManifestTarget>,
    #[serde(default)]
    bin: Vec<CargoManifestTarget>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    dependencies: BTreeMap<String, toml::Value>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, toml::Value>,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
struct CargoManifestPackage {
    name: String,
}

#[derive(Debug, Default, Deserialize)]
struct CargoManifestWorkspace {
    #[serde(default)]
    members: Vec<String>,
    #[serde(default, rename = "default-members")]
    default_members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CargoManifestTarget {
    name: Option<String>,
}

fn normalize_package_name(name: &str) -> String {
    name.replace('-', "_")
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn read_manifest(path: &Path) -> Result<CargoManifest> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading cargo manifest {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("parsing cargo manifest {}", path.display()))
}

fn cargo_manifest_paths(root: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();
    for result in walker {
        let entry =
            result.with_context(|| format!("walking cargo manifests under {}", root.display()))?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if entry.path().file_name().and_then(|name| name.to_str()) == Some("Cargo.toml") {
            paths.push(entry.path().to_path_buf());
        }
    }
    paths.sort();
    Ok(paths)
}

fn nearest_workspace_root(package_root: &Path, workspaces: &[CargoWorkspaceInfo]) -> PathBuf {
    workspaces
        .iter()
        .filter(|workspace| package_root.starts_with(&workspace.workspace_root))
        .max_by_key(|workspace| workspace.workspace_root.components().count())
        .map(|workspace| workspace.workspace_root.clone())
        .unwrap_or_else(|| package_root.to_path_buf())
}

fn dependency_rows(
    kind: &str,
    deps: &BTreeMap<String, toml::Value>,
    rows: &mut Vec<CargoDependencyInfo>,
) {
    rows.extend(deps.keys().map(|name| CargoDependencyInfo {
        name: name.clone(),
        kind: kind.to_string(),
    }));
}

fn package_targets(manifest: &CargoManifest, package_name: &str) -> Vec<String> {
    let mut targets = Vec::new();
    if let Some(lib) = &manifest.lib {
        targets.push(format!(
            "lib:{}",
            lib.name.clone().unwrap_or_else(|| package_name.to_string())
        ));
    }
    targets.extend(
        manifest
            .bin
            .iter()
            .map(|bin| format!("bin:{}", bin.name.as_deref().unwrap_or(package_name))),
    );
    targets.sort();
    targets.dedup();
    targets
}

pub fn cargo_package_db_path(root: &Path, scope_id: &str) -> PathBuf {
    root.join(".tsift/indexes")
        .join("cargo")
        .join(scope_id)
        .join("index.db")
}

pub fn discover_cargo_inventory(root: &Path) -> Result<CargoInventory> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", root.display()))?;
    let manifest_paths = cargo_manifest_paths(&root)?;
    let mut parsed = Vec::new();
    for path in manifest_paths {
        let manifest = read_manifest(&path)?;
        parsed.push((path, manifest));
    }

    let mut workspaces = Vec::new();
    for (manifest_path, manifest) in &parsed {
        let Some(workspace) = &manifest.workspace else {
            continue;
        };
        let workspace_root = manifest_path
            .parent()
            .unwrap_or(root.as_path())
            .to_path_buf();
        let relative_root = relative_display(&root, &workspace_root);
        let id = if relative_root.is_empty() {
            "root".to_string()
        } else {
            relative_root.clone()
        };
        workspaces.push(CargoWorkspaceInfo {
            id,
            manifest_path: manifest_path.clone(),
            workspace_root,
            relative_manifest_path: relative_display(&root, manifest_path),
            relative_root,
            members: workspace.members.clone(),
            default_members: workspace.default_members.clone(),
        });
    }

    let mut package_roots_by_name: HashMap<String, Vec<String>> = HashMap::new();
    for (manifest_path, manifest) in &parsed {
        if let Some(package) = &manifest.package {
            let package_root = manifest_path
                .parent()
                .unwrap_or(root.as_path())
                .to_path_buf();
            package_roots_by_name
                .entry(package.name.clone())
                .or_default()
                .push(relative_display(&root, &package_root));
        }
    }

    let mut packages = Vec::new();
    for (manifest_path, manifest) in parsed {
        let Some(package) = &manifest.package else {
            continue;
        };
        let package_root = manifest_path
            .parent()
            .unwrap_or(root.as_path())
            .to_path_buf();
        let workspace_root = nearest_workspace_root(&package_root, &workspaces);
        let relative_root = relative_display(&root, &package_root);
        let scope_id = if package_roots_by_name
            .get(&package.name)
            .is_some_and(|roots| roots.len() > 1)
        {
            relative_root.clone()
        } else {
            package.name.clone()
        };
        let mut dependencies = Vec::new();
        dependency_rows("normal", &manifest.dependencies, &mut dependencies);
        dependency_rows("dev", &manifest.dev_dependencies, &mut dependencies);
        dependency_rows("build", &manifest.build_dependencies, &mut dependencies);
        dependencies
            .sort_by(|left, right| left.name.cmp(&right.name).then(left.kind.cmp(&right.kind)));
        dependencies.dedup();
        let mut features = manifest.features.keys().cloned().collect::<Vec<_>>();
        features.sort();
        packages.push(CargoPackageInfo {
            name: package.name.clone(),
            normalized_name: normalize_package_name(&package.name),
            scope_id,
            manifest_path: manifest_path.clone(),
            package_root: package_root.clone(),
            workspace_root: workspace_root.clone(),
            relative_manifest_path: relative_display(&root, &manifest_path),
            relative_root,
            relative_workspace_root: relative_display(&root, &workspace_root),
            features,
            targets: package_targets(&manifest, &package.name),
            dependencies,
        });
    }
    packages.sort_by(|left, right| left.relative_root.cmp(&right.relative_root));
    Ok(CargoInventory {
        workspaces,
        packages,
    })
}

fn package_matches_selector(package: &CargoPackageInfo, selector: &str) -> bool {
    package.name == selector
        || package.normalized_name == selector
        || package.scope_id == selector
        || package.relative_root == selector
        || package.relative_manifest_path == selector
}

pub fn find_cargo_package(root: &Path, selector: &str) -> Result<Option<CargoPackageInfo>> {
    let inventory = discover_cargo_inventory(root)?;
    let matches = inventory
        .packages
        .into_iter()
        .filter(|package| package_matches_selector(package, selector))
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => {
            let options = matches
                .iter()
                .map(|package| package.relative_root.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("ambiguous cargo package selector `{selector}`. Use one of: {options}");
        }
    }
}

pub fn infer_cargo_package_from_path(root: &Path, path: &Path) -> Result<Option<CargoPackageInfo>> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    let inventory = discover_cargo_inventory(root)?;
    Ok(inventory
        .packages
        .into_iter()
        .filter(|package| canonical.starts_with(&package.package_root))
        .max_by_key(|package| package.package_root.components().count()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn cargo_inventory_discovers_nested_workspace_packages() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            &dir.path().join("Cargo.toml"),
            r#"[workspace]
members = ["crates/core", "tools/cli"]
"#,
        );
        write_file(
            &dir.path().join("crates/core/Cargo.toml"),
            r#"[package]
name = "core-lib"

[features]
default = []

[dependencies]
serde = "1"
"#,
        );
        write_file(
            &dir.path().join("tools/cli/Cargo.toml"),
            r#"[package]
name = "workspace-cli"

[[bin]]
name = "workspace-cli"
"#,
        );

        let inventory = discover_cargo_inventory(dir.path()).unwrap();

        assert_eq!(inventory.workspaces.len(), 1);
        assert_eq!(inventory.packages.len(), 2);
        let core = inventory
            .packages
            .iter()
            .find(|package| package.name == "core-lib")
            .unwrap();
        assert_eq!(core.normalized_name, "core_lib");
        assert_eq!(core.relative_workspace_root, "");
        assert_eq!(core.features, vec!["default"]);
        assert_eq!(core.dependencies[0].name, "serde");
    }

    #[test]
    fn duplicate_cargo_package_names_use_relative_scope_ids() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            &dir.path().join("pkg/a/Cargo.toml"),
            "[package]\nname = \"shared\"\n",
        );
        write_file(
            &dir.path().join("vendor/shared/Cargo.toml"),
            "[package]\nname = \"shared\"\n",
        );

        let inventory = discover_cargo_inventory(dir.path()).unwrap();
        let ids = inventory
            .packages
            .iter()
            .map(|package| package.scope_id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["pkg/a", "vendor/shared"]);
        let err = find_cargo_package(dir.path(), "shared").unwrap_err();
        assert!(err.to_string().contains("ambiguous cargo package selector"));
        assert!(
            find_cargo_package(dir.path(), "vendor/shared")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn infer_cargo_package_prefers_deepest_root() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            &dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/core\"]\n",
        );
        write_file(
            &dir.path().join("crates/core/Cargo.toml"),
            "[package]\nname = \"core-lib\"\n",
        );
        write_file(
            &dir.path().join("crates/core/src/lib.rs"),
            "pub fn core() {}\n",
        );

        let package =
            infer_cargo_package_from_path(dir.path(), &dir.path().join("crates/core/src/lib.rs"))
                .unwrap()
                .unwrap();

        assert_eq!(package.name, "core-lib");
    }
}
