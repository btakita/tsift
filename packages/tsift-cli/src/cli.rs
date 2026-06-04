use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::output::ResponseBudgetPreset;

#[derive(Parser)]
#[command(
    name = "tsift",
    version,
    about = "Token-efficient search for Claude Code"
)]
pub struct Cli {
    /// Reduce human-readable output volume across commands
    #[arg(long, global = true)]
    pub compact: bool,

    /// Use pretty-printed (indented) JSON instead of compact single-line JSON
    #[arg(long, global = true)]
    pub pretty: bool,

    /// Use terse JSON with abbreviated field names and inline schema (implies --json)
    #[arg(long, global = true)]
    pub terse: bool,

    /// Ultra-terse: strip properties from graph nodes/edges, truncate snippets to 80 chars, compact coverage snapshots (implies --terse)
    #[arg(long, global = true)]
    pub ultra_terse: bool,

    /// Show absolute paths instead of project-relative
    #[arg(long, global = true)]
    pub absolute: bool,

    /// Output repeated structures as TSV with header row
    #[arg(long, global = true)]
    pub tabular: bool,

    /// Schema-then-values: headers once, rows as arrays (implies --json)
    #[arg(long, global = true)]
    pub schema: bool,

    /// Wrap supported JSON responses in a common summary envelope (implies --json)
    #[arg(long, global = true)]
    pub envelope: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
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
        /// Restrict indexed symbol results to one or more languages (repeatable)
        #[arg(long = "lang")]
        lang: Vec<String>,
        /// Restrict indexed symbol results to one or more symbol kinds (repeatable)
        #[arg(long = "kind")]
        kind: Vec<String>,
        /// Restrict indexed symbol results to one or more tree-sitter node kinds (repeatable)
        #[arg(long = "node-kind")]
        node_kind: Vec<String>,
        /// Restrict Markdown/indexed AST results to an enclosing section path element or handle (repeatable)
        #[arg(long = "section")]
        section: Vec<String>,
        /// Restrict indexed AST results to symbols with a matching parent name or span handle (repeatable)
        #[arg(long = "parent")]
        parent: Vec<String>,
        /// Restrict indexed AST results to symbols with a matching direct child name or span handle (repeatable)
        #[arg(long = "child")]
        child: Vec<String>,
        /// Restrict Markdown code-block results by fenced-code language (repeatable)
        #[arg(long = "fence-language")]
        fence_language: Vec<String>,
        /// Restrict Markdown list-item results by list depth (repeatable)
        #[arg(long = "list-depth")]
        list_depth: Vec<usize>,
        /// Restrict Markdown heading results by heading level (repeatable)
        #[arg(long = "heading-level")]
        heading_level: Vec<usize>,
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
        /// Skip tagpath index lookup (do not annotate hits with `tagpath_handle`).
        #[arg(long)]
        no_tagpath: bool,
        /// Fail closed when a tagpath index is present but stale, instead of
        /// emitting `tagpath_index_stale: true` and falling back silently.
        #[arg(long)]
        tagpath_strict: bool,
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
    /// Validate semantic AST edit intents and optionally apply supported edits
    EditIntents {
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict symbol resolution to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Read intents from a file instead of stdin
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Apply supported, conflict-free semantic edit intents
        #[arg(long)]
        apply: bool,
        /// Verify supported intents in a temporary git worktree before mutating this tree
        #[arg(long)]
        verify: bool,
        /// Shell command to run in the temporary verification worktree after reindexing
        #[arg(long, requires = "verify")]
        verify_command: Option<String>,
        /// Preview-mode item cap for planned intents
        #[arg(long)]
        max_items: Option<usize>,
        /// Preview-mode per-field byte cap for messages
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Named preview budget preset (auto adapts from context-window env vars)
        #[arg(long, value_enum)]
        budget: Option<ResponseBudgetPreset>,
    },
    /// Recommend a Claude model tier for a task (haiku/search, sonnet/edit, opus/architecture)
    Route {
        /// Task description to classify
        task: String,
        /// Output only the model ID (for scripting)
        #[arg(long)]
        id: bool,
    },
    /// Manage first-party tsift memory state and migration imports
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
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
        /// Skip tagpath index lookup (do not annotate edges with `tagpath_handle`).
        #[arg(long)]
        no_tagpath: bool,
        /// Fail closed when a tagpath index is present but stale, instead of
        /// emitting a stale diagnostic and falling back silently.
        #[arg(long)]
        tagpath_strict: bool,
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
        /// Skip tagpath index lookup (do not annotate members with `tagpath_handle`).
        #[arg(long)]
        no_tagpath: bool,
        /// Fail closed when a tagpath index is present but stale, instead of
        /// emitting a stale diagnostic and falling back silently.
        #[arg(long)]
        tagpath_strict: bool,
    },
    /// Run graph algorithms over the indexed call graph
    Analyze {
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Entry point for dead-code reachability. Repeatable. Defaults to detected roots.
        #[arg(long = "entry")]
        entry_points: Vec<String>,
        /// Max rows shown in human output (0 = unlimited)
        #[arg(short, long, default_value = "20")]
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
        /// Skip tagpath index lookup (do not annotate path nodes with `tagpath_handle`).
        #[arg(long)]
        no_tagpath: bool,
        /// Fail closed when a tagpath index is present but stale, instead of
        /// emitting a stale diagnostic and falling back silently.
        #[arg(long)]
        tagpath_strict: bool,
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
        /// Skip tagpath index lookup (do not annotate hits with `tagpath_handle`).
        #[arg(long)]
        no_tagpath: bool,
        /// Fail closed when a tagpath index is present but stale, instead of
        /// emitting a stale diagnostic and falling back silently.
        #[arg(long)]
        tagpath_strict: bool,
    },
    /// Build a Graphify-style traversal graph for files, symbols, sessions, and backlog items
    Traverse {
        /// Node handle, symbol name, file path, or backlog id to explain
        node: Option<String>,
        /// Optional target node for shortest-path traversal
        #[arg(long)]
        to: Option<String>,
        /// Path to the indexed codebase or workspace (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict indexed code nodes to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Neighborhood depth around the selected node
        #[arg(long, default_value = "1")]
        depth: usize,
        /// Max neighborhood/recommendation/export items (0 = unlimited)
        #[arg(short, long, default_value = "50")]
        limit: usize,
        /// Output format for the graph traversal report
        #[arg(long, value_enum, default_value = "json")]
        format: TraverseFormat,
        /// Validate a Convex nodes/edges snapshot before trusting projected graph reads
        #[arg(long)]
        convex_snapshot: Option<PathBuf>,
    },
    /// Plan Convex nodes/edges sync batches for the local graph projection
    ConvexSync {
        /// Path to the indexed codebase or workspace (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict indexed code nodes to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Existing Convex rows snapshot to diff against
        #[arg(long)]
        snapshot: Option<PathBuf>,
        /// Max rows per planned mutation chunk. Default 50 keeps `upsertEdges` under the
        /// Convex isolate's 99 MiB carry-over limit on the demo schema; raise to 100+ only
        /// when targeting a schema that has already optimized its upsert mutations.
        #[arg(long, default_value = "50")]
        chunk_size: usize,
        /// Pull the current remote Convex rows through the configured transport before diffing
        #[arg(long, conflicts_with = "snapshot")]
        remote_snapshot: bool,
        /// Apply the planned chunks through the configured Convex transport
        #[arg(long)]
        apply: bool,
        /// Convex HTTP action endpoint; falls back to TSIFT_CONVEX_GRAPH_URL
        #[arg(long)]
        endpoint: Option<String>,
        /// Environment variable that holds the bearer token
        #[arg(long, default_value = "TSIFT_CONVEX_AUTH_TOKEN")]
        auth_token_env: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Query the provider-neutral graph database API over SQLite, tokensave, or a Convex snapshot
    GraphDb {
        /// Path to the indexed codebase or workspace (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict indexed code nodes to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Graph backend to query
        #[arg(long, value_enum, default_value = "sqlite")]
        backend: GraphDbBackend,
        /// Convex nodes/edges snapshot for --backend convex-snapshot
        #[arg(long)]
        convex_snapshot: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        query: GraphDbQuery,
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
    /// Project a Markdown file into stable AST node handles and expansion commands
    MarkdownAst {
        /// Markdown file to project (relative to --path/root unless absolute)
        file: PathBuf,
        /// Path to the codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict output to one stable node handle (`mdast-*`/`span-*`)
        #[arg(long)]
        node: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Preview-mode item cap for AST nodes
        #[arg(long)]
        max_items: Option<usize>,
        /// Preview-mode per-field byte cap for node names
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Named preview budget preset (auto adapts from context-window env vars)
        #[arg(long, value_enum)]
        budget: Option<ResponseBudgetPreset>,
    },
    /// Read a token-budgeted symbol packet with body, child symbols, and expansion handles
    SymbolRead {
        /// Symbol name or tag-style query to inspect
        symbol: String,
        /// Optional source file hint to disambiguate duplicate symbols
        #[arg(long)]
        file: Option<PathBuf>,
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict index refs to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Preview-mode item cap for child symbol/summary refs
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
    /// Reconcile the tsift symbol index against the tagpath `.naming/index.json` source
    /// set and report files covered by one but not the other.
    AuditTagpath {
        /// Path to the project root (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict the audit to a single workspace scope/submodule
        #[arg(long)]
        scope: Option<String>,
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
        /// Also install project-local OpenCode tsift command shortcuts
        #[arg(long)]
        opencode: bool,
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
    /// Find related cached semantic concepts/entities from the local graph store
    Semantic {
        /// Concept or entity text to compare against cached semantic graph rows
        query: String,
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict indexed code nodes to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Max related items to return (0 = unlimited)
        #[arg(short, long, default_value = "10")]
        limit: usize,
        /// Which semantic node family to search
        #[arg(long, value_enum, default_value = "concept")]
        kind: SemanticRelatedKind,
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
        /// Maximum changed files to parse with tree-sitter (0 = unlimited)
        #[arg(long, default_value = "25")]
        max_parsed_files: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Estimate affected tests from changed files, imports, and call edges
    Impact {
        /// Path to the codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Compare the staged index against HEAD instead of the working tree
        #[arg(long, conflicts_with = "revision")]
        cached: bool,
        /// Compare a single revision against its first parent instead of the working tree
        #[arg(long)]
        revision: Option<String>,
        /// Restrict graph evidence to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Maximum affected test targets to display (0 = unlimited)
        #[arg(short, long, default_value = "20")]
        limit: usize,
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
        /// Validate a Convex nodes/edges snapshot before trusting projected graph reads
        #[arg(long)]
        convex_snapshot: Option<PathBuf>,
    },
    /// Rank candidate worker scopes and flag parallel-dispatch merge risks
    ConflictMatrix {
        /// Candidate backlog ids, job handles, or graph node ids to compare
        targets: Vec<String>,
        /// Agent-doc session document or repo path to plan against
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict graph evidence to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Graph-db evidence depth for each target
        #[arg(long, default_value = "3")]
        depth: usize,
        /// Max graph-db evidence rows per target (0 = unlimited)
        #[arg(long, default_value = "8")]
        limit: usize,
        /// Maximum affected test targets to include from impact
        #[arg(long, default_value = "20")]
        impact_limit: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Export a graph-backed dispatch trace for operator review
    DispatchTrace {
        /// Candidate backlog ids, job handles, or graph node ids to trace
        targets: Vec<String>,
        /// Agent-doc session document or repo path to trace
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict graph evidence to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Graph-db evidence depth for each target
        #[arg(long, default_value = "3")]
        depth: usize,
        /// Max graph-db evidence rows per target (0 = unlimited)
        #[arg(long, default_value = "8")]
        limit: usize,
        /// Maximum affected test targets to include from impact
        #[arg(long, default_value = "20")]
        impact_limit: usize,
        /// Output format for the dispatch trace
        #[arg(long, value_enum, default_value = "json")]
        format: DispatchTraceFormat,
        /// Output as JSON (equivalent to --format json)
        #[arg(long)]
        json: bool,
    },
    /// Extract a graph-level dependency DAG for agent-doc backlog work
    DependencyDag {
        /// Backlog ids, job handles, or graph node ids to schedule (defaults to the session backlog)
        targets: Vec<String>,
        /// Agent-doc session document or repo path to schedule
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Restrict graph evidence to a specific submodule
        #[arg(long)]
        scope: Option<String>,
        /// Graph traversal depth for semantic and worker-result evidence
        #[arg(long, default_value = "4")]
        depth: usize,
        /// Max graph rows per evidence family (0 = unlimited)
        #[arg(long, default_value = "12")]
        limit: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
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
    /// Collect and evaluate cross-surface token gate samples
    TokenGate {
        #[command(subcommand)]
        command: TokenGateCommand,
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
        /// Skip automatic index fixes (auto-fix is now the default)
        #[arg(long)]
        no_fix: bool,
        /// [deprecated] Auto-fix is now the default; use --no-fix to skip
        #[arg(long, hide = true)]
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

#[derive(Subcommand)]
pub enum MemoryCommand {
    /// Report schema, hook, graph retrieval, and claude-mem retirement readiness
    Status {
        /// Project root whose .tsift/memory.db should be inspected
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Override the claude-mem SQLite DB path
        #[arg(long)]
        claude_mem_db: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Initialize the tsift memory database
    Init {
        /// Project root whose .tsift/memory.db should be initialized
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Plan or apply a read-only import from claude-mem SQLite into tsift memory
    ImportClaudeMem {
        /// Project root whose .tsift/memory.db should receive imported rows
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Override the claude-mem SQLite DB path
        #[arg(long)]
        db: Option<PathBuf>,
        /// Maximum rows to read from each supported claude-mem table; defaults to 1000 unless --all is set
        #[arg(long, conflicts_with = "all", value_name = "ROWS")]
        limit: Option<usize>,
        /// Read every supported claude-mem row instead of applying the default per-table cap
        #[arg(long)]
        all: bool,
        /// Apply the import; omitted means dry-run plan only
        #[arg(long)]
        apply: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Capture an agent-doc closeout event bundle into tsift memory
    CaptureAgentDocCloseout {
        /// Project root whose .tsift/memory.db should receive captured events
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Session document path that produced the closeout
        #[arg(long)]
        session_path: PathBuf,
        /// Prompt target or queue head that was answered
        #[arg(long)]
        prompt_target: String,
        /// Bounded response summary to store
        #[arg(long)]
        response_summary: String,
        /// Optional commit hash from the closeout
        #[arg(long)]
        commit_hash: Option<String>,
        /// Final session-check status for the closeout
        #[arg(long, default_value = "unknown")]
        session_check_status: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Estimate whether memory text can fit into a bounded model handoff
    HandoffPlan {
        /// Text to include in the memory handoff
        text: String,
        /// Maximum prompt token budget before reserve
        #[arg(long, default_value = "4096")]
        budget_tokens: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Fail closed before oversized memory/tool payloads are sent to a model
    BudgetGuard {
        /// Inline payload text to guard
        #[arg(long, conflicts_with = "file")]
        text: Option<String>,
        /// Payload file to guard
        #[arg(long, conflicts_with = "text")]
        file: Option<PathBuf>,
        /// Start byte when guarding a file chunk
        #[arg(long)]
        byte_start: Option<usize>,
        /// End byte when guarding a file chunk
        #[arg(long)]
        byte_end: Option<usize>,
        /// Stable source reference used in retry commands
        #[arg(long, default_value = "inline")]
        source_ref: String,
        /// Payload kind: tool_result, raw_log, transcript, or session
        #[arg(long, default_value = "tool_result")]
        payload_kind: String,
        /// Maximum prompt token budget before reserve
        #[arg(long, default_value = "4096")]
        budget_tokens: usize,
        /// Tokens reserved for system, instruction, and response overhead
        #[arg(long, default_value = "512")]
        reserve_tokens: usize,
        /// Maximum tokens allowed for any single memory event/chunk
        #[arg(long, default_value = "1536")]
        max_chunk_tokens: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Describe the stable query packet contract for memory retrieval
    QueryPlan {
        /// Query text
        query: String,
        /// Maximum memory packets a future query should return
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Maximum output tokens a future query should use
        #[arg(long, default_value = "2000")]
        max_tokens: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

impl MemoryCommand {
    pub fn json_output(&self) -> bool {
        match self {
            Self::Status { json, .. }
            | Self::Init { json, .. }
            | Self::ImportClaudeMem { json, .. }
            | Self::CaptureAgentDocCloseout { json, .. }
            | Self::HandoffPlan { json, .. }
            | Self::BudgetGuard { json, .. }
            | Self::QueryPlan { json, .. } => *json,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum TraverseFormat {
    Json,
    Html,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum DispatchTraceFormat {
    Json,
    Html,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SemanticRelatedKind {
    Concept,
    Entity,
    All,
}

#[derive(Subcommand)]
pub enum TokenGateCommand {
    /// Run one surface and emit a TokenGateSample entry as JSON
    Sample {
        /// Surface to sample: context_pack, session_review_next_context, graph_db_evidence,
        /// conflict_matrix, dispatch_trace
        #[arg(long)]
        surface: String,
        /// Path to the target document or repo root
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Scope for graph evidence (submodule)
        #[arg(long)]
        scope: Option<String>,
        /// Evidence target (required for graph_db_evidence, conflict_matrix, dispatch_trace)
        #[arg(long)]
        target: Option<String>,
        /// Evidence depth
        #[arg(long, default_value = "3")]
        depth: usize,
        /// Sample index for the id (defaults to 1)
        #[arg(long, default_value = "1")]
        sample_index: usize,
        /// Output as JSON (default true for sample)
        #[arg(long)]
        json: bool,
    },
    /// Evaluate the token gate against a history file
    Evaluate {
        /// Path to token-gate-history.json (defaults to fixtures/token-gate-history.json)
        #[arg(long)]
        history: Option<PathBuf>,
        /// Allowed regression percentage (0-100)
        #[arg(long, default_value = "20.0")]
        allowed_regression_percent: f64,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum GraphDbBackend {
    Sqlite,
    ConvexSnapshot,
    Tokensave,
}

#[derive(Subcommand, Debug, Clone)]
pub enum GraphDbQuery {
    /// Materialize or refresh the local SQLite graph.db projection for operator workflows
    Refresh,
    /// Report graph.db freshness, projection metadata, row counts, tombstone counts, and next commands without refreshing
    Status,
    /// Diagnose graph.db or Convex snapshot health without refreshing the local projection
    Doctor,
    /// Compare the local SQLite projection against a Convex snapshot before apply/read operations
    Drift,
    /// Reclaim SQLite graph.db storage after refresh/Convex reconciliation
    Compact {
        /// Execute WAL checkpoint/VACUUM instead of returning the dry-run policy
        #[arg(long)]
        apply: bool,
        /// Delete retained tombstone rows before VACUUM. Requires --confirmed-convex-reconciled.
        #[arg(long = "prune-tombstones")]
        prune_tombstones: bool,
        /// Confirm Convex consumers have already reconciled deletion tombstones.
        #[arg(long = "confirmed-convex-reconciled")]
        confirmed_convex_reconciled: bool,
    },
    /// Benchmark experimental read-only GraphStore candidates against SQLite before promotion
    BackendEval {
        /// Candidate backend prototype to evaluate. Repeatable; defaults to DuckDB/DuckPGQ, FalkorDB, Ladybug, Kuzu, and SurrealDB. Values: duckdb-duckpgq, falkordb, ladybug, kuzu, surrealdb.
        #[arg(long = "candidate")]
        candidates: Vec<String>,
        /// Backlog ids, job handles, or graph node ids to use for evidence/planning benchmarks
        #[arg(long = "target")]
        targets: Vec<String>,
        /// Include an opt-in full-project projection dataset in addition to the bounded path-hinted dataset
        #[arg(long = "full-projection")]
        full_projection: bool,
    },
    /// Build a bounded worker handoff evidence packet from a backlog id or job packet handle
    Evidence {
        /// Backlog id, job packet handle, or graph node id
        target: String,
        /// Maximum directed hops to follow when collecting context evidence
        #[arg(long, default_value = "3")]
        depth: usize,
        /// Maximum worker/source evidence records to return (0 = unlimited)
        #[arg(long, default_value = "8")]
        limit: usize,
    },
    /// Resolve a natural-language phrase to semantic seeds, then expand graph neighborhoods around them
    Related {
        /// Natural-language concept/entity phrase to retrieve context for
        query: String,
        /// Which semantic node family to seed from
        #[arg(long, value_enum, default_value = "all")]
        kind: SemanticRelatedKind,
        /// Incident/outgoing graph hops to expand around each semantic seed
        #[arg(long, default_value = "2")]
        depth: usize,
        /// Max semantic seed nodes to expand (0 = unlimited)
        #[arg(long = "seed-limit", default_value = "5")]
        seed_limit: usize,
        /// Max graph nodes to return after seed expansion (0 = unlimited)
        #[arg(short, long, default_value = "25")]
        limit: usize,
    },
    /// Show the stable JSON shape for graph database records and responses
    Schema,
    /// Look up one node by stable id
    Node {
        /// Stable graph node id
        id: String,
    },
    /// Look up one edge by stable edge id
    Edge {
        /// Stable graph edge id
        id: String,
    },
    /// Scan graph edges
    Edges {
        /// Restrict scanned edges to this kind
        #[arg(long)]
        edge_kind: Option<String>,
        /// Return records after this edge id cursor
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum edge records to return (0 = unlimited)
        #[arg(long)]
        limit: Option<usize>,
        /// Require an edge property match, formatted KEY=VALUE. Repeatable.
        #[arg(long = "property", value_name = "KEY=VALUE")]
        property_filters: Vec<String>,
    },
    /// Scan incoming and outgoing edges incident to a node
    Incident {
        /// Stable graph node id
        id: String,
        /// Restrict scanned edges to this kind
        #[arg(long)]
        edge_kind: Option<String>,
        /// Return records after this edge id cursor
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum edge records to return (0 = unlimited)
        #[arg(long)]
        limit: Option<usize>,
        /// Require an edge property match, formatted KEY=VALUE. Repeatable.
        #[arg(long = "property", value_name = "KEY=VALUE")]
        property_filters: Vec<String>,
    },
    /// Scan nodes by kind
    Kind {
        /// Node kind to scan
        kind: String,
        /// Return records after this node id cursor
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum node records to return (0 = unlimited)
        #[arg(long)]
        limit: Option<usize>,
        /// Require a node property match, formatted KEY=VALUE. Repeatable.
        #[arg(long = "property", value_name = "KEY=VALUE")]
        property_filters: Vec<String>,
    },
    /// Read an outgoing neighborhood from one node
    Neighborhood {
        /// Stable graph node id
        id: String,
        /// Outgoing traversal depth
        #[arg(long, default_value = "1")]
        depth: usize,
        /// Restrict traversed edges to this kind
        #[arg(long)]
        edge_kind: Option<String>,
        /// Return nodes after this node id cursor
        #[arg(long)]
        cursor: Option<String>,
        /// Maximum node records to return (0 = unlimited)
        #[arg(long)]
        limit: Option<usize>,
        /// Require a node property match, formatted KEY=VALUE. Repeatable.
        #[arg(long = "property", value_name = "KEY=VALUE")]
        property_filters: Vec<String>,
    },
    /// Find the shortest directed path between two nodes
    Path {
        /// Starting graph node id
        from: String,
        /// Target graph node id
        to: String,
        /// Restrict traversed edges to this kind
        #[arg(long)]
        edge_kind: Option<String>,
        /// Stop directed path search after this many hops
        #[arg(long)]
        max_hops: Option<usize>,
    },
}
