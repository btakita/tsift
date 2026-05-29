use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::output::{OutputFormat, ResponseBudget, ToolEnvelopeSummary};
use crate::{
    DegradedSearchMode, TagpathSearchOpts, abbreviate_kind, abbreviate_match_type,
    annotate_hits_with_tagpath, build_search_budget_follow_up, build_search_budget_report,
    compact_snippet, degraded_search_mode, emit_degraded_search_note, envelope_metric,
    federated_exact_search, federated_sift_search, federated_symbol_search, format_score,
    group_search_hits, inject_tagpath_stale_into_json,
    maybe_apply_search_post_precheck_test_hooks, maybe_apply_search_worker_test_hooks,
    precheck_search_indexes, print_json_or_envelope, print_search_budget_human, relativize,
    relativize_index_summary, relativize_json_paths, relativize_symbol_hits,
    resolve_search_strategy, run_exact_search_with_timeout, run_index_update, run_search_with_timeout,
    run_sift_search, should_collapse_search_hits, to_json_schema,
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
    let root = tsift::lint::resolve_project_root_or_canonical_path(path)?;

    if workspace || submodule.is_some() {
        let cfg = tsift::config::Config::load(&root)?;
        let targets: Vec<(String, PathBuf, Option<tsift::config::WorkspaceScope>)> =
            if let Some(name) = submodule {
                let scope = tsift::config::Config::resolve_submodule(&root, name)?;
                vec![(scope.id.clone(), scope.source_root.clone(), Some(scope))]
            } else {
                tsift::config::Config::submodule_dirs(&root)?
                    .into_iter()
                    .map(|scope| (scope.id.clone(), scope.source_root.clone(), Some(scope)))
                    .collect()
            };

        if targets.is_empty() {
            bail!("no submodules found in {}", root.display());
        }

        let mut any_stale = false;
        for (name, sub_path, scope) in &targets {
            if !sub_path.exists() {
                eprintln!("  skip {} (not found: {})", name, sub_path.display());
                continue;
            }
            let db_path = cfg.db_path_for(&root, name);
            let mut summary = if rebuild {
                run_index_update(
                    &db_path,
                    sub_path,
                    format!("rebuilding submodule `{}` index", name),
                    &root,
                    Some(name.as_str()),
                    true,
                    false,
                )?
            } else if check {
                tsift::index::IndexDb::inspect_read_only(&db_path, sub_path, prune)?.summary
            } else if prune {
                run_index_update(
                    &db_path,
                    sub_path,
                    format!("pruning submodule `{}` index", name),
                    &root,
                    Some(name.as_str()),
                    false,
                    true,
                )?
            } else {
                run_index_update(
                    &db_path,
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
                        to_json_schema(&entry, pretty, terse, schema)?
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
        tsift::index::IndexDb::inspect_read_only(&db_path, &root, prune)?.summary
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
            println!("{}", to_json_schema(&summary, pretty, terse, schema)?);
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
        if !quiet && !summary.changes.is_empty() {
            println!();
            for change in &summary.changes {
                let marker = match change.kind {
                    tsift::index::ChangeKind::New => "+",
                    tsift::index::ChangeKind::Modified => "~",
                    tsift::index::ChangeKind::Deleted => "-",
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
    absolute: bool,
    tabular: bool,
    schema: bool,
) -> Result<()> {
    cmd_search_with_budget(
        query,
        path,
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
        absolute,
        tabular,
        schema,
        false,
        ResponseBudget::default(),
        TagpathSearchOpts::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_search_with_budget(
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
    absolute: bool,
    tabular: bool,
    schema: bool,
    envelope: bool,
    budget: ResponseBudget,
    tagpath_opts: TagpathSearchOpts,
) -> Result<()> {
    let base_path = path.unwrap_or_else(|| PathBuf::from("."));
    let format = OutputFormat {
        json_output,
        compact,
        pretty,
        terse,
        schema,
        envelope,
    };
    let root = tsift::lint::resolve_project_root_or_canonical_path(&base_path)?;
    let search_cache_dir = root.join(".tsift/search-cache");
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
        tsift::config::Config::infer_submodule_from_path(&root, &base_path)?
    } else {
        None
    };

    let (symbol_hits, sift_path, federated_tagpath_diag) =
        if let Some(scope) = inferred_scope.as_ref() {
            let cfg = tsift::config::Config::load(&root)?;
            let db_path = cfg.db_path_for(&root, &scope.id);
            let hits = if db_path.exists() {
                let db = tsift::index::IndexDb::open_read_only_resilient(&db_path)?;
                db.symbol_search(&query, limit)?
            } else {
                Vec::new()
            };
            (hits, scope.source_root.clone(), None)
        } else if let Some(ref scope_name) = scope {
            let cfg = tsift::config::Config::load(&root)?;
            let scope = tsift::config::Config::resolve_submodule(&root, scope_name)?;
            let db_path = cfg.db_path_for(&root, &scope.id);
            let hits = if db_path.exists() {
                let db = tsift::index::IndexDb::open_read_only_resilient(&db_path)?;
                db.symbol_search(&query, limit)?
            } else {
                Vec::new()
            };
            (hits, scope.source_root, None)
        } else if federated {
            let (hits, diag) = federated_symbol_search(&root, &query, limit, &tagpath_opts)?;
            (hits, root.clone(), Some(diag))
        } else {
            let db_path = root.join(".tsift/index.db");
            let hits = if db_path.exists() {
                let db = tsift::index::IndexDb::open_read_only_resilient(&db_path)?;
                db.symbol_search(&query, limit)?
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

    let response = if exact_search {
        if federated && scope.is_none() {
            federated_exact_search(&root, &query, limit, timeout_secs)?
        } else {
            let exact_path = if requested_exact_search && scope.is_none() {
                &base_path
            } else {
                &sift_path
            };
            run_exact_search_with_timeout(exact_path, &query, limit, timeout_secs)?
        }
    } else if federated && scope.is_none() {
        federated_sift_search(
            &root,
            &search_cache_dir,
            &query,
            limit,
            timeout_secs,
            &effective_strategy,
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
        )?
    };

    if budget.is_active() {
        let report = build_search_budget_report(
            &query,
            &effective_strategy,
            &root,
            &response,
            &symbol_hits,
            absolute,
            budget,
        );
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
            symbols: &'a [tsift::index::SymbolHit],
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
    Ok(())
}

pub(crate) fn cmd_search_worker(
    path: &Path,
    cache_dir: &Path,
    query: &str,
    limit: usize,
    strategy: &str,
    output: &Path,
) -> Result<()> {
    maybe_apply_search_worker_test_hooks()?;
    let response = run_sift_search(path, cache_dir, query, limit, strategy)?;
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
