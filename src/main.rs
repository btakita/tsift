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
    /// Recommend a Claude model tier for a task (haiku/search, sonnet/edit, opus/architecture)
    Route {
        /// Task description to classify
        task: String,
        /// Output only the model ID (for scripting)
        #[arg(long)]
        id: bool,
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
        Some(Commands::Route { task, id }) => cmd_route(&task, id),
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
pub fn apply_edit_op(content: &str, op: &EditOp) -> Result<(String, usize)> {
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
