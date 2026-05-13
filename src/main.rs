use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sift::{SearchInput, SearchOptions, Sift};
#[cfg(test)]
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tagpath::{family as tagpath_family, ontology as tagpath_ontology};
use tempfile::NamedTempFile;

pub mod audit;
pub mod config;
pub mod dci_benchmark;
pub mod diff_digest;
pub mod graph;
pub mod index;
pub mod init;
mod lang;
pub mod lint;
pub mod log_digest;
pub mod metric_digest;
pub mod runtime_churn;
pub mod session_cost;
pub mod session_digest;
pub mod session_review;
pub mod status;
pub mod summarize;
pub mod test_digest;
pub mod walk;

#[cfg(test)]
mod sim_world;

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

    /// Wrap supported JSON responses in a common summary envelope (implies --json)
    #[arg(long, global = true)]
    envelope: bool,

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
        /// Search strategy: lexical, exact, vector, hybrid, path-hybrid
        #[arg(short, long)]
        strategy: Option<String>,
        /// Use the exact-text backend (`rg -F`) instead of sift/BM25
        #[arg(long, conflicts_with = "strategy")]
        exact: bool,
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
        /// Preview-mode item cap for token-budgeted responses
        #[arg(long)]
        max_items: Option<usize>,
        /// Preview-mode per-field byte cap for token-budgeted responses
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Named preview budget preset (auto adapts from context-window env vars)
        #[arg(long, value_enum)]
        budget: Option<ResponseBudgetPreset>,
    },
    #[command(hide = true, name = "__search-worker")]
    SearchWorker {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        cache_dir: PathBuf,
        #[arg(long)]
        query: String,
        #[arg(long)]
        limit: usize,
        #[arg(long)]
        strategy: String,
        #[arg(long)]
        output: PathBuf,
    },
    #[command(hide = true, name = "__digest-runner")]
    DigestRunner {
        /// Digest mode: test or log
        #[arg(long)]
        kind: String,
        /// Path to the codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Force the test parser (`cargo`, `pytest`, or `auto`) when kind=test
        #[arg(long)]
        runner: Option<String>,
        /// Shell command to execute and digest
        #[arg(long)]
        shell_command: String,
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
    /// Rewrite a shell command to use tsift, or run the bounded tsift equivalent directly
    Rewrite {
        /// The shell command to potentially rewrite
        command: String,
        /// Execute the rewritten tsift command instead of only printing it
        #[arg(long)]
        run: bool,
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
        /// Preview-mode item cap for token-budgeted responses
        #[arg(long)]
        max_items: Option<usize>,
        /// Preview-mode per-field byte cap for token-budgeted responses
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Named preview budget preset (auto adapts from context-window env vars)
        #[arg(long, value_enum)]
        budget: Option<ResponseBudgetPreset>,
    },
    /// Read a bounded source-file line window with expansion handles and index refs
    SourceRead {
        /// Source file to preview (relative to --path/root unless absolute)
        file: PathBuf,
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// First line to include (1-based)
        #[arg(long, default_value = "1")]
        start: usize,
        /// Number of lines to include
        #[arg(long, default_value = "80", conflicts_with = "end")]
        lines: usize,
        /// Last line to include (1-based, inclusive)
        #[arg(long)]
        end: Option<usize>,
        /// Restrict index refs to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Preview-mode item cap for symbol/summary refs
        #[arg(long)]
        max_items: Option<usize>,
        /// Preview-mode per-field byte cap for snippets and summaries
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Named preview budget preset (auto adapts from context-window env vars)
        #[arg(long, value_enum)]
        budget: Option<ResponseBudgetPreset>,
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
    /// Summarize git-changed files into a bounded, code-aware digest
    DiffDigest {
        /// Path to the codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Compare the staged index against HEAD instead of the working tree
        #[arg(long, conflicts_with = "revision")]
        cached: bool,
        /// Compare a single revision against its first parent instead of the working tree
        #[arg(long)]
        revision: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Summarize captured test runner output into grouped failures
    TestDigest {
        /// Path to the codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Read captured test output from a file instead of stdin
        #[arg(long)]
        input: Option<PathBuf>,
        /// Force the parser (`cargo`, `pytest`, or `auto`)
        #[arg(long)]
        runner: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Summarize captured verbose logs into bounded signals, anchors, and repeated lines
    LogDigest {
        /// Path to the codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Read captured log output from a file instead of stdin
        #[arg(long)]
        input: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Compose session-review --next-context plus diff/test/log digests into one resumable handoff payload
    ContextPack {
        /// Target document or repo path to review (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Read captured test output from a file and inline its digest
        #[arg(long)]
        test_input: Option<PathBuf>,
        /// Force the test parser (`cargo`, `pytest`, or `auto`) when --test-input is present
        #[arg(long)]
        runner: Option<String>,
        /// Read captured build/install log output from a file and inline its digest
        #[arg(long)]
        log_input: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Preview-mode item cap for token-budgeted responses
        #[arg(long)]
        max_items: Option<usize>,
        /// Preview-mode per-field byte cap for token-budgeted responses
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Named preview budget preset (auto adapts from context-window env vars)
        #[arg(long, value_enum)]
        budget: Option<ResponseBudgetPreset>,
    },
    /// Compare raw symbol output with compact tag-family preview envelopes
    TokenSavings {
        /// Fixture describing raw symbols, tagpath families, and minimum savings thresholds
        #[arg(long)]
        fixture: PathBuf,
        /// Exit non-zero when any case misses its fixture threshold
        #[arg(long)]
        fail_under: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Summarize repeated metric runs into compact deltas and news-ready tables
    MetricDigest {
        /// Read metric-run JSON from a file instead of stdin
        #[arg(long)]
        input: Option<PathBuf>,
        /// Optional baseline/history JSON to compare against
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Focus the digest on specific metric keys (repeatable)
        #[arg(long = "metric")]
        metrics: Vec<String>,
        /// Override the direction for metrics where lower values are better (repeatable)
        #[arg(long = "lower-is-better")]
        lower_is_better: Vec<String>,
        /// Override the direction for metrics where higher values are better (repeatable)
        #[arg(long = "higher-is-better")]
        higher_is_better: Vec<String>,
        /// Number of runs to keep in the emitted history/news table
        #[arg(long, default_value = "3")]
        history: usize,
        /// Number of top improvements/regressions to emit
        #[arg(long, default_value = "3")]
        top: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Compare recorded DCI search workflows across exact, lexical, and hybrid strategies
    DciBenchmark {
        /// Fixture describing multi-hop tasks and recorded strategy metrics
        #[arg(long)]
        fixture: PathBuf,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Print composable agent workflows that preserve tsift result handles
    Workflow {
        /// Workflow topic to print
        #[arg(default_value = "search")]
        topic: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Summarize session transcripts into prompt targets, commands, touched code, failures, and closeout evidence
    SessionDigest {
        /// Path to the codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Read session transcript input from a file instead of stdin
        #[arg(long)]
        input: Option<PathBuf>,
        /// Force the transcript source (`markdown`, `claude-jsonl`, `codex-jsonl`, or `agent-doc-log`)
        #[arg(long)]
        source: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Summarize Claude/Codex token usage and agent-doc restart churn into bounded cost reports
    SessionCost {
        /// Read session transcript or agent-doc log input from a file instead of stdin
        #[arg(long)]
        input: Option<PathBuf>,
        /// Force the input source (`claude-jsonl`, `codex-jsonl`, or `agent-doc-log`)
        #[arg(long)]
        source: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Auto-discover related Claude/Codex/agent-doc logs for a document or repo path and aggregate one bounded review
    SessionReview {
        /// Target document or repo path to review (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Emit only the bounded resumable handoff pack instead of the full review
        #[arg(long)]
        next_context: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Preview-mode item cap for token-budgeted responses
        #[arg(long)]
        max_items: Option<usize>,
        /// Preview-mode per-field byte cap for token-budgeted responses
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Named preview budget preset (auto adapts from context-window env vars)
        #[arg(long, value_enum)]
        budget: Option<ResponseBudgetPreset>,
    },
    /// Report index + summary status and recommended commands for this session
    Status {
        /// Path to the codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Apply safe local fixes before reporting: refresh tsift instructions and rebuild stale/missing indexes
        #[arg(long)]
        fix: bool,
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

#[derive(Clone, Copy)]
struct OutputFormat {
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
    envelope: bool,
}

#[derive(Serialize)]
struct ToolEnvelopeMetric {
    label: String,
    value: String,
}

#[derive(Serialize)]
struct ToolEnvelopeSummary {
    text: String,
    metrics: Vec<ToolEnvelopeMetric>,
}

#[derive(Serialize)]
struct ToolEnvelope<'a, T: Serialize> {
    tool: &'a str,
    view: &'a str,
    summary: ToolEnvelopeSummary,
    truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    follow_up: Vec<String>,
    report: &'a T,
}

#[derive(Serialize)]
struct TranscriptArtifactRef {
    handle: String,
    path: String,
    bytes: usize,
    lines: usize,
    expand: String,
}

#[derive(Clone, Copy, Debug, Default)]
struct ResponseBudget {
    max_items: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ResponseBudgetPreset {
    Small,
    Normal,
    Deep,
    Auto,
}

impl ResponseBudget {
    fn new(max_items: Option<usize>, max_bytes: Option<usize>) -> Self {
        Self {
            max_items,
            max_bytes,
        }
    }

    fn from_cli(
        max_items: Option<usize>,
        max_bytes: Option<usize>,
        preset: Option<ResponseBudgetPreset>,
        envelope: bool,
    ) -> Self {
        let preset = preset.or_else(|| envelope.then_some(ResponseBudgetPreset::Auto));
        let Some(preset) = preset else {
            return Self::new(max_items, max_bytes);
        };

        let defaults = preset.resolve();
        Self::new(
            max_items.or(defaults.max_items),
            max_bytes.or(defaults.max_bytes),
        )
    }

    fn is_active(self) -> bool {
        self.max_items.is_some() || self.max_bytes.is_some()
    }

    fn preview_items(self) -> usize {
        self.max_items.unwrap_or(DEFAULT_BUDGET_ITEMS)
    }

    fn preview_bytes(self) -> usize {
        self.max_bytes.unwrap_or(DEFAULT_BUDGET_BYTES)
    }

    fn follow_up_items(self) -> usize {
        self.preview_items().max(DEFAULT_FOLLOW_UP_ITEMS)
    }
}

impl ResponseBudgetPreset {
    fn resolve(self) -> ResponseBudget {
        match self {
            ResponseBudgetPreset::Small => ResponseBudget::new(Some(3), Some(120)),
            ResponseBudgetPreset::Normal => {
                ResponseBudget::new(Some(DEFAULT_BUDGET_ITEMS), Some(DEFAULT_BUDGET_BYTES))
            }
            ResponseBudgetPreset::Deep => ResponseBudget::new(Some(10), Some(240)),
            ResponseBudgetPreset::Auto => adaptive_response_budget(),
        }
    }
}

fn adaptive_response_budget() -> ResponseBudget {
    let context_window = [
        "TSIFT_CONTEXT_WINDOW",
        "CODEX_CONTEXT_WINDOW",
        "CLAUDE_CONTEXT_WINDOW",
    ]
    .iter()
    .find_map(|key| {
        std::env::var(key)
            .ok()
            .and_then(|value| value.replace('_', "").parse::<usize>().ok())
    });

    match context_window {
        Some(window) if window <= 64_000 => ResponseBudgetPreset::Small.resolve(),
        Some(window) if window >= 200_000 => ResponseBudgetPreset::Deep.resolve(),
        _ => ResponseBudgetPreset::Normal.resolve(),
    }
}

struct MetricDigestOptions<'a> {
    input_path: Option<&'a Path>,
    baseline_path: Option<&'a Path>,
    metrics: &'a [String],
    lower_is_better: &'a [String],
    higher_is_better: &'a [String],
    history: usize,
    top: usize,
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
    let envelope = cli.envelope;
    match cli.command {
        Some(Commands::Search {
            query,
            path,
            limit,
            strategy,
            exact,
            scope,
            federated,
            json,
            autoindex,
            no_autoindex,
            timeout,
            max_items,
            max_bytes,
            budget,
        }) => cmd_search_with_budget(
            query,
            path,
            limit,
            if exact {
                Some("exact".to_string())
            } else {
                strategy
            },
            scope,
            federated,
            json || terse || schema || envelope,
            autoindex || !no_autoindex,
            timeout,
            compact,
            pretty,
            terse,
            absolute,
            tabular,
            schema,
            envelope,
            ResponseBudget::from_cli(max_items, max_bytes, budget, envelope),
        ),
        Some(Commands::SearchWorker {
            path,
            cache_dir,
            query,
            limit,
            strategy,
            output,
        }) => cmd_search_worker(&path, &cache_dir, &query, limit, &strategy, &output),
        Some(Commands::DigestRunner {
            kind,
            path,
            runner,
            shell_command,
            json,
        }) => cmd_digest_runner(
            &kind,
            &path,
            runner.as_deref(),
            &shell_command,
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
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
            json || terse || schema || envelope,
            compact,
            pretty,
            terse,
            absolute,
            schema,
        ),
        Some(Commands::Rewrite { command, run }) => cmd_rewrite(
            &command,
            run,
            OutputFormat {
                json_output: terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
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
            json || terse || schema || envelope,
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
            json || terse || schema || envelope,
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
            json || terse || schema || envelope,
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
            json || terse || schema || envelope,
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
            max_items,
            max_bytes,
            budget,
        }) => cmd_explain_with_budget(
            &symbol,
            &path,
            scope.as_deref(),
            limit,
            json || terse || schema || envelope,
            compact,
            pretty,
            terse,
            absolute,
            tabular,
            schema,
            envelope,
            ResponseBudget::from_cli(max_items, max_bytes, budget, envelope),
        ),
        Some(Commands::SourceRead {
            file,
            path,
            start,
            lines,
            end,
            scope,
            json,
            max_items,
            max_bytes,
            budget,
        }) => cmd_source_read(
            &file,
            &path,
            start,
            lines,
            end,
            scope.as_deref(),
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
            absolute,
            ResponseBudget::from_cli(max_items, max_bytes, budget, envelope),
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
            json || terse || schema || envelope,
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
            json || terse || schema || envelope,
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
            json || terse || schema || envelope,
            compact,
            pretty,
            terse,
            schema,
        ),
        Some(Commands::DiffDigest {
            path,
            cached,
            revision,
            json,
        }) => cmd_diff_digest(
            &path,
            cached,
            revision.as_deref(),
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
        Some(Commands::TestDigest {
            path,
            input,
            runner,
            json,
        }) => cmd_test_digest(
            &path,
            input.as_deref(),
            runner.as_deref(),
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
        Some(Commands::LogDigest { path, input, json }) => cmd_log_digest(
            &path,
            input.as_deref(),
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
        Some(Commands::ContextPack {
            path,
            test_input,
            runner,
            log_input,
            json,
            max_items,
            max_bytes,
            budget,
        }) => cmd_context_pack(
            &path,
            test_input.as_deref(),
            runner.as_deref(),
            log_input.as_deref(),
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
            ResponseBudget::from_cli(max_items, max_bytes, budget, envelope),
        ),
        Some(Commands::TokenSavings {
            fixture,
            fail_under,
            json,
        }) => cmd_token_savings(
            &fixture,
            fail_under,
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
        Some(Commands::MetricDigest {
            input,
            baseline,
            metrics,
            lower_is_better,
            higher_is_better,
            history,
            top,
            json,
        }) => cmd_metric_digest(
            MetricDigestOptions {
                input_path: input.as_deref(),
                baseline_path: baseline.as_deref(),
                metrics: &metrics,
                lower_is_better: &lower_is_better,
                higher_is_better: &higher_is_better,
                history,
                top,
            },
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
        Some(Commands::DciBenchmark { fixture, json }) => cmd_dci_benchmark(
            &fixture,
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
        Some(Commands::Workflow { topic, json }) => cmd_workflow(
            &topic,
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
        Some(Commands::SessionDigest {
            path,
            input,
            source,
            json,
        }) => cmd_session_digest(
            &path,
            input.as_deref(),
            source.as_deref(),
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
        Some(Commands::SessionCost {
            input,
            source,
            json,
        }) => cmd_session_cost(
            input.as_deref(),
            source.as_deref(),
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
        ),
        Some(Commands::SessionReview {
            path,
            next_context,
            json,
            max_items,
            max_bytes,
            budget,
        }) => cmd_session_review_with_budget(
            &path,
            next_context,
            OutputFormat {
                json_output: json || terse || schema || envelope,
                compact,
                pretty,
                terse,
                schema,
                envelope,
            },
            ResponseBudget::from_cli(max_items, max_bytes, budget, envelope),
        ),
        Some(Commands::Status { path, fix, json }) => cmd_status(
            &path,
            fix,
            json || terse || schema || envelope,
            compact,
            pretty,
            terse,
            schema,
        ),
        Some(Commands::Locks { path, scope, json }) => cmd_locks(
            &path,
            scope.as_deref(),
            json || terse || schema || envelope,
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

fn envelope_metric(label: &str, value: impl ToString) -> ToolEnvelopeMetric {
    ToolEnvelopeMetric {
        label: label.to_string(),
        value: value.to_string(),
    }
}

fn dedupe_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            deduped.push(value);
        }
    }
    deduped
}

fn print_json_or_envelope<T: Serialize>(
    report: &T,
    format: &OutputFormat,
    tool: &str,
    view: &str,
    summary: ToolEnvelopeSummary,
    truncated: bool,
    follow_up: Vec<String>,
) -> Result<()> {
    if format.envelope {
        let envelope = ToolEnvelope {
            tool,
            view,
            summary,
            truncated,
            follow_up: dedupe_preserve_order(follow_up),
            report,
        };
        println!(
            "{}",
            to_json_schema(&envelope, format.pretty, format.terse, format.schema)?
        );
    } else {
        println!(
            "{}",
            to_json_schema(report, format.pretty, format.terse, format.schema)?
        );
    }
    Ok(())
}

#[derive(Serialize)]
struct WorkflowStep {
    name: &'static str,
    goal: &'static str,
    command: &'static str,
    preserves: Vec<&'static str>,
    next: Vec<&'static str>,
}

#[derive(Serialize)]
struct WorkflowRecipe {
    topic: &'static str,
    summary: &'static str,
    handle_contract: Vec<&'static str>,
    steps: Vec<WorkflowStep>,
}

fn search_workflow_recipe() -> WorkflowRecipe {
    WorkflowRecipe {
        topic: "search",
        summary: "Chain exact search, semantic search, explain, summarize, and digest commands without dropping the stable handles emitted by each envelope.",
        handle_contract: vec![
            "Keep every handle with its originating command, query, path, and strategy.",
            "Use each step's expand command for deeper context, but cite the parent handle in notes and follow-up prompts.",
            "Prefer --envelope plus --budget normal when handing results to an agent so handles, follow_up commands, and truncation state stay machine-readable.",
        ],
        steps: vec![
            WorkflowStep {
                name: "exact-anchor",
                goal: "Start from a literal identifier, file path, error text, or prior handle label.",
                command: "tsift --envelope search \"<literal>\" --exact --path . --budget normal",
                preserves: vec![
                    "summary.handle",
                    "report.symbols[].handle",
                    "report.hits[].handle",
                ],
                next: vec![
                    "Run the matching report.symbols[].expand or report.hits[].expand command before broadening the query.",
                ],
            },
            WorkflowStep {
                name: "semantic-search",
                goal: "Broaden from the exact anchor to lexical, vector, or hybrid retrieval while keeping search-family handles.",
                command: "tsift --envelope search \"<concept>\" --path . --strategy hybrid --budget normal",
                preserves: vec![
                    "sfam-* symbol-family handles",
                    "shit-* content-hit handles",
                    "follow_up[]",
                ],
                next: vec![
                    "Use a symbol-family expand command for more search results, or pass the selected symbol name to explain.",
                ],
            },
            WorkflowStep {
                name: "explain-symbol",
                goal: "Expand a selected symbol into definitions, callers, callees, and community context.",
                command: "tsift --envelope explain \"<symbol>\" --path . --budget normal",
                preserves: vec![
                    "edef-* definition handles",
                    "ecall-* caller handles",
                    "eces-* callee handles",
                ],
                next: vec![
                    "Run edge expand commands for neighboring symbols, or summarize the selected symbol/file when the cache is available.",
                ],
            },
            WorkflowStep {
                name: "summarize-selection",
                goal: "Read cached summaries for the selected symbol or file without mutating the summary cache.",
                command: "tsift summarize \"<symbol>\" --path . --json",
                preserves: vec![
                    "summary refs emitted by search, explain, test-digest, log-digest, diff-digest, and context-pack",
                ],
                next: vec![
                    "If summaries are missing, run the status-recommended summarize --extract command outside the read-only query path.",
                ],
            },
            WorkflowStep {
                name: "digest-expansion",
                goal: "Expand from code navigation into changed files, tests, logs, or session context while retaining digest artifact handles.",
                command: "tsift --envelope context-pack <path> --test-input test.log --log-input build.log --budget normal",
                preserves: vec![
                    "artifact handles",
                    "touched symbol handles",
                    "digest summary handles",
                    "resume_commands[]",
                ],
                next: vec![
                    "Use resume_commands[] or each digest entry's expand command, and carry forward the original search/explain handle that motivated the digest.",
                ],
            },
        ],
    }
}

fn workflow_recipe(topic: &str) -> Result<WorkflowRecipe> {
    match topic {
        "search" | "search-handles" | "search-workflow" => Ok(search_workflow_recipe()),
        other => bail!("unknown workflow `{other}`; available workflows: search"),
    }
}

fn print_workflow_human(recipe: &WorkflowRecipe, compact: bool) {
    if compact {
        println!("workflow:{} steps:{}", recipe.topic, recipe.steps.len());
        for step in &recipe.steps {
            println!("  {} cmd:{}", step.name, step.command);
        }
        return;
    }

    println!("Workflow: {}", recipe.topic);
    println!("{}", recipe.summary);
    println!();
    println!("Handle contract:");
    for item in &recipe.handle_contract {
        println!("  - {item}");
    }
    println!();
    println!("Steps:");
    for (index, step) in recipe.steps.iter().enumerate() {
        println!("  {}. {} - {}", index + 1, step.name, step.goal);
        println!("     cmd: {}", step.command);
        println!("     preserves: {}", step.preserves.join(", "));
        println!("     next: {}", step.next.join(" "));
    }
}

fn cmd_workflow(topic: &str, format: OutputFormat) -> Result<()> {
    let recipe = workflow_recipe(topic)?;
    if format.json_output {
        print_json_or_envelope(
            &recipe,
            &format,
            "workflow",
            recipe.topic,
            ToolEnvelopeSummary {
                text: recipe.summary.to_string(),
                metrics: vec![envelope_metric("steps", recipe.steps.len())],
            },
            false,
            recipe
                .steps
                .iter()
                .map(|step| step.command.to_string())
                .collect(),
        )
    } else {
        print_workflow_human(&recipe, format.compact);
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
struct TokenSavingsFixture {
    schema_version: u64,
    #[serde(default)]
    description: String,
    token_estimate: String,
    cases: Vec<TokenSavingsFixtureCase>,
}

#[derive(Deserialize, Serialize)]
struct TokenSavingsFixtureCase {
    name: String,
    surface: String,
    minimum_savings_percent: f64,
    raw_symbols: Vec<TokenSavingsRawSymbol>,
    tagpath_families: Vec<TokenSavingsFamily>,
    #[serde(default)]
    session_review_inputs: Option<TokenSavingsSessionReviewInputs>,
    #[serde(default)]
    context_pack_inputs: Option<TokenSavingsContextPackInputs>,
}

#[derive(Deserialize, Serialize)]
struct TokenSavingsRawSymbol {
    identifier: String,
    file: String,
    line: u64,
    context: String,
}

#[derive(Deserialize, Serialize)]
struct TokenSavingsFamily {
    canonical: String,
    count: usize,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
struct TokenSavingsSessionReviewInputs {
    prompt_targets: Vec<serde_json::Value>,
    sessions: Vec<serde_json::Value>,
    commands: Vec<serde_json::Value>,
    touched_files: Vec<serde_json::Value>,
    touched_symbols: Vec<serde_json::Value>,
    failures: Vec<serde_json::Value>,
    guardrails: Vec<serde_json::Value>,
    largest_turns: Vec<serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
struct TokenSavingsContextPackInputs {
    next_context: Vec<serde_json::Value>,
    diff: Vec<serde_json::Value>,
    test: Vec<serde_json::Value>,
    log: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct TokenSavingsEnvelopeFamily {
    handle: String,
    tag_alias: String,
    count: usize,
    expand: String,
}

#[derive(Serialize)]
struct TokenSavingsSessionReviewEnvelope<'a> {
    section: &'a str,
    handle: String,
    count: usize,
    expand: String,
}

#[derive(Serialize)]
struct TokenSavingsContextPackEnvelope<'a> {
    section: &'a str,
    handle: String,
    count: usize,
    expand: String,
}

#[derive(Serialize)]
struct TokenSavingsCaseReport {
    name: String,
    surface: String,
    raw_symbol_count: usize,
    family_count: usize,
    raw_bytes: usize,
    envelope_bytes: usize,
    byte_delta: usize,
    raw_estimated_tokens: usize,
    envelope_estimated_tokens: usize,
    estimated_token_delta: usize,
    savings_percent: f64,
    minimum_savings_percent: f64,
    status: String,
}

#[derive(Serialize)]
struct TokenSavingsTotals {
    cases: usize,
    raw_bytes: usize,
    envelope_bytes: usize,
    byte_delta: usize,
    raw_estimated_tokens: usize,
    envelope_estimated_tokens: usize,
    estimated_token_delta: usize,
    savings_percent: f64,
}

#[derive(Serialize)]
struct TokenSavingsReport {
    schema_version: u64,
    token_estimate: String,
    pass: bool,
    totals: TokenSavingsTotals,
    cases: Vec<TokenSavingsCaseReport>,
}

fn estimated_tokens_from_bytes(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

fn savings_percent(raw_bytes: usize, envelope_bytes: usize) -> f64 {
    if raw_bytes == 0 || envelope_bytes >= raw_bytes {
        0.0
    } else {
        ((raw_bytes - envelope_bytes) as f64 / raw_bytes as f64) * 100.0
    }
}

fn token_savings_expand_command(surface: &str, canonical: &str) -> String {
    let query = canonical.replace('_', " ");
    match surface {
        "explain" => format!(
            "tsift --envelope explain {} --budget normal",
            shell_quote(canonical)
        ),
        "session-review" => format!("tsift summarize {}", shell_quote(canonical)),
        "context-pack" => {
            "tsift --envelope context-pack <target> --test-input <test.log> --log-input <build.log> --budget normal"
                .to_string()
        }
        _ => format!(
            "tsift --envelope search {} --budget normal",
            shell_quote(&query)
        ),
    }
}

fn token_savings_envelope_families(
    case: &TokenSavingsFixtureCase,
) -> Vec<TokenSavingsEnvelopeFamily> {
    case.tagpath_families
        .iter()
        .map(|family| {
            let key = format!("{}:{}:{}", case.surface, case.name, family.canonical);
            TokenSavingsEnvelopeFamily {
                handle: stable_handle("tfam", &key),
                tag_alias: family.canonical.replace('_', "/"),
                count: family.count,
                expand: token_savings_expand_command(&case.surface, &family.canonical),
            }
        })
        .collect()
}

fn token_savings_context_pack_raw_bytes(inputs: &TokenSavingsContextPackInputs) -> Result<usize> {
    Ok(serde_json::to_vec(inputs)?.len())
}

fn token_savings_session_review_raw_bytes(
    inputs: &TokenSavingsSessionReviewInputs,
) -> Result<usize> {
    Ok(serde_json::to_vec(inputs)?.len())
}

fn token_savings_session_review_envelope(
    case: &TokenSavingsFixtureCase,
    inputs: &TokenSavingsSessionReviewInputs,
) -> Vec<TokenSavingsSessionReviewEnvelope<'static>> {
    let mut rows = vec![
        TokenSavingsSessionReviewEnvelope {
            section: "prompt_targets",
            handle: stable_handle("tsr", &format!("{}:prompt_targets", case.name)),
            count: inputs.prompt_targets.len(),
            expand: "tsift session-review <target> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "sessions",
            handle: stable_handle("tsr", &format!("{}:sessions", case.name)),
            count: inputs.sessions.len(),
            expand: "tsift session-review <target> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "commands",
            handle: stable_handle("tsr", &format!("{}:commands", case.name)),
            count: inputs.commands.len(),
            expand: "tsift session-digest --source auto --input <transcript> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "files",
            handle: stable_handle("tsr", &format!("{}:files", case.name)),
            count: inputs.touched_files.len(),
            expand: "tsift session-review <target> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "symbols",
            handle: stable_handle("tsr", &format!("{}:symbols", case.name)),
            count: inputs.touched_symbols.len(),
            expand: "tsift --envelope search <symbol> --budget normal".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "failures",
            handle: stable_handle("tsr", &format!("{}:failures", case.name)),
            count: inputs.failures.len(),
            expand: "tsift session-review <target> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "guardrails",
            handle: stable_handle("tsr", &format!("{}:guardrails", case.name)),
            count: inputs.guardrails.len(),
            expand: "tsift session-cost --input <transcript> --json".to_string(),
        },
        TokenSavingsSessionReviewEnvelope {
            section: "largest_turns",
            handle: stable_handle("tsr", &format!("{}:largest_turns", case.name)),
            count: inputs.largest_turns.len(),
            expand: "tsift session-cost --input <transcript> --json".to_string(),
        },
    ];
    rows.retain(|row| row.count > 0);
    rows
}

fn token_savings_context_pack_envelope(
    case: &TokenSavingsFixtureCase,
    inputs: &TokenSavingsContextPackInputs,
) -> Vec<TokenSavingsContextPackEnvelope<'static>> {
    let mut rows = vec![
        TokenSavingsContextPackEnvelope {
            section: "next_context",
            handle: stable_handle("tcp", &format!("{}:next_context", case.name)),
            count: inputs.next_context.len(),
            expand: "tsift session-review --next-context <target> --json".to_string(),
        },
        TokenSavingsContextPackEnvelope {
            section: "diff",
            handle: stable_handle("tcp", &format!("{}:diff", case.name)),
            count: inputs.diff.len(),
            expand: "tsift diff-digest . --json".to_string(),
        },
        TokenSavingsContextPackEnvelope {
            section: "test",
            handle: stable_handle("tcp", &format!("{}:test", case.name)),
            count: inputs.test.len(),
            expand: "tsift test-digest --path . < test.log".to_string(),
        },
        TokenSavingsContextPackEnvelope {
            section: "log",
            handle: stable_handle("tcp", &format!("{}:log", case.name)),
            count: inputs.log.len(),
            expand: "tsift log-digest --path . < build.log".to_string(),
        },
    ];
    rows.retain(|row| row.count > 0);
    rows
}

fn build_token_savings_report(fixture: &TokenSavingsFixture) -> Result<TokenSavingsReport> {
    let mut cases = Vec::new();
    let mut total_raw_bytes = 0;
    let mut total_envelope_bytes = 0;

    for case in &fixture.cases {
        let mut raw_bytes = serde_json::to_vec(&case.raw_symbols)?.len();
        let envelope = token_savings_envelope_families(case);
        let mut envelope_bytes = serde_json::to_vec(&envelope)?.len();
        if let Some(inputs) = &case.session_review_inputs {
            raw_bytes += token_savings_session_review_raw_bytes(inputs)?;
            envelope_bytes +=
                serde_json::to_vec(&token_savings_session_review_envelope(case, inputs))?.len();
        }
        if let Some(inputs) = &case.context_pack_inputs {
            raw_bytes += token_savings_context_pack_raw_bytes(inputs)?;
            envelope_bytes +=
                serde_json::to_vec(&token_savings_context_pack_envelope(case, inputs))?.len();
        }
        let byte_delta = raw_bytes.saturating_sub(envelope_bytes);
        let raw_estimated_tokens = estimated_tokens_from_bytes(raw_bytes);
        let envelope_estimated_tokens = estimated_tokens_from_bytes(envelope_bytes);
        let estimated_token_delta = raw_estimated_tokens.saturating_sub(envelope_estimated_tokens);
        let savings_percent = savings_percent(raw_bytes, envelope_bytes);
        let pass = savings_percent >= case.minimum_savings_percent;

        total_raw_bytes += raw_bytes;
        total_envelope_bytes += envelope_bytes;
        cases.push(TokenSavingsCaseReport {
            name: case.name.clone(),
            surface: case.surface.clone(),
            raw_symbol_count: case.raw_symbols.len(),
            family_count: case.tagpath_families.len(),
            raw_bytes,
            envelope_bytes,
            byte_delta,
            raw_estimated_tokens,
            envelope_estimated_tokens,
            estimated_token_delta,
            savings_percent,
            minimum_savings_percent: case.minimum_savings_percent,
            status: if pass { "pass" } else { "fail" }.to_string(),
        });
    }

    let total_byte_delta = total_raw_bytes.saturating_sub(total_envelope_bytes);
    let total_raw_estimated_tokens = estimated_tokens_from_bytes(total_raw_bytes);
    let total_envelope_estimated_tokens = estimated_tokens_from_bytes(total_envelope_bytes);
    let total_estimated_token_delta =
        total_raw_estimated_tokens.saturating_sub(total_envelope_estimated_tokens);
    let pass = cases.iter().all(|case| case.status == "pass");

    Ok(TokenSavingsReport {
        schema_version: fixture.schema_version,
        token_estimate: fixture.token_estimate.clone(),
        pass,
        totals: TokenSavingsTotals {
            cases: cases.len(),
            raw_bytes: total_raw_bytes,
            envelope_bytes: total_envelope_bytes,
            byte_delta: total_byte_delta,
            raw_estimated_tokens: total_raw_estimated_tokens,
            envelope_estimated_tokens: total_envelope_estimated_tokens,
            estimated_token_delta: total_estimated_token_delta,
            savings_percent: savings_percent(total_raw_bytes, total_envelope_bytes),
        },
        cases,
    })
}

fn print_token_savings_human(report: &TokenSavingsReport) {
    println!(
        "surface\tcase\traw_bytes\tenvelope_bytes\tbyte_delta\traw_tokens\tenvelope_tokens\ttoken_delta\tsavings_percent\tminimum_percent\tstatus"
    );
    for case in &report.cases {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{}",
            case.surface,
            case.name,
            case.raw_bytes,
            case.envelope_bytes,
            case.byte_delta,
            case.raw_estimated_tokens,
            case.envelope_estimated_tokens,
            case.estimated_token_delta,
            case.savings_percent,
            case.minimum_savings_percent,
            case.status
        );
    }
    println!(
        "total\tall\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t-\t{}",
        report.totals.raw_bytes,
        report.totals.envelope_bytes,
        report.totals.byte_delta,
        report.totals.raw_estimated_tokens,
        report.totals.envelope_estimated_tokens,
        report.totals.estimated_token_delta,
        report.totals.savings_percent,
        if report.pass { "pass" } else { "fail" }
    );
}

fn cmd_token_savings(fixture_path: &Path, fail_under: bool, format: OutputFormat) -> Result<()> {
    let fixture_body = fs::read_to_string(fixture_path)
        .with_context(|| format!("reading token-savings fixture: {}", fixture_path.display()))?;
    let fixture: TokenSavingsFixture = serde_json::from_str(&fixture_body)
        .with_context(|| format!("parsing token-savings fixture: {}", fixture_path.display()))?;
    let report = build_token_savings_report(&fixture)?;

    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "token-savings",
            "report",
            ToolEnvelopeSummary {
                text: "token-savings report".to_string(),
                metrics: vec![
                    envelope_metric("cases", report.totals.cases),
                    envelope_metric("raw_tokens", report.totals.raw_estimated_tokens),
                    envelope_metric("envelope_tokens", report.totals.envelope_estimated_tokens),
                    envelope_metric("token_delta", report.totals.estimated_token_delta),
                    envelope_metric(
                        "savings_percent",
                        format!("{:.1}", report.totals.savings_percent),
                    ),
                ],
            },
            false,
            vec![],
        )?;
    } else {
        print_token_savings_human(&report);
    }

    if fail_under && !report.pass {
        bail!("token-savings threshold failed");
    }
    Ok(())
}

fn persist_transcript_artifact(
    root: &Path,
    prefix: &str,
    suffix: &str,
    key: &str,
    body: &str,
    expand: String,
) -> Result<TranscriptArtifactRef> {
    let handle = stable_handle(prefix, key);
    let artifacts_dir = root.join(".tsift/artifacts");
    fs::create_dir_all(&artifacts_dir).with_context(|| {
        format!(
            "creating transcript artifacts dir: {}",
            artifacts_dir.display()
        )
    })?;
    let file_name = format!("{handle}.{suffix}");
    let artifact_path = artifacts_dir.join(file_name);
    fs::write(&artifact_path, body)
        .with_context(|| format!("writing transcript artifact: {}", artifact_path.display()))?;
    let rel_path = relativize_pathbuf(&artifact_path, root);
    Ok(TranscriptArtifactRef {
        handle,
        path: rel_path.display().to_string(),
        bytes: body.len(),
        lines: body.lines().count(),
        expand,
    })
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
        "tool" => "tl",
        "view" => "vw",
        "truncated" => "tr",
        "follow_up" => "fu",
        "report" => "rp",
        "metrics" => "ms",
        "label" => "lb",
        "value" => "v",
        "command" => "cmd",
        "exit_code" => "xc",
        "success" => "ok",
        "artifact" => "art",
        "digest" => "dg",
        "bytes" => "bt",
        "lines" => "lns",
        "expand" => "xp",
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
    ("tool", "tl"),
    ("view", "vw"),
    ("truncated", "tr"),
    ("follow_up", "fu"),
    ("report", "rp"),
    ("metrics", "ms"),
    ("label", "lb"),
    ("value", "v"),
    ("command", "cmd"),
    ("exit_code", "xc"),
    ("success", "ok"),
    ("artifact", "art"),
    ("digest", "dg"),
    ("bytes", "bt"),
    ("lines", "lns"),
    ("expand", "xp"),
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

fn transcript_artifact_root(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    let start = if canonical.is_dir() {
        canonical.clone()
    } else {
        canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| canonical.clone())
    };

    for ancestor in start.ancestors() {
        if ancestor.join(".git").exists() || ancestor.join(".gitmodules").is_file() {
            return Ok(ancestor.to_path_buf());
        }
    }

    Ok(start)
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

const DEFAULT_BUDGET_ITEMS: usize = 5;
const DEFAULT_BUDGET_BYTES: usize = 160;
const DEFAULT_FOLLOW_UP_ITEMS: usize = 4;

fn stable_handle(prefix: &str, key: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(prefix.as_bytes());
    hasher.update(&[0]);
    hasher.update(key.as_bytes());
    let hex = hasher.finalize().to_hex();
    format!("{prefix}-{}", &hex[..10])
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CanonicalTagFamily {
    canonical: String,
    tag_alias: String,
}

fn canonical_family_from_tagpath_family(
    family: tagpath_family::TagFamily,
) -> Option<CanonicalTagFamily> {
    let tag_alias = if family.dimensions.is_empty() {
        family.tags.join("/")
    } else {
        family
            .dimensions
            .iter()
            .filter(|dimension| !dimension.tags.is_empty())
            .map(|dimension| dimension.tags.join("."))
            .collect::<Vec<_>>()
            .join("/")
    };

    if tag_alias.is_empty() {
        None
    } else {
        Some(CanonicalTagFamily {
            canonical: family.canonical,
            tag_alias,
        })
    }
}

fn canonical_tag_family_from_name(name: &str) -> Option<CanonicalTagFamily> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }

    canonical_family_from_tagpath_family(tagpath_family::generate_family(trimmed))
}

fn canonical_tag_family_from_tags(tags: &str) -> Option<CanonicalTagFamily> {
    let canonical = tags
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if canonical.is_empty() {
        None
    } else {
        canonical_family_from_tagpath_family(tagpath_family::generate_family(&canonical))
    }
}

fn canonical_tag_family_from_symbol(name: &str, tags: Option<&str>) -> Option<CanonicalTagFamily> {
    tags.and_then(canonical_tag_family_from_tags)
        .or_else(|| canonical_tag_family_from_name(name))
}

fn tag_alias_from_name(name: &str) -> Option<String> {
    canonical_tag_family_from_name(name).map(|family| family.tag_alias)
}

fn tag_alias_from_tags(name: &str, tags: Option<&str>) -> Option<String> {
    canonical_tag_family_from_symbol(name, tags).map(|family| family.tag_alias)
}

fn family_query_from_tag_alias(tag_alias: &str) -> Option<String> {
    let query = tag_alias
        .split(['/', '.'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if query.is_empty() { None } else { Some(query) }
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
struct CompactOntologyRefPreview {
    handle: String,
    tag: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
}

#[derive(Clone, Debug)]
struct TagOntologyPreviewContext {
    project_root: PathBuf,
    tags: BTreeMap<String, tagpath_ontology::OntologyTag>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
struct CompactSymbolRefPreview {
    handle: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_alias: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    ontology_refs: Vec<CompactOntologyRefPreview>,
}

fn build_compact_symbol_ref(
    prefix: &str,
    key: &str,
    name: &str,
    tags: Option<&str>,
    max_bytes: usize,
) -> CompactSymbolRefPreview {
    build_compact_symbol_ref_with_ontology(prefix, key, name, tags, max_bytes, None)
}

fn build_compact_symbol_ref_with_ontology(
    prefix: &str,
    key: &str,
    name: &str,
    tags: Option<&str>,
    max_bytes: usize,
    ontology: Option<&TagOntologyPreviewContext>,
) -> CompactSymbolRefPreview {
    let tag_alias = tag_alias_from_tags(name, tags);
    let ontology_refs = tag_alias
        .as_deref()
        .map(|alias| ontology_refs_for_alias(ontology, alias))
        .unwrap_or_default();
    CompactSymbolRefPreview {
        handle: stable_handle(prefix, key),
        name: truncate_for_budget(name, max_bytes),
        tag_alias: tag_alias.map(|alias| truncate_for_budget(&alias, max_bytes)),
        ontology_refs,
    }
}

fn load_tag_ontology_preview_context(root: &Path) -> Option<TagOntologyPreviewContext> {
    let report = tagpath_ontology::load_project(root).ok()?;
    if report.tags.is_empty() {
        return None;
    }
    Some(TagOntologyPreviewContext {
        project_root: report.project_path,
        tags: report
            .tags
            .into_iter()
            .map(|tag| (tag.tag.clone(), tag))
            .collect(),
    })
}

fn ontology_refs_for_alias(
    ontology: Option<&TagOntologyPreviewContext>,
    alias: &str,
) -> Vec<CompactOntologyRefPreview> {
    let Some(ontology) = ontology else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    alias
        .split('/')
        .flat_map(|part| part.split('.'))
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .filter_map(|tag| {
            let key = tag.to_ascii_lowercase();
            if !seen.insert(key.clone()) {
                return None;
            }
            let ontology_tag = ontology.tags.get(&key)?;
            let path = relativize_ontology_path(&ontology_tag.path, &ontology.project_root);
            Some(CompactOntologyRefPreview {
                handle: stable_handle("tont", &format!("{}:{path}", ontology_tag.tag)),
                tag: ontology_tag.tag.clone(),
                path,
                title: ontology_tag.title.clone(),
                domain: ontology_tag.domain.clone(),
            })
        })
        .collect()
}

fn relativize_ontology_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn format_symbol_preview_line(handle: &str, name: &str, tag_alias: Option<&str>) -> String {
    match tag_alias {
        Some(alias) => format!("{handle} {name} tag:{alias}"),
        None => format!("{handle} {name}"),
    }
}

fn format_summary_ref_line(summary: &ContextPackSummaryRefPreview) -> String {
    match summary.tag_alias.as_deref() {
        Some(alias) => format!(
            "{} {} tag:{} expand:{}",
            summary.handle, summary.symbol, alias, summary.expand
        ),
        None => format!(
            "{} {} expand:{}",
            summary.handle, summary.symbol, summary.expand
        ),
    }
}

fn compact_symbol_ref_token(symbol: &CompactSymbolRefPreview) -> String {
    match symbol.tag_alias.as_deref() {
        Some(alias) => format!("{}@{}", symbol.handle, alias),
        None => format!("{}@{}", symbol.handle, symbol.name),
    }
}

fn truncate_for_budget(input: &str, max_bytes: usize) -> String {
    let trimmed = input.trim();
    if trimmed.len() <= max_bytes {
        return trimmed.to_string();
    }
    if max_bytes <= 3 {
        return ".".repeat(max_bytes);
    }

    let mut end = 0usize;
    for (idx, ch) in trimmed.char_indices() {
        let next = idx + ch.len_utf8();
        if next > max_bytes.saturating_sub(3) {
            break;
        }
        end = next;
    }

    if end == 0 {
        "...".to_string()
    } else {
        format!("{}...", &trimmed[..end])
    }
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

const SEARCH_GROUP_SAMPLE_LIMIT: usize = 2;

struct SearchHitGroup {
    path: String,
    first_rank: usize,
    top_score: f64,
    confidence: String,
    hits: usize,
    samples: Vec<String>,
}

fn format_search_sample(hit: &sift::SearchHit) -> Option<String> {
    let snippet = compact_snippet(&hit.snippet)?;
    Some(match hit.location.as_deref() {
        Some(location) => format!("{location}: {snippet}"),
        None => snippet,
    })
}

fn group_search_hits(hits: &[sift::SearchHit], root: &Path, absolute: bool) -> Vec<SearchHitGroup> {
    let mut positions = BTreeMap::new();
    let mut groups = Vec::new();
    for hit in hits {
        let path = if absolute {
            hit.path.clone()
        } else {
            relativize(&hit.path, root)
        };
        let entry = positions.entry(path.clone()).or_insert_with(|| {
            groups.push(SearchHitGroup {
                path: path.clone(),
                first_rank: hit.rank,
                top_score: hit.score,
                confidence: format!("{:?}", hit.confidence),
                hits: 0,
                samples: Vec::new(),
            });
            groups.len() - 1
        });
        let group = &mut groups[*entry];
        group.hits += 1;
        if hit.rank < group.first_rank {
            group.first_rank = hit.rank;
        }
        if hit.score > group.top_score {
            group.top_score = hit.score;
        }
        if let Some(sample) = format_search_sample(hit)
            && group.samples.len() < SEARCH_GROUP_SAMPLE_LIMIT
            && !group.samples.contains(&sample)
        {
            group.samples.push(sample);
        }
    }
    groups.sort_by_key(|group| group.first_rank);
    groups
}

fn should_collapse_search_hits(hits: &[sift::SearchHit], root: &Path, absolute: bool) -> bool {
    let groups = group_search_hits(hits, root, absolute);
    let max_hits_per_file = groups.iter().map(|group| group.hits).max().unwrap_or(0);
    max_hits_per_file >= 3 || (hits.len() >= 6 && groups.len() < hits.len())
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
        .map(|(file, names)| format!("  {} ({}): {}", file, names.len(), names.join(", ")))
        .collect()
}

fn should_collapse_edge_groups(edges: &[index::StoredEdge]) -> bool {
    let mut grouped: BTreeMap<&str, usize> = BTreeMap::new();
    for edge in edges {
        *grouped.entry(edge.caller_file.as_str()).or_default() += 1;
    }
    let max_hits_per_file = grouped.values().copied().max().unwrap_or(0);
    max_hits_per_file >= 3 || (edges.len() >= 6 && grouped.len() < edges.len())
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

#[derive(Serialize)]
struct SourceLinePreview {
    line: usize,
    text: String,
}

#[derive(Serialize)]
struct SourceRangePreview {
    start: usize,
    end: usize,
    total_lines: usize,
    truncated_before: bool,
    truncated_after: bool,
}

#[derive(Serialize)]
struct SourceExpandCommands {
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<String>,
    file: String,
}

#[derive(Serialize)]
struct SourceSymbolRef {
    handle: String,
    name: String,
    kind: String,
    language: String,
    file: String,
    line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    expand: String,
}

#[derive(Serialize)]
struct SourceSummaryRef {
    handle: String,
    symbol_name: String,
    file_path: String,
    summary: String,
    expand: String,
}

#[derive(Serialize)]
struct SourceReadReport {
    handle: String,
    root: String,
    file: String,
    range: SourceRangePreview,
    preview: Vec<SourceLinePreview>,
    symbols: Vec<SourceSymbolRef>,
    summaries: Vec<SourceSummaryRef>,
    expand: SourceExpandCommands,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    warnings: Vec<String>,
}

fn resolve_source_file(root: &Path, file: &Path) -> Result<PathBuf> {
    let candidate = if file.is_absolute() {
        file.to_path_buf()
    } else {
        root.join(file)
    };
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("canonicalizing source file {}", candidate.display()))?;
    if !canonical.is_file() {
        bail!("source file is not a regular file: {}", canonical.display());
    }
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing project root {}", root.display()))?;
    if !canonical.starts_with(&canonical_root) {
        bail!(
            "source file {} is outside project root {}",
            canonical.display(),
            canonical_root.display()
        );
    }
    Ok(canonical)
}

fn source_read_command(root: &Path, file: &str, start: usize, lines: usize) -> String {
    format!(
        "tsift source-read {} --path {} --start {} --lines {} --budget normal",
        shell_quote(file),
        shell_quote(&root.to_string_lossy()),
        start,
        lines
    )
}

fn source_symbol_expand_command(root: &Path, symbol: &str) -> String {
    format!(
        "tsift --envelope explain {} --path {} --budget normal",
        shell_quote(symbol),
        shell_quote(&root.to_string_lossy())
    )
}

fn source_summary_expand_command(root: &Path, symbol: &str) -> String {
    format!(
        "tsift summarize {} --path {} --json",
        shell_quote(symbol),
        shell_quote(&root.to_string_lossy())
    )
}

fn source_symbol_line(symbol: &index::StoredSymbol) -> usize {
    usize::try_from(symbol.line)
        .ok()
        .and_then(|line| line.checked_add(1))
        .unwrap_or(1)
}

fn source_symbol_end_line(symbol: &index::StoredSymbol) -> Option<usize> {
    symbol
        .end_line
        .and_then(|line| usize::try_from(line).ok())
        .and_then(|line| line.checked_add(1))
}

fn source_symbol_intersects(symbol: &index::StoredSymbol, start: usize, end: usize) -> bool {
    if end == 0 {
        return false;
    }
    let symbol_start = source_symbol_line(symbol);
    let symbol_end = source_symbol_end_line(symbol).unwrap_or(symbol_start);
    symbol_start <= end && symbol_end >= start
}

#[allow(clippy::too_many_arguments)]
fn load_source_symbols(
    root: &Path,
    file_abs: &Path,
    file_display: &str,
    scope: Option<&str>,
    start: usize,
    end: usize,
    limit: usize,
    max_bytes: usize,
    warnings: &mut Vec<String>,
) -> Vec<SourceSymbolRef> {
    let db_path = match resolve_query_db_path(root, file_abs, scope) {
        Ok(path) => path,
        Err(err) => {
            warnings.push(format!("index refs unavailable: {err:#}"));
            return Vec::new();
        }
    };
    if !db_path.exists() {
        warnings.push(format!(
            "index refs unavailable: no index found at {}",
            db_path.display()
        ));
        return Vec::new();
    }

    let db = match index::IndexDb::open_read_only_resilient(&db_path) {
        Ok(db) => db,
        Err(err) => {
            warnings.push(format!("index refs unavailable: {err:#}"));
            return Vec::new();
        }
    };

    let file_key = file_abs.to_string_lossy().to_string();
    let symbols = match db.symbols_for_file(&file_key) {
        Ok(symbols) => symbols,
        Err(err) => {
            warnings.push(format!("symbol refs unavailable: {err:#}"));
            return Vec::new();
        }
    };

    symbols
        .into_iter()
        .filter(|symbol| source_symbol_intersects(symbol, start, end))
        .take(limit)
        .map(|symbol| {
            let line = source_symbol_line(&symbol);
            let end_line = source_symbol_end_line(&symbol);
            let handle = stable_handle(
                "ssym",
                &format!("{}:{}:{}", file_display, symbol.name, line),
            );
            SourceSymbolRef {
                handle,
                name: truncate_for_budget(&symbol.name, max_bytes),
                kind: symbol.kind,
                language: symbol.language,
                file: file_display.to_string(),
                line,
                end_line,
                signature: symbol
                    .signature
                    .map(|signature| truncate_for_budget(&signature, max_bytes)),
                expand: source_symbol_expand_command(root, &symbol.name),
            }
        })
        .collect()
}

fn load_source_summaries(
    root: &Path,
    file_display: &str,
    limit: usize,
    max_bytes: usize,
    warnings: &mut Vec<String>,
) -> Vec<SourceSummaryRef> {
    let db_path = root.join(".tsift/summaries.db");
    if !db_path.exists() {
        return Vec::new();
    }
    let db = match summarize::SummaryDb::open_read_only_resilient(&db_path) {
        Ok(db) => db,
        Err(err) => {
            warnings.push(format!("summary refs unavailable: {err:#}"));
            return Vec::new();
        }
    };
    let summaries = match db.get_by_file(file_display) {
        Ok(summaries) => summaries,
        Err(err) => {
            warnings.push(format!("summary refs unavailable: {err:#}"));
            return Vec::new();
        }
    };

    summaries
        .into_iter()
        .take(limit)
        .map(|summary| SourceSummaryRef {
            handle: stable_handle(
                "sum",
                &format!(
                    "{}:{}:{}",
                    summary.file_path, summary.symbol_name, summary.id
                ),
            ),
            symbol_name: truncate_for_budget(&summary.symbol_name, max_bytes),
            file_path: summary.file_path,
            summary: truncate_for_budget(&summary.summary, max_bytes),
            expand: source_summary_expand_command(root, &summary.symbol_name),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn cmd_source_read(
    file: &Path,
    path: &Path,
    start: usize,
    lines: usize,
    end: Option<usize>,
    scope: Option<&str>,
    format: OutputFormat,
    absolute: bool,
    budget: ResponseBudget,
) -> Result<()> {
    if start == 0 {
        bail!("--start is 1-based and must be greater than zero");
    }
    if lines == 0 {
        bail!("--lines must be greater than zero");
    }
    if let Some(end) = end
        && end < start
    {
        bail!("--end must be greater than or equal to --start");
    }

    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let file_abs = resolve_source_file(&root, file)?;
    let file_display = if absolute {
        file_abs.to_string_lossy().to_string()
    } else {
        relativize_pathbuf(&file_abs, &root)
            .to_string_lossy()
            .to_string()
    };

    let source = fs::read(&file_abs).with_context(|| format!("reading {}", file_abs.display()))?;
    let text = String::from_utf8_lossy(&source);
    let all_lines: Vec<&str> = text.lines().collect();
    let total_lines = all_lines.len();
    if total_lines > 0 && start > total_lines {
        bail!(
            "--start {} is beyond end of {} ({} lines)",
            start,
            file_display,
            total_lines
        );
    }
    let requested_end = end.unwrap_or_else(|| start.saturating_add(lines).saturating_sub(1));
    let end_line = requested_end.min(total_lines);
    let max_bytes = budget.preview_bytes();
    let preview = if total_lines == 0 {
        Vec::new()
    } else {
        all_lines[(start - 1)..end_line]
            .iter()
            .enumerate()
            .map(|(idx, line)| SourceLinePreview {
                line: start + idx,
                text: truncate_for_budget(line, max_bytes),
            })
            .collect()
    };

    let mut warnings = Vec::new();
    let max_items = budget.preview_items();
    let symbols = load_source_symbols(
        &root,
        &file_abs,
        &file_display,
        scope,
        start,
        end_line,
        max_items,
        max_bytes,
        &mut warnings,
    );
    let summaries =
        load_source_summaries(&root, &file_display, max_items, max_bytes, &mut warnings);

    let effective_lines = end_line.saturating_sub(start).saturating_add(1).max(1);
    let expand = SourceExpandCommands {
        before: (start > 1).then(|| {
            let before_start = start.saturating_sub(lines).max(1);
            source_read_command(&root, &file_display, before_start, start - before_start)
        }),
        after: (end_line < total_lines)
            .then(|| source_read_command(&root, &file_display, end_line + 1, lines)),
        file: source_read_command(&root, &file_display, 1, total_lines.max(effective_lines)),
    };

    let report = SourceReadReport {
        handle: stable_handle("swin", &format!("{file_display}:{start}:{end_line}")),
        root: root.to_string_lossy().to_string(),
        file: file_display,
        range: SourceRangePreview {
            start,
            end: end_line,
            total_lines,
            truncated_before: start > 1,
            truncated_after: end_line < total_lines,
        },
        preview,
        symbols,
        summaries,
        expand,
        warnings,
    };

    if format.json_output {
        let truncated = report.range.truncated_before || report.range.truncated_after;
        let follow_up = [
            report.expand.before.clone(),
            report.expand.after.clone(),
            Some(report.expand.file.clone()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        print_json_or_envelope(
            &report,
            &format,
            "source-read",
            "window",
            ToolEnvelopeSummary {
                text: format!(
                    "source window {}:{}-{}",
                    report.file, report.range.start, report.range.end
                ),
                metrics: vec![
                    envelope_metric("lines", report.preview.len()),
                    envelope_metric("symbols", report.symbols.len()),
                    envelope_metric("summaries", report.summaries.len()),
                ],
            },
            truncated,
            follow_up,
        )?;
    } else if format.compact {
        println!(
            "source {}:{}-{} / {} handle:{}",
            report.file,
            report.range.start,
            report.range.end,
            report.range.total_lines,
            report.handle
        );
        for line in &report.preview {
            println!("{:>5} {}", line.line, line.text);
        }
        if !report.symbols.is_empty() {
            println!("syms[{}]:", report.symbols.len());
            for symbol in &report.symbols {
                println!("  {} {}:{}", symbol.name, symbol.file, symbol.line);
            }
        }
        if report.range.truncated_before || report.range.truncated_after {
            println!("expand: {}", report.expand.file);
        }
    } else {
        println!(
            "Source window `{}` lines {}-{} of {} ({})",
            report.file,
            report.range.start,
            report.range.end,
            report.range.total_lines,
            report.handle
        );
        for line in &report.preview {
            println!("{:>5} | {}", line.line, line.text);
        }
        if !report.symbols.is_empty() {
            println!();
            println!("Symbol refs:");
            for symbol in &report.symbols {
                println!(
                    "  {} `{}` {}:{} — {}",
                    symbol.handle, symbol.name, symbol.file, symbol.line, symbol.expand
                );
            }
        }
        if !report.summaries.is_empty() {
            println!();
            println!("Summary refs:");
            for summary in &report.summaries {
                println!(
                    "  {} `{}` — {}",
                    summary.handle, summary.symbol_name, summary.expand
                );
            }
        }
        if report.range.truncated_before || report.range.truncated_after {
            println!();
            println!("Expand:");
            if let Some(before) = &report.expand.before {
                println!("  before: {}", before);
            }
            if let Some(after) = &report.expand.after {
                println!("  after: {}", after);
            }
            println!("  file:   {}", report.expand.file);
        }
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
    }

    Ok(())
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
#[allow(dead_code)]
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
    cmd_explain_with_budget(
        symbol,
        path,
        scope,
        limit,
        json_output,
        compact,
        pretty,
        terse,
        absolute,
        tabular,
        schema,
        false,
        ResponseBudget::default(),
    )
}

#[allow(clippy::too_many_arguments)]
fn cmd_explain_with_budget(
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
    envelope: bool,
    budget: ResponseBudget,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let format = OutputFormat {
        json_output,
        compact,
        pretty,
        terse,
        schema,
        envelope,
    };
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

    if budget.is_active() {
        let report = build_explain_budget_report(
            symbol,
            &root,
            &symbols,
            &callers,
            callers_total,
            callers_truncated,
            &callees,
            callees_total,
            callees_truncated,
            community,
            budget,
        );
        if format.json_output {
            print_json_or_envelope(
                &report,
                &format,
                "explain",
                "preview",
                ToolEnvelopeSummary {
                    text: format!("explain preview for {}", symbol),
                    metrics: vec![
                        envelope_metric("definitions", report.definition_total),
                        envelope_metric("callers", report.callers_total),
                        envelope_metric("callees", report.callees_total),
                    ],
                },
                report.truncated,
                vec![format!(
                    "tsift explain {} --path {} --limit 0{}",
                    shell_quote(symbol),
                    shell_quote(path.to_string_lossy().as_ref()),
                    scope
                        .map(|value| format!(" --scope {}", shell_quote(value)))
                        .unwrap_or_default()
                )],
            )?;
        } else {
            print_explain_budget_human(&report);
        }
    } else if format.json_output {
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
        print_json_or_envelope(
            &out,
            &format,
            "explain",
            "report",
            ToolEnvelopeSummary {
                text: format!("explain results for {}", symbol),
                metrics: vec![
                    envelope_metric("definitions", symbols.len()),
                    envelope_metric("callers", callers_total),
                    envelope_metric("callees", callees_total),
                ],
            },
            callers_truncated || callees_truncated,
            vec![format!(
                "tsift explain {} --path {} --limit 0{}",
                shell_quote(symbol),
                shell_quote(path.to_string_lossy().as_ref()),
                scope
                    .map(|value| format!(" --scope {}", shell_quote(value)))
                    .unwrap_or_default()
            )],
        )?;
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
        } else if should_collapse_edge_groups(&callers) {
            for line in format_edge_groups(&callers, true) {
                println!("{line}");
            }
            if callers_truncated {
                println!(
                    "  (+{} more callers, use --limit 0 to show all)",
                    callers_total - limit
                );
            }
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
        } else if should_collapse_edge_groups(&callees) {
            for line in format_edge_groups(&callees, false) {
                println!("{line}");
            }
            if callees_truncated {
                println!(
                    "  (+{} more callees, use --limit 0 to show all)",
                    callees_total - limit
                );
            }
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

#[derive(Serialize)]
struct ExplainBudgetDefinitionPreview {
    handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_alias: Option<String>,
    kind: String,
    name: String,
    file: String,
    line: i64,
    expand: String,
}

#[derive(Serialize)]
struct ExplainBudgetEdgePreview {
    handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_alias: Option<String>,
    name: String,
    file: String,
    line: i64,
    expand: String,
}

#[derive(Serialize)]
struct ExplainBudgetCommunityPreview {
    size: usize,
    members: Vec<String>,
}

#[derive(Serialize)]
struct ExplainBudgetReport {
    symbol: String,
    max_items: usize,
    max_bytes: usize,
    definition_total: usize,
    callers_total: usize,
    callers_truncated_by_limit: bool,
    callees_total: usize,
    callees_truncated_by_limit: bool,
    truncated: bool,
    definitions: Vec<ExplainBudgetDefinitionPreview>,
    callers: Vec<ExplainBudgetEdgePreview>,
    callees: Vec<ExplainBudgetEdgePreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    community: Option<ExplainBudgetCommunityPreview>,
}

#[allow(clippy::too_many_arguments)]
fn build_explain_budget_report(
    symbol: &str,
    _root: &Path,
    symbols: &[index::StoredSymbol],
    callers: &[index::StoredEdge],
    callers_total: usize,
    callers_truncated_by_limit: bool,
    callees: &[index::StoredEdge],
    callees_total: usize,
    callees_truncated_by_limit: bool,
    community: Option<&graph::Community>,
    budget: ResponseBudget,
) -> ExplainBudgetReport {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    let definitions = symbols
        .iter()
        .take(max_items)
        .map(|entry| {
            let symbol_ref = build_compact_symbol_ref(
                "edef",
                &format!(
                    "{}:{}:{}:{}",
                    entry.kind, entry.name, entry.file, entry.line
                ),
                &entry.name,
                entry.tags.as_deref(),
                max_bytes,
            );
            ExplainBudgetDefinitionPreview {
                handle: symbol_ref.handle,
                tag_alias: symbol_ref.tag_alias,
                kind: entry.kind.clone(),
                name: symbol_ref.name,
                file: truncate_for_budget(&entry.file, max_bytes),
                line: entry.line,
                expand: format!(
                    "tsift search {} --exact --path {} --limit 20",
                    shell_quote(&entry.name),
                    shell_quote(&entry.file)
                ),
            }
        })
        .collect();
    let callers_preview: Vec<ExplainBudgetEdgePreview> = callers
        .iter()
        .take(max_items)
        .map(|entry| {
            let symbol_ref = build_compact_symbol_ref(
                "ecall",
                &format!(
                    "{}:{}:{}:{}",
                    entry.caller_name, entry.caller_file, entry.call_site_line, symbol
                ),
                &entry.caller_name,
                None,
                max_bytes,
            );
            ExplainBudgetEdgePreview {
                handle: symbol_ref.handle,
                tag_alias: symbol_ref.tag_alias,
                name: symbol_ref.name,
                file: truncate_for_budget(&entry.caller_file, max_bytes),
                line: entry.call_site_line,
                expand: format!(
                    "tsift explain {} --path {} --limit 0",
                    shell_quote(&entry.caller_name),
                    shell_quote(&entry.caller_file)
                ),
            }
        })
        .collect();
    let callees_preview: Vec<ExplainBudgetEdgePreview> = callees
        .iter()
        .take(max_items)
        .map(|entry| {
            let symbol_ref = build_compact_symbol_ref(
                "eces",
                &format!(
                    "{}:{}:{}:{}",
                    entry.callee_name, entry.caller_file, entry.call_site_line, symbol
                ),
                &entry.callee_name,
                None,
                max_bytes,
            );
            ExplainBudgetEdgePreview {
                handle: symbol_ref.handle,
                tag_alias: symbol_ref.tag_alias,
                name: symbol_ref.name,
                file: truncate_for_budget(&entry.caller_file, max_bytes),
                line: entry.call_site_line,
                expand: format!(
                    "tsift explain {} --path {} --limit 0",
                    shell_quote(&entry.callee_name),
                    shell_quote(&entry.caller_file)
                ),
            }
        })
        .collect();
    let community_preview = community.map(|entry| ExplainBudgetCommunityPreview {
        size: entry.members.len(),
        members: entry
            .members
            .iter()
            .take(max_items)
            .map(|member| truncate_for_budget(member, max_bytes))
            .collect(),
    });

    ExplainBudgetReport {
        symbol: symbol.to_string(),
        max_items,
        max_bytes,
        definition_total: symbols.len(),
        callers_total,
        callers_truncated_by_limit,
        callees_total,
        callees_truncated_by_limit,
        truncated: symbols.len() > max_items
            || callers_total > callers_preview.len()
            || callees_total > callees_preview.len()
            || community
                .map(|entry| entry.members.len() > max_items)
                .unwrap_or(false),
        definitions,
        callers: callers_preview,
        callees: callees_preview,
        community: community_preview,
    }
}

fn print_explain_budget_human(report: &ExplainBudgetReport) {
    println!(
        "explain-budget sym:{} defs:{}/{} crs:{}/{} ces:{}/{}",
        shell_quote(&report.symbol),
        report.definitions.len(),
        report.definition_total,
        report.callers.len(),
        report.callers_total,
        report.callees.len(),
        report.callees_total
    );
    for entry in &report.definitions {
        println!(
            "def {} {} {}:{} expand:{}",
            format_symbol_preview_line(&entry.handle, &entry.name, entry.tag_alias.as_deref()),
            entry.kind,
            entry.file,
            entry.line,
            entry.expand
        );
    }
    for entry in &report.callers {
        println!(
            "caller {} {}:{} expand:{}",
            format_symbol_preview_line(&entry.handle, &entry.name, entry.tag_alias.as_deref()),
            entry.file,
            entry.line,
            entry.expand
        );
    }
    for entry in &report.callees {
        println!(
            "callee {} {}:{} expand:{}",
            format_symbol_preview_line(&entry.handle, &entry.name, entry.tag_alias.as_deref()),
            entry.file,
            entry.line,
            entry.expand
        );
    }
    if let Some(community) = &report.community {
        println!(
            "community size:{} members:{}",
            community.size,
            community.members.join(", ")
        );
    }
    if report.truncated {
        println!(
            "budget truncated items:{} bytes:{}",
            report.max_items, report.max_bytes
        );
    }
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
        let extract_scope = resolve_extract_scope(&extract_base, &extract_path)?;
        let cfg = load_summarize_config(&root);

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
            if existing.is_empty() && deleted_summary_paths.is_empty() {
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
        let summary_db = summarize::SummaryDb::open(&db_path)?;

        if !diff {
            deleted_summary_paths.extend(summarize_full_extract_deleted_summary_paths(
                &summary_db,
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
            let rel_path = summarize_relative_file_path(&root, file_path);

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
        let s = summary_db.stats(&root)?;
        if json_output {
            println!("{}", to_json_schema(&s, pretty, terse, schema)?);
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

fn diff_digest_status_label(status: diff_digest::DiffDigestFileStatus) -> &'static str {
    match status {
        diff_digest::DiffDigestFileStatus::Added => "added",
        diff_digest::DiffDigestFileStatus::Modified => "modified",
        diff_digest::DiffDigestFileStatus::Deleted => "deleted",
    }
}

fn diff_digest_summary_label(state: diff_digest::DiffDigestSummaryState) -> &'static str {
    match state {
        diff_digest::DiffDigestSummaryState::Current => "current",
        diff_digest::DiffDigestSummaryState::Stale => "stale",
        diff_digest::DiffDigestSummaryState::Missing => "missing",
        diff_digest::DiffDigestSummaryState::Unavailable => "unavailable",
    }
}

fn test_digest_summary_label(state: test_digest::TestDigestSummaryState) -> &'static str {
    match state {
        test_digest::TestDigestSummaryState::Current => "current",
        test_digest::TestDigestSummaryState::Stale => "stale",
        test_digest::TestDigestSummaryState::Missing => "missing",
        test_digest::TestDigestSummaryState::Unavailable => "unavailable",
    }
}

fn log_digest_summary_label(state: log_digest::LogDigestSummaryState) -> &'static str {
    match state {
        log_digest::LogDigestSummaryState::Current => "current",
        log_digest::LogDigestSummaryState::Stale => "stale",
        log_digest::LogDigestSummaryState::Missing => "missing",
        log_digest::LogDigestSummaryState::Unavailable => "unavailable",
    }
}

fn cmd_diff_digest(
    path: &Path,
    cached: bool,
    revision: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let report = diff_digest::compute(path, diff_digest::DiffDigestOptions { cached, revision })?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(&report, format.pretty, format.terse, format.schema)?
        );
        return Ok(());
    }

    if report.files.is_empty() {
        println!("{}", diff_digest_empty_message(&report));
        return Ok(());
    }

    if format.compact {
        println!(
            "diff mode:{} files:{} summaries:{} syms:{} edges:+{}/-{}",
            diff_digest_mode_label(report.mode),
            report.files_changed,
            report.files_with_current_summaries,
            report.symbols_touched,
            report.call_edges_added,
            report.call_edges_removed
        );
        for file in &report.files {
            let symbols = if file.touched_symbols.is_empty() {
                "-".to_string()
            } else {
                truncate_for_compact(&file.touched_symbols.join(","), 60)
            };
            println!(
                "{} status:{} syms:{} sums:{} edges:+{}/-{}",
                file.path,
                diff_digest_status_label(file.status),
                symbols,
                diff_digest_summary_label(file.summary_state),
                file.added_call_edges.len(),
                file.removed_call_edges.len()
            );
        }
        return Ok(());
    }

    println!("Diff digest ({})", diff_digest_mode_display(&report));
    println!("  files changed:                 {}", report.files_changed);
    println!(
        "  files with current summaries: {}",
        report.files_with_current_summaries
    );
    println!("  touched symbols:              {}", report.symbols_touched);
    println!(
        "  call edges:                   +{} / -{}",
        report.call_edges_added, report.call_edges_removed
    );

    for file in &report.files {
        println!();
        println!("{} [{}]", file.path, diff_digest_status_label(file.status));
        if file.touched_symbols.is_empty() {
            println!("  touched symbols: none");
        } else {
            println!("  touched symbols: {}", file.touched_symbols.join(", "));
        }
        println!(
            "  cached summaries: {}",
            diff_digest_summary_label(file.summary_state)
        );
        for summary in &file.current_summaries {
            println!(
                "    - {}: {}",
                summary.symbol,
                truncate_for_compact(&summary.summary, 160)
            );
        }
        if !file.added_call_edges.is_empty() {
            println!("  call edges added:");
            for edge in &file.added_call_edges {
                println!("    - {}", edge);
            }
        }
        if !file.removed_call_edges.is_empty() {
            println!("  call edges removed:");
            for edge in &file.removed_call_edges {
                println!("    - {}", edge);
            }
        }
        for warning in &file.warnings {
            println!("  warning: {}", warning);
        }
    }

    Ok(())
}

fn diff_digest_mode_label(mode: diff_digest::DiffDigestMode) -> &'static str {
    match mode {
        diff_digest::DiffDigestMode::WorkingTree => "worktree",
        diff_digest::DiffDigestMode::Cached => "cached",
        diff_digest::DiffDigestMode::Revision => "revision",
    }
}

fn diff_digest_mode_display(report: &diff_digest::DiffDigestReport) -> String {
    match (&report.mode, &report.revision) {
        (diff_digest::DiffDigestMode::WorkingTree, _) => "working tree".to_string(),
        (diff_digest::DiffDigestMode::Cached, _) => "staged index".to_string(),
        (diff_digest::DiffDigestMode::Revision, Some(revision)) => {
            format!("revision {revision}")
        }
        (diff_digest::DiffDigestMode::Revision, None) => "revision".to_string(),
    }
}

fn diff_digest_empty_message(report: &diff_digest::DiffDigestReport) -> String {
    match (&report.mode, &report.revision) {
        (diff_digest::DiffDigestMode::WorkingTree, _) => "No git changes found.".to_string(),
        (diff_digest::DiffDigestMode::Cached, _) => "No staged git changes found.".to_string(),
        (diff_digest::DiffDigestMode::Revision, Some(revision)) => {
            format!("No diff found for revision {revision}.")
        }
        (diff_digest::DiffDigestMode::Revision, None) => "No revision diff found.".to_string(),
    }
}

fn cmd_test_digest(
    path: &Path,
    input_path: Option<&Path>,
    runner: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let input = match input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading test output: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading test output from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!("no test output provided; pass --input <file> or pipe runner output on stdin");
    }

    render_test_digest_from_input(path, &input, runner, format)
}

fn render_test_digest_from_input(
    path: &Path,
    input: &str,
    runner: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let report = test_digest::compute(path, input, runner)?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(&report, format.pretty, format.terse, format.schema)?
        );
        return Ok(());
    }

    if report.failure_groups.is_empty() {
        println!("No failures detected (runner: {}).", report.runner);
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    if format.compact {
        println!(
            "test runner:{} failures:{} groups:{} passed:{} failed:{} skipped:{}",
            report.runner,
            report.failures,
            report.grouped_failures,
            report.counts.passed.unwrap_or(0),
            report.counts.failed.unwrap_or(report.grouped_failures),
            report.counts.skipped.unwrap_or(0),
        );
        for failure in &report.failure_groups {
            let tests = truncate_for_compact(&failure.tests.join(","), 60);
            let location = match (&failure.path, failure.line) {
                (Some(path), Some(line)) => format!("{path}:{line}"),
                (Some(path), None) => path.clone(),
                _ => "-".to_string(),
            };
            println!(
                "{} tests:{} count:{} summaries:{} msg:{}",
                location,
                tests,
                failure.occurrences,
                test_digest_summary_label(failure.summary_state),
                truncate_for_compact(&failure.message, 80)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Test digest ({})", report.runner);
    println!("  failures:        {}", report.failures);
    println!("  failure groups:  {}", report.grouped_failures);
    if let Some(passed) = report.counts.passed {
        println!("  passed:          {}", passed);
    }
    if let Some(failed) = report.counts.failed {
        println!("  failed:          {}", failed);
    }
    if let Some(skipped) = report.counts.skipped {
        println!("  skipped:         {}", skipped);
    }

    for failure in &report.failure_groups {
        println!();
        match (&failure.path, failure.line, failure.column) {
            (Some(path), Some(line), Some(column)) => println!("{path}:{line}:{column}"),
            (Some(path), Some(line), None) => println!("{path}:{line}"),
            (Some(path), None, _) => println!("{path}"),
            (None, _, _) => println!("(no file anchor)"),
        }
        println!("  tests: {}", failure.tests.join(", "));
        println!("  occurrences: {}", failure.occurrences);
        println!("  message: {}", failure.message);
        println!(
            "  cached summaries: {}",
            test_digest_summary_label(failure.summary_state)
        );
        for summary in &failure.current_summaries {
            println!(
                "    - {}: {}",
                summary.symbol,
                truncate_for_compact(&summary.summary, 160)
            );
        }
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

fn cmd_log_digest(path: &Path, input_path: Option<&Path>, format: OutputFormat) -> Result<()> {
    let input = match input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading log output: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading log output from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!("no log output provided; pass --input <file> or pipe log output on stdin");
    }

    render_log_digest_from_input(path, &input, format)
}

fn cmd_context_pack(
    path: &Path,
    test_input: Option<&Path>,
    runner: Option<&str>,
    log_input: Option<&Path>,
    format: OutputFormat,
    budget: ResponseBudget,
) -> Result<()> {
    let report = build_context_pack_report(path, test_input, runner, log_input, budget)?;
    if format.json_output {
        print_json_or_envelope(
            &report,
            &format,
            "context-pack",
            "handoff",
            ToolEnvelopeSummary {
                text: format!("context pack for {}", report.target),
                metrics: vec![
                    envelope_metric("prompt_targets", report.next_context.prompt_target_total),
                    envelope_metric("files_changed", report.diff_digest.files_changed),
                    envelope_metric("test", &report.test_digest.status),
                    envelope_metric("log", &report.log_digest.status),
                ],
            },
            report.next_context.truncated
                || report.diff_digest.truncated
                || report
                    .test_digest
                    .report
                    .as_ref()
                    .map(|entry| entry.truncated)
                    .unwrap_or(false)
                || report
                    .log_digest
                    .report
                    .as_ref()
                    .map(|entry| entry.truncated)
                    .unwrap_or(false),
            report.resume_commands.clone(),
        )?;
        return Ok(());
    }

    print_context_pack_human(&report, format.compact);
    Ok(())
}

fn render_log_digest_from_input(path: &Path, input: &str, format: OutputFormat) -> Result<()> {
    let report = log_digest::compute(path, input)?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(&report, format.pretty, format.terse, format.schema)?
        );
        return Ok(());
    }

    if format.compact {
        println!(
            "log lines:{} signals:{} repeats:{} files:{} syms:{} stacks:{}",
            report.non_empty_lines,
            report.signal_groups,
            report.repeated_line_groups,
            report.file_ref_groups,
            report.symbol_ref_groups,
            report.stack_groups
        );
        for signal in &report.signals {
            let location = match (&signal.path, signal.line) {
                (Some(path), Some(line)) => format!("{path}:{line}"),
                (Some(path), None) => path.clone(),
                _ => "-".to_string(),
            };
            println!(
                "{} sev:{} count:{} sums:{} msg:{}",
                location,
                signal.severity,
                signal.occurrences,
                log_digest_summary_label(signal.summary_state),
                truncate_for_compact(&signal.message, 80)
            );
        }
        for repeated in &report.repeated_lines {
            println!(
                "repeat count:{} line:{}",
                repeated.occurrences,
                truncate_for_compact(&repeated.line, 80)
            );
        }
        for symbol in &report.symbol_refs {
            println!(
                "sym:{} count:{} sums:{}",
                symbol.symbol,
                symbol.occurrences,
                log_digest_summary_label(symbol.summary_state)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Log digest");
    println!("  lines:                    {}", report.total_lines);
    println!("  non-empty lines:          {}", report.non_empty_lines);
    println!("  signal groups:            {}", report.signal_groups);
    println!(
        "  repeated lines:           {}",
        report.repeated_line_groups
    );
    println!(
        "  repeated line instances:  {}",
        report.repeated_line_occurrences
    );
    println!("  file refs:                {}", report.file_ref_groups);
    println!("  symbol refs:              {}", report.symbol_ref_groups);
    println!("  stack groups:             {}", report.stack_groups);

    if !report.signals.is_empty() {
        println!();
        println!("Signals:");
        for signal in &report.signals {
            match (&signal.path, signal.line, signal.column) {
                (Some(path), Some(line), Some(column)) => println!("{path}:{line}:{column}"),
                (Some(path), Some(line), None) => println!("{path}:{line}"),
                (Some(path), None, _) => println!("{path}"),
                (None, _, _) => println!("(no file anchor)"),
            }
            println!("  severity: {}", signal.severity);
            println!("  occurrences: {}", signal.occurrences);
            println!("  message: {}", signal.message);
            println!(
                "  cached summaries: {}",
                log_digest_summary_label(signal.summary_state)
            );
            for summary in &signal.current_summaries {
                println!(
                    "    - {}: {}",
                    summary.symbol,
                    truncate_for_compact(&summary.summary, 160)
                );
            }
        }
    }

    if !report.repeated_lines.is_empty() {
        println!();
        println!("Repeated lines:");
        for repeated in &report.repeated_lines {
            println!(
                "  {}x {}",
                repeated.occurrences,
                truncate_for_compact(&repeated.line, 180)
            );
        }
    }

    if !report.file_refs.is_empty() {
        println!();
        println!("Anchored files:");
        for file_ref in &report.file_refs {
            match (file_ref.line, file_ref.column) {
                (Some(line), Some(column)) => println!("{}:{}:{}", file_ref.path, line, column),
                (Some(line), None) => println!("{}:{}", file_ref.path, line),
                (None, _) => println!("{}", file_ref.path),
            }
            println!("  occurrences: {}", file_ref.occurrences);
            println!(
                "  cached summaries: {}",
                log_digest_summary_label(file_ref.summary_state)
            );
            for summary in &file_ref.current_summaries {
                println!(
                    "    - {}: {}",
                    summary.symbol,
                    truncate_for_compact(&summary.summary, 160)
                );
            }
        }
    }

    if !report.symbol_refs.is_empty() {
        println!();
        println!("Symbol candidates:");
        for symbol in &report.symbol_refs {
            println!("{}", symbol.symbol);
            println!("  occurrences: {}", symbol.occurrences);
            println!(
                "  cached summaries: {}",
                log_digest_summary_label(symbol.summary_state)
            );
            for summary in &symbol.current_summaries {
                println!(
                    "    - {}: {}",
                    summary.symbol,
                    truncate_for_compact(&summary.summary, 160)
                );
            }
        }
    }

    if !report.stack_traces.is_empty() {
        println!();
        println!("Stack groups:");
        for stack in &report.stack_traces {
            println!("  occurrences: {}", stack.occurrences);
            for frame in &stack.frames {
                println!("    - {}", frame);
            }
        }
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

fn metric_digest_trend_label(trend: metric_digest::MetricDigestTrend) -> &'static str {
    match trend {
        metric_digest::MetricDigestTrend::Improved => "improved",
        metric_digest::MetricDigestTrend::Regressed => "regressed",
        metric_digest::MetricDigestTrend::Flat => "flat",
        metric_digest::MetricDigestTrend::Unknown => "changed",
    }
}

fn cmd_metric_digest(options: MetricDigestOptions<'_>, format: OutputFormat) -> Result<()> {
    let input = match options.input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading metric input: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading metric input from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!("no metric input provided; pass --input <file> or pipe JSON/NDJSON on stdin");
    }

    let baseline = match options.baseline_path {
        Some(file_path) => Some(
            fs::read_to_string(file_path)
                .with_context(|| format!("reading metric baseline: {}", file_path.display()))?,
        ),
        None => None,
    };

    let report = metric_digest::compute(
        &input,
        baseline.as_deref(),
        options.metrics,
        options.lower_is_better,
        options.higher_is_better,
        options.history,
        options.top,
    )?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(&report, format.pretty, format.terse, format.schema)?
        );
        return Ok(());
    }

    if format.compact {
        let previous = report
            .previous_run
            .as_ref()
            .map(|run| run.label.as_str())
            .unwrap_or("-");
        println!(
            "metric runs:{} current:{} previous:{} metrics:{} imp:{} reg:{}",
            report.runs_loaded,
            report.current_run.label,
            previous,
            report.shared_metrics.max(report.current_run.metrics.len()),
            report.top_improvements.len(),
            report.top_regressions.len()
        );
        for delta in &report.metric_deltas {
            println!(
                "{} current:{} prev:{} delta:{} trend:{}",
                delta.metric,
                metric_digest::format_number(delta.current),
                metric_digest::format_number(delta.previous),
                metric_digest::format_number(delta.delta),
                metric_digest_trend_label(delta.trend)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Metric digest");
    println!("  runs loaded:    {}", report.runs_loaded);
    println!("  current:        {}", report.current_run.label);
    match report.previous_run.as_ref() {
        Some(previous) => println!("  previous:       {}", previous.label),
        None => println!("  previous:       (none)"),
    }
    println!("  shared metrics: {}", report.shared_metrics);

    if report.metric_deltas.is_empty() {
        println!();
        println!("Current metrics:");
        for (metric, value) in &report.current_run.metrics {
            println!("  {}: {}", metric, metric_digest::format_number(*value));
        }
    } else {
        println!();
        println!("Current vs previous:");
        for delta in &report.metric_deltas {
            let percent = delta
                .percent_delta
                .map(|value| format!(", {value:+.2}%"))
                .unwrap_or_default();
            println!(
                "  {}: {} (prev {}, delta {:+}{}; {})",
                delta.metric,
                metric_digest::format_number(delta.current),
                metric_digest::format_number(delta.previous),
                metric_digest::format_number(delta.delta),
                percent,
                metric_digest_trend_label(delta.trend)
            );
        }
    }

    if !report.top_improvements.is_empty() {
        println!();
        println!("Top improvements:");
        for delta in &report.top_improvements {
            println!(
                "  {}: {} -> {}",
                delta.metric,
                metric_digest::format_number(delta.previous),
                metric_digest::format_number(delta.current)
            );
        }
    }

    if !report.top_regressions.is_empty() {
        println!();
        println!("Top regressions:");
        for delta in &report.top_regressions {
            println!(
                "  {}: {} -> {}",
                delta.metric,
                metric_digest::format_number(delta.previous),
                metric_digest::format_number(delta.current)
            );
        }
    }

    if !report.news_table_markdown.is_empty() {
        println!();
        println!("News-ready table:");
        println!("{}", report.news_table_markdown);
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

fn cmd_dci_benchmark(fixture_path: &Path, format: OutputFormat) -> Result<()> {
    let input = fs::read_to_string(fixture_path)
        .with_context(|| format!("reading dci-benchmark fixture: {}", fixture_path.display()))?;
    let report = dci_benchmark::compute(&input)?;

    if format.json_output {
        println!(
            "{}",
            to_json_schema(&report, format.pretty, format.terse, format.schema)?
        );
        return Ok(());
    }

    if format.compact {
        println!(
            "dci tasks:{} strategies:{} warnings:{}",
            report.tasks_loaded,
            report.strategies_compared,
            report.warnings.len()
        );
        for summary in &report.strategy_summaries {
            println!(
                "{} rank:{} loc:{}/{} rate:{} calls:{} latency_ms:{} tokens:{}",
                summary.strategy,
                summary.rank,
                summary.localized,
                summary.task_runs,
                dci_benchmark::format_number(summary.localization_rate * 100.0),
                dci_benchmark::format_number(summary.avg_tool_calls),
                dci_benchmark::format_number(summary.avg_latency_ms),
                dci_benchmark::format_number(summary.avg_estimated_tokens)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("DCI benchmark");
    if let Some(description) = &report.description {
        println!("  description: {}", description);
    }
    println!("  tasks loaded:        {}", report.tasks_loaded);
    println!("  strategies compared: {}", report.strategies_compared);

    println!();
    println!("Strategy summary:");
    for summary in &report.strategy_summaries {
        println!(
            "  #{} {}: localization {}/{} ({:.1}%), avg calls {}, avg latency {}ms, avg tokens {}",
            summary.rank,
            summary.strategy,
            summary.localized,
            summary.task_runs,
            summary.localization_rate * 100.0,
            dci_benchmark::format_number(summary.avg_tool_calls),
            dci_benchmark::format_number(summary.avg_latency_ms),
            dci_benchmark::format_number(summary.avg_estimated_tokens)
        );
    }

    println!();
    println!("Task winners:");
    for row in &report.task_rows {
        let label = row
            .label
            .as_ref()
            .map(|value| format!(" ({value})"))
            .unwrap_or_default();
        println!("  {}{}", row.task_id, label);
        println!("    localized: {}", row.best_localization.join(", "));
        println!(
            "    lowest calls: {}, lowest latency: {}, lowest tokens: {}",
            row.lowest_tool_calls.as_deref().unwrap_or("-"),
            row.lowest_latency.as_deref().unwrap_or("-"),
            row.lowest_token_budget.as_deref().unwrap_or("-")
        );
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

fn cmd_session_digest(
    path: &Path,
    input_path: Option<&Path>,
    source: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let input = match input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading session transcript: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading session transcript from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!("no session input provided; pass --input <file> or pipe transcript on stdin");
    }

    let report = session_digest::compute(path, &input, source)?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(&report, format.pretty, format.terse, format.schema)?
        );
        return Ok(());
    }

    if format.compact {
        println!(
            "session src:{} prompts:{} cmds:{} files:{} syms:{} fails:{} runtime:{} churn:{} closeout:{}",
            report.source,
            report.prompt_target_count,
            report.command_groups,
            report.file_groups,
            report.symbol_groups,
            report.failure_groups,
            report.runtime_event_groups,
            report.restart_churn_groups,
            report.closeout_groups
        );
        for prompt in &report.prompt_targets {
            println!("prompt: {}", truncate_for_compact(prompt, 100));
        }
        for command in &report.commands {
            println!(
                "cmd count:{} {}",
                command.occurrences,
                truncate_for_compact(&command.command, 100)
            );
        }
        for failure in &report.failures {
            println!(
                "fail {} count:{} {}",
                failure.kind,
                failure.occurrences,
                truncate_for_compact(&failure.message, 100)
            );
        }
        for event in &report.runtime_events {
            println!(
                "runtime count:{} {}",
                event.occurrences,
                truncate_for_compact(&event.event, 100)
            );
        }
        for churn in &report.restart_churn {
            let suffix = churn
                .max_restart_count
                .map(|value| format!(" max_restart:{}", value))
                .unwrap_or_default();
            println!(
                "churn {} count:{}{} {}",
                churn.family,
                churn.occurrences,
                suffix,
                truncate_for_compact(&churn.sample, 100)
            );
        }
        for entry in &report.closeout {
            println!(
                "closeout {} count:{} {}",
                entry.kind,
                entry.occurrences,
                truncate_for_compact(&entry.detail, 100)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Session digest ({})", report.source);
    println!("  transcript items: {}", report.transcript_items);
    println!("  prompt targets:   {}", report.prompt_target_count);
    println!("  commands:         {}", report.command_groups);
    println!("  touched files:    {}", report.file_groups);
    println!("  touched symbols:  {}", report.symbol_groups);
    println!("  failures:         {}", report.failure_groups);
    println!("  runtime events:   {}", report.runtime_event_groups);
    println!("  restart churn:    {}", report.restart_churn_groups);
    println!("  closeout:         {}", report.closeout_groups);

    if !report.prompt_targets.is_empty() {
        println!();
        println!("Prompt targets:");
        for prompt in &report.prompt_targets {
            println!("  - {}", prompt);
        }
    }

    if !report.commands.is_empty() {
        println!();
        println!("Commands:");
        for command in &report.commands {
            println!("  - {} ({})", command.command, command.occurrences);
        }
    }

    if !report.touched_files.is_empty() {
        println!();
        println!("Touched files:");
        for path in &report.touched_files {
            println!("  - {} ({})", path.path, path.occurrences);
        }
    }

    if !report.touched_symbols.is_empty() {
        println!();
        println!("Touched symbols:");
        for symbol in &report.touched_symbols {
            println!("  - {} ({})", symbol.symbol, symbol.occurrences);
        }
    }

    if !report.failures.is_empty() {
        println!();
        println!("Failures:");
        for failure in &report.failures {
            println!(
                "  - [{}] {} ({})",
                failure.kind, failure.message, failure.occurrences
            );
        }
    }

    if !report.runtime_events.is_empty() {
        println!();
        println!("Runtime events:");
        for event in &report.runtime_events {
            println!("  - {} ({})", event.event, event.occurrences);
        }
    }

    if !report.restart_churn.is_empty() {
        println!();
        println!("Restart churn:");
        for churn in &report.restart_churn {
            match churn.max_restart_count {
                Some(max_restart_count) => println!(
                    "  - {} ({}) max_restart={} sample: {}",
                    churn.family, churn.occurrences, max_restart_count, churn.sample
                ),
                None => println!(
                    "  - {} ({}) sample: {}",
                    churn.family, churn.occurrences, churn.sample
                ),
            }
        }
    }

    if !report.closeout.is_empty() {
        println!();
        println!("Closeout evidence:");
        for entry in &report.closeout {
            println!(
                "  - [{}] {} ({})",
                entry.kind, entry.detail, entry.occurrences
            );
        }
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

fn cmd_session_cost(
    input_path: Option<&Path>,
    source: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let input = match input_path {
        Some(file_path) => fs::read_to_string(file_path)
            .with_context(|| format!("reading session-cost input: {}", file_path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading session-cost input from stdin")?;
            buf
        }
    };
    if input.trim().is_empty() {
        bail!(
            "no session-cost input provided; pass --input <file> or pipe transcript/log data on stdin"
        );
    }

    let report = session_cost::compute(&input, source)?;
    if format.json_output {
        println!(
            "{}",
            to_json_schema(&report, format.pretty, format.terse, format.schema)?
        );
        return Ok(());
    }

    if format.compact {
        let cache_ratio = report
            .cached_input_ratio
            .map(|value| format!("{value:.2}%"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "session-cost src:{} samples:{} prompt:{} cached:{} cache_ratio:{} output:{} total:{} runtime:{} churn:{} loops:{} file_reads:{}",
            report.source,
            report.usage_samples,
            format_compact_count(report.prompt_tokens),
            format_compact_count(report.cached_input_tokens),
            cache_ratio,
            format_compact_count(report.output_tokens),
            format_compact_count(report.total_tokens),
            report.total_runtime_events,
            report.restart_churn_groups,
            report.loop_clusters.len(),
            report.file_read_diagnostics.len()
        );
        for turn in &report.largest_turns {
            println!(
                "turn total:{} prompt:{} cached:{} output:{} label:{}",
                format_compact_count(turn.total_tokens),
                format_compact_count(turn.prompt_tokens),
                format_compact_count(turn.cached_input_tokens),
                format_compact_count(turn.output_tokens),
                truncate_for_compact(&turn.label, 72)
            );
        }
        for event in &report.runtime_events {
            println!("event count:{} {}", event.occurrences, event.event);
        }
        for churn in &report.restart_churn {
            let suffix = churn
                .max_restart_count
                .map(|value| format!(" max_restart:{}", value))
                .unwrap_or_default();
            println!(
                "churn {} count:{}{} {}",
                churn.family,
                churn.occurrences,
                suffix,
                truncate_for_compact(&churn.sample, 100)
            );
        }
        for cluster in &report.loop_clusters {
            println!(
                "loop {} count:{} streak:{} {}",
                cluster.kind,
                cluster.occurrences,
                cluster.max_consecutive,
                truncate_for_compact(&cluster.label, 100)
            );
        }
        for diagnostic in &report.file_read_diagnostics {
            println!(
                "file-read count:{} duplicate_tokens:{} range:{} path:{} follow_up:{}",
                diagnostic.occurrences,
                format_compact_count(diagnostic.duplicate_estimated_tokens),
                diagnostic.range,
                truncate_for_compact(&diagnostic.path, 80),
                diagnostic
                    .follow_up_commands
                    .iter()
                    .map(|command| truncate_for_compact(command, 100))
                    .collect::<Vec<_>>()
                    .join(" || ")
            );
        }
        for guardrail in &report.guardrails {
            println!(
                "guardrail {} {} {}",
                guardrail.severity,
                guardrail.kind,
                truncate_for_compact(&guardrail.message, 100)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Session cost digest ({})", report.source);
    println!("  records:                {}", report.record_count);
    println!("  usage samples:          {}", report.usage_samples);
    println!("  prompt tokens:          {}", report.prompt_tokens);
    println!("  cached input tokens:    {}", report.cached_input_tokens);
    println!(
        "  cache creation tokens:  {}",
        report.cache_creation_input_tokens
    );
    println!("  output tokens:          {}", report.output_tokens);
    println!(
        "  reasoning output:       {}",
        report.reasoning_output_tokens
    );
    println!("  total tokens:           {}", report.total_tokens);
    if let Some(ratio) = report.cached_input_ratio {
        println!("  cached input ratio:     {ratio:.2}%");
    }
    println!(
        "  largest turn total:     {}",
        report.largest_turn_total_tokens
    );
    println!("  runtime events:         {}", report.total_runtime_events);
    println!("  runtime groups:         {}", report.runtime_event_groups);
    println!("  restart churn groups:   {}", report.restart_churn_groups);
    println!("  loop clusters:          {}", report.loop_clusters.len());
    println!(
        "  repeated file reads:    {}",
        report.file_read_diagnostics.len()
    );
    if let Some(max_restart_count) = report.max_restart_count {
        println!("  max restart count:      {}", max_restart_count);
    }

    if !report.largest_turns.is_empty() {
        println!();
        println!("Largest turns:");
        for turn in &report.largest_turns {
            println!(
                "  - {}: total {} | prompt {} | cached {} | output {} | reasoning {}",
                turn.label,
                turn.total_tokens,
                turn.prompt_tokens,
                turn.cached_input_tokens,
                turn.output_tokens,
                turn.reasoning_output_tokens
            );
        }
    }

    if !report.runtime_events.is_empty() {
        println!();
        println!("Runtime churn:");
        for event in &report.runtime_events {
            println!("  - {} ({})", event.event, event.occurrences);
        }
    }

    if !report.restart_churn.is_empty() {
        println!();
        println!("Restart churn:");
        for churn in &report.restart_churn {
            match churn.max_restart_count {
                Some(max_restart_count) => println!(
                    "  - {} ({}) max_restart={} sample: {}",
                    churn.family, churn.occurrences, max_restart_count, churn.sample
                ),
                None => println!(
                    "  - {} ({}) sample: {}",
                    churn.family, churn.occurrences, churn.sample
                ),
            }
        }
    }

    if !report.loop_clusters.is_empty() {
        println!();
        println!("Loop clusters:");
        for cluster in &report.loop_clusters {
            println!(
                "  - [{}] {} ({}) max_consecutive={}",
                cluster.kind, cluster.label, cluster.occurrences, cluster.max_consecutive
            );
        }
    }

    if !report.file_read_diagnostics.is_empty() {
        println!();
        println!("Repeated file reads:");
        for diagnostic in &report.file_read_diagnostics {
            println!(
                "  - {} {} ({}) duplicate tokens ~{}",
                diagnostic.path,
                diagnostic.range,
                diagnostic.occurrences,
                diagnostic.duplicate_estimated_tokens
            );
            for command in &diagnostic.follow_up_commands {
                println!("    follow-up: {command}");
            }
        }
    }

    if !report.guardrails.is_empty() {
        println!();
        println!("Guardrails:");
        for guardrail in &report.guardrails {
            println!(
                "  - [{}:{}] {} | guidance: {}",
                guardrail.severity, guardrail.kind, guardrail.message, guardrail.guidance
            );
        }
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

#[allow(dead_code)]
fn cmd_session_review(path: &Path, next_context: bool, format: OutputFormat) -> Result<()> {
    cmd_session_review_with_budget(path, next_context, format, ResponseBudget::default())
}

fn cmd_session_review_with_budget(
    path: &Path,
    next_context: bool,
    format: OutputFormat,
    budget: ResponseBudget,
) -> Result<()> {
    let report = session_review::compute(path)?;
    if budget.is_active() {
        if next_context {
            let budget_report =
                build_session_review_next_context_budget_report(&report, budget, None);
            if format.json_output {
                print_json_or_envelope(
                    &budget_report,
                    &format,
                    "session-review",
                    "next-context-preview",
                    ToolEnvelopeSummary {
                        text: format!("next-context preview for {}", budget_report.target),
                        metrics: vec![
                            envelope_metric("prompt_targets", budget_report.prompt_target_total),
                            envelope_metric("files", budget_report.touched_file_total),
                            envelope_metric("symbols", budget_report.touched_symbol_total),
                            envelope_metric("failures", budget_report.unresolved_failure_total),
                        ],
                    },
                    budget_report.truncated,
                    budget_report.next_digest_commands.clone(),
                )?;
            } else {
                print_session_review_next_context_budget_human(&budget_report);
            }
        } else {
            let budget_report = build_session_review_budget_report(&report, budget);
            if format.json_output {
                let mut follow_up = vec![format!(
                    "tsift session-review {} --next-context --json",
                    shell_quote(&budget_report.target)
                )];
                if let Some(session) = budget_report.sessions.first() {
                    follow_up.push(session.expand.clone());
                }
                print_json_or_envelope(
                    &budget_report,
                    &format,
                    "session-review",
                    "preview",
                    ToolEnvelopeSummary {
                        text: format!("session review preview for {}", budget_report.target),
                        metrics: vec![
                            envelope_metric("sessions", budget_report.sessions_matched),
                            envelope_metric("prompt_targets", budget_report.prompt_targets.len()),
                            envelope_metric("failures", budget_report.failures.len()),
                            envelope_metric("total_tokens", budget_report.total_tokens),
                        ],
                    },
                    budget_report.truncated,
                    follow_up,
                )?;
            } else {
                print_session_review_budget_human(&budget_report);
            }
        }
        return Ok(());
    }
    if next_context {
        if format.json_output {
            print_json_or_envelope(
                &report.next_context,
                &format,
                "session-review",
                "next-context",
                ToolEnvelopeSummary {
                    text: format!("next-context for {}", report.next_context.target),
                    metrics: vec![
                        envelope_metric(
                            "prompt_targets",
                            report.next_context.active_prompt_targets.len(),
                        ),
                        envelope_metric("files", report.next_context.touched_files.len()),
                        envelope_metric("symbols", report.next_context.touched_symbols.len()),
                        envelope_metric("failures", report.next_context.unresolved_failures.len()),
                    ],
                },
                false,
                report.next_context.next_digest_commands.clone(),
            )?;
            return Ok(());
        }

        println!("Next context");
        println!("  target:                 {}", report.next_context.target);
        println!(
            "  prompt targets:         {}",
            report.next_context.active_prompt_targets.len()
        );
        println!(
            "  touched files:          {}",
            report.next_context.touched_files.len()
        );
        println!(
            "  touched symbols:        {}",
            report.next_context.touched_symbols.len()
        );
        println!(
            "  unresolved failures:    {}",
            report.next_context.unresolved_failures.len()
        );
        println!(
            "  last verification:      {}",
            report.next_context.last_verification.status
        );
        println!(
            "  verification detail:    {}",
            report.next_context.last_verification.detail
        );

        if !report.next_context.active_prompt_targets.is_empty() {
            println!();
            println!("Active prompt targets:");
            for prompt in &report.next_context.active_prompt_targets {
                println!("  - {}", prompt);
            }
        }

        if !report.next_context.touched_files.is_empty() {
            println!();
            println!("Touched files:");
            for path in &report.next_context.touched_files {
                println!("  - {}", path);
            }
        }

        if !report.next_context.touched_symbols.is_empty() {
            println!();
            println!("Touched symbols:");
            for symbol in &report.next_context.touched_symbols {
                println!("  - {}", symbol);
            }
        }

        if !report.next_context.unresolved_failures.is_empty() {
            println!();
            println!("Unresolved failures:");
            for failure in &report.next_context.unresolved_failures {
                println!(
                    "  - [{}] {} ({}){}{}",
                    failure.kind,
                    failure.message,
                    failure.occurrences,
                    failure
                        .command
                        .as_ref()
                        .map(|command| format!(" command: {command}"))
                        .unwrap_or_default(),
                    failure
                        .session_path
                        .as_ref()
                        .map(|path| format!(" session: {path}"))
                        .unwrap_or_default()
                );
            }
        }

        println!();
        println!("Next digest commands:");
        for command in &report.next_context.next_digest_commands {
            println!("  - {}", command);
        }
        return Ok(());
    }

    if format.json_output {
        let mut follow_up = vec![format!(
            "tsift session-review {} --next-context --json",
            shell_quote(&report.target)
        )];
        follow_up.extend(report.next_context.next_digest_commands.clone());
        print_json_or_envelope(
            &report,
            &format,
            "session-review",
            "report",
            ToolEnvelopeSummary {
                text: format!("session review for {}", report.target),
                metrics: vec![
                    envelope_metric("sessions", report.sessions_matched),
                    envelope_metric("prompt_targets", report.prompt_target_count),
                    envelope_metric("failures", report.failure_groups),
                    envelope_metric("file_reads", report.file_read_diagnostics.len()),
                    envelope_metric("total_tokens", report.total_tokens),
                ],
            },
            false,
            follow_up,
        )?;
        return Ok(());
    }

    if format.compact {
        let cache_ratio = report
            .cached_input_ratio
            .map(|value| format!("{value:.2}%"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "session-review target:{} kind:{} matched:{} claude:{} codex:{} agent_doc:{} prompt:{} cached:{} cache_ratio:{} output:{} total:{} loops:{} file_reads:{}",
            report.target,
            report.target_kind,
            report.sessions_matched,
            report.claude_sessions,
            report.codex_sessions,
            report.agent_doc_logs,
            format_compact_count(report.prompt_tokens),
            format_compact_count(report.cached_input_tokens),
            cache_ratio,
            format_compact_count(report.output_tokens),
            format_compact_count(report.total_tokens),
            report.loop_clusters.len(),
            report.file_read_diagnostics.len()
        );
        for session in &report.sessions {
            println!(
                "session {} total:{} prompts:{} fails:{} matched_by:{} path:{}",
                session.source,
                format_compact_count(session.total_tokens),
                session.prompt_target_count,
                session.failure_groups,
                session.matched_by.join(","),
                truncate_for_compact(&session.path, 96)
            );
        }
        for prompt in &report.prompt_targets {
            println!(
                "prompt count:{} {}",
                prompt.occurrences,
                truncate_for_compact(&prompt.text, 100)
            );
        }
        for failure in &report.failures {
            println!(
                "fail {} count:{} {}{}{}",
                failure.kind,
                failure.occurrences,
                truncate_for_compact(&failure.message, 100),
                failure
                    .command
                    .as_ref()
                    .map(|command| format!(" command:{}", truncate_for_compact(command, 80)))
                    .unwrap_or_default(),
                failure
                    .session_path
                    .as_ref()
                    .map(|path| format!(" session:{}", truncate_for_compact(path, 80)))
                    .unwrap_or_default()
            );
        }
        for cluster in &report.loop_clusters {
            println!(
                "loop {} count:{} streak:{} {}",
                cluster.kind,
                cluster.occurrences,
                cluster.max_consecutive,
                truncate_for_compact(&cluster.label, 100)
            );
        }
        for diagnostic in &report.file_read_diagnostics {
            println!(
                "file-read count:{} duplicate_tokens:{} range:{} path:{} follow_up:{}",
                diagnostic.occurrences,
                format_compact_count(diagnostic.duplicate_estimated_tokens),
                diagnostic.range,
                truncate_for_compact(&diagnostic.path, 80),
                diagnostic
                    .follow_up_commands
                    .iter()
                    .map(|command| truncate_for_compact(command, 100))
                    .collect::<Vec<_>>()
                    .join(" || ")
            );
        }
        for guardrail in &report.guardrails {
            println!(
                "guardrail {} {} {}",
                guardrail.severity,
                guardrail.kind,
                truncate_for_compact(&guardrail.message, 100)
            );
        }
        for warning in &report.warnings {
            println!("warning: {warning}");
        }
        return Ok(());
    }

    println!("Session review ({})", report.target_kind);
    println!("  root:                   {}", report.root);
    println!("  target:                 {}", report.target);
    println!("  sessions considered:    {}", report.sessions_considered);
    println!("  sessions matched:       {}", report.sessions_matched);
    println!("  Claude sessions:        {}", report.claude_sessions);
    println!("  Codex sessions:         {}", report.codex_sessions);
    println!("  agent-doc logs:         {}", report.agent_doc_logs);
    println!("  prompt targets:         {}", report.prompt_target_count);
    println!("  commands:               {}", report.command_groups);
    println!("  touched files:          {}", report.file_groups);
    println!("  touched symbols:        {}", report.symbol_groups);
    println!("  failures:               {}", report.failure_groups);
    println!("  runtime events:         {}", report.runtime_event_groups);
    println!("  restart churn:          {}", report.restart_churn_groups);
    println!("  closeout:               {}", report.closeout_groups);
    println!("  loop clusters:          {}", report.loop_clusters.len());
    println!(
        "  repeated file reads:    {}",
        report.file_read_diagnostics.len()
    );
    println!("  usage samples:          {}", report.usage_samples);
    println!("  prompt tokens:          {}", report.prompt_tokens);
    println!("  cached input tokens:    {}", report.cached_input_tokens);
    println!(
        "  cache creation tokens:  {}",
        report.cache_creation_input_tokens
    );
    println!("  output tokens:          {}", report.output_tokens);
    println!(
        "  reasoning output:       {}",
        report.reasoning_output_tokens
    );
    println!("  total tokens:           {}", report.total_tokens);
    if let Some(ratio) = report.cached_input_ratio {
        println!("  cached input ratio:     {ratio:.2}%");
    }
    println!(
        "  largest turn total:     {}",
        report.largest_turn_total_tokens
    );

    if !report.sessions.is_empty() {
        println!();
        println!("Matched sessions:");
        for session in &report.sessions {
            println!(
                "  - [{}] {} | total {} | prompts {} | failures {} | matched by {}",
                session.source,
                session.path,
                session.total_tokens,
                session.prompt_target_count,
                session.failure_groups,
                session.matched_by.join(", ")
            );
        }
    }

    if !report.prompt_targets.is_empty() {
        println!();
        println!("Prompt targets:");
        for prompt in &report.prompt_targets {
            println!("  - {} ({})", prompt.text, prompt.occurrences);
        }
    }

    if !report.commands.is_empty() {
        println!();
        println!("Commands:");
        for command in &report.commands {
            println!("  - {} ({})", command.command, command.occurrences);
        }
    }

    if !report.failures.is_empty() {
        println!();
        println!("Failures:");
        for failure in &report.failures {
            println!(
                "  - [{}] {} ({}){}{}",
                failure.kind,
                failure.message,
                failure.occurrences,
                failure
                    .command
                    .as_ref()
                    .map(|command| format!(" command: {command}"))
                    .unwrap_or_default(),
                failure
                    .session_path
                    .as_ref()
                    .map(|path| format!(" session: {path}"))
                    .unwrap_or_default()
            );
        }
    }

    if !report.restart_churn.is_empty() {
        println!();
        println!("Restart churn:");
        for churn in &report.restart_churn {
            match churn.max_restart_count {
                Some(max_restart_count) => println!(
                    "  - {} ({}) max_restart={} sample: {}",
                    churn.family, churn.occurrences, max_restart_count, churn.sample
                ),
                None => println!(
                    "  - {} ({}) sample: {}",
                    churn.family, churn.occurrences, churn.sample
                ),
            }
        }
    }

    if !report.closeout.is_empty() {
        println!();
        println!("Closeout evidence:");
        for entry in &report.closeout {
            println!(
                "  - [{}] {} ({})",
                entry.kind, entry.detail, entry.occurrences
            );
        }
    }

    if !report.loop_clusters.is_empty() {
        println!();
        println!("Loop clusters:");
        for cluster in &report.loop_clusters {
            println!(
                "  - [{}] {} ({}) max_consecutive={}",
                cluster.kind, cluster.label, cluster.occurrences, cluster.max_consecutive
            );
        }
    }

    if !report.file_read_diagnostics.is_empty() {
        println!();
        println!("Repeated file reads:");
        for diagnostic in &report.file_read_diagnostics {
            println!(
                "  - {} {} ({}) duplicate tokens ~{}",
                diagnostic.path,
                diagnostic.range,
                diagnostic.occurrences,
                diagnostic.duplicate_estimated_tokens
            );
            for command in &diagnostic.follow_up_commands {
                println!("    follow-up: {command}");
            }
        }
    }

    if !report.largest_turns.is_empty() {
        println!();
        println!("Largest turns:");
        for turn in &report.largest_turns {
            println!(
                "  - [{}] {}: total {} | prompt {} | cached {} | output {} | reasoning {}",
                turn.source,
                turn.label,
                turn.total_tokens,
                turn.prompt_tokens,
                turn.cached_input_tokens,
                turn.output_tokens,
                turn.reasoning_output_tokens
            );
        }
    }

    if !report.guardrails.is_empty() {
        println!();
        println!("Guardrails:");
        for guardrail in &report.guardrails {
            println!(
                "  - [{}:{}] {} | guidance: {}",
                guardrail.severity, guardrail.kind, guardrail.message, guardrail.guidance
            );
        }
    }

    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    Ok(())
}

#[derive(Serialize)]
struct SessionReviewBudgetSessionPreview {
    handle: String,
    source: String,
    path: String,
    matched_by: Vec<String>,
    total_tokens: u64,
    prompt_targets: usize,
    failures: usize,
    expand: String,
}

#[derive(Serialize)]
struct SessionReviewBudgetPromptPreview {
    handle: String,
    text: String,
    occurrences: usize,
    expand: String,
}

#[derive(Serialize)]
struct SessionReviewBudgetFailurePreview {
    handle: String,
    kind: String,
    message: String,
    occurrences: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_path: Option<String>,
    expand: String,
}

#[derive(Serialize)]
struct SessionReviewBudgetReport {
    target: String,
    target_kind: String,
    max_items: usize,
    max_bytes: usize,
    sessions_matched: usize,
    prompt_tokens: u64,
    cached_input_tokens: u64,
    total_tokens: u64,
    truncated: bool,
    sessions: Vec<SessionReviewBudgetSessionPreview>,
    prompt_targets: Vec<SessionReviewBudgetPromptPreview>,
    failures: Vec<SessionReviewBudgetFailurePreview>,
    guardrails: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct SessionReviewNextContextBudgetReport {
    target: String,
    max_items: usize,
    max_bytes: usize,
    prompt_target_total: usize,
    touched_file_total: usize,
    touched_symbol_total: usize,
    unresolved_failure_total: usize,
    truncated: bool,
    prompt_targets: Vec<String>,
    touched_files: Vec<String>,
    touched_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    touched_symbol_refs: Vec<CompactSymbolRefPreview>,
    unresolved_failures: Vec<SessionReviewBudgetFailurePreview>,
    next_digest_commands: Vec<String>,
}

#[derive(Serialize)]
struct ContextPackReport {
    root: String,
    target: String,
    target_kind: String,
    max_items: usize,
    max_bytes: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    ontology_refs: Vec<CompactOntologyRefPreview>,
    next_context: SessionReviewNextContextBudgetReport,
    diff_digest: ContextPackDiffPreview,
    test_digest: ContextPackOptionalSection<ContextPackTestPreview>,
    log_digest: ContextPackOptionalSection<ContextPackLogPreview>,
    resume_commands: Vec<String>,
}

#[derive(Serialize)]
struct ContextPackOptionalSection<T> {
    status: String,
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<T>,
}

#[derive(Serialize)]
struct ContextPackDiffPreview {
    mode: String,
    files_changed: usize,
    files_with_current_summaries: usize,
    symbols_touched: usize,
    call_edges_added: usize,
    call_edges_removed: usize,
    truncated: bool,
    files: Vec<ContextPackDiffFilePreview>,
}

#[derive(Serialize)]
struct ContextPackDiffFilePreview {
    path: String,
    status: String,
    touched_symbols: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    touched_symbol_refs: Vec<CompactSymbolRefPreview>,
    summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    summary_refs: Vec<ContextPackSummaryRefPreview>,
    added_call_edges: usize,
    removed_call_edges: usize,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ContextPackSummaryRefPreview {
    handle: String,
    symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_alias: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    ontology_refs: Vec<CompactOntologyRefPreview>,
    summary: String,
    expand: String,
}

#[derive(Serialize)]
struct ContextPackTestPreview {
    runner: String,
    failures: usize,
    grouped_failures: usize,
    counts: ContextPackTestCounts,
    truncated: bool,
    failure_groups: Vec<ContextPackTestFailurePreview>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ContextPackTestCounts {
    #[serde(skip_serializing_if = "Option::is_none")]
    passed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<usize>,
}

#[derive(Serialize)]
struct ContextPackTestFailurePreview {
    tests: Vec<String>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    occurrences: usize,
    summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    summary_refs: Vec<ContextPackSummaryRefPreview>,
}

#[derive(Serialize)]
struct ContextPackLogPreview {
    total_lines: usize,
    non_empty_lines: usize,
    signal_groups: usize,
    repeated_line_groups: usize,
    file_ref_groups: usize,
    symbol_ref_groups: usize,
    stack_groups: usize,
    truncated: bool,
    signals: Vec<ContextPackLogSignalPreview>,
    repeated_lines: Vec<ContextPackLogRepeatedLinePreview>,
    file_refs: Vec<ContextPackLogFileRefPreview>,
    symbol_refs: Vec<ContextPackLogSymbolRefPreview>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ContextPackLogSignalPreview {
    severity: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    occurrences: usize,
    summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    summary_refs: Vec<ContextPackSummaryRefPreview>,
}

#[derive(Serialize)]
struct ContextPackLogRepeatedLinePreview {
    line: String,
    occurrences: usize,
}

#[derive(Serialize)]
struct ContextPackLogFileRefPreview {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    occurrences: usize,
    summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    summary_refs: Vec<ContextPackSummaryRefPreview>,
}

#[derive(Serialize)]
struct ContextPackLogSymbolRefPreview {
    handle: String,
    symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_alias: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    ontology_refs: Vec<CompactOntologyRefPreview>,
    occurrences: usize,
    summary_state: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    summary_refs: Vec<ContextPackSummaryRefPreview>,
}

fn session_review_source_flag(source: &str) -> &'static str {
    match source {
        "claude_jsonl" => "claude-jsonl",
        "codex_jsonl" => "codex-jsonl",
        "agent_doc_log" => "agent-doc-log",
        _ => "markdown",
    }
}

fn build_session_review_budget_report(
    report: &session_review::SessionReviewReport,
    budget: ResponseBudget,
) -> SessionReviewBudgetReport {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    let review_expand = format!(
        "tsift session-review {} --json",
        shell_quote(&report.target)
    );
    let sessions = report
        .sessions
        .iter()
        .take(max_items)
        .map(|entry| SessionReviewBudgetSessionPreview {
            handle: stable_handle(
                "srev",
                &format!("{}:{}:{}", entry.source, entry.path, entry.total_tokens),
            ),
            source: entry.source.clone(),
            path: truncate_for_budget(&entry.path, max_bytes),
            matched_by: entry
                .matched_by
                .iter()
                .take(max_items)
                .map(|value| truncate_for_budget(value, max_bytes))
                .collect(),
            total_tokens: entry.total_tokens,
            prompt_targets: entry.prompt_target_count,
            failures: entry.failure_groups,
            expand: format!(
                "tsift session-digest --path {} --input {} --source {}",
                shell_quote(&report.root),
                shell_quote(&entry.path),
                session_review_source_flag(&entry.source)
            ),
        })
        .collect();
    let prompt_targets = report
        .prompt_targets
        .iter()
        .take(max_items)
        .map(|entry| SessionReviewBudgetPromptPreview {
            handle: stable_handle("spt", &entry.text),
            text: truncate_for_budget(&entry.text, max_bytes),
            occurrences: entry.occurrences,
            expand: review_expand.clone(),
        })
        .collect();
    let failures = report
        .failures
        .iter()
        .take(max_items)
        .map(|entry| SessionReviewBudgetFailurePreview {
            handle: stable_handle("sfl", &format!("{}:{}", entry.kind, entry.message)),
            kind: entry.kind.clone(),
            message: truncate_for_budget(&entry.message, max_bytes),
            occurrences: entry.occurrences,
            command: entry
                .command
                .as_ref()
                .map(|command| truncate_for_budget(command, max_bytes)),
            session_path: entry
                .session_path
                .as_ref()
                .map(|path| truncate_for_budget(path, max_bytes)),
            expand: review_expand.clone(),
        })
        .collect();
    let guardrails = report
        .guardrails
        .iter()
        .take(max_items)
        .map(|entry| truncate_for_budget(&entry.message, max_bytes))
        .collect();
    let warnings = report
        .warnings
        .iter()
        .take(max_items)
        .map(|entry| truncate_for_budget(entry, max_bytes))
        .collect();

    SessionReviewBudgetReport {
        target: report.target.clone(),
        target_kind: report.target_kind.clone(),
        max_items,
        max_bytes,
        sessions_matched: report.sessions_matched,
        prompt_tokens: report.prompt_tokens,
        cached_input_tokens: report.cached_input_tokens,
        total_tokens: report.total_tokens,
        truncated: report.sessions.len() > max_items
            || report.prompt_targets.len() > max_items
            || report.failures.len() > max_items
            || report.guardrails.len() > max_items
            || report.warnings.len() > max_items,
        sessions,
        prompt_targets,
        failures,
        guardrails,
        warnings,
    }
}

fn build_session_review_next_context_budget_report(
    report: &session_review::SessionReviewReport,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> SessionReviewNextContextBudgetReport {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    let follow_up_items = budget.follow_up_items();
    SessionReviewNextContextBudgetReport {
        target: report.next_context.target.clone(),
        max_items,
        max_bytes,
        prompt_target_total: report.next_context.active_prompt_targets.len(),
        touched_file_total: report.next_context.touched_files.len(),
        touched_symbol_total: report.next_context.touched_symbols.len(),
        unresolved_failure_total: report.next_context.unresolved_failures.len(),
        truncated: report.next_context.active_prompt_targets.len() > max_items
            || report.next_context.touched_files.len() > max_items
            || report.next_context.touched_symbols.len() > max_items
            || report.next_context.unresolved_failures.len() > max_items
            || report.next_context.next_digest_commands.len() > follow_up_items,
        prompt_targets: report
            .next_context
            .active_prompt_targets
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        touched_files: report
            .next_context
            .touched_files
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        touched_symbols: report
            .next_context
            .touched_symbols
            .iter()
            .take(max_items)
            .map(|entry| truncate_for_budget(entry, max_bytes))
            .collect(),
        touched_symbol_refs: report
            .next_context
            .touched_symbols
            .iter()
            .take(max_items)
            .map(|entry| {
                build_compact_symbol_ref_with_ontology(
                    "ncsym",
                    &format!("{}:{}", report.next_context.target, entry),
                    entry,
                    None,
                    max_bytes,
                    ontology,
                )
            })
            .collect(),
        unresolved_failures: report
            .next_context
            .unresolved_failures
            .iter()
            .take(max_items)
            .map(|entry| SessionReviewBudgetFailurePreview {
                handle: stable_handle("snf", &format!("{}:{}", entry.kind, entry.message)),
                kind: entry.kind.clone(),
                message: truncate_for_budget(&entry.message, max_bytes),
                occurrences: entry.occurrences,
                command: entry
                    .command
                    .as_ref()
                    .map(|command| truncate_for_budget(command, max_bytes)),
                session_path: entry
                    .session_path
                    .as_ref()
                    .map(|path| truncate_for_budget(path, max_bytes)),
                expand: format!(
                    "tsift session-review {} --next-context --json",
                    shell_quote(&report.target)
                ),
            })
            .collect(),
        next_digest_commands: report
            .next_context
            .next_digest_commands
            .iter()
            .take(follow_up_items)
            .cloned()
            .collect(),
    }
}

fn print_session_review_budget_human(report: &SessionReviewBudgetReport) {
    println!(
        "session-review-budget target:{} kind:{} sessions:{}/{} prompt:{} cached:{} total:{}",
        shell_quote(&report.target),
        report.target_kind,
        report.sessions.len(),
        report.sessions_matched,
        format_compact_count(report.prompt_tokens),
        format_compact_count(report.cached_input_tokens),
        format_compact_count(report.total_tokens)
    );
    for session in &report.sessions {
        println!(
            "session {} {} total:{} prompts:{} fails:{} expand:{}",
            session.handle,
            session.path,
            format_compact_count(session.total_tokens),
            session.prompt_targets,
            session.failures,
            session.expand
        );
    }
    for prompt in &report.prompt_targets {
        println!(
            "prompt {} count:{} {} expand:{}",
            prompt.handle, prompt.occurrences, prompt.text, prompt.expand
        );
    }
    for failure in &report.failures {
        println!(
            "fail {} {} count:{} {}{}{} expand:{}",
            failure.handle,
            failure.kind,
            failure.occurrences,
            failure.message,
            failure
                .command
                .as_ref()
                .map(|command| format!(" command:{command}"))
                .unwrap_or_default(),
            failure
                .session_path
                .as_ref()
                .map(|path| format!(" session:{path}"))
                .unwrap_or_default(),
            failure.expand
        );
    }
    for guardrail in &report.guardrails {
        println!("guardrail {guardrail}");
    }
    for warning in &report.warnings {
        println!("warning {warning}");
    }
    if report.truncated {
        println!(
            "budget truncated items:{} bytes:{}",
            report.max_items, report.max_bytes
        );
    }
}

fn print_session_review_next_context_budget_human(report: &SessionReviewNextContextBudgetReport) {
    println!(
        "next-context-budget target:{} prompts:{}/{} files:{}/{} symbols:{}/{} failures:{}/{}",
        shell_quote(&report.target),
        report.prompt_targets.len(),
        report.prompt_target_total,
        report.touched_files.len(),
        report.touched_file_total,
        report.touched_symbols.len(),
        report.touched_symbol_total,
        report.unresolved_failures.len(),
        report.unresolved_failure_total
    );
    for prompt in &report.prompt_targets {
        println!("prompt {prompt}");
    }
    for file in &report.touched_files {
        println!("file {file}");
    }
    for symbol in &report.touched_symbols {
        if let Some(symbol_ref) = report
            .touched_symbol_refs
            .iter()
            .find(|entry| entry.name == *symbol)
        {
            println!(
                "symbol {}",
                format_symbol_preview_line(
                    &symbol_ref.handle,
                    &symbol_ref.name,
                    symbol_ref.tag_alias.as_deref()
                )
            );
        } else {
            println!("symbol {symbol}");
        }
    }
    for failure in &report.unresolved_failures {
        println!(
            "fail {} {} count:{} {}{}{} expand:{}",
            failure.handle,
            failure.kind,
            failure.occurrences,
            failure.message,
            failure
                .command
                .as_ref()
                .map(|command| format!(" command:{command}"))
                .unwrap_or_default(),
            failure
                .session_path
                .as_ref()
                .map(|path| format!(" session:{path}"))
                .unwrap_or_default(),
            failure.expand
        );
    }
    for command in &report.next_digest_commands {
        println!("next {command}");
    }
    if report.truncated {
        println!(
            "budget truncated items:{} bytes:{}",
            report.max_items, report.max_bytes
        );
    }
}

fn effective_context_budget(budget: ResponseBudget) -> ResponseBudget {
    ResponseBudget::new(Some(budget.preview_items()), Some(budget.preview_bytes()))
}

fn build_context_summary_refs<'a>(
    prefix: &str,
    key_scope: &str,
    file_path: Option<&str>,
    snippets: impl Iterator<Item = (&'a str, &'a str)>,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> Vec<ContextPackSummaryRefPreview> {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    snippets
        .take(max_items)
        .map(|(symbol, summary)| {
            let tag_alias = tag_alias_from_name(symbol);
            let ontology_refs = tag_alias
                .as_deref()
                .map(|alias| ontology_refs_for_alias(ontology, alias))
                .unwrap_or_default();
            let expand = match file_path {
                Some(path) => format!("tsift summarize --file {}", shell_quote(path)),
                None => format!("tsift summarize {}", shell_quote(symbol)),
            };
            ContextPackSummaryRefPreview {
                handle: stable_handle(prefix, &format!("{key_scope}:{symbol}:{summary}")),
                symbol: truncate_for_budget(symbol, max_bytes),
                tag_alias: tag_alias.map(|alias| truncate_for_budget(&alias, max_bytes)),
                ontology_refs,
                summary: truncate_for_budget(summary, max_bytes),
                expand,
            }
        })
        .collect()
}

fn build_context_pack_diff_preview(
    report: &diff_digest::DiffDigestReport,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> ContextPackDiffPreview {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    ContextPackDiffPreview {
        mode: diff_digest_mode_label(report.mode).to_string(),
        files_changed: report.files_changed,
        files_with_current_summaries: report.files_with_current_summaries,
        symbols_touched: report.symbols_touched,
        call_edges_added: report.call_edges_added,
        call_edges_removed: report.call_edges_removed,
        truncated: report.files.len() > max_items,
        files: report
            .files
            .iter()
            .take(max_items)
            .map(|file| ContextPackDiffFilePreview {
                path: truncate_for_budget(&file.path, max_bytes),
                status: diff_digest_status_label(file.status).to_string(),
                touched_symbols: file
                    .touched_symbols
                    .iter()
                    .take(max_items)
                    .map(|symbol| truncate_for_budget(symbol, max_bytes))
                    .collect(),
                touched_symbol_refs: file
                    .touched_symbols
                    .iter()
                    .take(max_items)
                    .map(|symbol| {
                        build_compact_symbol_ref_with_ontology(
                            "cdsym",
                            &format!("{}:{}", file.path, symbol),
                            symbol,
                            None,
                            max_bytes,
                            ontology,
                        )
                    })
                    .collect(),
                summary_state: diff_digest_summary_label(file.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "cdsum",
                    &file.path,
                    Some(&file.path),
                    file.current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
                added_call_edges: file.added_call_edges.len(),
                removed_call_edges: file.removed_call_edges.len(),
                warnings: file
                    .warnings
                    .iter()
                    .take(max_items)
                    .map(|warning| truncate_for_budget(warning, max_bytes))
                    .collect(),
            })
            .collect(),
    }
}

fn enrich_next_context_with_diff_symbols(
    next_context: &mut SessionReviewNextContextBudgetReport,
    diff_digest: &ContextPackDiffPreview,
    ontology: Option<&TagOntologyPreviewContext>,
) {
    let mut symbols = next_context.touched_symbols.clone();
    for file in &diff_digest.files {
        for symbol in &file.touched_symbol_refs {
            if !symbols.iter().any(|existing| existing == &symbol.name) {
                symbols.push(symbol.name.clone());
            }
        }
    }

    if symbols.is_empty() {
        return;
    }

    let max_items = next_context.max_items;
    let max_bytes = next_context.max_bytes;
    next_context.touched_symbol_total = next_context.touched_symbol_total.max(symbols.len());
    next_context.truncated |= symbols.len() > max_items;
    next_context.touched_symbols = symbols
        .iter()
        .take(max_items)
        .map(|entry| truncate_for_budget(entry, max_bytes))
        .collect();
    next_context.touched_symbol_refs = symbols
        .iter()
        .take(max_items)
        .map(|entry| {
            build_compact_symbol_ref_with_ontology(
                "ncsym",
                &format!("{}:{}", next_context.target, entry),
                entry,
                None,
                max_bytes,
                ontology,
            )
        })
        .collect();
}

fn build_context_pack_test_preview(
    report: &test_digest::TestDigestReport,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> ContextPackTestPreview {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    ContextPackTestPreview {
        runner: report.runner.clone(),
        failures: report.failures,
        grouped_failures: report.grouped_failures,
        counts: ContextPackTestCounts {
            passed: report.counts.passed,
            failed: report.counts.failed,
            skipped: report.counts.skipped,
        },
        truncated: report.failure_groups.len() > max_items || report.warnings.len() > max_items,
        failure_groups: report
            .failure_groups
            .iter()
            .take(max_items)
            .map(|failure| ContextPackTestFailurePreview {
                tests: failure
                    .tests
                    .iter()
                    .take(max_items)
                    .map(|test| truncate_for_budget(test, max_bytes))
                    .collect(),
                message: truncate_for_budget(&failure.message, max_bytes),
                path: failure
                    .path
                    .as_ref()
                    .map(|path| truncate_for_budget(path, max_bytes)),
                line: failure.line,
                occurrences: failure.occurrences,
                summary_state: test_digest_summary_label(failure.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "ctsum",
                    failure.path.as_deref().unwrap_or("test-failure"),
                    failure.path.as_deref(),
                    failure
                        .current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
            })
            .collect(),
        warnings: report
            .warnings
            .iter()
            .take(max_items)
            .map(|warning| truncate_for_budget(warning, max_bytes))
            .collect(),
    }
}

fn build_context_pack_log_preview(
    report: &log_digest::LogDigestReport,
    budget: ResponseBudget,
    ontology: Option<&TagOntologyPreviewContext>,
) -> ContextPackLogPreview {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    ContextPackLogPreview {
        total_lines: report.total_lines,
        non_empty_lines: report.non_empty_lines,
        signal_groups: report.signal_groups,
        repeated_line_groups: report.repeated_line_groups,
        file_ref_groups: report.file_ref_groups,
        symbol_ref_groups: report.symbol_ref_groups,
        stack_groups: report.stack_groups,
        truncated: report.signals.len() > max_items
            || report.repeated_lines.len() > max_items
            || report.file_refs.len() > max_items
            || report.symbol_refs.len() > max_items
            || report.warnings.len() > max_items,
        signals: report
            .signals
            .iter()
            .take(max_items)
            .map(|signal| ContextPackLogSignalPreview {
                severity: signal.severity.clone(),
                message: truncate_for_budget(&signal.message, max_bytes),
                path: signal
                    .path
                    .as_ref()
                    .map(|path| truncate_for_budget(path, max_bytes)),
                line: signal.line,
                occurrences: signal.occurrences,
                summary_state: log_digest_summary_label(signal.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "clsum",
                    signal.path.as_deref().unwrap_or("log-signal"),
                    signal.path.as_deref(),
                    signal
                        .current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
            })
            .collect(),
        repeated_lines: report
            .repeated_lines
            .iter()
            .take(max_items)
            .map(|line| ContextPackLogRepeatedLinePreview {
                line: truncate_for_budget(&line.line, max_bytes),
                occurrences: line.occurrences,
            })
            .collect(),
        file_refs: report
            .file_refs
            .iter()
            .take(max_items)
            .map(|file| ContextPackLogFileRefPreview {
                path: truncate_for_budget(&file.path, max_bytes),
                line: file.line,
                occurrences: file.occurrences,
                summary_state: log_digest_summary_label(file.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "clfsum",
                    &file.path,
                    Some(&file.path),
                    file.current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
            })
            .collect(),
        symbol_refs: report
            .symbol_refs
            .iter()
            .take(max_items)
            .map(|symbol| ContextPackLogSymbolRefPreview {
                handle: stable_handle("clsym", &symbol.symbol),
                symbol: truncate_for_budget(&symbol.symbol, max_bytes),
                tag_alias: tag_alias_from_name(&symbol.symbol)
                    .map(|alias| truncate_for_budget(&alias, max_bytes)),
                ontology_refs: tag_alias_from_name(&symbol.symbol)
                    .as_deref()
                    .map(|alias| ontology_refs_for_alias(ontology, alias))
                    .unwrap_or_default(),
                occurrences: symbol.occurrences,
                summary_state: log_digest_summary_label(symbol.summary_state).to_string(),
                summary_refs: build_context_summary_refs(
                    "clssum",
                    &symbol.symbol,
                    None,
                    symbol
                        .current_summaries
                        .iter()
                        .map(|snippet| (snippet.symbol.as_str(), snippet.summary.as_str())),
                    budget,
                    ontology,
                ),
            })
            .collect(),
        warnings: report
            .warnings
            .iter()
            .take(max_items)
            .map(|warning| truncate_for_budget(warning, max_bytes))
            .collect(),
    }
}

fn enrich_log_preview_with_diff_symbols(
    log_preview: &mut ContextPackLogPreview,
    diff_digest: &ContextPackDiffPreview,
    ontology: Option<&TagOntologyPreviewContext>,
) {
    if !log_preview.symbol_refs.is_empty() {
        return;
    }

    let mut symbols = Vec::new();
    for file in &diff_digest.files {
        for symbol in &file.touched_symbol_refs {
            if !symbols
                .iter()
                .any(|existing: &String| existing == &symbol.name)
            {
                symbols.push(symbol.name.clone());
            }
        }
    }

    if symbols.is_empty() {
        return;
    }

    log_preview.symbol_ref_groups = log_preview.symbol_ref_groups.max(symbols.len());
    log_preview.symbol_refs = symbols
        .into_iter()
        .map(|symbol| ContextPackLogSymbolRefPreview {
            handle: stable_handle("clsym", &symbol),
            symbol: symbol.clone(),
            tag_alias: tag_alias_from_name(&symbol),
            ontology_refs: tag_alias_from_name(&symbol)
                .as_deref()
                .map(|alias| ontology_refs_for_alias(ontology, alias))
                .unwrap_or_default(),
            occurrences: 1,
            summary_state: "unavailable".to_string(),
            summary_refs: Vec::new(),
        })
        .collect();
}

fn insert_ontology_refs(
    refs: &mut BTreeMap<String, CompactOntologyRefPreview>,
    candidates: &[CompactOntologyRefPreview],
) {
    for candidate in candidates {
        refs.entry(candidate.handle.clone())
            .or_insert_with(|| candidate.clone());
    }
}

fn collect_context_pack_ontology_refs(
    next_context: &SessionReviewNextContextBudgetReport,
    diff_digest: &ContextPackDiffPreview,
    test_digest: &ContextPackOptionalSection<ContextPackTestPreview>,
    log_digest: &ContextPackOptionalSection<ContextPackLogPreview>,
) -> Vec<CompactOntologyRefPreview> {
    let mut refs = BTreeMap::new();
    for symbol in &next_context.touched_symbol_refs {
        insert_ontology_refs(&mut refs, &symbol.ontology_refs);
    }
    for file in &diff_digest.files {
        for symbol in &file.touched_symbol_refs {
            insert_ontology_refs(&mut refs, &symbol.ontology_refs);
        }
        for summary in &file.summary_refs {
            insert_ontology_refs(&mut refs, &summary.ontology_refs);
        }
    }
    if let Some(test) = &test_digest.report {
        for failure in &test.failure_groups {
            for summary in &failure.summary_refs {
                insert_ontology_refs(&mut refs, &summary.ontology_refs);
            }
        }
    }
    if let Some(log) = &log_digest.report {
        for signal in &log.signals {
            for summary in &signal.summary_refs {
                insert_ontology_refs(&mut refs, &summary.ontology_refs);
            }
        }
        for file in &log.file_refs {
            for summary in &file.summary_refs {
                insert_ontology_refs(&mut refs, &summary.ontology_refs);
            }
        }
        for symbol in &log.symbol_refs {
            insert_ontology_refs(&mut refs, &symbol.ontology_refs);
            for summary in &symbol.summary_refs {
                insert_ontology_refs(&mut refs, &summary.ontology_refs);
            }
        }
    }
    refs.into_values().collect()
}

fn build_context_pack_report(
    path: &Path,
    test_input: Option<&Path>,
    runner: Option<&str>,
    log_input: Option<&Path>,
    budget: ResponseBudget,
) -> Result<ContextPackReport> {
    let budget = effective_context_budget(budget);
    let review = session_review::compute(path)?;
    let root = PathBuf::from(&review.root);
    let ontology = load_tag_ontology_preview_context(&root);
    let ontology_ref = ontology.as_ref();
    let mut next_context =
        build_session_review_next_context_budget_report(&review, budget, ontology_ref);
    let diff_digest = build_context_pack_diff_preview(
        &diff_digest::compute(
            &root,
            diff_digest::DiffDigestOptions {
                cached: false,
                revision: None,
            },
        )
        .with_context(|| format!("computing context-pack diff digest for {}", root.display()))?,
        budget,
        ontology_ref,
    );
    enrich_next_context_with_diff_symbols(&mut next_context, &diff_digest, ontology_ref);
    let test_digest = match test_input {
        Some(file_path) => {
            let input = fs::read_to_string(file_path)
                .with_context(|| format!("reading test output: {}", file_path.display()))?;
            if input.trim().is_empty() {
                bail!("no test output provided in {}", file_path.display());
            }
            let report = test_digest::compute(&root, &input, runner)?;
            ContextPackOptionalSection {
                status: "included".to_string(),
                command: format!(
                    "tsift test-digest --path . --input {}{}",
                    shell_quote(file_path.to_str().unwrap_or_default()),
                    runner
                        .map(|value| format!(" --runner {}", shell_quote(value)))
                        .unwrap_or_default()
                ),
                source: Some(file_path.display().to_string()),
                report: Some(build_context_pack_test_preview(
                    &report,
                    budget,
                    ontology_ref,
                )),
            }
        }
        None => ContextPackOptionalSection {
            status: "not_provided".to_string(),
            command: "tsift test-digest --path . < test.log".to_string(),
            source: None,
            report: None,
        },
    };
    let log_digest = match log_input {
        Some(file_path) => {
            let input = fs::read_to_string(file_path)
                .with_context(|| format!("reading log output: {}", file_path.display()))?;
            if input.trim().is_empty() {
                bail!("no log output provided in {}", file_path.display());
            }
            let report = log_digest::compute(&root, &input)?;
            let mut preview = build_context_pack_log_preview(&report, budget, ontology_ref);
            enrich_log_preview_with_diff_symbols(&mut preview, &diff_digest, ontology_ref);
            ContextPackOptionalSection {
                status: "included".to_string(),
                command: format!(
                    "tsift log-digest --path . --input {}",
                    shell_quote(file_path.to_str().unwrap_or_default())
                ),
                source: Some(file_path.display().to_string()),
                report: Some(preview),
            }
        }
        None => ContextPackOptionalSection {
            status: "not_provided".to_string(),
            command: "tsift log-digest --path . < build.log".to_string(),
            source: None,
            report: None,
        },
    };

    let ontology_refs =
        collect_context_pack_ontology_refs(&next_context, &diff_digest, &test_digest, &log_digest);

    Ok(ContextPackReport {
        root: review.root,
        target: review.target,
        target_kind: review.target_kind,
        max_items: budget.preview_items(),
        max_bytes: budget.preview_bytes(),
        ontology_refs,
        next_context,
        diff_digest,
        test_digest,
        log_digest,
        resume_commands: review.next_context.next_digest_commands,
    })
}

fn print_context_pack_human(report: &ContextPackReport, compact: bool) {
    if compact {
        println!(
            "context-pack target:{} prompts:{}/{} diff:{}/{} test:{} log:{}",
            shell_quote(&report.target),
            report.next_context.prompt_targets.len(),
            report.next_context.prompt_target_total,
            report.diff_digest.files.len(),
            report.diff_digest.files_changed,
            report.test_digest.status,
            report.log_digest.status
        );
        for prompt in &report.next_context.prompt_targets {
            println!("prompt {prompt}");
        }
        for file in &report.diff_digest.files {
            println!(
                "diff {} status:{} syms:{} sums:{}",
                file.path,
                file.status,
                if file.touched_symbol_refs.is_empty() {
                    "-".to_string()
                } else {
                    file.touched_symbol_refs
                        .iter()
                        .map(compact_symbol_ref_token)
                        .collect::<Vec<_>>()
                        .join(",")
                },
                if file.summary_refs.is_empty() {
                    "-".to_string()
                } else {
                    file.summary_refs
                        .iter()
                        .map(|summary| summary.handle.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                }
            );
        }
        if let Some(test) = &report.test_digest.report {
            println!(
                "test runner:{} failures:{} groups:{}",
                test.runner, test.failures, test.grouped_failures
            );
        } else {
            println!("test {}", report.test_digest.command);
        }
        if let Some(log) = &report.log_digest.report {
            println!(
                "log lines:{} signals:{} files:{} syms:{}",
                log.non_empty_lines, log.signal_groups, log.file_ref_groups, log.symbol_ref_groups
            );
        } else {
            println!("log {}", report.log_digest.command);
        }
        return;
    }

    println!("Context pack");
    println!("  target:                 {}", report.target);
    println!("  target kind:            {}", report.target_kind);
    println!("  root:                   {}", report.root);
    println!(
        "  preview budget:         {} items / {} bytes",
        report.max_items, report.max_bytes
    );
    println!();
    println!("Next context");
    println!(
        "  prompt targets:         {}/{}",
        report.next_context.prompt_targets.len(),
        report.next_context.prompt_target_total
    );
    println!(
        "  touched files:          {}/{}",
        report.next_context.touched_files.len(),
        report.next_context.touched_file_total
    );
    println!(
        "  touched symbols:        {}/{}",
        report.next_context.touched_symbols.len(),
        report.next_context.touched_symbol_total
    );
    println!(
        "  unresolved failures:    {}/{}",
        report.next_context.unresolved_failures.len(),
        report.next_context.unresolved_failure_total
    );
    if !report.next_context.prompt_targets.is_empty() {
        for prompt in &report.next_context.prompt_targets {
            println!("  - prompt: {prompt}");
        }
    }
    if !report.next_context.touched_files.is_empty() {
        for path in &report.next_context.touched_files {
            println!("  - file: {path}");
        }
    }
    if !report.next_context.touched_symbols.is_empty() {
        for symbol in &report.next_context.touched_symbol_refs {
            println!(
                "  - symbol: {}",
                format_symbol_preview_line(
                    &symbol.handle,
                    &symbol.name,
                    symbol.tag_alias.as_deref()
                )
            );
        }
    }

    println!();
    println!("Diff digest");
    println!("  mode:                   {}", report.diff_digest.mode);
    println!(
        "  files changed:          {}/{}",
        report.diff_digest.files.len(),
        report.diff_digest.files_changed
    );
    println!(
        "  touched symbols:        {}",
        report.diff_digest.symbols_touched
    );
    println!(
        "  call edges:             +{} / -{}",
        report.diff_digest.call_edges_added, report.diff_digest.call_edges_removed
    );
    for file in &report.diff_digest.files {
        println!("  - {} [{}]", file.path, file.status);
        if !file.touched_symbol_refs.is_empty() {
            println!(
                "    symbols: {}",
                file.touched_symbol_refs
                    .iter()
                    .map(|symbol| format_symbol_preview_line(
                        &symbol.handle,
                        &symbol.name,
                        symbol.tag_alias.as_deref()
                    ))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        if !file.warnings.is_empty() {
            println!("    warnings: {}", file.warnings.join(" | "));
        }
        if !file.summary_refs.is_empty() {
            println!(
                "    summaries: {}",
                file.summary_refs
                    .iter()
                    .map(format_summary_ref_line)
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
    }

    println!();
    println!("Test digest");
    println!("  status:                 {}", report.test_digest.status);
    match &report.test_digest.report {
        Some(test) => {
            println!("  runner:                 {}", test.runner);
            println!("  failures:               {}", test.failures);
            println!("  failure groups:         {}", test.grouped_failures);
            for failure in &test.failure_groups {
                let location = match (&failure.path, failure.line) {
                    (Some(path), Some(line)) => format!("{path}:{line}"),
                    (Some(path), None) => path.clone(),
                    _ => "(no file anchor)".to_string(),
                };
                println!(
                    "  - {} count:{} msg:{}",
                    location, failure.occurrences, failure.message
                );
                if !failure.summary_refs.is_empty() {
                    println!(
                        "    summaries: {}",
                        failure
                            .summary_refs
                            .iter()
                            .map(format_summary_ref_line)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    );
                }
            }
        }
        None => println!("  capture:                {}", report.test_digest.command),
    }

    println!();
    println!("Log digest");
    println!("  status:                 {}", report.log_digest.status);
    match &report.log_digest.report {
        Some(log) => {
            println!("  non-empty lines:        {}", log.non_empty_lines);
            println!("  signal groups:          {}", log.signal_groups);
            println!("  file refs:              {}", log.file_ref_groups);
            println!("  symbol refs:            {}", log.symbol_ref_groups);
            for signal in &log.signals {
                let location = match (&signal.path, signal.line) {
                    (Some(path), Some(line)) => format!("{path}:{line}"),
                    (Some(path), None) => path.clone(),
                    _ => "(no file anchor)".to_string(),
                };
                println!(
                    "  - {} {} count:{} msg:{}",
                    location, signal.severity, signal.occurrences, signal.message
                );
                if !signal.summary_refs.is_empty() {
                    println!(
                        "    summaries: {}",
                        signal
                            .summary_refs
                            .iter()
                            .map(format_summary_ref_line)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    );
                }
            }
            for symbol in &log.symbol_refs {
                println!(
                    "  - symbol: {} count:{} state:{}",
                    format_symbol_preview_line(
                        &symbol.handle,
                        &symbol.symbol,
                        symbol.tag_alias.as_deref()
                    ),
                    symbol.occurrences,
                    symbol.summary_state
                );
                if !symbol.summary_refs.is_empty() {
                    println!(
                        "    summaries: {}",
                        symbol
                            .summary_refs
                            .iter()
                            .map(format_summary_ref_line)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    );
                }
            }
        }
        None => println!("  capture:                {}", report.log_digest.command),
    }

    println!();
    println!("Resume commands:");
    for command in &report.resume_commands {
        println!("  - {}", command);
    }
}

fn format_compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn cmd_digest_runner(
    kind: &str,
    path: &Path,
    runner: Option<&str>,
    shell_command: &str,
    format: OutputFormat,
) -> Result<()> {
    let digest_kind = DigestRunnerKind::parse(kind)?;
    let root = transcript_artifact_root(path)?;
    let execution = run_digest_runner_command(shell_command)?;
    let output = &execution.output;
    let captured = String::from_utf8_lossy(&output.stdout).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);
    if format.json_output && format.envelope {
        let artifact_key = format!(
            "{}:{}:{}:{}",
            digest_kind.as_str(),
            shell_command,
            execution.executed_command,
            captured
        );
        let artifact = if captured.trim().is_empty() {
            None
        } else {
            let (suffix, expand) = match digest_kind {
                DigestRunnerKind::Test => (
                    "test.log",
                    format!(
                        "tsift test-digest --path {} --input {}{} --json",
                        shell_quote(root.to_string_lossy().as_ref()),
                        shell_quote(
                            root.join(".tsift/artifacts")
                                .join(format!("{}.test.log", stable_handle("tart", &artifact_key)))
                                .to_string_lossy()
                                .as_ref()
                        ),
                        runner
                            .map(|value| format!(" --runner {}", shell_quote(value)))
                            .unwrap_or_default()
                    ),
                ),
                DigestRunnerKind::Log => (
                    "log",
                    format!(
                        "tsift log-digest --path {} --input {} --json",
                        shell_quote(root.to_string_lossy().as_ref()),
                        shell_quote(
                            root.join(".tsift/artifacts")
                                .join(format!("{}.log", stable_handle("tart", &artifact_key)))
                                .to_string_lossy()
                                .as_ref()
                        )
                    ),
                ),
            };
            Some(persist_transcript_artifact(
                &root,
                "tart",
                suffix,
                &artifact_key,
                &captured,
                expand,
            )?)
        };
        let filter_report = execution.filter.as_ref().map(DigestRunnerFilter::to_json);

        match digest_kind {
            DigestRunnerKind::Test => {
                let digest_report = test_digest::compute(path, &captured, runner)?;
                let report = serde_json::json!({
                    "kind": digest_kind.as_str(),
                    "command": shell_command,
                    "executed_command": execution.executed_command,
                    "exit_code": exit_code,
                    "success": output.status.success(),
                    "filter": filter_report,
                    "artifact": artifact,
                    "digest": digest_report,
                });
                let mut follow_up = artifact
                    .as_ref()
                    .map(|entry| vec![entry.expand.clone()])
                    .unwrap_or_default();
                follow_up.push(format!(
                    "tsift rewrite --run {}",
                    shell_quote(shell_command)
                ));
                let summary_text = if output.status.success() && digest_report.failures == 0 {
                    format!("test run passed for {}", runner.unwrap_or("auto"))
                } else {
                    format!("test run captured {} failure(s)", digest_report.failures)
                };
                print_json_or_envelope(
                    &report,
                    &format,
                    "digest-runner",
                    "test-run",
                    ToolEnvelopeSummary {
                        text: summary_text,
                        metrics: vec![
                            envelope_metric("runner", &digest_report.runner),
                            envelope_metric("exit_code", exit_code),
                            envelope_metric("filter", execution.filter_label()),
                            envelope_metric("failures", digest_report.failures),
                            envelope_metric("groups", digest_report.grouped_failures),
                            envelope_metric(
                                "artifact",
                                artifact
                                    .as_ref()
                                    .map(|entry| entry.handle.as_str())
                                    .unwrap_or("-"),
                            ),
                        ],
                    },
                    false,
                    follow_up,
                )?;
            }
            DigestRunnerKind::Log => {
                let digest_report = log_digest::compute(path, &captured)?;
                let report = serde_json::json!({
                    "kind": digest_kind.as_str(),
                    "command": shell_command,
                    "executed_command": execution.executed_command,
                    "exit_code": exit_code,
                    "success": output.status.success(),
                    "filter": filter_report,
                    "artifact": artifact,
                    "digest": digest_report,
                });
                let mut follow_up = artifact
                    .as_ref()
                    .map(|entry| vec![entry.expand.clone()])
                    .unwrap_or_default();
                follow_up.push(format!(
                    "tsift rewrite --run {}",
                    shell_quote(shell_command)
                ));
                let summary_text = if output.status.success() && digest_report.signal_groups == 0 {
                    "command finished without log signals".to_string()
                } else {
                    format!(
                        "command emitted {} log signal group(s)",
                        digest_report.signal_groups
                    )
                };
                print_json_or_envelope(
                    &report,
                    &format,
                    "digest-runner",
                    "command-run",
                    ToolEnvelopeSummary {
                        text: summary_text,
                        metrics: vec![
                            envelope_metric("exit_code", exit_code),
                            envelope_metric("filter", execution.filter_label()),
                            envelope_metric("signals", digest_report.signal_groups),
                            envelope_metric("file_refs", digest_report.file_ref_groups),
                            envelope_metric(
                                "artifact",
                                artifact
                                    .as_ref()
                                    .map(|entry| entry.handle.as_str())
                                    .unwrap_or("-"),
                            ),
                        ],
                    },
                    false,
                    follow_up,
                )?;
            }
        }

        if output.status.success() {
            return Ok(());
        }
        if let Some(code) = output.status.code() {
            std::process::exit(code);
        }
        bail!("digest-wrapped command terminated by signal: {shell_command}");
    }

    if captured.trim().is_empty() {
        let label = match digest_kind {
            DigestRunnerKind::Test => "test",
            DigestRunnerKind::Log => "log",
        };
        println!("No {label} output captured.");
    } else {
        match digest_kind {
            DigestRunnerKind::Test => {
                render_test_digest_from_input(path, &captured, runner, format)?
            }
            DigestRunnerKind::Log => render_log_digest_from_input(path, &captured, format)?,
        }
    }

    if output.status.success() {
        return Ok(());
    }
    if let Some(code) = output.status.code() {
        std::process::exit(code);
    }
    bail!("digest-wrapped command terminated by signal: {shell_command}");
}

struct DigestRunnerExecution {
    output: std::process::Output,
    executed_command: String,
    filter: Option<DigestRunnerFilter>,
}

impl DigestRunnerExecution {
    fn filter_label(&self) -> &'static str {
        self.filter
            .as_ref()
            .map(|filter| filter.tool)
            .unwrap_or("none")
    }
}

struct DigestRunnerFilter {
    tool: &'static str,
    command: String,
}

impl DigestRunnerFilter {
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "tool": self.tool,
            "command": self.command,
        })
    }
}

fn run_digest_runner_command(shell_command: &str) -> Result<DigestRunnerExecution> {
    let filter = rtk_rewrite_for_digest_runner(shell_command);
    let executed_command = filter
        .as_ref()
        .map(|filter| filter.command.as_str())
        .unwrap_or(shell_command);
    let output = Command::new("sh")
        .arg("-lc")
        .arg(format!("({executed_command}) 2>&1"))
        .stdout(Stdio::piped())
        .output()
        .with_context(|| format!("running digest-wrapped command: {executed_command}"))?;

    Ok(DigestRunnerExecution {
        output,
        executed_command: executed_command.to_string(),
        filter,
    })
}

fn rtk_rewrite_for_digest_runner(shell_command: &str) -> Option<DigestRunnerFilter> {
    if shell_command.trim_start().starts_with("rtk ") || find_command_on_path("rtk").is_none() {
        return None;
    }
    let output = Command::new("rtk")
        .arg("rewrite")
        .arg(shell_command)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let rewritten = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if rewritten.is_empty() || rewritten == shell_command {
        return None;
    }
    Some(DigestRunnerFilter {
        tool: "rtk",
        command: rewritten,
    })
}

fn find_command_on_path(command: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

fn open_existing_summary_db_read_only(db_path: &Path) -> Result<summarize::SummaryDb> {
    if !db_path.exists() {
        bail!("no summaries.db found — run `tsift summarize --extract <path>` first");
    }
    summarize::SummaryDb::open_read_only_resilient(db_path)
}

fn cmd_status(
    path: &std::path::Path,
    fix: bool,
    json_output: bool,
    compact: bool,
    pretty: bool,
    terse: bool,
    schema: bool,
) -> Result<()> {
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let mut report = status::check_status(&root)?;
    if status_missing_workspace_scopes(&report) {
        autoindex_missing_workspace_scopes(&root, &report)?;
        report = status::check_status(&root)?;
    }
    if fix {
        apply_status_fixes(&root, &report)?;
        report = status::check_status(&root)?;
        if status_missing_workspace_scopes(&report) {
            autoindex_missing_workspace_scopes(&root, &report)?;
            report = status::check_status(&root)?;
        }
    }
    if json_output {
        println!("{}", to_json_schema(&report, pretty, terse, schema)?);
    } else {
        print!("{}", status::format_human(&report, compact));
    }
    Ok(())
}

fn status_index_needs_fix(report: &status::StatusReport) -> bool {
    !matches!(report.index, status::IndexStatus::Fresh { .. })
}

fn status_instructions_need_fix(report: &status::StatusReport) -> bool {
    !matches!(report.instructions, init::InstructionStatus::Current { .. })
}

fn apply_status_fixes(root: &Path, report: &status::StatusReport) -> Result<()> {
    if status_instructions_need_fix(report) {
        eprintln!("status fix: refreshing tsift instructions");
        init::init(root, false, false)?;
    }

    if !status_index_needs_fix(report) {
        return Ok(());
    }

    let scopes = config::Config::submodule_dirs(root)?;
    if scopes.is_empty() {
        eprintln!("status fix: refreshing index");
        run_index_update(
            &root.join(".tsift/index.db"),
            root,
            "status --fix refreshing index".to_string(),
            root,
            None,
            false,
            false,
        )?;
        return Ok(());
    }

    let cfg = config::Config::load(root)?;
    for scope in scopes {
        if !scope.source_root.exists() {
            eprintln!(
                "status fix: skipping missing submodule `{}` ({})",
                scope.id,
                scope.source_root.display()
            );
            continue;
        }
        eprintln!("status fix: refreshing submodule `{}` index", scope.id);
        run_index_update(
            &cfg.db_path_for(root, &scope.id),
            &scope.source_root,
            format!("status --fix refreshing submodule `{}` index", scope.id),
            root,
            Some(scope.id.as_str()),
            false,
            false,
        )?;
    }

    Ok(())
}

fn status_missing_workspace_scopes(report: &status::StatusReport) -> bool {
    match &report.index {
        status::IndexStatus::Fresh { missing_scopes, .. }
        | status::IndexStatus::Stale { missing_scopes, .. }
        | status::IndexStatus::Missing { missing_scopes } => !missing_scopes.is_empty(),
    }
}

fn autoindex_missing_workspace_scopes(root: &Path, report: &status::StatusReport) -> Result<()> {
    let missing_scopes = match &report.index {
        status::IndexStatus::Fresh { missing_scopes, .. }
        | status::IndexStatus::Stale { missing_scopes, .. }
        | status::IndexStatus::Missing { missing_scopes } => missing_scopes,
    };
    if missing_scopes.is_empty() {
        return Ok(());
    }

    let missing_scope_ids = missing_scopes
        .iter()
        .map(|scope| scope.scope.as_str())
        .collect::<std::collections::HashSet<_>>();
    let cfg = config::Config::load(root)?;
    for scope in config::Config::submodule_dirs(root)? {
        if !missing_scope_ids.contains(scope.id.as_str()) || !scope.source_root.exists() {
            continue;
        }
        let db_path = cfg.db_path_for(root, &scope.id);
        run_index_update(
            &db_path,
            &scope.source_root,
            format!(
                "autoindexing missing submodule `{}` during status",
                scope.id
            ),
            root,
            Some(scope.id.as_str()),
            false,
            false,
        )?;
    }
    Ok(())
}

fn emit_summary_stats_warnings(stats: &summarize::SummaryStats, root: &Path) {
    for warning in &stats.warnings {
        let rel_path = relativize_pathbuf(&warning.path, root);
        eprintln!(
            "warning: summarize stats {}: {}",
            rel_path.display(),
            warning.message
        );
    }
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

fn normalize_extract_scope_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("canonicalizing extract scope {}", path.display()));
    }

    Ok(summarize::normalize_lexical_path(path))
}

fn resolve_extract_scope(root: &Path, extract_path: &Path) -> Result<PathBuf> {
    let scope = if extract_path.is_absolute() {
        extract_path.to_path_buf()
    } else {
        root.join(extract_path)
    };
    normalize_extract_scope_path(&scope)
}

fn summarize_diff_matches_scope(changed_path: &Path, extract_scope: &Path) -> bool {
    normalize_extract_scope_path(changed_path)
        .unwrap_or_else(|_| summarize::normalize_lexical_path(changed_path))
        .starts_with(extract_scope)
}

fn summarize_relative_file_path(root: &Path, file_path: &Path) -> String {
    summarize::normalize_summary_file_key(file_path.strip_prefix(root).unwrap_or(file_path))
}

fn summarize_full_extract_deleted_summary_paths(
    summary_db: &summarize::SummaryDb,
    root: &Path,
    extract_scope: &Path,
    files_to_extract: &[PathBuf],
) -> Result<BTreeSet<String>> {
    let live_paths = files_to_extract
        .iter()
        .map(|file_path| summarize_relative_file_path(root, file_path))
        .collect::<BTreeSet<_>>();
    let mut deleted = BTreeSet::new();

    for cached_path in summary_db.cached_file_paths()? {
        if !summarize_diff_matches_scope(&root.join(&cached_path), extract_scope) {
            continue;
        }
        if !live_paths.contains(&cached_path) {
            deleted.insert(cached_path);
        }
    }

    Ok(deleted)
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct RebuildSearchTarget {
    label: String,
    reason: RebuildSearchReason,
    reindex_cmd: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RebuildSearchReason {
    Missing,
    Stale { stale_files: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DegradedSearchTarget {
    label: String,
    reason: RebuildSearchReason,
    reindex_cmd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DegradedSearchMode {
    ReadOnly,
    Exact,
}

#[derive(Debug)]
struct SearchPrecheck {
    targets: Vec<SearchIndexTarget>,
    degraded_targets: Vec<DegradedSearchTarget>,
}

fn is_active_writer_lock_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .to_string()
            .contains("another tsift index writer is already active")
    })
}

fn degraded_search_target(
    target: &SearchIndexTarget,
    reason: RebuildSearchReason,
) -> DegradedSearchTarget {
    DegradedSearchTarget {
        label: target.label.clone(),
        reason,
        reindex_cmd: target.reindex_cmd.clone(),
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

fn collect_rebuild_search_targets(
    targets: &[SearchIndexTarget],
) -> Result<Vec<RebuildSearchTarget>> {
    let mut rebuild_targets = Vec::new();
    for target in targets {
        let reason = match inspect_search_index(target)? {
            SearchIndexState::Missing => RebuildSearchReason::Missing,
            SearchIndexState::Fresh => continue,
            SearchIndexState::Stale { stale_files } => RebuildSearchReason::Stale { stale_files },
        };
        rebuild_targets.push(RebuildSearchTarget {
            label: target.label.clone(),
            reason,
            reindex_cmd: target.reindex_cmd.clone(),
        });
    }
    Ok(rebuild_targets)
}

fn rebuild_search_target_detail(target: &RebuildSearchTarget) -> String {
    match target.reason {
        RebuildSearchReason::Missing => format!("{} is missing", target.label),
        RebuildSearchReason::Stale { stale_files } => {
            let file_suffix = if stale_files == 1 { "" } else { "s" };
            format!(
                "{} is stale ({} file{})",
                target.label, stale_files, file_suffix
            )
        }
    }
}

fn rebuild_search_targets_message(rebuild_targets: &[RebuildSearchTarget]) -> String {
    if rebuild_targets.len() == 1 {
        let target = &rebuild_targets[0];
        return format!(
            "{}. Run `{}` to rebuild before retrying.",
            rebuild_search_target_detail(target),
            target.reindex_cmd
        );
    }

    let summary: Vec<String> = rebuild_targets
        .iter()
        .take(3)
        .map(rebuild_search_target_detail)
        .collect();
    let overflow = rebuild_targets.len().saturating_sub(summary.len());
    let mut details = summary.join(", ");
    if overflow > 0 {
        details.push_str(&format!(", +{} more", overflow));
    }
    let reindex_cmd = rebuild_targets[0].reindex_cmd.clone();
    format!(
        "{} indexes need rebuild: {}. Run `{}` to rebuild before retrying.",
        rebuild_targets.len(),
        details,
        reindex_cmd
    )
}

fn precheck_search_indexes(
    root: &Path,
    path_hint: &Path,
    scope: Option<&str>,
    federated: bool,
    autoindex: bool,
) -> Result<SearchPrecheck> {
    let targets = resolve_search_index_targets(root, path_hint, scope, federated)?;
    let mut stale_targets = Vec::new();
    let mut degraded_targets = Vec::new();

    for target in &targets {
        match inspect_search_index(target)? {
            SearchIndexState::Missing => {
                if autoindex && let Err(err) = apply_search_index_update(root, target) {
                    if is_active_writer_lock_error(&err) {
                        degraded_targets
                            .push(degraded_search_target(target, RebuildSearchReason::Missing));
                    } else {
                        return Err(err);
                    }
                }
            }
            SearchIndexState::Fresh => {}
            SearchIndexState::Stale { stale_files } => {
                if autoindex {
                    if let Err(err) = apply_search_index_update(root, target) {
                        if is_active_writer_lock_error(&err) {
                            degraded_targets.push(degraded_search_target(
                                target,
                                RebuildSearchReason::Stale { stale_files },
                            ));
                        } else {
                            return Err(err);
                        }
                    }
                } else {
                    stale_targets.push(RebuildSearchTarget {
                        label: target.label.clone(),
                        reason: RebuildSearchReason::Stale { stale_files },
                        reindex_cmd: target.reindex_cmd.clone(),
                    });
                }
            }
        }
    }

    if stale_targets.is_empty() {
        return Ok(SearchPrecheck {
            targets,
            degraded_targets,
        });
    }

    bail!(
        "tsift search aborted: {} \
         or re-run without `--no-autoindex`.",
        rebuild_search_targets_message(&stale_targets),
    );
}

fn degraded_search_mode(targets: &[DegradedSearchTarget]) -> Option<DegradedSearchMode> {
    if targets.is_empty() {
        return None;
    }

    if targets
        .iter()
        .all(|target| matches!(target.reason, RebuildSearchReason::Missing))
    {
        Some(DegradedSearchMode::Exact)
    } else {
        Some(DegradedSearchMode::ReadOnly)
    }
}

fn degraded_search_targets_summary(targets: &[DegradedSearchTarget]) -> String {
    if targets.len() == 1 {
        let target = &targets[0];
        return match target.reason {
            RebuildSearchReason::Missing => format!("{} is missing", target.label),
            RebuildSearchReason::Stale { stale_files } => {
                let file_suffix = if stale_files == 1 { "" } else { "s" };
                format!(
                    "{} is stale ({} file{})",
                    target.label, stale_files, file_suffix
                )
            }
        };
    }

    let missing = targets
        .iter()
        .filter(|target| matches!(target.reason, RebuildSearchReason::Missing))
        .count();
    let stale = targets.len().saturating_sub(missing);
    let mut parts = Vec::new();
    if stale > 0 {
        let suffix = if stale == 1 { "" } else { "es" };
        parts.push(format!("{stale} stale index{suffix}"));
    }
    if missing > 0 {
        let suffix = if missing == 1 { "" } else { "es" };
        parts.push(format!("{missing} missing index{suffix}"));
    }
    parts.join(", ")
}

fn emit_degraded_search_note(targets: &[DegradedSearchTarget], mode: DegradedSearchMode) {
    let summary = degraded_search_targets_summary(targets);
    let reindex_cmd = &targets[0].reindex_cmd;
    match mode {
        DegradedSearchMode::ReadOnly => eprintln!(
            "note: active tsift writer detected; skipping autoindex because {}. \
             Continuing with read-only search and the current index snapshot; symbol hits may lag. \
             Retry `{}` after the active writer finishes for fresh index results.",
            summary, reindex_cmd
        ),
        DegradedSearchMode::Exact => eprintln!(
            "note: active tsift writer detected; skipping autoindex because {}. \
             Continuing with exact live-file search. Retry `{}` after the active writer finishes \
             for indexed symbol hits.",
            summary, reindex_cmd
        ),
    }
}

fn search_timeout_message(
    timeout_secs: u64,
    strategy: &str,
    targets: &[SearchIndexTarget],
) -> Result<String> {
    let rebuild_targets = collect_rebuild_search_targets(targets)?;
    if rebuild_targets.is_empty() {
        return Ok(format!(
            "tsift search timed out after {}s (strategy: {}). \
             The search root looks fresh, so reindexing is unlikely to help. \
             Re-run with `--timeout 0` to disable the timeout, narrow `--path` / `--scope`, \
             or try a different strategy.",
            timeout_secs, strategy,
        ));
    }

    Ok(format!(
        "tsift search timed out after {}s (strategy: {}). {}",
        timeout_secs,
        strategy,
        rebuild_search_targets_message(&rebuild_targets),
    ))
}

fn is_exact_preferring_query_char(ch: char) -> bool {
    matches!(ch, '-' | '_' | '/' | '\\' | '.' | ':' | '#' | '@')
}

fn query_prefers_exact_search(query: &str) -> bool {
    let trimmed = query.trim();
    !trimmed.is_empty()
        && !trimmed.chars().any(char::is_whitespace)
        && trimmed.chars().any(|ch| ch.is_alphanumeric())
        && trimmed.chars().any(is_exact_preferring_query_char)
        && trimmed
            .chars()
            .all(|ch| ch.is_alphanumeric() || is_exact_preferring_query_char(ch))
}

fn resolve_search_strategy(query: &str, strategy: Option<String>) -> String {
    strategy.unwrap_or_else(|| {
        if query_prefers_exact_search(query) {
            "exact".to_string()
        } else {
            "lexical".to_string()
        }
    })
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
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
    )
}

#[allow(clippy::too_many_arguments)]
fn cmd_search_with_budget(
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
    let root = lint::resolve_project_root_or_canonical_path(&base_path)?;
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
            print_json_or_envelope(
                &report,
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
                report.truncated,
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
        print_json_or_envelope(
            &combined,
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

#[derive(Serialize)]
struct SearchBudgetSymbolPreview {
    handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag_alias: Option<String>,
    match_type: String,
    kind: String,
    name: String,
    file: String,
    line: i64,
    score: f64,
    match_count: usize,
    surface_count: usize,
    file_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    surface_examples: Vec<String>,
    expand: String,
}

#[derive(Serialize)]
struct SearchBudgetHitPreview {
    handle: String,
    rank: usize,
    path: String,
    confidence: String,
    score: f64,
    preview: String,
    expand: String,
}

#[derive(Serialize)]
struct SearchScaleSignals {
    preview_symbols: usize,
    symbol_families: usize,
    raw_symbol_matches: usize,
    preview_hits: usize,
    returned_hits: usize,
    indexed_artifacts: usize,
    skipped_artifacts: usize,
    max_items: usize,
    max_bytes: usize,
}

#[derive(Serialize)]
struct SearchScaleGuard {
    level: String,
    warning: String,
    signals: SearchScaleSignals,
    narrow_commands: Vec<String>,
}

#[derive(Serialize)]
struct SearchBudgetReport {
    query: String,
    strategy: String,
    indexed_artifacts: usize,
    skipped_artifacts: usize,
    max_items: usize,
    max_bytes: usize,
    symbol_total: usize,
    raw_symbol_total: usize,
    hit_total: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    scale_guard: Option<SearchScaleGuard>,
    symbols: Vec<SearchBudgetSymbolPreview>,
    hits: Vec<SearchBudgetHitPreview>,
}

const SEARCH_BUDGET_SURFACE_PREVIEW_LIMIT: usize = 3;

struct SearchBudgetSymbolFamily {
    canonical_family: Option<String>,
    canonical_tag_alias: Option<String>,
    representative_name: String,
    representative_kind: String,
    representative_match_type: String,
    representative_file: String,
    representative_line: i64,
    representative_score: f64,
    seen_surfaces: HashSet<String>,
    seen_files: HashSet<String>,
    surface_examples: Vec<String>,
    match_count: usize,
}

fn search_budget_family_query(tag_alias: Option<&str>, fallback_name: &str) -> String {
    if let Some(alias) = tag_alias
        && let Some(query) = family_query_from_tag_alias(alias)
    {
        return query;
    }
    fallback_name.to_string()
}

fn build_search_budget_family_expand(
    strategy: &str,
    path: &str,
    tag_alias: Option<&str>,
    fallback_name: &str,
) -> String {
    let query = search_budget_family_query(tag_alias, fallback_name);
    let effective_strategy = if strategy == "exact" {
        "lexical"
    } else {
        strategy
    };
    build_search_budget_follow_up(&query, effective_strategy, path)
}

fn format_search_budget_symbol_name(name: &str, surface_count: usize, max_bytes: usize) -> String {
    let preview = if surface_count > 1 {
        let extra = surface_count - 1;
        let label = if extra == 1 { "variant" } else { "variants" };
        format!("{name} (+{extra} {label})")
    } else {
        name.to_string()
    };
    truncate_for_budget(&preview, max_bytes)
}

fn format_search_budget_symbol_file(file: &str, file_count: usize, max_bytes: usize) -> String {
    let preview = if file_count > 1 {
        let extra = file_count - 1;
        let label = if extra == 1 { "file" } else { "files" };
        format!("{file} (+{extra} {label})")
    } else {
        file.to_string()
    };
    truncate_for_budget(&preview, max_bytes)
}

fn build_search_budget_follow_up(query: &str, strategy: &str, path: &str) -> String {
    let mut command = format!(
        "tsift search {} --path {} --limit 20",
        shell_quote(query),
        shell_quote(path)
    );
    if strategy == "exact" {
        command.push_str(" --exact");
    } else if strategy != "lexical" {
        command.push_str(&format!(" --strategy {}", shell_quote(strategy)));
    }
    command
}

fn build_search_exact_narrow_command(query: &str, path: &str, max_items: usize) -> String {
    format!(
        "tsift search {} --path {} --limit {} --exact",
        shell_quote(query),
        shell_quote(path),
        max_items.max(1)
    )
}

fn build_search_path_narrow_command(query: &str, strategy: &str, path: &str) -> String {
    let mut command = format!(
        "tsift search {} --path {} --limit 20",
        shell_quote(query),
        shell_quote(path)
    );
    if strategy == "exact" {
        command.push_str(" --exact");
    } else if strategy != "lexical" {
        command.push_str(&format!(" --strategy {}", shell_quote(strategy)));
    }
    command
}

#[allow(clippy::too_many_arguments)]
fn build_search_scale_guard(
    query: &str,
    strategy: &str,
    root: &Path,
    response: &sift::SearchResponse,
    symbol_total: usize,
    raw_symbol_total: usize,
    hit_total: usize,
    max_items: usize,
    max_bytes: usize,
    symbols: &[SearchBudgetSymbolPreview],
    hits: &[SearchBudgetHitPreview],
) -> Option<SearchScaleGuard> {
    let broad_symbols = symbol_total > max_items || raw_symbol_total > max_items;
    let broad_hits = hit_total > max_items;
    let broad_corpus = response
        .indexed_artifacts
        .saturating_add(response.skipped_artifacts)
        >= 250;
    if !broad_symbols && !broad_hits && !broad_corpus {
        return None;
    }

    let mut narrow_commands = Vec::new();
    let root_path = root.to_string_lossy();
    if strategy != "exact" {
        narrow_commands.push(build_search_exact_narrow_command(
            query,
            root_path.as_ref(),
            max_items,
        ));
    }
    if let Some(symbol) = symbols.first() {
        narrow_commands.push(symbol.expand.clone());
    }
    if let Some(hit) = hits.first() {
        narrow_commands.push(build_search_path_narrow_command(query, strategy, &hit.path));
    }
    narrow_commands.push(
        "tsift workflow search --json # preserve handles, expand only cited parents".to_string(),
    );

    Some(SearchScaleGuard {
        level: if broad_hits || broad_symbols {
            "high-hit".to_string()
        } else {
            "corpus-size".to_string()
        },
        warning: "Broad search surface: inspect the preview first and run a narrowing command before dispatching parallel agents."
            .to_string(),
        signals: SearchScaleSignals {
            preview_symbols: symbols.len(),
            symbol_families: symbol_total,
            raw_symbol_matches: raw_symbol_total,
            preview_hits: hits.len(),
            returned_hits: hit_total,
            indexed_artifacts: response.indexed_artifacts,
            skipped_artifacts: response.skipped_artifacts,
            max_items,
            max_bytes,
        },
        narrow_commands: dedupe_preserve_order(narrow_commands),
    })
}

fn build_search_budget_report(
    query: &str,
    strategy: &str,
    root: &Path,
    response: &sift::SearchResponse,
    symbol_hits: &[index::SymbolHit],
    absolute: bool,
    budget: ResponseBudget,
) -> SearchBudgetReport {
    let max_items = budget.preview_items();
    let max_bytes = budget.preview_bytes();
    let raw_symbol_total = symbol_hits.len();
    let hit_total = response.hits.len();
    let mut family_positions = HashMap::new();
    let mut families = Vec::new();

    for hit in symbol_hits {
        let display_file = if absolute {
            hit.file.clone()
        } else {
            relativize(&hit.file, root)
        };
        let canonical_family = canonical_tag_family_from_symbol(&hit.name, hit.tags.as_deref());
        let family_key = canonical_family
            .as_ref()
            .map(|family| family.canonical.clone())
            .unwrap_or_else(|| hit.name.clone());
        let position = *family_positions.entry(family_key).or_insert_with(|| {
            families.push(SearchBudgetSymbolFamily {
                canonical_family: canonical_family
                    .as_ref()
                    .map(|family| family.canonical.clone()),
                canonical_tag_alias: canonical_family
                    .as_ref()
                    .map(|family| family.tag_alias.clone()),
                representative_name: hit.name.clone(),
                representative_kind: hit.kind.clone(),
                representative_match_type: hit.match_type.clone(),
                representative_file: display_file.clone(),
                representative_line: hit.line,
                representative_score: hit.score,
                seen_surfaces: HashSet::new(),
                seen_files: HashSet::new(),
                surface_examples: Vec::new(),
                match_count: 0,
            });
            families.len() - 1
        });

        let family = &mut families[position];
        family.match_count += 1;
        if family.seen_surfaces.insert(hit.name.clone())
            && family.surface_examples.len() < SEARCH_BUDGET_SURFACE_PREVIEW_LIMIT
        {
            family
                .surface_examples
                .push(truncate_for_budget(&hit.name, max_bytes));
        }
        family.seen_files.insert(display_file);
    }

    let symbol_total = families.len();
    let symbols: Vec<SearchBudgetSymbolPreview> = families
        .into_iter()
        .take(max_items)
        .map(|family| {
            let file_count = family.seen_files.len();
            let surface_count = family.seen_surfaces.len();
            let key = format!(
                "{}:{}:{}:{}:{}:{}:{}",
                family
                    .canonical_family
                    .as_deref()
                    .or(family.canonical_tag_alias.as_deref())
                    .unwrap_or(&family.representative_name),
                family.canonical_tag_alias.as_deref().unwrap_or(""),
                family.representative_kind,
                family.representative_file,
                family.representative_line,
                query,
                strategy
            );
            SearchBudgetSymbolPreview {
                handle: stable_handle("sfam", &key),
                tag_alias: family
                    .canonical_tag_alias
                    .as_deref()
                    .map(|alias| truncate_for_budget(alias, max_bytes)),
                match_type: family.representative_match_type,
                kind: family.representative_kind,
                name: format_search_budget_symbol_name(
                    &family.representative_name,
                    surface_count,
                    max_bytes,
                ),
                file: format_search_budget_symbol_file(
                    &family.representative_file,
                    file_count,
                    max_bytes,
                ),
                line: family.representative_line,
                score: family.representative_score,
                match_count: family.match_count,
                surface_count,
                file_count,
                surface_examples: family.surface_examples,
                expand: build_search_budget_family_expand(
                    strategy,
                    root.to_string_lossy().as_ref(),
                    family.canonical_tag_alias.as_deref(),
                    &family.representative_name,
                ),
            }
        })
        .collect();

    let hits: Vec<SearchBudgetHitPreview> = response
        .hits
        .iter()
        .take(max_items)
        .map(|hit| {
            let display_path = if absolute {
                hit.path.clone()
            } else {
                relativize(&hit.path, root)
            };
            let key = format!("{}:{}:{}:{}", display_path, hit.rank, hit.score, query);
            let preview = compact_snippet(&hit.snippet)
                .map(|snippet| truncate_for_budget(&snippet, max_bytes))
                .unwrap_or_default();
            SearchBudgetHitPreview {
                handle: stable_handle("shit", &key),
                rank: hit.rank,
                path: truncate_for_budget(&display_path, max_bytes),
                confidence: format!("{:?}", hit.confidence),
                score: hit.score,
                preview,
                expand: build_search_budget_follow_up(query, strategy, &display_path),
            }
        })
        .collect();

    let scale_guard = build_search_scale_guard(
        query,
        strategy,
        root,
        response,
        symbol_total,
        raw_symbol_total,
        hit_total,
        max_items,
        max_bytes,
        &symbols,
        &hits,
    );

    SearchBudgetReport {
        query: query.to_string(),
        strategy: strategy.to_string(),
        indexed_artifacts: response.indexed_artifacts,
        skipped_artifacts: response.skipped_artifacts,
        max_items,
        max_bytes,
        symbol_total,
        raw_symbol_total,
        hit_total,
        truncated: symbol_total > max_items || hit_total > max_items,
        scale_guard,
        symbols,
        hits,
    }
}

fn print_search_budget_human(report: &SearchBudgetReport) {
    println!(
        "search-budget q:{} strategy:{} symbols:{}/{} raw-symbols:{} hits:{}/{} indexed:{} skipped:{}",
        shell_quote(&report.query),
        report.strategy,
        report.symbols.len(),
        report.symbol_total,
        report.raw_symbol_total,
        report.hits.len(),
        report.hit_total,
        report.indexed_artifacts,
        report.skipped_artifacts
    );
    for symbol in &report.symbols {
        let variants = if symbol.surface_examples.is_empty() {
            String::new()
        } else {
            format!(" variants:{}", symbol.surface_examples.join(", "))
        };
        println!(
            "sym {} [{}] {} {}:{} sc:{} matches:{} files:{}{} expand:{}",
            format_symbol_preview_line(&symbol.handle, &symbol.name, symbol.tag_alias.as_deref()),
            symbol.match_type,
            symbol.kind,
            symbol.file,
            symbol.line,
            format_score(symbol.score, true),
            symbol.match_count,
            symbol.file_count,
            variants,
            symbol.expand
        );
    }
    for hit in &report.hits {
        if hit.preview.is_empty() {
            println!(
                "hit {} #{} {} [{} {}] expand:{}",
                hit.handle,
                hit.rank,
                hit.path,
                hit.confidence,
                format_score(hit.score, true),
                hit.expand
            );
        } else {
            println!(
                "hit {} #{} {} [{} {}] {} expand:{}",
                hit.handle,
                hit.rank,
                hit.path,
                hit.confidence,
                format_score(hit.score, true),
                hit.preview,
                hit.expand
            );
        }
    }
    if report.truncated {
        println!(
            "budget truncated items:{} bytes:{}",
            report.max_items, report.max_bytes
        );
    }
    if let Some(guard) = &report.scale_guard {
        println!("scale guard [{}]: {}", guard.level, guard.warning);
        println!(
            "signals preview-symbols:{} symbol-families:{} raw-symbols:{} preview-hits:{} hits:{} indexed:{} skipped:{} budget-items:{} budget-bytes:{}",
            guard.signals.preview_symbols,
            guard.signals.symbol_families,
            guard.signals.raw_symbol_matches,
            guard.signals.preview_hits,
            guard.signals.returned_hits,
            guard.signals.indexed_artifacts,
            guard.signals.skipped_artifacts,
            guard.signals.max_items,
            guard.signals.max_bytes
        );
        for command in &guard.narrow_commands {
            println!("narrow: {command}");
        }
    }
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

    fn write_empty_root_index(root: &Path) {
        let index_dir = root.join(".tsift");
        fs::create_dir_all(&index_dir).unwrap();
        fs::write(index_dir.join("index.db"), "").unwrap();
    }

    fn write_repeated_lines(path: &Path, line: &str, lines: usize) -> PathBuf {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let body = std::iter::repeat_n(line, lines)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(path, format!("{body}\n")).unwrap();
        path.to_path_buf()
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
        let extract_scope = resolve_extract_scope(root, Path::new("src/feature")).unwrap();

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
        let extract_scope = resolve_extract_scope(root, Path::new("src/feature/main.rs")).unwrap();

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

        let extract_scope = resolve_extract_scope(dir.path(), Path::new("src")).unwrap();
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
        let extract_scope = resolve_extract_scope(&extract_base, Path::new(".")).unwrap();
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
    fn summarize_extract_scope_normalizes_dotdot_segments() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();

        let extract_scope = resolve_extract_scope(dir.path(), Path::new("src/../src")).unwrap();

        assert_eq!(extract_scope, source_dir.canonicalize().unwrap());
        assert!(summarize_diff_matches_scope(
            &source_dir.join("main.rs"),
            &extract_scope
        ));
    }

    #[cfg(unix)]
    #[test]
    fn summarize_extract_scope_canonicalizes_absolute_symlink_paths() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real");
        let source_dir = real_root.join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let symlink_scope = dir.path().join("scope-link");
        symlink(&source_dir, &symlink_scope).unwrap();

        let extract_scope = resolve_extract_scope(&real_root, &symlink_scope).unwrap();

        assert_eq!(extract_scope, source_dir.canonicalize().unwrap());
        assert!(summarize_diff_matches_scope(
            &source_dir.join("lib.rs"),
            &extract_scope
        ));
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
    fn summarize_diff_extract_treats_unborn_head_as_untracked_only() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

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
    fn summarize_diff_extract_tracks_git_renames() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let old_file = source_dir.join("old.rs");
        let new_file = source_dir.join("new.rs");
        std::fs::write(&old_file, "fn stale() {}\n").unwrap();
        init_git_repo(dir.path());

        let status = std::process::Command::new("git")
            .args(["mv", "src/old.rs", "src/new.rs"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git mv failed");

        let files = summarize::git_changed_files(dir.path()).unwrap();

        assert_eq!(files.existing, vec![new_file]);
        assert_eq!(files.deleted, vec![old_file]);
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
    fn summarize_diff_extract_deletes_renamed_summary_rows() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let old_file = source_dir.join("old.rs");
        std::fs::write(&old_file, "fn stale() {}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# repo\n").unwrap();
        init_git_repo(dir.path());

        let summary_db =
            summarize::SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        summary_db
            .insert(&summarize::Summary {
                id: 0,
                symbol_name: "stale".to_string(),
                file_path: "src/old.rs".to_string(),
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

        let status = std::process::Command::new("git")
            .args(["mv", "src/old.rs", "src/new.rs"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success(), "git mv failed");

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

        assert!(summary_db.get_by_file("src/old.rs").unwrap().is_empty());
    }

    #[test]
    fn summarize_full_extract_deletes_removed_summary_rows_when_scope_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().join("src");
        std::fs::create_dir_all(&source_dir).unwrap();
        let deleted_file = source_dir.join("gone.rs");
        std::fs::write(&deleted_file, "fn stale() {}\n").unwrap();

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
            false,
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
    fn summarize_stats_uses_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = tempfile::tempdir().unwrap();
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
        drop(summary_db);
        let _lock = hold_rollback_journal_lock(&dir.path().join(".tsift/summaries.db"));

        let result = cmd_summarize(
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
        );

        assert!(result.is_ok());
    }

    #[test]
    fn summarize_symbol_query_uses_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = tempfile::tempdir().unwrap();
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
        drop(summary_db);
        let _lock = hold_rollback_journal_lock(&dir.path().join(".tsift/summaries.db"));

        let result = cmd_summarize(
            Some("alpha_helper".to_string()),
            None,
            None,
            false,
            false,
            dir.path(),
            false,
            true,
            false,
            false,
            false,
        );

        assert!(result.is_ok());
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
            Some("tsift --envelope search \"authenticate\" --exact --budget normal".to_string(),)
        );
    }

    #[test]
    fn rewrite_rg_with_path() {
        let result = rewrite_command("rg authenticate src/");
        assert_eq!(
            result,
            Some(
                "tsift --envelope search \"authenticate\" --exact --budget normal --path \"src/\""
                    .to_string()
            )
        );
    }

    #[test]
    fn rewrite_rg_with_flags_ignored() {
        let result = rewrite_command("rg -i authenticate src/");
        assert_eq!(
            result,
            Some(
                "tsift --envelope search \"authenticate\" --exact --budget normal --path \"src/\""
                    .to_string()
            )
        );
    }

    #[test]
    fn rewrite_rg_with_type_flag() {
        // -t rs takes a value, should be skipped; pattern is next positional
        let result = rewrite_command("rg -t rs authenticate");
        assert_eq!(
            result,
            Some("tsift --envelope search \"authenticate\" --exact --budget normal".to_string())
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
            Some(
                "tsift --envelope search \"authenticate\" --exact --budget normal --path \"src/\""
                    .to_string()
            )
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
    fn rewrite_run_tsift_search_disables_timeout_by_default() {
        let result = effective_rewrite_run_command("tsift search hookcaps --exact --path /tmp/x");
        assert_eq!(
            result,
            "tsift search hookcaps --exact --path /tmp/x --timeout 0"
        );
    }

    #[test]
    fn rewrite_run_preserves_explicit_search_timeout() {
        let result = effective_rewrite_run_command(
            "tsift search hookcaps --exact --path /tmp/x --timeout 5",
        );
        assert_eq!(
            result,
            "tsift search hookcaps --exact --path /tmp/x --timeout 5"
        );
    }

    #[test]
    fn rewrite_unrelated_passthrough() {
        let result = rewrite_command("echo cargo build");
        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_rg_quoted_pattern() {
        let result = rewrite_command("rg \"fn main\"");
        assert_eq!(
            result,
            Some("tsift --envelope search \"fn main\" --exact --budget normal".to_string())
        );
    }

    #[test]
    fn rewrite_git_diff_to_diff_digest() {
        let result = rewrite_command("git diff");
        assert_eq!(result, Some("tsift diff-digest .".to_string()));
    }

    #[test]
    fn rewrite_git_diff_cached_to_diff_digest() {
        let result = rewrite_command("git diff --cached");
        assert_eq!(result, Some("tsift diff-digest --cached .".to_string()));
    }

    #[test]
    fn rewrite_git_diff_with_path_to_diff_digest() {
        let result = rewrite_command("git diff -- src/");
        assert_eq!(result, Some("tsift diff-digest \"src/\"".to_string()));
    }

    #[test]
    fn rewrite_git_diff_with_revision_passthrough() {
        let result = rewrite_command("git diff HEAD~1");
        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_git_show_to_revision_diff_digest() {
        let result = rewrite_command("git show HEAD~1");
        assert_eq!(
            result,
            Some("tsift diff-digest --revision \"HEAD~1\" .".to_string())
        );
    }

    #[test]
    fn rewrite_git_log_patch_history_to_revision_diff_digest() {
        let result = rewrite_command("git log -p -1 HEAD~2");
        assert_eq!(
            result,
            Some("tsift diff-digest --revision \"HEAD~2\" .".to_string())
        );
    }

    #[test]
    fn rewrite_cat_long_agent_doc_session_to_session_digest() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("tsift.md");
        let mut body = String::from("---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n");
        for index in 0..90 {
            body.push_str(&format!("❯ prompt {index}?\n"));
        }
        fs::write(&session, body).unwrap();

        let result = rewrite_command(&format!("cat {}", shell_quote(session.to_str().unwrap())));
        assert_eq!(
            result,
            Some(format!(
                "tsift session-digest --path {} --input {} --source markdown",
                shell_quote(&resolve_digest_context_path(&session)),
                shell_quote(session.to_str().unwrap())
            ))
        );
    }

    #[test]
    fn rewrite_head_long_claude_jsonl_to_session_digest() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        let line =
            r#"{"message":{"role":"assistant","content":[{"type":"text","text":"❯ do [#yyhd]"}]}}"#;
        let body = std::iter::repeat_n(line, 120)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&session, format!("{body}\n")).unwrap();

        let result = rewrite_command(&format!(
            "head -n 120 {}",
            shell_quote(session.to_str().unwrap())
        ));
        assert_eq!(
            result,
            Some(format!(
                "tsift session-digest --path {} --input {} --source claude-jsonl",
                shell_quote(&resolve_digest_context_path(&session)),
                shell_quote(session.to_str().unwrap())
            ))
        );
    }

    #[test]
    fn rewrite_head_long_codex_jsonl_to_session_digest() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("codex.jsonl");
        let line = r#"{"type":"event_msg","payload":{"type":"user_message","message":"do [#cdxlog]. spec-test-build-install-commit-push"}}"#;
        let body = std::iter::repeat_n(line, 120)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&session, format!("{body}\n")).unwrap();

        let result = rewrite_command(&format!(
            "head -n 120 {}",
            shell_quote(session.to_str().unwrap())
        ));
        assert_eq!(
            result,
            Some(format!(
                "tsift session-digest --path {} --input {} --source codex-jsonl",
                shell_quote(&resolve_digest_context_path(&session)),
                shell_quote(session.to_str().unwrap())
            ))
        );
    }

    #[test]
    fn rewrite_small_transcript_window_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        let line = r#"{"message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#;
        let body = std::iter::repeat_n(line, 120)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&session, format!("{body}\n")).unwrap();

        let result = rewrite_command(&format!(
            "tail -n 20 {}",
            shell_quote(session.to_str().unwrap())
        ));
        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_sed_large_agent_doc_range_to_session_digest() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("tsift.md");
        let mut body = String::from("---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n");
        for index in 0..120 {
            body.push_str(&format!("### Re: topic {index}\n"));
        }
        fs::write(&session, body).unwrap();

        let result = rewrite_command(&format!(
            "sed -n '1,120p' {}",
            shell_quote(session.to_str().unwrap())
        ));
        assert_eq!(
            result,
            Some(format!(
                "tsift session-digest --path {} --input {} --source markdown",
                shell_quote(&resolve_digest_context_path(&session)),
                shell_quote(session.to_str().unwrap())
            ))
        );
    }

    #[test]
    fn rewrite_cat_large_agent_doc_log_to_session_digest() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("tsift.log");
        let line = "[1776528398] claude_start mode=fresh_restart restart_count=1";
        let body = std::iter::repeat_n(line, 120)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&session, format!("{body}\n")).unwrap();

        let result = rewrite_command(&format!("cat {}", shell_quote(session.to_str().unwrap())));
        assert_eq!(
            result,
            Some(format!(
                "tsift session-digest --path {} --input {} --source agent-doc-log",
                shell_quote(&resolve_digest_context_path(&session)),
                shell_quote(session.to_str().unwrap())
            ))
        );
    }

    #[test]
    fn rewrite_session_reads_prefer_submodule_root_for_digest_path() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "src/tsift"]
	path = src/tsift
	url = https://example.com/tsift
"#,
        )
        .unwrap();
        let submodule = dir.path().join("src/tsift");
        fs::create_dir_all(submodule.join("tasks")).unwrap();
        fs::write(
            submodule.join(".git"),
            "gitdir: ../../.git/modules/src/tsift\n",
        )
        .unwrap();
        let session = submodule.join("tasks/plan.md");
        let mut body = String::from("---\nagent_doc_session: tsift-v0.1\n---\n\n## Exchange\n");
        for index in 0..90 {
            body.push_str(&format!("❯ prompt {index}?\n"));
        }
        fs::write(&session, body).unwrap();

        let result = rewrite_command(&format!("cat {}", shell_quote(session.to_str().unwrap())));

        assert_eq!(
            result,
            Some(format!(
                "tsift session-digest --path {} --input {} --source markdown",
                shell_quote(submodule.to_str().unwrap()),
                shell_quote(session.to_str().unwrap())
            ))
        );
    }

    #[test]
    fn rewrite_regular_markdown_read_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let readme = dir.path().join("README.md");
        let body = std::iter::repeat_n("plain markdown", 120)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&readme, format!("{body}\n")).unwrap();

        let result = rewrite_command(&format!("cat {}", shell_quote(readme.to_str().unwrap())));
        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_cat_large_source_to_source_read_in_indexed_repo() {
        let dir = tempfile::tempdir().unwrap();
        write_empty_root_index(dir.path());
        let source = write_repeated_lines(&dir.path().join("src/lib.rs"), "fn demo() {}", 120);

        let result = rewrite_command(&format!("cat {}", shell_quote(source.to_str().unwrap())));

        assert_eq!(
            result,
            Some(format!(
                "tsift --envelope source-read \"src/lib.rs\" --path {} --start 1 --lines 80 --budget normal",
                shell_quote(&dir.path().to_string_lossy())
            ))
        );
    }

    #[test]
    fn rewrite_head_small_source_window_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        write_empty_root_index(dir.path());
        let source = write_repeated_lines(&dir.path().join("src/lib.rs"), "fn demo() {}", 120);

        let result = rewrite_command(&format!(
            "head -n 20 {}",
            shell_quote(source.to_str().unwrap())
        ));

        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_sed_large_source_range_to_source_read() {
        let dir = tempfile::tempdir().unwrap();
        write_empty_root_index(dir.path());
        let source = write_repeated_lines(&dir.path().join("src/lib.rs"), "fn demo() {}", 200);

        let result = rewrite_command(&format!(
            "sed -n '40,160p' {}",
            shell_quote(source.to_str().unwrap())
        ));

        assert_eq!(
            result,
            Some(format!(
                "tsift --envelope source-read \"src/lib.rs\" --path {} --start 40 --lines 121 --budget normal",
                shell_quote(&dir.path().to_string_lossy())
            ))
        );
    }

    #[test]
    fn rewrite_tail_large_source_window_preserves_tail_anchor() {
        let dir = tempfile::tempdir().unwrap();
        write_empty_root_index(dir.path());
        let source = write_repeated_lines(&dir.path().join("src/lib.rs"), "fn demo() {}", 200);

        let result = rewrite_command(&format!(
            "tail -n 120 {}",
            shell_quote(source.to_str().unwrap())
        ));

        assert_eq!(
            result,
            Some(format!(
                "tsift --envelope source-read \"src/lib.rs\" --path {} --start 81 --lines 120 --budget normal",
                shell_quote(&dir.path().to_string_lossy())
            ))
        );
    }

    #[test]
    fn rewrite_large_non_source_read_passthrough_even_when_indexed() {
        let dir = tempfile::tempdir().unwrap();
        write_empty_root_index(dir.path());
        let text = write_repeated_lines(&dir.path().join("notes.txt"), "plain text", 120);

        let result = rewrite_command(&format!("cat {}", shell_quote(text.to_str().unwrap())));

        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_large_source_read_passthrough_without_index() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_repeated_lines(&dir.path().join("src/lib.rs"), "fn demo() {}", 120);

        let result = rewrite_command(&format!("cat {}", shell_quote(source.to_str().unwrap())));

        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_cargo_test_to_digest_runner() {
        let result = rewrite_command("cargo test --lib");
        assert_eq!(
            result,
            Some(
                "tsift --envelope __digest-runner --kind \"test\" --path \".\" --shell-command \"cargo test --lib\" --runner \"cargo\"".to_string()
            )
        );
    }

    #[test]
    fn rewrite_pytest_to_digest_runner() {
        let result = rewrite_command("pytest -q tests/test_cli.py");
        assert_eq!(
            result,
            Some(
                "tsift --envelope __digest-runner --kind \"test\" --path \".\" --shell-command \"pytest -q tests/test_cli.py\" --runner \"pytest\"".to_string()
            )
        );
    }

    #[test]
    fn rewrite_python_m_pytest_to_digest_runner() {
        let result = rewrite_command("python -m pytest tests/test_cli.py");
        assert_eq!(
            result,
            Some(
                "tsift --envelope __digest-runner --kind \"test\" --path \".\" --shell-command \"python -m pytest tests/test_cli.py\" --runner \"pytest\"".to_string()
            )
        );
    }

    #[test]
    fn rewrite_cargo_build_to_log_digest_runner() {
        let result = rewrite_command("cargo build --release");
        assert_eq!(
            result,
            Some(
                "tsift --envelope __digest-runner --kind \"log\" --path \".\" --shell-command \"cargo build --release\"".to_string()
            )
        );
    }

    #[test]
    fn rewrite_cargo_install_to_log_digest_runner() {
        let result = rewrite_command("cargo install --path . --force");
        assert_eq!(
            result,
            Some(
                "tsift --envelope __digest-runner --kind \"log\" --path \".\" --shell-command \"cargo install --path . --force\"".to_string()
            )
        );
    }

    #[test]
    fn rewrite_metacharacter_command_passthrough() {
        let result = rewrite_command("cargo test | head");
        assert_eq!(result, None);
    }

    #[test]
    fn rewrite_output_cap_detects_search_even_with_global_flag() {
        let cap = rewrite_output_cap("tsift --compact search foo").expect("cap");
        assert_eq!(cap.max_lines, 50);
        assert_eq!(cap.strip_prefix, Some("Strategy:"));
    }

    #[test]
    fn rewrite_output_cap_skips_structured_output() {
        assert!(rewrite_output_cap("tsift search foo --json").is_none());
        assert!(rewrite_output_cap("tsift --schema graph foo").is_none());
        assert!(rewrite_output_cap("tsift --envelope search foo").is_none());
    }

    #[test]
    fn rewrite_output_format_forwards_envelope_to_digest_runner() {
        let command = rewrite_command("cargo test --lib").expect("rewrite");
        let forwarded = apply_rewrite_output_format(
            &command,
            OutputFormat {
                json_output: true,
                compact: false,
                pretty: false,
                terse: false,
                schema: false,
                envelope: true,
            },
        );
        assert_eq!(
            forwarded,
            "tsift --envelope __digest-runner --kind \"test\" --path \".\" --shell-command \"cargo test --lib\" --runner \"cargo\""
        );
    }

    #[test]
    fn rewrite_output_format_forwards_json_when_requested() {
        let command = rewrite_command("cargo build --release").expect("rewrite");
        let forwarded = apply_rewrite_output_format(
            &command,
            OutputFormat {
                json_output: true,
                compact: false,
                pretty: true,
                terse: false,
                schema: false,
                envelope: false,
            },
        );
        assert_eq!(
            forwarded,
            "tsift --pretty --envelope __digest-runner --kind \"log\" --path \".\" --shell-command \"cargo build --release\""
        );
    }

    #[test]
    fn output_cap_strips_search_header_and_truncates() {
        let capped = apply_output_cap(
            b"Strategy: exact | Indexed: 0 | Skipped: 0\n\nline1\nline2\nline3\n",
            OutputCap {
                max_lines: 2,
                strip_prefix: Some("Strategy:"),
            },
        );
        assert_eq!(
            capped,
            "line1\nline2\n... (+1 more lines; rerun the underlying tsift command directly for the full output)\n"
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
        assert_eq!(lines, vec!["  src/main.rs (2): helper, render"]);
    }

    #[test]
    fn search_hit_groups_preserve_file_counts_and_samples() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let main_rs = root.join("src/main.rs");
        fs::create_dir_all(main_rs.parent().unwrap()).unwrap();
        fs::write(&main_rs, "claudescore-3 anchor\nclaudescore-3 follow-up\n").unwrap();
        let freshness = exact_search_file_timestamp(&main_rs);
        let hits = vec![
            sift::SearchHit {
                artifact_id: "a".to_string(),
                artifact_kind: sift::ContextArtifactKind::File,
                path: main_rs.display().to_string(),
                rank: 1,
                score: 10.0,
                confidence: sift::ScoreConfidence::High,
                location: Some("line 3".to_string()),
                snippet: "claudescore-3 anchor".to_string(),
                provenance: sift::ArtifactProvenance {
                    adapter: sift::AcquisitionAdapterKind::FileSystem,
                    source: "ripgrep -F".to_string(),
                    synthetic: false,
                },
                freshness: freshness.clone(),
                budget: sift::ArtifactBudget::from_text("claudescore-3 anchor", 1),
            },
            sift::SearchHit {
                artifact_id: "b".to_string(),
                artifact_kind: sift::ContextArtifactKind::File,
                path: main_rs.display().to_string(),
                rank: 2,
                score: 9.0,
                confidence: sift::ScoreConfidence::High,
                location: Some("line 7".to_string()),
                snippet: "claudescore-3 follow-up".to_string(),
                provenance: sift::ArtifactProvenance {
                    adapter: sift::AcquisitionAdapterKind::FileSystem,
                    source: "ripgrep -F".to_string(),
                    synthetic: false,
                },
                freshness: freshness.clone(),
                budget: sift::ArtifactBudget::from_text("claudescore-3 follow-up", 1),
            },
            sift::SearchHit {
                artifact_id: "c".to_string(),
                artifact_kind: sift::ContextArtifactKind::File,
                path: main_rs.display().to_string(),
                rank: 3,
                score: 8.0,
                confidence: sift::ScoreConfidence::High,
                location: Some("line 9".to_string()),
                snippet: "claudescore-3 tail".to_string(),
                provenance: sift::ArtifactProvenance {
                    adapter: sift::AcquisitionAdapterKind::FileSystem,
                    source: "ripgrep -F".to_string(),
                    synthetic: false,
                },
                freshness,
                budget: sift::ArtifactBudget::from_text("claudescore-3 tail", 1),
            },
        ];

        let groups = group_search_hits(&hits, root, false);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].path, "src/main.rs");
        assert_eq!(groups[0].hits, 3);
        assert_eq!(
            groups[0].samples,
            vec![
                "line 3: claudescore-3 anchor".to_string(),
                "line 7: claudescore-3 follow-up".to_string()
            ]
        );
        assert!(should_collapse_search_hits(&hits, root, false));
    }

    #[test]
    fn dense_edge_groups_trigger_collapse() {
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
                caller_name: "beta".to_string(),
                caller_line: 5,
                callee_name: "helper".to_string(),
                call_site_line: 6,
            },
            index::StoredEdge {
                caller_file: "src/main.rs".to_string(),
                caller_name: "gamma".to_string(),
                caller_line: 9,
                callee_name: "helper".to_string(),
                call_site_line: 10,
            },
        ];
        assert!(should_collapse_edge_groups(&edges));
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

        let response = federated_sift_search(
            dir.path(),
            &dir.path().join(".tsift/search-cache"),
            "fn",
            10,
            0,
            "lexical",
        )
        .unwrap();

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

        let response = federated_sift_search(
            dir.path(),
            &dir.path().join(".tsift/search-cache"),
            "fn",
            10,
            0,
            "lexical",
        )
        .unwrap();

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

    fn hold_wal_database_lock(db_path: &std::path::Path) -> Connection {
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE IF NOT EXISTS wal_lock_probe (id INTEGER PRIMARY KEY);
             INSERT INTO wal_lock_probe DEFAULT VALUES;
             PRAGMA locking_mode=EXCLUSIVE;
             BEGIN EXCLUSIVE;",
        )
        .unwrap();
        assert!(index::wal_sidecar_path(db_path).exists());
        conn
    }

    #[test]
    fn index_cmd_reports_wal_sidecar_diagnostics_without_tsift_writer_lock() {
        let dir = setup_graph_index();
        let db_path = dir.path().join(".tsift/index.db");
        let _lock = hold_wal_database_lock(&db_path);

        let err = cmd_index(
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
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("indexing"));
        assert!(msg.contains("lock diagnostics:"));
        assert!(msg.contains("lock: absent"));
        assert!(msg.contains("wal: present") || msg.contains("shm: present"));
        assert!(msg.contains("wedged writer holding live WAL sidecars"));
        assert!(msg.contains("snapshot fallback"));
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
    fn search_cmd_uses_wal_snapshot_fallback_when_lock_appears_after_precheck() {
        let dir = setup_graph_index();
        let _hook = install_search_post_precheck_wal_lock(dir.path().join(".tsift/index.db"));

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
    fn search_cmd_keeps_read_only_results_when_active_writer_blocks_autoindex() {
        let dir = setup_graph_index();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            dir.path().join("main.rs"),
            "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }",
        )
        .unwrap();
        let _lock = hold_writer_lock(&dir.path().join(".tsift/index.lock"));

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
        assert_eq!(summary.modified, 1);
    }

    #[test]
    fn search_cmd_autoindex_reports_lock_diagnostics_when_rollback_journal_blocks_writer() {
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
        assert!(msg.contains("next: inspect the host for a wedged rollback-journal writer"));
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
    fn exact_search_returns_literal_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "alpha\nclaudescore-3\nbeta\n").unwrap();

        let response = run_exact_search_with_timeout(dir.path(), "claudescore-3", 5, 0).unwrap();

        assert_eq!(response.strategy, "exact");
        assert_eq!(response.hits.len(), 1);
        assert!(response.hits[0].path.ends_with("notes.txt"));
        assert_eq!(response.hits[0].location.as_deref(), Some("line 2"));
        assert!(response.hits[0].snippet.contains("claudescore-3"));
    }

    #[test]
    fn exact_search_skips_stale_index_precheck() {
        let dir = setup_graph_index();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            dir.path().join("main.rs"),
            "fn helper() { println!(\"updated\"); }\nfn main() { helper(); }\n",
        )
        .unwrap();

        let result = cmd_search(
            "println!(\"updated\")".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("exact".to_string()),
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
    fn workspace_exact_search_does_not_require_shared_root_index() {
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

        let result = cmd_search(
            "alpha_helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            Some("exact".to_string()),
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
        assert!(!dir.path().join(".tsift/index.db").exists());
    }

    #[test]
    fn identifier_like_query_prefers_exact_search() {
        assert!(query_prefers_exact_search("claudescore-3"));
        assert!(query_prefers_exact_search("alpha_helper"));
        assert!(query_prefers_exact_search("src/main.rs"));
        assert!(query_prefers_exact_search("crate::module"));
        assert!(!query_prefers_exact_search("authenticate"));
        assert!(!query_prefers_exact_search("fn main"));
        assert!(!query_prefers_exact_search("."));
    }

    #[test]
    fn resolve_search_strategy_auto_promotes_identifier_like_queries() {
        assert_eq!(resolve_search_strategy("claudescore-3", None), "exact");
        assert_eq!(resolve_search_strategy("authenticate", None), "lexical");
        assert_eq!(
            resolve_search_strategy("claudescore-3", Some("hybrid".to_string())),
            "hybrid"
        );
    }

    #[test]
    fn workspace_identifier_like_search_auto_uses_exact_backend() {
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

        let result = cmd_search(
            "alpha_helper".to_string(),
            Some(dir.path().to_path_buf()),
            5,
            None,
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
        assert!(!dir.path().join(".tsift/index.db").exists());
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
    fn status_cmd_autoindexes_missing_workspace_scopes() {
        let dir = setup_workspace();
        let cfg = config::Config::load(dir.path()).unwrap();
        let alpha = config::Config::resolve_submodule(dir.path(), "alpha").unwrap();
        let alpha_db_path = cfg.db_path_for(dir.path(), &alpha.id);
        let alpha_db = index::IndexDb::open(&alpha_db_path).unwrap();
        alpha_db.apply_changes(&alpha.source_root).unwrap();

        let beta_db_path = cfg.db_path_for(dir.path(), "beta");
        assert!(!beta_db_path.exists());

        cmd_status(dir.path(), false, true, false, false, false, false).unwrap();

        assert!(beta_db_path.exists());
        let report = status::check_status(dir.path()).unwrap();
        assert!(matches!(report.index, status::IndexStatus::Fresh { .. }));
    }

    #[test]
    fn status_cmd_autoindexes_workspace_when_all_scopes_are_missing() {
        let dir = setup_workspace();
        let cfg = config::Config::load(dir.path()).unwrap();

        cmd_status(dir.path(), false, true, false, false, false, false).unwrap();

        assert!(cfg.db_path_for(dir.path(), "alpha").exists());
        assert!(cfg.db_path_for(dir.path(), "beta").exists());
        let report = status::check_status(dir.path()).unwrap();
        assert!(matches!(report.index, status::IndexStatus::Fresh { .. }));
    }

    #[test]
    fn status_cmd_fix_refreshes_stale_index() {
        let dir = setup_graph_index();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            dir.path().join("main.rs"),
            "fn helper() { println!(\"updated\"); }\nfn main() { helper(); Vec::new(); }\n",
        )
        .unwrap();

        let report = status::check_status(dir.path()).unwrap();
        assert!(matches!(report.index, status::IndexStatus::Stale { .. }));

        cmd_status(dir.path(), true, true, false, false, false, false).unwrap();

        let report = status::check_status(dir.path()).unwrap();
        assert!(matches!(report.index, status::IndexStatus::Fresh { .. }));
    }

    #[test]
    fn status_cmd_reports_wal_snapshot_recovery_without_tsift_writer_lock() {
        let dir = setup_graph_index();
        let db_path = dir.path().join(".tsift/index.db");
        let _lock = hold_wal_database_lock(&db_path);

        cmd_status(dir.path(), false, true, false, false, false, false).unwrap();

        let report = status::check_status(dir.path()).unwrap();
        assert!(matches!(
            report.index,
            status::IndexStatus::Fresh {
                recovery: Some(index::ReadOnlyRecovery::SnapshotFallbackWal),
                ..
            }
        ));
        let locks = status::check_locks(dir.path(), None, None).unwrap();
        assert!(matches!(
            locks.writer_lock,
            status::WriterLockStatus::Absent { .. }
        ));
        assert!(locks.wal_sidecar.present || locks.shared_memory_sidecar.present);
        assert!(
            locks
                .recommended_action
                .contains("wedged writer holding live WAL sidecars")
        );
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
        let cache_dir = search_dir.join(".tsift/search-cache");
        std::fs::write(search_dir.join("test.rs"), "fn main() {}").unwrap();
        let result = run_sift_search(&search_dir, &cache_dir, "main", 1, "lexical");
        assert!(result.is_ok(), "direct search should succeed");
        assert!(
            cache_dir.exists(),
            "search should create the configured cache dir"
        );
    }

    #[test]
    fn search_timeout_zero_disables_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let search_dir = dir.path().to_path_buf();
        let cache_dir = search_dir.join(".tsift/search-cache");
        std::fs::write(search_dir.join("test.rs"), "fn main() {}").unwrap();
        let result = run_search_with_timeout(&search_dir, &cache_dir, "main", 1, 0, "lexical", &[]);
        assert!(result.is_ok(), "timeout=0 should still work (no timeout)");
        assert!(
            cache_dir.exists(),
            "timeout=0 should keep using the stable search cache dir"
        );
    }

    #[test]
    fn search_timeout_message_reports_missing_index_as_rebuild_needed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
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
        let db_path = dir.path().join(".tsift/index.db");
        std::fs::remove_file(&db_path).unwrap();
        let search_target = SearchIndexTarget {
            label: "index".to_string(),
            db_path,
            source_root: dir.path().to_path_buf(),
            scope_name: None,
            reindex_cmd: format!("tsift index {}", dir.path().display()),
        };

        let message = search_timeout_message(1, "lexical", &[search_target]).unwrap();

        assert!(message.contains("timed out after 1s"));
        assert!(message.contains("index is missing"));
        assert!(message.contains("Run `tsift index"));
        assert!(!message.contains("search root looks fresh"));
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

    #[test]
    fn cli_workflow_defaults_to_search_topic() {
        let cli = Cli::parse_from(["tsift", "workflow"]);
        match cli.command {
            Some(Commands::Workflow { topic, json }) => {
                assert_eq!(topic, "search");
                assert!(!json);
            }
            _ => panic!("expected Workflow command"),
        }
    }

    #[test]
    fn search_workflow_recipe_preserves_handles_across_expansions() {
        let recipe = search_workflow_recipe();
        let step_names: Vec<&str> = recipe.steps.iter().map(|step| step.name).collect();
        assert_eq!(
            step_names,
            vec![
                "exact-anchor",
                "semantic-search",
                "explain-symbol",
                "summarize-selection",
                "digest-expansion"
            ]
        );
        assert!(
            recipe
                .handle_contract
                .iter()
                .any(|item| item.contains("originating command"))
        );
        assert!(
            recipe.steps[1]
                .preserves
                .iter()
                .any(|item| item.contains("sfam-*"))
        );
        assert!(
            recipe.steps[2]
                .preserves
                .iter()
                .any(|item| item.contains("ecall-*"))
        );
        assert!(
            recipe.steps[4]
                .preserves
                .iter()
                .any(|item| item.contains("artifact handles"))
        );
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
    fn cli_accepts_global_envelope_flag() {
        let cli = Cli::parse_from([
            "tsift",
            "--envelope",
            "context-pack",
            "tasks/software/tsift.md",
        ]);
        assert!(cli.envelope);
        assert!(matches!(cli.command, Some(Commands::ContextPack { .. })));
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
    fn cli_search_accepts_exact_flag() {
        let cli = Cli::parse_from(["tsift", "search", "test", "--exact"]);
        match cli.command {
            Some(Commands::Search {
                exact, strategy, ..
            }) => {
                assert!(exact);
                assert!(strategy.is_none());
            }
            _ => panic!("expected Search command"),
        }
    }

    #[test]
    fn cli_parses_diff_digest_command() {
        let cli = Cli::parse_from(["tsift", "diff-digest", "--json", "."]);
        match cli.command {
            Some(Commands::DiffDigest {
                json,
                path,
                cached,
                revision,
            }) => {
                assert!(json);
                assert_eq!(path, PathBuf::from("."));
                assert!(!cached);
                assert!(revision.is_none());
            }
            _ => panic!("expected DiffDigest command"),
        }
    }

    #[test]
    fn cli_rejects_conflicting_diff_digest_modes() {
        match Cli::try_parse_from([
            "tsift",
            "diff-digest",
            "--cached",
            "--revision",
            "HEAD",
            ".",
        ]) {
            Ok(_) => panic!("expected conflicting diff-digest modes to fail"),
            Err(err) => {
                assert!(err.to_string().contains("--cached"));
                assert!(err.to_string().contains("--revision"));
            }
        }
    }

    #[test]
    fn cli_parses_test_digest_command() {
        let cli = Cli::parse_from([
            "tsift",
            "test-digest",
            "--path",
            ".",
            "--input",
            "target/test.log",
            "--runner",
            "cargo",
            "--json",
        ]);
        match cli.command {
            Some(Commands::TestDigest {
                json,
                path,
                input,
                runner,
            }) => {
                assert!(json);
                assert_eq!(path, PathBuf::from("."));
                assert_eq!(input, Some(PathBuf::from("target/test.log")));
                assert_eq!(runner.as_deref(), Some("cargo"));
            }
            _ => panic!("expected TestDigest command"),
        }
    }

    #[test]
    fn cli_parses_log_digest_command() {
        let cli = Cli::parse_from([
            "tsift",
            "log-digest",
            "--path",
            ".",
            "--input",
            "target/build.log",
            "--json",
        ]);
        match cli.command {
            Some(Commands::LogDigest { json, path, input }) => {
                assert!(json);
                assert_eq!(path, PathBuf::from("."));
                assert_eq!(input, Some(PathBuf::from("target/build.log")));
            }
            _ => panic!("expected LogDigest command"),
        }
    }

    #[test]
    fn cli_parses_metric_digest_command() {
        let cli = Cli::parse_from([
            "tsift",
            "metric-digest",
            "--input",
            "target/runs.json",
            "--baseline",
            "target/prior.json",
            "--metric",
            "session_mae",
            "--lower-is-better",
            "session_mae",
            "--history",
            "4",
            "--top",
            "2",
            "--json",
        ]);
        match cli.command {
            Some(Commands::MetricDigest {
                input,
                baseline,
                metrics,
                lower_is_better,
                history,
                top,
                json,
                ..
            }) => {
                assert!(json);
                assert_eq!(input, Some(PathBuf::from("target/runs.json")));
                assert_eq!(baseline, Some(PathBuf::from("target/prior.json")));
                assert_eq!(metrics, vec!["session_mae"]);
                assert_eq!(lower_is_better, vec!["session_mae"]);
                assert_eq!(history, 4);
                assert_eq!(top, 2);
            }
            _ => panic!("expected MetricDigest command"),
        }
    }

    #[test]
    fn cli_parses_dci_benchmark_command() {
        let cli = Cli::parse_from([
            "tsift",
            "dci-benchmark",
            "--fixture",
            "fixtures/dci-search-benchmark.json",
            "--json",
        ]);
        match cli.command {
            Some(Commands::DciBenchmark { fixture, json }) => {
                assert!(json);
                assert_eq!(fixture, PathBuf::from("fixtures/dci-search-benchmark.json"));
            }
            _ => panic!("expected DciBenchmark command"),
        }
    }

    #[test]
    fn cli_parses_session_digest_command() {
        let cli = Cli::parse_from([
            "tsift",
            "session-digest",
            "--path",
            ".",
            "--input",
            "target/session.md",
            "--source",
            "markdown",
            "--json",
        ]);
        match cli.command {
            Some(Commands::SessionDigest {
                json,
                path,
                input,
                source,
            }) => {
                assert!(json);
                assert_eq!(path, PathBuf::from("."));
                assert_eq!(input, Some(PathBuf::from("target/session.md")));
                assert_eq!(source.as_deref(), Some("markdown"));
            }
            _ => panic!("expected SessionDigest command"),
        }
    }

    #[test]
    fn cli_parses_session_cost_command() {
        let cli = Cli::parse_from([
            "tsift",
            "session-cost",
            "--input",
            "target/session.jsonl",
            "--source",
            "codex-jsonl",
            "--json",
        ]);
        match cli.command {
            Some(Commands::SessionCost {
                json,
                input,
                source,
            }) => {
                assert!(json);
                assert_eq!(input, Some(PathBuf::from("target/session.jsonl")));
                assert_eq!(source.as_deref(), Some("codex-jsonl"));
            }
            _ => panic!("expected SessionCost command"),
        }
    }

    #[test]
    fn cli_parses_session_review_command() {
        let cli = Cli::parse_from([
            "tsift",
            "session-review",
            "tasks/software/tsift.md",
            "--next-context",
            "--json",
        ]);
        match cli.command {
            Some(Commands::SessionReview {
                json,
                next_context,
                path,
                ..
            }) => {
                assert!(json);
                assert!(next_context);
                assert_eq!(path, PathBuf::from("tasks/software/tsift.md"));
            }
            _ => panic!("expected SessionReview command"),
        }
    }

    #[test]
    fn cli_search_accepts_budget_flags() {
        let cli = Cli::parse_from([
            "tsift",
            "search",
            "alpha_helper",
            "--max-items",
            "3",
            "--max-bytes",
            "96",
        ]);
        match cli.command {
            Some(Commands::Search {
                max_items,
                max_bytes,
                ..
            }) => {
                assert_eq!(max_items, Some(3));
                assert_eq!(max_bytes, Some(96));
            }
            _ => panic!("expected Search command"),
        }
    }

    #[test]
    fn cli_search_accepts_budget_preset() {
        let cli = Cli::parse_from(["tsift", "search", "alpha_helper", "--budget", "small"]);
        match cli.command {
            Some(Commands::Search { budget, .. }) => {
                assert_eq!(budget, Some(ResponseBudgetPreset::Small));
            }
            _ => panic!("expected Search command"),
        }
    }

    #[test]
    fn response_budget_presets_fill_defaults_and_preserve_explicit_caps() {
        let small = ResponseBudget::from_cli(None, None, Some(ResponseBudgetPreset::Small), false);
        assert_eq!(small.preview_items(), 3);
        assert_eq!(small.preview_bytes(), 120);
        assert_eq!(small.follow_up_items(), 4);

        let overridden =
            ResponseBudget::from_cli(Some(7), None, Some(ResponseBudgetPreset::Small), false);
        assert_eq!(overridden.preview_items(), 7);
        assert_eq!(overridden.preview_bytes(), 120);
        assert_eq!(overridden.follow_up_items(), 7);

        let envelope_default = ResponseBudget::from_cli(None, None, None, true);
        assert!(envelope_default.is_active());
    }

    #[test]
    fn cli_explain_accepts_budget_flags() {
        let cli = Cli::parse_from([
            "tsift",
            "explain",
            "alpha_helper",
            "--max-items",
            "2",
            "--max-bytes",
            "80",
        ]);
        match cli.command {
            Some(Commands::Explain {
                max_items,
                max_bytes,
                ..
            }) => {
                assert_eq!(max_items, Some(2));
                assert_eq!(max_bytes, Some(80));
            }
            _ => panic!("expected Explain command"),
        }
    }

    #[test]
    fn cli_session_review_accepts_budget_flags() {
        let cli = Cli::parse_from([
            "tsift",
            "session-review",
            "tasks/software/tsift.md",
            "--max-items",
            "4",
            "--max-bytes",
            "120",
        ]);
        match cli.command {
            Some(Commands::SessionReview {
                max_items,
                max_bytes,
                ..
            }) => {
                assert_eq!(max_items, Some(4));
                assert_eq!(max_bytes, Some(120));
            }
            _ => panic!("expected SessionReview command"),
        }
    }

    #[test]
    fn cli_parses_context_pack_command() {
        let cli = Cli::parse_from([
            "tsift",
            "context-pack",
            "tasks/software/tsift.md",
            "--test-input",
            "target/test.log",
            "--runner",
            "cargo",
            "--log-input",
            "target/build.log",
            "--max-items",
            "3",
            "--max-bytes",
            "96",
            "--json",
        ]);
        match cli.command {
            Some(Commands::ContextPack {
                path,
                test_input,
                runner,
                log_input,
                json,
                max_items,
                max_bytes,
                budget,
            }) => {
                assert_eq!(path, PathBuf::from("tasks/software/tsift.md"));
                assert_eq!(test_input, Some(PathBuf::from("target/test.log")));
                assert_eq!(runner.as_deref(), Some("cargo"));
                assert_eq!(log_input, Some(PathBuf::from("target/build.log")));
                assert!(json);
                assert_eq!(max_items, Some(3));
                assert_eq!(max_bytes, Some(96));
                assert!(budget.is_none());
            }
            _ => panic!("expected ContextPack command"),
        }
    }

    #[test]
    fn cli_parses_token_savings_command() {
        let cli = Cli::parse_from([
            "tsift",
            "token-savings",
            "--fixture",
            "fixtures/tsift-token-savings.json",
            "--fail-under",
            "--json",
        ]);
        match cli.command {
            Some(Commands::TokenSavings {
                fixture,
                fail_under,
                json,
            }) => {
                assert_eq!(fixture, PathBuf::from("fixtures/tsift-token-savings.json"));
                assert!(fail_under);
                assert!(json);
            }
            _ => panic!("expected TokenSavings command"),
        }
    }

    #[test]
    fn token_savings_report_records_fixture_thresholds() {
        let raw_symbols = [
            "validate_user",
            "validateUser",
            "ValidateUser",
            "validate-user",
            "VALIDATE_USER",
            "Validate_User",
            "raw_symbol",
            "rawSymbol",
            "RawSymbol",
            "raw-symbol",
            "RAW_SYMBOL",
            "Raw_Symbol",
        ]
        .iter()
        .enumerate()
        .map(|(idx, identifier)| TokenSavingsRawSymbol {
            identifier: (*identifier).to_string(),
            file: format!("src/example_{idx}.rs"),
            line: (idx + 1) as u64,
            context: "function".to_string(),
        })
        .collect();
        let fixture = TokenSavingsFixture {
            schema_version: 1,
            description: "fixture".to_string(),
            token_estimate: "ceil(utf8_bytes / 4)".to_string(),
            cases: vec![TokenSavingsFixtureCase {
                name: "search-preview".to_string(),
                surface: "search".to_string(),
                minimum_savings_percent: 40.0,
                raw_symbols,
                tagpath_families: vec![
                    TokenSavingsFamily {
                        canonical: "validate_user".to_string(),
                        count: 6,
                        aliases: BTreeMap::new(),
                    },
                    TokenSavingsFamily {
                        canonical: "raw_symbol".to_string(),
                        count: 6,
                        aliases: BTreeMap::new(),
                    },
                ],
                context_pack_inputs: None,
                session_review_inputs: None,
            }],
        };

        let report = build_token_savings_report(&fixture).unwrap();

        assert!(report.pass);
        assert_eq!(report.cases[0].raw_symbol_count, 12);
        assert_eq!(report.cases[0].family_count, 2);
        assert_eq!(report.cases[0].status, "pass");
        assert!(report.cases[0].byte_delta > 0);
        assert!(report.cases[0].raw_estimated_tokens > report.cases[0].envelope_estimated_tokens);
        assert!(report.cases[0].savings_percent >= 40.0);
    }

    #[test]
    fn search_budget_report_truncates_symbol_preview_and_emits_stable_handle() {
        let response = empty_search_response(Path::new("/repo"), "lexical");
        let symbol_hits = vec![index::SymbolHit {
            name: "alpha_helper_with_a_long_name".to_string(),
            kind: "function".to_string(),
            language: "rust".to_string(),
            file: "/repo/src/lib.rs".to_string(),
            line: 12,
            end_line: None,
            tags: None,
            score: 0.98,
            match_type: "exact_name".to_string(),
        }];

        let report = build_search_budget_report(
            "alpha_helper_with_a_long_name",
            "lexical",
            Path::new("/repo"),
            &response,
            &symbol_hits,
            false,
            ResponseBudget::new(Some(1), Some(12)),
        );

        assert_eq!(report.symbols.len(), 1);
        assert!(report.symbols[0].handle.starts_with("sfam-"));
        assert_eq!(report.symbols[0].tag_alias.as_deref(), Some("alpha/hel..."));
        assert_eq!(report.symbols[0].name, "alpha_hel...");
        assert_eq!(report.symbols[0].file, "src/lib.rs");
        assert!(report.symbols[0].expand.contains("tsift search"));
    }

    #[test]
    fn search_budget_report_groups_repeated_symbols_by_canonical_tag_family() {
        let response = empty_search_response(Path::new("/repo"), "lexical");
        let symbol_hits = vec![
            index::SymbolHit {
                name: "alpha_helper".to_string(),
                kind: "function".to_string(),
                language: "rust".to_string(),
                file: "/repo/src/lib.rs".to_string(),
                line: 12,
                end_line: None,
                tags: Some("alpha,helper".to_string()),
                score: 0.98,
                match_type: "exact_name".to_string(),
            },
            index::SymbolHit {
                name: "alphaHelper".to_string(),
                kind: "method".to_string(),
                language: "rust".to_string(),
                file: "/repo/src/main.rs".to_string(),
                line: 34,
                end_line: None,
                tags: Some("alpha,helper".to_string()),
                score: 0.93,
                match_type: "tag_overlap".to_string(),
            },
            index::SymbolHit {
                name: "alpha_helper".to_string(),
                kind: "function".to_string(),
                language: "rust".to_string(),
                file: "/repo/src/worker.rs".to_string(),
                line: 56,
                end_line: None,
                tags: Some("alpha,helper".to_string()),
                score: 0.91,
                match_type: "tag_overlap".to_string(),
            },
        ];

        let report = build_search_budget_report(
            "alpha helper",
            "lexical",
            Path::new("/repo"),
            &response,
            &symbol_hits,
            false,
            ResponseBudget::new(Some(5), Some(48)),
        );

        assert_eq!(report.symbol_total, 1);
        assert_eq!(report.raw_symbol_total, 3);
        assert_eq!(report.symbols.len(), 1);
        assert_eq!(report.symbols[0].tag_alias.as_deref(), Some("alpha/helper"));
        assert_eq!(report.symbols[0].match_count, 3);
        assert_eq!(report.symbols[0].surface_count, 2);
        assert_eq!(report.symbols[0].file_count, 3);
        assert_eq!(
            report.symbols[0].surface_examples,
            vec!["alpha_helper".to_string(), "alphaHelper".to_string()]
        );
        assert!(report.symbols[0].name.contains("(+1 variant)"));
        assert!(report.symbols[0].file.contains("(+2 files)"));
        assert!(report.symbols[0].expand.contains("tsift search"));
        assert!(report.symbols[0].expand.contains("alpha helper"));
    }

    #[test]
    fn search_budget_report_warns_on_broad_preview_and_lists_narrowing_commands() {
        let mut response = empty_search_response(Path::new("/repo"), "lexical");
        response.indexed_artifacts = 450;
        let symbol_hits = vec![
            index::SymbolHit {
                name: "alpha_helper".to_string(),
                kind: "function".to_string(),
                language: "rust".to_string(),
                file: "/repo/src/lib.rs".to_string(),
                line: 12,
                end_line: None,
                tags: Some("alpha,helper".to_string()),
                score: 0.98,
                match_type: "exact_name".to_string(),
            },
            index::SymbolHit {
                name: "beta_helper".to_string(),
                kind: "function".to_string(),
                language: "rust".to_string(),
                file: "/repo/src/beta.rs".to_string(),
                line: 21,
                end_line: None,
                tags: Some("beta,helper".to_string()),
                score: 0.92,
                match_type: "tag_overlap".to_string(),
            },
        ];

        let report = build_search_budget_report(
            "helper",
            "lexical",
            Path::new("/repo"),
            &response,
            &symbol_hits,
            false,
            ResponseBudget::new(Some(1), Some(64)),
        );

        let guard = report
            .scale_guard
            .as_ref()
            .expect("broad previews should emit a scale guard");
        assert_eq!(guard.level, "high-hit");
        assert_eq!(guard.signals.indexed_artifacts, 450);
        assert_eq!(guard.signals.raw_symbol_matches, 2);
        assert!(
            guard
                .narrow_commands
                .iter()
                .any(|command| command.contains("--exact"))
        );
        assert!(
            guard
                .narrow_commands
                .iter()
                .any(|command| command.contains("alpha helper"))
        );
        assert!(
            guard
                .narrow_commands
                .last()
                .unwrap()
                .contains("workflow search")
        );
    }

    #[test]
    fn explain_budget_report_limits_edges_and_members() {
        let symbols = vec![index::StoredSymbol {
            name: "alpha_helper".to_string(),
            kind: "function".to_string(),
            language: "rust".to_string(),
            signature: None,
            file: "src/lib.rs".to_string(),
            line: 10,
            end_line: None,
            parent_module: None,
            visibility: None,
            tags: None,
        }];
        let callers = vec![
            index::StoredEdge {
                caller_file: "src/main.rs".to_string(),
                caller_name: "main".to_string(),
                caller_line: 1,
                callee_name: "alpha_helper".to_string(),
                call_site_line: 3,
            },
            index::StoredEdge {
                caller_file: "src/worker.rs".to_string(),
                caller_name: "worker".to_string(),
                caller_line: 5,
                callee_name: "alpha_helper".to_string(),
                call_site_line: 8,
            },
        ];
        let community = graph::Community {
            id: 1,
            members: vec![
                "alpha_helper".to_string(),
                "main".to_string(),
                "worker".to_string(),
            ],
            modularity_contribution: 0.5,
        };

        let report = build_explain_budget_report(
            "alpha_helper",
            Path::new("/repo"),
            &symbols,
            &callers,
            2,
            false,
            &[],
            0,
            false,
            Some(&community),
            ResponseBudget::new(Some(1), Some(24)),
        );

        assert_eq!(report.definitions.len(), 1);
        assert_eq!(report.callers.len(), 1);
        assert!(report.truncated);
        assert_eq!(report.community.as_ref().unwrap().members.len(), 1);
        assert_eq!(
            report.definitions[0].tag_alias.as_deref(),
            Some("alpha/helper")
        );
        assert!(report.callers[0].handle.starts_with("ecall-"));
        assert_eq!(report.callers[0].tag_alias.as_deref(), Some("main"));
    }

    #[test]
    fn session_review_next_context_budget_limits_lists() {
        let report = session_review::SessionReviewReport {
            root: "/repo".to_string(),
            target: "tasks/software/tsift.md".to_string(),
            target_kind: "file".to_string(),
            sessions_considered: 1,
            sessions_matched: 1,
            claude_sessions: 1,
            codex_sessions: 0,
            agent_doc_logs: 0,
            prompt_target_count: 2,
            command_groups: 0,
            file_groups: 2,
            symbol_groups: 1,
            failure_groups: 1,
            runtime_event_groups: 0,
            restart_churn_groups: 0,
            closeout_groups: 0,
            usage_samples: 1,
            prompt_tokens: 120,
            cached_input_tokens: 80,
            cache_creation_input_tokens: 0,
            output_tokens: 40,
            reasoning_output_tokens: 0,
            total_tokens: 240,
            cached_input_ratio: Some(40.0),
            largest_turn_total_tokens: 240,
            guardrails: vec![],
            loop_clusters: vec![],
            file_read_diagnostics: vec![],
            prompt_targets: vec![
                session_review::SessionReviewPromptTarget {
                    text: "do one".to_string(),
                    occurrences: 1,
                },
                session_review::SessionReviewPromptTarget {
                    text: "do two".to_string(),
                    occurrences: 1,
                },
            ],
            commands: vec![],
            touched_files: vec![],
            touched_symbols: vec![],
            failures: vec![],
            runtime_events: vec![],
            restart_churn: vec![],
            closeout: vec![],
            largest_turns: vec![],
            sessions: vec![session_review::SessionReviewSession {
                source: "claude_jsonl".to_string(),
                path: "/tmp/session.jsonl".to_string(),
                matched_by: vec!["path".to_string()],
                modified_unix_secs: None,
                prompt_target_count: 2,
                command_groups: 0,
                file_groups: 2,
                symbol_groups: 1,
                failure_groups: 1,
                runtime_event_groups: 0,
                restart_churn_groups: 0,
                closeout_groups: 0,
                usage_samples: 1,
                prompt_tokens: 120,
                cached_input_tokens: 80,
                cache_creation_input_tokens: 0,
                output_tokens: 40,
                reasoning_output_tokens: 0,
                total_tokens: 240,
            }],
            next_context: session_review::SessionReviewNextContext {
                target: "tasks/software/tsift.md".to_string(),
                active_prompt_targets: vec!["do one".to_string(), "do two".to_string()],
                last_verification: session_review::SessionReviewVerificationState {
                    status: "green".to_string(),
                    detail: "cargo test".to_string(),
                },
                touched_files: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
                touched_symbols: vec!["alpha_helper".to_string(), "main".to_string()],
                unresolved_failures: vec![session_review::SessionReviewFailure {
                    kind: "timeout".to_string(),
                    message: "search timed out".to_string(),
                    occurrences: 1,
                    command: None,
                    session_path: None,
                }],
                next_digest_commands: vec![
                    "tsift session-review --next-context tasks/software/tsift.md".to_string(),
                    "tsift diff-digest .".to_string(),
                    "tsift test-digest --path . < target/very-long-test-output-file-name-that-must-remain-executable.log".to_string(),
                    "tsift log-digest --path . < target/very-long-build-output-file-name-that-must-remain-executable.log".to_string(),
                ],
            },
            warnings: vec![],
        };

        let budget_report = build_session_review_next_context_budget_report(
            &report,
            ResponseBudget::new(Some(1), Some(12)),
            None,
        );

        assert!(budget_report.truncated);
        assert_eq!(budget_report.prompt_targets, vec!["do one"]);
        assert_eq!(budget_report.touched_files, vec!["src/lib.rs"]);
        assert!(
            budget_report.touched_symbol_refs[0]
                .handle
                .starts_with("ncsym-")
        );
        assert_eq!(
            budget_report.touched_symbol_refs[0].tag_alias.as_deref(),
            Some("alpha/helper")
        );
        assert!(
            budget_report.unresolved_failures[0]
                .handle
                .starts_with("snf-")
        );
        assert_eq!(budget_report.next_digest_commands.len(), 4);
        assert_eq!(
            budget_report.next_digest_commands[2],
            "tsift test-digest --path . < target/very-long-test-output-file-name-that-must-remain-executable.log"
        );
    }

    #[test]
    fn context_pack_diff_preview_limits_files_and_symbols() {
        let report = diff_digest::DiffDigestReport {
            root: "/repo".to_string(),
            mode: diff_digest::DiffDigestMode::WorkingTree,
            revision: None,
            files_changed: 2,
            files_with_current_summaries: 1,
            symbols_touched: 3,
            call_edges_added: 1,
            call_edges_removed: 0,
            files: vec![
                diff_digest::DiffDigestFile {
                    path: "src/lib.rs".to_string(),
                    status: diff_digest::DiffDigestFileStatus::Modified,
                    touched_symbols: vec!["alpha_helper".to_string(), "beta_helper".to_string()],
                    summary_state: diff_digest::DiffDigestSummaryState::Current,
                    current_summaries: vec![diff_digest::DiffDigestSummarySnippet {
                        symbol: "alpha_helper".to_string(),
                        summary: "alpha helper handles the main alpha workflow".to_string(),
                    }],
                    added_call_edges: vec!["alpha->beta".to_string()],
                    removed_call_edges: vec![],
                    warnings: vec!["stale parse".to_string()],
                },
                diff_digest::DiffDigestFile {
                    path: "src/main.rs".to_string(),
                    status: diff_digest::DiffDigestFileStatus::Added,
                    touched_symbols: vec!["main".to_string()],
                    summary_state: diff_digest::DiffDigestSummaryState::Missing,
                    current_summaries: vec![],
                    added_call_edges: vec![],
                    removed_call_edges: vec![],
                    warnings: vec![],
                },
            ],
        };

        let preview =
            build_context_pack_diff_preview(&report, ResponseBudget::new(Some(1), Some(11)), None);

        assert!(preview.truncated);
        assert_eq!(preview.files.len(), 1);
        assert_eq!(preview.files[0].path, "src/lib.rs");
        assert_eq!(preview.files[0].touched_symbols, vec!["alpha_he..."]);
        assert!(
            preview.files[0].touched_symbol_refs[0]
                .handle
                .starts_with("cdsym-")
        );
        assert_eq!(
            preview.files[0].touched_symbol_refs[0].tag_alias.as_deref(),
            Some("alpha/he...")
        );
        assert!(
            preview.files[0].summary_refs[0]
                .handle
                .starts_with("cdsum-")
        );
        assert_eq!(
            preview.files[0].summary_refs[0].tag_alias.as_deref(),
            Some("alpha/he...")
        );
        assert_eq!(preview.files[0].summary_refs[0].summary, "alpha he...");
        assert_eq!(
            preview.files[0].summary_refs[0].expand,
            "tsift summarize --file \"src/lib.rs\""
        );
        assert_eq!(preview.files[0].warnings, vec!["stale parse"]);
    }

    #[test]
    fn context_pack_diff_preview_attaches_tag_ontology_refs() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".naming/tags")).unwrap();
        fs::write(
            root.path().join(".naming/tags/alpha.md"),
            "+++\ntag = \"alpha\"\ntitle = \"Alpha Domain\"\ndomain = \"fixture\"\n+++\n\nAlpha definition.\n",
        )
        .unwrap();
        let ontology = load_tag_ontology_preview_context(root.path()).unwrap();
        let report = diff_digest::DiffDigestReport {
            root: root.path().display().to_string(),
            mode: diff_digest::DiffDigestMode::WorkingTree,
            revision: None,
            files_changed: 1,
            files_with_current_summaries: 1,
            symbols_touched: 1,
            call_edges_added: 0,
            call_edges_removed: 0,
            files: vec![diff_digest::DiffDigestFile {
                path: "src/lib.rs".to_string(),
                status: diff_digest::DiffDigestFileStatus::Modified,
                touched_symbols: vec!["alpha_helper".to_string()],
                summary_state: diff_digest::DiffDigestSummaryState::Current,
                current_summaries: vec![diff_digest::DiffDigestSummarySnippet {
                    symbol: "alpha_helper".to_string(),
                    summary: "alpha helper summary".to_string(),
                }],
                added_call_edges: vec![],
                removed_call_edges: vec![],
                warnings: vec![],
            }],
        };

        let preview = build_context_pack_diff_preview(
            &report,
            ResponseBudget::new(Some(1), Some(80)),
            Some(&ontology),
        );

        let symbol_ref = &preview.files[0].touched_symbol_refs[0].ontology_refs[0];
        assert!(symbol_ref.handle.starts_with("tont-"));
        assert_eq!(symbol_ref.tag, "alpha");
        assert_eq!(symbol_ref.path, ".naming/tags/alpha.md");
        assert_eq!(symbol_ref.title.as_deref(), Some("Alpha Domain"));
        assert_eq!(symbol_ref.domain.as_deref(), Some("fixture"));
        assert_eq!(
            preview.files[0].summary_refs[0].ontology_refs[0].path,
            ".naming/tags/alpha.md"
        );
    }

    #[test]
    fn context_pack_test_preview_limits_failure_groups() {
        let report = test_digest::TestDigestReport {
            root: "/repo".to_string(),
            runner: "cargo".to_string(),
            failures: 2,
            grouped_failures: 2,
            counts: test_digest::TestDigestCounts {
                passed: Some(8),
                failed: Some(2),
                skipped: Some(1),
            },
            failure_groups: vec![
                test_digest::TestDigestFailure {
                    tests: vec!["suite::alpha_failure".to_string()],
                    message: "assertion failed".to_string(),
                    path: Some("src/lib.rs".to_string()),
                    line: Some(42),
                    column: None,
                    occurrences: 1,
                    summary_state: test_digest::TestDigestSummaryState::Current,
                    current_summaries: vec![test_digest::TestDigestSummarySnippet {
                        symbol: "alpha_failure".to_string(),
                        summary: "failure summary for alpha test".to_string(),
                    }],
                },
                test_digest::TestDigestFailure {
                    tests: vec!["suite::beta_failure".to_string()],
                    message: "panic".to_string(),
                    path: Some("src/main.rs".to_string()),
                    line: Some(7),
                    column: None,
                    occurrences: 1,
                    summary_state: test_digest::TestDigestSummaryState::Missing,
                    current_summaries: vec![],
                },
            ],
            warnings: vec!["warning text".to_string()],
        };

        let preview =
            build_context_pack_test_preview(&report, ResponseBudget::new(Some(1), Some(14)), None);

        assert!(preview.truncated);
        assert_eq!(preview.failure_groups.len(), 1);
        assert_eq!(preview.failure_groups[0].tests, vec!["suite::alph..."]);
        assert_eq!(preview.failure_groups[0].message, "assertion f...");
        assert!(
            preview.failure_groups[0].summary_refs[0]
                .handle
                .starts_with("ctsum-")
        );
        assert_eq!(
            preview.failure_groups[0].summary_refs[0].expand,
            "tsift summarize --file \"src/lib.rs\""
        );
        assert_eq!(preview.warnings, vec!["warning text"]);
    }

    #[test]
    fn context_pack_log_preview_limits_signals_and_refs() {
        let report = log_digest::LogDigestReport {
            root: "/repo".to_string(),
            total_lines: 12,
            non_empty_lines: 10,
            signal_groups: 2,
            repeated_line_groups: 2,
            repeated_line_occurrences: 3,
            file_ref_groups: 2,
            symbol_ref_groups: 2,
            stack_groups: 1,
            signals: vec![
                log_digest::LogDigestSignal {
                    severity: "error".to_string(),
                    message: "src/lib.rs:42 boom".to_string(),
                    path: Some("src/lib.rs".to_string()),
                    line: Some(42),
                    column: None,
                    occurrences: 2,
                    summary_state: log_digest::LogDigestSummaryState::Current,
                    current_summaries: vec![log_digest::LogDigestSummarySnippet {
                        symbol: "alpha_helper".to_string(),
                        summary: "alpha helper cached log summary".to_string(),
                    }],
                },
                log_digest::LogDigestSignal {
                    severity: "warn".to_string(),
                    message: "slow path".to_string(),
                    path: None,
                    line: None,
                    column: None,
                    occurrences: 1,
                    summary_state: log_digest::LogDigestSummaryState::Unavailable,
                    current_summaries: vec![],
                },
            ],
            repeated_lines: vec![
                log_digest::LogDigestRepeatedLine {
                    line: "retrying work item alpha".to_string(),
                    occurrences: 3,
                },
                log_digest::LogDigestRepeatedLine {
                    line: "retrying work item beta".to_string(),
                    occurrences: 2,
                },
            ],
            file_refs: vec![
                log_digest::LogDigestFileRef {
                    path: "src/lib.rs".to_string(),
                    line: Some(42),
                    column: None,
                    occurrences: 2,
                    summary_state: log_digest::LogDigestSummaryState::Current,
                    current_summaries: vec![log_digest::LogDigestSummarySnippet {
                        symbol: "alpha_helper".to_string(),
                        summary: "alpha helper cached file summary".to_string(),
                    }],
                },
                log_digest::LogDigestFileRef {
                    path: "src/main.rs".to_string(),
                    line: Some(7),
                    column: None,
                    occurrences: 1,
                    summary_state: log_digest::LogDigestSummaryState::Missing,
                    current_summaries: vec![],
                },
            ],
            symbol_refs: vec![
                log_digest::LogDigestSymbolRef {
                    symbol: "alpha_helper".to_string(),
                    occurrences: 2,
                    summary_state: log_digest::LogDigestSummaryState::Current,
                    current_summaries: vec![log_digest::LogDigestSummarySnippet {
                        symbol: "alpha_helper".to_string(),
                        summary: "alpha helper cached symbol summary".to_string(),
                    }],
                },
                log_digest::LogDigestSymbolRef {
                    symbol: "beta_helper".to_string(),
                    occurrences: 1,
                    summary_state: log_digest::LogDigestSummaryState::Missing,
                    current_summaries: vec![],
                },
            ],
            stack_traces: vec![log_digest::LogDigestStackGroup {
                frames: vec!["frame one".to_string()],
                occurrences: 1,
            }],
            warnings: vec!["warning text".to_string()],
        };

        let preview =
            build_context_pack_log_preview(&report, ResponseBudget::new(Some(1), Some(14)), None);

        assert!(preview.truncated);
        assert_eq!(preview.signals.len(), 1);
        assert_eq!(preview.signals[0].message, "src/lib.rs:...");
        assert_eq!(preview.repeated_lines[0].line, "retrying wo...");
        assert_eq!(preview.file_refs.len(), 1);
        assert_eq!(preview.symbol_refs[0].symbol, "alpha_helper");
        assert!(
            preview.signals[0].summary_refs[0]
                .handle
                .starts_with("clsum-")
        );
        assert!(
            preview.file_refs[0].summary_refs[0]
                .handle
                .starts_with("clfsum-")
        );
        assert!(
            preview.symbol_refs[0].summary_refs[0]
                .handle
                .starts_with("clssum-")
        );
        assert_eq!(
            preview.symbol_refs[0].summary_refs[0].tag_alias.as_deref(),
            Some("alpha/helper")
        );
        assert_eq!(
            preview.symbol_refs[0].summary_refs[0].expand,
            "tsift summarize \"alpha_helper\""
        );
        assert_eq!(preview.warnings, vec!["warning text"]);
    }

    #[test]
    fn cli_search_rejects_exact_with_strategy_flag() {
        let cli = Cli::try_parse_from([
            "tsift",
            "search",
            "test",
            "--exact",
            "--strategy",
            "lexical",
        ]);
        assert!(cli.is_err());
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

// --- Command rewriting for hook integrations and manual bounded execution ---

/// Exit codes for `tsift rewrite` (matches rtk protocol):
///   0 + stdout → rewrite found, auto-allow
///   1          → no tsift equivalent, pass through
fn cmd_rewrite(command: &str, run: bool, format: OutputFormat) -> Result<()> {
    let rewritten = match rewrite_command(command) {
        Some(rewritten) => rewritten,
        None => std::process::exit(1),
    };
    let rewritten = apply_rewrite_output_format(&rewritten, format);

    if !run {
        print!("{}", rewritten);
        return Ok(());
    }

    let status_code = execute_rewritten_command(&rewritten)?;
    std::process::exit(status_code);
}

#[derive(Clone, Copy)]
struct OutputCap {
    max_lines: usize,
    strip_prefix: Option<&'static str>,
}

fn execute_rewritten_command(command: &str) -> Result<i32> {
    let effective_command = effective_rewrite_run_command(command);
    let parts = shell_split(&effective_command);
    let Some(program) = parts.first().map(|part| strip_shell_quotes(part)) else {
        bail!("rewritten command was empty");
    };
    let args: Vec<String> = parts[1..]
        .iter()
        .map(|part| strip_shell_quotes(part).to_string())
        .collect();
    let mut command = if program == "tsift" {
        Command::new(std::env::current_exe().context("resolving current tsift executable")?)
    } else {
        Command::new(program)
    };
    let output = command
        .args(&args)
        .output()
        .with_context(|| format!("executing rewritten command `{effective_command}`"))?;

    let stdout = if let Some(cap) = rewrite_output_cap(&effective_command) {
        apply_output_cap(&output.stdout, cap)
    } else {
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    if !stdout.is_empty() {
        print!("{stdout}");
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }

    Ok(output
        .status
        .code()
        .unwrap_or_else(|| if output.status.success() { 0 } else { 1 }))
}

fn effective_rewrite_run_command(command: &str) -> String {
    let parts = shell_split(command);
    if parts.first().map(|part| strip_shell_quotes(part)) != Some("tsift") {
        return command.to_string();
    }
    let structured = parts
        .iter()
        .skip(1)
        .any(|part| strip_shell_quotes(part) == "--timeout");
    let subcommand = parts
        .iter()
        .skip(1)
        .map(|part| strip_shell_quotes(part))
        .find(|part| !part.starts_with('-'));
    if matches!(subcommand, Some("search")) && !structured {
        format!("{command} --timeout 0")
    } else {
        command.to_string()
    }
}

fn apply_rewrite_output_format(command: &str, format: OutputFormat) -> String {
    let trimmed = command.trim_start();
    let Some(rest) = trimmed.strip_prefix("tsift") else {
        return command.to_string();
    };
    let existing_parts = shell_split(rest);

    let mut flags = Vec::new();
    if format.compact && !rewrite_has_global_flag(&existing_parts, "--compact") {
        flags.push("--compact");
    }
    if format.pretty && !rewrite_has_global_flag(&existing_parts, "--pretty") {
        flags.push("--pretty");
    }
    if format.terse && !rewrite_has_global_flag(&existing_parts, "--terse") {
        flags.push("--terse");
    }
    if format.schema && !rewrite_has_global_flag(&existing_parts, "--schema") {
        flags.push("--schema");
    }
    if format.envelope {
        if !rewrite_has_global_flag(&existing_parts, "--envelope") {
            flags.push("--envelope");
        }
    } else if format.json_output
        && !rewrite_has_global_flag(&existing_parts, "--json")
        && !rewrite_has_global_flag(&existing_parts, "--envelope")
    {
        flags.push("--json");
    }

    if flags.is_empty() {
        return command.to_string();
    }

    let forwarded = flags.join(" ");
    if rest.trim().is_empty() {
        format!("tsift {forwarded}")
    } else {
        format!("tsift {forwarded}{rest}")
    }
}

fn rewrite_has_global_flag(parts: &[&str], flag: &str) -> bool {
    parts
        .iter()
        .take_while(|part| {
            let value = strip_shell_quotes(part);
            value.starts_with('-') || value == "tsift"
        })
        .any(|part| strip_shell_quotes(part) == flag)
}

fn rewrite_output_cap(command: &str) -> Option<OutputCap> {
    let parts = shell_split(command);
    if strip_shell_quotes(parts.first()?) != "tsift" {
        return None;
    }
    let structured = parts.iter().skip(1).any(|part| {
        matches!(
            strip_shell_quotes(part),
            "--json" | "--terse" | "--schema" | "--tabular" | "--envelope"
        )
    });
    if structured {
        return None;
    }

    let subcommand = parts
        .iter()
        .skip(1)
        .map(|part| strip_shell_quotes(part))
        .find(|part| !part.starts_with('-'))?;
    match subcommand {
        "communities" => Some(OutputCap {
            max_lines: 80,
            strip_prefix: None,
        }),
        "explain" => Some(OutputCap {
            max_lines: 40,
            strip_prefix: None,
        }),
        "graph" => Some(OutputCap {
            max_lines: 50,
            strip_prefix: None,
        }),
        "index" => Some(OutputCap {
            max_lines: 30,
            strip_prefix: None,
        }),
        "search" => Some(OutputCap {
            max_lines: 50,
            strip_prefix: Some("Strategy:"),
        }),
        _ => None,
    }
}

fn apply_output_cap(stdout: &[u8], cap: OutputCap) -> String {
    let cleaned = strip_ansi_codes(&String::from_utf8_lossy(stdout));
    let mut lines: Vec<String> = cleaned
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .filter(|line| {
            cap.strip_prefix
                .map(|prefix| !line.starts_with(prefix))
                .unwrap_or(true)
        })
        .map(ToOwned::to_owned)
        .collect();
    if lines.len() > cap.max_lines {
        let hidden = lines.len() - cap.max_lines;
        lines.truncate(cap.max_lines);
        lines.push(format!(
            "... (+{hidden} more lines; rerun the underlying tsift command directly for the full output)"
        ));
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn strip_ansi_codes(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        output.push(ch);
    }
    output
}

/// Attempt to rewrite a shell command to use tsift.
/// Returns Some(rewritten) if applicable, None if no match.
pub(crate) fn rewrite_command(command: &str) -> Option<String> {
    let trimmed = command.trim();

    // Already a tsift command — pass through (exit 0, identical)
    if trimmed.starts_with("tsift ") || trimmed == "tsift" {
        return Some(command.to_string());
    }

    // rg <pattern> [path] [flags] → tsift search "<pattern>" --exact [--path <path>]
    if let Some(rewritten) = rewrite_rg(trimmed) {
        return Some(rewritten);
    }

    // grep -r <pattern> [path] → tsift search "<pattern>" --exact [--path <path>]
    if let Some(rewritten) = rewrite_grep(trimmed) {
        return Some(rewritten);
    }

    // git diff / git show / patch-style history → tsift diff-digest
    if let Some(rewritten) = rewrite_git_diff(trimmed) {
        return Some(rewritten);
    }
    if let Some(rewritten) = rewrite_git_show(trimmed) {
        return Some(rewritten);
    }
    if let Some(rewritten) = rewrite_git_patch_history(trimmed) {
        return Some(rewritten);
    }

    // long session/doc transcript reads → tsift session-digest
    if let Some(rewritten) = rewrite_session_read_command(trimmed) {
        return Some(rewritten);
    }

    // large source-file reads inside indexed repos → tsift source-read windows
    if let Some(rewritten) = rewrite_source_read_command(trimmed) {
        return Some(rewritten);
    }

    // cargo test / pytest → tsift-owned test digest wrapper that preserves exit status
    if let Some(rewritten) = rewrite_test_command(trimmed) {
        return Some(rewritten);
    }

    // verbose build/check/install commands → tsift-owned log digest wrapper
    if let Some(rewritten) = rewrite_log_command(trimmed) {
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

    Some(build_agent_search_preview_command(pattern?, path))
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

    Some(build_agent_search_preview_command(pattern?, path))
}

fn build_agent_search_preview_command(pattern: &str, path: Option<&str>) -> String {
    let mut result = format!(
        "tsift --envelope search {} --exact --budget normal",
        shell_quote(pattern)
    );
    if let Some(p) = path {
        result.push_str(&format!(" --path {}", shell_quote(p)));
    }
    result
}

fn rewrite_git_diff(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() < 2 || parts[0] != "git" || parts[1] != "diff" {
        return None;
    }
    let mut cached = false;
    let mut path = None;
    let mut after_double_dash = false;

    for part in &parts[2..] {
        if after_double_dash {
            if path.is_none() && !part.starts_with('-') {
                path = Some(*part);
                continue;
            }
            return None;
        }
        match *part {
            "--cached" | "--staged" => cached = true,
            "--" => after_double_dash = true,
            raw if looks_like_path_selector(raw) => {
                if path.replace(raw).is_some() {
                    return None;
                }
            }
            _ => return None,
        }
    }

    Some(build_diff_digest_command(path.unwrap_or("."), cached, None))
}

fn rewrite_git_show(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() < 2 || parts[0] != "git" || parts[1] != "show" {
        return None;
    }

    let mut revision = "HEAD";
    let mut path = None;
    let mut after_double_dash = false;

    for part in &parts[2..] {
        if after_double_dash {
            if path.is_none() && !part.starts_with('-') {
                path = Some(*part);
                continue;
            }
            return None;
        }
        match *part {
            "--" => after_double_dash = true,
            "-p" | "--patch" | "--stat" => {}
            raw if raw.starts_with("--format=") => {}
            raw if !raw.starts_with('-') => {
                if revision != "HEAD" {
                    return None;
                }
                revision = raw;
            }
            _ => return None,
        }
    }

    Some(build_diff_digest_command(
        path.unwrap_or("."),
        false,
        Some(revision),
    ))
}

fn rewrite_git_patch_history(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() < 2 || parts[0] != "git" || parts[1] != "log" {
        return None;
    }

    let mut saw_patch = false;
    let mut saw_single_commit = false;
    let mut revision = "HEAD";
    let mut path = None;
    let mut after_double_dash = false;
    let mut skip_next = false;

    for part in &parts[2..] {
        if skip_next {
            skip_next = false;
            if *part == "1" {
                saw_single_commit = true;
                continue;
            }
            return None;
        }
        if after_double_dash {
            if path.is_none() && !part.starts_with('-') {
                path = Some(*part);
                continue;
            }
            return None;
        }
        match *part {
            "--" => after_double_dash = true,
            "-p" | "--patch" => saw_patch = true,
            "-1" | "-n1" | "--max-count=1" => saw_single_commit = true,
            "-n" | "--max-count" => skip_next = true,
            raw if !raw.starts_with('-') => {
                if revision != "HEAD" {
                    return None;
                }
                revision = raw;
            }
            _ => return None,
        }
    }

    if !saw_patch || !saw_single_commit {
        return None;
    }

    Some(build_diff_digest_command(
        path.unwrap_or("."),
        false,
        Some(revision),
    ))
}

fn build_diff_digest_command(path: &str, cached: bool, revision: Option<&str>) -> String {
    let mut result = "tsift diff-digest".to_string();
    if cached {
        result.push_str(" --cached");
    }
    if let Some(revision) = revision {
        result.push_str(&format!(" --revision {}", shell_quote(revision)));
    }
    if path == "." {
        result.push_str(" .");
    } else {
        result.push_str(&format!(" {}", shell_quote(path)));
    }
    result
}

const SESSION_READ_LINE_THRESHOLD: usize = 80;
const SOURCE_READ_LINE_THRESHOLD: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileReadWindow {
    FullFile,
    FromStart { lines: usize },
    FromEnd { lines: usize },
    Range { start: usize, lines: usize },
}

struct FileReadTarget {
    input: String,
    requested_lines: Option<usize>,
    window: FileReadWindow,
}

fn rewrite_session_read_command(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let target = parse_file_read_target(cmd)?;
    let input_path = Path::new(&target.input);
    let source = detect_session_digest_source(input_path)?;

    if let Some(requested_lines) = target.requested_lines {
        if requested_lines < SESSION_READ_LINE_THRESHOLD {
            return None;
        }
    } else if !file_has_at_least_lines(input_path, SESSION_READ_LINE_THRESHOLD) {
        return None;
    }

    let digest_path = resolve_digest_context_path(input_path);
    Some(build_session_digest_command(
        &digest_path,
        &target.input,
        source,
    ))
}

fn rewrite_source_read_command(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let target = parse_file_read_target(cmd)?;
    let input_path = Path::new(&target.input);
    if !file_is_supported_source(input_path) {
        return None;
    }

    if let Some(requested_lines) = target.requested_lines {
        if requested_lines < SOURCE_READ_LINE_THRESHOLD {
            return None;
        }
    } else if !file_has_at_least_lines(input_path, SOURCE_READ_LINE_THRESHOLD) {
        return None;
    }

    let root = lint::find_project_root_for_path(input_path).ok()??;
    if !project_has_index(&root) {
        return None;
    }
    let file_abs = input_path.canonicalize().ok()?;
    let file_display = relativize_pathbuf(&file_abs, &root)
        .to_string_lossy()
        .to_string();
    let total_lines = count_file_lines(&file_abs)?;
    let (start, lines) = source_window_for_read(target.window, total_lines)?;
    Some(build_source_read_rewrite_command(
        &root,
        &file_display,
        start,
        lines,
    ))
}

fn parse_file_read_target(cmd: &str) -> Option<FileReadTarget> {
    let parts: Vec<&str> = shell_split(cmd);
    let head = parts.first().copied()?;
    match head {
        "cat" | "bat" | "batcat" => parse_cat_like_read_target(&parts),
        "head" | "tail" => parse_head_tail_read_target(&parts),
        "sed" => parse_sed_read_target(&parts),
        _ => None,
    }
}

fn parse_cat_like_read_target(parts: &[&str]) -> Option<FileReadTarget> {
    let mut input = None;
    for part in &parts[1..] {
        if part.starts_with('-') {
            continue;
        }
        if input.replace(strip_shell_quotes(part)).is_some() {
            return None;
        }
    }
    Some(FileReadTarget {
        input: input?.to_string(),
        requested_lines: None,
        window: FileReadWindow::FullFile,
    })
}

fn parse_head_tail_read_target(parts: &[&str]) -> Option<FileReadTarget> {
    let mut requested_lines = 10;
    let mut input = None;
    let mut index = 1;

    while index < parts.len() {
        let part = parts[index];
        if part == "-n" || part == "--lines" {
            index += 1;
            requested_lines = parse_requested_line_count(parts.get(index).copied()?)?;
            index += 1;
            continue;
        }
        if let Some(raw) = part.strip_prefix("-n")
            && !raw.is_empty()
        {
            requested_lines = parse_requested_line_count(raw)?;
            index += 1;
            continue;
        }
        if let Some(raw) = part.strip_prefix("--lines=") {
            requested_lines = parse_requested_line_count(raw)?;
            index += 1;
            continue;
        }
        if part.starts_with('-') && part[1..].chars().all(|ch| ch.is_ascii_digit()) {
            requested_lines = parse_requested_line_count(&part[1..])?;
            index += 1;
            continue;
        }
        if input.replace(strip_shell_quotes(part)).is_some() {
            return None;
        }
        index += 1;
    }

    let window = match parts[0] {
        "head" => FileReadWindow::FromStart {
            lines: requested_lines,
        },
        "tail" => FileReadWindow::FromEnd {
            lines: requested_lines,
        },
        _ => return None,
    };

    Some(FileReadTarget {
        input: input?.to_string(),
        requested_lines: Some(requested_lines),
        window,
    })
}

fn parse_sed_read_target(parts: &[&str]) -> Option<FileReadTarget> {
    if parts.len() != 4 || parts[1] != "-n" {
        return None;
    }

    let (start, lines) = parse_sed_print_window(parts[2])?;
    Some(FileReadTarget {
        input: strip_shell_quotes(parts[3]).to_string(),
        requested_lines: Some(lines),
        window: FileReadWindow::Range { start, lines },
    })
}

fn parse_requested_line_count(raw: &str) -> Option<usize> {
    let trimmed = strip_shell_quotes(raw);
    if let Some(number) = trimmed.strip_prefix('+') {
        number.parse::<usize>().ok()?;
        return Some(SESSION_READ_LINE_THRESHOLD);
    }
    trimmed.parse::<usize>().ok()
}

fn parse_sed_print_window(raw: &str) -> Option<(usize, usize)> {
    let trimmed = strip_shell_quotes(raw);
    let range = trimmed.strip_suffix('p')?;
    let (start, end) = range.split_once(',')?;
    let start = start.parse::<usize>().ok()?;
    let end = end.parse::<usize>().ok()?;
    (end >= start).then_some((start, end - start + 1))
}

fn file_is_supported_source(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .and_then(lang::Lang::from_extension)
        .is_some()
}

fn count_file_lines(path: &Path) -> Option<usize> {
    let file = fs::File::open(path).ok()?;
    Some(
        BufReader::new(file)
            .lines()
            .filter(|line| line.is_ok())
            .count(),
    )
}

fn source_window_for_read(window: FileReadWindow, total_lines: usize) -> Option<(usize, usize)> {
    if total_lines == 0 {
        return None;
    }
    match window {
        FileReadWindow::FullFile => Some((1, SOURCE_READ_LINE_THRESHOLD.min(total_lines))),
        FileReadWindow::FromStart { lines } => Some((1, lines.min(total_lines))),
        FileReadWindow::FromEnd { lines } => {
            let bounded = lines.min(total_lines);
            Some((total_lines - bounded + 1, bounded))
        }
        FileReadWindow::Range { start, lines } => {
            if start == 0 || start > total_lines {
                return None;
            }
            Some((start, lines.min(total_lines - start + 1)))
        }
    }
}

fn build_source_read_rewrite_command(
    root: &Path,
    file: &str,
    start: usize,
    lines: usize,
) -> String {
    format!(
        "tsift --envelope source-read {} --path {} --start {} --lines {} --budget normal",
        shell_quote(file),
        shell_quote(&root.to_string_lossy()),
        start,
        lines
    )
}

fn project_has_index(root: &Path) -> bool {
    let tsift_dir = root.join(".tsift");
    tsift_dir.join("index.db").is_file() || directory_contains_index_db(&tsift_dir.join("indexes"))
}

fn directory_contains_index_db(path: &Path) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == "index.db") && path.is_file() {
            return true;
        }
        if path.is_dir() && directory_contains_index_db(&path) {
            return true;
        }
    }
    false
}

fn detect_session_digest_source(path: &Path) -> Option<session_digest::SessionDigestSource> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("md") if file_looks_like_agent_doc_session(path) => {
            Some(session_digest::SessionDigestSource::Markdown)
        }
        Some("jsonl") if file_looks_like_claude_jsonl(path) => {
            Some(session_digest::SessionDigestSource::ClaudeJsonl)
        }
        Some("jsonl") if file_looks_like_codex_jsonl(path) => {
            Some(session_digest::SessionDigestSource::CodexJsonl)
        }
        Some("log") if file_looks_like_agent_doc_log(path) => {
            Some(session_digest::SessionDigestSource::AgentDocLog)
        }
        _ => None,
    }
}

fn file_looks_like_agent_doc_session(path: &Path) -> bool {
    let prefix = match read_file_prefix(path, 16 * 1024) {
        Some(prefix) => prefix,
        None => return false,
    };
    prefix.contains("agent_doc_session:")
        || prefix.contains("<!-- agent:exchange")
        || prefix.contains("\n## Exchange")
}

fn file_looks_like_claude_jsonl(path: &Path) -> bool {
    let prefix = match read_file_prefix(path, 16 * 1024) {
        Some(prefix) => prefix,
        None => return false,
    };

    prefix
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .any(|line| {
            let value = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => value,
                Err(_) => return false,
            };
            value.get("message").is_some()
                || value.get("role").is_some()
                || value.get("content").is_some()
        })
}

fn file_looks_like_codex_jsonl(path: &Path) -> bool {
    let prefix = match read_file_prefix(path, 16 * 1024) {
        Some(prefix) => prefix,
        None => return false,
    };

    prefix
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(8)
        .any(|line| {
            let value = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => value,
                Err(_) => return false,
            };
            matches!(
                value.get("type").and_then(serde_json::Value::as_str),
                Some("session_meta" | "response_item" | "event_msg")
            )
        })
}

fn file_looks_like_agent_doc_log(path: &Path) -> bool {
    let prefix = match read_file_prefix(path, 16 * 1024) {
        Some(prefix) => prefix,
        None => return false,
    };
    prefix
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(8)
        .all(|line| line.starts_with('[') && line.contains("] "))
}

fn read_file_prefix(path: &Path, max_bytes: usize) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    reader
        .by_ref()
        .take(max_bytes as u64)
        .read_to_end(&mut buffer)
        .ok()?;
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

fn file_has_at_least_lines(path: &Path, min_lines: usize) -> bool {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let reader = BufReader::new(file);
    reader
        .lines()
        .take(min_lines)
        .filter(|line| line.is_ok())
        .count()
        >= min_lines
}

fn build_session_digest_command(
    path: &str,
    input: &str,
    source: session_digest::SessionDigestSource,
) -> String {
    format!(
        "tsift session-digest --path {} --input {} --source {}",
        shell_quote(path),
        shell_quote(input),
        source.cli_arg()
    )
}

fn resolve_digest_context_path(path: &Path) -> String {
    crate::lint::resolve_harness_root_or_canonical_path(path)
        .map(|root| root.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

fn rewrite_test_command(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() >= 2 && parts[0] == "cargo" && parts[1] == "test" {
        return Some(build_digest_runner_command("test", ".", Some("cargo"), cmd));
    }
    if !parts.is_empty() && parts[0] == "pytest" {
        return Some(build_digest_runner_command(
            "test",
            ".",
            Some("pytest"),
            cmd,
        ));
    }
    if parts.len() >= 3 && parts[0] == "python" && parts[1] == "-m" && parts[2] == "pytest" {
        return Some(build_digest_runner_command(
            "test",
            ".",
            Some("pytest"),
            cmd,
        ));
    }
    None
}

fn rewrite_log_command(cmd: &str) -> Option<String> {
    if has_shell_metacharacters(cmd) {
        return None;
    }

    let parts: Vec<&str> = shell_split(cmd);
    if parts.len() >= 2
        && parts[0] == "cargo"
        && matches!(parts[1], "build" | "check" | "clippy" | "install")
    {
        return Some(build_digest_runner_command("log", ".", None, cmd));
    }
    None
}

fn build_digest_runner_command(
    kind: &str,
    path: &str,
    runner: Option<&str>,
    shell_command: &str,
) -> String {
    let mut result = format!(
        "tsift --envelope __digest-runner --kind {} --path {} --shell-command {}",
        shell_quote(kind),
        shell_quote(path),
        shell_quote(shell_command)
    );
    if let Some(runner) = runner {
        result.push_str(&format!(" --runner {}", shell_quote(runner)));
    }
    result
}

fn has_shell_metacharacters(cmd: &str) -> bool {
    cmd.contains('|') || cmd.contains('>') || cmd.contains('<') || cmd.contains('&')
}

fn strip_shell_quotes(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn looks_like_path_selector(raw: &str) -> bool {
    raw.ends_with('/')
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw.contains('/')
        || raw.contains('.')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DigestRunnerKind {
    Test,
    Log,
}

impl DigestRunnerKind {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "test" => Ok(Self::Test),
            "log" => Ok(Self::Log),
            other => bail!("unsupported digest runner kind `{other}`; expected test or log"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Log => "log",
        }
    }
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
    cache_dir: &Path,
    query: &str,
    limit: usize,
    timeout_secs: u64,
    strategy: &str,
) -> Result<sift::SearchResponse> {
    let targets = resolve_search_index_targets(root, root, None, true)?;
    if targets.is_empty() {
        if config::Config::submodule_dirs(root)?.is_empty() {
            return run_search_with_timeout(
                root,
                cache_dir,
                query,
                limit,
                timeout_secs,
                strategy,
                &[],
            );
        }
        return Ok(empty_search_response(root, strategy));
    }

    let mut responses = Vec::with_capacity(targets.len());
    for target in &targets {
        let mut response = run_search_with_timeout(
            &target.source_root,
            cache_dir,
            query,
            limit,
            timeout_secs,
            strategy,
            std::slice::from_ref(target),
        )?;
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

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum RipgrepJsonEvent {
    Match {
        data: RipgrepMatchData,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct RipgrepMatchData {
    path: RipgrepTextField,
    lines: RipgrepTextField,
    line_number: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RipgrepTextField {
    text: Option<String>,
}

fn federated_exact_search(
    root: &Path,
    query: &str,
    limit: usize,
    timeout_secs: u64,
) -> Result<sift::SearchResponse> {
    let cfg = config::Config::load(root)?;
    let mut responses = Vec::new();
    for scope in config::Config::submodule_dirs(root)? {
        if !cfg.federation_for_scope(&scope) {
            continue;
        }
        let mut response =
            run_exact_search_with_timeout(&scope.source_root, query, limit, timeout_secs)?;
        absolutize_search_hit_paths(&mut response, &scope.source_root);
        response.root = root.display().to_string();
        responses.push(response);
    }

    Ok(merge_search_responses(root, "exact", limit, responses))
}

fn run_sift_search(
    search_path: &Path,
    cache_dir: &Path,
    query: &str,
    limit: usize,
    strategy: &str,
) -> Result<sift::SearchResponse> {
    let engine = Sift::builder().with_cache_dir(cache_dir).build();
    let options = SearchOptions::default()
        .with_limit(limit)
        .with_strategy(strategy.to_string());
    let input = SearchInput::new(search_path, query).with_options(options);
    engine.search(input).context("sift search failed")
}

fn exact_search_timeout_message(timeout_secs: u64) -> String {
    format!(
        "tsift search timed out after {}s (strategy: exact). \
         Re-run with `--timeout 0` to disable the timeout or narrow `--path` / `--scope`.",
        timeout_secs
    )
}

fn exact_search_command(search_path: &Path, query: &str) -> Command {
    let mut command = Command::new("rg");
    command
        .arg("--json")
        .arg("--fixed-strings")
        .arg("--line-number")
        .arg("--hidden")
        .arg("--")
        .arg(query)
        .arg(search_path);
    command
}

fn exact_search_file_timestamp(path: &Path) -> sift::ArtifactFreshness {
    let observed_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let modified_unix_secs = fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64);
    sift::ArtifactFreshness {
        observed_unix_secs,
        modified_unix_secs,
    }
}

fn parse_exact_search_output(
    search_path: &Path,
    limit: usize,
    raw: &str,
) -> Result<sift::SearchResponse> {
    if limit == 0 {
        return Ok(sift::SearchResponse {
            strategy: "exact".to_string(),
            root: search_path.display().to_string(),
            indexed_artifacts: 0,
            skipped_artifacts: 0,
            coverage: empty_search_coverage(),
            hits: Vec::new(),
        });
    }

    let mut hits = Vec::new();
    for line in raw.lines() {
        let event: RipgrepJsonEvent =
            serde_json::from_str(line).context("parsing ripgrep exact-search output")?;
        let RipgrepJsonEvent::Match { data } = event else {
            continue;
        };
        let Some(path_text) = data.path.text else {
            continue;
        };
        let Some(lines_text) = data.lines.text else {
            continue;
        };
        let path = PathBuf::from(path_text);
        let snippet = lines_text.trim_end_matches(['\r', '\n']).to_string();
        let rank = hits.len() + 1;
        hits.push(sift::SearchHit {
            artifact_id: format!(
                "exact:{}:{}:{}",
                path.display(),
                data.line_number.unwrap_or(0),
                rank
            ),
            artifact_kind: sift::ContextArtifactKind::File,
            path: path.display().to_string(),
            rank,
            score: (limit.saturating_sub(rank).saturating_add(1)) as f64,
            confidence: sift::ScoreConfidence::High,
            location: data.line_number.map(|line| format!("line {}", line)),
            snippet: snippet.clone(),
            provenance: sift::ArtifactProvenance {
                adapter: sift::AcquisitionAdapterKind::FileSystem,
                source: "ripgrep -F".to_string(),
                synthetic: false,
            },
            freshness: exact_search_file_timestamp(&path),
            budget: sift::ArtifactBudget::from_text(&snippet, 1),
        });
        if hits.len() >= limit {
            break;
        }
    }

    Ok(sift::SearchResponse {
        strategy: "exact".to_string(),
        root: search_path.display().to_string(),
        indexed_artifacts: hits.len(),
        skipped_artifacts: 0,
        coverage: empty_search_coverage(),
        hits,
    })
}

fn exact_search_response_from_process(
    search_path: &Path,
    limit: usize,
    status: std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<sift::SearchResponse> {
    if !status.success() && status.code() != Some(1) {
        let message = String::from_utf8_lossy(stderr);
        let trimmed = message.trim();
        if trimmed.is_empty() {
            bail!("ripgrep exact search exited with status {}", status);
        }
        bail!("{}", trimmed);
    }

    let raw = String::from_utf8(stdout.to_vec()).context("decoding ripgrep exact-search output")?;
    parse_exact_search_output(search_path, limit, &raw)
}

fn run_exact_search(search_path: &Path, query: &str, limit: usize) -> Result<sift::SearchResponse> {
    let output = exact_search_command(search_path, query)
        .output()
        .context("running exact search with ripgrep")?;
    exact_search_response_from_process(
        search_path,
        limit,
        output.status,
        &output.stdout,
        &output.stderr,
    )
}

fn run_exact_search_with_timeout(
    search_path: &Path,
    query: &str,
    limit: usize,
    timeout_secs: u64,
) -> Result<sift::SearchResponse> {
    if timeout_secs == 0 {
        return run_exact_search(search_path, query, limit);
    }

    let mut child = exact_search_command(search_path, query)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning timed exact search worker")?;

    let timeout = Duration::from_secs(timeout_secs);
    let status = wait_for_child_exit(&mut child, timeout)
        .context("waiting for timed exact search worker")?;
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        bail!("{}", exact_search_timeout_message(timeout_secs));
    }

    let status = status.unwrap();
    let stdout = read_child_stdout(&mut child)?;
    let stderr = read_child_stderr(&mut child)?;
    exact_search_response_from_process(
        search_path,
        limit,
        status,
        stdout.as_bytes(),
        stderr.as_bytes(),
    )
}

fn run_search_with_timeout(
    search_path: &Path,
    cache_dir: &Path,
    query: &str,
    limit: usize,
    timeout_secs: u64,
    strategy: &str,
    search_targets: &[SearchIndexTarget],
) -> Result<sift::SearchResponse> {
    if timeout_secs == 0 {
        return run_sift_search(search_path, cache_dir, query, limit, strategy);
    }

    let output_path = next_search_worker_output_path();
    let mut child = Command::new(
        std::env::current_exe().context("resolving tsift executable for timed search")?,
    )
    .arg("__search-worker")
    .arg("--path")
    .arg(search_path)
    .arg("--cache-dir")
    .arg(cache_dir)
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
            "{}",
            search_timeout_message(timeout_secs, strategy, search_targets)?
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

fn read_child_stdout(child: &mut std::process::Child) -> Result<String> {
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .context("reading search worker stdout")?;
    }
    Ok(stdout)
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
    static SEARCH_POST_PRECHECK_LOCK_HOOK: RefCell<Option<SearchPostPrecheckLockHook>> = const { RefCell::new(None) };
}

#[cfg(test)]
enum SearchPostPrecheckLockMode {
    RollbackJournal,
    Wal,
}

#[cfg(test)]
struct SearchPostPrecheckLockHook {
    db_path: PathBuf,
    mode: SearchPostPrecheckLockMode,
}

#[cfg(test)]
struct SearchPostPrecheckLockGuard;

#[cfg(test)]
impl Drop for SearchPostPrecheckLockGuard {
    fn drop(&mut self) {
        SEARCH_POST_PRECHECK_LOCK_HOOK.with(|hook| {
            hook.borrow_mut().take();
        });
    }
}

#[cfg(test)]
fn install_search_post_precheck_lock(db_path: PathBuf) -> SearchPostPrecheckLockGuard {
    install_search_post_precheck_lock_hook(db_path, SearchPostPrecheckLockMode::RollbackJournal)
}

#[cfg(test)]
fn install_search_post_precheck_wal_lock(db_path: PathBuf) -> SearchPostPrecheckLockGuard {
    install_search_post_precheck_lock_hook(db_path, SearchPostPrecheckLockMode::Wal)
}

#[cfg(test)]
fn install_search_post_precheck_lock_hook(
    db_path: PathBuf,
    mode: SearchPostPrecheckLockMode,
) -> SearchPostPrecheckLockGuard {
    SEARCH_POST_PRECHECK_LOCK_HOOK.with(|hook| {
        assert!(
            hook.borrow().is_none(),
            "search post-precheck lock hook already installed"
        );
        *hook.borrow_mut() = Some(SearchPostPrecheckLockHook { db_path, mode });
    });
    SearchPostPrecheckLockGuard
}

#[cfg(test)]
fn maybe_apply_search_post_precheck_test_hooks() -> Result<()> {
    let Some(hook) = SEARCH_POST_PRECHECK_LOCK_HOOK.with(|hook| hook.borrow_mut().take()) else {
        return Ok(());
    };
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let conn = Connection::open(&hook.db_path).expect("opening db for search lock hook");
        match hook.mode {
            SearchPostPrecheckLockMode::RollbackJournal => {
                conn.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
                    .expect("acquiring rollback-journal hook lock");
                fs::write(index::rollback_journal_path(&hook.db_path), "locked")
                    .expect("writing rollback journal marker");
            }
            SearchPostPrecheckLockMode::Wal => {
                conn.execute_batch(
                    "PRAGMA journal_mode=WAL;
                     PRAGMA wal_autocheckpoint=0;
                     CREATE TABLE IF NOT EXISTS search_wal_lock_probe (id INTEGER PRIMARY KEY);
                     INSERT INTO search_wal_lock_probe DEFAULT VALUES;
                     PRAGMA locking_mode=EXCLUSIVE;
                     BEGIN EXCLUSIVE;",
                )
                .expect("acquiring WAL hook lock");
                assert!(index::wal_sidecar_path(&hook.db_path).exists());
            }
        }
        ready_tx.send(()).expect("signaling search lock hook");
        std::thread::sleep(Duration::from_millis(200));
        drop(conn);
        let _ = fs::remove_file(index::rollback_journal_path(&hook.db_path));
    });
    ready_rx
        .recv_timeout(Duration::from_secs(1))
        .context("waiting for search post-precheck lock hook")?;
    Ok(())
}

#[cfg(not(test))]
fn maybe_apply_search_post_precheck_test_hooks() -> Result<()> {
    Ok(())
}
