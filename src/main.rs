use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sift::{SearchInput, SearchOptions, Sift};
#[cfg(test)]
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

pub mod audit;
pub mod config;
pub mod graph;
pub mod index;
pub mod init;
mod lang;
pub mod lint;
pub mod status;
pub mod summarize;
pub mod walk;

#[derive(Parser)]
#[command(
    name = "tsift",
    version,
    about = "Token-efficient search for Claude Code"
)]
struct Cli {
    /// Reduce human-readable output volume across commands
    #[arg(long, global = true)]
    compact: bool,

    /// Use pretty-printed (indented) JSON instead of compact single-line JSON
    #[arg(long, global = true)]
    pretty: bool,

    /// Use terse JSON with abbreviated field names and inline schema (implies --json)
    #[arg(long, global = true)]
    terse: bool,

    /// Show absolute paths instead of project-relative
    #[arg(long, global = true)]
    absolute: bool,

    /// Output repeated structures as TSV with header row
    #[arg(long, global = true)]
    tabular: bool,

    /// Schema-then-values: headers once, rows as arrays (implies --json)
    #[arg(long, global = true)]
    schema: bool,

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
        /// Explicitly enable autoindexing before search (default behavior; kept for compatibility)
        #[arg(long)]
        autoindex: bool,
        /// Skip the default autoindexing pass and fail fast if an existing index is stale
        #[arg(long, conflicts_with = "autoindex")]
        no_autoindex: bool,
        /// Timeout in seconds for the sift search engine (0 = no timeout)
        #[arg(long, default_value = "30")]
        timeout: u64,
    },
    #[command(hide = true, name = "__search-worker")]
    SearchWorker {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        query: String,
        #[arg(long)]
        limit: usize,
        #[arg(long)]
        strategy: String,
        #[arg(long)]
        output: PathBuf,
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
        /// Conservative full scan for correctness; reserves the --prune surface for a future sound optimization
        #[arg(long)]
        prune: bool,
        /// Summary only — omit per-file change list (implied by --exit-code)
        #[arg(short, long)]
        quiet: bool,
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
        /// Max edges per direction (0 = unlimited)
        #[arg(short, long, default_value = "20")]
        limit: usize,
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
        /// Max communities to display (0 = unlimited)
        #[arg(short, long, default_value = "10")]
        limit: usize,
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
        /// Max callers/callees each (0 = unlimited)
        #[arg(short, long, default_value = "15")]
        limit: usize,
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
    /// Initialize tsift in a project — ensure Code Navigation in AGENTS.md and CLAUDE.md
    Init {
        /// Path to the project directory (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Also inject auto-reindex hook into .codex/hooks.json
        #[arg(long)]
        codex: bool,
        /// Resolve to the workspace root and install a workspace-wide hook
        #[arg(long)]
        workspace: bool,
    },
    /// Cached LLM analysis — pre-computed summaries, entities, relationships
    Summarize {
        /// Symbol name to look up
        symbol: Option<String>,
        /// Show cached summary for a file/module
        #[arg(long)]
        file: Option<String>,
        /// Run LLM extraction on the given path (relative paths resolve against --path)
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
    /// Diagnose tsift writer-lock and rollback-journal state for an index
    Locks {
        /// Path to the codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Inspect a specific submodule index
        #[arg(long)]
        scope: Option<String>,
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
    Skipped,
}

struct PlannedEdit {
    index: usize,
    file: PathBuf,
    new_content: String,
    replacements: usize,
}

struct StagedEdit {
    index: usize,
    file: PathBuf,
    replacements: usize,
    staged_file: NamedTempFile,
}

struct AppliedEdit {
    index: usize,
    file: PathBuf,
    replacements: usize,
    backup_path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let compact = cli.compact;
    let pretty = cli.pretty;
    let terse = cli.terse;
    let absolute = cli.absolute;
    let tabular = cli.tabular;
    let schema = cli.schema;
    match cli.command {
        Some(Commands::Search {
            query,
            path,
            limit,
            strategy,
            scope,
            federated,
            json,
            autoindex,
            no_autoindex,
            timeout,
        }) => cmd_search(
            query,
            path,
            limit,
            strategy,
            scope,
            federated,
            json || terse || schema,
            autoindex || !no_autoindex,
            timeout,
            compact,
            pretty,
            terse,
            absolute,
            tabular,
            schema,
        ),
        Some(Commands::SearchWorker {
            path,
            query,
            limit,
            strategy,
            output,
        }) => cmd_search_worker(&path, &query, limit, &strategy, &output),
        Some(Commands::Edit { dry_run, file }) => {
            cmd_edit(dry_run, file, compact, pretty, terse, schema)
        }
        Some(Commands::Index {
            path,
            rebuild,
            check,
            exit_code,
            prune,
            quiet,
            workspace,
            submodule,
            json,
        }) => cmd_index(
            &path,
            rebuild,
            check,
            exit_code,
            prune,
            quiet,
            workspace,
            submodule.as_deref(),
            json || terse || schema,
            compact,
            pretty,
            terse,
            absolute,
            schema,
        ),
        Some(Commands::Rewrite { command }) => cmd_rewrite(&command),
        Some(Commands::Route { task, id }) => cmd_route(&task, id),
        Some(Commands::Graph {
            symbol,
            path,
            callers,
            callees,
            scope,
            limit,
            json,
        }) => cmd_graph(
            &symbol,
            &path,
            callers,
            callees,
            scope.as_deref(),
            limit,
            json || terse || schema,
            compact,
            pretty,
            terse,
            absolute,
            tabular,
            schema,
        ),
        Some(Commands::Sql {
            db,
            query,
            table,
            json,
        }) => cmd_sql(
            &db,
            query,
            table,
            json || terse || schema,
            compact,
            pretty,
            terse,
            schema,
        ),
        Some(Commands::Communities {
            path,
            scope,
            min_size,
            limit,
            json,
        }) => cmd_communities(
            &path,
            scope.as_deref(),
            min_size,
            limit,
            json || terse || schema,
            compact,
            pretty,
            terse,
            tabular,
            schema,
        ),
        Some(Commands::Path {
            from,
            to,
            path,
            scope,
            json,
        }) => cmd_path(
            &from,
            &to,
            &path,
            scope.as_deref(),
            json || terse || schema,
            compact,
            pretty,
            terse,
            schema,
        ),
        Some(Commands::Explain {
            symbol,
            path,
            scope,
            limit,
            json,
        }) => cmd_explain(
            &symbol,
            &path,
            scope.as_deref(),
            limit,
            json || terse || schema,
            compact,
            pretty,
            terse,
            absolute,
            tabular,
            schema,
        ),
        Some(Commands::Audit {
            skills_dir,
            manifest,
            usage,
            cleanup,
            report,
            json,
        }) => cmd_audit(
            &skills_dir,
            manifest,
            usage,
            cleanup,
            report,
            json || terse || schema,
            compact,
            pretty,
            terse,
            schema,
        ),
        Some(Commands::Init {
            path,
            codex,
            workspace,
        }) => cmd_init(&path, codex, workspace),
        Some(Commands::Lint {
            file,
            index,
            entities_from,
            json,
        }) => cmd_lint(
            &file,
            index,
            entities_from,
            json || terse || schema,
            compact,
            pretty,
            terse,
            schema,
        ),
        Some(Commands::Summarize {
            symbol,
            file,
            extract,
            diff,
            stats,
            path,
            json,
        }) => cmd_summarize(
            symbol,
            file,
            extract,
            diff,
            stats,
            &path,
            json || terse || schema,
            compact,
            pretty,
            terse,
            schema,
        ),
        Some(Commands::Status { path, json }) => cmd_status(
            &path,
            json || terse || schema,
            compact,
            pretty,
            terse,
            schema,
        ),
        Some(Commands::Locks { path, scope, json }) => cmd_locks(
            &path,
            scope.as_deref(),
            json || terse || schema,
            compact,
            pretty,
            terse,
            schema,
        ),
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
    for signal in &[
        "architect",
        "architecture",
        "design",
        "plan",
        "strateg",
        "analy",
        "review",
        "evaluate",
        "assess",
    ] {
        if lower.contains(signal) {
            return ("opus", "claude-opus-4-6");
        }
    }
    // Edit/write signals → sonnet
    for signal in &[
        "edit",
        "write",
        "fix",
        "change",
        "update",
        "create",
        "add ",
        "remove",
        "delete",
        "modify",
        "refactor",
        "implement",
        "build",
    ] {
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

#[cfg(test)]
fn to_json<T: serde::Serialize>(val: &T, pretty: bool, terse: bool) -> anyhow::Result<String> {
    to_json_schema(val, pretty, terse, false)
}

fn to_json_schema<T: serde::Serialize>(
    val: &T,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> anyhow::Result<String> {
    if terse || schema {
        let value = serde_json::to_value(val)?;
        let mut transformed = if terse { terse_transform(value) } else { value };
        if schema {
            transformed = schema_transform(transformed);
        }
        if terse {
            let terse_schema = terse_schema_for(&transformed);
            let wrapped = serde_json::json!({"_s": terse_schema, "d": transformed});
            if pretty {
                Ok(serde_json::to_string_pretty(&wrapped)?)
            } else {
                Ok(serde_json::to_string(&wrapped)?)
            }
        } else if pretty {
            Ok(serde_json::to_string_pretty(&transformed)?)
        } else {
            Ok(serde_json::to_string(&transformed)?)
        }
    } else if pretty {
        Ok(serde_json::to_string_pretty(val)?)
    } else {
        Ok(serde_json::to_string(val)?)
    }
}

fn terse_key(key: &str) -> &str {
    match key {
        "name" => "n",
        "kind" => "k",
        "file" => "f",
        "line" => "l",
        "path" => "p",
        "from" => "fr",
        "type" => "ty",
        "text" => "tx",
        "new" => "nw",
        "run" => "r",
        "use" => "u",
        "score" => "sc",
        "language" => "la",
        "status" => "st",
        "state" => "stt",
        "error" => "err",
        "errors" => "ers",
        "hops" => "hp",
        "tags" => "tg",
        "model" => "ml",
        "skill" => "sk",
        "count" => "ct",
        "total" => "tot",
        "column" => "col",
        "description" => "dsc",
        "end_line" => "el",
        "signature" => "sig",
        "parent_module" => "pm",
        "visibility" => "vis",
        "match_type" => "mt",
        "caller_file" => "cf",
        "caller_name" => "cn",
        "caller_line" => "cl",
        "callee_name" => "en",
        "call_site_line" => "csl",
        "members" => "m",
        "modularity" => "q",
        "modularity_contribution" => "mc",
        "iterations" => "it",
        "node_count" => "nc",
        "edge_count" => "ec",
        "community_count" => "cc",
        "communities" => "cms",
        "community" => "cm",
        "symbol" => "s",
        "symbols" => "sy",
        "definitions" => "df",
        "callers" => "crs",
        "callees" => "ces",
        "total_tracked" => "tt",
        "modified" => "md",
        "deleted" => "dl",
        "unchanged" => "uc",
        "changes" => "ch",
        "prune_stats" => "ps",
        "hits" => "h",
        "rank" => "rk",
        "snippet" => "sn",
        "confidence" => "co",
        "index" => "ix",
        "summaries" => "sms",
        "recommendations" => "rec",
        "total_files" => "tf",
        "stale_files" => "sf",
        "last_indexed_secs_ago" => "age",
        "cached_files" => "caf",
        "total_indexed_files" => "tif",
        "coverage_pct" => "cov",
        "symbol_name" => "syn",
        "file_path" => "fp",
        "content_hash" => "hsh",
        "summary" => "sum",
        "entities" => "ent",
        "relationships" => "rel",
        "concept_labels" => "cls",
        "extracted_at" => "at",
        "tokens_input" => "ti",
        "tokens_output" => "tout",
        "total_summaries" => "ts",
        "stale_count" => "stc",
        "total_tokens_input" => "tti",
        "total_tokens_output" => "tto",
        "estimated_tokens_saved" => "ets",
        "files_processed" => "fps",
        "symbols_extracted" => "se",
        "skills_dir" => "sd",
        "healthy" => "ok",
        "broken" => "brk",
        "skills" => "sks",
        "manifest_diffs" => "mdf",
        "similar_pairs" => "sim",
        "usage" => "usg",
        "cleanup" => "cln",
        "has_skill_md" => "hsm",
        "is_symlink" => "isl",
        "issues" => "iss",
        "invocation_count" => "inv",
        "reasons" => "rsn",
        "token_estimate" => "te",
        "skill_a" => "sa",
        "skill_b" => "sb",
        "desc_a" => "da",
        "desc_b" => "db",
        "annotations" => "ann",
        "entity" => "ety",
        "suggestion" => "sug",
        "columns" => "cols",
        "row_count" => "rc",
        "notnull" => "nn",
        "default_value" => "dv",
        "replace_all" => "ra",
        other => other,
    }
}

fn terse_transform(val: serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (k, v) in map {
                new_map.insert(terse_key(&k).to_string(), terse_transform(v));
            }
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(terse_transform).collect())
        }
        other => other,
    }
}

fn terse_schema_for(val: &serde_json::Value) -> serde_json::Value {
    let mut keys = HashSet::new();
    collect_terse_keys(val, &mut keys);
    let mut schema = serde_json::Map::new();
    for (long, short) in TERSE_PAIRS {
        if keys.contains(*short) {
            schema.insert(
                short.to_string(),
                serde_json::Value::String(long.to_string()),
            );
        }
    }
    serde_json::Value::Object(schema)
}

fn collect_terse_keys(val: &serde_json::Value, keys: &mut HashSet<String>) {
    match val {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                keys.insert(k.clone());
                collect_terse_keys(v, keys);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_terse_keys(v, keys);
            }
        }
        _ => {}
    }
}

fn schema_transform(val: serde_json::Value) -> serde_json::Value {
    match val {
        serde_json::Value::Array(arr) if arr.len() >= 2 => {
            if let Some(cols) = homogeneous_keys(&arr) {
                let rows: Vec<serde_json::Value> = arr
                    .into_iter()
                    .map(|item| {
                        if let serde_json::Value::Object(map) = item {
                            let vals: Vec<serde_json::Value> = cols
                                .iter()
                                .map(|c| map.get(c).cloned().unwrap_or(serde_json::Value::Null))
                                .collect();
                            serde_json::Value::Array(vals)
                        } else {
                            item
                        }
                    })
                    .collect();
                let col_vals: Vec<serde_json::Value> =
                    cols.into_iter().map(serde_json::Value::String).collect();
                serde_json::json!({"_c": col_vals, "_r": rows})
            } else {
                serde_json::Value::Array(arr.into_iter().map(schema_transform).collect())
            }
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(schema_transform).collect())
        }
        serde_json::Value::Object(map) => {
            let new_map: serde_json::Map<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, v)| (k, schema_transform(v)))
                .collect();
            serde_json::Value::Object(new_map)
        }
        other => other,
    }
}

fn homogeneous_keys(arr: &[serde_json::Value]) -> Option<Vec<String>> {
    let first = arr.first()?.as_object()?;
    let keys: Vec<String> = first.keys().cloned().collect();
    for item in &arr[1..] {
        let obj = item.as_object()?;
        if obj.len() != keys.len() {
            return None;
        }
        for k in &keys {
            if !obj.contains_key(k) {
                return None;
            }
        }
    }
    Some(keys)
}

const TERSE_PAIRS: &[(&str, &str)] = &[
    ("name", "n"),
    ("kind", "k"),
    ("file", "f"),
    ("line", "l"),
    ("path", "p"),
    ("from", "fr"),
    ("type", "ty"),
    ("text", "tx"),
    ("new", "nw"),
    ("run", "r"),
    ("use", "u"),
    ("score", "sc"),
    ("language", "la"),
    ("status", "st"),
    ("state", "stt"),
    ("error", "err"),
    ("errors", "ers"),
    ("hops", "hp"),
    ("tags", "tg"),
    ("model", "ml"),
    ("skill", "sk"),
    ("count", "ct"),
    ("total", "tot"),
    ("column", "col"),
    ("description", "dsc"),
    ("end_line", "el"),
    ("signature", "sig"),
    ("parent_module", "pm"),
    ("visibility", "vis"),
    ("match_type", "mt"),
    ("caller_file", "cf"),
    ("caller_name", "cn"),
    ("caller_line", "cl"),
    ("callee_name", "en"),
    ("call_site_line", "csl"),
    ("members", "m"),
    ("modularity", "q"),
    ("modularity_contribution", "mc"),
    ("iterations", "it"),
    ("node_count", "nc"),
    ("edge_count", "ec"),
    ("community_count", "cc"),
    ("communities", "cms"),
    ("community", "cm"),
    ("symbol", "s"),
    ("symbols", "sy"),
    ("definitions", "df"),
    ("callers", "crs"),
    ("callees", "ces"),
    ("total_tracked", "tt"),
    ("modified", "md"),
    ("deleted", "dl"),
    ("unchanged", "uc"),
    ("changes", "ch"),
    ("prune_stats", "ps"),
    ("hits", "h"),
    ("rank", "rk"),
    ("snippet", "sn"),
    ("confidence", "co"),
    ("index", "ix"),
    ("summaries", "sms"),
    ("recommendations", "rec"),
    ("total_files", "tf"),
    ("stale_files", "sf"),
    ("last_indexed_secs_ago", "age"),
    ("cached_files", "caf"),
    ("total_indexed_files", "tif"),
    ("coverage_pct", "cov"),
    ("symbol_name", "syn"),
    ("file_path", "fp"),
    ("content_hash", "hsh"),
    ("summary", "sum"),
    ("entities", "ent"),
    ("relationships", "rel"),
    ("concept_labels", "cls"),
    ("extracted_at", "at"),
    ("tokens_input", "ti"),
    ("tokens_output", "tout"),
    ("total_summaries", "ts"),
    ("stale_count", "stc"),
    ("total_tokens_input", "tti"),
    ("total_tokens_output", "tto"),
    ("estimated_tokens_saved", "ets"),
    ("files_processed", "fps"),
    ("symbols_extracted", "se"),
    ("skills_dir", "sd"),
    ("healthy", "ok"),
    ("broken", "brk"),
    ("skills", "sks"),
    ("manifest_diffs", "mdf"),
    ("similar_pairs", "sim"),
    ("usage", "usg"),
    ("cleanup", "cln"),
    ("has_skill_md", "hsm"),
    ("is_symlink", "isl"),
    ("issues", "iss"),
    ("invocation_count", "inv"),
    ("reasons", "rsn"),
    ("token_estimate", "te"),
    ("skill_a", "sa"),
    ("skill_b", "sb"),
    ("desc_a", "da"),
    ("desc_b", "db"),
    ("annotations", "ann"),
    ("entity", "ety"),
    ("suggestion", "sug"),
    ("columns", "cols"),
    ("row_count", "rc"),
    ("notnull", "nn"),
    ("default_value", "dv"),
    ("replace_all", "ra"),
];

fn relativize(path: &str, root: &std::path::Path) -> String {
    let root_str = root.to_string_lossy();
    let prefix = format!("{}/", root_str.trim_end_matches('/'));
    path.strip_prefix(&prefix).unwrap_or(path).to_string()
}

fn relativize_pathbuf(path: &std::path::Path, root: &std::path::Path) -> PathBuf {
    path.strip_prefix(root)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| path.to_path_buf())
}

fn relativize_edges(edges: &mut [index::StoredEdge], root: &std::path::Path) {
    for edge in edges {
        edge.caller_file = relativize(&edge.caller_file, root);
    }
}

fn relativize_symbols(symbols: &mut [index::StoredSymbol], root: &std::path::Path) {
    for sym in symbols {
        sym.file = relativize(&sym.file, root);
    }
}

fn relativize_symbol_hits(hits: &mut [index::SymbolHit], root: &std::path::Path) {
    for hit in hits {
        hit.file = relativize(&hit.file, root);
    }
}

const JSON_PATH_KEYS: &[&str] = &["file", "path", "caller_file", "file_path"];

fn relativize_json_paths(val: &mut serde_json::Value, root: &std::path::Path) {
    let root_str = root.to_string_lossy();
    let prefix = format!("{}/", root_str.trim_end_matches('/'));
    relativize_json_inner(val, &prefix);
}

fn relativize_json_inner(val: &mut serde_json::Value, prefix: &str) {
    match val {
        serde_json::Value::Array(arr) => {
            for v in arr {
                relativize_json_inner(v, prefix);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if JSON_PATH_KEYS.contains(&k.as_str())
                    && let serde_json::Value::String(s) = v
                    && let Some(rest) = s.strip_prefix(prefix)
                {
                    *s = rest.to_string();
                }
                relativize_json_inner(v, prefix);
            }
        }
        _ => {}
    }
}

fn format_score(score: f64, compact: bool) -> String {
    if compact {
        format!("{score:.2}")
    } else {
        format!("{score:.4}")
    }
}

fn truncate_for_compact(input: &str, max_chars: usize) -> String {
    let trimmed = input.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }
    let prefix: String = trimmed.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{prefix}...")
}

fn compact_snippet(snippet: &str) -> Option<String> {
    snippet
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| truncate_for_compact(line, 100))
}

fn compact_members(members: &[String], limit: usize) -> String {
    if members.len() <= limit {
        return members.join(", ");
    }
    format!(
        "{} (+{} more)",
        members[..limit].join(", "),
        members.len() - limit
    )
}

fn abbreviate_kind(kind: &str) -> &str {
    match kind {
        "function" => "fn",
        "method" => "meth",
        "module" | "mod" => "mod",
        "struct" => "struct",
        "trait" => "trait",
        "impl" => "impl",
        "class" => "cls",
        "interface" => "iface",
        "type_alias" => "type",
        "data_class" => "data_cls",
        "sealed_class" => "sealed_cls",
        "enum_class" => "enum_cls",
        "companion_object" => "comp_obj",
        "object" => "obj",
        "heading" => "h",
        "code_block" => "code",
        "alias" => "alias",
        other => other,
    }
}

fn abbreviate_match_type(mt: &str) -> &str {
    match mt {
        "exact_name" => "exact",
        "all_tags" => "all_tags",
        "partial_tags" => "partial",
        other => other,
    }
}

fn symbol_path_summary(path: &[String]) -> String {
    path.join(" -> ")
}

fn format_edge_groups(edges: &[index::StoredEdge], use_callers: bool) -> Vec<String> {
    let mut grouped: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in edges {
        let key = edge.caller_file.as_str();
        let name = if use_callers {
            edge.caller_name.as_str()
        } else {
            edge.callee_name.as_str()
        };
        let names = grouped.entry(key).or_default();
        if !names.contains(&name) {
            names.push(name);
        }
    }

    grouped
        .into_iter()
        .map(|(file, names)| format!("  {}: {}", file, names.join(", ")))
        .collect()
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
        bail!(
            "old_string matches {} times (use replace_all or provide more context)",
            count
        );
    }
    let replaced = if op.replace_all {
        content.replace(op.old.as_str(), &op.new)
    } else {
        content.replacen(op.old.as_str(), &op.new, 1)
    };
    Ok((replaced, count))
}

fn build_edit_plan(batch: &EditBatch) -> Result<Vec<PlannedEdit>> {
    let mut plan = Vec::with_capacity(batch.edits.len());
    for (i, op) in batch.edits.iter().enumerate() {
        let content = fs::read_to_string(&op.file)
            .with_context(|| format!("edit #{}: reading {}", i + 1, op.file.display()))?;
        let (replaced, count) = apply_edit_op(&content, op)
            .with_context(|| format!("edit #{}: {}", i + 1, op.file.display()))?;
        plan.push(PlannedEdit {
            index: i,
            file: op.file.clone(),
            new_content: replaced,
            replacements: count,
        });
    }
    Ok(plan)
}

fn stage_edit_plan(plan: Vec<PlannedEdit>) -> Result<Vec<StagedEdit>> {
    let mut staged = Vec::with_capacity(plan.len());
    for planned in plan {
        let parent = planned.file.parent().unwrap_or_else(|| Path::new("."));
        let mut staged_file = NamedTempFile::new_in(parent)
            .with_context(|| format!("staging {}", planned.file.display()))?;
        staged_file
            .write_all(planned.new_content.as_bytes())
            .with_context(|| format!("staging {}", planned.file.display()))?;
        staged_file
            .as_file_mut()
            .sync_all()
            .with_context(|| format!("flushing staged edit for {}", planned.file.display()))?;
        staged.push(StagedEdit {
            index: planned.index,
            file: planned.file,
            replacements: planned.replacements,
            staged_file,
        });
    }
    Ok(staged)
}

fn edit_backup_path(file: &Path, index: usize) -> PathBuf {
    let parent = file.parent().unwrap_or_else(|| Path::new("."));
    let name = file
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "edit-target".to_string());
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    parent.join(format!(
        ".{name}.tsift-edit-{stamp}-{}-{index}.bak",
        std::process::id()
    ))
}

fn rollback_applied_edits(applied: &[AppliedEdit]) -> Result<()> {
    let mut rollback_errors = Vec::new();
    for entry in applied.iter().rev() {
        if let Err(err) = fs::remove_file(&entry.file)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            rollback_errors.push(format!(
                "removing {} during rollback: {}",
                entry.file.display(),
                err
            ));
            continue;
        }
        if let Err(err) = fs::rename(&entry.backup_path, &entry.file) {
            rollback_errors.push(format!(
                "restoring {} during rollback: {}",
                entry.file.display(),
                err
            ));
        }
    }
    if rollback_errors.is_empty() {
        Ok(())
    } else {
        bail!(rollback_errors.join("; "));
    }
}

fn cleanup_edit_backups(applied: &[AppliedEdit]) {
    for entry in applied {
        let _ = fs::remove_file(&entry.backup_path);
    }
}

fn ok_results_from_applied(applied: &[AppliedEdit]) -> Vec<EditResult> {
    applied
        .iter()
        .map(|entry| EditResult {
            file: entry.file.clone(),
            status: EditStatus::Ok,
            error: None,
            replacements: Some(entry.replacements),
        })
        .collect()
}

fn apply_edit_plan_atomically(plan: Vec<PlannedEdit>) -> Result<Vec<EditResult>> {
    apply_edit_plan_atomically_inner(plan, |_, _| Ok(()))
}

fn apply_edit_plan_atomically_inner<F>(
    plan: Vec<PlannedEdit>,
    mut before_swap: F,
) -> Result<Vec<EditResult>>
where
    F: FnMut(usize, &Path) -> Result<()>,
{
    let staged = stage_edit_plan(plan)?;
    let mut applied = Vec::with_capacity(staged.len());

    for (commit_index, staged_edit) in staged.into_iter().enumerate() {
        if let Err(err) = before_swap(commit_index, &staged_edit.file) {
            match rollback_applied_edits(&applied) {
                Ok(()) => cleanup_edit_backups(&applied),
                Err(rollback_error) => {
                    return Err(err.context(format!("rollback also failed: {rollback_error}")));
                }
            }
            return Err(err);
        }

        let backup_path = edit_backup_path(&staged_edit.file, staged_edit.index);
        if let Err(err) = fs::rename(&staged_edit.file, &backup_path) {
            match rollback_applied_edits(&applied) {
                Ok(()) => cleanup_edit_backups(&applied),
                Err(rollback_error) => {
                    bail!(
                        "moving {} into backup slot failed: {}; rollback also failed: {}",
                        staged_edit.file.display(),
                        err,
                        rollback_error
                    );
                }
            }
            bail!(
                "moving {} into backup slot failed: {}",
                staged_edit.file.display(),
                err
            );
        }
        match staged_edit.staged_file.persist(&staged_edit.file) {
            Ok(_) => applied.push(AppliedEdit {
                index: staged_edit.index,
                file: staged_edit.file,
                replacements: staged_edit.replacements,
                backup_path,
            }),
            Err(err) => {
                let persist_error = err.error;
                drop(err.file);
                let restore_error = fs::rename(&backup_path, &staged_edit.file).err();
                let rollback_error = rollback_applied_edits(&applied).err();
                if rollback_error.is_none() {
                    cleanup_edit_backups(&applied);
                }
                let mut message = format!(
                    "committing {} failed: {}",
                    staged_edit.file.display(),
                    persist_error
                );
                if let Some(restore_error) = restore_error {
                    message.push_str(&format!(
                        "; restoring original {} failed: {}",
                        staged_edit.file.display(),
                        restore_error
                    ));
                }
                if let Some(rollback_error) = rollback_error {
                    message.push_str(&format!("; rollback also failed: {rollback_error}"));
                }
                bail!(message);
            }
        }
    }

    applied.sort_by_key(|entry| entry.index);
    let results = ok_results_from_applied(&applied);
    cleanup_edit_backups(&applied);
    Ok(results)
}

fn cmd_edit(
    dry_run: bool,
    file: Option<PathBuf>,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
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
    let batch: EditBatch = serde_json::from_str(&input).context("parsing edit JSON")?;

    if batch.edits.is_empty() {
        println!("No edits provided.");
        return Ok(());
    }

    let plan = build_edit_plan(&batch)?;
    let results: Vec<EditResult> = if dry_run {
        plan.iter()
            .map(|entry| EditResult {
                file: entry.file.clone(),
                status: EditStatus::Skipped,
                error: Some("dry run".into()),
                replacements: Some(entry.replacements),
            })
            .collect()
    } else {
        apply_edit_plan_atomically(plan)?
    };

    // Summary output
    let ok_count = results
        .iter()
        .filter(|r| matches!(r.status, EditStatus::Ok))
        .count();
    let skip_count = results
        .iter()
        .filter(|r| matches!(r.status, EditStatus::Skipped))
        .count();
    let err_count = 0usize;

    if compact {
        println!(
            "applied:{} skipped:{} errors:{}",
            ok_count, skip_count, err_count
        );
    } else {
        println!(
            "{}",
            to_json_schema(
                &serde_json::json!({
                    "applied": ok_count,
                    "skipped": skip_count,
                    "errors": err_count,
                    "results": results,
                }),
                pretty,
                terse,
                schema
            )?
        );
    }

    if err_count > 0 {
        bail!("{} edit(s) failed", err_count);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_index(
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

    if workspace || submodule.is_some() {
        let cfg = config::Config::load(&root)?;
        let targets: Vec<(String, PathBuf, Option<config::WorkspaceScope>)> =
            if let Some(name) = submodule {
                let scope = config::Config::resolve_submodule(&root, name)?;
                vec![(scope.id.clone(), scope.source_root.clone(), Some(scope))]
            } else {
                config::Config::submodule_dirs(&root)?
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
                index::IndexDb::inspect_read_only(&db_path, sub_path, prune)?.summary
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

#[allow(clippy::too_many_arguments)]
fn cmd_graph(
    symbol: &str,
    path: &std::path::Path,
    callers: bool,
    callees: bool,
    scope: Option<&str>,
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    absolute: bool,
    tabular: bool,
    schema: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let db = open_index_db(path, scope)?;

    let show_both = !callers && !callees;

    if callers || show_both {
        let mut edges = db.callers_of(symbol)?;
        if !absolute {
            relativize_edges(&mut edges, &root);
        }
        let total = edges.len();
        let truncated = limit > 0 && total > limit;
        if truncated {
            edges.truncate(limit);
        }
        if json_output {
            if !show_both {
                let out = serde_json::json!({
                    "callers": edges,
                    "total": total,
                    "truncated": truncated,
                });
                println!("{}", to_json_schema(&out, pretty, terse, schema)?);
            }
        } else if tabular {
            println!("direction\tname\tfile\tline");
            for edge in &edges {
                println!(
                    "caller\t{}\t{}\t{}",
                    edge.caller_name, edge.caller_file, edge.call_site_line
                );
            }
            if truncated {
                println!("# (+{} more)", total - limit);
            }
        } else if compact {
            println!("crs[{}]:", total);
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
                    println!(
                        "  {} {}:{}",
                        edge.caller_name, edge.caller_file, edge.call_site_line
                    );
                }
                if truncated {
                    println!("  (+{} more)", total - limit);
                }
            }
        } else {
            println!("Callers of `{}`:", symbol);
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
                    println!(
                        "  {} ({}:{})",
                        edge.caller_name, edge.caller_file, edge.call_site_line
                    );
                }
                if truncated {
                    println!("  (+{} more, use --limit 0 to show all)", total - limit);
                }
            }
        }
        if show_both && !json_output && !compact && !tabular {
            println!();
        }
    }

    if callees || show_both {
        let mut edges = db.callees_of(symbol)?;
        if !absolute {
            relativize_edges(&mut edges, &root);
        }
        let total = edges.len();
        let truncated = limit > 0 && total > limit;
        if truncated {
            edges.truncate(limit);
        }
        if json_output {
            if !show_both {
                let out = serde_json::json!({
                    "callees": edges,
                    "total": total,
                    "truncated": truncated,
                });
                println!("{}", to_json_schema(&out, pretty, terse, schema)?);
            }
        } else if tabular {
            if !show_both {
                println!("direction\tname\tfile\tline");
            }
            for edge in &edges {
                println!(
                    "callee\t{}\t{}\t{}",
                    edge.callee_name, edge.caller_file, edge.call_site_line
                );
            }
            if truncated {
                println!("# (+{} more)", total - limit);
            }
        } else if compact {
            println!("ces[{}]:", total);
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
                    println!(
                        "  {} {}:{}",
                        edge.callee_name, edge.caller_file, edge.call_site_line
                    );
                }
                if truncated {
                    println!("  (+{} more)", total - limit);
                }
            }
        } else {
            println!("Callees of `{}`:", symbol);
            if edges.is_empty() {
                println!("  (none)");
            } else {
                for edge in &edges {
                    println!(
                        "  {} ({}:{})",
                        edge.callee_name, edge.caller_file, edge.call_site_line
                    );
                }
                if truncated {
                    println!("  (+{} more, use --limit 0 to show all)", total - limit);
                }
            }
        }
    }

    if show_both && json_output {
        let mut callers_edges = db.callers_of(symbol)?;
        let mut callees_edges = db.callees_of(symbol)?;
        if !absolute {
            relativize_edges(&mut callers_edges, &root);
            relativize_edges(&mut callees_edges, &root);
        }
        let callers_total = callers_edges.len();
        let callees_total = callees_edges.len();
        let callers_truncated = limit > 0 && callers_total > limit;
        let callees_truncated = limit > 0 && callees_total > limit;
        if callers_truncated {
            callers_edges.truncate(limit);
        }
        if callees_truncated {
            callees_edges.truncate(limit);
        }
        let combined = serde_json::json!({
            "symbol": symbol,
            "callers": callers_edges,
            "callers_total": callers_total,
            "callers_truncated": callers_truncated,
            "callees": callees_edges,
            "callees_total": callees_total,
            "callees_truncated": callees_truncated,
        });
        println!("{}", to_json_schema(&combined, pretty, terse, schema)?);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_communities(
    path: &std::path::Path,
    scope: Option<&str>,
    min_size: usize,
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    tabular: bool,
    schema: bool,
) -> Result<()> {
    let db = open_index_db(path, scope)?;
    let edges = db.all_edges()?;
    let result = graph::detect_communities(&edges);

    let filtered: Vec<&graph::Community> = result
        .communities
        .iter()
        .filter(|c| c.members.len() >= min_size)
        .collect();

    let total = filtered.len();
    let truncated = limit > 0 && total > limit;
    let display: Vec<&graph::Community> = if truncated {
        filtered[..limit].to_vec()
    } else {
        filtered
    };

    if json_output {
        let out = serde_json::json!({
            "modularity": result.modularity,
            "iterations": result.iterations,
            "node_count": result.node_count,
            "edge_count": result.edge_count,
            "community_count": total,
            "communities": display,
            "truncated": truncated,
        });
        println!("{}", to_json_schema(&out, pretty, terse, schema)?);
    } else if tabular {
        println!("id\tsize\tmembers");
        for (i, community) in display.iter().enumerate() {
            println!(
                "{}\t{}\t{}",
                i + 1,
                community.members.len(),
                community.members.join(",")
            );
        }
        if truncated {
            println!("# (+{} more)", total - limit);
        }
    } else if compact {
        println!(
            "comms n:{} e:{} iter:{} q:{:.4} cnt:{}",
            result.node_count, result.edge_count, result.iterations, result.modularity, total
        );
        if display.is_empty() {
            println!("  (none >= {})", min_size);
        } else {
            for (i, community) in display.iter().enumerate() {
                println!(
                    "  {}. {} mbrs {}",
                    i + 1,
                    community.members.len(),
                    compact_members(&community.members, 5)
                );
            }
            if truncated {
                println!("  (+{} more)", total - limit);
            }
        }
    } else {
        println!(
            "Communities ({} nodes, {} edges, {} iterations, Q={:.4})",
            result.node_count, result.edge_count, result.iterations, result.modularity
        );
        if display.is_empty() {
            println!("  (no communities with {} or more members)", min_size);
        } else {
            println!();
            for (i, c) in display.iter().enumerate() {
                println!(
                    "  [{}] {} members (Q={:.4}):",
                    i + 1,
                    c.members.len(),
                    c.modularity_contribution
                );
                for m in &c.members {
                    println!("    {}", m);
                }
                if i + 1 < display.len() {
                    println!();
                }
            }
            if truncated {
                println!();
                println!(
                    "  (+{} more communities, use --limit 0 to show all)",
                    total - limit
                );
            }
        }
    }
    Ok(())
}

fn resolve_query_db_path(root: &Path, path_hint: &Path, scope: Option<&str>) -> Result<PathBuf> {
    let cfg = config::Config::load(root)?;
    if let Some(scope_name) = scope {
        let scope = config::Config::resolve_submodule(root, scope_name)?;
        return Ok(cfg.db_path_for(root, &scope.id));
    }

    if let Some(scope) = config::Config::infer_submodule_from_path(root, path_hint)? {
        return Ok(cfg.db_path_for(root, &scope.id));
    }

    let db_path = root.join(".tsift/index.db");
    if db_path.exists() {
        return Ok(db_path);
    }

    let scopes = config::Config::submodule_dirs(root)?;
    if scopes.is_empty() {
        return Ok(db_path);
    }

    let available_scopes = scopes
        .iter()
        .map(|scope| scope.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let indexed_scopes = scopes
        .iter()
        .filter(|scope| cfg.db_path_for(root, &scope.id).exists())
        .map(|scope| scope.id.as_str())
        .collect::<Vec<_>>();
    let indexed_label = if indexed_scopes.is_empty() {
        "none".to_string()
    } else {
        indexed_scopes.join(", ")
    };

    bail!(
        "workspace root {} has no shared root index at {}. Read-only graph queries require `--scope <scope>` when the workspace is indexed into `.tsift/indexes/*/index.db`. Available scopes: {}. Indexed scopes: {}.",
        root.display(),
        db_path.display(),
        available_scopes,
        indexed_label
    );
}

fn open_index_db(path: &std::path::Path, scope: Option<&str>) -> Result<index::IndexDb> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let db_path = resolve_query_db_path(&root, path, scope)?;
    if !db_path.exists() {
        bail!(
            "no index found at {}. Run `tsift index` first.",
            db_path.display()
        );
    }
    index::IndexDb::open_read_only_resilient(&db_path)
}

#[allow(clippy::too_many_arguments)]
fn cmd_path(
    from: &str,
    to: &str,
    path: &std::path::Path,
    scope: Option<&str>,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let db = open_index_db(path, scope)?;
    let edges = db.all_edges()?;
    match graph::shortest_path(&edges, from, to) {
        Some(result) => {
            if json_output {
                println!("{}", to_json_schema(&result, pretty, terse, schema)?);
            } else if compact {
                println!(
                    "{} ({} hop{})",
                    symbol_path_summary(&result.path),
                    result.hops,
                    if result.hops == 1 { "" } else { "s" }
                );
            } else {
                println!(
                    "{} → {} ({} hop{})",
                    result.from,
                    result.to,
                    result.hops,
                    if result.hops == 1 { "" } else { "s" }
                );
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
                println!(
                    "{}",
                    to_json_schema(
                        &serde_json::json!({
                            "from": from,
                            "to": to,
                            "path": null,
                            "hops": null,
                        }),
                        pretty,
                        terse,
                        schema
                    )?
                );
            } else if compact {
                println!("no path {} -> {}", from, to);
            } else {
                println!("No path found between `{}` and `{}`.", from, to);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_explain(
    symbol: &str,
    path: &std::path::Path,
    scope: Option<&str>,
    limit: usize,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    absolute: bool,
    tabular: bool,
    schema: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let db = open_index_db(path, scope)?;

    let mut symbols = db.symbol_info(symbol)?;
    let mut callers = db.callers_of(symbol)?;
    let mut callees = db.callees_of(symbol)?;
    if !absolute {
        relativize_symbols(&mut symbols, &root);
        relativize_edges(&mut callers, &root);
        relativize_edges(&mut callees, &root);
    }

    let callers_total = callers.len();
    let callees_total = callees.len();
    let callers_truncated = limit > 0 && callers_total > limit;
    let callees_truncated = limit > 0 && callees_total > limit;
    if callers_truncated {
        callers.truncate(limit);
    }
    if callees_truncated {
        callees.truncate(limit);
    }

    let edges = db.all_edges()?;
    let comm_result = graph::detect_communities(&edges);
    let community = comm_result
        .communities
        .iter()
        .find(|c| c.members.iter().any(|m| m == symbol));

    if json_output {
        let out = serde_json::json!({
            "symbol": symbol,
            "definitions": symbols,
            "callers": callers,
            "callers_total": callers_total,
            "callers_truncated": callers_truncated,
            "callees": callees,
            "callees_total": callees_total,
            "callees_truncated": callees_truncated,
            "community": community,
        });
        println!("{}", to_json_schema(&out, pretty, terse, schema)?);
    } else if tabular {
        if !symbols.is_empty() {
            println!("section\tkind\tname\tfile\tline");
            for sym in &symbols {
                println!(
                    "def\t{}\t{}\t{}\t{}",
                    sym.kind, sym.name, sym.file, sym.line
                );
            }
        }
        if !callers.is_empty() {
            if !symbols.is_empty() {
                println!();
            }
            println!("direction\tname\tfile\tline");
            for edge in &callers {
                println!(
                    "caller\t{}\t{}\t{}",
                    edge.caller_name, edge.caller_file, edge.call_site_line
                );
            }
            if callers_truncated {
                println!("# (+{} more callers)", callers_total - limit);
            }
        }
        if !callees.is_empty() {
            for edge in &callees {
                println!(
                    "callee\t{}\t{}\t{}",
                    edge.callee_name, edge.caller_file, edge.call_site_line
                );
            }
            if callees_truncated {
                println!("# (+{} more callees)", callees_total - limit);
            }
        }
        if let Some(comm) = community {
            println!();
            println!(
                "community\t{}\t{}",
                comm.members.len(),
                comm.members.join(",")
            );
        }
    } else if compact {
        if symbols.is_empty() {
            println!("sym: {} (defs: none)", symbol);
        } else {
            for sym in &symbols {
                println!(
                    "sym: {} ({}) {}:{}",
                    sym.name,
                    abbreviate_kind(&sym.kind),
                    sym.file,
                    sym.line
                );
            }
        }

        println!("crs[{}]:", callers_total);
        if callers.is_empty() {
            println!("  (none)");
        } else {
            for line in format_edge_groups(&callers, true) {
                println!("{line}");
            }
            if callers_truncated {
                println!("  (+{} more)", callers_total - limit);
            }
        }

        println!("ces[{}]:", callees_total);
        if callees.is_empty() {
            println!("  (none)");
        } else {
            for line in format_edge_groups(&callees, false) {
                println!("{line}");
            }
            if callees_truncated {
                println!("  (+{} more)", callees_total - limit);
            }
        }

        if let Some(comm) = community {
            println!(
                "comm[{}]: {}",
                comm.members.len(),
                compact_members(&comm.members, 5)
            );
        }
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

        println!("Callers ({}):", callers_total);
        if callers.is_empty() {
            println!("  (none)");
        } else {
            for edge in &callers {
                println!(
                    "  {} ({}:{})",
                    edge.caller_name, edge.caller_file, edge.call_site_line
                );
            }
            if callers_truncated {
                println!(
                    "  (+{} more, use --limit 0 to show all)",
                    callers_total - limit
                );
            }
        }
        println!();

        println!("Callees ({}):", callees_total);
        if callees.is_empty() {
            println!("  (none)");
        } else {
            for edge in &callees {
                println!(
                    "  {} ({}:{})",
                    edge.callee_name, edge.caller_file, edge.call_site_line
                );
            }
            if callees_truncated {
                println!(
                    "  (+{} more, use --limit 0 to show all)",
                    callees_total - limit
                );
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

#[allow(clippy::too_many_arguments)]
fn cmd_audit(
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
fn cmd_summarize(
    symbol: Option<String>,
    file: Option<String>,
    extract: Option<PathBuf>,
    diff: bool,
    stats: bool,
    path: &std::path::Path,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let db_path = root.join(".tsift/summaries.db");

    // --extract mode: run LLM extraction
    if let Some(extract_path) = extract {
        let extract_base = resolve_extract_base(path)?;
        let extract_scope = resolve_extract_scope(&extract_base, &extract_path);
        let cfg = load_summarize_config(&root);

        let (files_to_extract, deleted_summary_paths) = if diff {
            let changed = summarize::git_changed_files(&root)?;
            let existing = changed
                .existing
                .into_iter()
                .filter(|f| summarize_diff_matches_scope(f, &extract_scope))
                .collect::<Vec<_>>();
            let deleted = changed
                .deleted
                .into_iter()
                .filter(|f| summarize_diff_matches_scope(f, &extract_scope))
                .map(|file_path| {
                    file_path
                        .strip_prefix(&root)
                        .unwrap_or(file_path.as_path())
                        .to_string_lossy()
                        .to_string()
                })
                .collect::<BTreeSet<_>>();
            (existing, deleted)
        } else {
            (collect_source_files(&extract_scope)?, BTreeSet::new())
        };

        if files_to_extract.is_empty() && deleted_summary_paths.is_empty() {
            println!("No files to extract.");
            return Ok(());
        }

        let _summary_write_lock = summarize::acquire_write_lock(&db_path)?;
        let summary_db = summarize::SummaryDb::open(&db_path)?;

        for rel_path in &deleted_summary_paths {
            summary_db.delete_by_file(rel_path)?;
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
                    report
                        .errors
                        .push(format!("{}: {}", file_path.display(), e));
                    continue;
                }
            };
            let hash = summarize::content_hash(&content);
            let rel_path = file_path
                .strip_prefix(&root)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            if summary_db.is_current(&rel_path, &hash)? {
                continue; // already extracted for this version
            }

            let symbol_context = find_symbols_db_for_file(&root, file_path)?;
            match summarize::extract_for_file(
                file_path,
                symbol_context.as_ref().map(|ctx| ctx.db_path.as_path()),
                symbol_context.as_ref().map(|ctx| ctx.source_root.as_path()),
                &cfg,
            ) {
                Ok(mut summaries) => {
                    for summary in &mut summaries {
                        summary.file_path = rel_path.clone();
                    }
                    let extracted_count = summaries.len();
                    let tokens_input = summaries
                        .iter()
                        .map(|summary| summary.tokens_input.unwrap_or(0))
                        .sum::<i64>();
                    let tokens_output = summaries
                        .iter()
                        .map(|summary| summary.tokens_output.unwrap_or(0))
                        .sum::<i64>();
                    summary_db.replace_file(&rel_path, &summaries)?;
                    report.symbols_extracted += extracted_count;
                    report.tokens_input += tokens_input;
                    report.tokens_output += tokens_output;
                    report.files_processed += 1;
                    if !json_output && !compact {
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
            println!("{}", to_json_schema(&report, pretty, terse, schema)?);
        } else if compact {
            println!(
                "extract files:{} symbols:{} tokens_in:{} tokens_out:{} errors:{}",
                report.files_processed,
                report.symbols_extracted,
                report.tokens_input,
                report.tokens_output,
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
        }
        return Ok(());
    }

    // --stats mode
    if stats {
        let summary_db = open_existing_summary_db_read_only(&db_path)?;
        let s = summary_db.stats()?;
        if json_output {
            println!("{}", to_json_schema(&s, pretty, terse, schema)?);
        } else if compact {
            println!(
                "summaries:{} files:{} in:{} out:{} saved:{}",
                s.total_summaries,
                s.total_files,
                s.total_tokens_input,
                s.total_tokens_output,
                s.estimated_tokens_saved
            );
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
    let summary_db = open_existing_summary_db_read_only(&db_path)?;

    if let Some(file_query) = file {
        let results = summary_db.get_by_file(&file_query)?;
        if results.is_empty() {
            println!("No cached summary for file: {}", file_query);
            println!("Run: tsift summarize --extract <path>");
            return Ok(());
        }
        if json_output {
            println!("{}", to_json_schema(&results, pretty, terse, schema)?);
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
            println!("{}", to_json_schema(&results, pretty, terse, schema)?);
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

fn open_existing_summary_db_read_only(db_path: &Path) -> Result<summarize::SummaryDb> {
    if !db_path.exists() {
        bail!("no summaries.db found — run `tsift summarize --extract <path>` first");
    }
    summarize::SummaryDb::open_read_only(db_path)
}

fn cmd_status(
    path: &std::path::Path,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let report = status::check_status(&root)?;
    if json_output {
        println!("{}", to_json_schema(&report, pretty, terse, schema)?);
    } else {
        print!("{}", status::format_human(&report, compact));
    }
    Ok(())
}

fn cmd_locks(
    path: &std::path::Path,
    scope: Option<&str>,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let report = status::check_locks(&root, Some(path), scope)?;
    if json_output {
        println!("{}", to_json_schema(&report, pretty, terse, schema)?);
    } else {
        print!("{}", status::format_locks_human(&report, compact));
    }
    Ok(())
}

fn contextualize_error(err: anyhow::Error, context: String) -> anyhow::Error {
    Result::<(), anyhow::Error>::Err(err)
        .context(context)
        .unwrap_err()
}

fn should_attach_lock_diagnostics(err: &anyhow::Error) -> bool {
    let message = err.to_string();
    message.contains("another tsift index writer is already active")
        || index::error_mentions_locked_db(err)
}

fn add_write_lock_context(
    err: anyhow::Error,
    action: String,
    root: &std::path::Path,
    scope: Option<&str>,
) -> anyhow::Error {
    if !should_attach_lock_diagnostics(&err) {
        return contextualize_error(err, action);
    }

    let Ok(report) = status::check_locks(root, None, scope) else {
        return contextualize_error(err, action);
    };

    contextualize_error(
        err,
        format!(
            "{}\n\nlock diagnostics:\n{}",
            action,
            status::format_locks_human(&report, false).trim_end()
        ),
    )
}

fn run_index_update(
    db_path: &std::path::Path,
    source_root: &std::path::Path,
    action: String,
    root: &std::path::Path,
    scope: Option<&str>,
    rebuild: bool,
    prune: bool,
) -> Result<index::IndexSummary> {
    let result = (|| {
        let db = index::IndexDb::open(db_path)?;
        if rebuild {
            db.rebuild(source_root)
        } else if prune {
            db.apply_changes_pruned(source_root)
        } else {
            db.apply_changes(source_root)
        }
    })();

    let summary = result.map_err(|err| add_write_lock_context(err, action, root, scope))?;
    emit_index_warnings(&summary, source_root, scope);
    Ok(summary)
}

fn relativize_index_summary(summary: &mut index::IndexSummary, root: &Path) {
    for change in &mut summary.changes {
        change.path = relativize_pathbuf(&change.path, root);
    }
    for warning in &mut summary.warnings {
        warning.path = relativize_pathbuf(&warning.path, root);
    }
}

fn emit_index_warnings(summary: &index::IndexSummary, root: &Path, scope: Option<&str>) {
    for warning in &summary.warnings {
        let rel_path = relativize_pathbuf(&warning.path, root);
        let stage = match warning.stage {
            index::IndexWarningStage::ReadSource => "read failed",
            index::IndexWarningStage::ExtractSymbols => "symbol extraction failed",
            index::IndexWarningStage::ExtractCallSites => "call extraction failed",
        };
        let scope_prefix = scope.map(|name| format!("[{}] ", name)).unwrap_or_default();
        let lang_suffix = warning
            .language
            .as_deref()
            .map(|lang| format!(" [{}]", lang))
            .unwrap_or_default();
        eprintln!(
            "warning: {}{}{}: {}: {}",
            scope_prefix,
            rel_path.display(),
            lang_suffix,
            stage,
            warning.message
        );
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtractSymbolContext {
    db_path: PathBuf,
    source_root: PathBuf,
}

fn find_symbols_db_for_file(root: &Path, file_path: &Path) -> Result<Option<ExtractSymbolContext>> {
    let cfg = config::Config::load(root)?;
    let mut submodules = config::Config::submodule_dirs(root)?;
    submodules.sort_by(|left, right| {
        right
            .source_root
            .components()
            .count()
            .cmp(&left.source_root.components().count())
    });

    for scope in submodules {
        if !file_path.starts_with(&scope.source_root) {
            continue;
        }
        let db_path = cfg.db_path_for(root, &scope.id);
        if db_path.exists() {
            return Ok(Some(ExtractSymbolContext {
                db_path,
                source_root: scope.source_root,
            }));
        }
    }

    let single = root.join(".tsift/index.db");
    if single.exists() && file_path.starts_with(root) {
        return Ok(Some(ExtractSymbolContext {
            db_path: single,
            source_root: root.to_path_buf(),
        }));
    }

    Ok(None)
}

fn resolve_extract_base(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;

    Ok(if canonical.is_dir() {
        canonical
    } else {
        canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(canonical)
    })
}

fn resolve_extract_scope(root: &Path, extract_path: &Path) -> PathBuf {
    if extract_path.is_absolute() {
        extract_path.to_path_buf()
    } else {
        root.join(extract_path)
    }
}

fn summarize_diff_matches_scope(changed_path: &Path, extract_scope: &Path) -> bool {
    changed_path.starts_with(extract_scope)
}

#[derive(Debug, Clone)]
struct SearchIndexTarget {
    label: String,
    db_path: PathBuf,
    source_root: PathBuf,
    scope_name: Option<String>,
    reindex_cmd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchIndexState {
    Missing,
    Fresh,
    Stale { stale_files: usize },
}

fn resolve_search_index_targets(
    root: &Path,
    path_hint: &Path,
    scope: Option<&str>,
    federated: bool,
) -> Result<Vec<SearchIndexTarget>> {
    if let Some(scope_name) = scope {
        let scope = config::Config::resolve_submodule(root, scope_name)?;
        let cfg = config::Config::load(root)?;
        return Ok(vec![SearchIndexTarget {
            label: format!("submodule `{}` index", scope.id),
            db_path: cfg.db_path_for(root, &scope.id),
            source_root: scope.source_root.clone(),
            scope_name: Some(scope.id.clone()),
            reindex_cmd: format!("tsift index --submodule {} {}", scope.id, root.display()),
        }]);
    }

    if federated {
        let cfg = config::Config::load(root)?;
        let mut targets = Vec::new();
        for scope in config::Config::submodule_dirs(root)? {
            if !cfg.federation_for_scope(&scope) {
                continue;
            }
            targets.push(SearchIndexTarget {
                label: format!("submodule `{}` index", scope.id),
                db_path: cfg.db_path_for(root, &scope.id),
                source_root: scope.source_root.clone(),
                scope_name: Some(scope.id.clone()),
                reindex_cmd: format!("tsift index --workspace {}", root.display()),
            });
        }
        return Ok(targets);
    }

    if let Some(scope) = config::Config::infer_submodule_from_path(root, path_hint)? {
        let cfg = config::Config::load(root)?;
        return Ok(vec![SearchIndexTarget {
            label: format!("submodule `{}` index", scope.id),
            db_path: cfg.db_path_for(root, &scope.id),
            source_root: scope.source_root.clone(),
            scope_name: Some(scope.id.clone()),
            reindex_cmd: format!("tsift index --submodule {} {}", scope.id, root.display()),
        }]);
    }

    let scopes = config::Config::submodule_dirs(root)?;
    if !scopes.is_empty() {
        let root_db = root.join(".tsift/index.db");
        if !root_db.exists() {
            let available_scopes = scopes
                .iter()
                .map(|scope| scope.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let cfg = config::Config::load(root)?;
            let indexed_scopes = scopes
                .iter()
                .filter(|scope| cfg.db_path_for(root, &scope.id).exists())
                .map(|scope| scope.id.as_str())
                .collect::<Vec<_>>();
            let indexed_label = if indexed_scopes.is_empty() {
                "none".to_string()
            } else {
                indexed_scopes.join(", ")
            };
            bail!(
                "workspace root {} has no shared root index at {}. Default search requires `--scope <scope>` or `--federated` when the workspace uses scoped `.tsift/indexes/*/index.db` files. Available scopes: {}. Indexed scopes: {}.",
                root.display(),
                root_db.display(),
                available_scopes,
                indexed_label,
            );
        }
    }

    Ok(vec![SearchIndexTarget {
        label: "index".to_string(),
        db_path: root.join(".tsift/index.db"),
        source_root: root.to_path_buf(),
        scope_name: None,
        reindex_cmd: format!("tsift index {}", root.display()),
    }])
}

fn inspect_search_index(target: &SearchIndexTarget) -> Result<SearchIndexState> {
    if !target.source_root.exists() || !target.db_path.exists() {
        return Ok(SearchIndexState::Missing);
    }

    let inspection =
        index::IndexDb::inspect_read_only(&target.db_path, &target.source_root, false)?;
    let stale_files =
        inspection.summary.new + inspection.summary.modified + inspection.summary.deleted;
    if stale_files == 0 {
        Ok(SearchIndexState::Fresh)
    } else {
        Ok(SearchIndexState::Stale { stale_files })
    }
}

fn apply_search_index_update(root: &Path, target: &SearchIndexTarget) -> Result<()> {
    run_index_update(
        &target.db_path,
        &target.source_root,
        format!("autoindexing {}", target.label),
        root,
        target.scope_name.as_deref(),
        false,
        false,
    )?;
    Ok(())
}

fn precheck_search_indexes(
    root: &Path,
    path_hint: &Path,
    scope: Option<&str>,
    federated: bool,
    autoindex: bool,
) -> Result<()> {
    let targets = resolve_search_index_targets(root, path_hint, scope, federated)?;
    let mut stale_targets: Vec<(String, usize, String)> = Vec::new();

    for target in targets {
        match inspect_search_index(&target)? {
            SearchIndexState::Missing => {
                if autoindex {
                    apply_search_index_update(root, &target)?;
                }
            }
            SearchIndexState::Fresh => {}
            SearchIndexState::Stale { stale_files } => {
                if autoindex {
                    apply_search_index_update(root, &target)?;
                } else {
                    stale_targets.push((target.label, stale_files, target.reindex_cmd));
                }
            }
        }
    }

    if stale_targets.is_empty() {
        return Ok(());
    }

    if stale_targets.len() == 1 {
        let (label, stale_files, reindex_cmd) = &stale_targets[0];
        let file_suffix = if *stale_files == 1 { "" } else { "s" };
        bail!(
            "tsift search aborted: {} is stale ({} file{}). \
             Run `{}` or re-run without `--no-autoindex`.",
            label,
            stale_files,
            file_suffix,
            reindex_cmd,
        );
    }

    let summary: Vec<String> = stale_targets
        .iter()
        .take(3)
        .map(|(label, stale_files, _)| format!("{} ({})", label, stale_files))
        .collect();
    let overflow = stale_targets.len().saturating_sub(summary.len());
    let mut details = summary.join(", ");
    if overflow > 0 {
        details.push_str(&format!(", +{} more", overflow));
    }
    let reindex_cmd = format!("tsift index --workspace {}", root.display());
    bail!(
        "tsift search aborted: {} indexes are stale: {}. \
         Run `{}` or re-run without `--no-autoindex`.",
        stale_targets.len(),
        details,
        reindex_cmd,
    );
}

#[allow(clippy::too_many_arguments)]
fn cmd_search(
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
    let base_path = path.unwrap_or_else(|| PathBuf::from("."));
    let root = lint::resolve_project_root_or_canonical_path(&base_path)?;
    precheck_search_indexes(&root, &base_path, scope.as_deref(), federated, autoindex)?;
    maybe_apply_search_post_precheck_test_hooks()?;

    let inferred_scope = if scope.is_none() && !federated {
        config::Config::infer_submodule_from_path(&root, &base_path)?
    } else {
        None
    };

    let (symbol_hits, sift_path) = if let Some(scope) = inferred_scope.as_ref() {
        let cfg = config::Config::load(&root)?;
        let db_path = cfg.db_path_for(&root, &scope.id);
        let hits = if db_path.exists() {
            let db = index::IndexDb::open_read_only_resilient(&db_path)?;
            db.symbol_search(&query, limit)?
        } else {
            Vec::new()
        };
        (hits, scope.source_root.clone())
    } else if let Some(ref scope_name) = scope {
        let cfg = config::Config::load(&root)?;
        let scope = config::Config::resolve_submodule(&root, scope_name)?;
        let db_path = cfg.db_path_for(&root, &scope.id);
        let hits = if db_path.exists() {
            let db = index::IndexDb::open_read_only_resilient(&db_path)?;
            db.symbol_search(&query, limit)?
        } else {
            Vec::new()
        };
        (hits, scope.source_root)
    } else if federated {
        (federated_symbol_search(&root, &query, limit)?, root.clone())
    } else {
        let db_path = root.join(".tsift/index.db");
        let hits = if db_path.exists() {
            let db = index::IndexDb::open_read_only_resilient(&db_path)?;
            db.symbol_search(&query, limit)?
        } else {
            Vec::new()
        };
        (hits, root.clone())
    };

    let mut symbol_hits = symbol_hits;
    if !absolute {
        relativize_symbol_hits(&mut symbol_hits, &root);
    }

    let effective_strategy = strategy.unwrap_or_else(|| "lexical".to_string());
    let response = if federated && scope.is_none() {
        federated_sift_search(&root, &query, limit, timeout_secs, &effective_strategy)?
    } else {
        run_search_with_timeout(&sift_path, &query, limit, timeout_secs, &effective_strategy)?
    };

    if json_output {
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
        println!("{}", to_json_schema(&combined, pretty, terse, schema)?);
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
        if symbol_hits.is_empty() && response.hits.is_empty() {
            println!("  No results.");
        }
    }
    Ok(())
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
                if matches!(
                    ext.as_ref(),
                    "rs" | "py"
                        | "ts"
                        | "tsx"
                        | "js"
                        | "jsx"
                        | "kt"
                        | "kts"
                        | "zig"
                        | "sh"
                        | "bash"
                        | "zsh"
                ) {
                    files.push(p.to_path_buf());
                }
            }
        }
    }
    Ok(files)
}

fn cmd_init(path: &std::path::Path, codex: bool, workspace: bool) -> Result<()> {
    let resolved = if workspace {
        init::resolve_workspace_dir(path)?
    } else {
        init::resolve_project_dir(path)?
    };
    if resolved != path {
        println!("resolved: {} → {}", path.display(), resolved.display());
    }
    let codex_workspace = codex && (workspace || init::has_submodules(&resolved)?);
    let result = init::init(&resolved, codex, codex_workspace)?;
    for update in result.updates {
        println!(
            "{}: {} ({})",
            update.file.display(),
            update.action,
            match update.action {
                init::InitAction::Created => "tsift Code Navigation section added",
                init::InitAction::Updated => "tsift Code Navigation section updated to latest",
                init::InitAction::AlreadyPresent => "no changes needed",
            }
        );
    }
    if result.gitignore_added {
        println!(".gitignore: added .tsift/");
    }
    if let Some(codex_result) = &result.codex_hooks {
        let scope_label = match codex_result.scope {
            init::CodexHookScope::Project => "project",
            init::CodexHookScope::Workspace => "workspace",
        };
        match codex_result.action {
            init::CodexHookAction::Added => {
                println!(
                    ".codex/hooks.json: tsift {} auto-reindex hook added",
                    scope_label
                );
            }
            init::CodexHookAction::Updated => {
                println!(
                    ".codex/hooks.json: tsift {} auto-reindex hook updated",
                    scope_label
                );
            }
            init::CodexHookAction::AlreadyPresent => {
                println!(
                    ".codex/hooks.json: tsift {} hook already present",
                    scope_label
                );
            }
            init::CodexHookAction::Created => {
                println!(
                    ".codex/hooks.json: created with tsift {} auto-reindex hook",
                    scope_label
                );
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_lint(
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
        entities.extend(lint::collect_entities_from_index_path(&index_dir)?);
    } else if let Some(root) = lint::find_project_root_for_path(file_path)? {
        entities.extend(lint::collect_entities_from_workspace_root(&root)?);
    }

    for md_path in &entities_from {
        entities.extend(lint::collect_entities_from_markdown(md_path)?);
    }

    entities.extend(lint::collect_entities_from_markdown(file_path)?);

    let result = lint::lint_markdown(file_path, &entities)?;

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

fn cmd_search_worker(
    path: &Path,
    query: &str,
    limit: usize,
    strategy: &str,
    output: &Path,
) -> Result<()> {
    maybe_apply_search_worker_test_hooks()?;
    let response = run_sift_search(path, query, limit, strategy)?;
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
mod tests {
    use super::*;

    fn init_git_repo(path: &Path) {
        let status = std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

        let status = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .status()
            .unwrap();
        assert!(status.success(), "git add failed");

        let status = std::process::Command::new("git")
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

    // --- classify_task ---

    #[test]
    fn route_search_defaults_to_haiku() {
        let (tier, model) = classify_task("find all uses of authenticate");
        assert_eq!(tier, "haiku");
        assert!(
            model.contains("haiku"),
            "expected haiku model, got {}",
            model
        );
    }

    #[test]
    fn route_edit_keywords_to_sonnet() {
        for kw in &[
            "edit the file",
            "fix the bug",
            "update the config",
            "remove dead code",
            "create a new module",
        ] {
            let (tier, _) = classify_task(kw);
            assert_eq!(tier, "sonnet", "expected sonnet for {:?}", kw);
        }
    }

    #[test]
    fn route_architecture_keywords_to_opus() {
        for kw in &[
            "design the API",
            "architecture review",
            "plan the migration",
            "analyze the system",
            "evaluate trade-offs",
        ] {
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

    #[test]
    fn cli_accepts_global_compact_flag() {
        let cli = Cli::parse_from(["tsift", "--compact", "status"]);
        assert!(cli.compact);
        assert!(matches!(cli.command, Some(Commands::Status { .. })));
    }

    #[test]
    fn summarize_diff_scope_matches_relative_directory() {
        let root = Path::new("/repo");
        let extract_scope = resolve_extract_scope(root, Path::new("src/feature"));

        assert!(summarize_diff_matches_scope(
            Path::new("/repo/src/feature/main.rs"),
            &extract_scope
        ));
        assert!(!summarize_diff_matches_scope(
            Path::new("/repo/src/other/main.rs"),
            &extract_scope
        ));
    }

    #[test]
    fn summarize_diff_scope_matches_relative_file() {
        let root = Path::new("/repo");
        let extract_scope = resolve_extract_scope(root, Path::new("src/feature/main.rs"));

        assert!(summarize_diff_matches_scope(
            Path::new("/repo/src/feature/main.rs"),
            &extract_scope
        ));
        assert!(!summarize_diff_matches_scope(
            Path::new("/repo/src/feature/lib.rs"),
            &extract_scope
        ));
    }

    #[test]
    fn summarize_extract_scope_walks_relative_paths_from_root() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let main_rs = source_dir.join("main.rs");
        std::fs::write(&main_rs, "fn alpha() {}\n").unwrap();

        let extract_scope = resolve_extract_scope(dir.path(), Path::new("src"));
        let files = collect_source_files(&extract_scope).unwrap();

        assert_eq!(files, vec![main_rs]);
    }

    #[test]
    fn summarize_extract_base_uses_nested_path_instead_of_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("root.rs"), "fn root_level() {}\n").unwrap();
        let nested_file = nested.join("main.rs");
        std::fs::write(&nested_file, "fn nested_only() {}\n").unwrap();

        let extract_base = resolve_extract_base(&nested).unwrap();
        let extract_scope = resolve_extract_scope(&extract_base, Path::new("."));
        let files = collect_source_files(&extract_scope).unwrap();

        assert_eq!(extract_scope, nested);
        assert_eq!(files, vec![nested_file]);
    }

    #[test]
    fn summarize_extract_base_uses_parent_of_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        let file_path = nested.join("main.rs");
        std::fs::write(&file_path, "fn nested_only() {}\n").unwrap();

        let extract_base = resolve_extract_base(&file_path).unwrap();

        assert_eq!(extract_base, nested);
    }

    #[test]
    fn summarize_diff_extract_includes_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# repo\n").unwrap();
        init_git_repo(dir.path());

        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let new_file = source_dir.join("new.rs");
        std::fs::write(&new_file, "fn alpha_helper() {}\n").unwrap();

        let files = summarize::git_changed_files(dir.path()).unwrap();

        assert_eq!(files.existing, vec![new_file]);
        assert!(files.deleted.is_empty());
    }

    #[test]
    fn summarize_diff_extract_tracks_deleted_files() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let deleted_file = source_dir.join("gone.rs");
        std::fs::write(&deleted_file, "fn stale() {}\n").unwrap();
        init_git_repo(dir.path());

        std::fs::remove_file(&deleted_file).unwrap();

        let files = summarize::git_changed_files(dir.path()).unwrap();

        assert!(files.existing.is_empty());
        assert_eq!(files.deleted, vec![deleted_file]);
    }

    #[test]
    fn summarize_diff_extract_deletes_removed_summary_rows() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let deleted_file = source_dir.join("gone.rs");
        std::fs::write(&deleted_file, "fn stale() {}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# repo\n").unwrap();
        init_git_repo(dir.path());

        let summary_db =
            summarize::SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        summary_db
            .insert(&summarize::Summary {
                id: 0,
                symbol_name: "stale".to_string(),
                file_path: "src/gone.rs".to_string(),
                content_hash: "hash1".to_string(),
                summary: "stale summary".to_string(),
                entities: None,
                relationships: None,
                concept_labels: None,
                extracted_at: "1700000000".to_string(),
                model: "test".to_string(),
                tokens_input: Some(100),
                tokens_output: Some(50),
            })
            .unwrap();

        std::fs::remove_file(&deleted_file).unwrap();

        cmd_summarize(
            None,
            None,
            Some(PathBuf::from("src")),
            true,
            false,
            dir.path(),
            false,
            true,
            false,
            false,
            false,
        )
        .unwrap();

        assert!(summary_db.get_by_file("src/gone.rs").unwrap().is_empty());
    }

    #[test]
    fn summarize_extract_fails_fast_when_summary_writer_lock_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let file = source_dir.join("lib.rs");
        std::fs::write(&file, "fn helper() {}\n").unwrap();

        let content = std::fs::read(&file).unwrap();
        let summary_db =
            summarize::SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        summary_db
            .insert(&summarize::Summary {
                id: 0,
                symbol_name: "lib.rs".to_string(),
                file_path: "src/lib.rs".to_string(),
                content_hash: summarize::content_hash(&content),
                summary: "cached summary".to_string(),
                entities: None,
                relationships: None,
                concept_labels: None,
                extracted_at: "1700000000".to_string(),
                model: "test".to_string(),
                tokens_input: Some(100),
                tokens_output: Some(50),
            })
            .unwrap();
        drop(summary_db);

        let lock_path = summarize::writer_lock_path(&dir.path().join(".tsift/summaries.db"));
        let _lock = hold_writer_lock(&lock_path);

        let err = cmd_summarize(
            None,
            None,
            Some(PathBuf::from("src")),
            false,
            false,
            dir.path(),
            false,
            true,
            false,
            false,
            false,
        )
        .unwrap_err();
        let message = err.to_string();

        assert!(message.contains("another tsift summarize extractor is already active"));
        assert!(message.contains("tsift summarize --extract"));
    }

    #[test]
    fn summarize_stats_fails_closed_when_cache_missing() {
        let dir = tempfile::tempdir().unwrap();
        let err = cmd_summarize(
            None,
            None,
            None,
            false,
            true,
            dir.path(),
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("no summaries.db found"),
            "got: {err}"
        );
        assert!(!dir.path().join(".tsift/summaries.db").exists());
    }

    #[test]
    fn summarize_cmd_uses_ancestor_project_root_for_nested_paths() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let summary_db =
            summarize::SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        summary_db
            .insert(&summarize::Summary {
                id: 0,
                symbol_name: "alpha_helper".to_string(),
                file_path: "src/lib.rs".to_string(),
                content_hash: "hash1".to_string(),
                summary: "cached summary".to_string(),
                entities: None,
                relationships: None,
                concept_labels: None,
                extracted_at: "1700000000".to_string(),
                model: "claude-haiku-4-5-20251001".to_string(),
                tokens_input: Some(100),
                tokens_output: Some(40),
            })
            .unwrap();

        let result = cmd_summarize(
            Some("alpha_helper".to_string()),
            None,
            None,
            false,
            false,
            &nested,
            false,
            true,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
        assert!(!nested.join(".tsift/summaries.db").exists());
    }

    #[test]
    fn summarize_extract_uses_matching_scoped_index_for_workspace_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
        )
        .unwrap();

        let alpha_root = dir.path().join("src/alpha");
        let beta_root = dir.path().join("src/beta");
        std::fs::create_dir_all(alpha_root.join("src")).unwrap();
        std::fs::create_dir_all(beta_root.join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join(".tsift/indexes/alpha")).unwrap();
        std::fs::create_dir_all(dir.path().join(".tsift/indexes/beta")).unwrap();
        std::fs::write(alpha_root.join("src/lib.rs"), "fn alpha_helper() {}\n").unwrap();
        let beta_file = beta_root.join("src/lib.rs");
        std::fs::write(&beta_file, "fn beta_helper() {}\n").unwrap();
        std::fs::write(dir.path().join(".tsift/indexes/alpha/index.db"), "").unwrap();
        std::fs::write(dir.path().join(".tsift/indexes/beta/index.db"), "").unwrap();

        let context = find_symbols_db_for_file(dir.path(), &beta_file)
            .unwrap()
            .expect("expected matching scoped index");

        assert_eq!(
            context.db_path,
            dir.path().join(".tsift/indexes/beta/index.db")
        );
        assert_eq!(context.source_root, beta_root);
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

    #[test]
    fn edit_batch_rolls_back_when_later_swap_fails() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha.txt");
        let beta = dir.path().join("beta.txt");
        fs::write(&alpha, "alpha old\n").unwrap();
        fs::write(&beta, "beta old\n").unwrap();

        let batch = EditBatch {
            edits: vec![
                EditOp {
                    file: alpha.clone(),
                    old: "old".to_string(),
                    new: "new".to_string(),
                    replace_all: false,
                },
                EditOp {
                    file: beta.clone(),
                    old: "old".to_string(),
                    new: "new".to_string(),
                    replace_all: false,
                },
            ],
        };

        let plan = build_edit_plan(&batch).unwrap();
        let err = match apply_edit_plan_atomically_inner(plan, |commit_index, _| {
            if commit_index == 1 {
                bail!("simulated swap failure");
            }
            Ok(())
        }) {
            Ok(_) => panic!("expected simulated swap failure"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("simulated swap failure"));
        assert_eq!(fs::read_to_string(&alpha).unwrap(), "alpha old\n");
        assert_eq!(fs::read_to_string(&beta).unwrap(), "beta old\n");
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
        assert_eq!(
            result,
            Some("tsift search \"authenticate\" --strategy lexical".to_string())
        );
    }

    #[test]
    fn rewrite_rg_with_path() {
        let result = rewrite_command("rg authenticate src/");
        assert_eq!(
            result,
            Some("tsift search \"authenticate\" --strategy lexical --path \"src/\"".to_string())
        );
    }

    #[test]
    fn rewrite_rg_with_flags_ignored() {
        let result = rewrite_command("rg -i authenticate src/");
        assert_eq!(
            result,
            Some("tsift search \"authenticate\" --strategy lexical --path \"src/\"".to_string())
        );
    }

    #[test]
    fn rewrite_rg_with_type_flag() {
        // -t rs takes a value, should be skipped; pattern is next positional
        let result = rewrite_command("rg -t rs authenticate");
        assert_eq!(
            result,
            Some("tsift search \"authenticate\" --strategy lexical".to_string())
        );
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
        assert_eq!(
            result,
            Some("tsift search \"authenticate\" --strategy lexical --path \"src/\"".to_string())
        );
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
        assert_eq!(
            result,
            Some("tsift search \"fn main\" --strategy lexical".to_string())
        );
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
        let (columns, rows) =
            execute_query(&conn, "SELECT name, email FROM users ORDER BY id").unwrap();
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
            "SELECT u.name, p.title FROM users u JOIN posts p ON u.id = p.user_id ORDER BY p.id",
        )
        .unwrap();
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
        conn.execute_batch("CREATE TABLE empty_tbl (id INTEGER PRIMARY KEY, data BLOB)")
            .unwrap();
        let tables = schema_overview(&conn).unwrap();
        assert_eq!(tables[0].row_count, 0);
        assert_eq!(tables[0].columns.len(), 2);
    }

    // --- graph command ---

    fn setup_graph_index() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "fn helper() { println!(\"hi\"); }\nfn main() { helper(); Vec::new(); }",
        )
        .unwrap();
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
        let result = cmd_graph(
            "main",
            dir.path(),
            false,
            false,
            None,
            20,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn compact_helpers_trim_scores_and_snippets() {
        assert_eq!(format_score(0.12345, true), "0.12");
        assert_eq!(format_score(0.12345, false), "0.1235");
        let snippet = compact_snippet("    first line with useful context\nsecond");
        assert_eq!(snippet.as_deref(), Some("first line with useful context"));
    }

    #[test]
    fn compact_members_caps_list() {
        let members = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
            "e".to_string(),
            "f".to_string(),
        ];
        assert_eq!(compact_members(&members, 5), "a, b, c, d, e (+1 more)");
    }

    #[test]
    fn abbreviate_kind_maps_common_kinds() {
        assert_eq!(abbreviate_kind("function"), "fn");
        assert_eq!(abbreviate_kind("method"), "meth");
        assert_eq!(abbreviate_kind("class"), "cls");
        assert_eq!(abbreviate_kind("interface"), "iface");
        assert_eq!(abbreviate_kind("type_alias"), "type");
        assert_eq!(abbreviate_kind("data_class"), "data_cls");
        assert_eq!(abbreviate_kind("sealed_class"), "sealed_cls");
        assert_eq!(abbreviate_kind("enum_class"), "enum_cls");
        assert_eq!(abbreviate_kind("companion_object"), "comp_obj");
        assert_eq!(abbreviate_kind("object"), "obj");
        assert_eq!(abbreviate_kind("heading"), "h");
        assert_eq!(abbreviate_kind("code_block"), "code");
        // short kinds pass through
        assert_eq!(abbreviate_kind("struct"), "struct");
        assert_eq!(abbreviate_kind("trait"), "trait");
        assert_eq!(abbreviate_kind("enum"), "enum");
        assert_eq!(abbreviate_kind("const"), "const");
        assert_eq!(abbreviate_kind("unknown_kind"), "unknown_kind");
    }

    #[test]
    fn abbreviate_match_type_maps_search_types() {
        assert_eq!(abbreviate_match_type("exact_name"), "exact");
        assert_eq!(abbreviate_match_type("partial_tags"), "partial");
        assert_eq!(abbreviate_match_type("all_tags"), "all_tags");
        assert_eq!(abbreviate_match_type("other_type"), "other_type");
    }

    #[test]
    fn explain_compact_groups_edges_by_file() {
        let edges = vec![
            index::StoredEdge {
                caller_file: "src/main.rs".to_string(),
                caller_name: "main".to_string(),
                caller_line: 1,
                callee_name: "helper".to_string(),
                call_site_line: 2,
            },
            index::StoredEdge {
                caller_file: "src/main.rs".to_string(),
                caller_name: "main".to_string(),
                caller_line: 1,
                callee_name: "render".to_string(),
                call_site_line: 3,
            },
        ];
        let lines = format_edge_groups(&edges, false);
        assert_eq!(lines, vec!["  src/main.rs: helper, render"]);
    }

    // --- workspace indexing ---

    fn setup_workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(".gitmodules"),
            r#"[submodule "src/alpha"]
	path = src/alpha
	url = https://example.com/alpha
[submodule "src/beta"]
	path = src/beta
	url = https://example.com/beta
"#,
        )
        .unwrap();
        let alpha = root.join("src/alpha");
        let beta = root.join("src/beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        std::fs::write(
            alpha.join("lib.rs"),
            "fn alpha_helper() {}\nfn alpha_main() { alpha_helper(); }",
        )
        .unwrap();
        std::fs::write(beta.join("lib.rs"), "fn beta_func() {}").unwrap();
        dir
    }

    fn setup_workspace_with_duplicate_leaf_names() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(".gitmodules"),
            r#"[submodule "pkg/app/foo"]
	path = pkg/app/foo
	url = https://example.com/pkg-app-foo
[submodule "vendor/foo"]
	path = vendor/foo
	url = https://example.com/vendor-foo
"#,
        )
        .unwrap();
        let pkg_foo = root.join("pkg/app/foo");
        let vendor_foo = root.join("vendor/foo");
        std::fs::create_dir_all(&pkg_foo).unwrap();
        std::fs::create_dir_all(&vendor_foo).unwrap();
        std::fs::write(
            pkg_foo.join("lib.rs"),
            "fn pkg_only() {}\nfn shared_name() { pkg_only(); }\n",
        )
        .unwrap();
        std::fs::write(
            vendor_foo.join("lib.rs"),
            "fn vendor_only() {}\nfn shared_name() { vendor_only(); }\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn workspace_index_creates_per_submodule_dbs() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        assert!(dir.path().join(".tsift/indexes/alpha/index.db").exists());
        assert!(dir.path().join(".tsift/indexes/beta/index.db").exists());
    }

    #[test]
    fn workspace_index_single_submodule() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            false,
            Some("alpha"),
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        assert!(dir.path().join(".tsift/indexes/alpha/index.db").exists());
        assert!(!dir.path().join(".tsift/indexes/beta/index.db").exists());
    }

    #[test]
    fn workspace_index_single_submodule_errors_on_unknown_scope() {
        let dir = setup_workspace();

        let err = cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            false,
            Some("missing"),
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("unknown scope `missing`"));
        assert!(msg.contains("Available scopes: alpha, beta"));
        assert!(!dir.path().join(".tsift/indexes/missing/index.db").exists());
    }

    #[test]
    fn workspace_index_uses_unique_scope_ids_when_leaf_names_collide() {
        let dir = setup_workspace_with_duplicate_leaf_names();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        assert!(
            dir.path()
                .join(".tsift/indexes/pkg/app/foo/index.db")
                .exists()
        );
        assert!(
            dir.path()
                .join(".tsift/indexes/vendor/foo/index.db")
                .exists()
        );
    }

    #[test]
    fn federated_search_across_submodules() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let hits = federated_symbol_search(dir.path(), "alpha_helper", 10).unwrap();
        assert!(
            !hits.is_empty(),
            "should find alpha_helper via federated search"
        );
    }

    #[test]
    fn federated_search_respects_isolation() {
        let dir = setup_workspace();
        let tsift_dir = dir.path().join(".tsift");
        std::fs::create_dir_all(&tsift_dir).unwrap();
        std::fs::write(
            tsift_dir.join("config.toml"),
            r#"
[overrides.alpha]
tier = "isolated"
"#,
        )
        .unwrap();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let hits = federated_symbol_search(dir.path(), "alpha_helper", 10).unwrap();
        assert!(
            hits.is_empty(),
            "isolated submodule should not appear in federated search"
        );
    }

    #[test]
    fn federated_lexical_search_respects_isolation() {
        let dir = setup_workspace();
        let tsift_dir = dir.path().join(".tsift");
        std::fs::create_dir_all(&tsift_dir).unwrap();
        std::fs::write(
            tsift_dir.join("config.toml"),
            r#"
[overrides.alpha]
tier = "isolated"
"#,
        )
        .unwrap();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let response = federated_sift_search(dir.path(), "fn", 10, 0, "lexical").unwrap();

        assert!(
            !response.hits.is_empty(),
            "shared scopes should still contribute lexical hits"
        );
        assert!(
            response
                .hits
                .iter()
                .all(|hit| hit.path.ends_with("src/beta/lib.rs")),
            "isolated scope should not leak lexical hits: {:?}",
            response.hits
        );
    }

    #[test]
    fn federated_lexical_search_respects_private_tier() {
        let dir = setup_workspace();
        let tsift_dir = dir.path().join(".tsift");
        std::fs::create_dir_all(&tsift_dir).unwrap();
        std::fs::write(
            tsift_dir.join("config.toml"),
            r#"
[overrides.alpha]
tier = "private"
"#,
        )
        .unwrap();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let response = federated_sift_search(dir.path(), "fn", 10, 0, "lexical").unwrap();

        assert!(
            !response.hits.is_empty(),
            "shared scopes should still contribute lexical hits"
        );
        assert!(
            response
                .hits
                .iter()
                .all(|hit| hit.path.ends_with("src/beta/lib.rs")),
            "private scope should not leak lexical hits: {:?}",
            response.hits
        );
    }

    #[test]
    fn scoped_search_finds_submodule_symbols() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let cfg = config::Config::load(dir.path()).unwrap();
        let db_path = cfg.db_path_for(dir.path(), "alpha");
        let db = index::IndexDb::open(&db_path).unwrap();
        let hits = db.symbol_search("alpha_main", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].name, "alpha_main");
    }

    #[test]
    fn scoped_search_cmd_errors_on_unknown_scope() {
        let dir = setup_workspace();

        let err = cmd_search(
            "alpha_main".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            Some("missing".to_string()),
            false,
            false,
            false,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("unknown scope `missing`"));
        assert!(msg.contains("Available scopes: alpha, beta"));
    }

    #[test]
    fn scoped_search_cmd_errors_on_ambiguous_legacy_scope_name() {
        let dir = setup_workspace_with_duplicate_leaf_names();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let err = cmd_search(
            "vendor_only".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            Some("foo".to_string()),
            false,
            false,
            false,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("ambiguous scope `foo`"));
        assert!(msg.contains("pkg/app/foo"));
        assert!(msg.contains("vendor/foo"));
    }

    #[test]
    fn scoped_graph_query() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let cfg = config::Config::load(dir.path()).unwrap();
        let db_path = cfg.db_path_for(dir.path(), "alpha");
        let db = index::IndexDb::open(&db_path).unwrap();
        let callees = db.callees_of("alpha_main").unwrap();
        let names: Vec<&str> = callees.iter().map(|e| e.callee_name.as_str()).collect();
        assert!(names.contains(&"alpha_helper"));
    }

    fn assert_workspace_query_requires_scope(err: anyhow::Error) {
        let msg = err.to_string();
        assert!(msg.contains("require `--scope <scope>`"), "{msg}");
        assert!(msg.contains("Available scopes: alpha, beta"), "{msg}");
        assert!(msg.contains("Indexed scopes: alpha, beta"), "{msg}");
        assert!(
            !msg.contains("no index found at"),
            "workspace query should fail with scope guidance, got: {msg}"
        );
    }

    fn assert_workspace_search_requires_explicit_target(err: anyhow::Error) {
        let msg = err.to_string();
        assert!(
            msg.contains("requires `--scope <scope>` or `--federated`"),
            "{msg}"
        );
        assert!(msg.contains("Available scopes: alpha, beta"), "{msg}");
        assert!(msg.contains("Indexed scopes: alpha, beta"), "{msg}");
        assert!(
            !msg.contains("autoindexing index"),
            "workspace search should fail before creating a shared root index: {msg}"
        );
    }

    #[test]
    fn graph_cmd_requires_scope_for_workspace_root_without_shared_index() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let err = cmd_graph(
            "alpha_main",
            dir.path(),
            false,
            false,
            None,
            20,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert_workspace_query_requires_scope(err);
    }

    #[test]
    fn graph_cmd_infers_scope_from_nested_workspace_path() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let nested = dir.path().join("src/alpha/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let result = cmd_graph(
            "alpha_main",
            &nested,
            false,
            false,
            None,
            20,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn communities_cmd_requires_scope_for_workspace_root_without_shared_index() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let err = cmd_communities(
            dir.path(),
            None,
            1,
            10,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert_workspace_query_requires_scope(err);
    }

    #[test]
    fn communities_cmd_infers_scope_from_nested_workspace_path() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let nested = dir.path().join("src/alpha/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let result = cmd_communities(
            &nested, None, 1, 10, false, false, false, false, false, false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn path_cmd_requires_scope_for_workspace_root_without_shared_index() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let err = cmd_path(
            "alpha_main",
            "alpha_helper",
            dir.path(),
            None,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert_workspace_query_requires_scope(err);
    }

    #[test]
    fn path_cmd_infers_scope_from_nested_workspace_path() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let nested = dir.path().join("src/alpha/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let result = cmd_path(
            "alpha_main",
            "alpha_helper",
            &nested,
            None,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn path_cmd_uses_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = setup_graph_index();
        let db_path = dir.path().join(".tsift/index.db");
        let _lock = hold_rollback_journal_lock(&db_path);

        let result = cmd_path(
            "main",
            "helper",
            dir.path(),
            None,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn explain_cmd_requires_scope_for_workspace_root_without_shared_index() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let err = cmd_explain(
            "alpha_main",
            dir.path(),
            None,
            15,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert_workspace_query_requires_scope(err);
    }

    #[test]
    fn explain_cmd_infers_scope_from_nested_workspace_path() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let nested = dir.path().join("src/alpha/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let result = cmd_explain(
            "alpha_main",
            &nested,
            None,
            15,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn explain_cmd_uses_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = setup_graph_index();
        let db_path = dir.path().join(".tsift/index.db");
        let _lock = hold_rollback_journal_lock(&db_path);

        let result = cmd_explain(
            "main",
            dir.path(),
            None,
            15,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
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
        let result = cmd_communities(
            dir.path(),
            None,
            2,
            10,
            false,
            false,
            false,
            false,
            false,
            false,
        );
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
        let result = cmd_path(
            "a",
            "b",
            dir.path(),
            None,
            false,
            false,
            false,
            false,
            false,
        );
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
        let result = cmd_explain(
            "main",
            dir.path(),
            None,
            15,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(result.is_err());
    }

    fn hold_write_lock(db_path: &std::path::Path) -> Connection {
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn
    }

    fn hold_writer_lock(lock_path: &std::path::Path) -> std::fs::File {
        use fs4::fs_std::FileExt;
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        assert!(file.try_lock_exclusive().unwrap());
        writeln!(file, "{}", std::process::id()).unwrap();
        file
    }

    fn hold_rollback_journal_lock(db_path: &std::path::Path) -> Connection {
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .unwrap();
        std::fs::write(index::rollback_journal_path(db_path), "locked").unwrap();
        conn
    }

    #[test]
    fn search_cmd_succeeds_while_writer_lock_is_held() {
        let dir = setup_graph_index();
        let db_path = dir.path().join(".tsift/index.db");
        let _lock = hold_write_lock(&db_path);

        let result = cmd_search(
            "main".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            None,
            false,
            false,
            false,
            0,
            true,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn search_cmd_uses_snapshot_fallback_when_rollback_journal_lock_appears_after_precheck() {
        let dir = setup_graph_index();
        let _hook = install_search_post_precheck_lock(dir.path().join(".tsift/index.db"));

        let result = cmd_search(
            "main".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            None,
            false,
            false,
            false,
            0,
            true,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn search_cmd_fails_fast_when_autoindex_disabled_and_index_is_stale() {
        let dir = setup_graph_index();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            dir.path().join("main.rs"),
            "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }",
        )
        .unwrap();

        let err = cmd_search(
            "helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            None,
            false,
            false,
            false,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("search aborted"));
        assert!(err.to_string().contains("index is stale"));
        assert!(err.to_string().contains("--no-autoindex"));
    }

    #[test]
    fn search_cmd_reports_stale_when_root_index_is_locked_by_rollback_journal() {
        let dir = setup_graph_index();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            dir.path().join("main.rs"),
            "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }",
        )
        .unwrap();
        let _lock = hold_rollback_journal_lock(&dir.path().join(".tsift/index.db"));

        let err = cmd_search(
            "helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            None,
            false,
            false,
            false,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("search aborted"));
        assert!(err.to_string().contains("index is stale"));
        assert!(!err.to_string().contains("database is locked"));
    }

    #[test]
    fn search_cmd_autoindexes_stale_index_by_default() {
        let dir = setup_graph_index();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            dir.path().join("main.rs"),
            "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }",
        )
        .unwrap();

        let result = cmd_search(
            "helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            None,
            false,
            false,
            true,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());

        let db = index::IndexDb::open_read_only(&dir.path().join(".tsift/index.db")).unwrap();
        let summary = db.compute_changes(dir.path()).unwrap();
        assert_eq!(summary.new + summary.modified + summary.deleted, 0);
    }

    #[test]
    fn search_cmd_autoindex_reports_lock_diagnostics_when_root_writer_is_blocked() {
        let dir = setup_graph_index();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            dir.path().join("main.rs"),
            "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }",
        )
        .unwrap();
        let _lock = hold_rollback_journal_lock(&dir.path().join(".tsift/index.db"));

        let err = cmd_search(
            "helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            None,
            false,
            false,
            true,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("autoindexing index"));
        assert!(msg.contains("lock diagnostics:"));
        assert!(msg.contains("journal: present"));
        assert!(msg.contains("next: inspect the host for a wedged writer"));
    }

    #[test]
    fn search_cmd_uses_ancestor_project_root_for_nested_paths() {
        let dir = setup_graph_index();
        let nested = dir.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let result = cmd_search(
            "helper".to_string(),
            Some(nested.clone()),
            5,
            Some("lexical".to_string()),
            None,
            false,
            false,
            true,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
        assert!(!nested.join(".tsift/index.db").exists());
    }

    #[test]
    fn index_cmd_uses_ancestor_project_root_for_nested_paths() {
        let dir = setup_graph_index();
        let nested = dir.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("extra.rs"), "fn nested_helper() {}\n").unwrap();

        let result = cmd_index(
            &nested, false, false, false, false, false, false, None, false, false, false, false,
            false, false,
        );

        assert!(result.is_ok());
        assert!(dir.path().join(".tsift/index.db").exists());
        assert!(!nested.join(".tsift/index.db").exists());
    }

    #[test]
    fn workspace_index_cmd_uses_ancestor_project_root_for_nested_paths() {
        let dir = setup_workspace();
        let nested = dir.path().join("docs/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let result = cmd_index(
            &nested, false, false, false, false, false, true, None, false, false, false, false,
            false, false,
        );

        let cfg = config::Config::load(dir.path()).unwrap();

        assert!(result.is_ok());
        assert!(cfg.db_path_for(dir.path(), "alpha").exists());
        assert!(cfg.db_path_for(dir.path(), "beta").exists());
    }

    #[test]
    fn locks_report_uses_ancestor_project_root_for_nested_paths() {
        let dir = setup_graph_index();
        let nested = dir.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let root = lint::resolve_project_root_or_canonical_path(&nested).unwrap();
        let report = status::check_locks(&root, Some(&nested), None).unwrap();

        assert_eq!(report.source_root, dir.path());
        assert_eq!(report.db_path, dir.path().join(".tsift/index.db"));
    }

    #[test]
    fn workspace_locks_report_infers_scope_from_nested_path() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let nested = dir.path().join("src/alpha/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let root = lint::resolve_project_root_or_canonical_path(&nested).unwrap();
        let report = status::check_locks(&root, Some(&nested), None).unwrap();
        let cfg = config::Config::load(dir.path()).unwrap();

        assert_eq!(report.label, "submodule `alpha` index");
        assert_eq!(report.source_root, dir.path().join("src/alpha"));
        assert_eq!(report.db_path, cfg.db_path_for(dir.path(), "alpha"));
        assert_eq!(
            report.reindex_command,
            format!("tsift index --submodule alpha {}", dir.path().display())
        );
    }

    #[test]
    fn scoped_search_cmd_autoindexes_stale_submodule_index_by_default() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let alpha = dir.path().join("src/alpha/lib.rs");
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            &alpha,
            "fn alpha_helper() { println!(\"updated\"); }\nfn alpha_main() { alpha_helper(); }",
        )
        .unwrap();

        let result = cmd_search(
            "alpha_helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            Some("alpha".to_string()),
            false,
            false,
            true,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());

        let cfg = config::Config::load(dir.path()).unwrap();
        let db = index::IndexDb::open_read_only(&cfg.db_path_for(dir.path(), "alpha")).unwrap();
        let summary = db.compute_changes(&dir.path().join("src/alpha")).unwrap();
        assert_eq!(summary.new + summary.modified + summary.deleted, 0);
    }

    #[test]
    fn scoped_search_cmd_reports_stale_when_submodule_index_is_locked_by_rollback_journal() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let alpha = dir.path().join("src/alpha/lib.rs");
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            &alpha,
            "fn alpha_helper() { println!(\"updated\"); }\nfn alpha_main() { alpha_helper(); }",
        )
        .unwrap();

        let cfg = config::Config::load(dir.path()).unwrap();
        let _lock = hold_rollback_journal_lock(&cfg.db_path_for(dir.path(), "alpha"));

        let err = cmd_search(
            "alpha_helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            Some("alpha".to_string()),
            false,
            false,
            false,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("search aborted"));
        assert!(err.to_string().contains("submodule `alpha` index"));
        assert!(!err.to_string().contains("database is locked"));
    }

    #[test]
    fn federated_search_cmd_autoindexes_stale_indexes_by_default() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let alpha = dir.path().join("src/alpha/lib.rs");
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            &alpha,
            "fn alpha_helper() { println!(\"updated\"); }\nfn alpha_main() { alpha_helper(); }",
        )
        .unwrap();

        let result = cmd_search(
            "alpha_helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            None,
            true,
            false,
            true,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());

        let cfg = config::Config::load(dir.path()).unwrap();
        let db = index::IndexDb::open_read_only(&cfg.db_path_for(dir.path(), "alpha")).unwrap();
        let summary = db.compute_changes(&dir.path().join("src/alpha")).unwrap();
        assert_eq!(summary.new + summary.modified + summary.deleted, 0);
    }

    #[test]
    fn federated_search_cmd_reports_stale_when_submodule_index_is_locked_by_rollback_journal() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let alpha = dir.path().join("src/alpha/lib.rs");
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            &alpha,
            "fn alpha_helper() { println!(\"updated\"); }\nfn alpha_main() { alpha_helper(); }",
        )
        .unwrap();

        let cfg = config::Config::load(dir.path()).unwrap();
        let _lock = hold_rollback_journal_lock(&cfg.db_path_for(dir.path(), "alpha"));

        let err = cmd_search(
            "alpha_helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            None,
            true,
            false,
            false,
            30,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("stale"));
        assert!(err.to_string().contains("submodule `alpha` index"));
        assert!(!err.to_string().contains("database is locked"));
    }

    #[test]
    fn workspace_search_cmd_requires_explicit_target_without_shared_root_index() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let err = cmd_search(
            "alpha_helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("lexical".to_string()),
            None,
            false,
            false,
            true,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap_err();

        assert_workspace_search_requires_explicit_target(err);
        assert!(!dir.path().join(".tsift/index.db").exists());
    }

    #[test]
    fn workspace_search_cmd_infers_scope_from_nested_path() {
        let dir = setup_workspace();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let nested = dir.path().join("src/alpha/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let result = cmd_search(
            "alpha_helper".to_string(),
            Some(nested),
            5,
            Some("lexical".to_string()),
            None,
            false,
            false,
            false,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn resolve_query_db_path_infers_matching_duplicate_leaf_scope_from_nested_path() {
        let dir = setup_workspace_with_duplicate_leaf_names();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            true,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();
        let nested = dir.path().join("vendor/foo/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let root = lint::resolve_project_root_or_canonical_path(&nested).unwrap();
        let db_path = resolve_query_db_path(&root, &nested, None).unwrap();
        let cfg = config::Config::load(dir.path()).unwrap();

        assert_eq!(db_path, cfg.db_path_for(dir.path(), "vendor/foo"));
    }

    #[test]
    fn graph_cmd_succeeds_while_writer_lock_is_held() {
        let dir = setup_graph_index();
        let db_path = dir.path().join(".tsift/index.db");
        let _lock = hold_write_lock(&db_path);

        let result = cmd_graph(
            "main",
            dir.path(),
            false,
            false,
            None,
            20,
            false,
            true,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn graph_cmd_uses_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = setup_graph_index();
        let db_path = dir.path().join(".tsift/index.db");
        let _lock = hold_rollback_journal_lock(&db_path);

        let result = cmd_graph(
            "main",
            dir.path(),
            false,
            false,
            None,
            20,
            false,
            true,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn graph_cmd_uses_ancestor_project_root_for_nested_paths() {
        let dir = setup_graph_index();
        let nested = dir.path().join("src/nested");
        std::fs::create_dir_all(&nested).unwrap();

        let result = cmd_graph(
            "helper", &nested, true, false, None, 20, false, false, false, false, false, false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn communities_cmd_succeeds_while_writer_lock_is_held() {
        let dir = setup_graph_index();
        let _lock = hold_writer_lock(&dir.path().join(".tsift/index.lock"));

        let result = cmd_communities(
            dir.path(),
            None,
            1,
            10,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn communities_cmd_uses_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = setup_graph_index();
        let db_path = dir.path().join(".tsift/index.db");
        let _lock = hold_rollback_journal_lock(&db_path);

        let result = cmd_communities(
            dir.path(),
            None,
            1,
            10,
            false,
            false,
            false,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn lint_finds_entities_from_project_root_index_db() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn alpha_helper() {}\n").unwrap();
        std::fs::write(
            dir.path().join("README.md"),
            "alpha_helper should be backticked.\n",
        )
        .unwrap();
        cmd_index(
            dir.path(),
            false,
            false,
            false,
            false,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        )
        .unwrap();

        let root = lint::find_project_root_for_path(&dir.path().join("README.md"))
            .unwrap()
            .unwrap();
        let entities = lint::collect_entities_from_index_path(&root).unwrap();
        let result = lint::lint_markdown(&dir.path().join("README.md"), &entities).unwrap();

        assert!(
            result
                .annotations
                .iter()
                .any(|ann| ann.text == "alpha_helper")
        );
    }

    // --- search timeout ---

    #[test]
    fn search_direct_runs_ok() {
        let dir = tempfile::tempdir().unwrap();
        let search_dir = dir.path().to_path_buf();
        std::fs::write(search_dir.join("test.rs"), "fn main() {}").unwrap();
        let result = run_sift_search(&search_dir, "main", 1, "lexical");
        assert!(result.is_ok(), "direct search should succeed");
    }

    #[test]
    fn search_timeout_zero_disables_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let search_dir = dir.path().to_path_buf();
        std::fs::write(search_dir.join("test.rs"), "fn main() {}").unwrap();
        let result = run_search_with_timeout(&search_dir, "main", 1, 0, "lexical");
        assert!(result.is_ok(), "timeout=0 should still work (no timeout)");
    }

    #[test]
    fn search_worker_output_path_uses_json_suffix() {
        let path = next_search_worker_output_path();
        assert!(path.extension().is_some_and(|ext| ext == "json"));
    }

    // --- index quiet mode ---

    #[test]
    fn index_quiet_suppresses_file_list() {
        let dir = setup_graph_index();
        let result = cmd_index(
            dir.path(),
            false,
            true,
            false,
            false,
            true,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn index_exit_code_implies_quiet() {
        let dir = setup_graph_index();
        let result = cmd_index(
            dir.path(),
            false,
            true,
            false,
            false,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn index_quiet_json_omits_changes() {
        let dir = setup_graph_index();
        let result = cmd_index(
            dir.path(),
            false,
            true,
            false,
            false,
            true,
            false,
            None,
            true,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(result.is_ok());
    }

    // --- JSON compact vs pretty ---

    #[test]
    fn to_json_compact_default() {
        let val = serde_json::json!({"a": 1, "b": [2, 3]});
        let compact = to_json(&val, false, false).unwrap();
        assert!(!compact.contains('\n'));
        assert!(
            compact.contains("\"a\":1")
                || compact.contains("\"a\": 1")
                || compact.contains("\"a\":")
        );
    }

    #[test]
    fn to_json_pretty_indents() {
        let val = serde_json::json!({"a": 1, "b": [2, 3]});
        let pretty = to_json(&val, true, false).unwrap();
        assert!(pretty.contains('\n'));
        assert!(pretty.contains("  "));
    }

    #[test]
    fn to_json_compact_is_shorter() {
        let val =
            serde_json::json!({"name": "test", "items": [1, 2, 3], "nested": {"key": "value"}});
        let compact = to_json(&val, false, false).unwrap();
        let pretty = to_json(&val, true, false).unwrap();
        assert!(compact.len() < pretty.len());
    }

    #[test]
    fn terse_renames_keys() {
        let val =
            serde_json::json!({"caller_file": "a.rs", "caller_name": "main", "call_site_line": 10});
        let result = to_json(&val, false, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["_s"].is_object());
        let d = &parsed["d"];
        assert_eq!(d["cf"], "a.rs");
        assert_eq!(d["cn"], "main");
        assert_eq!(d["csl"], 10);
    }

    #[test]
    fn terse_schema_only_includes_used_keys() {
        let val = serde_json::json!({"name": "test", "score": 0.5});
        let result = to_json(&val, false, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let schema = parsed["_s"].as_object().unwrap();
        assert_eq!(schema["n"], "name");
        assert_eq!(schema["sc"], "score");
        assert!(!schema.contains_key("cf"));
    }

    #[test]
    fn terse_nested_arrays() {
        let val = serde_json::json!({"callers": [{"caller_name": "a", "caller_file": "b.rs", "caller_line": 1, "callee_name": "c", "call_site_line": 2}]});
        let result = to_json(&val, false, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let d = &parsed["d"];
        assert_eq!(d["crs"][0]["cn"], "a");
        assert_eq!(d["crs"][0]["cf"], "b.rs");
    }

    #[test]
    fn terse_preserves_unknown_keys() {
        let val = serde_json::json!({"custom_field": "value", "name": "test"});
        let result = to_json(&val, false, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let d = &parsed["d"];
        assert_eq!(d["custom_field"], "value");
        assert_eq!(d["n"], "test");
    }

    // --- schema-then-values ---

    #[test]
    fn schema_converts_homogeneous_arrays() {
        let val = serde_json::json!({"symbols": [
            {"name": "foo", "kind": "fn", "line": 10},
            {"name": "bar", "kind": "fn", "line": 20}
        ]});
        let result = to_json_schema(&val, false, false, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let syms = &parsed["symbols"];
        // serde_json uses BTreeMap — keys sorted alphabetically
        assert_eq!(syms["_c"], serde_json::json!(["kind", "line", "name"]));
        assert_eq!(syms["_r"][0], serde_json::json!(["fn", 10, "foo"]));
        assert_eq!(syms["_r"][1], serde_json::json!(["fn", 20, "bar"]));
    }

    #[test]
    fn schema_skips_short_arrays() {
        let val = serde_json::json!({"items": [{"name": "only"}]});
        let result = to_json_schema(&val, false, false, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["items"].is_array());
        assert_eq!(parsed["items"][0]["name"], "only");
    }

    #[test]
    fn schema_skips_heterogeneous_arrays() {
        let val = serde_json::json!({"items": [{"a": 1}, {"b": 2}]});
        let result = to_json_schema(&val, false, false, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["items"].is_array());
        assert_eq!(parsed["items"][0]["a"], 1);
    }

    #[test]
    fn schema_with_terse_combines() {
        let val = serde_json::json!({"callers": [
            {"caller_name": "a", "caller_file": "x.rs"},
            {"caller_name": "b", "caller_file": "y.rs"}
        ]});
        let result = to_json_schema(&val, false, true, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["_s"].is_object());
        let d = &parsed["d"];
        let crs = &d["crs"];
        assert!(crs["_c"].is_array());
        assert!(crs["_r"].is_array());
        // terse: caller_file→cf, caller_name→cn; BTreeMap sorts: cf < cn
        assert_eq!(crs["_r"][0], serde_json::json!(["x.rs", "a"]));
    }

    #[test]
    fn schema_preserves_non_object_arrays() {
        let val = serde_json::json!({"tags": ["a", "b", "c"]});
        let result = to_json_schema(&val, false, false, true).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["tags"], serde_json::json!(["a", "b", "c"]));
    }

    #[test]
    fn cli_accepts_global_schema_flag() {
        let cli = Cli::parse_from(["tsift", "--schema", "search", "test"]);
        assert!(cli.schema);
        assert!(matches!(cli.command, Some(Commands::Search { .. })));
    }

    #[test]
    fn cli_accepts_locks_command() {
        let cli = Cli::parse_from(["tsift", "locks"]);
        assert!(matches!(cli.command, Some(Commands::Locks { .. })));
    }

    #[test]
    fn cli_locks_accepts_scope_flag() {
        let cli = Cli::parse_from(["tsift", "locks", "--scope", "alpha"]);
        match cli.command {
            Some(Commands::Locks { scope, .. }) => {
                assert_eq!(scope.as_deref(), Some("alpha"));
            }
            _ => panic!("expected Locks command"),
        }
    }

    #[test]
    fn cli_search_accepts_autoindex_flag() {
        let cli = Cli::parse_from(["tsift", "search", "test", "--autoindex"]);
        match cli.command {
            Some(Commands::Search {
                autoindex,
                no_autoindex,
                ..
            }) => {
                assert!(autoindex);
                assert!(!no_autoindex);
            }
            _ => panic!("expected Search command"),
        }
    }

    #[test]
    fn cli_search_autoindexes_by_default() {
        let cli = Cli::parse_from(["tsift", "search", "test"]);
        match cli.command {
            Some(Commands::Search {
                autoindex,
                no_autoindex,
                ..
            }) => {
                assert!(!autoindex);
                assert!(!no_autoindex);
                assert!(autoindex || !no_autoindex);
            }
            _ => panic!("expected Search command"),
        }
    }

    #[test]
    fn cli_search_accepts_no_autoindex_flag() {
        let cli = Cli::parse_from(["tsift", "search", "test", "--no-autoindex"]);
        match cli.command {
            Some(Commands::Search {
                autoindex,
                no_autoindex,
                ..
            }) => {
                assert!(!autoindex);
                assert!(no_autoindex);
            }
            _ => panic!("expected Search command"),
        }
    }

    #[test]
    fn cli_search_rejects_conflicting_autoindex_flags() {
        let cli = Cli::try_parse_from(["tsift", "search", "test", "--autoindex", "--no-autoindex"]);
        assert!(cli.is_err());
    }

    // --- relativize paths ---

    #[test]
    fn cli_accepts_global_absolute_flag() {
        let cli = Cli::parse_from(["tsift", "--absolute", "status"]);
        assert!(cli.absolute);
        assert!(matches!(cli.command, Some(Commands::Status { .. })));
    }

    #[test]
    fn cli_accepts_global_tabular_flag() {
        let cli = Cli::parse_from(["tsift", "--tabular", "search", "test"]);
        assert!(cli.tabular);
        assert!(matches!(cli.command, Some(Commands::Search { .. })));
    }

    #[test]
    fn cli_tabular_with_graph() {
        let cli = Cli::parse_from(["tsift", "--tabular", "graph", "main"]);
        assert!(cli.tabular);
        assert!(matches!(cli.command, Some(Commands::Graph { .. })));
    }

    #[test]
    fn cli_tabular_with_communities() {
        let cli = Cli::parse_from(["tsift", "--tabular", "communities"]);
        assert!(cli.tabular);
        assert!(matches!(cli.command, Some(Commands::Communities { .. })));
    }

    #[test]
    fn cli_tabular_with_explain() {
        let cli = Cli::parse_from(["tsift", "--tabular", "explain", "main"]);
        assert!(cli.tabular);
        assert!(matches!(cli.command, Some(Commands::Explain { .. })));
    }

    #[test]
    fn relativize_strips_root_prefix() {
        let root = std::path::Path::new("/home/user/project");
        assert_eq!(
            relativize("/home/user/project/src/main.rs", root),
            "src/main.rs"
        );
    }

    #[test]
    fn relativize_leaves_non_matching_path() {
        let root = std::path::Path::new("/home/user/project");
        assert_eq!(
            relativize("/other/path/file.rs", root),
            "/other/path/file.rs"
        );
    }

    #[test]
    fn relativize_leaves_already_relative() {
        let root = std::path::Path::new("/home/user/project");
        assert_eq!(relativize("src/main.rs", root), "src/main.rs");
    }

    #[test]
    fn relativize_pathbuf_strips_prefix() {
        let root = std::path::Path::new("/home/user/project");
        let path = std::path::Path::new("/home/user/project/src/lib.rs");
        assert_eq!(relativize_pathbuf(path, root), PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn relativize_edges_strips_caller_file() {
        let root = std::path::Path::new("/tmp/proj");
        let mut edges = vec![index::StoredEdge {
            caller_file: "/tmp/proj/src/main.rs".to_string(),
            caller_name: "main".to_string(),
            caller_line: 1,
            callee_name: "helper".to_string(),
            call_site_line: 5,
        }];
        relativize_edges(&mut edges, root);
        assert_eq!(edges[0].caller_file, "src/main.rs");
    }

    #[test]
    fn relativize_json_paths_strips_known_keys() {
        let root = std::path::Path::new("/tmp/proj");
        let mut val = serde_json::json!({
            "file": "/tmp/proj/src/main.rs",
            "path": "/tmp/proj/test.rs",
            "name": "/tmp/proj/not-a-path",
            "hits": [{"path": "/tmp/proj/nested.rs", "score": 1.0}]
        });
        relativize_json_paths(&mut val, root);
        assert_eq!(val["file"], "src/main.rs");
        assert_eq!(val["path"], "test.rs");
        assert_eq!(val["name"], "/tmp/proj/not-a-path");
        assert_eq!(val["hits"][0]["path"], "nested.rs");
    }

    // --- limit caps ---

    #[test]
    fn cli_graph_accepts_limit_flag() {
        let cli = Cli::parse_from(["tsift", "graph", "main", "--limit", "5"]);
        match cli.command {
            Some(Commands::Graph { limit, .. }) => assert_eq!(limit, 5),
            _ => panic!("expected Graph command"),
        }
    }

    #[test]
    fn cli_graph_default_limit_is_20() {
        let cli = Cli::parse_from(["tsift", "graph", "main"]);
        match cli.command {
            Some(Commands::Graph { limit, .. }) => assert_eq!(limit, 20),
            _ => panic!("expected Graph command"),
        }
    }

    #[test]
    fn cli_communities_accepts_limit_flag() {
        let cli = Cli::parse_from(["tsift", "communities", "--limit", "3"]);
        match cli.command {
            Some(Commands::Communities { limit, .. }) => assert_eq!(limit, 3),
            _ => panic!("expected Communities command"),
        }
    }

    #[test]
    fn cli_communities_default_limit_is_10() {
        let cli = Cli::parse_from(["tsift", "communities"]);
        match cli.command {
            Some(Commands::Communities { limit, .. }) => assert_eq!(limit, 10),
            _ => panic!("expected Communities command"),
        }
    }

    #[test]
    fn cli_explain_accepts_limit_flag() {
        let cli = Cli::parse_from(["tsift", "explain", "main", "--limit", "7"]);
        match cli.command {
            Some(Commands::Explain { limit, .. }) => assert_eq!(limit, 7),
            _ => panic!("expected Explain command"),
        }
    }

    #[test]
    fn cli_explain_default_limit_is_15() {
        let cli = Cli::parse_from(["tsift", "explain", "main"]);
        match cli.command {
            Some(Commands::Explain { limit, .. }) => assert_eq!(limit, 15),
            _ => panic!("expected Explain command"),
        }
    }

    #[test]
    fn cli_limit_zero_means_unlimited() {
        let cli = Cli::parse_from(["tsift", "graph", "main", "--limit", "0"]);
        match cli.command {
            Some(Commands::Graph { limit, .. }) => assert_eq!(limit, 0),
            _ => panic!("expected Graph command"),
        }
    }

    #[test]
    fn graph_cmd_limit_runs_ok() {
        let dir = setup_graph_index();
        let result = cmd_graph(
            "main",
            dir.path(),
            false,
            false,
            None,
            1,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn graph_cmd_unlimited_runs_ok() {
        let dir = setup_graph_index();
        let result = cmd_graph(
            "main",
            dir.path(),
            false,
            false,
            None,
            0,
            false,
            false,
            false,
            false,
            false,
            false,
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn graph_cmd_tabular_runs_ok() {
        let dir = setup_graph_index();
        let result = cmd_graph(
            "main",
            dir.path(),
            false,
            false,
            None,
            20,
            false,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn communities_cmd_tabular_runs_ok() {
        let dir = setup_graph_index();
        let result = cmd_communities(
            dir.path(),
            None,
            1,
            10,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn explain_cmd_tabular_runs_ok() {
        let dir = setup_graph_index();
        let result = cmd_explain(
            "main",
            dir.path(),
            None,
            15,
            false,
            false,
            false,
            false,
            false,
            true,
            false,
        );
        assert!(result.is_ok());
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
    let table_names: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut tables = Vec::new();
    for tbl in table_names {
        let columns = table_columns(conn, &tbl)?;
        let row_count: i64 =
            conn.query_row(&format!("SELECT COUNT(*) FROM \"{}\"", tbl), [], |row| {
                row.get(0)
            })?;
        tables.push(TableInfo {
            name: tbl,
            columns,
            row_count,
        });
    }
    Ok(tables)
}

/// Get column metadata for a single table.
pub(crate) fn table_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{}\")", table))?;
    let cols = stmt
        .query_map([], |row| {
            Ok(ColumnInfo {
                name: row.get(1)?,
                col_type: row.get::<_, String>(2).unwrap_or_default(),
                notnull: row.get::<_, bool>(3).unwrap_or(false),
                pk: row.get::<_, i32>(5).unwrap_or(0) > 0,
                default_value: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

/// Execute an arbitrary SQL query and return rows as JSON values.
pub(crate) fn execute_query(
    conn: &Connection,
    sql: &str,
) -> Result<(Vec<String>, Vec<Vec<serde_json::Value>>)> {
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

#[allow(clippy::too_many_arguments)]
fn cmd_sql(
    db_path: &std::path::Path,
    query: Option<String>,
    table: Option<String>,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let conn = open_db(db_path)?;

    match (query, table) {
        (Some(sql), _) => {
            let (columns, rows) = execute_query(&conn, &sql)?;
            if json_output {
                let json_rows: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|row| {
                        let obj: serde_json::Map<String, serde_json::Value> = columns
                            .iter()
                            .zip(row.iter())
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        serde_json::Value::Object(obj)
                    })
                    .collect();
                println!("{}", to_json_schema(&json_rows, pretty, terse, schema)?);
            } else if compact {
                println!("rows:{} cols:{}", rows.len(), columns.len());
                for row in &rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::Null => "NULL".to_string(),
                            serde_json::Value::String(s) => truncate_for_compact(s, 40),
                            other => other.to_string(),
                        })
                        .collect();
                    println!("  {}", cells.join(" | "));
                }
            } else {
                // Tabular output
                if columns.is_empty() {
                    println!("Query returned no columns.");
                    return Ok(());
                }
                // Header
                println!("{}", columns.join(" | "));
                println!(
                    "{}",
                    columns
                        .iter()
                        .map(|c| "-".repeat(c.len().max(4)))
                        .collect::<Vec<_>>()
                        .join("-+-")
                );
                for row in &rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|v| match v {
                            serde_json::Value::Null => "NULL".to_string(),
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect();
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
                println!("{}", to_json_schema(&cols, pretty, terse, schema)?);
            } else if compact {
                println!("table:{} columns:{}", tbl, cols.len());
                for col in &cols {
                    println!("  {} {}", col.name, col.col_type);
                }
            } else {
                println!("Table: {}", tbl);
                println!("{:<20} {:<12} {:<8} PK", "Column", "Type", "NotNull");
                println!("{}", "-".repeat(50));
                for col in &cols {
                    println!(
                        "{:<20} {:<12} {:<8} {}",
                        col.name,
                        col.col_type,
                        col.notnull,
                        if col.pk { "PK" } else { "" }
                    );
                }
            }
        }
        (None, None) => {
            let tables = schema_overview(&conn)?;
            if json_output {
                println!("{}", to_json_schema(&tables, pretty, terse, schema)?);
            } else if compact {
                println!("tables:{}", tables.len());
                for tbl in &tables {
                    println!(
                        "  {} rows:{} cols:{}",
                        tbl.name,
                        tbl.row_count,
                        tbl.columns.len()
                    );
                }
            } else {
                println!("Database: {}", db_path.display());
                println!("{} table(s)\n", tables.len());
                for tbl in &tables {
                    println!("  {} ({} rows)", tbl.name, tbl.row_count);
                    for col in &tbl.columns {
                        let flags = [
                            if col.pk { "PK" } else { "" },
                            if col.notnull { "NOT NULL" } else { "" },
                        ]
                        .iter()
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ");
                        let suffix = if flags.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", flags)
                        };
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
    if cmd.contains('|')
        || cmd.contains('>')
        || cmd.contains("--replace")
        || cmd.contains("--count")
        || cmd.contains("-c")
        || cmd.contains("--files-with-matches")
        || cmd.contains("-l")
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
        if matches!(
            *part,
            "-t" | "--type"
                | "-g"
                | "--glob"
                | "-A"
                | "-B"
                | "-C"
                | "--max-count"
                | "--max-depth"
                | "-m"
                | "-e"
        ) {
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
    let has_recursive = parts.iter().any(|p| {
        *p == "-r"
            || *p == "-R"
            || *p == "--recursive"
            || p.contains('r') && p.starts_with('-') && !p.starts_with("--")
    });
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
    let unquoted =
        if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
            &s[1..s.len() - 1]
        } else {
            s
        };

    if unquoted
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        format!("\"{}\"", unquoted)
    } else {
        format!(
            "\"{}\"",
            unquoted.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }
}

fn empty_search_coverage() -> sift::SearchCoverageSnapshot {
    sift::SearchCoverageSnapshot {
        mode: sift::SearchCoverageMode::Sealed,
        total_sector_count: 0,
        mounted_sector_count: 0,
        reused_sector_count: 0,
        dirty_sector_count: 0,
        completed_dirty_sector_count: 0,
        rebuilding_sector_count: 0,
        resumed_sector_count: 0,
        active_rebuild: None,
    }
}

fn aggregate_search_coverage(responses: &[sift::SearchResponse]) -> sift::SearchCoverageSnapshot {
    let total_sector_count = responses
        .iter()
        .map(|response| response.coverage.total_sector_count)
        .sum();
    let mounted_sector_count = responses
        .iter()
        .map(|response| response.coverage.mounted_sector_count)
        .sum();
    let reused_sector_count = responses
        .iter()
        .map(|response| response.coverage.reused_sector_count)
        .sum();
    let dirty_sector_count = responses
        .iter()
        .map(|response| response.coverage.dirty_sector_count)
        .sum();
    let completed_dirty_sector_count = responses
        .iter()
        .map(|response| response.coverage.completed_dirty_sector_count)
        .sum();
    let rebuilding_sector_count = responses
        .iter()
        .map(|response| response.coverage.rebuilding_sector_count)
        .sum();
    let resumed_sector_count = responses
        .iter()
        .map(|response| response.coverage.resumed_sector_count)
        .sum();

    let mode = if dirty_sector_count == 0 && rebuilding_sector_count == 0 {
        sift::SearchCoverageMode::Sealed
    } else if completed_dirty_sector_count > 0
        || rebuilding_sector_count > 0
        || resumed_sector_count > 0
    {
        sift::SearchCoverageMode::Converging
    } else {
        sift::SearchCoverageMode::Frontier
    };

    sift::SearchCoverageSnapshot {
        mode,
        total_sector_count,
        mounted_sector_count,
        reused_sector_count,
        dirty_sector_count,
        completed_dirty_sector_count,
        rebuilding_sector_count,
        resumed_sector_count,
        active_rebuild: responses
            .iter()
            .find_map(|response| response.coverage.active_rebuild.clone()),
    }
}

fn empty_search_response(root: &Path, strategy: &str) -> sift::SearchResponse {
    sift::SearchResponse {
        strategy: strategy.to_string(),
        root: root.display().to_string(),
        indexed_artifacts: 0,
        skipped_artifacts: 0,
        coverage: empty_search_coverage(),
        hits: Vec::new(),
    }
}

fn absolutize_search_hit_paths(response: &mut sift::SearchResponse, search_root: &Path) {
    for hit in &mut response.hits {
        let path = Path::new(&hit.path);
        if path.is_relative() {
            hit.path = search_root.join(path).display().to_string();
        }
    }
}

fn merge_search_responses(
    root: &Path,
    strategy: &str,
    limit: usize,
    responses: Vec<sift::SearchResponse>,
) -> sift::SearchResponse {
    let indexed_artifacts = responses
        .iter()
        .map(|response| response.indexed_artifacts)
        .sum();
    let skipped_artifacts = responses
        .iter()
        .map(|response| response.skipped_artifacts)
        .sum();
    let coverage = if responses.is_empty() {
        empty_search_coverage()
    } else {
        aggregate_search_coverage(&responses)
    };
    let mut hits: Vec<sift::SearchHit> = responses
        .into_iter()
        .flat_map(|response| response.hits)
        .collect();
    hits.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.location.cmp(&right.location))
    });
    hits.truncate(limit);
    for (rank, hit) in hits.iter_mut().enumerate() {
        hit.rank = rank + 1;
    }

    sift::SearchResponse {
        strategy: strategy.to_string(),
        root: root.display().to_string(),
        indexed_artifacts,
        skipped_artifacts,
        coverage,
        hits,
    }
}

fn federated_sift_search(
    root: &Path,
    query: &str,
    limit: usize,
    timeout_secs: u64,
    strategy: &str,
) -> Result<sift::SearchResponse> {
    let targets = resolve_search_index_targets(root, root, None, true)?;
    if targets.is_empty() {
        if config::Config::submodule_dirs(root)?.is_empty() {
            return run_search_with_timeout(root, query, limit, timeout_secs, strategy);
        }
        return Ok(empty_search_response(root, strategy));
    }

    let mut responses = Vec::with_capacity(targets.len());
    for target in targets {
        let mut response =
            run_search_with_timeout(&target.source_root, query, limit, timeout_secs, strategy)?;
        absolutize_search_hit_paths(&mut response, &target.source_root);
        response.root = root.display().to_string();
        responses.push(response);
    }

    Ok(merge_search_responses(root, strategy, limit, responses))
}

fn federated_symbol_search(
    root: &std::path::Path,
    query: &str,
    limit: usize,
) -> Result<Vec<index::SymbolHit>> {
    let cfg = config::Config::load(root)?;
    let submodules = config::Config::submodule_dirs(root)?;
    let mut all_hits: Vec<index::SymbolHit> = Vec::new();
    for scope in &submodules {
        if !cfg.federation_for_scope(scope) {
            continue;
        }
        let db_path = cfg.db_path_for(root, &scope.id);
        if !db_path.exists() {
            continue;
        }
        let db = index::IndexDb::open_read_only(&db_path)?;
        let mut hits = db.symbol_search(query, limit)?;
        all_hits.append(&mut hits);
    }
    all_hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    all_hits.truncate(limit);
    Ok(all_hits)
}

fn run_sift_search(
    search_path: &Path,
    query: &str,
    limit: usize,
    strategy: &str,
) -> Result<sift::SearchResponse> {
    let engine = Sift::builder().build();
    let options = SearchOptions::default()
        .with_limit(limit)
        .with_strategy(strategy.to_string());
    let input = SearchInput::new(search_path, query).with_options(options);
    engine.search(input).context("sift search failed")
}

fn run_search_with_timeout(
    search_path: &Path,
    query: &str,
    limit: usize,
    timeout_secs: u64,
    strategy: &str,
) -> Result<sift::SearchResponse> {
    if timeout_secs == 0 {
        return run_sift_search(search_path, query, limit, strategy);
    }

    let output_path = next_search_worker_output_path();
    let mut child = Command::new(
        std::env::current_exe().context("resolving tsift executable for timed search")?,
    )
    .arg("__search-worker")
    .arg("--path")
    .arg(search_path)
    .arg("--query")
    .arg(query)
    .arg("--limit")
    .arg(limit.to_string())
    .arg("--strategy")
    .arg(strategy)
    .arg("--output")
    .arg(&output_path)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .context("spawning timed sift search worker")?;

    let timeout = Duration::from_secs(timeout_secs);
    let status =
        wait_for_child_exit(&mut child, timeout).context("waiting for timed sift search worker")?;
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_file(&output_path);
        bail!(
            "tsift search timed out after {}s (strategy: {}). \
             The index may be stale — run `tsift index .` to rebuild, \
             or use `--timeout 0` to disable the timeout.",
            timeout_secs,
            strategy,
        );
    }

    let status = status.unwrap();
    let stderr = read_child_stderr(&mut child)?;
    if !status.success() {
        let _ = fs::remove_file(&output_path);
        let message = stderr.trim();
        if message.is_empty() {
            bail!("sift search worker exited with status {}", status);
        }
        bail!("{}", message);
    }

    let raw = fs::read_to_string(&output_path)
        .with_context(|| format!("reading search worker output: {}", output_path.display()))?;
    let _ = fs::remove_file(&output_path);
    serde_json::from_str(&raw).context("parsing search worker output")
}

fn next_search_worker_output_path() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "tsift-search-{}-{}.json",
        std::process::id(),
        stamp
    ))
}

fn wait_for_child_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<Option<std::process::ExitStatus>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if started.elapsed() >= timeout {
            return Ok(None);
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

fn read_child_stderr(child: &mut std::process::Child) -> Result<String> {
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr)
            .context("reading search worker stderr")?;
    }
    Ok(stderr)
}

fn maybe_apply_search_worker_test_hooks() -> Result<()> {
    if let Ok(path) = std::env::var("TSIFT_TEST_SEARCH_WORKER_PID_FILE") {
        fs::write(&path, std::process::id().to_string())
            .with_context(|| format!("writing search worker pid file: {path}"))?;
    }
    if let Ok(ms) = std::env::var("TSIFT_TEST_SEARCH_WORKER_SLEEP_MS") {
        let delay_ms = ms
            .parse::<u64>()
            .with_context(|| format!("parsing TSIFT_TEST_SEARCH_WORKER_SLEEP_MS={ms}"))?;
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static SEARCH_POST_PRECHECK_LOCK_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[cfg(test)]
struct SearchPostPrecheckLockGuard;

#[cfg(test)]
impl Drop for SearchPostPrecheckLockGuard {
    fn drop(&mut self) {
        SEARCH_POST_PRECHECK_LOCK_PATH.with(|path| {
            path.borrow_mut().take();
        });
    }
}

#[cfg(test)]
fn install_search_post_precheck_lock(db_path: PathBuf) -> SearchPostPrecheckLockGuard {
    SEARCH_POST_PRECHECK_LOCK_PATH.with(|path| {
        assert!(
            path.borrow().is_none(),
            "search post-precheck lock hook already installed"
        );
        *path.borrow_mut() = Some(db_path);
    });
    SearchPostPrecheckLockGuard
}

#[cfg(test)]
fn maybe_apply_search_post_precheck_test_hooks() -> Result<()> {
    let Some(db_path) = SEARCH_POST_PRECHECK_LOCK_PATH.with(|path| path.borrow_mut().take()) else {
        return Ok(());
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let conn = Connection::open(&db_path).expect("opening db for rollback-journal hook");
        conn.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .expect("acquiring rollback-journal hook lock");
        fs::write(index::rollback_journal_path(&db_path), "locked")
            .expect("writing rollback journal marker");
        ready_tx.send(()).expect("signaling rollback-journal hook");
        std::thread::sleep(Duration::from_millis(200));
        drop(conn);
        let _ = fs::remove_file(index::rollback_journal_path(&db_path));
    });
    ready_rx
        .recv_timeout(Duration::from_secs(1))
        .context("waiting for search post-precheck rollback-journal hook")?;
    Ok(())
}

#[cfg(not(test))]
fn maybe_apply_search_post_precheck_test_hooks() -> Result<()> {
    Ok(())
}
