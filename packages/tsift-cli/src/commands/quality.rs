use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::{
    inject_tagpath_stale_into_json, tagpath_audit_policy_hints,
    tagpath_audit_supported_extensions, to_json_schema,
};


pub(crate) fn cmd_audit_tagpath(
    path: &std::path::Path,
    scope: Option<&str>,
    json_output: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let workspace_root = tsift::lint::resolve_project_root_or_canonical_path(path)?;

    // Choose the project root for tagpath + the index.db path for tsift
    // based on whether the caller passed `--scope`. Scoped mode points
    // both indexes at the submodule.
    let (tagpath_root, db_path) = if let Some(scope_selector) = scope {
        let cfg = tsift::config::Config::load(&workspace_root)?;
        let resolved = tsift::config::Config::resolve_submodule(&workspace_root, scope_selector)?;
        let db = cfg.db_path_for(&workspace_root, &resolved.id);
        (resolved.source_root, db)
    } else {
        (
            workspace_root.clone(),
            workspace_root.join(".tsift/index.db"),
        )
    };

    if !db_path.exists() {
        bail!(
            "no tsift index found at {}. Run `tsift index` first.",
            db_path.display()
        );
    }
    let db = tsift::index::IndexDb::open_read_only_resilient(&db_path)?;

    // Collect tsift file paths and remember the absolute form so we can
    // re-query `symbols_for_file` later. Tsift stores absolute paths;
    // tagpath stores relative-to-root.
    let mut tsift_abs_by_rel: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut tsift_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for raw in db.file_paths()? {
        let path = std::path::Path::new(&raw);
        let abs = if path.is_absolute() {
            std::path::PathBuf::from(&raw)
        } else {
            tagpath_root.join(&raw)
        };
        let rel = abs
            .strip_prefix(&tagpath_root)
            .ok()
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| raw.clone());
        tsift_files.insert(rel.clone());
        tsift_abs_by_rel.insert(rel, raw);
    }

    let load = tsift::tagpath_adapter::try_load(&tagpath_root);
    let (adapter, tagpath_state) = match load {
        tsift::tagpath_adapter::LoadResult::Loaded(adapter) => (Some(adapter), "fresh"),
        tsift::tagpath_adapter::LoadResult::Stale { reason, .. } => {
            eprintln!(
                "tagpath_index_stale: true (reason={reason}); audit results reflect the last loaded snapshot",
            );
            // Still load the on-disk index so the audit produces useful
            // output; the staleness is reported in the response.
            let idx_path = tagpath::index::index_path(&tagpath_root);
            match tagpath::index::read(&idx_path) {
                Ok(index) => (
                    Some(tsift::tagpath_adapter::TagpathAdapter {
                        project_root: tagpath_root.clone(),
                        index,
                    }),
                    "stale",
                ),
                Err(_) => (None, "stale_unreadable"),
            }
        }
        tsift::tagpath_adapter::LoadResult::Missing => (None, "missing"),
    };

    let tagpath_files: std::collections::BTreeSet<String> = adapter
        .as_ref()
        .map(|a| {
            a.index
                .sources
                .iter()
                .map(|s| s.path.replace('\\', "/"))
                .collect()
        })
        .unwrap_or_default();

    let tsift_only_files: Vec<String> = tsift_files
        .iter()
        .filter(|f| !tagpath_files.contains(*f))
        .cloned()
        .collect();
    let tagpath_only_files: Vec<String> = tagpath_files
        .iter()
        .filter(|f| !tsift_files.contains(*f))
        .cloned()
        .collect();
    let tagpath_supported_extensions = tagpath_audit_supported_extensions(&tagpath_root);
    let tsift_only_policy_hints: Vec<(String, Vec<String>)> = tsift_only_files
        .iter()
        .filter_map(|file| {
            let hints = tagpath_audit_policy_hints(file, &tagpath_supported_extensions);
            if hints.is_empty() {
                None
            } else {
                Some((file.clone(), hints))
            }
        })
        .collect();
    let tsift_only_policy_hints_by_file: BTreeMap<&str, &[String]> = tsift_only_policy_hints
        .iter()
        .map(|(file, hints)| (file.as_str(), hints.as_slice()))
        .collect();

    // Aggregate tsift symbols whose definition file is in tsift_only_files
    // (these are the symbols that lose `tagpath_handle` recall today).
    let mut unindexed_symbol_count: usize = 0;
    let mut unindexed_files_with_symbols: Vec<(String, usize)> = Vec::new();
    for rel in &tsift_only_files {
        let lookup = tsift_abs_by_rel
            .get(rel)
            .map(String::as_str)
            .unwrap_or(rel.as_str());
        let count = db.symbols_for_file(lookup).map(|v| v.len()).unwrap_or(0);
        unindexed_symbol_count += count;
        if count > 0 {
            unindexed_files_with_symbols.push((rel.clone(), count));
        }
    }

    if json_output {
        let mut value = serde_json::json!({
            "project_root": tagpath_root.to_string_lossy(),
            "scope": scope,
            "tagpath_state": tagpath_state,
            "tsift_file_count": tsift_files.len(),
            "tagpath_file_count": tagpath_files.len(),
            "tsift_only_files": tsift_only_files,
            "tagpath_only_files": tagpath_only_files,
            "tsift_only_symbol_count": unindexed_symbol_count,
            "tsift_only_files_with_symbols": unindexed_files_with_symbols
                .iter()
                .map(|(file, count)| serde_json::json!({ "file": file, "symbols": count }))
                .collect::<Vec<_>>(),
            "tsift_only_files_with_policy_hints": tsift_only_policy_hints
                .iter()
                .map(|(file, hints)| serde_json::json!({ "file": file, "hints": hints }))
                .collect::<Vec<_>>(),
        });
        if tagpath_state == "stale" {
            inject_tagpath_stale_into_json(&mut value, true, Some("stale_snapshot_loaded"));
        }
        println!("{}", to_json_schema(&value, pretty, terse, schema)?);
    } else {
        println!("tagpath audit for {}", tagpath_root.display());
        if let Some(scope) = scope {
            println!("scope: {scope}");
        }
        println!("tagpath_state: {tagpath_state}");
        println!(
            "tsift files: {} | tagpath files: {}",
            tsift_files.len(),
            tagpath_files.len()
        );
        if tsift_only_files.is_empty() && tagpath_only_files.is_empty() {
            println!("✓ tsift and tagpath cover the same source set.");
        } else {
            if !tsift_only_files.is_empty() {
                println!(
                    "\ntsift-only files ({} files, {} symbols miss tagpath_handle):",
                    tsift_only_files.len(),
                    unindexed_symbol_count
                );
                for (file, count) in &unindexed_files_with_symbols {
                    let hints = tsift_only_policy_hints_by_file
                        .get(file.as_str())
                        .map(|hints| format!("; hints: {}", hints.join(", ")))
                        .unwrap_or_default();
                    println!(
                        "  {file}  ({count} sym{}{hints})",
                        if *count == 1 { "" } else { "s" }
                    );
                }
                for file in &tsift_only_files {
                    if !unindexed_files_with_symbols.iter().any(|(f, _)| f == file) {
                        let hints = tsift_only_policy_hints_by_file
                            .get(file.as_str())
                            .map(|hints| format!("; hints: {}", hints.join(", ")))
                            .unwrap_or_default();
                        println!("  {file}  (0 syms{hints})");
                    }
                }
            }
            if !tagpath_only_files.is_empty() {
                println!(
                    "\ntagpath-only files ({} files, no tsift symbols extracted):",
                    tagpath_only_files.len()
                );
                for file in &tagpath_only_files {
                    println!("  {file}");
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_audit(
    skills_dir: &str,
    manifest: Option<PathBuf>,
    usage: bool,
    cleanup: bool,
    report: Option<PathBuf>,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let expanded = if let Some(rest) = skills_dir.strip_prefix("~/") {
        let home = std::env::var("HOME").context("HOME not set")?;
        std::path::PathBuf::from(format!("{}/{}", home, rest))
    } else {
        std::path::PathBuf::from(skills_dir)
    };

    let mut result = tsift::audit::scan_skills(&expanded)?;

    if let Some(manifest_path) = manifest {
        tsift::audit::compare_manifest(&mut result, &manifest_path)?;
    }

    if usage || cleanup || report.is_some() {
        tsift::audit::track_usage(&mut result)?;
    }

    if cleanup || report.is_some() {
        tsift::audit::generate_cleanup(&mut result);
    }

    if let Some(report_path) = &report {
        tsift::audit::write_report(&result, report_path)?;
        println!("Report written to {}", report_path.display());
    }

    if json_output {
        println!("{}", to_json_schema(&result, pretty, terse, schema)?);
    } else if compact {
        println!(
            "skills:{} healthy:{} broken:{}",
            result.total, result.healthy, result.broken
        );
        for skill in &result.skills {
            let status = if skill.issues.is_empty() { "ok" } else { "bad" };
            let uses = skill
                .invocation_count
                .map(|count| format!(" uses:{count}"))
                .unwrap_or_default();
            println!("  {} {}{}", status, skill.name, uses);
            for issue in &skill.issues {
                println!("    ! {}", issue);
            }
        }
        if let Some(diffs) = &result.manifest_diffs
            && !diffs.is_empty()
        {
            println!("manifest_diffs:{}", diffs.len());
        }
        if !result.similar_pairs.is_empty() {
            println!("similar_pairs:{}", result.similar_pairs.len());
        }
        if let Some(cleanup_list) = &result.cleanup
            && !cleanup_list.is_empty()
        {
            println!("cleanup:{}", cleanup_list.len());
        }
    } else {
        println!("Skills directory: {}", result.skills_dir.display());
        println!(
            "Total: {}  Healthy: {}  Broken: {}",
            result.total, result.healthy, result.broken
        );
        println!();
        for skill in &result.skills {
            let status = if skill.issues.is_empty() {
                "✓"
            } else {
                "✗"
            };
            let desc = skill.description.as_deref().unwrap_or("-");
            let link = if skill.is_symlink { " (symlink)" } else { "" };
            let uses = skill
                .invocation_count
                .map(|c| format!(" [{} uses]", c))
                .unwrap_or_default();
            println!("  {} {}{} — {}{}", status, skill.name, link, desc, uses);
            for issue in &skill.issues {
                println!("    ! {}", issue);
            }
        }
        if let Some(diffs) = &result.manifest_diffs
            && !diffs.is_empty()
        {
            println!();
            println!("Manifest diffs:");
            for diff in diffs {
                let label = match diff.kind {
                    tsift::audit::DiffKind::Missing => "missing (expected but not installed)",
                    tsift::audit::DiffKind::Orphan => "orphan (installed but not in manifest)",
                };
                println!("  {} — {}", diff.name, label);
            }
        }
        if !result.similar_pairs.is_empty() {
            println!();
            println!("Possible duplicates (description similarity >= 30%):");
            for pair in &result.similar_pairs {
                println!(
                    "  {:.0}%  {} / {}",
                    pair.score * 100.0,
                    pair.skill_a,
                    pair.skill_b
                );
                println!("       A: {}", pair.desc_a);
                println!("       B: {}", pair.desc_b);
            }
        }
        if let Some(cleanup_list) = &result.cleanup
            && !cleanup_list.is_empty()
        {
            println!();
            println!("Cleanup recommendations:");
            for entry in cleanup_list {
                println!("  {} (~{} tokens)", entry.skill, entry.token_estimate);
                for reason in &entry.reasons {
                    println!("    - {}", reason);
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_lint(
    file: &str,
    index: Option<PathBuf>,
    entities_from: Vec<PathBuf>,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    use std::collections::HashSet;

    let file_path = std::path::Path::new(file);
    if !file_path.exists() {
        anyhow::bail!("file not found: {}", file);
    }

    let mut entities = HashSet::new();

    if let Some(index_dir) = index {
        entities.extend(tsift::lint::collect_entities_from_index_path(&index_dir)?);
    } else if let Some(root) = tsift::lint::find_project_root_for_path(file_path)? {
        entities.extend(tsift::lint::collect_entities_from_workspace_root(&root)?);
    }

    for md_path in &entities_from {
        entities.extend(tsift::lint::collect_entities_from_markdown(md_path)?);
    }

    entities.extend(tsift::lint::collect_entities_from_markdown(file_path)?);

    let result = tsift::lint::lint_markdown(file_path, &entities)?;

    if json_output {
        println!("{}", to_json_schema(&result, pretty, terse, schema)?);
    } else if compact {
        if result.annotations.is_empty() {
            println!("ok {}", file);
        } else {
            println!("{} annotations:{}", result.file, result.annotations.len());
            for ann in &result.annotations {
                println!(
                    "  {}:{} {} -> {}",
                    ann.line, ann.column, ann.text, ann.suggestion
                );
            }
        }
    } else {
        if result.annotations.is_empty() {
            println!("No unannotated concepts found in {}", file);
        } else {
            println!("{}:", result.file);
            for ann in &result.annotations {
                println!(
                    "  {}:{}: {} → {}",
                    ann.line, ann.column, ann.text, ann.suggestion
                );
            }
            println!();
            println!("{} unannotated concept(s) found.", result.annotations.len());
        }
    }

    Ok(())
}
