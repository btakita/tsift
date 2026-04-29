use crate::{config, index::IndexDb};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct LintResult {
    pub file: String,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Annotation {
    pub line: usize,
    pub column: usize,
    pub text: String,
    pub entity: String,
    pub kind: AnnotationKind,
    pub suggestion: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnotationKind {
    Symbol,
    Heading,
    Bold,
}

pub fn lint_markdown(path: &Path, entities: &HashSet<String>) -> Result<LintResult> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let mut annotations = Vec::new();
    let mut in_code_block = false;

    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        if trimmed.starts_with('#') || trimmed.starts_with("<!--") {
            continue;
        }

        find_unannotated(line, line_idx + 1, entities, &mut annotations);
    }

    Ok(LintResult {
        file: path.display().to_string(),
        annotations,
    })
}

fn find_unannotated(
    line: &str,
    line_num: usize,
    entities: &HashSet<String>,
    annotations: &mut Vec<Annotation>,
) {
    for entity in entities {
        let mut search_from = 0;
        while let Some(pos) = line[search_from..].find(entity.as_str()) {
            let abs_pos = search_from + pos;

            if is_already_annotated(line, abs_pos, entity.len()) {
                search_from = abs_pos + entity.len();
                continue;
            }

            if !is_word_boundary(line, abs_pos, entity.len()) {
                search_from = abs_pos + entity.len();
                continue;
            }

            let kind = guess_annotation_kind(entity);
            let suggestion = match kind {
                AnnotationKind::Symbol => format!("`{}`", entity),
                AnnotationKind::Bold => format!("**{}**", entity),
                AnnotationKind::Heading => {
                    format!("[{}](#{})", entity, entity.to_lowercase().replace(' ', "-"))
                }
            };

            annotations.push(Annotation {
                line: line_num,
                column: abs_pos + 1,
                text: entity.clone(),
                entity: entity.clone(),
                kind,
                suggestion,
            });

            search_from = abs_pos + entity.len();
        }
    }
}

fn is_already_annotated(line: &str, pos: usize, len: usize) -> bool {
    let before = if pos > 0 { &line[..pos] } else { "" };
    let after_end = pos + len;
    let after = if after_end < line.len() {
        &line[after_end..]
    } else {
        ""
    };

    // backtick-wrapped
    if before.ends_with('`') && after.starts_with('`') {
        return true;
    }
    // bold-wrapped
    if before.ends_with("**") && after.starts_with("**") {
        return true;
    }
    // link text
    if before.ends_with('[') && after.starts_with("](") {
        return true;
    }
    // inside inline code span
    let backtick_count_before = before.chars().filter(|&c| c == '`').count();
    if backtick_count_before % 2 == 1 {
        return true;
    }

    false
}

fn is_word_boundary(line: &str, pos: usize, len: usize) -> bool {
    let before_ok = pos == 0
        || line
            .as_bytes()
            .get(pos - 1)
            .is_none_or(|&b| !b.is_ascii_alphanumeric() && b != b'_');
    let after_end = pos + len;
    let after_ok = after_end >= line.len()
        || line
            .as_bytes()
            .get(after_end)
            .is_none_or(|&b| !b.is_ascii_alphanumeric() && b != b'_');
    before_ok && after_ok
}

fn guess_annotation_kind(entity: &str) -> AnnotationKind {
    if entity.contains('_')
        || entity.contains("::")
        || entity.chars().all(|c| c.is_ascii_lowercase() || c == '_')
    {
        AnnotationKind::Symbol
    } else if entity.chars().next().is_some_and(|c| c.is_uppercase()) && entity.contains(' ') {
        AnnotationKind::Heading
    } else {
        AnnotationKind::Bold
    }
}

fn project_root_from_canonical_path(canonical: &Path) -> Option<PathBuf> {
    let start = if canonical.is_dir() {
        canonical.to_path_buf()
    } else {
        canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| canonical.to_path_buf())
    };

    for ancestor in start.ancestors() {
        if ancestor.join(".tsift").is_dir() || ancestor.join(".gitmodules").is_file() {
            return Some(ancestor.to_path_buf());
        }
    }

    None
}

pub fn find_project_root_for_path(path: &Path) -> Result<Option<PathBuf>> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    Ok(project_root_from_canonical_path(&canonical))
}

pub fn resolve_project_root_or_canonical_path(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    Ok(project_root_from_canonical_path(&canonical).unwrap_or(canonical))
}

pub fn collect_entities_from_db(db_path: &Path) -> Result<HashSet<String>> {
    Ok(IndexDb::symbol_names_read_only_min_len(db_path, 4)?
        .into_iter()
        .collect())
}

pub fn collect_entities_from_index_path(index_path: &Path) -> Result<HashSet<String>> {
    if let Some(root) = workspace_root_for_aggregate_index_path(index_path)? {
        return collect_entities_from_workspace_root(&root);
    }

    let mut entities = HashSet::new();

    for db_path in discover_index_dbs(index_path)? {
        entities.extend(collect_entities_from_db(&db_path)?);
    }

    Ok(entities)
}

pub fn collect_entities_from_workspace_root(root: &Path) -> Result<HashSet<String>> {
    let mut entities = HashSet::new();

    push_entities_if_exists(&mut entities, &root.join(".tsift/index.db"))?;

    let cfg = config::Config::load(root)?;
    for scope in config::Config::submodule_dirs(root)? {
        if !cfg.federation_for_scope(&scope) {
            continue;
        }
        push_entities_if_exists(&mut entities, &cfg.db_path_for(root, &scope.id))?;
    }

    Ok(entities)
}

fn workspace_root_for_aggregate_index_path(index_path: &Path) -> Result<Option<PathBuf>> {
    if !index_path.exists() {
        return Ok(None);
    }

    let canonical = index_path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", index_path.display()))?;
    let Some(root) = project_root_from_canonical_path(&canonical) else {
        return Ok(None);
    };
    if config::Config::submodule_dirs(&root)?.is_empty() {
        return Ok(None);
    }

    let is_workspace_aggregate_target = canonical == root
        || canonical == root.join(".tsift")
        || canonical == root.join(".tsift/index.db")
        || canonical == root.join(".tsift/indexes");

    Ok(is_workspace_aggregate_target.then_some(root))
}

fn discover_index_dbs(index_path: &Path) -> Result<Vec<PathBuf>> {
    let mut dbs = BTreeSet::new();

    if index_path.is_file()
        && index_path.file_name().and_then(|name| name.to_str()) == Some("index.db")
    {
        dbs.insert(index_path.to_path_buf());
    }

    if index_path.is_dir()
        && index_path.file_name().and_then(|name| name.to_str()) == Some("indexes")
    {
        collect_child_index_dbs(&mut dbs, index_path)?;
    }

    push_if_exists(&mut dbs, &index_path.join("index.db"));
    push_if_exists(&mut dbs, &index_path.join(".tsift/index.db"));
    collect_child_index_dbs(&mut dbs, &index_path.join("indexes"))?;
    collect_child_index_dbs(&mut dbs, &index_path.join(".tsift/indexes"))?;

    Ok(dbs.into_iter().collect())
}

fn push_if_exists(dbs: &mut BTreeSet<PathBuf>, db_path: &Path) {
    if db_path.is_file() {
        dbs.insert(db_path.to_path_buf());
    }
}

fn push_entities_if_exists(entities: &mut HashSet<String>, db_path: &Path) -> Result<()> {
    if db_path.is_file() {
        entities.extend(collect_entities_from_db(db_path)?);
    }
    Ok(())
}

fn collect_child_index_dbs(dbs: &mut BTreeSet<PathBuf>, indexes_dir: &Path) -> Result<()> {
    if !indexes_dir.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(indexes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            push_if_exists(dbs, &path.join("index.db"));
            collect_child_index_dbs(dbs, &path)?;
        }
    }

    Ok(())
}

pub fn collect_entities_from_markdown(path: &Path) -> Result<HashSet<String>> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let mut entities = HashSet::new();

    for line in content.lines() {
        let trimmed = line.trim();
        // headings
        if let Some(heading) = trimmed.strip_prefix('#') {
            let heading = heading.trim_start_matches('#').trim();
            if heading.len() >= 4 {
                entities.insert(heading.to_string());
            }
        }
        // bold terms
        let mut in_bold = false;
        for part in trimmed.split("**") {
            if in_bold && part.len() >= 4 {
                entities.insert(part.to_string());
            }
            in_bold = !in_bold;
        }
        // backtick terms
        let mut in_code = false;
        for part in trimmed.split('`') {
            if in_code && part.len() >= 4 {
                entities.insert(part.to_string());
            }
            in_code = !in_code;
        }
    }

    Ok(entities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_symbol_index(db_path: &Path, names: &[&str]) {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }

        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch("CREATE TABLE symbols (name TEXT NOT NULL);")
            .unwrap();
        for name in names {
            conn.execute("INSERT INTO symbols (name) VALUES (?1)", [name])
                .unwrap();
        }
    }

    #[test]
    fn find_unannotated_plain_text() {
        let entities: HashSet<String> = ["scan_skills".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated(
            "The scan_skills function works.",
            1,
            &entities,
            &mut annotations,
        );
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].text, "scan_skills");
        assert_eq!(annotations[0].column, 5);
        assert_eq!(annotations[0].suggestion, "`scan_skills`");
    }

    #[test]
    fn skip_already_backtick_wrapped() {
        let entities: HashSet<String> = ["scan_skills".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated(
            "The `scan_skills` function works.",
            1,
            &entities,
            &mut annotations,
        );
        assert!(annotations.is_empty());
    }

    #[test]
    fn skip_already_bold_wrapped() {
        let entities: HashSet<String> = ["AuditResult".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated(
            "The **AuditResult** struct.",
            1,
            &entities,
            &mut annotations,
        );
        assert!(annotations.is_empty());
    }

    #[test]
    fn skip_link_text() {
        let entities: HashSet<String> = ["SPEC".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated(
            "See [SPEC](spec.md) for details.",
            1,
            &entities,
            &mut annotations,
        );
        assert!(annotations.is_empty());
    }

    #[test]
    fn word_boundary_prevents_partial_match() {
        let entities: HashSet<String> = ["scan".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated("The scanning process.", 1, &entities, &mut annotations);
        assert!(annotations.is_empty());
    }

    #[test]
    fn multiple_occurrences_on_same_line() {
        let entities: HashSet<String> = ["test".to_string()].into();
        let mut annotations = Vec::new();
        find_unannotated(
            "Run test then check test output.",
            1,
            &entities,
            &mut annotations,
        );
        assert_eq!(annotations.len(), 2);
    }

    #[test]
    fn lint_skips_code_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(
            &file,
            "# Doc\n\nscan_skills works.\n\n```\nscan_skills in code block\n```\n",
        )
        .unwrap();
        let entities: HashSet<String> = ["scan_skills".to_string()].into();
        let result = lint_markdown(&file, &entities).unwrap();
        assert_eq!(result.annotations.len(), 1);
        assert_eq!(result.annotations[0].line, 3);
    }

    #[test]
    fn lint_skips_headings_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(
            &file,
            "# scan_skills\n\n<!-- scan_skills -->\n\nPlain scan_skills here.\n",
        )
        .unwrap();
        let entities: HashSet<String> = ["scan_skills".to_string()].into();
        let result = lint_markdown(&file, &entities).unwrap();
        assert_eq!(result.annotations.len(), 1);
        assert_eq!(result.annotations[0].line, 5);
    }

    #[test]
    fn collect_entities_from_markdown_extracts_headings_bold_code() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.md");
        fs::write(
            &file,
            "# Architecture\n\nThe **AuditResult** struct uses `scan_skills` internally.\n",
        )
        .unwrap();
        let entities = collect_entities_from_markdown(&file).unwrap();
        assert!(entities.contains("Architecture"));
        assert!(entities.contains("AuditResult"));
        assert!(entities.contains("scan_skills"));
    }

    #[test]
    fn guess_annotation_kind_symbols() {
        assert!(matches!(
            guess_annotation_kind("scan_skills"),
            AnnotationKind::Symbol
        ));
        assert!(matches!(
            guess_annotation_kind("std::path"),
            AnnotationKind::Symbol
        ));
    }

    #[test]
    fn guess_annotation_kind_headings() {
        assert!(matches!(
            guess_annotation_kind("Audit Result"),
            AnnotationKind::Heading
        ));
    }

    #[test]
    fn guess_annotation_kind_bold() {
        assert!(matches!(
            guess_annotation_kind("AuditResult"),
            AnnotationKind::Bold
        ));
    }

    #[test]
    fn find_project_root_uses_markdown_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".tsift")).unwrap();
        let file = dir.path().join("docs/README.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "alpha_helper should be annotated.\n").unwrap();

        let root = find_project_root_for_path(&file).unwrap();

        assert_eq!(root.unwrap(), dir.path());
    }

    #[test]
    fn resolve_project_root_or_canonical_path_promotes_nested_subdir_to_tsift_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".tsift")).unwrap();
        let nested = dir.path().join("src/nested");
        fs::create_dir_all(&nested).unwrap();

        let root = resolve_project_root_or_canonical_path(&nested).unwrap();

        assert_eq!(root, dir.path());
    }

    #[test]
    fn resolve_project_root_or_canonical_path_promotes_nested_workspace_subdir_to_gitmodules_root()
    {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
"#,
        )
        .unwrap();
        let nested = dir.path().join("docs/nested");
        fs::create_dir_all(&nested).unwrap();

        let root = resolve_project_root_or_canonical_path(&nested).unwrap();

        assert_eq!(root, dir.path());
    }

    #[test]
    fn collect_entities_from_project_root_index_db() {
        let dir = tempfile::tempdir().unwrap();
        create_symbol_index(&dir.path().join(".tsift/index.db"), &["alpha_helper"]);

        let entities = collect_entities_from_index_path(dir.path()).unwrap();

        assert!(entities.contains("alpha_helper"));
    }

    #[test]
    fn collect_entities_from_scoped_index_dbs() {
        let dir = tempfile::tempdir().unwrap();
        create_symbol_index(
            &dir.path().join(".tsift/indexes/alpha/index.db"),
            &["alpha_helper"],
        );
        create_symbol_index(
            &dir.path().join(".tsift/indexes/beta/index.db"),
            &["beta_helper"],
        );

        let entities = collect_entities_from_index_path(dir.path()).unwrap();

        assert!(entities.contains("alpha_helper"));
        assert!(entities.contains("beta_helper"));
    }

    #[test]
    fn collect_entities_from_explicit_indexes_dir() {
        let dir = tempfile::tempdir().unwrap();
        create_symbol_index(
            &dir.path().join(".tsift/indexes/alpha/index.db"),
            &["alpha_helper"],
        );
        create_symbol_index(
            &dir.path().join(".tsift/indexes/beta/index.db"),
            &["beta_helper"],
        );

        let entities =
            collect_entities_from_index_path(&dir.path().join(".tsift/indexes")).unwrap();

        assert!(entities.contains("alpha_helper"));
        assert!(entities.contains("beta_helper"));
    }

    #[test]
    fn collect_entities_from_explicit_indexes_dir_recurses_nested_scope_ids() {
        let dir = tempfile::tempdir().unwrap();
        create_symbol_index(
            &dir.path().join(".tsift/indexes/pkg/app/foo/index.db"),
            &["pkg_helper"],
        );
        create_symbol_index(
            &dir.path().join(".tsift/indexes/vendor/foo/index.db"),
            &["vendor_helper"],
        );

        let entities =
            collect_entities_from_index_path(&dir.path().join(".tsift/indexes")).unwrap();

        assert!(entities.contains("pkg_helper"));
        assert!(entities.contains("vendor_helper"));
    }

    #[test]
    fn collect_entities_from_workspace_index_targets_skip_non_federated_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".tsift/indexes/public")).unwrap();
        fs::create_dir_all(root.join(".tsift/indexes/private")).unwrap();
        fs::create_dir_all(root.join(".tsift/indexes/isolated")).unwrap();
        fs::create_dir_all(root.join(".tsift/indexes/nonfed")).unwrap();
        fs::write(
            root.join(".gitmodules"),
            r#"[submodule "src/public"]
	path = src/public
	url = https://example.com/public
[submodule "src/private"]
	path = src/private
	url = https://example.com/private
[submodule "src/isolated"]
	path = src/isolated
	url = https://example.com/isolated
[submodule "src/nonfed"]
	path = src/nonfed
	url = https://example.com/nonfed
"#,
        )
        .unwrap();
        fs::write(
            root.join(".tsift/config.toml"),
            r#"
[overrides.private]
tier = "private"

[overrides.isolated]
tier = "isolated"

[overrides.nonfed]
federation = false
"#,
        )
        .unwrap();

        create_symbol_index(&root.join(".tsift/index.db"), &["root_helper"]);
        create_symbol_index(
            &root.join(".tsift/indexes/public/index.db"),
            &["public_helper"],
        );
        create_symbol_index(
            &root.join(".tsift/indexes/private/index.db"),
            &["private_helper"],
        );
        create_symbol_index(
            &root.join(".tsift/indexes/isolated/index.db"),
            &["isolated_helper"],
        );
        create_symbol_index(
            &root.join(".tsift/indexes/nonfed/index.db"),
            &["nonfed_helper"],
        );

        for target in [
            root.to_path_buf(),
            root.join(".tsift"),
            root.join(".tsift/indexes"),
        ] {
            let entities = collect_entities_from_index_path(&target).unwrap();

            assert!(entities.contains("root_helper"));
            assert!(entities.contains("public_helper"));
            assert!(!entities.contains("private_helper"));
            assert!(!entities.contains("isolated_helper"));
            assert!(!entities.contains("nonfed_helper"));
        }
    }

    #[test]
    fn collect_entities_from_workspace_root_ignores_repo_root_index_db() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".tsift")).unwrap();
        fs::write(
            root.join(".gitmodules"),
            r#"[submodule "src/public"]
	path = src/public
	url = https://example.com/public
"#,
        )
        .unwrap();
        create_symbol_index(&root.join(".tsift/index.db"), &["root_helper"]);
        let conn = rusqlite::Connection::open(root.join("index.db")).unwrap();
        conn.execute_batch("CREATE TABLE unrelated (id INTEGER PRIMARY KEY);")
            .unwrap();

        let entities = collect_entities_from_index_path(root).unwrap();

        assert!(entities.contains("root_helper"));
    }

    #[test]
    fn collect_entities_from_explicit_private_scope_dir_keeps_private_entities() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".tsift/indexes/private")).unwrap();
        fs::write(
            root.join(".gitmodules"),
            r#"[submodule "src/private"]
	path = src/private
	url = https://example.com/private
"#,
        )
        .unwrap();
        fs::write(
            root.join(".tsift/config.toml"),
            r#"
[overrides.private]
tier = "private"
"#,
        )
        .unwrap();
        create_symbol_index(
            &root.join(".tsift/indexes/private/index.db"),
            &["private_helper"],
        );

        let entities =
            collect_entities_from_index_path(&root.join(".tsift/indexes/private")).unwrap();

        assert!(entities.contains("private_helper"));
    }

    #[test]
    fn collect_entities_from_workspace_root_skips_non_federated_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".tsift")).unwrap();
        fs::write(
            root.join(".gitmodules"),
            r#"[submodule "src/public"]
	path = src/public
	url = https://example.com/public
[submodule "src/private"]
	path = src/private
	url = https://example.com/private
[submodule "src/isolated"]
	path = src/isolated
	url = https://example.com/isolated
[submodule "src/nonfed"]
	path = src/nonfed
	url = https://example.com/nonfed
"#,
        )
        .unwrap();
        fs::write(
            root.join(".tsift/config.toml"),
            r#"
[overrides.private]
tier = "private"

[overrides.isolated]
tier = "isolated"

[overrides.nonfed]
federation = false
"#,
        )
        .unwrap();

        create_symbol_index(&root.join(".tsift/index.db"), &["root_helper"]);
        create_symbol_index(
            &root.join(".tsift/indexes/public/index.db"),
            &["public_helper"],
        );
        create_symbol_index(
            &root.join(".tsift/indexes/private/index.db"),
            &["private_helper"],
        );
        create_symbol_index(
            &root.join(".tsift/indexes/isolated/index.db"),
            &["isolated_helper"],
        );
        create_symbol_index(
            &root.join(".tsift/indexes/nonfed/index.db"),
            &["nonfed_helper"],
        );

        let entities = collect_entities_from_workspace_root(root).unwrap();

        assert!(entities.contains("root_helper"));
        assert!(entities.contains("public_helper"));
        assert!(!entities.contains("private_helper"));
        assert!(!entities.contains("isolated_helper"));
        assert!(!entities.contains("nonfed_helper"));
    }

    #[test]
    fn collect_entities_uses_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(".tsift/index.db");
        create_symbol_index(&db_path, &["alpha_helper"]);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .unwrap();
        fs::write(format!("{}-journal", db_path.display()), "locked").unwrap();

        let entities = collect_entities_from_db(&db_path).unwrap();

        assert!(entities.contains("alpha_helper"));
    }
}
