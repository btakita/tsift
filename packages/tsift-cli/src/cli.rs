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
        /// Path to search (repeatable; defaults to current directory). With
        /// multiple paths, exact search forwards them all to ripgrep and
        /// indexed/lexical hits are pruned to their union.
        #[arg(short, long)]
        path: Vec<PathBuf>,
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
        /// #015t Phase 4b — FTS index freshness verdict from the parent's
        /// precheck (skips the worker's redundant freshness re-inspect). Absent
        /// ⇒ the worker inspects on its own.
        #[arg(long)]
        fts_index_fresh: Option<bool>,
    },
    /// Run a shell command and emit a bounded, artifact-backed test/log digest envelope
    #[command(name = "digest-runner", alias = "__digest-runner")]
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
    /// Structural (ast-grep) code search and rewrite over AST patterns
    AstGrep {
        #[command(subcommand)]
        command: AstGrepCommand,
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
    /// Inspect local model support, GPU usage, and recommended KG model profiles
    LocalModel {
        #[command(subcommand)]
        command: LocalModelCommand,
    },
    /// Extract a local Knowledge Graph from source text via a local model (#lmlazy)
    Kg {
        #[command(subcommand)]
        command: KgCommand,
    },
    /// Capture and query authored findings anchored to code (Findings Graph Layer)
    Finding {
        #[command(subcommand)]
        command: FindingCommand,
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
        /// Resolve the symbol across every federated submodule index and report
        /// the owning scope. Automatic at a workspace root with no shared root
        /// index.
        #[arg(long)]
        federated: bool,
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
        /// Resolve across every federated submodule index. Automatic at a
        /// workspace root with no shared root index.
        #[arg(long)]
        federated: bool,
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
        /// Resolve across every federated submodule index. Automatic at a
        /// workspace root with no shared root index.
        #[arg(long)]
        federated: bool,
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
        /// Resolve the symbol across every federated submodule index and report
        /// the owning scope. Automatic at a workspace root with no shared root
        /// index.
        #[arg(long)]
        federated: bool,
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
    /// Read a source file as an AST-symbol projection by default, or as a bounded line window
    SourceRead {
        /// Source file to preview (relative to --path/root unless absolute)
        file: PathBuf,
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Projection style: ast emits indexed symbols/spans; window emits numbered source lines
        #[arg(long, value_enum, default_value = "ast")]
        style: SourceReadStyle,
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
        /// Search all workspace scopes (automatic at a workspace root)
        #[arg(long)]
        federated: bool,
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
        /// Retry files with a cached terminal extraction failure
        #[arg(long, requires = "extract")]
        force: bool,
        /// Maximum estimated source tokens per extracted file
        #[arg(long, requires = "extract")]
        max_file_tokens: Option<usize>,
        /// Show cache statistics
        #[arg(long)]
        stats: bool,
        /// Path to the indexed codebase (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Pin or downgrade the local model for this extraction only (#gctrl2).
        /// Pass a profile id, "hash" to force the CPU/hash fallback, or omit
        /// for auto-rank. Informational until a real provider is wired in.
        #[arg(long)]
        profile: Option<String>,
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
        /// Pin or downgrade the local model for this call only (#gctrl2).
        /// Pass a profile id, "hash" to force the CPU/hash fallback, or omit
        /// for auto-rank. Informational until a real provider is wired in.
        #[arg(long)]
        profile: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Summarize git-changed files into a bounded, code-aware digest
    DiffDigest {
        /// Path to the codebase (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict the digest to a git pathspec (repeatable, relative to the codebase root)
        #[arg(long = "pathspec")]
        pathspecs: Vec<String>,
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
        /// Run a token-savings + false-negative fixture gate instead of digesting one log
        #[arg(long)]
        fixture: Option<PathBuf>,
        /// Exit non-zero when any fixture case misses its savings or signal thresholds
        #[arg(long)]
        fail_under: bool,
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
        /// Run a prompt-cache effectiveness fixture instead of one transcript/log
        #[arg(long)]
        fixture: Option<PathBuf>,
        /// Exit non-zero when a prompt-cache fixture case misses its thresholds
        #[arg(long)]
        fail_under: bool,
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
        /// Also refresh tracked Code Navigation instruction files (same writes as `tsift init`)
        #[arg(long)]
        fix_instructions: bool,
        /// [deprecated] Index auto-fix is the default; this now only adds --fix-instructions
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
pub enum AstGrepCommand {
    /// Find code matching a structural pattern (e.g. `foo($A)`, `if $C { $$$B }`)
    Search {
        /// ast-grep pattern to match
        pattern: String,
        /// Files or directories to scan (defaults to the current directory)
        #[arg(long = "path")]
        paths: Vec<PathBuf>,
        /// Force a language instead of inferring it per file extension
        #[arg(long)]
        lang: Option<String>,
        /// Include files excluded by .gitignore and hidden files
        #[arg(long)]
        no_ignore: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Preview-mode item cap
        #[arg(long)]
        max_items: Option<usize>,
        /// Preview-mode per-field byte cap
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Named preview budget preset
        #[arg(long, value_enum)]
        budget: Option<ResponseBudgetPreset>,
    },
    /// Rewrite code matching a structural pattern; previews unless --apply
    Rewrite {
        /// ast-grep pattern to match
        pattern: String,
        /// Replacement template, reusing the pattern's metavariables
        rewrite: String,
        /// Files or directories to scan (defaults to the current directory)
        #[arg(long = "path")]
        paths: Vec<PathBuf>,
        /// Force a language instead of inferring it per file extension
        #[arg(long)]
        lang: Option<String>,
        /// Include files excluded by .gitignore and hidden files
        #[arg(long)]
        no_ignore: bool,
        /// Write the rewrite to disk (default is a preview)
        #[arg(long)]
        apply: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
        /// Preview-mode item cap
        #[arg(long)]
        max_items: Option<usize>,
        /// Preview-mode per-field byte cap
        #[arg(long)]
        max_bytes: Option<usize>,
        /// Named preview budget preset
        #[arg(long, value_enum)]
        budget: Option<ResponseBudgetPreset>,
    },
    /// List the structural languages compiled into this build
    Languages {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum FindingCommand {
    /// Author a finding/decision/note anchored to a symbol or file
    Add {
        /// Project root (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Node kind: finding | decision | note
        #[arg(long, default_value = "finding")]
        kind: String,
        /// Short title
        #[arg(long)]
        title: String,
        /// Finding body / the "why"
        #[arg(long)]
        body: String,
        /// Symbol name or file path the finding concerns (anchor target)
        #[arg(long)]
        about: String,
        /// Optional confidence in [0.0, 1.0]
        #[arg(long)]
        confidence: Option<f64>,
        /// Trust status: draft | trusted (explicit adds default to trusted)
        #[arg(long, default_value = "trusted")]
        status: String,
        /// Optional id of an existing finding this one relates_to
        #[arg(long)]
        relates: Option<String>,
        /// Restrict anchor resolution to a specific submodule index
        #[arg(long)]
        scope: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// List captured findings, flagging those whose anchor has moved (stale)
    List {
        /// Project root (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Filter by the anchored symbol/file
        #[arg(long)]
        about: Option<String>,
        /// Filter by node kind: finding | decision | note
        #[arg(long)]
        kind: Option<String>,
        /// Filter by trust status: draft | trusted
        #[arg(long)]
        status: Option<String>,
        /// Include findings whose anchor moved (stale); hidden by default
        #[arg(long)]
        include_stale: bool,
        /// Restrict anchor re-resolution to a specific submodule index
        #[arg(long)]
        scope: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Passively harvest `draft` candidate findings from agent-doc session archives (config-gated, #trt1p4)
    Harvest {
        /// Project root (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict anchor resolution to a specific submodule index
        #[arg(long)]
        scope: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Promote a `draft` finding to `trusted` so it becomes eligible for hot-path injection (#trt1p4)
    Promote {
        /// Finding id to promote
        id: String,
        /// Project root (defaults to current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum KgCommand {
    /// Extract entities/relations from source text via an Ollama-served model
    Extract {
        /// Profile id (e.g. `qwen3-32b-q4-ollama`). Use `--model` to bypass
        /// profile resolution entirely.
        #[arg(long)]
        profile: Option<String>,
        /// Explicit Ollama model tag; overrides the profile's `model_ref`.
        #[arg(long)]
        model: Option<String>,
        /// Ollama host URL (defaults to `http://127.0.0.1:11434`, honors `OLLAMA_HOST`).
        #[arg(long)]
        host: Option<String>,
        /// Read source text from this file instead of stdin.
        #[arg(long)]
        input: Option<PathBuf>,
        /// Source reference label used for KG provenance.
        #[arg(long)]
        source_ref: Option<String>,
        /// Upsert the resulting projection into this graph.db path.
        #[arg(long)]
        graph_db: Option<PathBuf>,
        /// Skip cooperative GPU lease coordination (#kgleasewire). By default an
        /// extract acquires an exclusive lease for the profile so concurrent
        /// extracts serialize on the GPU.
        #[arg(long)]
        no_lease: bool,
        /// Idle TTL (seconds) recorded on the acquired lease; 0 means no
        /// TTL-based staleness (pid-liveness still reclaims crashed holders).
        #[arg(long, default_value_t = 0)]
        idle_ttl_seconds: u64,
        /// Keep the model resident after extraction instead of unloading it when
        /// this extract released the last reference (reference-counted unload).
        #[arg(long)]
        keep_loaded: bool,
        /// Override the cooperative lease registry file path.
        #[arg(long)]
        lease_file: Option<PathBuf>,
        /// Skip graph-aware context injection (#kgctxinject). By default, when
        /// `--graph-db` already holds entities, a bounded known-entity pack is
        /// injected into the extractor prompt so the model reconciles against
        /// existing canonical stable ids instead of re-inventing them.
        #[arg(long)]
        no_context: bool,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long, short)]
        json: bool,
    },
    /// Report KG state in a `.tsift/graph.db` (spec local-kg-model.md line 31-32).
    Status {
        /// Graph db path (defaults to `<cwd>/.tsift/graph.db`).
        #[arg(long)]
        graph_db: Option<PathBuf>,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long, short)]
        json: bool,
    },
    /// Report which extracted sources are stale (changed since extraction) so
    /// `.tsift/graph.db` can be refreshed on demand (#kgextractrefresh), or —
    /// with `--apply` — automatically re-extract them (#kgrefreshapply).
    Refresh {
        /// Graph db path (defaults to `<cwd>/.tsift/graph.db`).
        #[arg(long)]
        graph_db: Option<PathBuf>,
        /// Re-extract every stale / no_recorded_hash source whose file is still
        /// readable, reusing the lease-aware `kg extract` path (#kgrefreshapply).
        /// Operator-gated: needs GPU + Ollama. Without this flag `refresh` is the
        /// read-only staleness plan from #kgextractrefresh.
        #[arg(long)]
        apply: bool,
        /// Profile id for `--apply` re-extraction (default qwen3-32b-q4-ollama).
        #[arg(long)]
        profile: Option<String>,
        /// Explicit Ollama model tag for `--apply`; bypasses profile resolution.
        #[arg(long)]
        model: Option<String>,
        /// Ollama host URL for `--apply` (defaults to http://127.0.0.1:11434).
        #[arg(long)]
        host: Option<String>,
        /// Skip cooperative GPU lease coordination for `--apply` re-extraction.
        #[arg(long)]
        no_lease: bool,
        /// Idle TTL (seconds) recorded on `--apply` acquired leases; 0 means no
        /// TTL-based staleness (pid-liveness still reclaims crashed holders).
        #[arg(long, default_value_t = 0)]
        idle_ttl_seconds: u64,
        /// Keep the model resident after `--apply` instead of unloading it when
        /// an extract released the last reference.
        #[arg(long)]
        keep_loaded: bool,
        /// Override the cooperative lease registry file path for `--apply`.
        #[arg(long)]
        lease_file: Option<PathBuf>,
        /// Skip graph-aware context injection during `--apply` re-extraction
        /// (#kgctxincremental). By default re-extraction reconciles against the
        /// existing graph's stable ids instead of duplicating them.
        #[arg(long)]
        no_context: bool,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long, short)]
        json: bool,
    },
    /// Look up Knowledge Graph evidence for a symbol/kind in `.tsift/graph.db`
    /// (#kgadactivate — agent-doc's read seam per spec line 29-30).
    Evidence {
        /// Substring matched case-insensitively against node label, id, and kind.
        #[arg(long)]
        symbol: Option<String>,
        /// Restrict matches to a single node kind (e.g. `kg_source`, `concept`).
        #[arg(long)]
        kind: Option<String>,
        /// Maximum number of matched nodes to return (default 20).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Graph db path (defaults to `<cwd>/.tsift/graph.db`).
        #[arg(long)]
        graph_db: Option<PathBuf>,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long, short)]
        json: bool,
    },
    /// Unload the active KG extractor model from the provider (#kgunloadpost).
    Unload {
        /// Profile id (e.g. `qwen3-32b-q4-ollama`).
        #[arg(long)]
        profile: Option<String>,
        /// Explicit Ollama model tag; overrides the profile's `model_ref`.
        #[arg(long)]
        model: Option<String>,
        /// Ollama host URL (defaults to `http://127.0.0.1:11434`, honors `OLLAMA_HOST`).
        #[arg(long)]
        host: Option<String>,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long, short)]
        json: bool,
    },
    /// Run a small end-to-end extraction against a live Ollama server (smoke test).
    Smoke {
        /// Profile id (e.g. `qwen3-32b-q4-ollama`).
        #[arg(long)]
        profile: Option<String>,
        /// Explicit Ollama model tag; overrides the profile's `model_ref`.
        #[arg(long)]
        model: Option<String>,
        /// Ollama host URL (defaults to `http://127.0.0.1:11434`, honors `OLLAMA_HOST`).
        #[arg(long)]
        host: Option<String>,
        /// Unload the model after the smoke run (default: leave resident).
        #[arg(long)]
        unload: bool,
        /// Emit machine-readable JSON instead of a human summary.
        #[arg(long, short)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum LocalModelCommand {
    /// Report GPU probe, RTX 5090 model ranking, and recommended local KG profiles
    Status {
        /// Skip nvidia-smi probing and report only static profile ranking
        #[arg(long)]
        no_probe: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Plan provider unload actions and evaluate before/after VRAM cleanup
    Unload {
        /// Model profile id to unload or validate
        #[arg(long, default_value = "qwen3-32b-q4")]
        profile: String,
        /// Provider API endpoint for unload/sleep hooks
        #[arg(long)]
        provider_endpoint: Option<String>,
        /// Isolated provider worker pid for process-exit fallback
        #[arg(long)]
        provider_pid: Option<u32>,
        /// Idle TTL to record in the lease report
        #[arg(long, default_value_t = 0)]
        idle_ttl_seconds: u64,
        /// Skip live nvidia-smi probes and emit a plan unless synthetic MiB values are supplied
        #[arg(long)]
        no_probe: bool,
        /// Synthetic pre-load used VRAM MiB for deterministic cleanup checks
        #[arg(long)]
        pre_used_mib: Option<u64>,
        /// Synthetic post-unload used VRAM MiB for deterministic cleanup checks
        #[arg(long)]
        post_used_mib: Option<u64>,
        /// Allowed post-unload VRAM delta above the pre-load baseline
        #[arg(long, default_value_t = 768)]
        tolerance_mib: u64,
        /// Exit non-zero when cleanup is not proven
        #[arg(long)]
        strict: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage the cooperative GPU lease registry (#gctrl1)
    Lease {
        #[command(subcommand)]
        command: LeaseCommand,
    },
    /// Resolve a `--profile` preference against the live GPU probe (#gctrl2)
    Resolve {
        /// Pin to a profile id, "hash" to force the CPU/hash fallback, or omit for auto-rank
        #[arg(long)]
        profile: Option<String>,
        /// Role the resolved profile will be used for
        #[arg(long, value_enum, default_value = "extract")]
        role: ResolveRole,
        /// Skip nvidia-smi probing
        #[arg(long)]
        no_probe: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Swap from one local model profile to another in a single action (#gctrl3)
    Swap {
        /// Profile id to unload
        #[arg(long)]
        from: String,
        /// Profile id to load after the source unload is proven
        #[arg(long)]
        to: String,
        /// Provider API endpoint for unload/sleep hooks
        #[arg(long)]
        provider_endpoint: Option<String>,
        /// Isolated provider worker pid for process-exit fallback
        #[arg(long)]
        provider_pid: Option<u32>,
        /// Idle TTL to record in the lease report
        #[arg(long, default_value_t = 0)]
        idle_ttl_seconds: u64,
        /// Skip live nvidia-smi probes and emit a plan unless synthetic MiB values are supplied
        #[arg(long)]
        no_probe: bool,
        /// Synthetic baseline used VRAM MiB (before unload)
        #[arg(long)]
        pre_used_mib: Option<u64>,
        /// Synthetic post-unload used VRAM MiB
        #[arg(long)]
        post_used_mib: Option<u64>,
        /// Allowed post-unload VRAM delta above the pre-load baseline
        #[arg(long, default_value_t = 768)]
        tolerance_mib: u64,
        /// Exit non-zero when cleanup is not proven or the target is unselectable
        #[arg(long)]
        strict: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq, Debug)]
pub enum ResolveRole {
    Extract,
    Embed,
    Rerank,
}

impl ResolveRole {
    pub fn to_model_role(self) -> tsift_local_model::ModelRole {
        match self {
            ResolveRole::Extract => tsift_local_model::ModelRole::Extract,
            ResolveRole::Embed => tsift_local_model::ModelRole::Embed,
            ResolveRole::Rerank => tsift_local_model::ModelRole::Rerank,
        }
    }
}

#[derive(Subcommand)]
pub enum LeaseCommand {
    /// Acquire a cooperative GPU lease for a profile (fails on conflict)
    Acquire {
        /// Model profile id to acquire a lease for
        #[arg(long)]
        profile: String,
        /// Holder pid (defaults to the current process pid)
        #[arg(long)]
        holder_pid: Option<u32>,
        /// Holder command label recorded in the registry
        #[arg(long, default_value = "tsift")]
        holder_command: String,
        /// Idle TTL in seconds; 0 means no TTL-based staleness
        #[arg(long, default_value_t = 0)]
        idle_ttl_seconds: u64,
        /// Synthetic VRAM baseline in MiB; omit to probe nvidia-smi
        #[arg(long)]
        vram_baseline_mib: Option<u64>,
        /// Skip nvidia-smi probing for the baseline
        #[arg(long)]
        no_probe: bool,
        /// Override the lease registry file path
        #[arg(long)]
        lease_file: Option<PathBuf>,
        /// Exit non-zero when the acquire conflicts with a live holder
        #[arg(long)]
        strict: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Release a previously acquired GPU lease
    Release {
        /// Model profile id to release
        #[arg(long)]
        profile: String,
        /// Holder pid (defaults to the current process pid)
        #[arg(long)]
        holder_pid: Option<u32>,
        /// Override the lease registry file path
        #[arg(long)]
        lease_file: Option<PathBuf>,
        /// Unload the profile's model (Ollama keep_alive:0) when this release
        /// drops the live holder count to zero — reference-counted unload.
        #[arg(long)]
        unload_on_last_release: bool,
        /// Provider host/endpoint for the unload POST (defaults to the resolved
        /// Ollama endpoint)
        #[arg(long)]
        host: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Heartbeat: slide a held lease's TTL window forward so a long-lived
    /// session is not reclaimed as stale
    Renew {
        /// Model profile id to renew
        #[arg(long)]
        profile: String,
        /// Holder pid (defaults to the current process pid)
        #[arg(long)]
        holder_pid: Option<u32>,
        /// Override the lease registry file path
        #[arg(long)]
        lease_file: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Reap stale leases (dead pids or expired TTL) and report which profiles
    /// dropped to zero references
    Reap {
        /// Override the lease registry file path
        #[arg(long)]
        lease_file: Option<PathBuf>,
        /// Unload the model (Ollama keep_alive:0) for each profile whose last
        /// reference was reclaimed this reap
        #[arg(long)]
        unload_empty: bool,
        /// Provider host/endpoint for the unload POST (defaults to the resolved
        /// Ollama endpoint)
        #[arg(long)]
        host: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show the current GPU lease registry (after pruning stale entries)
    Show {
        /// Override the lease registry file path
        #[arg(long)]
        lease_file: Option<PathBuf>,
        /// Show stale entries instead of pruning them
        #[arg(long)]
        include_stale: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

impl LocalModelCommand {
    pub fn json_output(&self) -> bool {
        match self {
            Self::Status { json, .. } => *json,
            Self::Unload { json, .. } => *json,
            Self::Lease { command } => command.json_output(),
            Self::Resolve { json, .. } => *json,
            Self::Swap { json, .. } => *json,
        }
    }
}

impl LeaseCommand {
    pub fn json_output(&self) -> bool {
        match self {
            Self::Acquire { json, .. }
            | Self::Release { json, .. }
            | Self::Renew { json, .. }
            | Self::Reap { json, .. }
            | Self::Show { json, .. } => *json,
        }
    }
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
    /// Project stored memory events into the shared code graph store so memory
    /// nodes are queryable alongside code symbols (#memgraphrag2)
    ProjectGraph {
        /// Project root whose .tsift/memory.db is projected into .tsift/graph.db
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Override the destination graph DB path (defaults to .tsift/graph.db)
        #[arg(long)]
        graph_db: Option<PathBuf>,
        /// Maximum memory events to project
        #[arg(long, default_value = "5000")]
        limit: usize,
        /// Memory read policy for the bounded projection slice
        #[arg(long, value_enum, default_value_t = MemoryProjectReadPolicy::RecentFirst)]
        read_policy: MemoryProjectReadPolicy,
        /// Query text required when --read-policy=query-relevant
        #[arg(long)]
        query: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Derive and materialize the Semantic Ontology Graph layer (node/edge KIND
    /// type-nodes + permitted relations) from the shared graph store (#memgraphrag-ont)
    OntologyGraph {
        /// Project root whose .tsift/graph.db is introspected and updated
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Override the graph DB path (defaults to .tsift/graph.db)
        #[arg(long)]
        graph_db: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// List/query authored finding/decision/note nodes from the shared graph, newest first (#trt1 retrieval)
    Findings {
        /// Project root whose .tsift/graph.db is queried
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Filter by kind: finding | decision | note | all
        #[arg(long, default_value = "all")]
        kind: String,
        /// Only findings anchored to this symbol handle
        #[arg(long)]
        anchor: Option<String>,
        /// Lexical filter on finding text (case-insensitive substring/term overlap)
        #[arg(long)]
        query: Option<String>,
        /// Maximum findings to return
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Override the graph DB path (defaults to .tsift/graph.db)
        #[arg(long)]
        graph_db: Option<PathBuf>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Add an authored finding/decision/note node anchored to a symbol handle (#trt1)
    FindingAdd {
        /// Project root whose .tsift/graph.db receives the authored node
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Authored node kind: finding | decision | note
        #[arg(long, default_value = "finding")]
        kind: String,
        /// Finding text
        #[arg(long)]
        text: String,
        /// Stable symbol handle / graph node id to anchor to (NOT a line number)
        #[arg(long)]
        anchor: String,
        /// Confidence in 0..=1
        #[arg(long, default_value = "1.0")]
        confidence: f64,
        /// Optional session id to tag the authored node
        #[arg(long)]
        session_id: Option<String>,
        /// Override the graph DB path (defaults to .tsift/graph.db)
        #[arg(long)]
        graph_db: Option<PathBuf>,
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
            | Self::QueryPlan { json, .. }
            | Self::ProjectGraph { json, .. }
            | Self::OntologyGraph { json, .. }
            | Self::Findings { json, .. }
            | Self::FindingAdd { json, .. } => *json,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum MemoryProjectReadPolicy {
    RecentFirst,
    OldestFirst,
    QueryRelevant,
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
pub enum SourceReadStyle {
    Ast,
    Window,
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

/// On-demand projection format for `graph-db map` (#trt1p3). The graph store is
/// the source of truth; `md`/`html` are rendered views of the same overview +
/// attached findings. `md` is greppable / commit-friendly; `html` is an
/// interactive human view.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum MapFormat {
    Md,
    Html,
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
    /// Export the current local SQLite graph.db as a compressed shareable snapshot artifact
    SnapshotExport {
        /// Output artifact path. The artifact is a gzip-compressed SQLite graph.db.
        output: PathBuf,
        /// Overwrite an existing artifact path.
        #[arg(long)]
        force: bool,
    },
    /// Import a compressed SQLite graph.db snapshot after freshness and doctor validation
    SnapshotImport {
        /// Snapshot artifact path created by graph-db snapshot-export.
        artifact: PathBuf,
        /// Replace an existing local .tsift/graph.db after validation.
        #[arg(long)]
        replace: bool,
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
        /// Return evidence after this cursor (node id from a previous page)
        #[arg(long)]
        cursor: Option<String>,
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
    /// Produce a two-tier graph map: overview (communities, top hubs, edge-kind histogram, module tree) plus optional focus tier
    Map {
        /// Symbol name for the optional focus tier (reuses explain envelope for a single deep-dive)
        #[arg(long)]
        focus: Option<String>,
        /// Max top-degree hubs to include in the overview (0 = unlimited)
        #[arg(long, default_value = "10")]
        top_hubs: usize,
        /// Max communities to include in the overview (0 = unlimited)
        #[arg(long, default_value = "20")]
        community_limit: usize,
        /// Maximum directed hops for focus-tier neighborhood expansion
        #[arg(long, default_value = "2")]
        focus_depth: usize,
        /// Render an on-demand projection (md|html) of the map + attached findings instead of JSON/text
        #[arg(long, value_enum)]
        format: Option<MapFormat>,
    },
}
