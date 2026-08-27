use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use tsift_quality::lint;
use tsift_summarize::summarize;

use crate::{
    collect_source_files, emit_summary_stats_warnings, find_symbols_db_for_file,
    load_summarize_config, open_existing_summary_db_read_only, resolve_extract_base,
    resolve_extract_scope, summarize_diff_matches_scope,
    summarize_full_extract_deleted_summary_paths, summarize_relative_file_path, to_json_schema,
    truncate_for_compact,
};

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_summarize(
    symbol: Option<String>,
    file: Option<String>,
    extract: Option<PathBuf>,
    diff: bool,
    force: bool,
    max_file_tokens: Option<usize>,
    stats: bool,
    path: &std::path::Path,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
    profile: Option<String>,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let db_path = root.join(".tsift/summaries.db");

    // --extract mode: run LLM extraction
    if let Some(extract_path) = extract {
        if let Some(note) = crate::profile_preference_note(
            profile.as_deref(),
            tsift_local_model::ModelRole::Extract,
        ) {
            eprintln!("note: {note}");
        }
        let extract_base = resolve_extract_base(path)?;
        let extract_scope = resolve_extract_scope(&extract_base, &extract_path)?;
        let mut cfg = load_summarize_config(&root);
        if let Some(max_file_tokens) = max_file_tokens {
            if max_file_tokens == 0 {
                bail!("--max-file-tokens must be greater than zero");
            }
            cfg.max_file_tokens = max_file_tokens;
        }

        let (files_to_extract, mut deleted_summary_paths) = if diff {
            let changed = summarize::git_changed_files(&root)?;
            let existing = changed
                .existing
                .into_iter()
                .filter(|f| summarize_diff_matches_scope(f, &extract_scope))
                .collect::<Vec<_>>();
            let deleted_summary_paths = changed
                .deleted
                .into_iter()
                .filter(|f| summarize_diff_matches_scope(f, &extract_scope))
                .map(|file_path| summarize_relative_file_path(&root, &file_path))
                .collect::<BTreeSet<_>>();
            if existing.is_empty() && deleted_summary_paths.is_empty() && !db_path.exists() {
                println!("No files to extract.");
                return Ok(());
            }
            (existing, deleted_summary_paths)
        } else {
            (collect_source_files(&extract_scope)?, BTreeSet::new())
        };

        if !diff && files_to_extract.is_empty() && !db_path.exists() {
            println!("No files to extract.");
            return Ok(());
        }

        let _summary_write_lock = summarize::acquire_write_lock(&db_path)?;
        let summary_cache = summarize::SummaryCache::new(summarize::SummaryDb::open(&db_path)?);
        summary_cache
            .db()
            .prune_terminal_failures_for_non_candidates(&root)?;

        if !diff {
            deleted_summary_paths.extend(summarize_full_extract_deleted_summary_paths(
                summary_cache.db(),
                &root,
                &extract_scope,
                &files_to_extract,
            )?);
        }

        if files_to_extract.is_empty() && deleted_summary_paths.is_empty() {
            println!("No files to extract.");
            return Ok(());
        }

        for rel_path in &deleted_summary_paths {
            summary_cache.db().delete_by_file(rel_path)?;
            summary_cache.invalidate_file(rel_path, None);
        }

        let mut report = summarize::ExtractionReport {
            files_processed: 0,
            symbols_extracted: 0,
            tokens_input: 0,
            tokens_output: 0,
            terminal_failures_skipped: 0,
            errors: Vec::new(),
        };
        let mut too_large_failures_skipped = 0usize;
        let mut largest_required_tokens = None::<usize>;

        let mut pending_extractions = Vec::new();
        for file_path in &files_to_extract {
            let content = match std::fs::read(file_path) {
                Ok(c) => c,
                Err(e) => {
                    report
                        .errors
                        .push(format!("{}: {}", file_path.display(), e));
                    continue;
                }
            };
            if content.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let hash = summarize::content_hash(&content);
            let rel_path = summarize_relative_file_path(&root, file_path);
            if !force
                && let Some(failure) = summary_cache.db().terminal_failure(&rel_path, &hash)?
                && failure.applies_at_max_file_tokens(cfg.max_file_tokens)
            {
                report.terminal_failures_skipped += 1;
                if failure.kind == summarize::ExtractionFailureKind::TooLarge {
                    too_large_failures_skipped += 1;
                    if let Some(required) = failure.required_max_file_tokens() {
                        largest_required_tokens = Some(
                            largest_required_tokens
                                .map_or(required, |current| current.max(required)),
                        );
                    }
                }
                continue;
            }
            if summary_cache.current_by_file(&rel_path, &hash)?.is_none() {
                pending_extractions.push((file_path, hash, rel_path));
            }
        }

        let extraction_client = if pending_extractions.is_empty() {
            None
        } else {
            Some(summarize::ExtractionClient::resolve(&cfg)?)
        };

        let extraction_count = pending_extractions.len();
        for (index, (file_path, hash, rel_path)) in pending_extractions.into_iter().enumerate() {
            eprintln!(
                "extracting {}/{}: {}",
                index + 1,
                extraction_count,
                rel_path
            );
            match summary_cache.get_or_extract_file(&rel_path, &hash, || {
                let symbol_context = find_symbols_db_for_file(&root, file_path)?;
                let mut summaries = summarize::extract_for_file_with_client(
                    file_path,
                    symbol_context.as_ref().map(|ctx| ctx.db_path.as_path()),
                    symbol_context.as_ref().map(|ctx| ctx.source_root.as_path()),
                    &cfg,
                    extraction_client
                        .as_ref()
                        .expect("pending_extractions is non-empty inside extraction loop"),
                )?;
                for summary in &mut summaries {
                    summary.file_path = rel_path.clone();
                }
                Ok(summaries)
            }) {
                Ok(lookup) => {
                    if lookup.source == summarize::SummaryCacheSource::Cached {
                        continue;
                    }
                    let extracted_count = lookup.summaries.len();
                    let tokens_input = lookup
                        .summaries
                        .iter()
                        .map(|summary| summary.tokens_input.unwrap_or(0))
                        .sum::<i64>();
                    let tokens_output = lookup
                        .summaries
                        .iter()
                        .map(|summary| summary.tokens_output.unwrap_or(0))
                        .sum::<i64>();
                    report.symbols_extracted += extracted_count;
                    report.tokens_input += tokens_input;
                    report.tokens_output += tokens_output;
                    report.files_processed += 1;
                    if !json_output && !compact {
                        println!("  extracted: {}", rel_path);
                    }
                }
                Err(e) => {
                    if let Some((kind, message)) = summarize::terminal_extraction_failure(&e) {
                        let failed_limit = (kind == summarize::ExtractionFailureKind::TooLarge)
                            .then_some(cfg.max_file_tokens);
                        summary_cache.db().record_terminal_failure_with_limit(
                            &rel_path,
                            &hash,
                            kind,
                            failed_limit,
                            &message,
                        )?;
                    }
                    report.errors.push(format!("{}: {}", rel_path, e));
                    if !json_output {
                        eprintln!("  error: {}: {}", rel_path, e);
                    }
                }
            }
        }

        if json_output {
            println!("{}", to_json_schema(&report, pretty, terse, false, schema)?);
        } else if compact {
            println!(
                "extract files:{} symbols:{} tokens_in:{} tokens_out:{} terminal_skipped:{} errors:{}",
                report.files_processed,
                report.symbols_extracted,
                report.tokens_input,
                report.tokens_output,
                report.terminal_failures_skipped,
                report.errors.len()
            );
        } else {
            println!("\nExtraction complete:");
            println!("  files: {}", report.files_processed);
            println!("  symbols: {}", report.symbols_extracted);
            println!(
                "  tokens: {} in / {} out",
                report.tokens_input, report.tokens_output
            );
            if !report.errors.is_empty() {
                println!("  errors: {}", report.errors.len());
            }
            if report.terminal_failures_skipped > 0 {
                let other_failures = report
                    .terminal_failures_skipped
                    .saturating_sub(too_large_failures_skipped);
                let mut hints = Vec::new();
                if too_large_failures_skipped > 0 {
                    let hint = largest_required_tokens.map_or_else(
                        || "raise --max-file-tokens or use --force to retry".to_string(),
                        |required| format!("raise --max-file-tokens to at least {required}"),
                    );
                    hints.push(format!("{too_large_failures_skipped} too_large; {hint}"));
                }
                if other_failures > 0 {
                    hints.push(format!(
                        "use --force to retry {other_failures} other failure(s)"
                    ));
                }
                println!(
                    "  skipped terminal failures: {} ({})",
                    report.terminal_failures_skipped,
                    hints.join("; ")
                );
            }
        }
        return Ok(());
    }

    // --stats mode
    if stats {
        let summary_db = open_existing_summary_db_read_only(&db_path)?;
        let s = summary_db.stats(&root)?;
        if json_output {
            println!("{}", to_json_schema(&s, pretty, terse, false, schema)?);
        } else if compact {
            println!(
                "summaries:{} files:{} stale:{} in:{} out:{} saved:{}",
                s.total_summaries,
                s.total_files,
                s.stale_count,
                s.total_tokens_input,
                s.total_tokens_output,
                s.estimated_tokens_saved
            );
        } else {
            println!("Summary cache statistics:");
            println!("  summaries:       {}", s.total_summaries);
            println!("  files:           {}", s.total_files);
            println!("  stale files:     {}", s.stale_count);
            println!("  tokens input:    {}", s.total_tokens_input);
            println!("  tokens output:   {}", s.total_tokens_output);
            println!("  est. savings:    {} tokens", s.estimated_tokens_saved);
        }
        emit_summary_stats_warnings(&s, &root);
        return Ok(());
    }

    // Query mode: --file or positional symbol
    let summary_db = open_existing_summary_db_read_only(&db_path)?;

    if let Some(file_query) = file {
        let query_base = resolve_extract_base(path)?;
        let mut results = Vec::new();
        for candidate in
            summarize::file_lookup_candidates(Path::new(&file_query), &query_base, &root)
        {
            results = summary_db.get_by_file(&candidate)?;
            if !results.is_empty() {
                break;
            }
        }
        if results.is_empty() {
            println!("No cached summary for file: {}", file_query);
            println!("Run: tsift summarize --extract <path>");
            return Ok(());
        }
        if json_output {
            println!(
                "{}",
                to_json_schema(&results, pretty, terse, false, schema)?
            );
        } else if compact {
            for summary in &results {
                println!(
                    "[{}] {}",
                    summary.symbol_name,
                    truncate_for_compact(&summary.summary, 120)
                );
            }
        } else {
            for s in &results {
                println!("[{}] {}", s.symbol_name, s.summary);
                if let Some(ref labels) = s.concept_labels
                    && !labels.is_empty()
                {
                    println!("  concepts: {}", labels.join(", "));
                }
            }
        }
        return Ok(());
    }

    if let Some(sym) = symbol {
        let results = summary_db.get_by_symbol(&sym)?;
        if results.is_empty() {
            println!("No cached summary for symbol: {}", sym);
            println!("Run: tsift summarize --extract <path>");
            return Ok(());
        }
        if json_output {
            println!(
                "{}",
                to_json_schema(&results, pretty, terse, false, schema)?
            );
        } else if compact {
            for summary in &results {
                println!(
                    "{} {}",
                    summary.symbol_name,
                    truncate_for_compact(&summary.summary, 120)
                );
            }
        } else {
            for s in &results {
                println!("{} ({})", s.symbol_name, s.file_path);
                println!("  {}", s.summary);
                if let Some(ref entities) = s.entities
                    && !entities.is_empty()
                {
                    println!("  entities:");
                    for e in entities {
                        println!("    {} ({}): {}", e.name, e.kind, e.description);
                    }
                }
                if let Some(ref rels) = s.relationships
                    && !rels.is_empty()
                {
                    println!("  relationships:");
                    for r in rels {
                        println!("    {} --{}-> {}", r.from, r.kind, r.to);
                    }
                }
                if let Some(ref labels) = s.concept_labels
                    && !labels.is_empty()
                {
                    println!("  concepts: {}", labels.join(", "));
                }
                println!();
            }
        }
        return Ok(());
    }

    bail!("specify a symbol, --file, --extract, or --stats");
}
