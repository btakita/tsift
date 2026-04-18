use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sift::{SearchInput, SearchOptions, Sift};
use std::fs;
use std::io::Read as _;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "tsift", version, about = "Token-efficient search for Claude Code")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Search a codebase using hybrid BM25 + vector search
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
            json,
        }) => cmd_search(query, path, limit, strategy, json),
        Some(Commands::Edit { dry_run, file }) => cmd_edit(dry_run, file),
        None => {
            println!("tsift v{}", env!("CARGO_PKG_VERSION"));
            println!("Run `tsift --help` for usage.");
            Ok(())
        }
    }
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
    let mut plan: Vec<(usize, String, String, usize)> = Vec::new(); // (idx, original, replacement, count)

    for (i, op) in batch.edits.iter().enumerate() {
        let content = fs::read_to_string(&op.file)
            .with_context(|| format!("edit #{}: reading {}", i + 1, op.file.display()))?;

        if op.old == op.new {
            bail!("edit #{}: old and new strings are identical in {}", i + 1, op.file.display());
        }

        let count = content.matches(&op.old).count();
        if count == 0 {
            bail!(
                "edit #{}: old_string not found in {}",
                i + 1,
                op.file.display()
            );
        }
        if count > 1 && !op.replace_all {
            bail!(
                "edit #{}: old_string matches {} times in {} (use replace_all or provide more context)",
                i + 1,
                count,
                op.file.display()
            );
        }

        let replaced = if op.replace_all {
            content.replace(&op.old, &op.new)
        } else {
            content.replacen(&op.old, &op.new, 1)
        };
        plan.push((i, op.file.display().to_string(), replaced, count));
    }

    // Phase 2: write all validated edits
    let mut results: Vec<EditResult> = Vec::new();

    for (i, _file_path, new_content, count) in &plan {
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

fn cmd_search(
    query: String,
    path: Option<PathBuf>,
    limit: usize,
    strategy: Option<String>,
    json_output: bool,
) -> Result<()> {
    let search_path = path.unwrap_or_else(|| PathBuf::from("."));
    let engine = Sift::builder().build();
    let mut options = SearchOptions::default().with_limit(limit);
    if let Some(s) = strategy {
        options = options.with_strategy(s);
    }
    let input = SearchInput::new(&search_path, &query).with_options(options);
    let response = engine.search(input)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
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
        if response.hits.is_empty() {
            println!("  No results.");
        }
    }
    Ok(())
}
