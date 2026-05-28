use std::path::Path;

pub fn relative_path_is_generated_artifact(relative: &str) -> bool {
    let relative = relative.trim_start_matches("./").replace('\\', "/");
    relative == ".tsift"
        || relative.starts_with(".tsift/")
        || relative.ends_with("/.tsift")
        || relative.contains("/.tsift/")
        || relative == ".agent-doc"
        || relative.starts_with(".agent-doc/")
        || relative.ends_with("/.agent-doc")
        || relative.contains("/.agent-doc/")
        || relative == "target"
        || relative.starts_with("target/")
        || relative.contains("/target/")
}

pub fn path_is_generated_artifact(root: &Path, source_root: &Path, path: &Path) -> bool {
    let mut relatives = Vec::new();
    if let Ok(relative) = path.strip_prefix(source_root) {
        relatives.push(relative.to_string_lossy().replace('\\', "/"));
    }
    if let Ok(relative) = path.strip_prefix(root) {
        relatives.push(relative.to_string_lossy().replace('\\', "/"));
    }
    if !path.is_absolute() {
        relatives.push(path.to_string_lossy().replace('\\', "/"));
    }
    relatives
        .iter()
        .any(|relative| relative_path_is_generated_artifact(relative))
}

pub fn index_snapshot_part_is_generated(root: &Path, source_root: &Path, part: &str) -> bool {
    let Some(rest) = part.strip_prefix("file:") else {
        return false;
    };
    let path = rest.split(':').next().unwrap_or(rest);
    path_is_generated_artifact(root, source_root, Path::new(path))
}

pub fn is_planner_config_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    normalized.starts_with(".github/")
        || normalized.starts_with(".codex/")
        || normalized.starts_with(".agent-doc/")
        || normalized.contains("/.github/")
        || matches!(
            file_name,
            "agents.md"
                | "claude.md"
                | "cargo.toml"
                | "cargo.lock"
                | "package.json"
                | "package-lock.json"
                | "pnpm-lock.yaml"
                | "yarn.lock"
                | "makefile"
                | "justfile"
                | "dockerfile"
                | "docker-compose.yml"
                | "docker-compose.yaml"
                | "config.toml"
                | "tsconfig.json"
        )
        || file_name.ends_with(".config.js")
        || file_name.ends_with(".config.ts")
        || file_name.ends_with(".yml")
        || file_name.ends_with(".yaml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn relative_path_is_tsift_dir() {
        assert!(relative_path_is_generated_artifact(".tsift"));
        assert!(relative_path_is_generated_artifact(".tsift/graph.db"));
        assert!(relative_path_is_generated_artifact("foo/.tsift"));
        assert!(relative_path_is_generated_artifact("foo/.tsift/bar"));
    }

    #[test]
    fn relative_path_is_agent_doc_dir() {
        assert!(relative_path_is_generated_artifact(".agent-doc"));
        assert!(relative_path_is_generated_artifact(".agent-doc/snapshots"));
    }

    #[test]
    fn relative_path_is_target_dir() {
        assert!(relative_path_is_generated_artifact("target"));
        assert!(relative_path_is_generated_artifact("target/debug"));
        assert!(relative_path_is_generated_artifact("foo/target/bar"));
    }

    #[test]
    fn relative_path_not_generated() {
        assert!(!relative_path_is_generated_artifact("src/main.rs"));
        assert!(!relative_path_is_generated_artifact("lib/index.ts"));
    }

    #[test]
    fn relative_path_normalizes_backslashes() {
        assert!(relative_path_is_generated_artifact("target\\debug"));
    }

    #[test]
    fn relative_path_strips_dot_slash() {
        assert!(relative_path_is_generated_artifact("./.tsift"));
    }

    #[test]
    fn path_is_generated_artifact_with_source_root() {
        let root = PathBuf::from("/project");
        let source_root = PathBuf::from("/project/src");
        let path = PathBuf::from("/project/src/.tsift/config");
        assert!(path_is_generated_artifact(&root, &source_root, &path));
    }

    #[test]
    fn path_is_generated_artifact_not_generated() {
        let root = PathBuf::from("/project");
        let source_root = PathBuf::from("/project/src");
        let path = PathBuf::from("/project/src/main.rs");
        assert!(!path_is_generated_artifact(&root, &source_root, &path));
    }

    #[test]
    fn index_snapshot_part_generated() {
        let root = PathBuf::from("/project");
        let source_root = PathBuf::from("/project");
        assert!(index_snapshot_part_is_generated(
            &root,
            &source_root,
            "file:target/debug/out.rs:line"
        ));
    }

    #[test]
    fn index_snapshot_part_not_generated() {
        let root = PathBuf::from("/project");
        let source_root = PathBuf::from("/project");
        assert!(!index_snapshot_part_is_generated(
            &root,
            &source_root,
            "file:src/main.rs:line"
        ));
    }

    #[test]
    fn index_snapshot_part_no_file_prefix() {
        let root = PathBuf::from("/project");
        let source_root = PathBuf::from("/project");
        assert!(!index_snapshot_part_is_generated(
            &root,
            &source_root,
            "scope:main"
        ));
    }

    #[test]
    fn is_planner_config_github() {
        assert!(is_planner_config_path(".github/workflows/ci.yml"));
    }

    #[test]
    fn is_planner_config_agents_md() {
        assert!(is_planner_config_path("AGENTS.md"));
    }

    #[test]
    fn is_planner_config_cargo() {
        assert!(is_planner_config_path("Cargo.toml"));
    }

    #[test]
    fn is_planner_config_yml_extension() {
        assert!(is_planner_config_path("config/production.yml"));
    }

    #[test]
    fn is_planner_config_config_ts() {
        assert!(is_planner_config_path("vite.config.ts"));
    }

    #[test]
    fn is_planner_config_not_config() {
        assert!(!is_planner_config_path("src/main.rs"));
        assert!(!is_planner_config_path("lib/index.ts"));
    }

    #[test]
    fn is_planner_config_case_insensitive() {
        assert!(is_planner_config_path(".GitHub/test.yml"));
    }

    #[test]
    fn is_planner_config_codex() {
        assert!(is_planner_config_path(".codex/hooks.json"));
    }
}
