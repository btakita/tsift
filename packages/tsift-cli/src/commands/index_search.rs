use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tsift_index::{config, index, multiplicity};
use tsift_quality::lint;

use crate::output::{OutputFormat, ResponseBudget, ToolEnvelopeSummary};
use crate::{
    DegradedSearchMode, SearchBudgetReportInput, SearchFacetFilters, TagpathSearchOpts,
    abbreviate_kind, abbreviate_match_type, annotate_hits_with_tagpath, apply_search_facet_filters,
    build_search_budget_follow_up, build_search_budget_report, compact_snippet,
    degraded_search_mode, emit_degraded_search_note, envelope_metric, federated_exact_search,
    federated_sift_search, federated_symbol_search, format_score, group_search_hits,
    inject_tagpath_stale_into_json, maybe_apply_search_post_precheck_test_hooks,
    maybe_apply_search_worker_test_hooks, precheck_search_indexes, print_json_or_envelope,
    print_search_budget_human, relativize, relativize_index_summary, relativize_json_paths,
    relativize_symbol_hits, resolve_search_strategy, run_exact_search_with_timeout,
    run_index_update, run_search_with_timeout, run_sift_search, should_collapse_search_hits,
    to_json_schema,
};

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_index(
    path: &std::path::Path,
    rebuild: bool,
    check: bool,
    exit_code: bool,
    prune: bool,
    quiet: bool,
    workspace: bool,
    submodule: Option<&str>,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    absolute: bool,
    schema: bool,
) -> Result<()> {
    let quiet = quiet || exit_code;
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let workspace_discovery = if workspace && submodule.is_none() {
        Some(config::Config::workspace_discovery(&root)?)
    } else {
        None
    };
    let fall_back_to_root = workspace_discovery
        .as_ref()
        .is_some_and(|discovery| discovery.scopes.is_empty());

    if fall_back_to_root {
        let discovery = workspace_discovery
            .as_ref()
            .expect("workspace discovery exists when root fallback is selected");
        if discovery.unresolvable.is_empty() {
            eprintln!("workspace: no resolvable scopes; indexing root tree instead");
        } else {
            let details = discovery
                .unresolvable
                .iter()
                .map(|scope| format!("{} — no gitlink and path absent", scope.relative_path))
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                "workspace: {} declared scope{} unresolvable ({}); indexing root tree instead",
                discovery.unresolvable.len(),
                if discovery.unresolvable.len() == 1 {
                    ""
                } else {
                    "s"
                },
                details
            );
        }
    }

    if (workspace || submodule.is_some()) && !fall_back_to_root {
        let cfg = config::Config::load(&root)?;
        let targets: Vec<(String, PathBuf, PathBuf, Option<config::WorkspaceScope>)> =
            if let Some(name) = submodule {
                if let Some(scope) = config::Config::find_submodule(&root, name)? {
                    let db_path = cfg.db_path_for(&root, &scope.id);
                    vec![(
                        scope.id.clone(),
                        scope.source_root.clone(),
                        db_path,
                        Some(scope),
                    )]
                } else if let Some(package) = multiplicity::find_cargo_package(&root, name)? {
                    let db_path = multiplicity::cargo_package_db_path(&root, &package.scope_id);
                    vec![(
                        package.scope_id.clone(),
                        package.package_root.clone(),
                        db_path,
                        None,
                    )]
                } else {
                    config::Config::resolve_submodule(&root, name)?;
                    Vec::new()
                }
        } else {
            match workspace_discovery.as_ref() {
                Some(discovery) => discovery.scopes.clone(),
                None => config::Config::submodule_dirs(&root)?,
            }
                .into_iter()
                .map(|scope| {
                        let db_path = cfg.db_path_for(&root, &scope.id);
                        (
                            scope.id.clone(),
                            scope.source_root.clone(),
                            db_path,
                            Some(scope),
                        )
                    })
                    .collect()
            };

        if targets.is_empty() {
            bail!("no submodules found in {}", root.display());
        }

        let mut any_stale = false;
        for (name, sub_path, db_path, scope) in &targets {
            if !sub_path.exists() {
                eprintln!("  skip {} (not found: {})", name, sub_path.display());
                continue;
            }
            let mut summary = if rebuild {
                run_index_update(
                    db_path,
                    sub_path,
                    format!("rebuilding submodule `{}` index", name),
                    &root,
                    Some(name.as_str()),
                    true,
                    false,
                )?
            } else if check {
                index::IndexDb::inspect_read_only(db_path, sub_path, prune)?.summary
            } else if prune {
                run_index_update(
                    db_path,
                    sub_path,
                    format!("pruning submodule `{}` index", name),
                    &root,
                    Some(name.as_str()),
                    false,
                    true,
                )?
            } else {
                run_index_update(
                    db_path,
                    sub_path,
                    format!("indexing submodule `{}`", name),
                    &root,
                    Some(name.as_str()),
                    false,
                    false,
                )?
            };
            if !absolute {
                relativize_index_summary(&mut summary, sub_path);
            }
            if summary.has_changes() {
                any_stale = true;
            }
            let tier = scope
                .as_ref()
                .map(|scope| cfg.tier_for_scope(scope))
                .unwrap_or_else(|| cfg.tier_for(name));
            if json_output {
                let entry = if quiet {
                    serde_json::json!({
                        "submodule": name,
                        "tier": format!("{:?}", tier).to_lowercase(),
                        "total_tracked": summary.total_tracked,
                        "new": summary.new,
                        "modified": summary.modified,
                        "deleted": summary.deleted,
                        "unchanged": summary.unchanged,
                    })
                } else {
                    serde_json::json!({
                        "submodule": name,
                        "tier": format!("{:?}", tier).to_lowercase(),
                        "summary": summary,
                    })
                };
                println!(
                    "{}",
                    if quiet {
                        serde_json::to_string(&entry)?
                    } else {
                        to_json_schema(&entry, pretty, terse, false, schema)?
                    }
                );
            } else if compact {
                let mode = if rebuild {
                    "rebuild"
                } else if check {
                    "check"
                } else if prune {
                    "prune-safe"
                } else {
                    "incremental"
                };
                print!(
                    "[{}] {} {:?} tracked:{} new:{} mod:{} del:{} unch:{}",
                    name,
                    mode,
                    tier,
                    summary.total_tracked,
                    summary.new,
                    summary.modified,
                    summary.deleted,
                    summary.unchanged
                );
                if let Some(ref ps) = summary.prune_stats {
                    print!(
                        " pruned:{} walked:{} skipped:{}",
                        ps.dirs_pruned, ps.dirs_walked, ps.files_pruned
                    );
                }
                println!();
            } else {
                let mode = if rebuild {
                    "rebuild"
                } else if check {
                    "check"
                } else if prune {
                    "prune-safe"
                } else {
                    "incremental"
                };
                print!(
                    "[{}] ({}, {:?}) {} files tracked — new:{} mod:{} del:{} unch:{}",
                    name,
                    mode,
                    tier,
                    summary.total_tracked,
                    summary.new,
                    summary.modified,
                    summary.deleted,
                    summary.unchanged
                );
                if let Some(ref ps) = summary.prune_stats {
                    print!(
                        " | pruned:{} dirs ({}d walked, {} files skipped)",
                        ps.dirs_pruned, ps.dirs_walked, ps.files_pruned
                    );
                }
                println!();
            }
        }
        if exit_code && check && any_stale {
            std::process::exit(1);
        }
        return Ok(());
    }

    let db_path = root.join(".tsift/index.db");
    let summary = if rebuild {
        run_index_update(
            &db_path,
            &root,
            "rebuilding index".to_string(),
            &root,
            None,
            true,
            false,
        )?
    } else if check {
        index::IndexDb::inspect_read_only(&db_path, &root, prune)?.summary
    } else if prune {
        run_index_update(
            &db_path,
            &root,
            "scanning index (--prune safety mode)".to_string(),
            &root,
            None,
            false,
            true,
        )?
    } else {
        run_index_update(
            &db_path,
            &root,
            "indexing index".to_string(),
            &root,
            None,
            false,
            false,
        )?
    };

    let mut summary = summary;
    if !absolute {
        relativize_index_summary(&mut summary, &root);
    }

    if json_output {
        if quiet {
            let compact = serde_json::json!({
                "total_tracked": summary.total_tracked,
                "new": summary.new,
                "modified": summary.modified,
                "deleted": summary.deleted,
                "unchanged": summary.unchanged,
                "prune_stats": summary.prune_stats,
            });
            println!("{}", serde_json::to_string(&compact)?);
        } else {
            println!(
                "{}",
                to_json_schema(&summary, pretty, terse, false, schema)?
            );
        }
    } else if compact {
        let mode = if rebuild {
            "rebuild"
        } else if check {
            "check"
        } else if prune {
            "prune-safe"
        } else {
            "incremental"
        };
        print!(
            "index {} tracked:{} new:{} mod:{} del:{} unch:{}",
            mode,
            summary.total_tracked,
            summary.new,
            summary.modified,
            summary.deleted,
            summary.unchanged
        );
        if let Some(ref ps) = summary.prune_stats {
            print!(
                " pruned:{} walked:{} skipped:{}",
                ps.dirs_pruned, ps.dirs_walked, ps.files_pruned
            );
        }
        if summary.skipped.files > 0 {
            print!(" unsupported:{}", summary.skipped.files);
        }
        println!();
    } else {
        let mode = if rebuild {
            "rebuild"
        } else if check {
            "check"
        } else if prune {
            "prune-safe"
        } else {
            "incremental"
        };
        println!("Index ({}): {} files tracked", mode, summary.total_tracked);
        print!(
            "  new: {}  modified: {}  deleted: {}  unchanged: {}",
            summary.new, summary.modified, summary.deleted, summary.unchanged
        );
        if let Some(ref ps) = summary.prune_stats {
            print!(
                " | pruned: {} dirs ({} walked, {} files skipped)",
                ps.dirs_pruned, ps.dirs_walked, ps.files_pruned
            );
        }
        println!();
        // #goindex: a skipped file used to leave no trace anywhere, so a Go
        // module indexing 8 of its 26 files still read as a complete index.
        if summary.skipped.files > 0 {
            let breakdown = summary
                .skipped
                .ranked_extensions()
                .into_iter()
                .take(6)
                .map(|(ext, count)| format!("{ext} {count}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "  skipped: {} (unsupported extension) — {}",
                summary.skipped.files, breakdown
            );
        }
        if !quiet && !summary.changes.is_empty() {
            println!();
            for change in &summary.changes {
                let marker = match change.kind {
                    index::ChangeKind::New => "+",
                    index::ChangeKind::Modified => "~",
                    index::ChangeKind::Deleted => "-",
                };
                let lang = change.language.as_deref().unwrap_or("");
                println!("  {} {} [{}]", marker, change.path.display(), lang);
            }
        }
    }
    if exit_code && check && summary.has_changes() {
        std::process::exit(1);
    }
    Ok(())
}

#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn cmd_search(
    query: String,
    path: Option<PathBuf>,
    limit: usize,
    strategy: Option<String>,
    scope: Option<String>,
    federated: bool,
    json_output: bool,
    autoindex: bool,
    timeout_secs: u64,
    compact: bool,
    pretty: bool,
    terse: bool,
    ultra_terse: bool,
    absolute: bool,
    tabular: bool,
    schema: bool,
) -> Result<()> {
    let paths = path.into_iter().collect::<Vec<_>>();
    cmd_search_with_budget(
        query,
        paths,
        limit,
        strategy,
        scope,
        federated,
        json_output,
        autoindex,
        timeout_secs,
        compact,
        pretty,
        terse,
        ultra_terse,
        absolute,
        tabular,
        schema,
        false,
        ResponseBudget::default(),
        TagpathSearchOpts::default(),
        SearchFacetFilters::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_search_with_budget(
    query: String,
    paths: Vec<PathBuf>,
    limit: usize,
    strategy: Option<String>,
    scope: Option<String>,
    federated: bool,
    json_output: bool,
    autoindex: bool,
    timeout_secs: u64,
    compact: bool,
    pretty: bool,
    terse: bool,
    ultra_terse: bool,
    absolute: bool,
    tabular: bool,
    schema: bool,
    envelope: bool,
    budget: ResponseBudget,
    tagpath_opts: TagpathSearchOpts,
    facet_filters: SearchFacetFilters,
) -> Result<()> {
    let base_path = paths.first().cloned().unwrap_or_else(|| PathBuf::from("."));
    let format = OutputFormat {
        json_output,
        compact,
        pretty,
        terse,
        ultra_terse,
        schema,
        envelope,
    };
    let root = lint::resolve_project_root_or_canonical_path(&base_path)?;
    // #wsfed: federate by default at a workspace root with no shared root index,
    // for every strategy. The `exact` path already federated there, so plain
    // `tsift search` succeeded or exited 1 depending on whether the query
    // happened to route to `exact` — a rule no caller can infer.
    let federated =
        federated || crate::should_auto_federate(&root, &base_path, scope.as_deref(), federated)?;
    let search_cache_dir = root.join(".tsift/search-cache");
    // #ve5f path-prune: when `--path` (base_path) narrows to a strict subdirectory of
    // the project root, the FTS/lexical path still searches the *whole* project index
    // (`content_fts` paths are absolute), so its hits were never scoped to the subdir —
    // unlike exact search, where `rg` runs in `base_path`. Capture that sub-path so the
    // non-exact result set can be pruned to it for parity. `--path` is repeatable, so
    // collect every provided path that resolves to a strict subdir of the root; a hit is
    // kept if it falls under any of them. Empty (no `--path`, or only the root itself)
    // preserves the whole-index default.
    let path_scopes: Vec<PathBuf> = paths
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .filter(|canon| canon != &root && canon.starts_with(&root))
        .collect();
    let requested_strategy = resolve_search_strategy(&query, strategy);
    let requested_exact_search = requested_strategy == "exact";
    let precheck = if requested_exact_search {
        None
    } else {
        Some(precheck_search_indexes(
            &root,
            &base_path,
            scope.as_deref(),
            federated,
            autoindex,
        )?)
    };
    let degraded_mode = precheck
        .as_ref()
        .and_then(|precheck| degraded_search_mode(&precheck.degraded_targets));
    let exact_search = requested_exact_search || degraded_mode == Some(DegradedSearchMode::Exact);
    let effective_strategy = if exact_search {
        "exact".to_string()
    } else {
        requested_strategy
    };
    // #015t Phase 4b: forward the freshness verdict the precheck already computed
    // so the FTS path skips its redundant `inspect_read_only` walk. Fresh = the
    // precheck ran and the index was NOT degraded to read-only (a read-only
    // degrade means a stale index held by a concurrent writer ⇒ `Some(false)` so
    // the FTS path falls back to live `TokenIndex` results).
    let fts_index_fresh = precheck
        .as_ref()
        .map(|_| degraded_mode != Some(DegradedSearchMode::ReadOnly));
    let search_targets = if requested_exact_search {
        Vec::new()
    } else if let Some(precheck) = precheck.as_ref() {
        if let Some(mode) = degraded_mode {
            emit_degraded_search_note(&precheck.degraded_targets, mode);
        }
        if exact_search {
            Vec::new()
        } else {
            maybe_apply_search_post_precheck_test_hooks()?;
            precheck.targets.clone()
        }
    } else {
        Vec::new()
    };

    let inferred_scope = if scope.is_none() && !federated {
        config::Config::infer_submodule_from_path(&root, &base_path)?
    } else {
        None
    };

    let symbol_search_limit = if facet_filters.is_empty() || limit == 0 {
        limit
    } else {
        limit.saturating_mul(20).max(limit).max(100)
    };
    let include_markdown_symbols = !facet_filters.is_empty();

    let (symbol_hits, sift_path, federated_tagpath_diag) =
        if let Some(scope) = inferred_scope.as_ref() {
            let cfg = config::Config::load(&root)?;
            let db_path = cfg.db_path_for(&root, &scope.id);
            let hits = if db_path.exists() {
                let db = index::IndexDb::open_read_only_resilient(&db_path)?;
                if include_markdown_symbols {
                    db.symbol_search(&query, symbol_search_limit)?
                } else {
                    db.code_symbol_search(&query, symbol_search_limit)?
                }
            } else {
                Vec::new()
            };
            (hits, scope.source_root.clone(), None)
        } else if let Some(ref scope_name) = scope {
            let cfg = config::Config::load(&root)?;
            let scope = config::Config::resolve_submodule(&root, scope_name)?;
            let db_path = cfg.db_path_for(&root, &scope.id);
            let hits = if db_path.exists() {
                let db = index::IndexDb::open_read_only_resilient(&db_path)?;
                if include_markdown_symbols {
                    db.symbol_search(&query, symbol_search_limit)?
                } else {
                    db.code_symbol_search(&query, symbol_search_limit)?
                }
            } else {
                Vec::new()
            };
            (hits, scope.source_root, None)
        } else if federated {
            let (hits, diag) = federated_symbol_search(
                &root,
                &query,
                symbol_search_limit,
                include_markdown_symbols,
                &tagpath_opts,
            )?;
            (hits, root.clone(), Some(diag))
        } else {
            let db_path = root.join(".tsift/index.db");
            let hits = if db_path.exists() {
                let db = index::IndexDb::open_read_only_resilient(&db_path)?;
                if include_markdown_symbols {
                    db.symbol_search(&query, symbol_search_limit)?
                } else {
                    db.code_symbol_search(&query, symbol_search_limit)?
                }
            } else {
                Vec::new()
            };
            (hits, root.clone(), None)
        };

    let mut symbol_hits = symbol_hits;
    // Use `sift_path` (which equals `scope.source_root` for scoped /
    // inferred-scope paths and the workspace root otherwise) so the
    // tagpath adapter walks for `.naming.toml` from the right project
    // root. The previous behavior walked from the workspace root,
    // which silently dropped handles when the submodule owned the
    // tagpath project but the workspace did not — the same shape as
    // the federated bug closed in #p6tsifullfederated (0.1.57).
    let tagpath_diag = if let Some(diag) = federated_tagpath_diag {
        diag
    } else {
        annotate_hits_with_tagpath(&mut symbol_hits, &sift_path, &tagpath_opts)?
    };
    if !absolute {
        relativize_symbol_hits(&mut symbol_hits, &root);
    }
    if tagpath_diag.stale && !tagpath_opts.no_tagpath {
        eprintln!(
            "tagpath_index_stale: true (reason={}); falling back to live extraction",
            tagpath_diag.reason.as_deref().unwrap_or("unknown"),
        );
    }
    symbol_hits = apply_search_facet_filters(&root, symbol_hits, &facet_filters);
    symbol_hits.truncate(limit);

    let mut response = if exact_search {
        if federated && scope.is_none() {
            federated_exact_search(&root, &query, limit, timeout_secs)?
        } else {
            let exact_paths: Vec<PathBuf> = if requested_exact_search && scope.is_none() {
                if paths.is_empty() {
                    vec![PathBuf::from(".")]
                } else {
                    paths.clone()
                }
            } else {
                vec![sift_path.clone()]
            };
            run_exact_search_with_timeout(&exact_paths, &query, limit, timeout_secs)?
        }
    } else if federated && scope.is_none() {
        federated_sift_search(
            &root,
            &search_cache_dir,
            &query,
            limit,
            timeout_secs,
            &effective_strategy,
            fts_index_fresh,
        )?
    } else {
        run_search_with_timeout(
            &sift_path,
            &search_cache_dir,
            &query,
            limit,
            timeout_secs,
            &effective_strategy,
            &search_targets,
            fts_index_fresh,
        )?
    };

    // #ve5f: prune the result set to the requested `--path` sub-scopes. Exact search is
    // already scoped (rg runs across every provided path) so this is a no-op there;
    // federated search spans multiple repos so sub-paths must not drop cross-repo hits.
    // The FTS/lexical path searches the whole index, so this is where sub-path
    // narrowing lands — a hit is kept if it resolves under any provided scope.
    if !path_scopes.is_empty() && !federated {
        prune_hits_to_path_scope(&mut response, &path_scopes, &root);
    }

    // #trt1p2b hot-path injection: fold trusted, fresh findings for the search
    // result-set nodes (matched symbol names and their files) into the envelope.
    let result_set_findings = {
        let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for hit in &symbol_hits {
            keys.insert(hit.name.clone());
            keys.insert(hit.file.clone());
        }
        crate::commands::finding::collect_result_set_finding_previews(
            &root,
            &keys,
            scope.as_deref(),
            10,
            240,
        )
    };

    if budget.is_active() {
        let report = build_search_budget_report(SearchBudgetReportInput {
            query: &query,
            strategy: &effective_strategy,
            root: &root,
            response: &response,
            symbol_hits: &symbol_hits,
            absolute,
            budget,
            filters: &facet_filters,
        });
        if format.json_output {
            let mut follow_up = report
                .scale_guard
                .as_ref()
                .map(|guard| guard.narrow_commands.clone())
                .unwrap_or_default();
            follow_up.push(build_search_budget_follow_up(
                &query,
                &effective_strategy,
                base_path.to_string_lossy().as_ref(),
            ));
            if let Some(symbol) = report.symbols.first() {
                follow_up.push(symbol.expand.clone());
            }
            if let Some(hit) = report.hits.first() {
                follow_up.push(hit.expand.clone());
            }
            let report_truncated = report.truncated;
            let mut report_value = serde_json::to_value(&report)?;
            if !result_set_findings.is_empty()
                && let Some(obj) = report_value.as_object_mut()
            {
                obj.insert(
                    "findings".to_string(),
                    serde_json::to_value(&result_set_findings)?,
                );
            }
            inject_tagpath_stale_into_json(
                &mut report_value,
                tagpath_diag.stale && !tagpath_opts.no_tagpath,
                tagpath_diag.reason.as_deref(),
            );
            print_json_or_envelope(
                &report_value,
                &format,
                "search",
                "preview",
                ToolEnvelopeSummary {
                    text: format!("search preview for {}", query),
                    metrics: vec![
                        envelope_metric("strategy", &report.strategy),
                        envelope_metric("symbols", report.symbol_total),
                        envelope_metric("hits", report.hit_total),
                        envelope_metric("indexed", report.indexed_artifacts),
                        envelope_metric("skipped", report.skipped_artifacts),
                    ],
                },
                report_truncated,
                follow_up,
            )?;
        } else {
            print_search_budget_human(&report);
        }
    } else if format.json_output {
        #[derive(Serialize)]
        struct CombinedResponse<'a> {
            symbols: &'a [index::SymbolHit],
            #[serde(flatten)]
            sift: &'a serde_json::Value,
        }
        let mut sift_value = serde_json::to_value(&response)?;
        if !absolute {
            relativize_json_paths(&mut sift_value, &root);
        }
        let combined = CombinedResponse {
            symbols: &symbol_hits,
            sift: &sift_value,
        };
        let mut combined_value = serde_json::to_value(&combined)?;
        if !result_set_findings.is_empty()
            && let Some(obj) = combined_value.as_object_mut()
        {
            obj.insert(
                "findings".to_string(),
                serde_json::to_value(&result_set_findings)?,
            );
        }
        inject_tagpath_stale_into_json(
            &mut combined_value,
            tagpath_diag.stale && !tagpath_opts.no_tagpath,
            tagpath_diag.reason.as_deref(),
        );
        print_json_or_envelope(
            &combined_value,
            &format,
            "search",
            "report",
            ToolEnvelopeSummary {
                text: format!("search results for {}", query),
                metrics: vec![
                    envelope_metric("strategy", &effective_strategy),
                    envelope_metric("symbols", symbol_hits.len()),
                    envelope_metric("hits", response.hits.len()),
                    envelope_metric("indexed", response.indexed_artifacts),
                    envelope_metric("skipped", response.skipped_artifacts),
                ],
            },
            false,
            vec![build_search_budget_follow_up(
                &query,
                &effective_strategy,
                base_path.to_string_lossy().as_ref(),
            )],
        )?;
    } else if tabular {
        if !symbol_hits.is_empty() {
            println!("match_type\tkind\tname\tfile\tline\tscore");
            for hit in &symbol_hits {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    hit.match_type,
                    hit.kind,
                    hit.name,
                    hit.file,
                    hit.line,
                    format_score(hit.score, true)
                );
            }
        }
        if !response.hits.is_empty() {
            if !symbol_hits.is_empty() {
                println!();
            }
            println!("rank\tpath\tconfidence\tscore");
            for hit in &response.hits {
                let hp = if absolute {
                    hit.path.clone()
                } else {
                    relativize(&hit.path, &root)
                };
                println!(
                    "{}\t{}\t{:?}\t{}",
                    hit.rank,
                    hp,
                    hit.confidence,
                    format_score(hit.score, true)
                );
            }
        }
        if symbol_hits.is_empty() && response.hits.is_empty() {
            println!("(none)");
        }
    } else if compact {
        if !symbol_hits.is_empty() {
            println!("syms[{}]:", symbol_hits.len());
            for (i, hit) in symbol_hits.iter().enumerate() {
                println!(
                    "  {}. [{}] {} {} {}:{} {}",
                    i + 1,
                    abbreviate_match_type(&hit.match_type),
                    abbreviate_kind(&hit.kind),
                    hit.name,
                    hit.file,
                    hit.line,
                    format_score(hit.score, true)
                );
            }
        }

        println!("hits[{}]:", response.hits.len());
        if should_collapse_search_hits(&response.hits, &root, absolute) {
            for group in group_search_hits(&response.hits, &root, absolute) {
                let sample_suffix = if group.samples.is_empty() {
                    String::new()
                } else {
                    format!(" {}", group.samples.join(" | "))
                };
                println!(
                    "  {}. {} [{} {} hits:{}]{}",
                    group.first_rank,
                    group.path,
                    group.confidence,
                    format_score(group.top_score, true),
                    group.hits,
                    sample_suffix
                );
            }
        } else {
            for hit in &response.hits {
                let hp = if absolute {
                    hit.path.clone()
                } else {
                    relativize(&hit.path, &root)
                };
                let snippet = compact_snippet(&hit.snippet).unwrap_or_default();
                if snippet.is_empty() {
                    println!(
                        "  {}. {} [{:?} {}]",
                        hit.rank,
                        hp,
                        hit.confidence,
                        format_score(hit.score, true)
                    );
                } else {
                    println!(
                        "  {}. {} [{:?} {}] {}",
                        hit.rank,
                        hp,
                        hit.confidence,
                        format_score(hit.score, true),
                        snippet
                    );
                }
            }
        }
        if symbol_hits.is_empty() && response.hits.is_empty() {
            println!("  (none)");
        }
    } else {
        if !symbol_hits.is_empty() {
            println!("Symbol matches ({}):", symbol_hits.len());
            println!();
            for (i, hit) in symbol_hits.iter().enumerate() {
                println!(
                    "  #{} [{}] {} {} ({}:{}) score: {:.4}",
                    i + 1,
                    hit.match_type,
                    hit.kind,
                    hit.name,
                    hit.file,
                    hit.line,
                    hit.score
                );
            }
            println!();
        }

        println!(
            "Strategy: {} | Indexed: {} | Skipped: {}",
            response.strategy, response.indexed_artifacts, response.skipped_artifacts
        );
        println!();
        if should_collapse_search_hits(&response.hits, &root, absolute) {
            let groups = group_search_hits(&response.hits, &root, absolute);
            println!(
                "File matches ({} files / {} hits):",
                groups.len(),
                response.hits.len()
            );
            println!();
            for group in groups {
                println!(
                    "  #{} [{}] {} (hits: {}, top score: {:.4})",
                    group.first_rank, group.confidence, group.path, group.hits, group.top_score
                );
                for sample in &group.samples {
                    println!("    {}", sample);
                }
                let hidden_hits = group.hits.saturating_sub(group.samples.len());
                if hidden_hits > 0 {
                    println!("    (+{} more hits in file)", hidden_hits);
                }
                println!();
            }
        } else {
            for hit in &response.hits {
                let hp = if absolute {
                    hit.path.clone()
                } else {
                    relativize(&hit.path, &root)
                };
                println!(
                    "  #{} [{:?}] {} (score: {:.4})",
                    hit.rank, hit.confidence, hp, hit.score
                );
                if !hit.snippet.is_empty() {
                    for line in hit.snippet.lines().take(3) {
                        println!("    {}", line);
                    }
                }
                println!();
            }
        }
        if symbol_hits.is_empty() && response.hits.is_empty() {
            println!("  No results.");
        }
    }
    // #trt1p2b: authored findings for the result set, in non-JSON output.
    if !format.json_output && !result_set_findings.is_empty() {
        println!();
        println!("Findings (authored why, anchored to the result set):");
        for finding in &result_set_findings {
            println!(
                "  [{}] {} (about {})",
                finding.kind, finding.title, finding.about
            );
        }
    }
    Ok(())
}

/// #ve5f path-prune: narrow a non-exact search result set to the requested `--path`
/// sub-scope. The FTS/lexical path searches the whole project index (`content_fts`
/// stores absolute paths), so a `--path <subdir>` filter never reached its hits —
/// unlike exact search, where `rg` already runs inside `base_path`. Retain only the
/// hits whose path resolves under `scope_dir` (a canonical strict subdirectory of
/// `root`). Relative hit paths are joined to `root` before the prefix test; absolute
/// paths are tested directly. Each surviving hit keeps its original BM25 `rank` (and
/// the matching `artifact_id`), so the result set stays a strict subsequence of the
/// global ranking — narrowing changes which files appear, never their relative order.
fn prune_hits_to_path_scope(
    response: &mut tsift_search::sift::SearchResponse,
    scope_dirs: &[PathBuf],
    root: &Path,
) {
    response.hits.retain(|hit| {
        let raw = Path::new(&hit.path);
        let abs = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            root.join(raw)
        };
        scope_dirs
            .iter()
            .any(|scope_dir| abs.starts_with(scope_dir))
    });
}

pub(crate) fn cmd_search_worker(
    path: &Path,
    cache_dir: &Path,
    query: &str,
    limit: usize,
    strategy: &str,
    output: &Path,
    fts_index_fresh: Option<bool>,
) -> Result<()> {
    maybe_apply_search_worker_test_hooks()?;
    let response = run_sift_search(path, cache_dir, query, limit, strategy, fts_index_fresh)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .with_context(|| format!("creating search worker output: {}", output.display()))?;
    serde_json::to_writer(&mut file, &response)
        .with_context(|| format!("writing search worker output: {}", output.display()))?;
    file.flush()
        .with_context(|| format!("flushing search worker output: {}", output.display()))?;
    Ok(())
}

#[cfg(test)]
mod prune_path_scope_tests {
    use super::*;
    use tsift_search::sift::{
        AcquisitionAdapterKind, ArtifactBudget, ArtifactFreshness, ArtifactProvenance,
        ContextArtifactKind, ScoreConfidence, SearchCoverageMode, SearchCoverageSnapshot,
        SearchHit, SearchResponse,
    };

    fn hit(path: &str, rank: usize) -> SearchHit {
        SearchHit {
            artifact_id: format!("fts:{path}:1:{rank}"),
            artifact_kind: ContextArtifactKind::File,
            budget: ArtifactBudget::from_text("x", 1),
            confidence: ScoreConfidence::High,
            freshness: ArtifactFreshness {
                modified_unix_secs: None,
                observed_unix_secs: 0,
            },
            location: Some("line 1".to_string()),
            path: path.to_string(),
            provenance: ArtifactProvenance {
                adapter: AcquisitionAdapterKind::FileSystem,
                source: "test".to_string(),
                synthetic: false,
            },
            rank,
            score: 1.0,
            snippet: "x".to_string(),
        }
    }

    fn response(hits: Vec<SearchHit>) -> SearchResponse {
        SearchResponse {
            coverage: SearchCoverageSnapshot {
                active_rebuild: None,
                completed_dirty_sector_count: 0,
                dirty_sector_count: 0,
                mode: SearchCoverageMode::Sealed,
                mounted_sector_count: 0,
                rebuilding_sector_count: 0,
                resumed_sector_count: 0,
                reused_sector_count: 0,
                total_sector_count: 0,
            },
            hits,
            indexed_artifacts: 0,
            root: "/proj".to_string(),
            skipped_artifacts: 0,
            strategy: "fts".to_string(),
        }
    }

    #[test]
    fn prunes_absolute_hits_to_subdir_and_preserves_global_rank() {
        let root = Path::new("/proj");
        let scope = Path::new("/proj/src/foo");
        let mut resp = response(vec![
            hit("/proj/src/foo/a.rs", 1),
            hit("/proj/src/bar/b.rs", 2),
            hit("/proj/src/foo/nested/c.rs", 3),
            // sibling whose string prefix matches but whose path component does not —
            // component-based `starts_with` must NOT retain it.
            hit("/proj/src/foobar/d.rs", 4),
        ]);
        prune_hits_to_path_scope(&mut resp, &[scope.to_path_buf()], root);
        let paths: Vec<&str> = resp.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["/proj/src/foo/a.rs", "/proj/src/foo/nested/c.rs"]
        );
        // No renumber: survivors keep their original BM25 ranks (strict subsequence).
        assert_eq!(
            resp.hits.iter().map(|h| h.rank).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    #[test]
    fn joins_relative_hits_against_root_before_prefix_test() {
        let root = Path::new("/proj");
        let scope = Path::new("/proj/src/foo");
        let mut resp = response(vec![hit("src/foo/a.rs", 1), hit("src/bar/b.rs", 2)]);
        prune_hits_to_path_scope(&mut resp, &[scope.to_path_buf()], root);
        let paths: Vec<&str> = resp.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["src/foo/a.rs"]);
    }

    #[test]
    fn empty_result_when_no_hit_under_scope() {
        let root = Path::new("/proj");
        let scope = Path::new("/proj/src/foo");
        let mut resp = response(vec![hit("/proj/src/bar/b.rs", 1)]);
        prune_hits_to_path_scope(&mut resp, &[scope.to_path_buf()], root);
        assert!(resp.hits.is_empty());
    }

    #[test]
    fn keeps_hits_under_any_of_multiple_scopes() {
        let root = Path::new("/proj");
        let scopes = vec![
            PathBuf::from("/proj/src/foo"),
            PathBuf::from("/proj/lib/bar"),
        ];
        let mut resp = response(vec![
            hit("/proj/src/foo/a.rs", 1),
            hit("/proj/src/baz/b.rs", 2),
            hit("/proj/lib/bar/c.rs", 3),
            hit("/proj/lib/qux/d.rs", 4),
        ]);
        prune_hits_to_path_scope(&mut resp, &scopes, root);
        let paths: Vec<&str> = resp.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["/proj/src/foo/a.rs", "/proj/lib/bar/c.rs"]);
        assert_eq!(
            resp.hits.iter().map(|h| h.rank).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }
}
