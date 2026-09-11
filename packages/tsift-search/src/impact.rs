use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImpactEvidenceNode {
    pub symbol: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImpactEvidencePath {
    pub changed_symbol: String,
    pub target_symbol: String,
    pub target_file: String,
    pub nodes: Vec<ImpactEvidenceNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactProductionModule {
    pub module: String,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
    pub reasons: Vec<String>,
    pub evidence_paths: Vec<ImpactEvidencePath>,
    pub evidence_paths_total: usize,
    pub evidence_paths_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImpactTestSignal {
    pub path: String,
    pub kind: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_symbol: Option<String>,
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
    #[serde(default)]
    pub production_modules_total: usize,
    #[serde(default)]
    pub production_modules: Vec<ImpactProductionModule>,
    #[serde(default)]
    pub test_signals_total: usize,
    #[serde(default)]
    pub test_signals: Vec<ImpactTestSignal>,
    #[serde(default)]
    pub context_truncated: bool,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct ImpactTargetBuilder {
    reasons: BTreeSet<String>,
    symbols: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct ImpactProductionModuleBuilder {
    files: BTreeSet<String>,
    symbols: BTreeSet<String>,
    reasons: BTreeSet<String>,
    evidence_paths: BTreeSet<ImpactEvidencePath>,
}

impl ImpactProductionModuleBuilder {
    fn add_file(&mut self, file: impl Into<String>) {
        self.files.insert(file.into());
    }

    fn add_symbol(&mut self, symbol: impl Into<String>) {
        self.symbols.insert(symbol.into());
    }

    fn add_reason(&mut self, reason: impl Into<String>) {
        self.reasons.insert(reason.into());
    }

    fn add_evidence_path(&mut self, path: ImpactEvidencePath) {
        self.evidence_paths.insert(path);
    }
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
    let changed_file_set = changed_files.iter().cloned().collect::<BTreeSet<_>>();
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
    let mut changed_symbol_files = BTreeMap::<String, BTreeSet<String>>::new();
    for file in &diff.files {
        for symbol in &file.touched_symbols {
            changed_symbol_files
                .entry(symbol.to_ascii_lowercase())
                .or_default()
                .insert(file.path.clone());
        }
    }
    let changed_tokens = changed_reference_tokens(&changed_files, &changed_symbols);

    let test_scan_started = Instant::now();
    let mut targets = BTreeMap::<String, ImpactTargetBuilder>::new();
    let mut production_targets = BTreeMap::<String, ImpactProductionModuleBuilder>::new();
    let mut test_signals = BTreeSet::<ImpactTestSignal>::new();
    for file in &diff.files {
        let abs = root.join(&file.path);
        let test_path = is_test_path(&file.path);
        if test_path || file_contains_inline_tests(&abs) {
            let entry = targets.entry(file.path.clone()).or_default();
            entry.add_reason("changed test-bearing file");
            for symbol in &file.touched_symbols {
                entry.add_symbol(symbol);
            }
            test_signals.insert(ImpactTestSignal {
                path: file.path.clone(),
                kind: "changed_test".to_string(),
                detail: "changed test-bearing file".to_string(),
                changed_symbol: None,
            });
        }
        if !test_path {
            add_changed_production_file(&mut production_targets, &file.path, &file.touched_symbols);
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
                    let edges = db.all_stored_edges()?;
                    let call_started = Instant::now();
                    add_call_edge_impacts(
                        &root,
                        &edges,
                        &changed_symbol_set,
                        &mut targets,
                        &mut test_signals,
                    );
                    add_production_call_impacts(
                        &root,
                        &edges,
                        &changed_symbol_files,
                        &mut production_targets,
                    );
                    call_edge_micros = call_started.elapsed().as_micros();
                    let route_started = Instant::now();
                    add_route_handler_impacts(
                        &root,
                        &db,
                        &changed_symbol_set,
                        &mut targets,
                        &mut production_targets,
                        &mut test_signals,
                    )?;
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
    add_import_impacts(
        &root,
        &source_root,
        &changed_file_set,
        &changed_tokens,
        &mut targets,
        &mut production_targets,
        &mut test_signals,
    )?;
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

    let (production_modules_total, production_modules, production_context_truncated) =
        bounded_production_modules(production_targets, limit);
    let test_signals_total = test_signals.len();
    let mut test_signals = test_signals.into_iter().collect::<Vec<_>>();
    let test_signals_truncated = limit > 0 && test_signals.len() > limit;
    if test_signals_truncated {
        test_signals.truncate(limit);
    }
    let context_truncated = production_context_truncated || test_signals_truncated;

    let report = ImpactReport {
        root: root.display().to_string(),
        mode: diff.mode,
        revision: diff.revision,
        changed_files,
        changed_symbols,
        affected_tests_total,
        affected_tests,
        production_modules_total,
        production_modules,
        test_signals_total,
        test_signals,
        context_truncated,
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

fn production_module_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [
            group @ ("packages" | "crates" | "apps" | "services"),
            name,
            ..,
        ] => {
            format!("{group}/{name}")
        }
        [top, ..] if parts.len() > 1 => (*top).to_string(),
        _ => ".".to_string(),
    }
}

fn production_entry<'a>(
    modules: &'a mut BTreeMap<String, ImpactProductionModuleBuilder>,
    path: &str,
) -> &'a mut ImpactProductionModuleBuilder {
    modules.entry(production_module_name(path)).or_default()
}

fn add_changed_production_file(
    modules: &mut BTreeMap<String, ImpactProductionModuleBuilder>,
    path: &str,
    symbols: &[String],
) {
    let entry = production_entry(modules, path);
    entry.add_file(path);
    entry.add_reason("contains changed source");
    for symbol in symbols {
        entry.add_symbol(symbol);
        entry.add_evidence_path(ImpactEvidencePath {
            changed_symbol: symbol.clone(),
            target_symbol: symbol.clone(),
            target_file: path.to_string(),
            nodes: vec![ImpactEvidenceNode {
                symbol: symbol.clone(),
                file: path.to_string(),
                line: None,
            }],
        });
    }
}

fn add_call_edge_impacts(
    root: &Path,
    edges: &[index::StoredEdge],
    changed_symbols: &BTreeSet<String>,
    targets: &mut BTreeMap<String, ImpactTargetBuilder>,
    test_signals: &mut BTreeSet<ImpactTestSignal>,
) {
    if changed_symbols.is_empty() {
        return;
    }
    for edge in edges {
        if !changed_symbols.contains(&edge.callee_name.to_ascii_lowercase()) {
            continue;
        }
        let path = rel_path(root, Path::new(&edge.caller_file));
        if !is_test_path(&path) && !file_contains_inline_tests(&root.join(&path)) {
            continue;
        }
        let entry = targets.entry(path.clone()).or_default();
        entry.add_reason(format!(
            "call graph reaches changed symbol {}",
            edge.callee_name
        ));
        entry.add_symbol(&edge.callee_name);
        test_signals.insert(ImpactTestSignal {
            path,
            kind: "call_graph".to_string(),
            detail: format!(
                "{} calls changed symbol {}",
                edge.caller_name, edge.callee_name
            ),
            changed_symbol: Some(edge.callee_name.clone()),
        });
    }
}

const MAX_IMPACT_EVIDENCE_DEPTH: usize = 4;

fn add_production_call_impacts(
    root: &Path,
    edges: &[index::StoredEdge],
    changed_symbol_files: &BTreeMap<String, BTreeSet<String>>,
    modules: &mut BTreeMap<String, ImpactProductionModuleBuilder>,
) {
    let mut incoming = BTreeMap::<String, Vec<&index::StoredEdge>>::new();
    for edge in edges {
        incoming
            .entry(edge.callee_name.to_ascii_lowercase())
            .or_default()
            .push(edge);
    }
    for candidates in incoming.values_mut() {
        candidates.sort_by(|left, right| {
            left.caller_file
                .cmp(&right.caller_file)
                .then_with(|| left.caller_name.cmp(&right.caller_name))
                .then_with(|| left.call_site_line.cmp(&right.call_site_line))
        });
    }

    for (changed_key, seed_files) in changed_symbol_files {
        let changed_symbol = incoming
            .get(changed_key)
            .and_then(|candidates| candidates.first())
            .map(|edge| edge.callee_name.clone())
            .unwrap_or_else(|| changed_key.clone());
        let seed_file = seed_files.iter().next().cloned().unwrap_or_default();
        let seed = ImpactEvidenceNode {
            symbol: changed_symbol.clone(),
            file: seed_file,
            line: None,
        };
        let mut queue = VecDeque::from([(changed_key.clone(), vec![seed])]);
        let mut visited = BTreeSet::from([changed_key.clone()]);

        while let Some((callee, nodes)) = queue.pop_front() {
            if nodes.len() >= MAX_IMPACT_EVIDENCE_DEPTH {
                continue;
            }
            let Some(candidates) = incoming.get(&callee) else {
                continue;
            };
            for edge in candidates {
                let path = rel_path(root, Path::new(&edge.caller_file));
                let mut next_nodes = nodes.clone();
                next_nodes.push(ImpactEvidenceNode {
                    symbol: edge.caller_name.clone(),
                    file: path.clone(),
                    line: usize::try_from(edge.caller_line).ok(),
                });
                if !is_test_path(&path) {
                    let entry = production_entry(modules, &path);
                    entry.add_file(&path);
                    entry.add_symbol(&edge.caller_name);
                    entry.add_reason(format!("calls changed symbol {changed_symbol}"));
                    entry.add_evidence_path(ImpactEvidencePath {
                        changed_symbol: changed_symbol.clone(),
                        target_symbol: edge.caller_name.clone(),
                        target_file: path,
                        nodes: next_nodes.clone(),
                    });
                }
                let caller_key = edge.caller_name.to_ascii_lowercase();
                if next_nodes.len() <= MAX_IMPACT_EVIDENCE_DEPTH
                    && visited.insert(caller_key.clone())
                {
                    queue.push_back((caller_key, next_nodes));
                }
            }
        }
    }
}

fn add_route_handler_impacts(
    root: &Path,
    db: &index::IndexDb,
    changed_symbols: &BTreeSet<String>,
    targets: &mut BTreeMap<String, ImpactTargetBuilder>,
    modules: &mut BTreeMap<String, ImpactProductionModuleBuilder>,
    test_signals: &mut BTreeSet<ImpactTestSignal>,
) -> Result<()> {
    if changed_symbols.is_empty() {
        return Ok(());
    }
    for route in db.all_routes()? {
        if !changed_symbols.contains(&route.handler_name.to_ascii_lowercase()) {
            continue;
        }
        let path = rel_path(root, Path::new(&route.file));
        let detail = format!(
            "route {} handled by changed symbol {}",
            route.route_path, route.handler_name
        );
        let test_path = is_test_path(&path);
        if test_path || file_contains_inline_tests(&root.join(&path)) {
            let entry = targets.entry(path.clone()).or_default();
            entry.add_reason(&detail);
            entry.add_symbol(&route.handler_name);
            test_signals.insert(ImpactTestSignal {
                path: path.clone(),
                kind: "route".to_string(),
                detail: detail.clone(),
                changed_symbol: Some(route.handler_name.clone()),
            });
        }
        if !test_path {
            let entry = production_entry(modules, &path);
            entry.add_file(&path);
            entry.add_symbol(&route.handler_name);
            entry.add_reason(detail);
            entry.add_evidence_path(ImpactEvidencePath {
                changed_symbol: route.handler_name.clone(),
                target_symbol: route.handler_name.clone(),
                target_file: path.clone(),
                nodes: vec![ImpactEvidenceNode {
                    symbol: route.handler_name.clone(),
                    file: path,
                    line: usize::try_from(route.line).ok(),
                }],
            });
        }
    }
    Ok(())
}

fn add_import_impacts(
    root: &Path,
    source_root: &Path,
    changed_files: &BTreeSet<String>,
    changed_tokens: &BTreeSet<String>,
    targets: &mut BTreeMap<String, ImpactTargetBuilder>,
    modules: &mut BTreeMap<String, ImpactProductionModuleBuilder>,
    test_signals: &mut BTreeSet<ImpactTestSignal>,
) -> Result<()> {
    if changed_tokens.is_empty() {
        return Ok(());
    }
    for entry in walk::walk_files(source_root)? {
        let path = rel_path(root, &entry.path);
        if changed_files.contains(&path) {
            continue;
        }
        let content = std::fs::read_to_string(&entry.path)
            .with_context(|| format!("reading {}", entry.path.display()))?;
        let matched = import_tokens_for_file(entry.lang, &content, changed_tokens);
        if matched.is_empty() {
            continue;
        }
        let test_path = is_test_path(&path);
        if test_path || file_contains_inline_tests(&entry.path) {
            let target = targets.entry(path.clone()).or_default();
            for token in &matched {
                let detail = format!("imports changed module or symbol {token}");
                target.add_reason(&detail);
                test_signals.insert(ImpactTestSignal {
                    path: path.clone(),
                    kind: "import".to_string(),
                    detail,
                    changed_symbol: Some(token.clone()),
                });
            }
        }
        if !test_path {
            let target = production_entry(modules, &path);
            target.add_file(&path);
            for token in matched {
                target.add_symbol(&token);
                target.add_reason(format!("imports changed module or symbol {token}"));
            }
        }
    }
    Ok(())
}

fn bounded_production_modules(
    modules: BTreeMap<String, ImpactProductionModuleBuilder>,
    limit: usize,
) -> (usize, Vec<ImpactProductionModule>, bool) {
    let total = modules.len();
    let mut module_entries = modules.into_iter().collect::<Vec<_>>();
    let modules_truncated = limit > 0 && module_entries.len() > limit;
    if modules_truncated {
        module_entries.truncate(limit);
    }

    let mut remaining_paths = if limit == 0 { usize::MAX } else { limit };
    let mut context_truncated = modules_truncated;
    let modules = module_entries
        .into_iter()
        .map(|(module, builder)| {
            let evidence_paths_total = builder.evidence_paths.len();
            let take = evidence_paths_total.min(remaining_paths);
            let evidence_paths = builder
                .evidence_paths
                .into_iter()
                .take(take)
                .collect::<Vec<_>>();
            remaining_paths = remaining_paths.saturating_sub(take);
            let evidence_paths_truncated = evidence_paths.len() < evidence_paths_total;
            context_truncated |= evidence_paths_truncated;
            ImpactProductionModule {
                module,
                files: builder.files.into_iter().collect(),
                symbols: builder.symbols.into_iter().collect(),
                reasons: builder.reasons.into_iter().collect(),
                evidence_paths,
                evidence_paths_total,
                evidence_paths_truncated,
            }
        })
        .collect();
    (total, modules, context_truncated)
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
        #[cfg(feature = "lang-csharp")]
        Lang::CSharp => trimmed.starts_with("using ") || trimmed.starts_with("global using "),
        #[cfg(feature = "lang-c")]
        Lang::C => trimmed.starts_with("#include "),
        #[cfg(feature = "lang-cpp")]
        Lang::Cpp => trimmed.starts_with("#include ") || trimmed.starts_with("import "),
        #[cfg(feature = "lang-odin")]
        Lang::Odin => trimmed.starts_with("import ") || trimmed.starts_with("foreign import "),
        // Jai pulls in a module with `#import "Name"` (optionally bound as
        // `Name :: #import "Name"`) and a sibling file with `#load "file.jai"`.
        #[cfg(feature = "lang-jai")]
        Lang::Jai => {
            trimmed.starts_with("#import ")
                || trimmed.starts_with("#load ")
                || trimmed.contains(":: #import ")
        }
        // Luau pulls in a module with `require(...)`: a path string
        // (`require("./util")`) or a Roblox instance path
        // (`require(script.Parent.Foo)`), whose last segment is the module name.
        #[cfg(feature = "lang-luau")]
        Lang::Luau => trimmed.contains("require(") || trimmed.contains("require \""),
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
            dir.path().join("src/service.rs"),
            "use crate::add;\npub fn calculate() -> i32 { add(1, 2) }\n",
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
        assert!(report.test_signals.iter().any(|signal| {
            signal.path == "tests/add_test.rs"
                && signal.kind == "call_graph"
                && signal.changed_symbol.as_deref() == Some("add")
        }));

        let production = report
            .production_modules
            .iter()
            .find(|module| module.module == "src")
            .expect("expected grouped src production module");
        assert!(production.files.contains(&"src/lib.rs".to_string()));
        assert!(production.files.contains(&"src/service.rs".to_string()));
        let path = production
            .evidence_paths
            .iter()
            .find(|path| path.target_symbol == "calculate")
            .expect("expected reverse call evidence for calculate");
        assert_eq!(
            path.nodes
                .iter()
                .map(|node| node.symbol.as_str())
                .collect::<Vec<_>>(),
            vec!["add", "calculate"]
        );
    }

    #[test]
    fn production_context_uses_one_shared_limit_for_modules_and_paths() {
        let mut modules = BTreeMap::new();
        for (module, file) in [("apps/a", "apps/a/src.rs"), ("apps/b", "apps/b/src.rs")] {
            let entry = modules
                .entry(module.to_string())
                .or_insert_with(ImpactProductionModuleBuilder::default);
            entry.add_file(file);
            for symbol in ["one", "two"] {
                entry.add_evidence_path(ImpactEvidencePath {
                    changed_symbol: symbol.to_string(),
                    target_symbol: symbol.to_string(),
                    target_file: file.to_string(),
                    nodes: Vec::new(),
                });
            }
        }

        let (total, bounded, truncated) = bounded_production_modules(modules, 1);
        assert_eq!(total, 2);
        assert_eq!(bounded.len(), 1);
        assert_eq!(bounded[0].evidence_paths.len(), 1);
        assert_eq!(bounded[0].evidence_paths_total, 2);
        assert!(bounded[0].evidence_paths_truncated);
        assert!(truncated);
    }

    #[test]
    fn production_module_names_group_workspace_units() {
        assert_eq!(
            production_module_name("packages/search/src/lib.rs"),
            "packages/search"
        );
        assert_eq!(
            production_module_name("crates/core/src/lib.rs"),
            "crates/core"
        );
        assert_eq!(production_module_name("src/lib.rs"), "src");
        assert_eq!(production_module_name("build.rs"), ".");
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

    #[cfg(feature = "lang-jai")]
    #[test]
    fn jai_import_lines_are_import_and_load_directives() {
        assert!(is_import_line(Lang::Jai, "#import \"Basic\";"));
        assert!(is_import_line(Lang::Jai, "#load \"util.jai\";"));
        assert!(is_import_line(Lang::Jai, "Math :: #import \"Math\";"));
        assert!(!is_import_line(Lang::Jai, "main :: () {"));
    }

    #[cfg(feature = "lang-luau")]
    #[test]
    fn luau_import_lines_are_require_calls() {
        assert!(is_import_line(Lang::Luau, "local Util = require(\"./util\")"));
        assert!(is_import_line(
            Lang::Luau,
            "local Signal = require(script.Parent.Signal)"
        ));
        assert!(is_import_line(
            Lang::Luau,
            "local Net = require(ReplicatedStorage.Packages.Net)"
        ));
        assert!(!is_import_line(Lang::Luau, "local speed = 16"));
        // A Roblox instance path names the module by its last segment.
        let tokens = BTreeSet::from(["signal".to_string()]);
        let matched = import_tokens_for_file(
            Lang::Luau,
            "local Signal = require(script.Parent.Signal)\nSignal.fire()\n",
            &tokens,
        );
        assert_eq!(matched.into_iter().collect::<Vec<_>>(), vec!["signal"]);
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
