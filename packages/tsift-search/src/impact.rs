use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tsift_digest::diff_digest::{self, DiffDigestOptions};
use tsift_graph::lang::Lang;
use tsift_index::{config, index, walk};
use tsift_quality::lint;
use tsift_summarize::summarize;

#[derive(Debug, Clone, Serialize)]
pub struct ImpactPhaseTiming {
    pub name: String,
    pub duration_micros: u128,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ImpactOptions<'a> {
    pub cached: bool,
    pub revision: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactTestTarget {
    pub path: String,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub symbols: Vec<String>,
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactReport {
    pub root: String,
    pub mode: diff_digest::DiffDigestMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub changed_files: Vec<String>,
    pub changed_symbols: Vec<String>,
    pub affected_tests_total: usize,
    pub affected_tests: Vec<ImpactTestTarget>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct ImpactTargetBuilder {
    reasons: BTreeSet<String>,
    symbols: BTreeSet<String>,
}

impl ImpactTargetBuilder {
    fn add_reason(&mut self, reason: impl Into<String>) {
        self.reasons.insert(reason.into());
    }

    fn add_symbol(&mut self, symbol: impl Into<String>) {
        self.symbols.insert(symbol.into());
    }
}

pub fn compute(path: &Path, options: ImpactOptions<'_>) -> Result<ImpactReport> {
    compute_with_phases(path, options).map(|(report, _phases)| report)
}

pub fn compute_with_phases(
    path: &Path,
    options: ImpactOptions<'_>,
) -> Result<(ImpactReport, Vec<ImpactPhaseTiming>)> {
    let mut phases: Vec<ImpactPhaseTiming> = Vec::with_capacity(7);

    let context_started = Instant::now();
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let source_root = impact_source_root(&root, path, options.scope)?;
    phases.push(ImpactPhaseTiming {
        name: "context_resolution".to_string(),
        duration_micros: context_started.elapsed().as_micros(),
        detail: "project root and source root resolution".to_string(),
    });

    let diff_started = Instant::now();
    let diff = diff_digest::compute(
        &root,
        DiffDigestOptions {
            cached: options.cached,
            revision: options.revision,
            pathspecs: &[],
            max_parsed_files: None,
        },
    )?;
    phases.push(ImpactPhaseTiming {
        name: "diff_digest".to_string(),
        duration_micros: diff_started.elapsed().as_micros(),
        detail: "diff_digest::compute call for changed files/symbols".to_string(),
    });

    let mut warnings = Vec::new();
    let changed_files = diff
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    let changed_symbols = diff
        .files
        .iter()
        .flat_map(|file| file.touched_symbols.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let changed_symbol_set = changed_symbols
        .iter()
        .map(|symbol| symbol.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let changed_tokens = changed_reference_tokens(&changed_files, &changed_symbols);

    let test_scan_started = Instant::now();
    let mut targets = BTreeMap::<String, ImpactTargetBuilder>::new();
    for file in &diff.files {
        let abs = root.join(&file.path);
        if is_test_path(&file.path) || file_contains_inline_tests(&abs) {
            let entry = targets.entry(file.path.clone()).or_default();
            entry.add_reason("changed test-bearing file");
            for symbol in &file.touched_symbols {
                entry.add_symbol(symbol);
            }
        }
    }
    phases.push(ImpactPhaseTiming {
        name: "test_path_scan".to_string(),
        duration_micros: test_scan_started.elapsed().as_micros(),
        detail: "per-changed-file test path / inline test classification".to_string(),
    });

    let index_started = Instant::now();
    let db_path = impact_db_path(&root, path, options.scope);
    let mut call_edge_micros: u128 = 0;
    let mut route_handler_micros: u128 = 0;
    match db_path {
        Ok(db_path) if db_path.exists() => {
            match index::IndexDb::open_read_only_resilient(&db_path) {
                Ok(db) => {
                    let call_started = Instant::now();
                    add_call_edge_impacts(&root, &db, &changed_symbol_set, &mut targets)?;
                    call_edge_micros = call_started.elapsed().as_micros();
                    let route_started = Instant::now();
                    add_route_handler_impacts(&root, &db, &changed_symbol_set, &mut targets)?;
                    route_handler_micros = route_started.elapsed().as_micros();
                }
                Err(err) => warnings.push(format!(
                    "call-edge impact unavailable from {}: {err:#}",
                    db_path.display()
                )),
            }
        }
        Ok(db_path) => warnings.push(format!(
            "call-edge impact unavailable: no index found at {}",
            db_path.display()
        )),
        Err(err) => warnings.push(format!("call-edge impact unavailable: {err:#}")),
    }
    let index_total_micros = index_started.elapsed().as_micros();
    let index_open_micros = index_total_micros
        .saturating_sub(call_edge_micros)
        .saturating_sub(route_handler_micros);
    phases.push(ImpactPhaseTiming {
        name: "index_open".to_string(),
        duration_micros: index_open_micros,
        detail: "index db path resolution and read-only open".to_string(),
    });
    phases.push(ImpactPhaseTiming {
        name: "call_edge_impacts".to_string(),
        duration_micros: call_edge_micros,
        detail: "add_call_edge_impacts SQL/walk for symbol caller expansion".to_string(),
    });
    phases.push(ImpactPhaseTiming {
        name: "route_handler_impacts".to_string(),
        duration_micros: route_handler_micros,
        detail: "add_route_handler_impacts route/handler annotation".to_string(),
    });

    let import_started = Instant::now();
    add_import_impacts(&root, &source_root, &changed_tokens, &mut targets)?;
    phases.push(ImpactPhaseTiming {
        name: "import_impacts".to_string(),
        duration_micros: import_started.elapsed().as_micros(),
        detail: "add_import_impacts source-root walk for token references".to_string(),
    });

    let assembly_started = Instant::now();
    let mut affected_tests = targets
        .into_iter()
        .map(|(path, builder)| ImpactTestTarget {
            commands: test_commands_for_path(&path),
            path,
            reasons: builder.reasons.into_iter().collect(),
            symbols: builder.symbols.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    affected_tests.sort_by(|left, right| {
        right
            .reasons
            .len()
            .cmp(&left.reasons.len())
            .then_with(|| left.path.cmp(&right.path))
    });
    let affected_tests_total = affected_tests.len();
    let limit = options.limit;
    let truncated = limit > 0 && affected_tests.len() > limit;
    if truncated {
        affected_tests.truncate(limit);
    }

    let report = ImpactReport {
        root: root.display().to_string(),
        mode: diff.mode,
        revision: diff.revision,
        changed_files,
        changed_symbols,
        affected_tests_total,
        affected_tests,
        truncated,
        warnings,
    };
    phases.push(ImpactPhaseTiming {
        name: "report_assembly".to_string(),
        duration_micros: assembly_started.elapsed().as_micros(),
        detail: "sort, truncate, and report construction".to_string(),
    });
    Ok((report, phases))
}

fn impact_source_root(root: &Path, path: &Path, scope: Option<&str>) -> Result<PathBuf> {
    if let Some(scope) = scope {
        return Ok(config::Config::resolve_submodule(root, scope)?.source_root);
    }
    if let Ok(Some(scope)) = config::Config::infer_submodule_from_path(root, path) {
        return Ok(scope.source_root);
    }
    Ok(root.to_path_buf())
}

fn impact_db_path(root: &Path, path: &Path, scope: Option<&str>) -> Result<PathBuf> {
    if let Some(scope_name) = scope {
        let cfg = config::Config::load(root)?;
        let scope = config::Config::resolve_submodule(root, scope_name)?;
        return Ok(cfg.db_path_for(root, &scope.id));
    }
    if let Ok(Some(scope)) = config::Config::infer_submodule_from_path(root, path) {
        let cfg = config::Config::load(root)?;
        let scoped = cfg.db_path_for(root, &scope.id);
        if scoped.exists() {
            return Ok(scoped);
        }
    }
    Ok(root.join(".tsift/index.db"))
}

fn rel_path(root: &Path, path: &Path) -> String {
    summarize::normalize_summary_file_key(path.strip_prefix(root).unwrap_or(path))
}

fn changed_reference_tokens(files: &[String], symbols: &[String]) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    for file in files {
        let path = Path::new(file);
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            tokens.insert(stem.to_ascii_lowercase());
        }
        let without_ext = path.with_extension("");
        let path_token = summarize::normalize_summary_file_key(&without_ext);
        tokens.insert(path_token.to_ascii_lowercase());
        tokens.insert(path_token.replace('/', "::").to_ascii_lowercase());
        tokens.insert(path_token.replace('/', ".").to_ascii_lowercase());
    }
    for symbol in symbols {
        tokens.insert(symbol.to_ascii_lowercase());
    }
    tokens
}

fn is_test_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/").to_ascii_lowercase();
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    normalized.starts_with("tests/")
        || normalized.contains("/tests/")
        || normalized.contains("__tests__/")
        || file_name.contains("_test.")
        || file_name.contains(".test.")
        || file_name.contains("_spec.")
        || file_name.contains(".spec.")
}

fn file_contains_inline_tests(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|content| {
            content.contains("#[cfg(test)]")
                || content.contains("#[test]")
                || content.contains("def test_")
                || content.contains("describe(")
                || content.contains("it(")
        })
        .unwrap_or(false)
}

fn add_call_edge_impacts(
    root: &Path,
    db: &index::IndexDb,
    changed_symbols: &BTreeSet<String>,
    targets: &mut BTreeMap<String, ImpactTargetBuilder>,
) -> Result<()> {
    if changed_symbols.is_empty() {
        return Ok(());
    }
    for edge in db.all_stored_edges()? {
        if !changed_symbols.contains(&edge.callee_name.to_ascii_lowercase()) {
            continue;
        }
        let path = rel_path(root, Path::new(&edge.caller_file));
        if !is_test_path(&path) && !file_contains_inline_tests(&root.join(&path)) {
            continue;
        }
        let entry = targets.entry(path).or_default();
        entry.add_reason(format!(
            "call graph reaches changed symbol {}",
            edge.callee_name
        ));
        entry.add_symbol(edge.callee_name);
    }
    Ok(())
}

fn add_route_handler_impacts(
    root: &Path,
    db: &index::IndexDb,
    changed_symbols: &BTreeSet<String>,
    targets: &mut BTreeMap<String, ImpactTargetBuilder>,
) -> Result<()> {
    if changed_symbols.is_empty() {
        return Ok(());
    }
    for route in db.all_routes()? {
        if !changed_symbols.contains(&route.handler_name.to_ascii_lowercase()) {
            continue;
        }
        let path = rel_path(root, Path::new(&route.file));
        let entry = targets.entry(path).or_default();
        entry.add_reason(format!(
            "route {} handled by changed symbol {}",
            route.route_path, route.handler_name
        ));
        entry.add_symbol(route.handler_name);
    }
    Ok(())
}

fn add_import_impacts(
    root: &Path,
    source_root: &Path,
    changed_tokens: &BTreeSet<String>,
    targets: &mut BTreeMap<String, ImpactTargetBuilder>,
) -> Result<()> {
    if changed_tokens.is_empty() {
        return Ok(());
    }
    for entry in walk::walk_files(source_root)? {
        let path = rel_path(root, &entry.path);
        if !is_test_path(&path) && !file_contains_inline_tests(&entry.path) {
            continue;
        }
        let content = std::fs::read_to_string(&entry.path)
            .with_context(|| format!("reading {}", entry.path.display()))?;
        let matched = import_tokens_for_file(entry.lang, &content, changed_tokens);
        if matched.is_empty() {
            continue;
        }
        let target = targets.entry(path).or_default();
        for token in matched {
            target.add_reason(format!("imports changed module or symbol {token}"));
        }
    }
    Ok(())
}

fn import_tokens_for_file(
    lang: Lang,
    content: &str,
    changed_tokens: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut matched = BTreeSet::new();
    for line in content.lines() {
        if !is_import_line(lang, line) {
            continue;
        }
        let lowered = line.to_ascii_lowercase();
        for token in changed_tokens {
            if token.len() >= 3 && lowered.contains(token) {
                matched.insert(token.clone());
            }
        }
    }
    matched
}

fn is_import_line(lang: Lang, line: &str) -> bool {
    let trimmed = line.trim_start();
    match lang {
        #[cfg(feature = "lang-rust")]
        Lang::Rust => trimmed.starts_with("use ") || trimmed.starts_with("mod "),
        #[cfg(feature = "lang-python")]
        Lang::Python => trimmed.starts_with("import ") || trimmed.starts_with("from "),
        #[cfg(feature = "lang-typescript")]
        Lang::TypeScript | Lang::Tsx => {
            trimmed.starts_with("import ") || trimmed.contains("require(")
        }
        #[cfg(feature = "lang-javascript")]
        Lang::JavaScript | Lang::Jsx => {
            trimmed.starts_with("import ") || trimmed.contains("require(")
        }
        // Go imports are usually a parenthesized block, so the `import` keyword
        // is on its own line and each dependency is a bare quoted path.
        #[cfg(feature = "lang-go")]
        Lang::Go => {
            trimmed.starts_with("import ")
                || trimmed.starts_with("import(")
                || (trimmed.starts_with('"') && trimmed.ends_with("\""))
                || trimmed
                    .split_once(' ')
                    .is_some_and(|(_alias, rest)| rest.starts_with('"') && rest.ends_with('"'))
        }
        // GDScript has no `import`: a script pulls in another script by
        // extending it or by `preload`/`load`ing a `res://` path.
        #[cfg(feature = "lang-gdscript")]
        Lang::GdScript => {
            trimmed.starts_with("extends ")
                || trimmed.contains("preload(")
                || trimmed.contains("load(\"res://")
        }
        _ => false,
    }
}

fn test_commands_for_path(path: &str) -> Vec<String> {
    let normalized = path.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    let stem = file_name.split('.').next().unwrap_or(file_name);
    if normalized.ends_with(".py") {
        vec![format!("pytest {}", shell_quote(&normalized))]
    } else if normalized.ends_with(".ts")
        || normalized.ends_with(".tsx")
        || normalized.ends_with(".js")
        || normalized.ends_with(".jsx")
    {
        vec![format!("npm test -- {}", shell_quote(&normalized))]
    } else if normalized.starts_with("tests/") && normalized.ends_with(".rs") {
        vec![format!("cargo test --test {}", shell_quote(stem))]
    } else {
        vec![format!("cargo test {}", shell_quote(stem))]
    }
}

fn shell_quote(input: &str) -> String {
    if input.is_empty() {
        return "''".to_string();
    }
    if input
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        return input.to_string();
    }
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn init_git_repo(path: &Path) {
        let status = Command::new("git")
            .args(["init"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

        let status = Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git add failed");

        let status = Command::new("git")
            .args([
                "-c",
                "user.name=tsift-tests",
                "-c",
                "user.email=tsift-tests@example.com",
                "commit",
                "--quiet",
                "-m",
                "init",
            ])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git commit failed");
    }

    #[test]
    fn impact_reports_tests_from_call_edges_and_imports() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("tests")).unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn add(left: i32, right: i32) -> i32 { left + right }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("tests/add_test.rs"),
            "use tsift_runner_fixture::add;\n#[test]\nfn adds_numbers() { let value = add(1, 2); assert_eq!(value, 3); }\n",
        )
        .unwrap();
        init_git_repo(dir.path());
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn add(left: i32, right: i32) -> i32 { left + right + 1 }\n",
        )
        .unwrap();

        let report = compute(
            dir.path(),
            ImpactOptions {
                limit: 10,
                ..ImpactOptions::default()
            },
        )
        .unwrap();

        let target = report
            .affected_tests
            .iter()
            .find(|target| target.path == "tests/add_test.rs")
            .expect("expected integration test target");
        assert!(
            target
                .reasons
                .iter()
                .any(|reason| reason.contains("call graph reaches changed symbol add")),
            "expected call-edge reason, got {:?}",
            target.reasons
        );
        assert!(
            target
                .reasons
                .iter()
                .any(|reason| reason.contains("imports changed module or symbol add")),
            "expected import reason, got {:?}",
            target.reasons
        );
        assert_eq!(target.commands, vec!["cargo test --test add_test"]);
    }

    #[test]
    fn import_tokens_only_consider_import_lines() {
        let mut tokens = BTreeSet::new();
        tokens.insert("api".to_string());
        let matched = import_tokens_for_file(
            Lang::Python,
            "value = 'api'\nfrom src.api import handler\n",
            &tokens,
        );
        assert_eq!(matched.into_iter().collect::<Vec<_>>(), vec!["api"]);
    }

    #[cfg(feature = "lang-gdscript")]
    #[test]
    fn gdscript_import_lines_are_extends_and_preload() {
        assert!(is_import_line(
            Lang::GdScript,
            "extends \"res://player.gd\""
        ));
        assert!(is_import_line(
            Lang::GdScript,
            "const Bullet = preload(\"res://bullet.gd\")"
        ));
        assert!(is_import_line(
            Lang::GdScript,
            "\tvar scene = load(\"res://level.tscn\")"
        ));
        // A method that merely ends in `load` is not an import.
        assert!(!is_import_line(Lang::GdScript, "\tdownloader.load(path)"));
        assert!(!is_import_line(Lang::GdScript, "\tvar health = 100"));
    }
}
