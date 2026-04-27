use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sift::{SearchInput, SearchOptions, Sift};
use std::fs;
use std::io::Read as _;
use std::path::PathBuf;

pub mod audit;
pub mod init;
pub mod lint;
pub mod config;
pub mod graph;
pub mod index;
mod lang;
pub mod status;
pub mod summarize;
pub mod walk;

#[derive(Parser)]
#[command(name = "tsift", version, about = "Token-efficient search for Claude Code")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Search a codebase (lexical by default; hybrid/vector available)
    Search {
        /// Query string
        query: String,
        /// Path to search (defaults to current directory)
        #[arg(short, long)]
        path: Option<PathBuf>,
        /// Maximum number of results
        #[arg(short, long, default_value = "10")]
        limit: usize,
        /// Search strategy: lexical, vector, hybrid, path-hybrid
        #[arg(short, long)]
        strategy: Option<String>,
        /// Restrict search to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Search all federated submodule indexes
        #[arg(long)]
        federated: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Apply multiple file edits in one invocation (reads JSON from stdin)
    Edit {
        /// Preview changes without writing
        #[arg(long)]
        dry_run: bool,
        /// Read edits from a file instead of stdin
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Recommend a Claude model tier for a task (haiku/search, sonnet/edit, opus/architecture)
    Route {
        /// Task description to classify
        task: String,
        /// Output only the model ID (for scripting)
        #[arg(long)]
        id: bool,
    },
    /// Rewrite a shell command to use tsift (for Claude Code hook integration)
    Rewrite {
        /// The shell command to potentially rewrite
        command: String,
    },
    /// Build or update the file index (mtime-based incremental)
    Index {
        /// Path to index (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Drop existing state and re-index from scratch
        #[arg(long)]
        rebuild: bool,
        /// Report stale files without updating the index
        #[arg(long)]
        check: bool,
        /// Exit with code 1 when --check finds stale files (for scripting/hooks)
        #[arg(long)]
        exit_code: bool,
        /// Skip unchanged directory subtrees (directory mtime pruning for large repos)
        #[arg(long)]
        prune: bool,
        /// Index all submodules into per-submodule databases
        #[arg(long)]
        workspace: bool,
        /// Index only this submodule
        #[arg(long)]
        submodule: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Query the call graph (callers/callees of a symbol)
    Graph {
        /// Symbol name to query
        symbol: String,
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Show callers of the symbol
        #[arg(long)]
        callers: bool,
        /// Show callees of the symbol
        #[arg(long)]
        callees: bool,
        /// Restrict to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Query a SQLite database — show schema or run SQL
    Sql {
        /// Path to SQLite database file
        db: PathBuf,
        /// SQL query to execute (omit for schema overview)
        #[arg(short, long)]
        query: Option<String>,
        /// Show schema for a specific table
        #[arg(short, long)]
        table: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Detect architectural communities using Louvain clustering over the call graph
    Communities {
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Show only communities with at least this many members
        #[arg(long, default_value = "2")]
        min_size: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Find the shortest path between two symbols in the call graph
    Path {
        /// Source symbol name
        from: String,
        /// Target symbol name
        to: String,
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show full context for a symbol: callers, callees, and community membership
    Explain {
        /// Symbol name to explain
        symbol: String,
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Audit installed Claude Code skills — scan directories, check health, compare against manifest
    Audit {
        /// Path to the skills directory
        #[arg(long, default_value = "~/.claude/skills")]
        skills_dir: String,
        /// Path to a manifest file listing expected skills (one per line)
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Track skill usage from session history
        #[arg(long)]
        usage: bool,
        /// Generate cleanup recommendations
        #[arg(long)]
        cleanup: bool,
        /// Write markdown report to this path
        #[arg(long)]
        report: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Lint markdown files — detect unannotated concepts (symbols, headings, bold terms)
    Lint {
        /// Markdown file to lint
        file: String,
        /// Path to index directory (uses .tsift/ by default for symbol entities)
        #[arg(long)]
        index: Option<PathBuf>,
        /// Additional markdown files to extract entities from
        #[arg(long)]
        entities_from: Vec<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Initialize tsift in a project — inject Code Navigation section into AGENTS.md/CLAUDE.md
    Init {
        /// Path to the project directory (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Cached LLM analysis — pre-computed summaries, entities, relationships
    Summarize {
        /// Symbol name to look up
        symbol: Option<String>,
        /// Show cached summary for a file/module
        #[arg(long)]
        file: Option<String>,
        /// Run LLM extraction on the given path
        #[arg(long)]
        extract: Option<PathBuf>,
        /// Only re-extract git-changed files (use with --extract)
        #[arg(long)]
        diff: bool,
        /// Show cache statistics
        #[arg(long)]
        stats: bool,
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Report index + summary status and recommended commands for this session
    Status {
        /// Path to the codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Deserialize)]
struct EditBatch {
    edits: Vec<EditOp>,
}

#[derive(Deserialize)]
struct EditOp {
    /// File path to edit
    file: PathBuf,
    /// Text to find and replace
    old: String,
    /// Replacement text
    new: String,
    /// Replace all occurrences (default: false — fails if not unique)
    #[serde(default)]
    replace_all: bool,
}

#[derive(Serialize)]
struct EditResult {
    file: PathBuf,
    status: EditStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replacements: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum EditStatus {
    Ok,
    Error,
    Skipped,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Search {
            query,
            path,
            limit,
            strategy,
            scope,
            federated,
            json,
        }) => cmd_search(query, path, limit, strategy, scope, federated, json),
        Some(Commands::Edit { dry_run, file }) => cmd_edit(dry_run, file),
        Some(Commands::Index { path, rebuild, check, exit_code, prune, workspace, submodule, json }) => cmd_index(&path, rebuild, check, exit_code, prune, workspace, submodule.as_deref(), json),
        Some(Commands::Rewrite { command }) => cmd_rewrite(&command),
        Some(Commands::Route { task, id }) => cmd_route(&task, id),
        Some(Commands::Graph { symbol, path, callers, callees, scope, json }) => cmd_graph(&symbol, &path, callers, callees, scope.as_deref(), json),
        Some(Commands::Sql { db, query, table, json }) => cmd_sql(&db, query, table, json),
        Some(Commands::Communities { path, scope, min_size, json }) => cmd_communities(&path, scope.as_deref(), min_size, json),
        Some(Commands::Path { from, to, path, scope, json }) => cmd_path(&from, &to, &path, scope.as_deref(), json),
        Some(Commands::Explain { symbol, path, scope, json }) => cmd_explain(&symbol, &path, scope.as_deref(), json),
        Some(Commands::Audit { skills_dir, manifest, usage, cleanup, report, json }) => cmd_audit(&skills_dir, manifest, usage, cleanup, report, json),
        Some(Commands::Init { path }) => cmd_init(&path),
        Some(Commands::Lint { file, index, entities_from, json }) => cmd_lint(&file, index, entities_from, json),
        Some(Commands::Summarize { symbol, file, extract, diff, stats, path, json }) => cmd_summarize(symbol, file, extract, diff, stats, &path, json),
        Some(Commands::Status { path, json }) => cmd_status(&path, json),
        None => {
            println!("tsift v{}", env!("CARGO_PKG_VERSION"));
            println!("Run `tsift --help` for usage.");
            Ok(())
        }
    }
}

/// Classify a task description into a model tier.
/// Returns (tier_name, model_id).
pub fn classify_task(task: &str) -> (&'static str, &'static str) {
    let lower = task.to_lowercase();
    // Architecture/design signals → opus
    for signal in &["architect", "architecture", "design", "plan", "strateg", "analy", "review", "evaluate", "assess"] {
        if lower.contains(signal) {
            return ("opus", "claude-opus-4-6");
        }
    }
    // Edit/write signals → sonnet
    for signal in &["edit", "write", "fix", "change", "update", "create", "add ", "remove", "delete", "modify", "refactor", "implement", "build"] {
        if lower.contains(signal) {
            return ("sonnet", "claude-sonnet-4-6");
        }
    }
    // Default: search/lookup → haiku
    ("haiku", "claude-haiku-4-5-20251001")
}

fn cmd_route(task: &str, id_only: bool) -> Result<()> {
    let (tier, model_id) = classify_task(task);
    if id_only {
        println!("{}", model_id);
    } else {
        println!("tier:  {}", tier);
        println!("model: {}", model_id);
        println!("task:  {}", task);
    }
    Ok(())
}

/// Apply a single edit operation to file contents. Returns new content.
pub(crate) fn apply_edit_op(content: &str, op: &EditOp) -> Result<(String, usize)> {
    if op.old == op.new {
        bail!("old and new strings are identical");
    }
    let count = content.matches(op.old.as_str()).count();
    if count == 0 {
        bail!("old_string not found");
    }
    if count > 1 && !op.replace_all {
        bail!("old_string matches {} times (use replace_all or provide more context)", count);
    }
    let replaced = if op.replace_all {
        content.replace(op.old.as_str(), &op.new)
    } else {
        content.replacen(op.old.as_str(), &op.new, 1)
    };
    Ok((replaced, count))
}

fn cmd_edit(dry_run: bool, file: Option<PathBuf>) -> Result<()> {
    let input = match file {
        Some(path) => fs::read_to_string(&path)
            .with_context(|| format!("reading edit file: {}", path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading edits from stdin")?;
            buf
        }
    };
    let batch: EditBatch =
        serde_json::from_str(&input).context("parsing edit JSON")?;

    if batch.edits.is_empty() {
        println!("No edits provided.");
        return Ok(());
    }

    // Phase 1: validate all edits before writing any (atomic batch)
    let mut plan: Vec<(usize, String, usize)> = Vec::new(); // (idx, new_content, replacement_count)

    for (i, op) in batch.edits.iter().enumerate() {
        let content = fs::read_to_string(&op.file)
            .with_context(|| format!("edit #{}: reading {}", i + 1, op.file.display()))?;
        let (replaced, count) = apply_edit_op(&content, op)
            .with_context(|| format!("edit #{}: {}", i + 1, op.file.display()))?;
        plan.push((i, replaced, count));
    }

    // Phase 2: write all validated edits
    let mut results: Vec<EditResult> = Vec::new();

    for (i, new_content, count) in &plan {
        if dry_run {
            results.push(EditResult {
                file: batch.edits[*i].file.clone(),
                status: EditStatus::Skipped,
                error: Some("dry run".into()),
                replacements: Some(*count),
            });
        } else {
            match fs::write(&batch.edits[*i].file, new_content) {
                Ok(()) => {
                    results.push(EditResult {
                        file: batch.edits[*i].file.clone(),
                        status: EditStatus::Ok,
                        error: None,
                        replacements: Some(*count),
                    });
                }
                Err(e) => {
                    results.push(EditResult {
                        file: batch.edits[*i].file.clone(),
                        status: EditStatus::Error,
                        error: Some(e.to_string()),
                        replacements: None,
                    });
                }
            }
        }
    }

    // Summary output
    let ok_count = results.iter().filter(|r| matches!(r.status, EditStatus::Ok)).count();
    let skip_count = results.iter().filter(|r| matches!(r.status, EditStatus::Skipped)).count();
    let err_count = results.iter().filter(|r| matches!(r.status, EditStatus::Error)).count();

    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "applied": ok_count,
        "skipped": skip_count,
        "errors": err_count,
        "results": results,
    }))?);

    if err_count > 0 {
        bail!("{} edit(s) failed", err_count);
    }
    Ok(())
}

fn cmd_index(path: &std::path::Path, rebuild: bool, check: bool, exit_code: bool, prune: bool, workspace: bool, submodule: Option<&str>, json_output: bool) -> Result<()> {
    let root = path.canonicalize()
        .with_context(|| format!("resolving path: {}", path.display()))?;

    if workspace || submodule.is_some() {
        let cfg = config::Config::load(&root)?;
        let targets: Vec<(String, PathBuf)> = if let Some(name) = submodule {
            let sub_path = config::Config::submodule_dirs(&root)?
                .into_iter()
                .find(|(n, _)| n == name)
                .map(|(_, p)| p)
                .unwrap_or_else(|| root.join(name));
            vec![(name.to_string(), sub_path)]
        } else {
            config::Config::submodule_dirs(&root)?
        };

        if targets.is_empty() {
            bail!("no submodules found in {}", root.display());
        }

        let mut any_stale = false;
        for (name, sub_path) in &targets {
            if !sub_path.exists() {
                eprintln!("  skip {} (not found: {})", name, sub_path.display());
                continue;
            }
            let db_path = cfg.db_path_for(&root, name);
            let db = index::IndexDb::open(&db_path)?;
            let summary = if rebuild {
                db.rebuild(sub_path)?
            } else if check {
                if prune { db.compute_changes_pruned(sub_path)? } else { db.compute_changes(sub_path)? }
            } else if prune {
                db.apply_changes_pruned(sub_path)?
            } else {
                db.apply_changes(sub_path)?
            };
            if summary.has_changes() {
                any_stale = true;
            }
            let tier = cfg.tier_for(name);
            if json_output {
                let entry = serde_json::json!({
                    "submodule": name,
                    "tier": format!("{:?}", tier).to_lowercase(),
                    "summary": summary,
                });
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                let mode = if rebuild { "rebuild" } else if check { "check" } else if prune { "pruned" } else { "incremental" };
                print!("[{}] ({}, {:?}) {} files tracked — new:{} mod:{} del:{} unch:{}",
                    name, mode, tier, summary.total_tracked,
                    summary.new, summary.modified, summary.deleted, summary.unchanged);
                if let Some(ref ps) = summary.prune_stats {
                    print!(" | pruned:{} dirs ({}d walked, {} files skipped)", ps.dirs_pruned, ps.dirs_walked, ps.files_pruned);
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
    let db = index::IndexDb::open(&db_path)?;
    let summary = if rebuild {
        db.rebuild(&root)?
    } else if check {
        if prune { db.compute_changes_pruned(&root)? } else { db.compute_changes(&root)? }
    } else if prune {
        db.apply_changes_pruned(&root)?
    } else {
        db.apply_changes(&root)?
    };

    if json_output {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        let mode = if rebuild { "rebuild" } else if check { "check" } else if prune { "pruned" } else { "incremental" };
        println!("Index ({}): {} files tracked", mode, summary.total_tracked);
        print!("  new: {}  modified: {}  deleted: {}  unchanged: {}",
            summary.new, summary.modified, summary.deleted, summary.unchanged);
        if let Some(ref ps) = summary.prune_stats {
            print!(" | pruned: {} dirs ({} walked, {} files skipped)", ps.dirs_pruned, ps.dirs_walked, ps.files_pruned);
        }
        println!();
        if !summary.changes.is_empty() {
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

fn cmd_graph(symbol: &str, path: &std::path::Path, callers: bool, callees: bool, scope: Option<&str>, json_output: bool) -> Result<()> {
    let root = path.canonicalize()
        .with_context(|| format!("resolving path: {}", path.display()))?;
    let db_path = if let Some(scope_name) = scope {
        let cfg = config::Config::load(&root)?;
        cfg.db_path_for(&root, scope_name)
    } else {
        root.join(".tsift/index.db")
    };
    if !db_path.exists() {
        bail!("no index found at {}. Run `tsift index` first.", db_path.display());
    }
    let db = index::IndexDb::open(&db_path)?;

    let show_both = !callers && !callees;

    if callers || show_both {
        let edges = db.callers_of(symbol)?;
        if json_output {
            if !show_both {
                println!("{}", serde_json::to_string_pretty(&edges)?);
            }
        } else {
            println!("Callers of `{}`:", symbol);
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
                    println!("  {} ({}:{})", edge.caller_name, edge.caller_file, edge.call_site_line);
                }
            }
        }
        if show_both && !json_output {
            println!();
        }
    }

    if callees || show_both {
        let edges = db.callees_of(symbol)?;
        if json_output {
            if !show_both {
                println!("{}", serde_json::to_string_pretty(&edges)?);
            }
        } else {
            println!("Callees of `{}`:", symbol);
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
                    println!("  {} ({}:{})", edge.callee_name, edge.caller_file, edge.call_site_line);
                }
            }
        }
    }

    if show_both && json_output {
        let callers_edges = db.callers_of(symbol)?;
        let callees_edges = db.callees_of(symbol)?;
        let combined = serde_json::json!({
            "symbol": symbol,
            "callers": callers_edges,
            "callees": callees_edges,
        });
        println!("{}", serde_json::to_string_pretty(&combined)?);
    }

    Ok(())
}

fn cmd_communities(path: &std::path::Path, scope: Option<&str>, min_size: usize, json_output: bool) -> Result<()> {
    let root = path.canonicalize()
        .with_context(|| format!("resolving path: {}", path.display()))?;
    let db_path = if let Some(scope_name) = scope {
        let cfg = config::Config::load(&root)?;
        cfg.db_path_for(&root, scope_name)
    } else {
        root.join(".tsift/index.db")
    };
    if !db_path.exists() {
        bail!("no index found at {}. Run `tsift index` first.", db_path.display());
    }
    let db = index::IndexDb::open(&db_path)?;
    let edges = db.all_edges()?;
    let result = graph::detect_communities(&edges);

    let filtered: Vec<&graph::Community> = result.communities.iter()
        .filter(|c| c.members.len() >= min_size)
        .collect();

    if json_output {
        let out = serde_json::json!({
            "modularity": result.modularity,
            "iterations": result.iterations,
            "node_count": result.node_count,
            "edge_count": result.edge_count,
            "community_count": filtered.len(),
            "communities": filtered,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("Communities ({} nodes, {} edges, {} iterations, Q={:.4})",
            result.node_count, result.edge_count, result.iterations, result.modularity);
        if filtered.is_empty() {
            println!("  (no communities with {} or more members)", min_size);
        } else {
            println!();
            for (i, c) in filtered.iter().enumerate() {
                println!("  [{}] {} members (Q={:.4}):", i + 1, c.members.len(), c.modularity_contribution);
                for m in &c.members {
                    println!("    {}", m);
                }
                if i + 1 < filtered.len() {
                    println!();
                }
            }
        }
    }
    Ok(())
}

fn open_index_db(path: &std::path::Path, scope: Option<&str>) -> Result<index::IndexDb> {
    let root = path.canonicalize()
        .with_context(|| format!("resolving path: {}", path.display()))?;
    let db_path = if let Some(scope_name) = scope {
        let cfg = config::Config::load(&root)?;
        cfg.db_path_for(&root, scope_name)
    } else {
        root.join(".tsift/index.db")
    };
    if !db_path.exists() {
        bail!("no index found at {}. Run `tsift index` first.", db_path.display());
    }
    index::IndexDb::open(&db_path)
}

fn cmd_path(from: &str, to: &str, path: &std::path::Path, scope: Option<&str>, json_output: bool) -> Result<()> {
    let db = open_index_db(path, scope)?;
    let edges = db.all_edges()?;
    match graph::shortest_path(&edges, from, to) {
        Some(result) => {
            if json_output {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{} → {} ({} hop{})", result.from, result.to, result.hops, if result.hops == 1 { "" } else { "s" });
                println!();
                for (i, node) in result.path.iter().enumerate() {
                    if i > 0 {
                        println!("  ↓");
                    }
                    println!("  {}", node);
                }
            }
        }
        None => {
            if json_output {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "from": from,
                    "to": to,
                    "path": null,
                    "hops": null,
                }))?);
            } else {
                println!("No path found between `{}` and `{}`.", from, to);
            }
        }
    }
    Ok(())
}

fn cmd_explain(symbol: &str, path: &std::path::Path, scope: Option<&str>, json_output: bool) -> Result<()> {
    let db = open_index_db(path, scope)?;

    let symbols = db.symbol_info(symbol)?;
    let callers = db.callers_of(symbol)?;
    let callees = db.callees_of(symbol)?;

    let edges = db.all_edges()?;
    let comm_result = graph::detect_communities(&edges);
    let community = comm_result.communities.iter()
        .find(|c| c.members.iter().any(|m| m == symbol));

    if json_output {
        let out = serde_json::json!({
            "symbol": symbol,
            "definitions": symbols,
            "callers": callers,
            "callees": callees,
            "community": community,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        if symbols.is_empty() {
            println!("Symbol `{}` not found in index.", symbol);
            println!("(Checking call graph for references...)");
            println!();
        } else {
            for sym in &symbols {
                println!("{} ({}, {})", sym.name, sym.kind, sym.language);
                println!("  {}:{}", sym.file, sym.line);
            }
            println!();
        }

        println!("Callers ({}):", callers.len());
        if callers.is_empty() {
            println!("  (none)");
        } else {
            for edge in &callers {
                println!("  {} ({}:{})", edge.caller_name, edge.caller_file, edge.call_site_line);
            }
        }
        println!();

        println!("Callees ({}):", callees.len());
        if callees.is_empty() {
            println!("  (none)");
        } else {
            for edge in &callees {
                println!("  {} ({}:{})", edge.callee_name, edge.caller_file, edge.call_site_line);
            }
        }

        if let Some(comm) = community {
            println!();
            println!("Community {} ({} members):", comm.id, comm.members.len());
            for m in &comm.members {
                let marker = if m == symbol { "→ " } else { "  " };
                println!("{}{}", marker, m);
            }
        }
    }
    Ok(())
}

fn cmd_audit(skills_dir: &str, manifest: Option<PathBuf>, usage: bool, cleanup: bool, report: Option<PathBuf>, json_output: bool) -> Result<()> {
    let expanded = if let Some(rest) = skills_dir.strip_prefix("~/") {
        let home = std::env::var("HOME").context("HOME not set")?;
        std::path::PathBuf::from(format!("{}/{}", home, rest))
    } else {
        std::path::PathBuf::from(skills_dir)
    };

    let mut result = audit::scan_skills(&expanded)?;

    if let Some(manifest_path) = manifest {
        audit::compare_manifest(&mut result, &manifest_path)?;
    }

    if usage || cleanup || report.is_some() {
        audit::track_usage(&mut result)?;
    }

    if cleanup || report.is_some() {
        audit::generate_cleanup(&mut result);
    }

    if let Some(report_path) = &report {
        audit::write_report(&result, report_path)?;
        println!("Report written to {}", report_path.display());
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Skills directory: {}", result.skills_dir.display());
        println!("Total: {}  Healthy: {}  Broken: {}", result.total, result.healthy, result.broken);
        println!();
        for skill in &result.skills {
            let status = if skill.issues.is_empty() { "✓" } else { "✗" };
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
                    audit::DiffKind::Missing => "missing (expected but not installed)",
                    audit::DiffKind::Orphan => "orphan (installed but not in manifest)",
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
        if let Some(cleanup_list) = &result.cleanup {
            if !cleanup_list.is_empty() {
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
    }
    Ok(())
}

fn cmd_summarize(
    symbol: Option<String>,
    file: Option<String>,
    extract: Option<PathBuf>,
    diff: bool,
    stats: bool,
    path: &std::path::Path,
    json_output: bool,
) -> Result<()> {
    let root = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let db_path = root.join(".tsift/summaries.db");

    // --extract mode: run LLM extraction
    if let Some(extract_path) = extract {
        let cfg = load_summarize_config(&root);
        let symbols_db = find_symbols_db(&root);
        let summary_db = summarize::SummaryDb::open(&db_path)?;

        let files_to_extract = if diff {
            let changed = summarize::git_changed_files(&root)?;
            changed.into_iter()
                .filter(|f| f.starts_with(&extract_path) || extract_path.starts_with(f.parent().unwrap_or(f)))
                .collect::<Vec<_>>()
        } else {
            collect_source_files(&extract_path)?
        };

        if files_to_extract.is_empty() {
            println!("No files to extract.");
            return Ok(());
        }

        let mut report = summarize::ExtractionReport {
            files_processed: 0,
            symbols_extracted: 0,
            tokens_input: 0,
            tokens_output: 0,
            errors: Vec::new(),
        };

        for file_path in &files_to_extract {
            let content = match std::fs::read(file_path) {
                Ok(c) => c,
                Err(e) => {
                    report.errors.push(format!("{}: {}", file_path.display(), e));
                    continue;
                }
            };
            let hash = summarize::content_hash(&content);
            let rel_path = file_path.strip_prefix(&root).unwrap_or(file_path).to_string_lossy().to_string();

            if summary_db.is_current(&rel_path, &hash)? {
                continue; // already extracted for this version
            }

            match summarize::extract_for_file(file_path, symbols_db.as_deref(), &cfg) {
                Ok(summaries) => {
                    summary_db.delete_by_file(&rel_path)?;
                    for mut s in summaries {
                        s.file_path = rel_path.clone();
                        report.symbols_extracted += 1;
                        report.tokens_input += s.tokens_input.unwrap_or(0);
                        report.tokens_output += s.tokens_output.unwrap_or(0);
                        summary_db.insert(&s)?;
                    }
                    report.files_processed += 1;
                    if !json_output {
                        println!("  extracted: {}", rel_path);
                    }
                }
                Err(e) => {
                    report.errors.push(format!("{}: {}", rel_path, e));
                    if !json_output {
                        eprintln!("  error: {}: {}", rel_path, e);
                    }
                }
            }
        }

        if json_output {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("\nExtraction complete:");
            println!("  files: {}", report.files_processed);
            println!("  symbols: {}", report.symbols_extracted);
            println!("  tokens: {} in / {} out", report.tokens_input, report.tokens_output);
            if !report.errors.is_empty() {
                println!("  errors: {}", report.errors.len());
            }
        }
        return Ok(());
    }

    // --stats mode
    if stats {
        let summary_db = summarize::SummaryDb::open(&db_path)?;
        let s = summary_db.stats()?;
        if json_output {
            println!("{}", serde_json::to_string_pretty(&s)?);
        } else {
            println!("Summary cache statistics:");
            println!("  summaries:       {}", s.total_summaries);
            println!("  files:           {}", s.total_files);
            println!("  tokens input:    {}", s.total_tokens_input);
            println!("  tokens output:   {}", s.total_tokens_output);
            println!("  est. savings:    {} tokens", s.estimated_tokens_saved);
        }
        return Ok(());
    }

    // Query mode: --file or positional symbol
    if !db_path.exists() {
        bail!("no summaries.db found — run `tsift summarize --extract <path>` first");
    }
    let summary_db = summarize::SummaryDb::open(&db_path)?;

    if let Some(file_query) = file {
        let results = summary_db.get_by_file(&file_query)?;
        if results.is_empty() {
            println!("No cached summary for file: {}", file_query);
            println!("Run: tsift summarize --extract <path>");
            return Ok(());
        }
        if json_output {
            println!("{}", serde_json::to_string_pretty(&results)?);
        } else {
            for s in &results {
                println!("[{}] {}", s.symbol_name, s.summary);
                if let Some(ref labels) = s.concept_labels {
                    if !labels.is_empty() {
                        println!("  concepts: {}", labels.join(", "));
                    }
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
            println!("{}", serde_json::to_string_pretty(&results)?);
        } else {
            for s in &results {
                println!("{} ({})", s.symbol_name, s.file_path);
                println!("  {}", s.summary);
                if let Some(ref entities) = s.entities {
                    if !entities.is_empty() {
                        println!("  entities:");
                        for e in entities {
                            println!("    {} ({}): {}", e.name, e.kind, e.description);
                        }
                    }
                }
                if let Some(ref rels) = s.relationships {
                    if !rels.is_empty() {
                        println!("  relationships:");
                        for r in rels {
                            println!("    {} --{}-> {}", r.from, r.kind, r.to);
                        }
                    }
                }
                if let Some(ref labels) = s.concept_labels {
                    if !labels.is_empty() {
                        println!("  concepts: {}", labels.join(", "));
                    }
                }
                println!();
            }
        }
        return Ok(());
    }

    bail!("specify a symbol, --file, --extract, or --stats");
}

fn cmd_status(path: &std::path::Path, json_output: bool) -> Result<()> {
    let root = path.canonicalize()
        .with_context(|| format!("resolving path: {}", path.display()))?;
    let report = status::check_status(&root)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", status::format_human(&report));
    }
    Ok(())
}

fn load_summarize_config(root: &std::path::Path) -> summarize::SummarizeConfig {
    let config_path = root.join(".tsift/config.toml");
    if !config_path.exists() {
        return summarize::SummarizeConfig::default();
    }
    #[derive(serde::Deserialize, Default)]
    struct RawConfig {
        #[serde(default)]
        summarize: Option<RawSummarize>,
    }
    #[derive(serde::Deserialize)]
    struct RawSummarize {
        model: Option<String>,
        max_file_tokens: Option<usize>,
        api_key_env: Option<String>,
    }
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let raw: RawConfig = toml::from_str(&content).unwrap_or_default();
    let defaults = summarize::SummarizeConfig::default();
    match raw.summarize {
        Some(s) => summarize::SummarizeConfig {
            model: s.model.unwrap_or(defaults.model),
            max_file_tokens: s.max_file_tokens.unwrap_or(defaults.max_file_tokens),
            api_key_env: s.api_key_env.unwrap_or(defaults.api_key_env),
        },
        None => defaults,
    }
}

fn find_symbols_db(root: &std::path::Path) -> Option<PathBuf> {
    let single = root.join(".tsift/index.db");
    if single.exists() {
        return Some(single);
    }
    let indexes = root.join(".tsift/indexes");
    if indexes.exists() {
        if let Ok(entries) = std::fs::read_dir(&indexes) {
            for entry in entries.flatten() {
                let db = entry.path().join("index.db");
                if db.exists() {
                    return Some(db);
                }
            }
        }
    }
    None
}

fn collect_source_files(path: &std::path::Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if path.is_file() {
        files.push(path.to_path_buf());
        return Ok(files);
    }
    let walker = ignore::WalkBuilder::new(path)
        .hidden(true)
        .git_ignore(true)
        .build();
    for entry in walker {
        let entry = entry?;
        if entry.file_type().is_some_and(|ft| ft.is_file()) {
            let p = entry.path();
            if let Some(ext) = p.extension() {
                let ext = ext.to_string_lossy();
                if matches!(ext.as_ref(), "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "kt" | "kts" | "zig" | "sh" | "bash" | "zsh") {
                    files.push(p.to_path_buf());
                }
            }
        }
    }
    Ok(files)
}

fn cmd_init(path: &std::path::Path) -> Result<()> {
    let resolved = init::resolve_project_dir(path)?;
    if resolved != path {
        println!("resolved: {} → {}", path.display(), resolved.display());
    }
    let result = init::init(&resolved)?;
    println!("{}: {} ({})", result.file.display(), result.action,
        match result.action {
            init::InitAction::Created => "tsift Code Navigation section added",
            init::InitAction::Updated => "tsift Code Navigation section updated to latest",
            init::InitAction::AlreadyPresent => "no changes needed",
        });
    if result.gitignore_added {
        println!(".gitignore: added .tsift/");
    }
    Ok(())
}

fn cmd_lint(file: &str, index: Option<PathBuf>, entities_from: Vec<PathBuf>, json_output: bool) -> Result<()> {
    use std::collections::HashSet;

    let file_path = std::path::Path::new(file);
    if !file_path.exists() {
        anyhow::bail!("file not found: {}", file);
    }

    let mut entities = HashSet::new();

    if let Some(index_dir) = index {
        let db_path = index_dir.join("symbols.db");
        if db_path.exists() {
            entities.extend(lint::collect_entities_from_db(&db_path)?);
        }
    } else {
        let default_db = std::path::Path::new(".tsift/indexes");
        if default_db.exists() {
            for entry in std::fs::read_dir(default_db)? {
                let entry = entry?;
                let db = entry.path().join("symbols.db");
                if db.exists() {
                    entities.extend(lint::collect_entities_from_db(&db)?);
                }
            }
        }
    }

    for md_path in &entities_from {
        entities.extend(lint::collect_entities_from_markdown(md_path)?);
    }

    entities.extend(lint::collect_entities_from_markdown(file_path)?);

    let result = lint::lint_markdown(file_path, &entities)?;

    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- classify_task ---

    #[test]
    fn route_search_defaults_to_haiku() {
        let (tier, model) = classify_task("find all uses of authenticate");
        assert_eq!(tier, "haiku");
        assert!(model.contains("haiku"), "expected haiku model, got {}", model);
    }

    #[test]
    fn route_edit_keywords_to_sonnet() {
        for kw in &["edit the file", "fix the bug", "update the config", "remove dead code", "create a new module"] {
            let (tier, _) = classify_task(kw);
            assert_eq!(tier, "sonnet", "expected sonnet for {:?}", kw);
        }
    }

    #[test]
    fn route_architecture_keywords_to_opus() {
        for kw in &["design the API", "architecture review", "plan the migration", "analyze the system", "evaluate trade-offs"] {
            let (tier, _) = classify_task(kw);
            assert_eq!(tier, "opus", "expected opus for {:?}", kw);
        }
    }

    #[test]
    fn route_architecture_beats_edit() {
        // "design and implement" — architecture signal wins (checked first)
        let (tier, _) = classify_task("design and implement the new auth service");
        assert_eq!(tier, "opus");
    }

    // --- apply_edit_op ---

    fn make_op(old: &str, new: &str, replace_all: bool) -> EditOp {
        EditOp {
            file: PathBuf::from("dummy.txt"),
            old: old.to_string(),
            new: new.to_string(),
            replace_all,
        }
    }

    #[test]
    fn edit_replaces_single_occurrence() {
        let content = "hello world";
        let op = make_op("world", "rust", false);
        let (result, count) = apply_edit_op(content, &op).unwrap();
        assert_eq!(result, "hello rust");
        assert_eq!(count, 1);
    }

    #[test]
    fn edit_replace_all_replaces_every_occurrence() {
        let content = "foo foo foo";
        let op = make_op("foo", "bar", true);
        let (result, count) = apply_edit_op(content, &op).unwrap();
        assert_eq!(result, "bar bar bar");
        assert_eq!(count, 3);
    }

    #[test]
    fn edit_fails_when_old_not_found() {
        let content = "hello world";
        let op = make_op("missing", "x", false);
        assert!(apply_edit_op(content, &op).is_err());
    }

    #[test]
    fn edit_fails_when_ambiguous_without_replace_all() {
        let content = "foo foo";
        let op = make_op("foo", "bar", false);
        let err = apply_edit_op(content, &op).unwrap_err();
        assert!(err.to_string().contains("2 times"), "got: {}", err);
    }

    #[test]
    fn edit_fails_when_old_equals_new() {
        let content = "hello";
        let op = make_op("hello", "hello", false);
        assert!(apply_edit_op(content, &op).is_err());
    }

    // --- SQL introspection ---

    fn setup_test_db() -> (tempfile::NamedTempFile, Connection) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT);
             INSERT INTO users VALUES (1, 'Alice', 'alice@example.com');
             INSERT INTO users VALUES (2, 'Bob', NULL);
             CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, title TEXT NOT NULL, body TEXT,
                 FOREIGN KEY(user_id) REFERENCES users(id));
             INSERT INTO posts VALUES (1, 1, 'Hello World', 'First post');
             INSERT INTO posts VALUES (2, 1, 'Second', NULL);
             INSERT INTO posts VALUES (3, 2, 'Bob post', 'Content here');"
        ).unwrap();
        (tmp, conn)
    }

    // --- rewrite_command ---

    #[test]
    fn rewrite_rg_simple_pattern() {
        let result = rewrite_command("rg authenticate");
        assert_eq!(result, Some("tsift search \"authenticate\" --strategy lexical".to_string()));
    }

    #[test]
    fn rewrite_rg_with_path() {
        let result = rewrite_command("rg authenticate src/");
        assert_eq!(result, Some("tsift search \"authenticate\" --strategy lexical --path \"src/\"".to_string()));
    }

    #[test]
    fn rewrite_rg_with_flags_ignored() {
        let result = rewrite_command("rg -i authenticate src/");
        assert_eq!(result, Some("tsift search \"authenticate\" --strategy lexical --path \"src/\"".to_string()));
    }

    #[test]
    fn rewrite_rg_with_type_flag() {
        // -t rs takes a value, should be skipped; pattern is next positional
        let result = rewrite_command("rg -t rs authenticate");
        assert_eq!(result, Some("tsift search \"authenticate\" --strategy lexical".to_string()));
    }

    #[test]
    fn rewrite_rg_pipe_passthrough() {
        // Pipe chains can't be translated — pass through
        let result = rewrite_command("rg authenticate | head -5");
        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_grep_recursive() {
        let result = rewrite_command("grep -r authenticate src/");
        assert_eq!(result, Some("tsift search \"authenticate\" --strategy lexical --path \"src/\"".to_string()));
    }

    #[test]
    fn rewrite_grep_non_recursive_passthrough() {
        let result = rewrite_command("grep authenticate file.txt");
        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_tsift_passthrough() {
        let result = rewrite_command("tsift search \"foo\"");
        assert_eq!(result, Some("tsift search \"foo\"".to_string()));
    }

    #[test]
    fn rewrite_unrelated_passthrough() {
        let result = rewrite_command("cargo build");
        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_rg_quoted_pattern() {
        let result = rewrite_command("rg \"fn main\"");
        assert_eq!(result, Some("tsift search \"fn main\" --strategy lexical".to_string()));
    }

    #[test]
    fn sql_schema_overview_lists_tables() {
        let (_tmp, conn) = setup_test_db();
        let tables = schema_overview(&conn).unwrap();
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, &["posts", "users"]);
    }

    #[test]
    fn sql_schema_overview_row_counts() {
        let (_tmp, conn) = setup_test_db();
        let tables = schema_overview(&conn).unwrap();
        let users = tables.iter().find(|t| t.name == "users").unwrap();
        let posts = tables.iter().find(|t| t.name == "posts").unwrap();
        assert_eq!(users.row_count, 2);
        assert_eq!(posts.row_count, 3);
    }

    #[test]
    fn sql_table_columns_metadata() {
        let (_tmp, conn) = setup_test_db();
        let cols = table_columns(&conn, "users").unwrap();
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].name, "id");
        assert!(cols[0].pk);
        assert_eq!(cols[1].name, "name");
        assert!(cols[1].notnull);
        assert_eq!(cols[2].name, "email");
        assert!(!cols[2].notnull);
    }

    #[test]
    fn sql_execute_query_returns_rows() {
        let (_tmp, conn) = setup_test_db();
        let (columns, rows) = execute_query(&conn, "SELECT name, email FROM users ORDER BY id").unwrap();
        assert_eq!(columns, &["name", "email"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], serde_json::json!("Alice"));
        assert_eq!(rows[0][1], serde_json::json!("alice@example.com"));
        assert_eq!(rows[1][1], serde_json::Value::Null);
    }

    #[test]
    fn sql_execute_query_aggregate() {
        let (_tmp, conn) = setup_test_db();
        let (columns, rows) = execute_query(&conn, "SELECT COUNT(*) as cnt FROM posts").unwrap();
        assert_eq!(columns, &["cnt"]);
        assert_eq!(rows[0][0], serde_json::json!(3));
    }

    #[test]
    fn sql_execute_query_join() {
        let (_tmp, conn) = setup_test_db();
        let (_cols, rows) = execute_query(
            &conn,
            "SELECT u.name, p.title FROM users u JOIN posts p ON u.id = p.user_id ORDER BY p.id"
        ).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], serde_json::json!("Alice"));
        assert_eq!(rows[2][0], serde_json::json!("Bob"));
    }

    #[test]
    fn sql_open_db_read_only() {
        let (tmp, _conn) = setup_test_db();
        drop(_conn);
        let ro_conn = open_db(tmp.path()).unwrap();
        let result = ro_conn.execute("INSERT INTO users VALUES (99, 'Fail', NULL)", []);
        assert!(result.is_err(), "read-only connection should reject writes");
    }

    #[test]
    fn sql_empty_table_schema() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch("CREATE TABLE empty_tbl (id INTEGER PRIMARY KEY, data BLOB)").unwrap();
        let tables = schema_overview(&conn).unwrap();
        assert_eq!(tables[0].row_count, 0);
        assert_eq!(tables[0].columns.len(), 2);
    }

    // --- graph command ---

    fn setup_graph_index() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "fn helper() { println!(\"hi\"); }\nfn main() { helper(); Vec::new(); }"
        ).unwrap();
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();
        dir
    }

    #[test]
    fn graph_callers_query() {
        let dir = setup_graph_index();
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        let callers = db.callers_of("helper").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].caller_name, "main");
    }

    #[test]
    fn graph_callees_query() {
        let dir = setup_graph_index();
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        let callees = db.callees_of("main").unwrap();
        let names: Vec<&str> = callees.iter().map(|e| e.callee_name.as_str()).collect();
        assert!(names.contains(&"helper"));
        assert!(names.contains(&"new"));
    }

    #[test]
    fn graph_no_callers_returns_empty() {
        let dir = setup_graph_index();
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        let callers = db.callers_of("nonexistent").unwrap();
        assert!(callers.is_empty());
    }

    #[test]
    fn graph_cmd_no_index_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = cmd_graph("main", dir.path(), false, false, None, false);
        assert!(result.is_err());
    }

    // --- workspace indexing ---

    fn setup_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitmodules"), r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#).unwrap();
        let alpha = root.join("src/alpha");
        let beta = root.join("src/beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        std::fs::write(alpha.join("lib.rs"), "fn alpha_helper() {}\nfn alpha_main() { alpha_helper(); }").unwrap();
        std::fs::write(beta.join("lib.rs"), "fn beta_func() {}").unwrap();
        dir
    }

    #[test]
    fn workspace_index_creates_per_submodule_dbs() {
        let dir = setup_workspace();
        cmd_index(dir.path(), false, false, false, false, true, None, false).unwrap();
        assert!(dir.path().join(".tsift/indexes/alpha/index.db").exists());
        assert!(dir.path().join(".tsift/indexes/beta/index.db").exists());
    }

    #[test]
    fn workspace_index_single_submodule() {
        let dir = setup_workspace();
        cmd_index(dir.path(), false, false, false, false, false, Some("alpha"), false).unwrap();
        assert!(dir.path().join(".tsift/indexes/alpha/index.db").exists());
        assert!(!dir.path().join(".tsift/indexes/beta/index.db").exists());
    }

    #[test]
    fn federated_search_across_submodules() {
        let dir = setup_workspace();
        cmd_index(dir.path(), false, false, false, false, true, None, false).unwrap();
        let hits = federated_symbol_search(dir.path(), "alpha_helper", 10).unwrap();
        assert!(!hits.is_empty(), "should find alpha_helper via federated search");
    }

    #[test]
    fn federated_search_respects_isolation() {
        let dir = setup_workspace();
        let tsift_dir = dir.path().join(".tsift");
        std::fs::create_dir_all(&tsift_dir).unwrap();
        std::fs::write(tsift_dir.join("config.toml"), r#"
[overrides.alpha]
tier = "isolated"
"#).unwrap();
        cmd_index(dir.path(), false, false, false, false, true, None, false).unwrap();
        let hits = federated_symbol_search(dir.path(), "alpha_helper", 10).unwrap();
        assert!(hits.is_empty(), "isolated submodule should not appear in federated search");
    }

    #[test]
    fn scoped_search_finds_submodule_symbols() {
        let dir = setup_workspace();
        cmd_index(dir.path(), false, false, false, false, true, None, false).unwrap();
        let cfg = config::Config::load(dir.path()).unwrap();
        let db_path = cfg.db_path_for(dir.path(), "alpha");
        let db = index::IndexDb::open(&db_path).unwrap();
        let hits = db.symbol_search("alpha_main", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "alpha_main");
    }

    #[test]
    fn scoped_graph_query() {
        let dir = setup_workspace();
        cmd_index(dir.path(), false, false, false, false, true, None, false).unwrap();
        let cfg = config::Config::load(dir.path()).unwrap();
        let db_path = cfg.db_path_for(dir.path(), "alpha");
        let db = index::IndexDb::open(&db_path).unwrap();
        let callees = db.callees_of("alpha_main").unwrap();
        let names: Vec<&str> = callees.iter().map(|e| e.callee_name.as_str()).collect();
        assert!(names.contains(&"alpha_helper"));
    }

    // --- community detection ---

    #[test]
    fn community_detection_groups_related() {
        let dir = setup_graph_index();
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        let edges = db.all_edges().unwrap();
        let result = graph::detect_communities(&edges);
        assert!(result.node_count > 0);
        assert!(!result.communities.is_empty());
    }

    #[test]
    fn community_cmd_no_index_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = cmd_communities(dir.path(), None, 2, false);
        assert!(result.is_err());
    }

    // --- path ---

    #[test]
    fn path_finds_connected_symbols() {
        let dir = setup_graph_index();
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        let edges = db.all_edges().unwrap();
        let result = graph::shortest_path(&edges, "main", "helper");
        assert!(result.is_some());
        let path = result.unwrap();
        assert_eq!(path.hops, 1);
    }

    #[test]
    fn path_returns_none_for_unknown() {
        let dir = setup_graph_index();
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        let edges = db.all_edges().unwrap();
        assert!(graph::shortest_path(&edges, "main", "nonexistent").is_none());
    }

    #[test]
    fn path_cmd_no_index_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = cmd_path("a", "b", dir.path(), None, false);
        assert!(result.is_err());
    }

    // --- explain ---

    #[test]
    fn explain_shows_symbol_info() {
        let dir = setup_graph_index();
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        let symbols = db.symbol_info("main").unwrap();
        assert!(!symbols.is_empty());
        assert_eq!(symbols[0].name, "main");
        assert_eq!(symbols[0].kind, "function");
    }

    #[test]
    fn explain_cmd_no_index_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = cmd_explain("main", dir.path(), None, false);
        assert!(result.is_err());
    }
}

// --- SQL introspection ---

#[derive(Serialize)]
struct TableInfo {
    name: String,
    columns: Vec<ColumnInfo>,
    row_count: i64,
}

#[derive(Serialize)]
struct ColumnInfo {
    name: String,
    #[serde(rename = "type")]
    col_type: String,
    notnull: bool,
    pk: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_value: Option<String>,
}

/// Open a SQLite connection (read-only).
pub(crate) fn open_db(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening database: {}", path.display()))?;
    Ok(conn)
}

/// List all user tables with column metadata and row counts.
pub(crate) fn schema_overview(conn: &Connection) -> Result<Vec<TableInfo>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let table_names: Vec<String> = stmt.query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut tables = Vec::new();
    for tbl in table_names {
        let columns = table_columns(conn, &tbl)?;
        let row_count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM \"{}\"", tbl),
            [],
            |row| row.get(0),
        )?;
        tables.push(TableInfo { name: tbl, columns, row_count });
    }
    Ok(tables)
}

/// Get column metadata for a single table.
pub(crate) fn table_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{}\")", table))?;
    let cols = stmt.query_map([], |row| {
        Ok(ColumnInfo {
            name: row.get(1)?,
            col_type: row.get::<_, String>(2).unwrap_or_default(),
            notnull: row.get::<_, bool>(3).unwrap_or(false),
            pk: row.get::<_, i32>(5).unwrap_or(0) > 0,
            default_value: row.get(4)?,
        })
    })?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

/// Execute an arbitrary SQL query and return rows as JSON values.
pub(crate) fn execute_query(conn: &Connection, sql: &str) -> Result<(Vec<String>, Vec<Vec<serde_json::Value>>)> {
    let mut stmt = conn.prepare(sql).context("preparing SQL query")?;
    let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let col_count = col_names.len();

    let mut rows = Vec::new();
    let mut query_rows = stmt.query([])?;
    while let Some(row) = query_rows.next()? {
        let mut vals = Vec::with_capacity(col_count);
        for i in 0..col_count {
            let val = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                rusqlite::types::ValueRef::Integer(n) => serde_json::json!(n),
                rusqlite::types::ValueRef::Real(f) => serde_json::json!(f),
                rusqlite::types::ValueRef::Text(s) => {
                    serde_json::Value::String(String::from_utf8_lossy(s).into_owned())
                }
                rusqlite::types::ValueRef::Blob(b) => {
                    serde_json::Value::String(format!("<blob {} bytes>", b.len()))
                }
            };
            vals.push(val);
        }
        rows.push(vals);
    }
    Ok((col_names, rows))
}

fn cmd_sql(db_path: &std::path::Path, query: Option<String>, table: Option<String>, json_output: bool) -> Result<()> {
    let conn = open_db(db_path)?;

    match (query, table) {
        (Some(sql), _) => {
            let (columns, rows) = execute_query(&conn, &sql)?;
            if json_output {
                let json_rows: Vec<serde_json::Value> = rows.iter().map(|row| {
                    let obj: serde_json::Map<String, serde_json::Value> = columns.iter()
                        .zip(row.iter())
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    serde_json::Value::Object(obj)
                }).collect();
                println!("{}", serde_json::to_string_pretty(&json_rows)?);
            } else {
                // Tabular output
                if columns.is_empty() {
                    println!("Query returned no columns.");
                    return Ok(());
                }
                // Header
                println!("{}", columns.join(" | "));
                println!("{}", columns.iter().map(|c| "-".repeat(c.len().max(4))).collect::<Vec<_>>().join("-+-"));
                for row in &rows {
                    let cells: Vec<String> = row.iter().map(|v| match v {
                        serde_json::Value::Null => "NULL".to_string(),
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    }).collect();
                    println!("{}", cells.join(" | "));
                }
                println!("\n{} row(s)", rows.len());
            }
        }
        (None, Some(tbl)) => {
            let cols = table_columns(&conn, &tbl)?;
            if cols.is_empty() {
                bail!("table '{}' not found or has no columns", tbl);
            }
            if json_output {
                println!("{}", serde_json::to_string_pretty(&cols)?);
            } else {
                println!("Table: {}", tbl);
                println!("{:<20} {:<12} {:<8} PK", "Column", "Type", "NotNull");
                println!("{}", "-".repeat(50));
                for col in &cols {
                    println!("{:<20} {:<12} {:<8} {}", col.name, col.col_type, col.notnull, if col.pk { "PK" } else { "" });
                }
            }
        }
        (None, None) => {
            let tables = schema_overview(&conn)?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&tables)?);
            } else {
                println!("Database: {}", db_path.display());
                println!("{} table(s)\n", tables.len());
                for tbl in &tables {
                    println!("  {} ({} rows)", tbl.name, tbl.row_count);
                    for col in &tbl.columns {
                        let flags = [
                            if col.pk { "PK" } else { "" },
                            if col.notnull { "NOT NULL" } else { "" },
                        ].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(", ");
                        let suffix = if flags.is_empty() { String::new() } else { format!(" [{}]", flags) };
                        println!("    {} {}{}", col.name, col.col_type, suffix);
                    }
                    println!();
                }
            }
        }
    }
    Ok(())
}

// --- Command rewriting for Claude Code hooks ---

/// Exit codes for `tsift rewrite` (matches rtk protocol):
///   0 + stdout → rewrite found, auto-allow
///   1          → no tsift equivalent, pass through
fn cmd_rewrite(command: &str) -> Result<()> {
    match rewrite_command(command) {
        Some(rewritten) => {
            print!("{}", rewritten);
            Ok(())
        }
        None => {
            std::process::exit(1);
        }
    }
}

/// Attempt to rewrite a shell command to use tsift.
/// Returns Some(rewritten) if applicable, None if no match.
pub(crate) fn rewrite_command(command: &str) -> Option<String> {
    let trimmed = command.trim();

    // Already a tsift command — pass through (exit 0, identical)
    if trimmed.starts_with("tsift ") || trimmed == "tsift" {
        return Some(command.to_string());
    }

    // rg <pattern> [path] [flags] → tsift search "<pattern>" --strategy lexical [--path <path>]
    if let Some(rewritten) = rewrite_rg(trimmed) {
        return Some(rewritten);
    }

    // grep -r <pattern> [path] → tsift search "<pattern>" --strategy lexical [--path <path>]
    if let Some(rewritten) = rewrite_grep(trimmed) {
        return Some(rewritten);
    }

    None
}

/// Rewrite `rg` (ripgrep) commands to tsift search.
fn rewrite_rg(cmd: &str) -> Option<String> {
    let parts: Vec<&str> = shell_split(cmd);
    if parts.is_empty() || parts[0] != "rg" {
        return None;
    }

    // Skip if rg is used with complex flags we can't translate
    // (pipe chains, output redirection, --replace, --count, etc.)
    if cmd.contains('|') || cmd.contains('>') || cmd.contains("--replace")
        || cmd.contains("--count") || cmd.contains("-c")
        || cmd.contains("--files-with-matches") || cmd.contains("-l")
    {
        return None;
    }

    // Extract the pattern (first non-flag argument after rg)
    let mut pattern = None;
    let mut path = None;
    let mut skip_next = false;

    for part in &parts[1..] {
        if skip_next {
            skip_next = false;
            continue;
        }
        // Flags that take a value
        if matches!(*part, "-t" | "--type" | "-g" | "--glob" | "-A" | "-B" | "-C"
            | "--max-count" | "--max-depth" | "-m" | "-e") {
            skip_next = true;
            continue;
        }
        // Skip standalone flags
        if part.starts_with('-') {
            continue;
        }
        // First positional = pattern, second = path
        if pattern.is_none() {
            pattern = Some(*part);
        } else if path.is_none() {
            path = Some(*part);
        }
    }

    let pattern = pattern?;
    let mut result = format!("tsift search {} --strategy lexical", shell_quote(pattern));
    if let Some(p) = path {
        result.push_str(&format!(" --path {}", shell_quote(p)));
    }
    Some(result)
}

/// Rewrite `grep -r` commands to tsift search.
fn rewrite_grep(cmd: &str) -> Option<String> {
    let parts: Vec<&str> = shell_split(cmd);
    if parts.is_empty() || parts[0] != "grep" {
        return None;
    }

    // Only rewrite recursive grep
    let has_recursive = parts.iter().any(|p| *p == "-r" || *p == "-R" || *p == "--recursive"
        || p.contains('r') && p.starts_with('-') && !p.starts_with("--"));
    if !has_recursive {
        return None;
    }

    // Skip pipe chains
    if cmd.contains('|') || cmd.contains('>') {
        return None;
    }

    let mut pattern = None;
    let mut path = None;
    let mut skip_next = false;

    for part in &parts[1..] {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(*part, "--include" | "--exclude" | "--exclude-dir" | "-e") {
            skip_next = true;
            continue;
        }
        if part.starts_with('-') {
            continue;
        }
        if pattern.is_none() {
            pattern = Some(*part);
        } else if path.is_none() {
            path = Some(*part);
        }
    }

    let pattern = pattern?;
    let mut result = format!("tsift search {} --strategy lexical", shell_quote(pattern));
    if let Some(p) = path {
        result.push_str(&format!(" --path {}", shell_quote(p)));
    }
    Some(result)
}

/// Simple shell word splitting (handles single and double quotes).
fn shell_split(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        // Skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // closing quote
            }
        } else {
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        parts.push(&s[start..i]);
    }
    parts
}

/// Quote a string for shell if it contains special characters.
fn shell_quote(s: &str) -> String {
    // Strip existing quotes
    let unquoted = if (s.starts_with('"') && s.ends_with('"'))
        || (s.starts_with('\'') && s.ends_with('\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    };

    if unquoted.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/') {
        format!("\"{}\"", unquoted)
    } else {
        format!("\"{}\"", unquoted.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn federated_symbol_search(root: &std::path::Path, query: &str, limit: usize) -> Result<Vec<index::SymbolHit>> {
    let cfg = config::Config::load(root)?;
    let submodules = config::Config::submodule_dirs(root)?;
    let mut all_hits: Vec<index::SymbolHit> = Vec::new();
    for (name, _) in &submodules {
        if !cfg.federation_for(name) {
            continue;
        }
        let db_path = cfg.db_path_for(root, name);
        if !db_path.exists() {
            continue;
        }
        let db = index::IndexDb::open(&db_path)?;
        let mut hits = db.symbol_search(query, limit)?;
        all_hits.append(&mut hits);
    }
    all_hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    all_hits.truncate(limit);
    Ok(all_hits)
}

fn cmd_search(
    query: String,
    path: Option<PathBuf>,
    limit: usize,
    strategy: Option<String>,
    scope: Option<String>,
    federated: bool,
    json_output: bool,
) -> Result<()> {
    let base_path = path.unwrap_or_else(|| PathBuf::from("."));

    let (symbol_hits, sift_path) = if let Some(ref scope_name) = scope {
        let root = base_path.canonicalize().unwrap_or(base_path.clone());
        let cfg = config::Config::load(&root)?;
        let db_path = cfg.db_path_for(&root, scope_name);
        let hits = if db_path.exists() {
            let db = index::IndexDb::open(&db_path)?;
            db.symbol_search(&query, limit)?
        } else {
            Vec::new()
        };
        let sub_path = config::Config::submodule_dirs(&root)?
            .into_iter()
            .find(|(name, _)| name == scope_name)
            .map(|(_, p)| p)
            .unwrap_or(base_path.clone());
        (hits, sub_path)
    } else if federated {
        let root = base_path.canonicalize().unwrap_or(base_path.clone());
        (federated_symbol_search(&root, &query, limit)?, base_path.clone())
    } else {
        let db_path = base_path.join(".tsift/index.db");
        let hits = if db_path.exists() {
            let db = index::IndexDb::open(&db_path)?;
            db.symbol_search(&query, limit)?
        } else {
            Vec::new()
        };
        (hits, base_path.clone())
    };

    let engine = Sift::builder().build();
    let effective_strategy = strategy.unwrap_or_else(|| "lexical".to_string());
    let options = SearchOptions::default()
        .with_limit(limit)
        .with_strategy(effective_strategy);
    let input = SearchInput::new(&sift_path, &query).with_options(options);
    let response = engine.search(input)?;

    if json_output {
        #[derive(Serialize)]
        struct CombinedResponse<'a> {
            symbols: &'a [index::SymbolHit],
            #[serde(flatten)]
            sift: &'a serde_json::Value,
        }
        let sift_value = serde_json::to_value(&response)?;
        let combined = CombinedResponse { symbols: &symbol_hits, sift: &sift_value };
        println!("{}", serde_json::to_string_pretty(&combined)?);
    } else {
        if !symbol_hits.is_empty() {
            println!("Symbol matches ({}):", symbol_hits.len());
            println!();
            for (i, hit) in symbol_hits.iter().enumerate() {
                println!(
                    "  #{} [{}] {} {} ({}:{}) score: {:.4}",
                    i + 1, hit.match_type, hit.kind, hit.name, hit.file, hit.line, hit.score
                );
            }
            println!();
        }

        println!(
            "Strategy: {} | Indexed: {} | Skipped: {}",
            response.strategy, response.indexed_artifacts, response.skipped_artifacts
        );
        println!();
        for hit in &response.hits {
            println!(
                "  #{} [{:?}] {} (score: {:.4})",
                hit.rank, hit.confidence, hit.path, hit.score
            );
            if !hit.snippet.is_empty() {
                for line in hit.snippet.lines().take(3) {
                    println!("    {}", line);
                }
            }
            println!();
        }
        if symbol_hits.is_empty() && response.hits.is_empty() {
            println!("  No results.");
        }
    }
    Ok(())
}
