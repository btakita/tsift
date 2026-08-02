use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::{Builder as TempFileBuilder, NamedTempFile, TempDir};
use tree_sitter::StreamingIterator as _;
use tsift_astgrep::AstGrepLang;
use tsift_graph as graph;
use tsift_index::index;
use tsift_quality::lint;
use tsift_search::impact;

use crate::output::{OutputFormat, ResponseBudget, ToolEnvelopeSummary};
use crate::{
    SourceRangePreview, envelope_metric, markdown_ast_projection, print_json_or_envelope,
    relativize_pathbuf, resolve_query_db_path, resolve_source_file, shell_quote,
    source_read_command, source_symbol_line, source_symbol_read_command, stable_handle,
    stored_symbol_ast_span, symbol_hit_ast_span, symbol_hit_end_line, symbol_hit_line,
    truncate_for_budget,
};

#[derive(Deserialize)]
pub(crate) struct EditBatch {
    pub(crate) edits: Vec<EditOp>,
}

#[derive(Deserialize)]
pub(crate) struct SemanticEditIntentBatch {
    pub(crate) intents: Vec<SemanticEditIntent>,
}

#[derive(Deserialize)]
pub(crate) struct SemanticEditIntent {
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) target_handle: Option<String>,
    #[serde(default)]
    pub(crate) symbol: Option<String>,
    #[serde(default)]
    pub(crate) file: Option<PathBuf>,
    #[serde(default)]
    pub(crate) destination_symbol: Option<String>,
    #[serde(default)]
    pub(crate) position: Option<String>,
    #[serde(default)]
    pub(crate) replacement: Option<String>,
    /// ast-grep structural pattern, used by `structural_rewrite`.
    #[serde(default)]
    pub(crate) pattern: Option<String>,
    #[serde(default)]
    pub(crate) call_replacement: Option<String>,
    #[serde(default)]
    pub(crate) new_name: Option<String>,
    /// One-based inclusive line range, used by `extract_function`.
    ///
    /// `target_handle` cannot express this: a run of sibling statements is not
    /// one AST node, so there is no span to hand back. This is the only intent
    /// field that selects by position rather than by identity, and it is
    /// rejected for every other kind so a stray range cannot silently widen an
    /// edit that resolved its target by name.
    #[serde(default)]
    pub(crate) start_line: Option<usize>,
    #[serde(default)]
    pub(crate) end_line: Option<usize>,
    #[serde(default)]
    pub(crate) expected_content_hash: Option<String>,
}

#[derive(Serialize, Clone)]
pub(crate) struct AstSpanPreview {
    pub(crate) handle: String,
    pub(crate) node_kind: String,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_start_byte: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_end_byte: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_handle: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) child_handles: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) markdown: Option<MarkdownSpanMetadata>,
}

#[derive(Serialize, Clone)]
pub(crate) struct MarkdownSpanMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) heading_level: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) section_path: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) section_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) list_depth: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fence_language: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) embedded_symbols: Vec<MarkdownEmbeddedSymbol>,
}

#[derive(Serialize, Clone)]
pub(crate) struct MarkdownEmbeddedSymbol {
    pub(crate) handle: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) language: String,
    pub(crate) node_kind: String,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_start_byte: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_end_byte: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_end_line: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct SemanticEditIntentPlan {
    pub(crate) handle: String,
    pub(crate) kind: String,
    pub(crate) status: String,
    pub(crate) apply_supported: bool,
    pub(crate) applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_selection: Option<SemanticEditTargetSelection>,
    pub(crate) target_symbol: Option<SemanticEditSymbolTarget>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) call_refs: Vec<SemanticEditCallRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cross_file_call_ref_total: Option<usize>,
    pub(crate) target_file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) destination_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_range: Option<SourceRangePreview>,
    /// The lines this edit actually rewrote. `target_range` is the *declaration
    /// span of the resolved symbol* — for a rename it is one line, while the
    /// edit reaches every call site in the file. Reporting only the former
    /// invites reading it as the extent of the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) edited_range: Option<SourceRangePreview>,
    /// Other files this rename rewrites, so the report names the full extent
    /// rather than only the declaring file.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) rename_caller_files: Vec<String>,
    pub(crate) content_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) patch_proposal: Option<SemanticEditPatchProposal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) formatter: Option<String>,
    pub(crate) message: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct SemanticEditCallRef {
    pub(crate) file: String,
    pub(crate) caller: String,
    pub(crate) line: usize,
}

pub(crate) struct SemanticEditCallRefContext<'a> {
    pub(crate) refs: &'a [SemanticEditCallRef],
    pub(crate) cross_file_total: usize,
}

#[derive(Serialize)]
pub(crate) struct SemanticEditSymbolTarget {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) language: String,
    pub(crate) file: String,
    pub(crate) line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) span: Option<AstSpanPreview>,
}

#[derive(Serialize, Clone)]
pub(crate) struct SemanticEditTargetSelection {
    pub(crate) requested_handle: String,
    pub(crate) matched_handle: String,
    pub(crate) handle_family: String,
    pub(crate) source: String,
    pub(crate) file: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) language: String,
    pub(crate) line: usize,
    pub(crate) end_line: usize,
    pub(crate) span: AstSpanPreview,
    pub(crate) source_window: String,
    pub(crate) symbol_read: String,
    pub(crate) message: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct SemanticEditPatchProposal {
    pub(crate) schema_version: u8,
    pub(crate) strategy: String,
    pub(crate) status: String,
    pub(crate) parser_state: SemanticEditPatchParserState,
    pub(crate) trivia: SemanticEditPatchTriviaPolicy,
    pub(crate) files: Vec<SemanticEditPatchFileProposal>,
    pub(crate) message: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct SemanticEditPatchParserState {
    pub(crate) input: String,
    pub(crate) output: String,
    pub(crate) validator: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct SemanticEditPatchTriviaPolicy {
    pub(crate) mode: String,
    pub(crate) preserves_comments: bool,
    pub(crate) preserves_formatting: bool,
    pub(crate) preserves_trivia: bool,
    pub(crate) message: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct SemanticEditPatchFileProposal {
    pub(crate) file: String,
    pub(crate) language: String,
    pub(crate) before_hash: String,
    pub(crate) after_hash: String,
    pub(crate) hunks: Vec<SemanticEditPatchHunk>,
}

#[derive(Serialize, Clone)]
pub(crate) struct SemanticEditPatchHunk {
    pub(crate) before: SemanticEditPatchRange,
    pub(crate) after: SemanticEditPatchRange,
    pub(crate) context_before: usize,
    pub(crate) context_after: usize,
    pub(crate) preview_truncated: bool,
    pub(crate) diff: String,
}

#[derive(Serialize, Clone)]
pub(crate) struct SemanticEditPatchRange {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) line_count: usize,
}

#[derive(Serialize)]
pub(crate) struct SemanticEditIntentReport {
    pub(crate) root: String,
    pub(crate) mode: String,
    pub(crate) intents_total: usize,
    pub(crate) planned_total: usize,
    pub(crate) applied_total: usize,
    pub(crate) conflict_total: usize,
    pub(crate) unsupported_total: usize,
    pub(crate) formatted_total: usize,
    pub(crate) plans: Vec<SemanticEditIntentPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) verification: Option<SemanticEditVerificationReport>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) warnings: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct SemanticEditVerificationReport {
    pub(crate) status: String,
    pub(crate) worktree: String,
    pub(crate) reindexed: bool,
    pub(crate) temp_applied_total: usize,
    pub(crate) temp_formatted_total: usize,
    pub(crate) source_reads: Vec<SemanticEditVerificationSourceRead>,
    pub(crate) impact: SemanticEditVerificationImpact,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<SemanticEditVerificationCommand>,
    pub(crate) message: String,
}

#[derive(Serialize)]
pub(crate) struct SemanticEditVerificationSourceRead {
    pub(crate) file: String,
    pub(crate) start: usize,
    pub(crate) lines: usize,
    pub(crate) preview_lines: usize,
    pub(crate) symbol_refs: usize,
    pub(crate) summary_refs: usize,
    pub(crate) command: String,
}

#[derive(Serialize)]
pub(crate) struct SemanticEditVerificationImpact {
    pub(crate) changed_files: usize,
    pub(crate) changed_symbols: usize,
    pub(crate) affected_tests: usize,
    pub(crate) affected_tests_total: usize,
    pub(crate) truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) warnings: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct SemanticEditVerificationCommand {
    pub(crate) command: String,
    pub(crate) status: String,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Clone, Copy)]
pub(crate) struct SemanticEditVerifyOptions<'a> {
    pub(crate) enabled: bool,
    pub(crate) command: Option<&'a str>,
}

pub(crate) struct SemanticEditIntentDraft {
    pub(crate) plan: SemanticEditIntentPlan,
    pub(crate) file_abs: PathBuf,
    pub(crate) destination_file_abs: Option<PathBuf>,
    pub(crate) language: String,
    /// Other files a `rename_symbol` must rewrite for the tree to still build.
    pub(crate) rename_caller_files: Vec<PathBuf>,
}

struct SemanticEditResolvedHandleTarget {
    target_symbol: SemanticEditSymbolTarget,
    file_abs: PathBuf,
    target_range: SourceRangePreview,
    selection: SemanticEditTargetSelection,
}

struct SemanticEditTargetSelectionInput<'a> {
    requested_handle: &'a str,
    matched_handle: &'a str,
    handle_family: &'a str,
    source: &'a str,
    symbol: &'a index::StoredSymbol,
    file_abs: &'a Path,
    span: &'a AstSpanPreview,
}

#[derive(Deserialize)]
pub(crate) struct EditOp {
    /// File path to edit
    pub(crate) file: PathBuf,
    /// Text to find and replace
    pub(crate) old: String,
    /// Replacement text
    pub(crate) new: String,
    /// Replace all occurrences (default: false — fails if not unique)
    #[serde(default)]
    pub(crate) replace_all: bool,
}

pub(crate) struct MetricDigestOptions<'a> {
    pub(crate) input_path: Option<&'a Path>,
    pub(crate) baseline_path: Option<&'a Path>,
    pub(crate) metrics: &'a [String],
    pub(crate) lower_is_better: &'a [String],
    pub(crate) higher_is_better: &'a [String],
    pub(crate) history: usize,
    pub(crate) top: usize,
}

#[derive(Serialize)]
pub(crate) struct EditResult {
    pub(crate) file: PathBuf,
    pub(crate) status: EditStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) replacements: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EditStatus {
    Ok,
    Skipped,
}

pub(crate) const SEMANTIC_EDIT_RUST_KINDS: &[&str] = &[
    "rename_symbol",
    "replace_function_body",
    "insert_import",
    "add_method",
    "update_call_signature",
    "move_declaration",
    "rewrite_call_sites",
    "structural_rewrite",
];
/// The script tier: Python and the JS-like grammars.
///
/// `extract_function` is here rather than on Python alone because the family
/// this tier describes *is* the family whose signature is derivable without
/// type information — an untyped parameter list and an explicit `return`.
/// Rust is deliberately absent: choosing `T`, `&T`, or `&mut T` needs types
/// `tsift-graph` does not have, and a guessed signature parses without
/// building.
///
/// TypeScript is in it only because it can *copy* an annotation the file
/// already has. Where it cannot, the planner refuses rather than writing
/// `unknown` or leaving a parameter implicitly `any`, so the registration
/// advertises an edit that either lands correctly or declines by name — never
/// one that type-checks something other than what the code does.
pub(crate) const SEMANTIC_EDIT_SCRIPT_KINDS: &[&str] = &[
    "rename_symbol",
    "replace_function_body",
    "insert_import",
    "structural_rewrite",
    "extract_function",
];
/// Languages with an ast-grep grammar and no `tsift-graph` binding.
/// `structural_rewrite` needs neither an index nor a per-kind executor: it is
/// dispatched ahead of the family split and only requires a grammar to match
/// with and one to reparse the result. The symbol-resolved kinds do need a
/// resolvable symbol, so they stay unrecognized rather than falling through to
/// another family's implementation.
pub(crate) const SEMANTIC_EDIT_STRUCTURAL_KINDS: &[&str] = &["structural_rewrite"];
/// Indexed languages with no per-kind executor of their own.
///
/// `rename_symbol` is here because it is not per-kind work at all: identifier
/// occurrences come out of the grammar and the rename's extent comes out of the
/// call graph, both of which every indexed language already has. The kinds that
/// are absent — `replace_function_body`, `insert_import`, `add_method` — need
/// language-specific rewriting that these executors genuinely do not have.
pub(crate) const SEMANTIC_EDIT_INDEXED_KINDS: &[&str] = &["rename_symbol", "structural_rewrite"];
/// The same tier for an indexed language this build compiled no ast-grep
/// grammar for. Dropping `structural_rewrite` is a refusal by registration:
/// advertising it would mean planning an edit that `preview_structural_rewrite`
/// can only fail on.
pub(crate) const SEMANTIC_EDIT_INDEXED_RENAME_ONLY_KINDS: &[&str] = &["rename_symbol"];
/// The same tier for an indexed language that also has an extraction emitter.
///
/// GDScript is the one language in that position: no ast-grep grammar in this
/// build, so no `structural_rewrite`, but an indentation-scoped block and an
/// untyped `func` signature, so an extraction is derivable. Splitting the row
/// keeps the two facts independent — a grammar arriving later changes one of
/// them without silently changing the other.
pub(crate) const SEMANTIC_EDIT_INDEXED_EXTRACT_KINDS: &[&str] =
    &["rename_symbol", "extract_function"];
pub(crate) const SEMANTIC_EDIT_MARKDOWN_KINDS: &[&str] = &[
    "rename_heading",
    "replace_section_body",
    "insert_section",
    "move_section",
    "insert_list_item",
    "rewrite_code_fence",
    "structural_rewrite",
];
pub(crate) const SEMANTIC_EDIT_MARKDOWN_APPLY_KINDS: &[&str] = &[
    "rename_heading",
    "replace_section_body",
    "insert_section",
    "move_section",
    "insert_list_item",
    "rewrite_code_fence",
    "structural_rewrite",
];
pub(crate) const SEMANTIC_EDIT_KINDS: &[&str] = &[
    "rename_symbol",
    "replace_function_body",
    "insert_import",
    "add_method",
    "update_call_signature",
    "move_declaration",
    "rewrite_call_sites",
    "rename_heading",
    "replace_section_body",
    "insert_section",
    "move_section",
    "insert_list_item",
    "rewrite_code_fence",
    "structural_rewrite",
    "extract_function",
];

pub(crate) struct PlannedEdit {
    pub(crate) index: usize,
    pub(crate) file: PathBuf,
    pub(crate) new_content: String,
    pub(crate) replacements: usize,
}

pub(crate) struct StagedEdit {
    pub(crate) index: usize,
    pub(crate) file: PathBuf,
    pub(crate) replacements: usize,
    pub(crate) staged_file: NamedTempFile,
}

pub(crate) struct AppliedEdit {
    pub(crate) index: usize,
    pub(crate) file: PathBuf,
    pub(crate) replacements: usize,
    pub(crate) backup_path: PathBuf,
}

fn normalize_semantic_edit_kind(kind: &str) -> String {
    kind.trim().replace('-', "_")
}

fn semantic_edit_kind_requires_symbol(kind: &str) -> bool {
    matches!(
        kind,
        "rename_symbol"
            | "add_method"
            | "replace_function_body"
            | "update_call_signature"
            | "move_declaration"
            | "rewrite_call_sites"
            | "rename_heading"
            | "replace_section_body"
            | "move_section"
            | "insert_list_item"
            | "rewrite_code_fence"
    )
}

fn semantic_edit_kind_requires_replacement(kind: &str) -> bool {
    matches!(
        kind,
        "replace_function_body"
            | "insert_import"
            | "add_method"
            | "update_call_signature"
            | "rewrite_call_sites"
            | "replace_section_body"
            | "insert_section"
            | "insert_list_item"
            | "rewrite_code_fence"
            | "structural_rewrite"
    )
}

/// `structural_rewrite` is the only kind that selects its target by ast-grep
/// pattern instead of by resolved symbol, so it is the only kind that both
/// requires `pattern` and accepts it.
fn semantic_edit_kind_requires_pattern(kind: &str) -> bool {
    matches!(kind, "structural_rewrite")
}

fn semantic_edit_kind_requires_new_name(kind: &str) -> bool {
    matches!(kind, "rename_symbol" | "rename_heading" | "extract_function")
}

/// `extract_function` is the only kind that selects a range instead of a named
/// target, so it is the only kind that both requires the range and accepts it.
fn semantic_edit_kind_requires_line_range(kind: &str) -> bool {
    matches!(kind, "extract_function")
}

fn semantic_edit_kind_requires_destination_symbol(kind: &str) -> bool {
    matches!(kind, "move_section")
}

fn semantic_edit_kind_requires_file(kind: &str) -> bool {
    matches!(
        kind,
        "insert_import"
            | "move_declaration"
            | "insert_section"
            | "move_section"
            | "insert_list_item"
            | "rewrite_code_fence"
            | "structural_rewrite"
            | "extract_function"
    )
}

fn validate_semantic_edit_intent(kind: &str, intent: &SemanticEditIntent) -> Result<()> {
    if !SEMANTIC_EDIT_KINDS.contains(&kind) {
        bail!(
            "unknown semantic edit kind {kind:?}; expected one of {}",
            SEMANTIC_EDIT_KINDS.join(", ")
        );
    }
    let has_target_handle = intent
        .target_handle
        .as_deref()
        .is_some_and(|handle| !handle.trim().is_empty());
    if semantic_edit_kind_requires_symbol(kind)
        && intent.symbol.as_deref().is_none_or(str::is_empty)
        && !has_target_handle
    {
        bail!("semantic edit kind {kind:?} requires `symbol` or `target_handle`");
    }
    if semantic_edit_kind_requires_file(kind)
        && intent.file.is_none()
        && !(has_target_handle && kind != "move_declaration")
    {
        bail!("semantic edit kind {kind:?} requires `file`");
    }
    if semantic_edit_kind_requires_replacement(kind)
        && intent.replacement.as_deref().is_none_or(str::is_empty)
    {
        bail!("semantic edit kind {kind:?} requires `replacement`");
    }
    if semantic_edit_kind_requires_new_name(kind)
        && intent.new_name.as_deref().is_none_or(str::is_empty)
    {
        bail!("semantic edit kind {kind:?} requires `new_name`");
    }
    if semantic_edit_kind_requires_destination_symbol(kind)
        && intent
            .destination_symbol
            .as_deref()
            .is_none_or(str::is_empty)
    {
        bail!("semantic edit kind {kind:?} requires `destination_symbol`");
    }
    if semantic_edit_kind_requires_pattern(kind)
        && intent.pattern.as_deref().is_none_or(str::is_empty)
    {
        bail!("semantic edit kind {kind:?} requires `pattern`");
    }
    if intent.pattern.is_some() && !semantic_edit_kind_requires_pattern(kind) {
        bail!("semantic edit kind {kind:?} does not support `pattern`");
    }
    let has_line_range = intent.start_line.is_some() || intent.end_line.is_some();
    if semantic_edit_kind_requires_line_range(kind) {
        let (Some(start), Some(end)) = (intent.start_line, intent.end_line) else {
            bail!("semantic edit kind {kind:?} requires `start_line` and `end_line`");
        };
        if start == 0 {
            bail!("semantic edit `start_line` is one-based, so 0 is not a line");
        }
        if end < start {
            bail!("semantic edit `end_line` must not precede `start_line`");
        }
    } else if has_line_range {
        bail!("semantic edit kind {kind:?} does not support `start_line`/`end_line`");
    }
    if let Some(position) = intent.position.as_deref() {
        if !matches!(position, "before" | "after") {
            bail!("semantic edit `position` must be either \"before\" or \"after\"");
        }
        if !matches!(kind, "insert_section" | "move_section" | "insert_list_item") {
            bail!("semantic edit kind {kind:?} does not support `position`");
        }
    }
    Ok(())
}

fn resolve_semantic_edit_symbol(
    root: &Path,
    scope: Option<&str>,
    symbol: &str,
    file_hint: Option<&Path>,
    budget: ResponseBudget,
) -> Result<(index::SymbolHit, PathBuf)> {
    let hinted_file_abs = file_hint
        .map(|file| resolve_source_file(root, file))
        .transpose()?;
    let path_hint = hinted_file_abs.as_deref().unwrap_or(root);
    let db_path = resolve_query_db_path(root, path_hint, scope)?;
    if !db_path.exists() {
        bail!(
            "index refs unavailable: no index found at {}",
            db_path.display()
        );
    }
    let db = index::IndexDb::open_read_only_resilient(&db_path)
        .with_context(|| format!("opening symbol index {}", db_path.display()))?;
    let hits = db
        .symbol_search(symbol, budget.follow_up_items().max(10))
        .with_context(|| format!("searching symbols for {symbol:?}"))?;
    let hit = hits
        .into_iter()
        .find(|hit| {
            let Some(hinted_file_abs) = &hinted_file_abs else {
                return true;
            };
            resolve_source_file(root, Path::new(&hit.file))
                .map(|hit_file| hit_file == *hinted_file_abs)
                .unwrap_or(false)
        })
        .with_context(|| format!("no indexed symbol matched {symbol:?}"))?;
    let file_abs = resolve_source_file(root, Path::new(&hit.file))?;
    Ok((hit, file_abs))
}

fn resolve_semantic_edit_call_refs(
    root: &Path,
    scope: Option<&str>,
    symbol: &str,
    target_file_abs: &Path,
) -> Result<(Vec<SemanticEditCallRef>, usize)> {
    let db_path = resolve_query_db_path(root, target_file_abs, scope)?;
    if !db_path.exists() {
        bail!(
            "index refs unavailable: no index found at {}",
            db_path.display()
        );
    }
    let db = index::IndexDb::open_read_only_resilient(&db_path)
        .with_context(|| format!("opening symbol index {}", db_path.display()))?;
    let edges = db
        .callers_of(symbol)
        .with_context(|| format!("loading indexed call refs for {symbol:?}"))?;
    let mut refs = Vec::new();
    let mut cross_file = 0usize;
    for edge in edges {
        let caller_file_abs = resolve_source_file(root, Path::new(&edge.caller_file))
            .with_context(|| format!("resolving indexed caller file {}", edge.caller_file))?;
        let line = usize::try_from(edge.call_site_line)
            .ok()
            .and_then(|line| line.checked_add(1))
            .unwrap_or(1);
        if caller_file_abs == target_file_abs {
            refs.push(SemanticEditCallRef {
                file: semantic_edit_file_display(root, &caller_file_abs),
                caller: edge.caller_name,
                line,
            });
        } else {
            cross_file += 1;
        }
    }
    refs.sort_by(|left, right| {
        left.line
            .cmp(&right.line)
            .then(left.caller.cmp(&right.caller))
    });
    Ok((refs, cross_file))
}

/// The executor family a file renames within. `None` means the file has no
/// registered executor, which cannot match any target and so is excluded.
/// What the index resolved this rename target to be, for the occurrence walk.
///
/// A grammar can tell a struct field from a function call only when it is told
/// which of the two it is looking for. With no resolved symbol there is nothing
/// to narrow by, and the walk keeps every identifier kind.
fn semantic_edit_rename_target(
    target_symbol: Option<&SemanticEditSymbolTarget>,
) -> graph::RenameTarget {
    target_symbol
        .map(|symbol| graph::RenameTarget::from_indexed_kind(&symbol.kind))
        .unwrap_or_default()
}

fn semantic_edit_rename_family(file_abs: &Path) -> Option<&'static str> {
    let language = semantic_edit_language_for_file(file_abs);
    semantic_edit_executor_language(&language, file_abs).map(|executor| executor.rename_family())
}

/// Which files a rename has to touch, and whether it is safe to touch them.
pub(crate) struct SemanticEditRenameScope {
    /// Files other than the declaring file that reference the symbol.
    pub(crate) caller_files: Vec<PathBuf>,
    /// Files holding a second definition of the same name, in the same family.
    pub(crate) ambiguous_definition_files: Vec<String>,
}

impl SemanticEditRenameScope {
    /// A competing definition only makes a rename unsafe when there is a
    /// cross-file reference to *attribute*. With no cross-file callers the
    /// rename never leaves the declaring file, so another module defining the
    /// same name is irrelevant — two independent modules each with their own
    /// `beta` is ordinary, not ambiguous. Refusing there would block the common
    /// case to guard one that cannot occur.
    fn is_ambiguous(&self) -> bool {
        !self.caller_files.is_empty() && !self.ambiguous_definition_files.is_empty()
    }
}

/// Resolve the cross-file extent of a rename from the indexed call graph.
///
/// A rename used to edit one file and report success, leaving every caller in
/// every other file referring to a name that no longer exists — a tree that
/// does not compile, reported as `conflicts=0`. The index already knows which
/// files call the symbol; this reads that out so the rename can either cover
/// them or decline.
///
/// The ambiguity check is the reason this cannot just rename every file that
/// mentions the name. Two definitions sharing a name (a method on an unrelated
/// type, say) make each reference unattributable from call edges alone, and a
/// rename that silently renames the wrong one is worse than a rename that
/// refuses.
fn resolve_semantic_edit_rename_scope(
    root: &Path,
    scope: Option<&str>,
    symbol: &str,
    target_file_abs: &Path,
) -> Result<SemanticEditRenameScope> {
    let db_path = resolve_query_db_path(root, target_file_abs, scope)?;
    if !db_path.exists() {
        bail!(
            "index refs unavailable: no index found at {}",
            db_path.display()
        );
    }
    let db = index::IndexDb::open_read_only_resilient(&db_path)
        .with_context(|| format!("opening symbol index {}", db_path.display()))?;

    // Both lookups below are by name, and a name is not unique across
    // languages: a Python `beta` and a JavaScript `beta` are unrelated symbols
    // that no rename of one can affect. Scope everything to the target's
    // executor family, which keeps `.ts`/`.tsx`/`.js`/`.jsx` together (they do
    // call each other) while separating languages that cannot.
    let target_family = semantic_edit_rename_family(target_file_abs);
    let same_family = |file_abs: &Path| semantic_edit_rename_family(file_abs) == target_family;

    let mut definition_files = BTreeSet::new();
    let mut ambiguous_definition_files = Vec::new();
    for definition in db
        .symbol_info(symbol)
        .with_context(|| format!("loading indexed definitions for {symbol:?}"))?
    {
        let definition_abs = resolve_source_file(root, Path::new(&definition.file))
            .unwrap_or_else(|_| PathBuf::from(&definition.file));
        if definition_abs != target_file_abs && same_family(&definition_abs) {
            ambiguous_definition_files.push(semantic_edit_file_display(root, &definition_abs));
            definition_files.insert(definition_abs);
        }
    }
    ambiguous_definition_files.sort();
    ambiguous_definition_files.dedup();

    let mut caller_files = Vec::new();
    for edge in db
        .callers_of(symbol)
        .with_context(|| format!("loading indexed call refs for {symbol:?}"))?
    {
        let caller_file_abs = resolve_source_file(root, Path::new(&edge.caller_file))
            .with_context(|| format!("resolving indexed caller file {}", edge.caller_file))?;
        if caller_file_abs == target_file_abs || !same_family(&caller_file_abs) {
            continue;
        }
        // Call edges are matched by name, not by resolved binding, so a file
        // that defines its own function of the same name looks like a caller of
        // ours. Its own definition shadows any import, so it is calling itself.
        // Renaming there would rewrite an unrelated function.
        if definition_files.contains(&caller_file_abs) {
            continue;
        }
        caller_files.push(caller_file_abs);
    }
    caller_files.sort();
    caller_files.dedup();

    Ok(SemanticEditRenameScope {
        caller_files,
        ambiguous_definition_files,
    })
}

pub(crate) struct SemanticEditRenameFilePreview {
    pub(crate) file_abs: PathBuf,
    pub(crate) language: String,
    pub(crate) before: String,
    pub(crate) after: String,
}

/// Rename the symbol in every referencing file, so the patch proposal, the
/// diff preview, and `--verify` all see the whole change rather than the
/// declaring file alone.
fn semantic_edit_rename_patch_inputs(
    _root: &Path,
    rename_scope: Option<&(String, SemanticEditRenameScope)>,
    new_name: Option<&str>,
    target: graph::RenameTarget,
) -> Result<Vec<SemanticEditRenameFilePreview>> {
    let Some((symbol, scope)) = rename_scope else {
        return Ok(Vec::new());
    };
    let Some(new_name) = new_name else {
        return Ok(Vec::new());
    };
    let mut previews = Vec::new();
    for file_abs in &scope.caller_files {
        let language = semantic_edit_language_for_file(file_abs);
        let Some(executor) = semantic_edit_executor_language(&language, file_abs) else {
            continue;
        };
        let before = fs::read_to_string(file_abs)
            .with_context(|| format!("reading rename caller file {}", file_abs.display()))?;
        let Some(lang) = executor.contract().graph_lang else {
            continue;
        };
        // A caller file that no longer mentions the symbol is not an error: the
        // index can lag a deletion. It simply contributes no patch.
        let Ok((after, replacements)) =
            rename_identifier_occurrences(&before, symbol, new_name, lang, executor.name(), target)
        else {
            continue;
        };
        if replacements == 0 {
            continue;
        }
        previews.push(SemanticEditRenameFilePreview {
            file_abs: file_abs.clone(),
            language,
            before,
            after,
        });
    }
    Ok(previews)
}

fn semantic_edit_handle_family(handle: &str) -> Option<(&'static str, &'static str)> {
    if handle.starts_with("span-") {
        Some(("ast_span", "search/source/symbol AST span"))
    } else if handle.starts_with("ssym-") {
        Some(("source_symbol", "source-read symbol reference"))
    } else if handle.starts_with("sread-") {
        Some(("symbol_read", "symbol-read target"))
    } else if handle.starts_with("gsym-") {
        Some(("graph_symbol", "graph traversal symbol"))
    } else {
        None
    }
}

fn semantic_edit_stored_symbol_handles(
    root: &Path,
    symbol: &index::StoredSymbol,
    file_abs: &Path,
    span: &AstSpanPreview,
) -> Vec<(String, &'static str, &'static str)> {
    let file_display = semantic_edit_file_display(root, file_abs);
    let mut handles = vec![
        (
            span.handle.clone(),
            "ast_span",
            "search/source/symbol AST span",
        ),
        (
            stable_handle(
                "ssym",
                &format!(
                    "{}:{}:{}",
                    file_display,
                    symbol.name,
                    source_symbol_line(symbol)
                ),
            ),
            "source_symbol",
            "source-read symbol reference",
        ),
        (
            stable_handle(
                "sread",
                &format!("{}:{}:{}", file_display, symbol.name, span.start_line),
            ),
            "symbol_read",
            "symbol-read target",
        ),
        (
            stable_handle(
                "gsym",
                &format!("symbol:{}:{}:{}", file_display, symbol.line, symbol.name),
            ),
            "graph_symbol",
            "graph traversal symbol",
        ),
    ];
    let display_span_handle = stable_handle(
        "span",
        &format!(
            "{}:{}:{}:{}:{}",
            file_display, symbol.kind, symbol.name, span.start_byte, span.end_byte
        ),
    );
    if display_span_handle != span.handle {
        handles.push((
            display_span_handle,
            "ast_span",
            "search/source/symbol AST span",
        ));
    }
    handles
}

fn semantic_edit_target_selection(
    root: &Path,
    input: SemanticEditTargetSelectionInput<'_>,
) -> (
    SemanticEditSymbolTarget,
    SourceRangePreview,
    SemanticEditTargetSelection,
) {
    let SemanticEditTargetSelectionInput {
        requested_handle,
        matched_handle,
        handle_family,
        source,
        symbol,
        file_abs,
        span,
    } = input;
    let file_display = semantic_edit_file_display(root, file_abs);
    let line_count = span
        .end_line
        .saturating_sub(span.start_line)
        .saturating_add(1)
        .max(1);
    let target_symbol = SemanticEditSymbolTarget {
        name: symbol.name.clone(),
        kind: symbol.kind.clone(),
        language: symbol.language.clone(),
        file: file_display.clone(),
        line: span.start_line,
        end_line: Some(span.end_line),
        span: Some(span.clone()),
    };
    let target_range = SourceRangePreview {
        start: span.start_line,
        end: span.end_line,
        total_lines: 0,
        truncated_before: false,
        truncated_after: false,
    };
    let selection = SemanticEditTargetSelection {
        requested_handle: requested_handle.to_string(),
        matched_handle: matched_handle.to_string(),
        handle_family: handle_family.to_string(),
        source: source.to_string(),
        file: file_display.clone(),
        name: symbol.name.clone(),
        kind: symbol.kind.clone(),
        language: symbol.language.clone(),
        line: span.start_line,
        end_line: span.end_line,
        span: span.clone(),
        source_window: source_read_command(root, &file_display, span.start_line, line_count),
        symbol_read: source_symbol_read_command(root, &symbol.name, &file_display),
        message: "resolved target_handle to a concrete indexed AST span without mutating source"
            .to_string(),
    };
    (target_symbol, target_range, selection)
}

fn resolve_semantic_edit_target_handle(
    root: &Path,
    scope: Option<&str>,
    handle: &str,
    file_hint: Option<&Path>,
    budget: ResponseBudget,
) -> Result<SemanticEditResolvedHandleTarget> {
    let handle = handle.trim();
    if handle.is_empty() {
        bail!("semantic edit `target_handle` must not be empty");
    }
    if matches!(
        handle.split_once('-').map(|(prefix, _)| prefix),
        Some("sfam" | "srnk" | "shit")
    ) {
        bail!(
            "search preview handle {handle:?} is not a concrete AST/CST target; pass the nested `ast.span.handle` from the search result instead"
        );
    }
    let Some((expected_family, expected_source)) = semantic_edit_handle_family(handle) else {
        bail!(
            "unsupported semantic edit target_handle {handle:?}; expected span-*, ssym-*, sread-*, or gsym-*"
        );
    };

    let hinted_file_abs = file_hint
        .map(|file| resolve_source_file(root, file))
        .transpose()?;
    let path_hint = hinted_file_abs.as_deref().unwrap_or(root);
    let db_path = resolve_query_db_path(root, path_hint, scope)?;
    if !db_path.exists() {
        bail!(
            "index refs unavailable: no index found at {}",
            db_path.display()
        );
    }
    let db = index::IndexDb::open_read_only_resilient(&db_path)
        .with_context(|| format!("opening symbol index {}", db_path.display()))?;
    let symbols = db
        .all_symbols()
        .with_context(|| format!("loading symbols from {}", db_path.display()))?;
    let mut symbols_by_file: BTreeMap<String, Vec<index::StoredSymbol>> = BTreeMap::new();
    for symbol in &symbols {
        symbols_by_file
            .entry(symbol.file.clone())
            .or_default()
            .push(symbol.clone());
    }
    let mut source_cache: BTreeMap<String, (PathBuf, Vec<u8>)> = BTreeMap::new();
    let mut matches = Vec::new();

    for symbol in &symbols {
        if !source_cache.contains_key(&symbol.file) {
            let file_abs = resolve_source_file(root, Path::new(&symbol.file))
                .with_context(|| format!("resolving indexed source file for {}", symbol.file))?;
            let source =
                fs::read(&file_abs).with_context(|| format!("reading {}", file_abs.display()))?;
            source_cache.insert(symbol.file.clone(), (file_abs, source));
        }
        let (file_abs, source) = source_cache
            .get(&symbol.file)
            .expect("source cache populated above");
        if hinted_file_abs
            .as_ref()
            .is_some_and(|hinted| hinted != file_abs)
        {
            continue;
        }
        let file_symbols = symbols_by_file
            .get(&symbol.file)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let Some(span) =
            stored_symbol_ast_span(symbol, source, file_symbols, budget.preview_items())
        else {
            continue;
        };
        for (candidate, family, source_label) in
            semantic_edit_stored_symbol_handles(root, symbol, file_abs, &span)
        {
            if candidate == handle {
                let (target_symbol, target_range, selection) = semantic_edit_target_selection(
                    root,
                    SemanticEditTargetSelectionInput {
                        requested_handle: handle,
                        matched_handle: &candidate,
                        handle_family: family,
                        source: source_label,
                        symbol,
                        file_abs,
                        span: &span,
                    },
                );
                matches.push(SemanticEditResolvedHandleTarget {
                    target_symbol,
                    file_abs: file_abs.clone(),
                    target_range,
                    selection,
                });
            }
        }
    }

    if matches.is_empty() {
        bail!(
            "{expected_source} handle {handle:?} did not match any indexed concrete AST span in {}",
            db_path.display()
        );
    }
    if matches.len() > 1 {
        bail!(
            "{expected_family} handle {handle:?} resolved ambiguously to {} indexed targets; add `file` to narrow it",
            matches.len()
        );
    }
    Ok(matches.remove(0))
}

fn semantic_edit_file_display(root: &Path, file_abs: &Path) -> String {
    relativize_pathbuf(file_abs, root)
        .to_string_lossy()
        .to_string()
}

fn semantic_edit_content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn semantic_edit_language_for_file(file_abs: &Path) -> String {
    let Some(ext) = file_abs.extension().and_then(|value| value.to_str()) else {
        return "unknown".to_string();
    };
    semantic_edit_language_contract_for_extension(ext)
        .map(|contract| contract.id.to_string())
        .unwrap_or_else(|| ext.to_string())
}

fn semantic_edit_target_language(
    target_symbol: Option<&SemanticEditSymbolTarget>,
    file_abs: &Path,
) -> String {
    target_symbol
        .map(|symbol| symbol.language.clone())
        .unwrap_or_else(|| semantic_edit_language_for_file(file_abs))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SemanticEditExecutorLanguage {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Markdown,
    Kotlin,
    Bash,
    // Indexed executors with no ast-grep grammar in this build: renamable
    // through the `tsift-graph` binding, but not structurally matchable.
    Zig,
    GdScript,
    // Structural-only executors with no `tsift-graph` binding. Reparse goes
    // through the ast-grep grammar instead — see `reparse_language`.
    C,
    Cpp,
    CSharp,
    Css,
    Dart,
    Elixir,
    Go,
    Haskell,
    Hcl,
    Html,
    Java,
    Json,
    Lua,
    Nix,
    Php,
    Ruby,
    Scala,
    Solidity,
    Swift,
    Yaml,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SemanticEditLanguageFamily {
    Rust,
    Python,
    JsLike,
    Markdown,
    /// Indexed by `tsift-graph` but with no per-kind executor of its own.
    ///
    /// `rename_symbol` is language-general — it reads identifier nodes out of
    /// the tree and rewrites their spans — so it needs a grammar and an index,
    /// not a hand-written per-family scan. Every other symbol-resolved kind
    /// still does need language-specific rewriting and stays unrecognized here.
    Indexed,
    /// A grammar and nothing else: no per-kind rewriting, structural patterns
    /// only. These languages have no `tsift-graph` binding, so they are not
    /// indexed, searchable, or graphable, and nothing can resolve a symbol in
    /// them to rename.
    Structural,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SemanticEditFormatterContract {
    Rustfmt,
    PythonAuto,
    Prettier,
    None,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SemanticEditLanguageContract {
    pub(crate) executor: SemanticEditExecutorLanguage,
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    /// The `tsift-graph` binding for this language, when it has one.
    ///
    /// `None` is not a degraded state — it means the language is rewritable but
    /// not indexed. Reparsing the planner's output needs a grammar, not an
    /// index, so it falls back to the ast-grep grammar rather than refusing.
    pub(crate) graph_lang: Option<graph::Lang>,
    pub(crate) temp_suffix: &'static str,
    pub(crate) aliases: &'static [&'static str],
    pub(crate) extensions: &'static [&'static str],
    pub(crate) recognized_intents: &'static [&'static str],
    pub(crate) apply_supported_intents: &'static [&'static str],
    pub(crate) family: SemanticEditLanguageFamily,
    pub(crate) formatter: SemanticEditFormatterContract,
}

const SEMANTIC_EDIT_LANGUAGE_CONTRACTS: &[SemanticEditLanguageContract] = &[
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Rust,
        id: "rust",
        name: "Rust",
        graph_lang: Some(graph::Lang::Rust),
        temp_suffix: ".rs",
        aliases: &["rust", "rs"],
        extensions: &["rs"],
        recognized_intents: SEMANTIC_EDIT_RUST_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_RUST_KINDS,
        family: SemanticEditLanguageFamily::Rust,
        formatter: SemanticEditFormatterContract::Rustfmt,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Python,
        id: "python",
        name: "Python",
        graph_lang: Some(graph::Lang::Python),
        temp_suffix: ".py",
        aliases: &["python", "py", "pyi"],
        extensions: &["py", "pyi"],
        recognized_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        family: SemanticEditLanguageFamily::Python,
        formatter: SemanticEditFormatterContract::PythonAuto,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::TypeScript,
        id: "typescript",
        name: "TypeScript",
        graph_lang: Some(graph::Lang::TypeScript),
        temp_suffix: ".ts",
        aliases: &["typescript", "ts"],
        extensions: &["ts"],
        recognized_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        family: SemanticEditLanguageFamily::JsLike,
        formatter: SemanticEditFormatterContract::Prettier,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Tsx,
        id: "tsx",
        name: "TSX",
        graph_lang: Some(graph::Lang::Tsx),
        temp_suffix: ".tsx",
        aliases: &["tsx"],
        extensions: &["tsx"],
        recognized_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        family: SemanticEditLanguageFamily::JsLike,
        formatter: SemanticEditFormatterContract::Prettier,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::JavaScript,
        id: "javascript",
        name: "JavaScript",
        graph_lang: Some(graph::Lang::JavaScript),
        temp_suffix: ".js",
        aliases: &["javascript", "js", "mjs", "cjs"],
        extensions: &["js", "mjs", "cjs"],
        recognized_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        family: SemanticEditLanguageFamily::JsLike,
        formatter: SemanticEditFormatterContract::Prettier,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Jsx,
        id: "jsx",
        name: "JSX",
        graph_lang: Some(graph::Lang::Jsx),
        temp_suffix: ".jsx",
        aliases: &["jsx"],
        extensions: &["jsx"],
        recognized_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_SCRIPT_KINDS,
        family: SemanticEditLanguageFamily::JsLike,
        formatter: SemanticEditFormatterContract::Prettier,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Markdown,
        id: "markdown",
        name: "Markdown",
        graph_lang: Some(graph::Lang::Markdown),
        temp_suffix: ".md",
        aliases: &["markdown", "md", "mdx"],
        extensions: &["md", "mdx"],
        recognized_intents: SEMANTIC_EDIT_MARKDOWN_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_MARKDOWN_APPLY_KINDS,
        family: SemanticEditLanguageFamily::Markdown,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Kotlin,
        id: "kotlin",
        name: "Kotlin",
        graph_lang: Some(graph::Lang::Kotlin),
        temp_suffix: ".kt",
        aliases: &["kotlin", "kt", "kts"],
        extensions: &["kt", "kts"],
        recognized_intents: SEMANTIC_EDIT_INDEXED_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_INDEXED_KINDS,
        family: SemanticEditLanguageFamily::Indexed,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Bash,
        id: "bash",
        name: "Bash",
        graph_lang: Some(graph::Lang::Bash),
        temp_suffix: ".sh",
        aliases: &["bash", "sh", "shell"],
        extensions: &["sh", "bash"],
        recognized_intents: SEMANTIC_EDIT_INDEXED_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_INDEXED_KINDS,
        family: SemanticEditLanguageFamily::Indexed,
        formatter: SemanticEditFormatterContract::None,
    },
    // Indexed, but with no ast-grep grammar compiled in this build, so the
    // recognized set is `rename_symbol` alone.
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Zig,
        id: "zig",
        name: "Zig",
        graph_lang: Some(graph::Lang::Zig),
        temp_suffix: ".zig",
        aliases: &["zig"],
        extensions: &["zig"],
        recognized_intents: SEMANTIC_EDIT_INDEXED_RENAME_ONLY_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_INDEXED_RENAME_ONLY_KINDS,
        family: SemanticEditLanguageFamily::Indexed,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::GdScript,
        id: "gdscript",
        name: "GDScript",
        graph_lang: Some(graph::Lang::GdScript),
        temp_suffix: ".gd",
        aliases: &["gdscript", "gd"],
        extensions: &["gd"],
        recognized_intents: SEMANTIC_EDIT_INDEXED_EXTRACT_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_INDEXED_EXTRACT_KINDS,
        family: SemanticEditLanguageFamily::Indexed,
        formatter: SemanticEditFormatterContract::None,
    },
    // Structural-only tier: an ast-grep grammar and no `tsift-graph` binding.
    // They are not indexed, searchable, or graphable, and their recognized set
    // is `structural_rewrite` alone — the one kind that selects by shape rather
    // than by a resolved symbol, and so needs only a grammar.
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::C,
        id: "c",
        name: "C",
        graph_lang: None,
        temp_suffix: ".c",
        aliases: &["c"],
        extensions: &["c", "h"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Cpp,
        id: "cpp",
        name: "C++",
        graph_lang: None,
        temp_suffix: ".cpp",
        aliases: &["cpp", "c++", "cc", "cxx"],
        extensions: &["cc", "cpp", "cxx", "hpp", "hh", "hxx"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::CSharp,
        id: "csharp",
        name: "C#",
        graph_lang: None,
        temp_suffix: ".cs",
        aliases: &["csharp", "cs", "c#"],
        extensions: &["cs"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Css,
        id: "css",
        name: "CSS",
        graph_lang: None,
        temp_suffix: ".css",
        aliases: &["css"],
        extensions: &["css"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Dart,
        id: "dart",
        name: "Dart",
        graph_lang: None,
        temp_suffix: ".dart",
        aliases: &["dart"],
        extensions: &["dart"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Elixir,
        id: "elixir",
        name: "Elixir",
        graph_lang: None,
        temp_suffix: ".ex",
        aliases: &["elixir", "ex"],
        extensions: &["ex", "exs"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Go,
        id: "go",
        name: "Go",
        graph_lang: None,
        temp_suffix: ".go",
        aliases: &["go", "golang"],
        extensions: &["go"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Haskell,
        id: "haskell",
        name: "Haskell",
        graph_lang: None,
        temp_suffix: ".hs",
        aliases: &["haskell", "hs"],
        extensions: &["hs"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Hcl,
        id: "hcl",
        name: "HCL",
        graph_lang: None,
        temp_suffix: ".hcl",
        aliases: &["hcl", "terraform", "tf"],
        extensions: &["hcl", "tf", "tfvars"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Html,
        id: "html",
        name: "HTML",
        graph_lang: None,
        temp_suffix: ".html",
        aliases: &["html", "htm"],
        extensions: &["html", "htm"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Java,
        id: "java",
        name: "Java",
        graph_lang: None,
        temp_suffix: ".java",
        aliases: &["java"],
        extensions: &["java"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Json,
        id: "json",
        name: "JSON",
        graph_lang: None,
        temp_suffix: ".json",
        aliases: &["json"],
        extensions: &["json"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Lua,
        id: "lua",
        name: "Lua",
        graph_lang: None,
        temp_suffix: ".lua",
        aliases: &["lua"],
        extensions: &["lua"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Nix,
        id: "nix",
        name: "Nix",
        graph_lang: None,
        temp_suffix: ".nix",
        aliases: &["nix"],
        extensions: &["nix"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Php,
        id: "php",
        name: "PHP",
        graph_lang: None,
        temp_suffix: ".php",
        aliases: &["php"],
        extensions: &["php"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Ruby,
        id: "ruby",
        name: "Ruby",
        graph_lang: None,
        temp_suffix: ".rb",
        aliases: &["ruby", "rb"],
        extensions: &["rb"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Scala,
        id: "scala",
        name: "Scala",
        graph_lang: None,
        temp_suffix: ".scala",
        aliases: &["scala"],
        extensions: &["scala", "sc"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Solidity,
        id: "solidity",
        name: "Solidity",
        graph_lang: None,
        temp_suffix: ".sol",
        aliases: &["solidity", "sol"],
        extensions: &["sol"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Swift,
        id: "swift",
        name: "Swift",
        graph_lang: None,
        temp_suffix: ".swift",
        aliases: &["swift"],
        extensions: &["swift"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
    SemanticEditLanguageContract {
        executor: SemanticEditExecutorLanguage::Yaml,
        id: "yaml",
        name: "YAML",
        graph_lang: None,
        temp_suffix: ".yaml",
        aliases: &["yaml", "yml"],
        extensions: &["yaml", "yml"],
        recognized_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_STRUCTURAL_KINDS,
        family: SemanticEditLanguageFamily::Structural,
        formatter: SemanticEditFormatterContract::None,
    },
];

/// One conformance fixture per registered semantic-edit executor.
///
/// A contract row only declares that a language *is* an executor. These rows
/// put every one of them through the real `structural_rewrite` path — match,
/// rewrite, and reparse the result with that executor's own grammar — so a
/// registration that cannot actually mutate its language fails here instead of
/// in the field. Grammar quirks are row data, not prose, so a grammar upgrade
/// that lifts a limit is noticed rather than leaving a stale note behind.
#[cfg(test)]
struct SemanticEditExecutorFixture {
    executor: SemanticEditExecutorLanguage,
    /// An alias that must resolve to `executor`.
    alias: &'static str,
    /// A path whose extension must resolve to `executor` on its own.
    sample_path: &'static str,
    source: &'static str,
    pattern: &'static str,
    replacement: &'static str,
    /// Must be non-zero: a fixture that expects no rewrite proves nothing.
    expected_replacements: usize,
    /// Text that must appear in the rewritten buffer.
    marker: &'static str,
}

#[cfg(test)]
const SEMANTIC_EDIT_EXECUTOR_FIXTURES: &[SemanticEditExecutorFixture] = &[
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Rust,
        alias: "rust",
        sample_path: "src/lib.rs",
        source: "fn main() {\n    foo(1);\n    foo(2);\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Python,
        alias: "python",
        sample_path: "app.py",
        source: "def main():\n    foo(1)\n    foo(2)\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::TypeScript,
        alias: "typescript",
        sample_path: "app.ts",
        source: "function main(): void {\n  foo(1);\n  foo(2);\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Tsx,
        alias: "tsx",
        sample_path: "App.tsx",
        source: "const App = () => {\n  foo(1);\n  foo(2);\n  return null;\n};\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::JavaScript,
        alias: "javascript",
        sample_path: "app.js",
        source: "function main() {\n  foo(1);\n  foo(2);\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Jsx,
        alias: "jsx",
        sample_path: "view.jsx",
        source: "function main() {\n  foo(1);\n  foo(2);\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Markdown,
        alias: "markdown",
        sample_path: "README.md",
        source: "# a\n\n# b\n",
        pattern: "# $A",
        replacement: "## $A",
        expected_replacements: 2,
        marker: "## a",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Kotlin,
        alias: "kotlin",
        sample_path: "Main.kt",
        source: "fun main() {\n    foo(1)\n    foo(2)\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Bash,
        alias: "bash",
        sample_path: "run.sh",
        source: "foo 1\nfoo 2\n",
        pattern: "foo $A",
        replacement: "bar $A",
        expected_replacements: 2,
        marker: "bar 1",
    },
    // tree-sitter-c reads a bare call as a declaration, so the pattern needs
        // the statement terminator.
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::C,
        alias: "c",
        sample_path: "main.c",
        source: "int main(void) {\n  foo(1);\n  foo(2);\n  return 0;\n}\n",
        pattern: "foo($A);",
        replacement: "bar($A);",
        expected_replacements: 2,
        marker: "bar(1);",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Cpp,
        alias: "cpp",
        sample_path: "main.cpp",
        source: "int main() {\n  foo(1);\n  foo(2);\n  return 0;\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::CSharp,
        alias: "csharp",
        sample_path: "Program.cs",
        source: "class C {\n  void M() {\n    Foo(1);\n    Foo(2);\n  }\n}\n",
        pattern: "Foo($A)",
        replacement: "Bar($A)",
        expected_replacements: 2,
        marker: "Bar(1)",
    },
    // CSS needs the declaration terminator for the same reason C does.
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Css,
        alias: "css",
        sample_path: "site.css",
        source: "body { color: red; }\n.x { color: blue; }\n",
        pattern: "color: $V;",
        replacement: "background: $V;",
        expected_replacements: 2,
        marker: "background: red;",
    },
    // tree-sitter-dart cannot parse an expression fragment as a standalone
        // pattern, so Dart selects at declaration granularity only.
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Dart,
        alias: "dart",
        sample_path: "main.dart",
        source: "void main() { print(\"a\"); }\n",
        pattern: "void main() { print($A); }",
        replacement: "void main() { log($A); }",
        expected_replacements: 1,
        marker: "log(\"a\")",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Elixir,
        alias: "elixir",
        sample_path: "worker.ex",
        source: "defmodule M do\n  def run do\n    foo(1)\n    foo(2)\n  end\nend\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Go,
        alias: "go",
        sample_path: "main.go",
        source: "package main\n\nfunc main() {\n\tfoo(1)\n\tfoo(2)\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Haskell,
        alias: "haskell",
        sample_path: "Main.hs",
        source: "main = do\n  foo 1\n  foo 2\n",
        pattern: "foo $A",
        replacement: "bar $A",
        expected_replacements: 2,
        marker: "bar 1",
    },
    // HCL has no expression statements, so a call only matches as an attribute.
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Hcl,
        alias: "hcl",
        sample_path: "main.tf",
        source: "resource \"a\" \"b\" {\n  x = foo(1)\n  y = foo(2)\n}\n",
        pattern: "$K = foo($A)",
        replacement: "$K = bar($A)",
        expected_replacements: 2,
        marker: "x = bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Html,
        alias: "html",
        sample_path: "index.html",
        source: "<div><span>a</span><span>b</span></div>\n",
        pattern: "<span>$A</span>",
        replacement: "<em>$A</em>",
        expected_replacements: 2,
        marker: "<em>a</em>",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Java,
        alias: "java",
        sample_path: "C.java",
        source: "class C {\n  void m() {\n    foo(1);\n    foo(2);\n  }\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    // A literal key with a metavariable value parses to two nodes, so both
        // sides must be metavariable-shaped.
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Json,
        alias: "json",
        sample_path: "data.json",
        source: "{\"a\": 1, \"b\": 1}\n",
        pattern: "$K: $V",
        replacement: "$K: 2",
        expected_replacements: 2,
        marker: "\"a\": 2",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Lua,
        alias: "lua",
        sample_path: "init.lua",
        source: "foo(1)\nfoo(2)\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Nix,
        alias: "nix",
        sample_path: "default.nix",
        source: "{ a = foo 1; b = foo 2; }\n",
        pattern: "foo $A",
        replacement: "bar $A",
        expected_replacements: 2,
        marker: "bar 1",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Php,
        alias: "php",
        sample_path: "index.php",
        source: "<?php\nfoo(1);\nfoo(2);\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Ruby,
        alias: "ruby",
        sample_path: "app.rb",
        source: "foo(1)\nfoo(2)\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Scala,
        alias: "scala",
        sample_path: "M.scala",
        source: "object M {\n  def run(): Unit = {\n    foo(1)\n    foo(2)\n  }\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    // Same declaration-only granularity as Dart.
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Solidity,
        alias: "solidity",
        sample_path: "C.sol",
        source: "contract C {\n  function f() public { emit E(1); }\n}\n",
        pattern: "function f() public { $$$B }",
        replacement: "function g() public { $$$B }",
        expected_replacements: 1,
        marker: "function g() public",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Swift,
        alias: "swift",
        sample_path: "main.swift",
        source: "func main() {\n    foo(1)\n    foo(2)\n}\n",
        pattern: "foo($A)",
        replacement: "bar($A)",
        expected_replacements: 2,
        marker: "bar(1)",
    },
    SemanticEditExecutorFixture {
        executor: SemanticEditExecutorLanguage::Yaml,
        alias: "yaml",
        sample_path: "config.yaml",
        source: "a: 1\nb: 1\n",
        pattern: "$K: 1",
        replacement: "$K: 2",
        expected_replacements: 2,
        marker: "a: 2",
    },
];

/// One rename conformance fixture per executor that recognizes `rename_symbol`.
///
/// A structural fixture proves an executor can *match and rewrite*; it cannot
/// prove a rename lands on names rather than on prose and data. Every renamable
/// language declares the positions that must change and the positions that must
/// not, so a rename that rewrites a comment or a string literal fails here for
/// that language specifically, rather than being caught only for the three
/// languages someone happened to write a hand test for.
#[cfg(test)]
struct SemanticEditRenameFixture {
    executor: SemanticEditExecutorLanguage,
    alias: &'static str,
    source: &'static str,
    symbol: &'static str,
    /// The indexed symbol kind the planner would resolve, as a capture name
    /// from `Lang::symbol_query`. Rows carry it rather than defaulting to
    /// "unresolved" so the kind-directed narrowing is exercised, not bypassed.
    symbol_kind: &'static str,
    new_name: &'static str,
    /// Must be non-zero: a fixture that renames nothing proves nothing.
    expected_replacements: usize,
    /// Substrings that must appear *after* the rename.
    renamed: &'static [&'static str],
    /// Substrings that must survive the rename byte for byte — comments,
    /// string literals, and data that merely shares the name.
    untouched: &'static [&'static str],
}

#[cfg(test)]
const SEMANTIC_EDIT_RENAME_FIXTURES: &[SemanticEditRenameFixture] = &[
    SemanticEditRenameFixture {
        executor: SemanticEditExecutorLanguage::Rust,
        alias: "rust",
        // A struct field, a field read, an inherent method, and a method call
        // are all spelled `field_identifier` here. Only the callable positions
        // can be the function being renamed, and the method call must stay —
        // dropping it would leave a call site broken, which is the failure the
        // narrowing must not trade for.
        source: "/// doc widget_count\nstruct Meter { widget_count: usize }\nfn widget_count() -> usize { 3 }\nimpl Meter { fn widget_count(&self) -> usize { self.widget_count } }\nfn describe(m: &Meter) -> String {\n    // widget_count comment\n    let label = \"widget_count\";\n    let total = m.widget_count;\n    let called = m.widget_count();\n    let _ = (total, called);\n    format!(\"{label}: {}\", widget_count())\n}\n",
        symbol: "widget_count",
        symbol_kind: "function",
        new_name: "gadget_count",
        expected_replacements: 4,
        renamed: &[
            "fn gadget_count() -> usize",
            "fn gadget_count(&self)",
            "let called = m.gadget_count();",
            "gadget_count())",
        ],
        untouched: &[
            "/// doc widget_count",
            "// widget_count comment",
            "\"widget_count\"",
            "struct Meter { widget_count: usize }",
            "{ self.widget_count }",
            "let total = m.widget_count;",
        ],
    },
        SemanticEditRenameFixture {
            executor: SemanticEditExecutorLanguage::Python,
            alias: "python",
            source: "import mod\n\ndef widget_count():\n    # widget_count comment\n    return \"widget_count\"\n\nclass Panel:\n    def widget_count(self):\n        return 2\n\nread = panel.widget_count\ncalled = panel.widget_count()\ndirect = widget_count()\nmodule_read = mod.widget_count\n",
            symbol: "widget_count",
            symbol_kind: "function",
            new_name: "gadget_count",
            expected_replacements: 5,
            renamed: &[
                "def gadget_count()",
                "def gadget_count(self)",
                "called = panel.gadget_count()",
                "direct = gadget_count()",
                "module_read = mod.gadget_count",
            ],
            untouched: &[
                "# widget_count comment",
                "\"widget_count\"",
                "read = panel.widget_count",
            ],
    },
    SemanticEditRenameFixture {
        executor: SemanticEditExecutorLanguage::TypeScript,
        alias: "typescript",
        source: "// widgetCount comment\nfunction widgetCount(): number { return 1; }\nconst label = \"widgetCount\";\nconst keyed = { widgetCount: 1 };\nconst shorthand = { widgetCount };\nclass Panel { widgetCount(): number { return 2; } }\nconst total = keyed.widgetCount + widgetCount();\n",
        symbol: "widgetCount",
        symbol_kind: "function",
        new_name: "gadgetCount",
        expected_replacements: 3,
        renamed: &[
            "function gadgetCount()",
            "+ gadgetCount()",
            "{ widgetCount: gadgetCount }",
        ],
        untouched: &[
            "// widgetCount comment",
            "\"widgetCount\"",
            "{ widgetCount: 1 }",
            "class Panel { widgetCount()",
            "keyed.widgetCount",
        ],
    },
    SemanticEditRenameFixture {
        executor: SemanticEditExecutorLanguage::Tsx,
        alias: "tsx",
        source: "// widgetCount comment\nfunction widgetCount(): number { return 1; }\nconst label = \"widgetCount\";\nconst keyed = { widgetCount: 1 };\nconst App = () => widgetCount() + keyed.widgetCount;\n",
        symbol: "widgetCount",
        symbol_kind: "function",
        new_name: "gadgetCount",
        expected_replacements: 2,
        renamed: &["function gadgetCount()", "=> gadgetCount()"],
        untouched: &[
            "// widgetCount comment",
            "\"widgetCount\"",
            "{ widgetCount: 1 }",
            "keyed.widgetCount",
        ],
    },
    SemanticEditRenameFixture {
        executor: SemanticEditExecutorLanguage::JavaScript,
        alias: "javascript",
        source: "// widgetCount comment\nfunction widgetCount() { return 1; }\nconst label = \"widgetCount\";\nconst keyed = { widgetCount: 1 };\nconst total = keyed.widgetCount + widgetCount();\n",
        symbol: "widgetCount",
        symbol_kind: "function",
        new_name: "gadgetCount",
        expected_replacements: 2,
        renamed: &["function gadgetCount()", "+ gadgetCount()"],
        untouched: &[
            "// widgetCount comment",
            "\"widgetCount\"",
            "{ widgetCount: 1 }",
            "keyed.widgetCount",
        ],
    },
    SemanticEditRenameFixture {
        executor: SemanticEditExecutorLanguage::Jsx,
        alias: "jsx",
        source: "// widgetCount comment\nfunction widgetCount() { return 1; }\nconst label = \"widgetCount\";\nconst keyed = { widgetCount: 1 };\nconst total = keyed.widgetCount + widgetCount();\n",
        symbol: "widgetCount",
        symbol_kind: "function",
        new_name: "gadgetCount",
        expected_replacements: 2,
        renamed: &["function gadgetCount()", "+ gadgetCount()"],
        untouched: &[
            "// widgetCount comment",
            "\"widgetCount\"",
            "{ widgetCount: 1 }",
            "keyed.widgetCount",
        ],
    },
        SemanticEditRenameFixture {
            executor: SemanticEditExecutorLanguage::Kotlin,
            alias: "kotlin",
            source: "// widgetCount comment\nfun widgetCount(): Int { return 1 }\nval label = \"widgetCount\"\nclass Panel {\n    fun widgetCount(): Int { return 2 }\n}\nobject Registry {\n    fun widgetCount(): Int { return 3 }\n}\nval read = panel.widgetCount\nval called = panel.widgetCount()\nval qualified = Registry.widgetCount\nfun caller(): Int { return widgetCount() }\n",
            symbol: "widgetCount",
            symbol_kind: "function",
            new_name: "gadgetCount",
            expected_replacements: 6,
            renamed: &[
                "fun gadgetCount(): Int { return 1 }",
                "fun gadgetCount(): Int { return 2 }",
                "fun gadgetCount(): Int { return 3 }",
                "val called = panel.gadgetCount()",
                "val qualified = Registry.gadgetCount",
                "return gadgetCount()",
            ],
            untouched: &[
                "// widgetCount comment",
                "\"widgetCount\"",
                "val read = panel.widgetCount",
            ],
        },
    SemanticEditRenameFixture {
        executor: SemanticEditExecutorLanguage::Bash,
        alias: "bash",
        // `echo widget_count` is the case that makes bash different: an
        // unquoted argument is the same `word` node as a command name, and it
        // is data.
        source: "widget_count() {\n  echo widget_count\n  local label=\"widget_count\"\n  # widget_count comment\n}\nwidget_count\n",
        symbol: "widget_count",
        symbol_kind: "function",
        new_name: "gadget_count",
        expected_replacements: 2,
        renamed: &["gadget_count() {", "}\ngadget_count\n"],
        untouched: &[
            "echo widget_count\n",
            "label=\"widget_count\"",
            "# widget_count comment",
        ],
    },
    SemanticEditRenameFixture {
        executor: SemanticEditExecutorLanguage::Zig,
        alias: "zig",
        // Zig reaches another file only through `@import`, and a container type
        // is also a namespace, so both member forms have to survive; only a
        // member read off a value receiver is a struct field.
        source: "// widget_count comment\nconst m = @import(\"m.zig\");\npub fn widget_count() u32 {\n    const label = \"widget_count\";\n    _ = label;\n    return 3;\n}\nconst Panel = struct {\n    widget_count: u32 = 0,\n};\npub fn caller(p: Panel) u32 { return widget_count() + m.widget_count + p.widget_count; }\n",
        symbol: "widget_count",
        symbol_kind: "function",
        new_name: "gadget_count",
        expected_replacements: 3,
        renamed: &[
            "pub fn gadget_count()",
            "return gadget_count() +",
            "m.gadget_count +",
        ],
        untouched: &[
            "// widget_count comment",
            "\"widget_count\"",
            "    widget_count: u32 = 0,",
            "p.widget_count;",
        ],
    },
    SemanticEditRenameFixture {
        executor: SemanticEditExecutorLanguage::GdScript,
        alias: "gdscript",
        // `func widget_count` and `var widget_count` are both `name` nodes;
        // only the `func` can be the function being renamed.
        source: "# widget_count comment\nfunc widget_count():\n\tvar widget_count = \"widget_count\"\n\treturn 1\n\nfunc caller():\n\treturn widget_count()\n",
        symbol: "widget_count",
        symbol_kind: "function",
        new_name: "gadget_count",
        // The `func` declaration and the call. The shadowing local's
        // declaration is excluded; nothing reads it by name, so there is no
        // ambiguous reference and the rename does not have to refuse.
        expected_replacements: 2,
        renamed: &["func gadget_count():", "return gadget_count()"],
        untouched: &[
            "# widget_count comment",
            "\"widget_count\"",
            "var widget_count =",
        ],
    },
];

/// One extraction conformance fixture per executor that recognizes
/// `extract_function`.
///
/// A rename fixture proves an executor can find every occurrence of a name it
/// was *given*. An extraction is the opposite problem: nothing names the thing
/// being moved, and the signature is derived rather than supplied. So each row
/// asserts the emitted function and the call site it left behind
/// *individually* — a runner that compared whole buffers would report green on
/// a signature and a call that were both wrong in the same direction, which is
/// the failure this intent is organized against.
#[cfg(test)]
struct SemanticEditExtractionFixture {
    executor: SemanticEditExecutorLanguage,
    alias: &'static str,
    /// A path whose extension must resolve to `executor` on its own.
    sample_path: &'static str,
    source: &'static str,
    /// One-based inclusive, as the intent spells it.
    start_line: usize,
    end_line: usize,
    new_name: &'static str,
    /// The call the caller is left with, byte for byte including indentation
    /// and the language's statement terminator.
    call_site: &'static str,
    /// Substrings the emitted function must contain, each asserted on its own.
    emitted: &'static [&'static str],
    /// Text that must appear exactly once in the result: the range moved, it
    /// was not copied.
    hoisted_once: &'static str,
}

#[cfg(test)]
const SEMANTIC_EDIT_EXTRACTION_FIXTURES: &[SemanticEditExtractionFixture] = &[
    SemanticEditExtractionFixture {
        executor: SemanticEditExecutorLanguage::Python,
        alias: "python",
        sample_path: "app.py",
        source: "def outer(base, scale):\n    prefix = base * 2\n    acc = 0\n    for item in range(scale):\n        acc += item * prefix\n    return acc\n",
        start_line: 3,
        end_line: 5,
        new_name: "accumulate",
        call_site: "    acc = accumulate(prefix, scale)\n",
        emitted: &[
            "def accumulate(prefix, scale):",
            "\n    acc = 0",
            "\n        acc += item * prefix",
            "\n    return acc",
        ],
        hoisted_once: "for item in range(scale):",
    },
    SemanticEditExtractionFixture {
        executor: SemanticEditExecutorLanguage::TypeScript,
        alias: "typescript",
        // Both parameters carry an annotation, which is the only condition
        // under which TypeScript is extractable at all.
        sample_path: "tool.ts",
        source: "function outer(base: number, scale: number) {\n  let acc = 0;\n  acc = base * scale;\n  return acc;\n}\n",
        start_line: 3,
        end_line: 3,
        new_name: "combine",
        call_site: "  acc = combine(base, scale);\n",
        emitted: &[
            "function combine(base: number, scale: number) {",
            // `acc` was declared outside the range, so the new function has to
            // declare it: without this line the body assigns a name that is
            // not in scope.
            "\n  let acc;\n  acc = base * scale;",
            "\n  return acc;",
        ],
        hoisted_once: "base * scale",
    },
    SemanticEditExtractionFixture {
        executor: SemanticEditExecutorLanguage::Tsx,
        alias: "tsx",
        sample_path: "View.tsx",
        source: "function outer(rows: number[], scale: number) {\n  let total = 0;\n  total = rows.length * scale;\n  return total;\n}\n",
        start_line: 3,
        end_line: 3,
        new_name: "measure",
        call_site: "  total = measure(rows, scale);\n",
        emitted: &[
            "function measure(rows: number[], scale: number) {",
            "\n  let total;\n  total = rows.length * scale;",
            "\n  return total;",
        ],
        hoisted_once: "rows.length * scale",
    },
    SemanticEditExtractionFixture {
        executor: SemanticEditExecutorLanguage::JavaScript,
        alias: "javascript",
        sample_path: "app.js",
        // `acc` is declared inside the range, so the call site has to declare
        // it rather than assign it — the one place a JS extraction differs
        // from the Python one it shares a derivation with.
        source: "function outer(base, scale) {\n  const prefix = base * 2;\n  let acc = 0;\n  for (const item of range(scale)) {\n    acc += item * prefix;\n  }\n  return acc;\n}\n",
        start_line: 3,
        end_line: 6,
        new_name: "accumulate",
        call_site: "  let acc = accumulate(prefix, scale);\n",
        emitted: &[
            "function accumulate(prefix, scale) {",
            "\n  let acc = 0;",
            "\n    acc += item * prefix;",
            "\n  return acc;",
        ],
        hoisted_once: "for (const item of range(scale)) {",
    },
    SemanticEditExtractionFixture {
        executor: SemanticEditExecutorLanguage::Jsx,
        alias: "jsx",
        sample_path: "view.jsx",
        source: "function outer(items) {\n  let count = 0;\n  count = items.length;\n  return count;\n}\n",
        start_line: 3,
        end_line: 3,
        new_name: "size",
        call_site: "  count = size(items);\n",
        emitted: &[
            "function size(items) {",
            "\n  let count;\n  count = items.length;",
            "\n  return count;",
        ],
        hoisted_once: "items.length",
    },
    SemanticEditExtractionFixture {
        executor: SemanticEditExecutorLanguage::GdScript,
        alias: "gdscript",
        sample_path: "player.gd",
        // Tab-indented, and the emitted body has to match: an extraction that
        // hard-coded four spaces would still parse and would leave a file that
        // no longer agrees with itself.
        source: "func outer(base, scale):\n\tvar prefix = base * 2\n\tvar acc = 0\n\tfor item in range(scale):\n\t\tacc += item * prefix\n\treturn acc\n",
        start_line: 3,
        end_line: 5,
        new_name: "accumulate",
        call_site: "\tvar acc = accumulate(prefix, scale)\n",
        emitted: &[
            "func accumulate(prefix, scale):",
            "\n\tvar acc = 0",
            "\n\t\tacc += item * prefix",
            "\n\treturn acc",
        ],
        hoisted_once: "for item in range(scale):",
    },
];

fn semantic_edit_language_contract_for_extension(
    ext: &str,
) -> Option<&'static SemanticEditLanguageContract> {
    let normalized = ext.trim().to_ascii_lowercase();
    SEMANTIC_EDIT_LANGUAGE_CONTRACTS
        .iter()
        .find(|contract| contract.extensions.contains(&normalized.as_str()))
}

impl SemanticEditExecutorLanguage {
    fn contract(self) -> &'static SemanticEditLanguageContract {
        SEMANTIC_EDIT_LANGUAGE_CONTRACTS
            .iter()
            .find(|contract| contract.executor == self)
            .expect("semantic edit executor language must have a contract")
    }

    fn name(self) -> &'static str {
        self.contract().name
    }

    fn graph_lang(self) -> Option<graph::Lang> {
        self.contract().graph_lang
    }

    /// The grammar used to reparse this executor's input and output.
    ///
    /// Validating a rewritten buffer is a parser-level need, so an executor
    /// does not require a `tsift-graph` binding to have one. Indexed languages
    /// keep using their `graph::Lang` grammar — Markdown in particular parses
    /// through `tsift-md-ast`, not ast-grep's `tree-sitter-md` — and everything
    /// else reparses with the same ast-grep grammar its pattern matched
    /// against. An executor with neither is a registration bug, refused here by
    /// name rather than silently parsed with some other language's rules.
    fn reparse_language(self) -> Result<tree_sitter::Language> {
        if let Some(graph_lang) = self.graph_lang() {
            return Ok(graph_lang.tree_sitter_language());
        }
        let lang = self.ast_grep_lang().with_context(|| {
            format!(
                "no grammar is compiled for the {} executor; structural languages in this build: {}",
                self.name(),
                AstGrepLang::supported_names()
            )
        })?;
        Ok(lang.tree_sitter_language())
    }

    /// The ast-grep grammar backing structural patterns for this executor.
    ///
    /// Resolved through the contract `id` rather than a second parallel match
    /// so a newly registered executor language cannot silently claim structural
    /// support it has no grammar for. Returns `None` when this build compiled
    /// no ast-grep grammar for the language (for example a `lang-*` feature is
    /// off), which callers must turn into a refusal, never a skip.
    fn ast_grep_lang(self) -> Option<AstGrepLang> {
        AstGrepLang::from_name(self.contract().id)
    }

    fn temp_suffix(self) -> &'static str {
        self.contract().temp_suffix
    }

    fn recognized_intents(self) -> &'static [&'static str] {
        self.contract().recognized_intents
    }

    fn apply_supported_intents(self) -> &'static [&'static str] {
        self.contract().apply_supported_intents
    }

    fn formatter(self) -> SemanticEditFormatterContract {
        self.contract().formatter
    }

    /// The set of files whose symbols can genuinely reference this one's.
    ///
    /// `callers_of` and `symbol_info` match by *name*, not by resolved binding,
    /// so a rename has to be told which languages could actually be referring
    /// to the same symbol. The JS-like executors are one family because they do
    /// call each other; every language in the indexed and structural tiers is
    /// its own family, because a Bash `deploy` and a Zig `deploy` sharing a name
    /// is a coincidence, and renaming one must never rewrite the other.
    fn rename_family(self) -> &'static str {
        match self.contract().family {
            SemanticEditLanguageFamily::JsLike => "js-like",
            SemanticEditLanguageFamily::Rust => "rust",
            SemanticEditLanguageFamily::Python => "python",
            SemanticEditLanguageFamily::Markdown => "markdown",
            SemanticEditLanguageFamily::Indexed | SemanticEditLanguageFamily::Structural => {
                self.contract().id
            }
        }
    }

    fn is_script(self) -> bool {
        matches!(
            self.contract().family,
            SemanticEditLanguageFamily::Python | SemanticEditLanguageFamily::JsLike
        )
    }

    fn is_markdown(self) -> bool {
        self.contract().family == SemanticEditLanguageFamily::Markdown
    }

    fn is_indexed_generic(self) -> bool {
        self.contract().family == SemanticEditLanguageFamily::Indexed
    }

    fn is_python(self) -> bool {
        self.contract().family == SemanticEditLanguageFamily::Python
    }

    fn is_js_like(self) -> bool {
        self.contract().family == SemanticEditLanguageFamily::JsLike
    }
}

fn semantic_edit_executor_language(
    language: &str,
    file_abs: &Path,
) -> Option<SemanticEditExecutorLanguage> {
    let normalized = language.trim().to_ascii_lowercase();
    if let Some(contract) = SEMANTIC_EDIT_LANGUAGE_CONTRACTS
        .iter()
        .find(|contract| contract.aliases.contains(&normalized.as_str()))
    {
        return Some(contract.executor);
    }
    file_abs
        .extension()
        .and_then(|value| value.to_str())
        .and_then(semantic_edit_language_contract_for_extension)
        .map(|contract| contract.executor)
}

fn semantic_edit_kind_apply_supported(kind: &str, language: &str, file_abs: &Path) -> bool {
    let Some(executor) = semantic_edit_executor_language(language, file_abs) else {
        return false;
    };
    if !executor.recognized_intents().contains(&kind) {
        return false;
    }
    executor.apply_supported_intents().contains(&kind)
}

fn semantic_edit_executor_name(language: &str, file_abs: &Path) -> String {
    semantic_edit_executor_language(language, file_abs)
        .map(|executor| executor.name().to_string())
        .unwrap_or_else(|| language.to_string())
}

#[cfg(test)]
#[test]
fn semantic_edit_language_contracts_resolve_current_executor_surface() {
    let mut cases = vec![
        (
            "rust",
            "src/lib.rs",
            SemanticEditExecutorLanguage::Rust,
            "rust",
            SEMANTIC_EDIT_RUST_KINDS,
            SEMANTIC_EDIT_RUST_KINDS,
            SemanticEditFormatterContract::Rustfmt,
        ),
        (
            "python",
            "script.py",
            SemanticEditExecutorLanguage::Python,
            "python",
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SemanticEditFormatterContract::PythonAuto,
        ),
        (
            "typescript",
            "tool.ts",
            SemanticEditExecutorLanguage::TypeScript,
            "typescript",
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SemanticEditFormatterContract::Prettier,
        ),
        (
            "tsx",
            "view.tsx",
            SemanticEditExecutorLanguage::Tsx,
            "tsx",
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SemanticEditFormatterContract::Prettier,
        ),
        (
            "javascript",
            "app.js",
            SemanticEditExecutorLanguage::JavaScript,
            "javascript",
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SemanticEditFormatterContract::Prettier,
        ),
        (
            "jsx",
            "view.jsx",
            SemanticEditExecutorLanguage::Jsx,
            "jsx",
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SEMANTIC_EDIT_SCRIPT_KINDS,
            SemanticEditFormatterContract::Prettier,
        ),
        (
            "markdown",
            "README.md",
            SemanticEditExecutorLanguage::Markdown,
            "markdown",
            SEMANTIC_EDIT_MARKDOWN_KINDS,
            SEMANTIC_EDIT_MARKDOWN_APPLY_KINDS,
            SemanticEditFormatterContract::None,
        ),
        (
            "mdx",
            "docs/page.mdx",
            SemanticEditExecutorLanguage::Markdown,
            "markdown",
            SEMANTIC_EDIT_MARKDOWN_KINDS,
            SEMANTIC_EDIT_MARKDOWN_APPLY_KINDS,
            SemanticEditFormatterContract::None,
        ),
        // The indexed tier is listed by hand rather than driven from the
        // fixtures, because its halves differ in exactly the ways that matter:
        // zig and gdscript have no ast-grep grammar in this build and so must
        // *not* advertise `structural_rewrite`, and gdscript alone among them
        // has an extraction emitter. Two independent facts, two rows.
        (
            "kotlin",
            "src/Main.kt",
            SemanticEditExecutorLanguage::Kotlin,
            "kotlin",
            SEMANTIC_EDIT_INDEXED_KINDS,
            SEMANTIC_EDIT_INDEXED_KINDS,
            SemanticEditFormatterContract::None,
        ),
        (
            "bash",
            "scripts/deploy.sh",
            SemanticEditExecutorLanguage::Bash,
            "bash",
            SEMANTIC_EDIT_INDEXED_KINDS,
            SEMANTIC_EDIT_INDEXED_KINDS,
            SemanticEditFormatterContract::None,
        ),
        (
            "zig",
            "src/main.zig",
            SemanticEditExecutorLanguage::Zig,
            "zig",
            SEMANTIC_EDIT_INDEXED_RENAME_ONLY_KINDS,
            SEMANTIC_EDIT_INDEXED_RENAME_ONLY_KINDS,
            SemanticEditFormatterContract::None,
        ),
        (
            "gdscript",
            "player.gd",
            SemanticEditExecutorLanguage::GdScript,
            "gdscript",
            SEMANTIC_EDIT_INDEXED_EXTRACT_KINDS,
            SEMANTIC_EDIT_INDEXED_EXTRACT_KINDS,
            SemanticEditFormatterContract::None,
        ),
    ];

    // The structural-only tier is 20+ languages whose contracts differ only in
    // their names, so listing them again by hand would be a second table to
    // drift. They are driven from the conformance fixtures instead, which ties
    // "registered" and "actually exercised" to the same row.
    cases.extend(
        SEMANTIC_EDIT_EXECUTOR_FIXTURES
            .iter()
            .filter(|fixture| {
                fixture.executor.contract().family == SemanticEditLanguageFamily::Structural
            })
            .map(|fixture| {
                (
                    fixture.alias,
                    fixture.sample_path,
                    fixture.executor,
                    fixture.executor.contract().id,
                    SEMANTIC_EDIT_STRUCTURAL_KINDS,
                    SEMANTIC_EDIT_STRUCTURAL_KINDS,
                    SemanticEditFormatterContract::None,
                )
            }),
    );

    // Every registered contract must appear above: a new executor language
    // added without a case would otherwise be covered by nothing at all.
    for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
        assert!(
            cases.iter().any(|case| case.2 == contract.executor),
            "executor {} has no case in this contract test",
            contract.id
        );
    }

    for (language, file, executor, canonical, recognized, apply_supported, formatter) in cases {
        let path = Path::new(file);
        let contract = executor.contract();
        assert_eq!(contract.id, canonical);
        assert_eq!(contract.formatter, formatter);
        assert_eq!(executor.recognized_intents(), recognized);
        assert!(contract.aliases.contains(&language));
        assert_eq!(semantic_edit_language_for_file(path), canonical);
        assert_eq!(
            semantic_edit_executor_language(language, path),
            Some(executor)
        );
        for &ext in contract.extensions {
            assert_eq!(
                semantic_edit_language_contract_for_extension(ext)
                    .map(|contract| contract.executor),
                Some(executor)
            );
        }
        for &kind in SEMANTIC_EDIT_KINDS {
            assert_eq!(
                semantic_edit_kind_apply_supported(kind, language, path),
                apply_supported.contains(&kind),
                "{language} support mismatch for {kind}"
            );
        }
    }

    assert_eq!(
        semantic_edit_executor_language("unknown", Path::new("tool.ts")),
        Some(SemanticEditExecutorLanguage::TypeScript)
    );
    assert_eq!(
        semantic_edit_executor_language("markdown", Path::new("README.md")),
        Some(SemanticEditExecutorLanguage::Markdown)
    );
    assert!(!semantic_edit_kind_apply_supported(
        "rewrite_call_sites",
        "typescript",
        Path::new("tool.ts")
    ));
    assert!(semantic_edit_kind_apply_supported(
        "rename_heading",
        "markdown",
        Path::new("README.md")
    ));
    assert!(semantic_edit_kind_apply_supported(
        "insert_list_item",
        "markdown",
        Path::new("README.md")
    ));
    assert!(semantic_edit_kind_apply_supported(
        "rewrite_code_fence",
        "markdown",
        Path::new("README.md")
    ));
}

fn rust_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn validate_rust_identifier(name: &str, field: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("{field} must not be empty");
    };
    if !(first == '_' || first.is_ascii_alphabetic()) || !chars.all(rust_ident_char) {
        bail!("{field} {name:?} is not a supported Rust identifier");
    }
    Ok(())
}

fn replace_rust_identifier(
    content: &str,
    old: &str,
    new: &str,
    target: graph::RenameTarget,
) -> Result<(String, usize)> {
    validate_rust_identifier(old, "symbol")?;
    validate_rust_identifier(new, "new_name")?;
    if old == new {
        bail!("old and new identifiers are identical");
    }
    rename_identifier_occurrences(content, old, new, graph::Lang::Rust, "Rust", target)
}

/// Rename through the grammar rather than through the characters.
///
/// This used to be a `match_indices` scan with an identifier-boundary guard in
/// each family. A boundary guard cannot distinguish an identifier from the same
/// characters inside a string literal or a comment, so a rename rewrote both —
/// and rewriting a string literal changes what the program *does*, not what it
/// is called. `graph::identifier_occurrences` only yields identifier nodes, so
/// prose and data are excluded by construction.
fn rename_identifier_occurrences(
    content: &str,
    old: &str,
    new: &str,
    lang: graph::Lang,
    language_label: &str,
    target: graph::RenameTarget,
) -> Result<(String, usize)> {
    let occurrences = graph::identifier_occurrences_for(lang, content.as_bytes(), old, target)
        .with_context(|| format!("collecting {language_label} identifier occurrences"))?;
    if occurrences.is_empty() {
        bail!("identifier {old:?} was not found as a whole {language_label} identifier");
    }
    let (out, replacements) = graph::replace_occurrences(content, &occurrences, new);
    Ok((out, replacements))
}

fn line_indent_at(content: &str, idx: usize) -> String {
    let line_start = content[..idx].rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    content[line_start..]
        .chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .collect()
}

fn parse_semantic_edit_source(
    content: &str,
    executor: SemanticEditExecutorLanguage,
    context: &str,
) -> Result<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    let language = executor.reparse_language()?;
    parser.set_language(&language)?;
    let tree = parser
        .parse(content.as_bytes(), None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    if tree.root_node().has_error() {
        bail!(
            "{context} produced {} source with parse errors",
            executor.name()
        );
    }
    Ok(tree)
}

fn script_ident_char(ch: char, executor: SemanticEditExecutorLanguage) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric() || (executor.is_js_like() && ch == '$')
}

fn validate_script_identifier(
    name: &str,
    field: &str,
    executor: SemanticEditExecutorLanguage,
) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("{field} must not be empty");
    };
    let first_ok =
        first == '_' || first.is_ascii_alphabetic() || (executor.is_js_like() && first == '$');
    if !first_ok || !chars.all(|ch| script_ident_char(ch, executor)) {
        bail!(
            "{field} {name:?} is not a supported {} identifier",
            executor.name()
        );
    }
    Ok(())
}

fn replace_script_identifier(
    content: &str,
    old: &str,
    new: &str,
    executor: SemanticEditExecutorLanguage,
    target: graph::RenameTarget,
) -> Result<(String, usize)> {
    validate_script_identifier(old, "symbol", executor)?;
    validate_script_identifier(new, "new_name", executor)?;
    if old == new {
        bail!("old and new identifiers are identical");
    }
    parse_semantic_edit_source(content, executor, "rename_symbol input")?;
    let Some(lang) = executor.contract().graph_lang else {
        bail!(
            "rename_symbol needs an indexed grammar; the {} executor has none",
            executor.name()
        );
    };
    let (out, replacements) =
        rename_identifier_occurrences(content, old, new, lang, executor.name(), target)?;
    parse_semantic_edit_source(&out, executor, "rename_symbol")?;
    Ok((out, replacements))
}

/// Rename in an indexed language that has no per-kind executor of its own.
///
/// The rewriting is the same AST splice every other family now uses; what is
/// language-specific here is only the shape of a legal name. Bash is the one
/// that differs: a function name is a `word`, and words admit `-` and `.`,
/// which are ordinary in shell function names and illegal everywhere else.
fn replace_indexed_identifier(
    content: &str,
    old: &str,
    new: &str,
    executor: SemanticEditExecutorLanguage,
    target: graph::RenameTarget,
) -> Result<(String, usize)> {
    validate_indexed_identifier(old, "symbol", executor)?;
    validate_indexed_identifier(new, "new_name", executor)?;
    if old == new {
        bail!("old and new identifiers are identical");
    }
    parse_semantic_edit_source(content, executor, "rename_symbol input")?;
    let Some(lang) = executor.contract().graph_lang else {
        bail!(
            "rename_symbol needs an indexed grammar; the {} executor has none",
            executor.name()
        );
    };
    let (out, replacements) =
        rename_identifier_occurrences(content, old, new, lang, executor.name(), target)?;
    parse_semantic_edit_source(&out, executor, "rename_symbol")?;
    Ok((out, replacements))
}

fn validate_indexed_identifier(
    name: &str,
    field: &str,
    executor: SemanticEditExecutorLanguage,
) -> Result<()> {
    let shell = executor == SemanticEditExecutorLanguage::Bash;
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("{field} must not be empty");
    };
    // A leading `-` would let `--flag` be offered as a symbol; the first
    // character stays strict for every language.
    let rest_ok = |ch: char| {
        ch == '_' || ch.is_ascii_alphanumeric() || (shell && matches!(ch, '-' | '.'))
    };
    if !(first == '_' || first.is_ascii_alphabetic()) || !chars.all(rest_ok) {
        bail!(
            "{field} {name:?} is not a supported {} identifier",
            executor.name()
        );
    }
    Ok(())
}

fn normalize_script_import(
    replacement: &str,
    executor: SemanticEditExecutorLanguage,
) -> Result<String> {
    let trimmed = replacement.trim();
    if trimmed.is_empty() {
        bail!("insert_import requires a non-empty replacement");
    }
    if executor.is_python() {
        if trimmed.starts_with("import ") || trimmed.starts_with("from ") {
            return Ok(trimmed.to_string());
        }
        return Ok(format!("import {trimmed}"));
    }

    let mut import = if trimmed.starts_with("import ") || trimmed.starts_with("export ") {
        trimmed.to_string()
    } else {
        format!("import {trimmed}")
    };
    if !import.ends_with(';') {
        import.push(';');
    }
    Ok(import)
}

fn script_import_insert_offset(content: &str, executor: SemanticEditExecutorLanguage) -> usize {
    let mut offset = 0usize;
    let mut insert_at = 0usize;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        let is_prelude = if executor.is_python() {
            trimmed.is_empty()
                || trimmed.starts_with("#!")
                || trimmed.starts_with("# -*-")
                || trimmed.starts_with("import ")
                || trimmed.starts_with("from ")
        } else {
            trimmed.is_empty()
                || trimmed.starts_with("#!")
                || trimmed.starts_with("import ")
                || (trimmed.starts_with("export ") && trimmed.contains(" from "))
        };
        if is_prelude {
            insert_at = offset + line.len();
            offset += line.len();
            continue;
        }
        break;
    }
    insert_at
}

fn insert_script_import(
    content: &str,
    replacement: &str,
    executor: SemanticEditExecutorLanguage,
) -> Result<(String, usize)> {
    parse_semantic_edit_source(content, executor, "insert_import input")?;
    let import = normalize_script_import(replacement, executor)?;
    if content.lines().any(|line| line.trim() == import) {
        return Ok((content.to_string(), 0));
    }
    let insert_at = script_import_insert_offset(content, executor);
    let mut out = String::with_capacity(content.len() + import.len() + 1);
    out.push_str(&content[..insert_at]);
    out.push_str(&import);
    out.push('\n');
    out.push_str(&content[insert_at..]);
    parse_semantic_edit_source(&out, executor, "insert_import")?;
    Ok((out, 1))
}

fn script_function_body_query(executor: SemanticEditExecutorLanguage) -> &'static str {
    if executor.is_python() {
        r#"
        (function_definition name: (identifier) @decl.name body: (block) @decl.body) @decl.item
        "#
    } else {
        r#"
        (function_declaration name: (identifier) @decl.name body: (statement_block) @decl.body) @decl.item
        (lexical_declaration (variable_declarator name: (identifier) @decl.name value: (arrow_function body: (statement_block) @decl.body))) @decl.item
        (variable_declaration (variable_declarator name: (identifier) @decl.name value: (arrow_function body: (statement_block) @decl.body))) @decl.item
        "#
    }
}

fn find_script_function_body_range(
    content: &str,
    symbol: &str,
    executor: SemanticEditExecutorLanguage,
) -> Result<(usize, usize, String)> {
    validate_script_identifier(symbol, "symbol", executor)?;
    let source = content.as_bytes();
    let language = executor.reparse_language()?;
    let tree = parse_semantic_edit_source(content, executor, "replace_function_body input")?;
    let query = tree_sitter::Query::new(&language, script_function_body_query(executor))?;
    let capture_names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut name_node = None;
        let mut body_node = None;
        for capture in m.captures {
            match capture_names[capture.index as usize] {
                "decl.name" => name_node = Some(capture.node),
                "decl.body" => body_node = Some(capture.node),
                _ => {}
            }
        }
        let (Some(name_node), Some(body_node)) = (name_node, body_node) else {
            continue;
        };
        if name_node.utf8_text(source)? != symbol {
            continue;
        }
        if executor.is_python() {
            return Ok((
                body_node.start_byte(),
                body_node.end_byte(),
                line_indent_at(content, body_node.start_byte()),
            ));
        }
        let start = body_node.start_byte();
        let end = body_node.end_byte();
        if source.get(start).copied() != Some(b'{')
            || source.get(end.saturating_sub(1)).copied() != Some(b'}')
        {
            bail!(
                "{} function {symbol:?} does not have a supported statement block body",
                executor.name()
            );
        }
        return Ok((
            start + 1,
            end.saturating_sub(1),
            line_indent_at(content, start),
        ));
    }
    bail!(
        "could not find {} function {symbol:?} with a supported body",
        executor.name()
    )
}

fn script_body_replacement(
    replacement: &str,
    base_indent: &str,
    executor: SemanticEditExecutorLanguage,
) -> String {
    let trimmed = replacement.trim_matches('\n');
    if executor.is_python() {
        let replacement = if trimmed.trim().is_empty() {
            "pass"
        } else {
            trimmed
        };
        return replacement
            .lines()
            .map(|line| {
                if line.trim().is_empty() {
                    String::new()
                } else {
                    format!("{base_indent}{}", line.trim())
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    }

    let body_indent = format!("{base_indent}  ");
    if trimmed.trim().is_empty() {
        return format!("\n{base_indent}");
    }
    let mut body = String::new();
    body.push('\n');
    for line in trimmed.lines() {
        if line.trim().is_empty() {
            body.push('\n');
        } else {
            body.push_str(&body_indent);
            body.push_str(line.trim());
            body.push('\n');
        }
    }
    body.push_str(base_indent);
    body
}

fn replace_script_function_body(
    content: &str,
    symbol: &str,
    replacement: &str,
    executor: SemanticEditExecutorLanguage,
) -> Result<(String, usize)> {
    let (start, end, base_indent) = find_script_function_body_range(content, symbol, executor)?;
    let replacement = script_body_replacement(replacement, &base_indent, executor);
    let mut out = String::with_capacity(content.len() + replacement.len());
    out.push_str(&content[..start]);
    out.push_str(&replacement);
    out.push_str(&content[end..]);
    parse_semantic_edit_source(&out, executor, "replace_function_body")?;
    Ok((out, 1))
}

fn rust_function_body_replacement(replacement: &str, base_indent: &str) -> String {
    let body_indent = format!("{base_indent}    ");
    let trimmed = replacement.trim_matches('\n');
    if trimmed.trim().is_empty() {
        return format!("\n{base_indent}");
    }

    let mut body = String::new();
    body.push('\n');
    for line in trimmed.lines() {
        if line.trim().is_empty() {
            body.push('\n');
        } else {
            body.push_str(&body_indent);
            body.push_str(line.trim());
            body.push('\n');
        }
    }
    body.push_str(base_indent);
    body
}

fn rust_target_span_matches_node(
    target: &SemanticEditSymbolTarget,
    node: tree_sitter::Node,
) -> bool {
    target
        .span
        .as_ref()
        .is_none_or(|span| node.start_byte() == span.start_byte && node.end_byte() == span.end_byte)
}

fn find_rust_function_body_range(
    content: &str,
    target: &SemanticEditSymbolTarget,
) -> Result<(usize, usize, String)> {
    validate_rust_identifier(&target.name, "symbol")?;
    let source = content.as_bytes();
    let language = graph::Lang::Rust.tree_sitter_language();
    let tree = parse_semantic_edit_source(
        content,
        SemanticEditExecutorLanguage::Rust,
        "replace_function_body input",
    )?;
    let query = tree_sitter::Query::new(
        &language,
        r#"
        (function_item name: (identifier) @function.name body: (block) @function.body) @function.item
        "#,
    )?;
    let capture_names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    let mut candidates = Vec::new();
    while let Some(m) = matches.next() {
        let mut name_node = None;
        let mut body_node = None;
        let mut item_node = None;
        for capture in m.captures {
            match capture_names[capture.index as usize] {
                "function.name" => name_node = Some(capture.node),
                "function.body" => body_node = Some(capture.node),
                "function.item" => item_node = Some(capture.node),
                _ => {}
            }
        }
        let (Some(name_node), Some(body_node), Some(item_node)) = (name_node, body_node, item_node)
        else {
            continue;
        };
        if name_node.utf8_text(source)? != target.name {
            continue;
        }
        if source.get(body_node.start_byte()).copied() != Some(b'{')
            || source.get(body_node.end_byte().saturating_sub(1)).copied() != Some(b'}')
        {
            bail!(
                "Rust function {:?} does not have a supported block body",
                target.name
            );
        }
        candidates.push((
            body_node.start_byte() + 1,
            body_node.end_byte().saturating_sub(1),
            line_indent_at(content, body_node.start_byte()),
            rust_target_span_matches_node(target, item_node),
        ));
    }

    if target.span.is_some() {
        let mut exact_span_candidates = candidates
            .iter()
            .filter(|(_, _, _, span_matches)| *span_matches)
            .cloned()
            .collect::<Vec<_>>();
        match exact_span_candidates.len() {
            1 => {
                let (start, end, indent, _) = exact_span_candidates.remove(0);
                return Ok((start, end, indent));
            }
            count if count > 1 => {
                bail!(
                    "Rust function {:?} matched multiple resolved AST spans",
                    target.name
                )
            }
            _ => {}
        }
    }

    match candidates.len() {
        1 => {
            let (start, end, indent, _) = candidates.remove(0);
            Ok((start, end, indent))
        }
        0 if target.span.is_some() => bail!(
            "could not find Rust function {:?} at the resolved AST span",
            target.name
        ),
        0 => bail!("could not find Rust function {:?}", target.name),
        _ if target.span.is_some() => bail!(
            "Rust function {:?} resolved AST span is stale and the current file has ambiguous functions with that name",
            target.name
        ),
        _ => bail!(
            "Rust function {:?} is ambiguous without a concrete AST span; pass target_handle from search/source-read/symbol-read",
            target.name
        ),
    }
}

fn replace_rust_function_body(
    content: &str,
    target: &SemanticEditSymbolTarget,
    replacement: &str,
) -> Result<(String, usize)> {
    let (start, end, base_indent) = find_rust_function_body_range(content, target)?;
    let mut out = String::with_capacity(content.len() + replacement.len());
    out.push_str(&content[..start]);
    out.push_str(&rust_function_body_replacement(replacement, &base_indent));
    out.push_str(&content[end..]);
    parse_semantic_edit_source(
        &out,
        SemanticEditExecutorLanguage::Rust,
        "replace_function_body",
    )?;
    Ok((out, 1))
}

fn normalize_rust_import(replacement: &str) -> Result<String> {
    let trimmed = replacement.trim();
    if trimmed.is_empty() {
        bail!("insert_import requires a non-empty replacement");
    }
    let mut import = if trimmed.starts_with("use ")
        || trimmed.starts_with("pub use ")
        || trimmed.starts_with("extern crate ")
    {
        trimmed.to_string()
    } else {
        format!("use {trimmed}")
    };
    if !import.ends_with(';') {
        import.push(';');
    }
    Ok(import)
}

fn line_end_after_byte(content: &str, idx: usize) -> usize {
    content[idx..]
        .find('\n')
        .map(|relative| idx + relative + 1)
        .unwrap_or(content.len())
}

fn rust_import_insert_offset(content: &str) -> Result<usize> {
    let source = content.as_bytes();
    let tree = parse_semantic_edit_source(
        content,
        SemanticEditExecutorLanguage::Rust,
        "insert_import input",
    )?;
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut insert_at = 0usize;
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "shebang" | "inner_attribute_item" | "use_declaration" | "extern_crate_declaration" => {
                insert_at = line_end_after_byte(content, child.end_byte());
            }
            "line_comment" | "block_comment" => {
                let text = child.utf8_text(source)?.trim_start();
                if text.starts_with("//!") || text.starts_with("/*!") {
                    insert_at = line_end_after_byte(content, child.end_byte());
                    continue;
                }
                break;
            }
            _ => break,
        }
    }
    Ok(insert_at)
}

fn insert_rust_import(content: &str, replacement: &str) -> Result<(String, usize)> {
    let import = normalize_rust_import(replacement)?;
    if content.lines().any(|line| line.trim() == import) {
        return Ok((content.to_string(), 0));
    }
    let insert_at = rust_import_insert_offset(content)?;

    let mut out = String::with_capacity(content.len() + import.len() + 1);
    out.push_str(&content[..insert_at]);
    out.push_str(&import);
    out.push('\n');
    out.push_str(&content[insert_at..]);
    parse_semantic_edit_source(&out, SemanticEditExecutorLanguage::Rust, "insert_import")?;
    Ok((out, 1))
}

fn validate_rust_expression_replacement(replacement: &str, field: &str) -> Result<String> {
    let trimmed = replacement.trim();
    if trimmed.is_empty() {
        bail!("{field} requires a non-empty Rust expression replacement");
    }
    let probe = format!("fn __tsift_probe() {{ let _ = {trimmed}; }}");
    let mut parser = tree_sitter::Parser::new();
    let language = graph::Lang::Rust.tree_sitter_language();
    parser.set_language(&language)?;
    let tree = parser
        .parse(probe.as_bytes(), None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    if tree.root_node().has_error() {
        bail!("{field} {trimmed:?} is not a valid Rust expression replacement");
    }
    Ok(trimmed.to_string())
}

fn rust_signature_replacement_name(replacement: &str) -> Result<String> {
    let trimmed = replacement.trim();
    if trimmed.is_empty() {
        bail!("update_call_signature requires a non-empty Rust function signature replacement");
    }
    if trimmed.contains('{') || trimmed.contains('}') {
        bail!("update_call_signature replacement must be a function signature without a body");
    }
    let probe = format!("{trimmed} {{}}\n");
    let mut parser = tree_sitter::Parser::new();
    let language = graph::Lang::Rust.tree_sitter_language();
    parser.set_language(&language)?;
    let tree = parser
        .parse(probe.as_bytes(), None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    if tree.root_node().has_error() {
        bail!(
            "update_call_signature replacement {trimmed:?} is not a valid Rust function signature"
        );
    }
    let query = tree_sitter::Query::new(
        &language,
        "(function_item name: (identifier) @function.name)",
    )?;
    let mut cursor = tree_sitter::QueryCursor::new();
    let capture_names = query.capture_names();
    let mut matches = cursor.matches(&query, tree.root_node(), probe.as_bytes());
    while let Some(m) = matches.next() {
        for capture in m.captures {
            if capture_names[capture.index as usize] == "function.name" {
                return Ok(capture.node.utf8_text(probe.as_bytes())?.to_string());
            }
        }
    }
    bail!("update_call_signature replacement did not parse to a Rust function signature")
}

fn find_rust_function_signature_range(content: &str, name: &str) -> Result<(usize, usize)> {
    validate_rust_identifier(name, "symbol")?;
    let source = content.as_bytes();
    let mut parser = tree_sitter::Parser::new();
    let language = graph::Lang::Rust.tree_sitter_language();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    let query = tree_sitter::Query::new(
        &language,
        "(function_item name: (identifier) @function.name)",
    )?;
    let mut cursor = tree_sitter::QueryCursor::new();
    let capture_names = query.capture_names();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        for capture in m.captures {
            if capture_names[capture.index as usize] != "function.name" {
                continue;
            }
            if capture.node.utf8_text(source)? != name {
                continue;
            }
            let Some(function_node) = capture.node.parent() else {
                continue;
            };
            let Some(body) = function_node.child_by_field_name("body") else {
                bail!("Rust function {name:?} has no body node for signature replacement");
            };
            return Ok((function_node.start_byte(), body.start_byte()));
        }
    }
    bail!("could not find Rust function {name:?} for signature replacement")
}

fn update_rust_function_signature(
    content: &str,
    name: &str,
    replacement: &str,
) -> Result<(String, usize)> {
    let replacement = replacement.trim();
    let replacement_name = rust_signature_replacement_name(replacement)?;
    if replacement_name != name {
        bail!(
            "update_call_signature replacement targets function {replacement_name:?}, expected {name:?}"
        );
    }
    let (start, end) = find_rust_function_signature_range(content, name)?;
    let mut out = String::with_capacity(content.len() + replacement.len());
    out.push_str(&content[..start]);
    out.push_str(replacement);
    out.push_str(&content[end..]);
    Ok((out, 1))
}

fn rust_call_expression_ranges(
    content: &str,
    symbol: &str,
    indexed_lines: &[usize],
) -> Result<Vec<(usize, usize)>> {
    validate_rust_identifier(symbol, "symbol")?;
    if indexed_lines.is_empty() {
        return Ok(Vec::new());
    }
    let source = content.as_bytes();
    let mut parser = tree_sitter::Parser::new();
    let language = graph::Lang::Rust.tree_sitter_language();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    if tree.root_node().has_error() {
        bail!("Rust source has parse errors; refusing call-site rewrite");
    }
    let query = tree_sitter::Query::new(
        &language,
        r#"
        (call_expression function: (identifier) @call.name) @call.expr
        (call_expression function: (scoped_identifier name: (identifier) @call.name)) @call.expr
        (call_expression function: (field_expression field: (field_identifier) @call.name)) @call.expr
        "#,
    )?;
    let capture_names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut candidates = Vec::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut name_node = None;
        let mut expr_node = None;
        for capture in m.captures {
            match capture_names[capture.index as usize] {
                "call.name" => name_node = Some(capture.node),
                "call.expr" => expr_node = Some(capture.node),
                _ => {}
            }
        }
        let (Some(name_node), Some(expr_node)) = (name_node, expr_node) else {
            continue;
        };
        if name_node.utf8_text(source)? != symbol {
            continue;
        }
        candidates.push((
            name_node.start_position().row.saturating_add(1),
            expr_node.start_byte(),
            expr_node.end_byte(),
        ));
    }

    let mut used = vec![false; candidates.len()];
    let mut ranges = Vec::with_capacity(indexed_lines.len());
    for line in indexed_lines {
        let Some((idx, (_, start, end))) = candidates
            .iter()
            .enumerate()
            .find(|(idx, (candidate_line, _, _))| !used[*idx] && candidate_line == line)
        else {
            bail!(
                "indexed call ref for {symbol:?} at line {line} did not match a Rust AST call expression"
            );
        };
        used[idx] = true;
        ranges.push((*start, *end));
    }
    Ok(ranges)
}

fn rewrite_rust_call_sites(
    content: &str,
    symbol: &str,
    indexed_lines: &[usize],
    replacement: &str,
) -> Result<(String, usize)> {
    if indexed_lines.is_empty() {
        bail!("no same-file indexed call refs found for Rust symbol {symbol:?}");
    }
    let replacement = validate_rust_expression_replacement(replacement, "rewrite_call_sites")?;
    let mut ranges = rust_call_expression_ranges(content, symbol, indexed_lines)?;
    ranges.sort_by_key(|(start, _)| *start);
    ranges.dedup();
    let mut out = content.to_string();
    for (start, end) in ranges.iter().rev() {
        out.replace_range(*start..*end, &replacement);
    }
    Ok((out, ranges.len()))
}

fn update_rust_call_signature(
    content: &str,
    symbol: &str,
    indexed_lines: &[usize],
    signature_replacement: &str,
    call_replacement: Option<&str>,
) -> Result<(String, usize)> {
    let (mut updated, mut replacements) =
        update_rust_function_signature(content, symbol, signature_replacement)?;
    if !indexed_lines.is_empty() {
        let call_replacement = call_replacement.with_context(|| {
            format!(
                "update_call_signature for {symbol:?} has indexed call refs and requires `call_replacement`"
            )
        })?;
        let (rewritten, call_replacements) =
            rewrite_rust_call_sites(&updated, symbol, indexed_lines, call_replacement)?;
        updated = rewritten;
        replacements += call_replacements;
    }
    Ok((updated, replacements))
}

fn validate_rust_source_fragment(content: &str, context: &str) -> Result<()> {
    let mut parser = tree_sitter::Parser::new();
    let language = graph::Lang::Rust.tree_sitter_language();
    parser.set_language(&language)?;
    let tree = parser
        .parse(content.as_bytes(), None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    if tree.root_node().has_error() {
        bail!("{context} produced Rust source with parse errors");
    }
    Ok(())
}

fn validate_rust_method_replacement(replacement: &str) -> Result<String> {
    let trimmed = replacement.trim();
    if trimmed.is_empty() {
        bail!("add_method requires a non-empty Rust method replacement");
    }
    let probe = format!("struct __TsiftProbe;\nimpl __TsiftProbe {{\n{trimmed}\n}}\n");
    validate_rust_source_fragment(&probe, "add_method")?;
    let source = probe.as_bytes();
    let mut parser = tree_sitter::Parser::new();
    let language = graph::Lang::Rust.tree_sitter_language();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    let query =
        tree_sitter::Query::new(&language, "(function_item name: (identifier) @method.name)")?;
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut method_count = 0usize;
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        method_count += m.captures.len();
    }
    match method_count {
        1 => Ok(trimmed.to_string()),
        0 => bail!("add_method replacement must contain one Rust method"),
        _ => bail!("add_method replacement must contain exactly one Rust method"),
    }
}

fn rust_indented_fragment(fragment: &str, indent: &str) -> String {
    fragment
        .trim_matches('\n')
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{}", line.trim())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn find_rust_inherent_impl_insert(
    content: &str,
    type_name: &str,
) -> Result<Option<(usize, String)>> {
    validate_rust_identifier(type_name, "symbol")?;
    let source = content.as_bytes();
    let mut parser = tree_sitter::Parser::new();
    let language = graph::Lang::Rust.tree_sitter_language();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    if tree.root_node().has_error() {
        bail!("Rust source has parse errors; refusing add_method");
    }
    let query = tree_sitter::Query::new(
        &language,
        r#"
        (impl_item type: (type_identifier) @impl.type) @impl.item
        (impl_item type: (generic_type type: (type_identifier) @impl.type)) @impl.item
        "#,
    )?;
    let capture_names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut type_node = None;
        let mut impl_node = None;
        for capture in m.captures {
            match capture_names[capture.index as usize] {
                "impl.type" => type_node = Some(capture.node),
                "impl.item" => impl_node = Some(capture.node),
                _ => {}
            }
        }
        let (Some(type_node), Some(impl_node)) = (type_node, impl_node) else {
            continue;
        };
        if type_node.utf8_text(source)? != type_name {
            continue;
        }
        if impl_node.child_by_field_name("trait").is_some() {
            continue;
        }
        let Some(body) = impl_node.child_by_field_name("body") else {
            continue;
        };
        let insert_at = body.end_byte().saturating_sub(1);
        if source.get(insert_at).copied() != Some(b'}') {
            bail!("could not find closing brace for Rust impl {type_name:?}");
        }
        return Ok(Some((
            insert_at,
            line_indent_at(content, impl_node.start_byte()),
        )));
    }
    Ok(None)
}

fn find_rust_type_insert_after(content: &str, type_name: &str) -> Result<(usize, String)> {
    validate_rust_identifier(type_name, "symbol")?;
    let source = content.as_bytes();
    let mut parser = tree_sitter::Parser::new();
    let language = graph::Lang::Rust.tree_sitter_language();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    if tree.root_node().has_error() {
        bail!("Rust source has parse errors; refusing add_method");
    }
    let query = tree_sitter::Query::new(
        &language,
        r#"
        (struct_item name: (type_identifier) @type.name) @type.item
        (enum_item name: (type_identifier) @type.name) @type.item
        "#,
    )?;
    let capture_names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut name_node = None;
        let mut item_node = None;
        for capture in m.captures {
            match capture_names[capture.index as usize] {
                "type.name" => name_node = Some(capture.node),
                "type.item" => item_node = Some(capture.node),
                _ => {}
            }
        }
        let (Some(name_node), Some(item_node)) = (name_node, item_node) else {
            continue;
        };
        if name_node.utf8_text(source)? == type_name {
            return Ok((
                item_node.end_byte(),
                line_indent_at(content, item_node.start_byte()),
            ));
        }
    }
    bail!("could not find Rust struct or enum {type_name:?} for add_method")
}

fn add_rust_method(content: &str, type_name: &str, replacement: &str) -> Result<(String, usize)> {
    let method = validate_rust_method_replacement(replacement)?;
    if let Some((insert_at, base_indent)) = find_rust_inherent_impl_insert(content, type_name)? {
        let method_indent = format!("{base_indent}    ");
        let insertion = format!(
            "\n{}\n{base_indent}",
            rust_indented_fragment(&method, &method_indent)
        );
        let mut out = String::with_capacity(content.len() + insertion.len());
        out.push_str(&content[..insert_at]);
        out.push_str(&insertion);
        out.push_str(&content[insert_at..]);
        return Ok((out, 1));
    }

    let (insert_at, base_indent) = find_rust_type_insert_after(content, type_name)?;
    let method_indent = format!("{base_indent}    ");
    let insertion = format!(
        "\n\n{base_indent}impl {type_name} {{\n{}\n{base_indent}}}",
        rust_indented_fragment(&method, &method_indent)
    );
    let mut out = String::with_capacity(content.len() + insertion.len());
    out.push_str(&content[..insert_at]);
    out.push_str(&insertion);
    out.push_str(&content[insert_at..]);
    Ok((out, 1))
}

fn rust_node_kind_matches_symbol_kind(node_kind: &str, symbol_kind: &str) -> bool {
    matches!(
        (node_kind, symbol_kind),
        ("function_item", "function")
            | ("struct_item", "struct")
            | ("enum_item", "enum")
            | ("trait_item", "trait")
            | ("impl_item", "impl")
            | ("mod_item", "mod")
            | ("type_item", "type_alias")
            | ("const_item", "const")
            | ("static_item", "static")
    )
}

fn rust_named_declaration_range(
    content: &str,
    symbol: &str,
    symbol_kind: &str,
) -> Result<(usize, usize, String)> {
    let source = content.as_bytes();
    let mut parser = tree_sitter::Parser::new();
    let language = graph::Lang::Rust.tree_sitter_language();
    parser.set_language(&language)?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("parse failed"))?;
    if tree.root_node().has_error() {
        bail!("Rust source has parse errors; refusing move_declaration");
    }
    let query = tree_sitter::Query::new(
        &language,
        r#"
        (function_item name: (identifier) @decl.name) @decl.item
        (struct_item name: (type_identifier) @decl.name) @decl.item
        (enum_item name: (type_identifier) @decl.name) @decl.item
        (trait_item name: (type_identifier) @decl.name) @decl.item
        (impl_item type: (type_identifier) @decl.name) @decl.item
        (impl_item type: (generic_type type: (type_identifier) @decl.name)) @decl.item
        (mod_item name: (identifier) @decl.name) @decl.item
        (type_item name: (type_identifier) @decl.name) @decl.item
        (const_item name: (identifier) @decl.name) @decl.item
        (static_item name: (identifier) @decl.name) @decl.item
        "#,
    )?;
    let capture_names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut name_node = None;
        let mut item_node = None;
        for capture in m.captures {
            match capture_names[capture.index as usize] {
                "decl.name" => name_node = Some(capture.node),
                "decl.item" => item_node = Some(capture.node),
                _ => {}
            }
        }
        let (Some(name_node), Some(item_node)) = (name_node, item_node) else {
            continue;
        };
        if name_node.utf8_text(source)? != symbol {
            continue;
        }
        if !rust_node_kind_matches_symbol_kind(item_node.kind(), symbol_kind) {
            continue;
        }
        return Ok((
            item_node.start_byte(),
            item_node.end_byte(),
            item_node.utf8_text(source)?.to_string(),
        ));
    }
    bail!("could not find Rust {symbol_kind} declaration {symbol:?}")
}

fn remove_rust_declaration_range(content: &str, start: usize, end: usize) -> String {
    let mut remove_start = start;
    let line_start = content[..start].rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    if content[line_start..start].trim().is_empty() {
        remove_start = line_start;
    }
    let mut remove_end = end;
    if content[remove_end..].starts_with("\n\n") {
        remove_end += 2;
    } else if content[remove_end..].starts_with('\n') {
        remove_end += 1;
    }

    let mut out = String::with_capacity(content.len().saturating_sub(remove_end - remove_start));
    out.push_str(&content[..remove_start]);
    out.push_str(&content[remove_end..]);
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out
}

fn rust_item_prelude_insert_offset(content: &str) -> usize {
    let mut offset = 0usize;
    let mut insert_at = 0usize;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("#!")
            || trimmed.starts_with("//!")
            || trimmed.starts_with("use ")
            || trimmed.starts_with("pub use ")
            || trimmed.starts_with("extern crate ")
            || trimmed.starts_with("mod ")
            || trimmed.starts_with("pub mod ")
        {
            insert_at = offset + line.len();
            offset += line.len();
            continue;
        }
        break;
    }
    insert_at
}

fn insert_rust_item_after_prelude(content: &str, item: &str) -> String {
    let item = item.trim();
    let insert_at = rust_item_prelude_insert_offset(content);
    let before = &content[..insert_at];
    let after = &content[insert_at..];
    let prefix = if before.is_empty() || before.ends_with("\n\n") {
        ""
    } else {
        "\n"
    };
    let suffix = if after.is_empty() || after.starts_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    format!("{before}{prefix}{item}{suffix}{after}")
}

fn rust_move_module_name(source_file: &Path, destination_file: &Path) -> Result<String> {
    if source_file.parent() != destination_file.parent() {
        bail!(
            "move_declaration currently supports existing destination files in the same directory"
        );
    }
    let module = destination_file
        .file_stem()
        .and_then(|value| value.to_str())
        .context("move_declaration destination file must have a UTF-8 file stem")?;
    validate_rust_identifier(module, "destination module name")?;
    Ok(module.to_string())
}

fn ensure_rust_mod_decl(content: &str, module: &str) -> Result<(String, usize)> {
    validate_rust_identifier(module, "destination module name")?;
    let private_decl = format!("mod {module};");
    let public_decl = format!("pub mod {module};");
    if content
        .lines()
        .any(|line| matches!(line.trim(), value if value == private_decl || value == public_decl))
    {
        return Ok((content.to_string(), 0));
    }
    Ok((insert_rust_item_after_prelude(content, &private_decl), 1))
}

fn ensure_rust_use_decl(content: &str, module: &str, symbol: &str) -> Result<(String, usize)> {
    validate_rust_identifier(module, "destination module name")?;
    validate_rust_identifier(symbol, "symbol")?;
    let import = format!("use {module}::{symbol};");
    if content.lines().any(|line| line.trim() == import) {
        return Ok((content.to_string(), 0));
    }
    Ok((insert_rust_item_after_prelude(content, &import), 1))
}

fn preview_rust_move_declaration(
    source_content: &str,
    destination_content: &str,
    source_file_abs: &Path,
    destination_file_abs: &Path,
    symbol: &str,
    symbol_kind: &str,
) -> Result<((String, usize), (String, usize))> {
    if source_file_abs == destination_file_abs {
        bail!("move_declaration destination must differ from source file");
    }
    let module = rust_move_module_name(source_file_abs, destination_file_abs)?;
    let (start, end, declaration) =
        rust_named_declaration_range(source_content, symbol, symbol_kind)?;
    let mut updated_source = remove_rust_declaration_range(source_content, start, end);
    let (with_mod, mod_count) = ensure_rust_mod_decl(&updated_source, &module)?;
    updated_source = with_mod;
    let (with_use, use_count) = ensure_rust_use_decl(&updated_source, &module, symbol)?;
    updated_source = with_use;
    let updated_destination = insert_rust_item_after_prelude(destination_content, &declaration);
    validate_rust_source_fragment(&updated_source, "move_declaration source")?;
    validate_rust_source_fragment(&updated_destination, "move_declaration destination")?;
    Ok((
        (updated_source, 1 + mod_count + use_count),
        (updated_destination, 1),
    ))
}

fn target_symbol_name<'a>(
    target_symbol: Option<&'a SemanticEditSymbolTarget>,
    kind: &str,
) -> Result<&'a str> {
    target_symbol
        .map(|symbol| symbol.name.as_str())
        .with_context(|| format!("semantic edit kind {kind:?} requires a resolved target symbol"))
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum MarkdownSectionPosition {
    Before,
    After,
}

#[derive(Clone)]
pub(crate) struct MarkdownSectionEditSpan {
    pub(crate) name: String,
    pub(crate) level: usize,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) body_start_byte: usize,
    pub(crate) body_end_byte: usize,
}

#[derive(Clone)]
pub(crate) struct MarkdownBlockEditSpan {
    pub(crate) name: String,
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) body_start_byte: usize,
    pub(crate) body_end_byte: usize,
}

fn markdown_section_position(intent: &SemanticEditIntent) -> Result<MarkdownSectionPosition> {
    match intent.position.as_deref().unwrap_or("after").trim() {
        "before" => Ok(MarkdownSectionPosition::Before),
        "after" => Ok(MarkdownSectionPosition::After),
        value => {
            bail!("Markdown section position {value:?} is unsupported; expected before or after")
        }
    }
}

fn markdown_line_start(content: &str, start: usize) -> usize {
    let start = start.min(content.len());
    content[..start].rfind('\n').map(|pos| pos + 1).unwrap_or(0)
}

fn markdown_line_end(content: &str, start: usize) -> usize {
    let start = start.min(content.len());
    content.as_bytes()[start..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| start + offset)
        .unwrap_or(content.len())
}

fn markdown_code_fence_body_span(
    content: &str,
    start_byte: usize,
    end_byte: usize,
) -> Result<(usize, usize)> {
    let start_byte = start_byte.min(content.len());
    let end_byte = end_byte.min(content.len());
    let opening_end = markdown_line_end(content, start_byte);
    let body_start = if opening_end < content.len() {
        opening_end + 1
    } else {
        opening_end
    };
    let mut cursor = end_byte;
    while cursor > body_start {
        let mut search_end = cursor;
        if search_end > 0 && content.as_bytes()[search_end - 1] == b'\n' {
            search_end -= 1;
        }
        if search_end <= body_start {
            break;
        }
        let line_start = markdown_line_start(content, search_end);
        let line_end = markdown_line_end(content, line_start);
        let line = content.get(line_start..line_end).unwrap_or("");
        if line_start > start_byte
            && matches!(line.trim_start(), marker if marker.starts_with("```") || marker.starts_with("~~~"))
        {
            return Ok((body_start.min(line_start), line_start));
        }
        cursor = line_start;
    }
    bail!("Markdown code fence target does not have a supported closing fence")
}

fn markdown_heading_line_level(line: &str) -> Option<usize> {
    let marker = line.trim_start();
    let level = marker.chars().take_while(|ch| *ch == '#').count();
    (1..=6).contains(&level).then_some(level)
}

pub(crate) fn markdown_section_spans(content: &str) -> Result<Vec<MarkdownSectionEditSpan>> {
    parse_semantic_edit_source(
        content,
        SemanticEditExecutorLanguage::Markdown,
        "markdown edit input",
    )?;
    let projection = markdown_ast_projection("semantic-edit", content.as_bytes())
        .context("extracting Markdown heading spans")?;
    let mut sections = projection
        .nodes
        .into_iter()
        .filter(|symbol| symbol.kind == "heading")
        .map(|symbol| {
            let line_end = markdown_line_end(content, symbol.start_byte);
            let heading_line = content
                .get(symbol.start_byte..line_end)
                .context("indexed Markdown heading span is not on a UTF-8 boundary")?;
            let level = markdown_heading_line_level(heading_line)
                .context("indexed Markdown heading did not have an ATX marker")?;
            Ok(MarkdownSectionEditSpan {
                name: symbol.name,
                level,
                start_byte: symbol.start_byte,
                end_byte: symbol.end_byte,
                body_start_byte: symbol.body_start_byte.unwrap_or(symbol.end_byte),
                body_end_byte: symbol.body_end_byte.unwrap_or(symbol.end_byte),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    sections.sort_by(|left, right| {
        left.start_byte
            .cmp(&right.start_byte)
            .then(left.level.cmp(&right.level))
            .then(left.name.cmp(&right.name))
    });
    Ok(sections)
}

pub(crate) fn markdown_block_spans(
    content: &str,
    kind: &str,
) -> Result<Vec<MarkdownBlockEditSpan>> {
    parse_semantic_edit_source(
        content,
        SemanticEditExecutorLanguage::Markdown,
        "markdown edit input",
    )?;
    let projection = markdown_ast_projection("semantic-edit", content.as_bytes())
        .context("extracting Markdown block spans")?;
    let mut blocks = projection
        .nodes
        .into_iter()
        .filter(|symbol| symbol.kind == kind)
        .map(|symbol| -> Result<MarkdownBlockEditSpan> {
            let (body_start_byte, body_end_byte) = if kind == "code_block" {
                markdown_code_fence_body_span(content, symbol.start_byte, symbol.end_byte)?
            } else {
                (
                    symbol.body_start_byte.unwrap_or(symbol.start_byte),
                    symbol.body_end_byte.unwrap_or(symbol.end_byte),
                )
            };
            Ok(MarkdownBlockEditSpan {
                name: symbol.name,
                start_byte: symbol.start_byte,
                end_byte: symbol.end_byte,
                body_start_byte,
                body_end_byte,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    blocks.sort_by(|left, right| {
        left.start_byte
            .cmp(&right.start_byte)
            .then(left.name.cmp(&right.name))
    });
    Ok(blocks)
}

fn markdown_unique_section(content: &str, name: &str) -> Result<MarkdownSectionEditSpan> {
    let matches = markdown_section_spans(content)?
        .into_iter()
        .filter(|section| section.name == name)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [section] => Ok(section.clone()),
        [] => bail!("Markdown heading {name:?} was not found"),
        _ => bail!("Markdown heading {name:?} is ambiguous; supply a unique heading"),
    }
}

fn markdown_unique_block(content: &str, kind: &str, name: &str) -> Result<MarkdownBlockEditSpan> {
    let matches = markdown_block_spans(content, kind)?
        .into_iter()
        .filter(|block| block.name == name)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [block] => Ok(block.clone()),
        [] => bail!("Markdown {kind} {name:?} was not found"),
        _ => bail!("Markdown {kind} {name:?} is ambiguous; supply a unique target"),
    }
}

fn markdown_target_heading_name<'a>(
    target_symbol: Option<&'a SemanticEditSymbolTarget>,
    kind: &str,
) -> Result<&'a str> {
    let target = target_symbol.with_context(|| {
        format!("semantic edit kind {kind:?} requires a target Markdown heading")
    })?;
    if target.language != "markdown" || target.kind != "heading" {
        bail!("semantic edit kind {kind:?} requires a Markdown heading target");
    }
    Ok(&target.name)
}

fn markdown_target_block_name<'a>(
    target_symbol: Option<&'a SemanticEditSymbolTarget>,
    kind: &str,
    expected_kind: &str,
) -> Result<&'a str> {
    let target = target_symbol
        .with_context(|| format!("semantic edit kind {kind:?} requires a target Markdown block"))?;
    if target.language != "markdown" || target.kind != expected_kind {
        bail!("semantic edit kind {kind:?} requires a Markdown {expected_kind} target");
    }
    Ok(&target.name)
}

fn markdown_normalize_heading_name(name: &str, field: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("{field} must not be empty");
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        bail!("{field} must be a single Markdown heading line");
    }
    Ok(trimmed.to_string())
}

fn markdown_normalize_block(replacement: &str, field: &str) -> Result<String> {
    let trimmed = replacement.trim_matches('\n');
    if trimmed.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    let mut block = trimmed.to_string();
    if !block.ends_with('\n') {
        block.push('\n');
    }
    Ok(block)
}

fn markdown_strip_list_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .or_else(|| {
            let digit_end = trimmed
                .find(|ch: char| !ch.is_ascii_digit())
                .unwrap_or(trimmed.len());
            let (digits, rest) = trimmed.split_at(digit_end);
            (!digits.is_empty())
                .then_some(rest)
                .and_then(|rest| rest.strip_prefix(". "))
        })
}

fn markdown_list_marker_for_span(
    content: &str,
    list_item: &MarkdownBlockEditSpan,
) -> Result<(String, String)> {
    let line_start = markdown_line_start(content, list_item.start_byte);
    let line_end = markdown_line_end(content, list_item.start_byte);
    let line = content
        .get(line_start..line_end)
        .context("Markdown list item span is not on a UTF-8 boundary")?;
    let indent = line
        .chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .collect::<String>();
    let marker_source = line[indent.len()..].trim_start();
    let marker = if marker_source.starts_with("- ") {
        "-"
    } else if marker_source.starts_with("* ") {
        "*"
    } else if marker_source.starts_with("+ ") {
        "+"
    } else {
        let digit_end = marker_source
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(marker_source.len());
        let (digits, rest) = marker_source.split_at(digit_end);
        if !digits.is_empty() && rest.starts_with(". ") {
            &marker_source[..digit_end + 1]
        } else {
            bail!("Markdown list item target did not have a supported list marker");
        }
    };
    Ok((indent, marker.to_string()))
}

fn markdown_normalize_list_item(
    content: &str,
    list_item: &MarkdownBlockEditSpan,
    replacement: &str,
) -> Result<String> {
    let trimmed = replacement.trim();
    if trimmed.is_empty() {
        bail!("insert_list_item replacement must not be empty");
    }
    if trimmed.lines().count() != 1 {
        bail!("insert_list_item replacement must be a single Markdown list item");
    }
    let item_text = markdown_strip_list_marker(trimmed)
        .unwrap_or(trimmed)
        .trim();
    if item_text.is_empty() {
        bail!("insert_list_item replacement must contain list item text");
    }
    let (indent, marker) = markdown_list_marker_for_span(content, list_item)?;
    Ok(format!("{indent}{marker} {item_text}\n"))
}

fn markdown_normalize_section_block(replacement: &str) -> Result<String> {
    let block = markdown_normalize_block(replacement, "insert_section replacement")?;
    let first = block
        .lines()
        .find(|line| !line.trim().is_empty())
        .context("insert_section replacement must contain a Markdown heading")?;
    if markdown_heading_line_level(first).is_none() {
        bail!("insert_section replacement must start with an ATX heading");
    }
    parse_semantic_edit_source(
        &block,
        SemanticEditExecutorLanguage::Markdown,
        "insert_section replacement",
    )?;
    Ok(block)
}

fn markdown_join_section_parts<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for part in parts {
        let trimmed = part.trim_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(trimmed);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn markdown_insert_section_at(content: &str, offset: usize, section: &str) -> String {
    let offset = offset.min(content.len());
    markdown_join_section_parts([&content[..offset], section, &content[offset..]])
}

fn markdown_insert_list_line_at(content: &str, offset: usize, line: &str) -> String {
    let offset = offset.min(content.len());
    let mut out = String::with_capacity(content.len() + line.len() + 1);
    out.push_str(&content[..offset]);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(line.trim_end_matches('\n'));
    out.push('\n');
    out.push_str(&content[offset..]);
    out
}

fn markdown_trim_trailing_blank_lines_before(content: &str, offset: usize) -> usize {
    let mut offset = offset.min(content.len());
    while offset > 0 {
        let mut search_end = offset;
        if search_end > 0 && content.as_bytes()[search_end - 1] == b'\n' {
            search_end -= 1;
        }
        let line_start = markdown_line_start(content, search_end);
        let line_end = markdown_line_end(content, line_start);
        let line = content.get(line_start..line_end).unwrap_or("");
        if !line.trim().is_empty() {
            break;
        }
        offset = line_start;
    }
    offset
}

fn markdown_replace_heading(
    content: &str,
    section: &MarkdownSectionEditSpan,
    new_name: &str,
) -> Result<String> {
    let new_name = markdown_normalize_heading_name(new_name, "new_name")?;
    let line_end = markdown_line_end(content, section.start_byte);
    let heading_line = content
        .get(section.start_byte..line_end)
        .context("Markdown heading span is not on a UTF-8 boundary")?;
    let marker_start = heading_line
        .find('#')
        .context("Markdown heading line did not contain an ATX marker")?;
    let prefix = &heading_line[..marker_start];
    let replacement = format!("{prefix}{} {new_name}", "#".repeat(section.level));
    let mut out = String::with_capacity(content.len() + replacement.len());
    out.push_str(&content[..section.start_byte]);
    out.push_str(&replacement);
    out.push_str(&content[line_end..]);
    parse_semantic_edit_source(
        &out,
        SemanticEditExecutorLanguage::Markdown,
        "rename_heading",
    )?;
    Ok(out)
}

fn markdown_replace_section_body(
    content: &str,
    section: &MarkdownSectionEditSpan,
    replacement: &str,
) -> Result<String> {
    let body = markdown_normalize_block(replacement, "replace_section_body replacement")?;
    let out = markdown_join_section_parts([
        &content[..section.body_start_byte],
        &body,
        &content[section.body_end_byte..],
    ]);
    parse_semantic_edit_source(
        &out,
        SemanticEditExecutorLanguage::Markdown,
        "replace_section_body",
    )?;
    Ok(out)
}

fn markdown_insert_section(
    content: &str,
    target_symbol: Option<&SemanticEditSymbolTarget>,
    intent: &SemanticEditIntent,
) -> Result<String> {
    let block = markdown_normalize_section_block(
        intent
            .replacement
            .as_deref()
            .context("insert_section requires replacement")?,
    )?;
    let offset = if let Some(target_symbol) = target_symbol {
        let target_name = markdown_target_heading_name(Some(target_symbol), "insert_section")?;
        let target = markdown_unique_section(content, target_name)?;
        match markdown_section_position(intent)? {
            MarkdownSectionPosition::Before => target.start_byte,
            MarkdownSectionPosition::After => target.end_byte,
        }
    } else {
        content.len()
    };
    let out = markdown_insert_section_at(content, offset, &block);
    parse_semantic_edit_source(
        &out,
        SemanticEditExecutorLanguage::Markdown,
        "insert_section",
    )?;
    Ok(out)
}

fn markdown_insert_list_item(
    content: &str,
    target_symbol: Option<&SemanticEditSymbolTarget>,
    intent: &SemanticEditIntent,
) -> Result<String> {
    let target_name = markdown_target_block_name(target_symbol, "insert_list_item", "list_item")?;
    let target = markdown_unique_block(content, "list_item", target_name)?;
    let item = markdown_normalize_list_item(
        content,
        &target,
        intent
            .replacement
            .as_deref()
            .context("insert_list_item requires replacement")?,
    )?;
    let offset = match markdown_section_position(intent)? {
        MarkdownSectionPosition::Before => markdown_line_start(content, target.start_byte),
        MarkdownSectionPosition::After => {
            markdown_trim_trailing_blank_lines_before(content, target.end_byte)
        }
    };
    let out = markdown_insert_list_line_at(content, offset, &item);
    parse_semantic_edit_source(
        &out,
        SemanticEditExecutorLanguage::Markdown,
        "insert_list_item",
    )?;
    Ok(out)
}

fn markdown_move_section(
    content: &str,
    target_symbol: Option<&SemanticEditSymbolTarget>,
    intent: &SemanticEditIntent,
) -> Result<String> {
    let target_name = markdown_target_heading_name(target_symbol, "move_section")?;
    let destination_name = intent
        .destination_symbol
        .as_deref()
        .context("move_section requires destination_symbol")?;
    let target = markdown_unique_section(content, target_name)?;
    let destination = markdown_unique_section(content, destination_name)?;
    if target.start_byte == destination.start_byte {
        bail!("move_section destination must differ from the target section");
    }
    if destination.start_byte >= target.start_byte && destination.start_byte < target.end_byte {
        bail!("move_section destination cannot be inside the target section");
    }

    let moved = content
        .get(target.start_byte..target.end_byte)
        .context("Markdown section span is not on a UTF-8 boundary")?
        .to_string();
    let insert_at = match markdown_section_position(intent)? {
        MarkdownSectionPosition::Before => destination.start_byte,
        MarkdownSectionPosition::After => destination.end_byte,
    };
    let mut without = String::with_capacity(content.len() - (target.end_byte - target.start_byte));
    without.push_str(&content[..target.start_byte]);
    without.push_str(&content[target.end_byte..]);
    let adjusted_insert_at = if insert_at > target.end_byte {
        insert_at - (target.end_byte - target.start_byte)
    } else {
        insert_at
    };
    let out = markdown_insert_section_at(&without, adjusted_insert_at, &moved);
    parse_semantic_edit_source(&out, SemanticEditExecutorLanguage::Markdown, "move_section")?;
    Ok(out)
}

fn markdown_normalize_code_fence_body(replacement: &str) -> Result<String> {
    let trimmed = replacement.trim_matches('\n');
    if trimmed.trim().is_empty() {
        bail!("rewrite_code_fence replacement must not be empty");
    }
    if trimmed
        .lines()
        .any(|line| matches!(line.trim_start(), marker if marker.starts_with("```") || marker.starts_with("~~~")))
    {
        bail!("rewrite_code_fence replacement must not include fence markers");
    }
    let mut body = trimmed.to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    Ok(body)
}

fn markdown_rewrite_code_fence(
    content: &str,
    target_symbol: Option<&SemanticEditSymbolTarget>,
    intent: &SemanticEditIntent,
) -> Result<String> {
    let target_name =
        markdown_target_block_name(target_symbol, "rewrite_code_fence", "code_block")?;
    let target = markdown_unique_block(content, "code_block", target_name)?;
    let body = markdown_normalize_code_fence_body(
        intent
            .replacement
            .as_deref()
            .context("rewrite_code_fence requires replacement")?,
    )?;
    let mut out = String::with_capacity(content.len() + body.len());
    out.push_str(&content[..target.body_start_byte]);
    out.push_str(&body);
    out.push_str(&content[target.body_end_byte..]);
    parse_semantic_edit_source(
        &out,
        SemanticEditExecutorLanguage::Markdown,
        "rewrite_code_fence",
    )?;
    Ok(out)
}

fn preview_markdown_edit_content(
    content: &str,
    kind: &str,
    intent: &SemanticEditIntent,
    target_symbol: Option<&SemanticEditSymbolTarget>,
) -> Result<(String, usize)> {
    match kind {
        "rename_heading" => {
            let target_name = markdown_target_heading_name(target_symbol, kind)?;
            let target = markdown_unique_section(content, target_name)?;
            Ok((
                markdown_replace_heading(
                    content,
                    &target,
                    intent
                        .new_name
                        .as_deref()
                        .context("rename_heading requires new_name")?,
                )?,
                1,
            ))
        }
        "replace_section_body" => {
            let target_name = markdown_target_heading_name(target_symbol, kind)?;
            let target = markdown_unique_section(content, target_name)?;
            Ok((
                markdown_replace_section_body(
                    content,
                    &target,
                    intent
                        .replacement
                        .as_deref()
                        .context("replace_section_body requires replacement")?,
                )?,
                1,
            ))
        }
        "insert_section" => Ok((markdown_insert_section(content, target_symbol, intent)?, 1)),
        "move_section" => Ok((markdown_move_section(content, target_symbol, intent)?, 1)),
        "insert_list_item" => Ok((
            markdown_insert_list_item(content, target_symbol, intent)?,
            1,
        )),
        "rewrite_code_fence" => Ok((
            markdown_rewrite_code_fence(content, target_symbol, intent)?,
            1,
        )),
        _ => bail!("semantic edit kind {kind:?} is not supported by the Markdown executor yet"),
    }
}

/// Plan an extraction: hoist a run of statements into a new function and leave
/// a call in their place.
///
/// The signature is *derived*, not supplied, so the two edits this produces have
/// to agree with each other — `tsift_graph::plan_extraction` computes both from
/// one analysis, and a refusal from it is surfaced verbatim rather than being
/// downgraded into a partial edit. The result is reparsed with the executor's
/// grammar like every other kind, so `--verify`, formatting, and rollback apply
/// unchanged.
///
/// The emitter is chosen from the plan's own language, not from this executor,
/// so a `def` can never be spelled into a `.gd` file by a mismatched pair.
fn preview_extract_function(
    content: &str,
    executor: SemanticEditExecutorLanguage,
    intent: &SemanticEditIntent,
) -> Result<(String, usize)> {
    let new_name = intent
        .new_name
        .as_deref()
        .context("extract_function requires new_name")?;
    let start_line = intent
        .start_line
        .context("extract_function requires start_line")?;
    let end_line = intent
        .end_line
        .context("extract_function requires end_line")?;
    let lang = executor.graph_lang().with_context(|| {
        format!(
            "extract_function has no grammar compiled for {}",
            executor.name()
        )
    })?;
    parse_semantic_edit_source(content, executor, "extract_function input")?;

    let plan = graph::plan_extraction(
        lang,
        content.as_bytes(),
        start_line.saturating_sub(1),
        end_line.saturating_sub(1),
        new_name,
    )
    .map_err(|refusal| anyhow::anyhow!("extract_function refused: {}", refusal.message()))?;

    let (function, call) = graph::render_extraction(&plan, content, new_name);
    // Insert the new function first: splicing the call would move
    // `plan.insert_byte`, and the two edits are derived from one analysis of the
    // original bytes.
    let mut out = String::with_capacity(content.len() + function.len());
    out.push_str(&content[..plan.start_byte]);
    out.push_str(call.trim_start_matches(&plan.indent));
    out.push_str(&content[plan.end_byte..plan.insert_byte]);
    out.push_str(&function);
    out.push_str(&content[plan.insert_byte..]);
    // Two edits derived from one analysis still have to agree with the
    // *grammar*: a signature and a call that match each other can still be
    // spliced into a buffer that no longer parses.
    parse_semantic_edit_source(&out, executor, "extract_function")?;
    Ok((out, 1))
}

/// Plan a pattern-driven codemod for one file.
///
/// This is the only intent kind whose target is a *shape* rather than a
/// resolved symbol, so it selects through ast-grep instead of the index. It
/// still crosses the same executor boundary as every other kind: the input and
/// the rewritten buffer are both reparsed with the executor's tree-sitter
/// grammar, so `--verify`, formatting, patch proposals, and rollback apply
/// unchanged.
///
/// Both degenerate outcomes fail closed rather than planning an empty edit:
/// a pattern that matches nothing did not express what the caller meant, and a
/// template that reproduces its own match would report a no-op as completed
/// work.
fn preview_structural_rewrite(
    content: &str,
    executor: SemanticEditExecutorLanguage,
    intent: &SemanticEditIntent,
) -> Result<(String, usize)> {
    let pattern = intent
        .pattern
        .as_deref()
        .context("structural_rewrite requires pattern")?;
    let rewrite = intent
        .replacement
        .as_deref()
        .context("structural_rewrite requires replacement")?;
    let lang = executor.ast_grep_lang().with_context(|| {
        format!(
            "structural_rewrite has no ast-grep grammar compiled for {}; structural languages in this build: {}",
            executor.name(),
            AstGrepLang::supported_names()
        )
    })?;
    parse_semantic_edit_source(content, executor, "structural_rewrite input")?;

    let outcome = tsift_astgrep::rewrite_source(content, lang, pattern, rewrite)?;
    if outcome.replacements == 0 {
        bail!("structural pattern {pattern:?} matched nothing in this file");
    }
    if outcome.unchanged {
        bail!(
            "structural_rewrite is a no-op: rewrite {rewrite:?} reproduces every match of pattern {pattern:?}"
        );
    }
    parse_semantic_edit_source(&outcome.source, executor, "structural_rewrite")?;
    Ok((outcome.source, outcome.replacements))
}

/// First and last line that differ between two revisions of one file.
///
/// Line-based rather than byte-based because that is what a reviewer scrolls
/// to. Returns `None` when nothing changed.
fn changed_line_range(before: &str, after: &str, total_lines: usize) -> Option<SourceRangePreview> {
    if before == after {
        return None;
    }
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let common_prefix = before_lines
        .iter()
        .zip(after_lines.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = before_lines
        .len()
        .min(after_lines.len())
        .saturating_sub(common_prefix);
    let common_suffix = before_lines
        .iter()
        .rev()
        .zip(after_lines.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(max_suffix);
    let start = common_prefix + 1;
    let end = after_lines.len().saturating_sub(common_suffix).max(start);
    Some(SourceRangePreview {
        start,
        end,
        total_lines,
        truncated_before: false,
        truncated_after: false,
    })
}

fn preview_semantic_edit_content(
    content: &str,
    file_abs: &Path,
    language: &str,
    kind: &str,
    intent: &SemanticEditIntent,
    target_symbol: Option<&SemanticEditSymbolTarget>,
    call_ref_context: SemanticEditCallRefContext<'_>,
) -> Result<(String, usize)> {
    let Some(executor) = semantic_edit_executor_language(language, file_abs) else {
        bail!("no executor registered for language {language:?}");
    };
    // The family split below routes anything that is not markdown or script to
    // the Rust implementations. An executor whose contract does not recognize
    // this kind must be refused here rather than reaching another family's
    // rewriting by falling through.
    if !executor.recognized_intents().contains(&kind) {
        bail!(
            "semantic edit kind {kind:?} is not supported by the {} executor (supported: {})",
            executor.name(),
            executor.recognized_intents().join(", ")
        );
    }
    // Structural patterns are language-agnostic by construction, so they are
    // dispatched ahead of the per-family executor split rather than duplicated
    // into each arm.
    if kind == "structural_rewrite" {
        return preview_structural_rewrite(content, executor, intent);
    }
    // An extraction has no symbol to resolve, so like `structural_rewrite` it is
    // dispatched before the family split rather than threaded through the
    // symbol-resolved arms.
    if kind == "extract_function" {
        return preview_extract_function(content, executor, intent);
    }
    if executor.is_markdown() {
        return preview_markdown_edit_content(content, kind, intent, target_symbol);
    }
    if executor.is_indexed_generic() {
        return match kind {
            "rename_symbol" => replace_indexed_identifier(
                content,
                target_symbol_name(target_symbol, kind)?,
                intent
                    .new_name
                    .as_deref()
                    .context("rename_symbol requires new_name")?,
                executor,
                semantic_edit_rename_target(target_symbol),
            ),
            _ => bail!(
                "semantic edit kind {kind:?} is not supported by the {} executor yet",
                executor.name()
            ),
        };
    }
    if executor.is_script() {
        return match kind {
            "rename_symbol" => replace_script_identifier(
                content,
                target_symbol_name(target_symbol, kind)?,
                intent
                    .new_name
                    .as_deref()
                    .context("rename_symbol requires new_name")?,
                executor,
                semantic_edit_rename_target(target_symbol),
            ),
            "replace_function_body" => replace_script_function_body(
                content,
                target_symbol_name(target_symbol, kind)?,
                intent
                    .replacement
                    .as_deref()
                    .context("replace_function_body requires replacement")?,
                executor,
            ),
            "insert_import" => insert_script_import(
                content,
                intent
                    .replacement
                    .as_deref()
                    .context("insert_import requires replacement")?,
                executor,
            ),
            _ => bail!(
                "semantic edit kind {kind:?} is not supported by the {} executor yet",
                executor.name()
            ),
        };
    }
    if call_ref_context.cross_file_total > 0
        && matches!(kind, "rewrite_call_sites" | "update_call_signature")
    {
        bail!(
            "{kind} found {} indexed call ref(s) outside the target file; cross-file Rust rewrites are not supported yet",
            call_ref_context.cross_file_total
        );
    }
    let indexed_lines = call_ref_context
        .refs
        .iter()
        .map(|call| call.line)
        .collect::<Vec<_>>();

    match kind {
        "rename_symbol" => replace_rust_identifier(
            content,
            target_symbol_name(target_symbol, kind)?,
            intent
                .new_name
                .as_deref()
                .context("rename_symbol requires new_name")?,
            semantic_edit_rename_target(target_symbol),
        ),
        "replace_function_body" => replace_rust_function_body(
            content,
            target_symbol.with_context(
                || "semantic edit kind \"replace_function_body\" requires a resolved target symbol",
            )?,
            intent
                .replacement
                .as_deref()
                .context("replace_function_body requires replacement")?,
        ),
        "insert_import" => insert_rust_import(
            content,
            intent
                .replacement
                .as_deref()
                .context("insert_import requires replacement")?,
        ),
        "add_method" => {
            let target = target_symbol.with_context(
                || "semantic edit kind \"add_method\" requires a resolved target symbol",
            )?;
            if !matches!(target.kind.as_str(), "struct" | "enum") {
                bail!("add_method currently supports Rust struct and enum targets");
            }
            add_rust_method(
                content,
                &target.name,
                intent
                    .replacement
                    .as_deref()
                    .context("add_method requires replacement")?,
            )
        }
        "rewrite_call_sites" => rewrite_rust_call_sites(
            content,
            target_symbol_name(target_symbol, kind)?,
            &indexed_lines,
            intent
                .replacement
                .as_deref()
                .context("rewrite_call_sites requires replacement")?,
        ),
        "update_call_signature" => update_rust_call_signature(
            content,
            target_symbol_name(target_symbol, kind)?,
            &indexed_lines,
            intent
                .replacement
                .as_deref()
                .context("update_call_signature requires replacement")?,
            intent.call_replacement.as_deref(),
        ),
        _ => bail!("semantic edit kind {kind:?} is not supported by the Rust executor yet"),
    }
}

fn semantic_edit_line_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (idx, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(idx + 1);
        }
    }
    offsets
}

fn semantic_edit_patch_range_for_lines(
    content: &str,
    start_line_index: usize,
    end_line_index: usize,
) -> SemanticEditPatchRange {
    let offsets = semantic_edit_line_offsets(content);
    let start_byte = offsets
        .get(start_line_index)
        .copied()
        .unwrap_or(content.len());
    let end_byte = offsets
        .get(end_line_index)
        .copied()
        .unwrap_or(content.len());
    let line_count = end_line_index.saturating_sub(start_line_index);
    SemanticEditPatchRange {
        start_byte,
        end_byte,
        start_line: start_line_index + 1,
        end_line: if line_count == 0 {
            start_line_index + 1
        } else {
            start_line_index + line_count
        },
        line_count,
    }
}

fn semantic_edit_diff_hunk(
    before: &str,
    after: &str,
    budget: ResponseBudget,
) -> Option<SemanticEditPatchHunk> {
    if before == after {
        return None;
    }

    let before_lines = before.lines().collect::<Vec<_>>();
    let after_lines = after.lines().collect::<Vec<_>>();
    let mut prefix = 0usize;
    while prefix < before_lines.len()
        && prefix < after_lines.len()
        && before_lines[prefix] == after_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < before_lines.len().saturating_sub(prefix)
        && suffix < after_lines.len().saturating_sub(prefix)
        && before_lines[before_lines.len() - 1 - suffix]
            == after_lines[after_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let before_changed_end = before_lines.len().saturating_sub(suffix);
    let after_changed_end = after_lines.len().saturating_sub(suffix);
    let before_start = prefix.saturating_sub(2);
    let after_start = before_start;
    let before_end = before_changed_end.min(prefix + 8);
    let after_end = after_changed_end.min(prefix + 8);
    let preview_truncated = before_end < before_changed_end || after_end < after_changed_end;
    let mut lines = vec![
        "--- before".to_string(),
        "+++ after".to_string(),
        format!(
            "@@ -{},{} +{},{} @@",
            before_start + 1,
            before_end.saturating_sub(before_start),
            after_start + 1,
            after_end.saturating_sub(after_start)
        ),
    ];
    for line in &before_lines[before_start..prefix] {
        lines.push(format!(" {line}"));
    }
    for line in &before_lines[prefix..before_end] {
        lines.push(format!("-{line}"));
    }
    for line in &after_lines[prefix..after_end] {
        lines.push(format!("+{line}"));
    }
    Some(SemanticEditPatchHunk {
        before: semantic_edit_patch_range_for_lines(before, prefix, before_changed_end),
        after: semantic_edit_patch_range_for_lines(after, prefix, after_changed_end),
        context_before: prefix.saturating_sub(before_start),
        context_after: before_end.saturating_sub(before_changed_end.min(before_end)),
        preview_truncated,
        diff: truncate_for_budget(&lines.join("\n"), budget.preview_bytes()),
    })
}

pub(crate) fn semantic_edit_diff_preview(
    before: &str,
    after: &str,
    budget: ResponseBudget,
) -> Option<String> {
    semantic_edit_diff_hunk(before, after, budget).map(|hunk| hunk.diff)
}

struct SemanticEditPatchFileInput<'a> {
    file_abs: &'a Path,
    language: &'a str,
    before: &'a str,
    after: &'a str,
}

fn semantic_edit_patch_proposal(
    root: &Path,
    kind: &str,
    files: &[SemanticEditPatchFileInput<'_>],
    budget: ResponseBudget,
) -> Result<Option<SemanticEditPatchProposal>> {
    let mut proposal_files = Vec::new();
    let mut validator_names = Vec::new();
    for file in files {
        if file.before == file.after {
            continue;
        }
        let executor =
            semantic_edit_executor_language(file.language, file.file_abs).with_context(|| {
                format!(
                    "no parser registered for language {:?} while building patch proposal",
                    file.language
                )
            })?;
        parse_semantic_edit_source(file.before, executor, "patch proposal input")?;
        parse_semantic_edit_source(file.after, executor, "patch proposal output")?;
        validator_names.push(executor.name());
        let Some(hunk) = semantic_edit_diff_hunk(file.before, file.after, budget) else {
            continue;
        };
        proposal_files.push(SemanticEditPatchFileProposal {
            file: semantic_edit_file_display(root, file.file_abs),
            language: file.language.to_string(),
            before_hash: semantic_edit_content_hash(file.before.as_bytes()),
            after_hash: semantic_edit_content_hash(file.after.as_bytes()),
            hunks: vec![hunk],
        });
    }

    if proposal_files.is_empty() {
        return Ok(None);
    }
    validator_names.sort_unstable();
    validator_names.dedup();
    Ok(Some(SemanticEditPatchProposal {
        schema_version: 1,
        strategy: "ast_cst_minimal_textual_patch".to_string(),
        status: "ready".to_string(),
        parser_state: SemanticEditPatchParserState {
            input: "valid".to_string(),
            output: "valid".to_string(),
            validator: validator_names.join(", "),
        },
        trivia: SemanticEditPatchTriviaPolicy {
            mode: "preserve_unchanged_bytes".to_string(),
            preserves_comments: true,
            preserves_formatting: true,
            preserves_trivia: true,
            message:
                "unchanged bytes outside proposed hunks are copied verbatim; inserted text is bounded to executor-selected ranges"
                    .to_string(),
        },
        files: proposal_files,
        message: format!(
            "validated {kind} patch proposal against parser input/output and bounded diff hunks"
        ),
    }))
}

fn command_available(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn semantic_formatter_staging_file(
    file_abs: &Path,
    executor: SemanticEditExecutorLanguage,
    content: &str,
) -> Result<NamedTempFile> {
    let parent = file_abs.parent().unwrap_or_else(|| Path::new("."));
    let mut staged = TempFileBuilder::new()
        .prefix(".tsift-semantic-format-")
        .suffix(executor.temp_suffix())
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "creating formatter staging file near {}",
                file_abs.display()
            )
        })?;
    staged
        .write_all(content.as_bytes())
        .with_context(|| format!("writing formatter staging file for {}", file_abs.display()))?;
    staged
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("flushing formatter staging file for {}", file_abs.display()))?;
    Ok(staged)
}

fn run_semantic_formatter(
    staged: &NamedTempFile,
    file_abs: &Path,
    command: &str,
    args: &[&str],
    label: &str,
) -> Result<String> {
    let output = Command::new(command)
        .args(args)
        .arg(staged.path())
        .output()
        .with_context(|| format!("running {label} for semantic edit intent"))?;
    if !output.status.success() {
        let rejected_label = if label.starts_with("rustfmt ") {
            "rustfmt"
        } else {
            label
        };
        bail!(
            "{rejected_label} rejected semantic edit output for {}: {}",
            file_abs.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    fs::read_to_string(staged.path())
        .with_context(|| format!("reading formatted staging file for {}", file_abs.display()))
}

fn format_semantic_edit_content(
    file_abs: &Path,
    language: &str,
    content: &str,
) -> Result<(String, Option<String>)> {
    let Some(executor) = semantic_edit_executor_language(language, file_abs) else {
        return Ok((content.to_string(), None));
    };
    if executor != SemanticEditExecutorLanguage::Rust {
        parse_semantic_edit_source(content, executor, "formatter input")?;
    }

    let formatter = match executor.formatter() {
        SemanticEditFormatterContract::Rustfmt => Some((
            "rustfmt",
            vec!["--edition", "2024"],
            "rustfmt --edition 2024",
        )),
        SemanticEditFormatterContract::PythonAuto if command_available("ruff") => {
            Some(("ruff", vec!["format"], "ruff format"))
        }
        SemanticEditFormatterContract::PythonAuto if command_available("black") => {
            Some(("black", vec!["--quiet"], "black --quiet"))
        }
        SemanticEditFormatterContract::Prettier if command_available("prettier") => {
            Some(("prettier", vec!["--write"], "prettier --write"))
        }
        _ => None,
    };
    let Some((command, args, label)) = formatter else {
        return Ok((content.to_string(), None));
    };

    let staged = semantic_formatter_staging_file(file_abs, executor, content)?;
    let formatted = run_semantic_formatter(&staged, file_abs, command, &args, label)?;
    parse_semantic_edit_source(&formatted, executor, "formatter output")?;
    Ok((formatted, Some(label.to_string())))
}

fn plan_semantic_edit_intent(
    root: &Path,
    scope: Option<&str>,
    intent: &SemanticEditIntent,
    index: usize,
    budget: ResponseBudget,
) -> Result<SemanticEditIntentDraft> {
    let kind = normalize_semantic_edit_kind(&intent.kind);
    validate_semantic_edit_intent(&kind, intent)?;
    let destination_file_abs = if kind == "move_declaration" {
        Some(resolve_source_file(
            root,
            intent
                .file
                .as_deref()
                .context("move_declaration requires destination `file`")?,
        )?)
    } else {
        None
    };

    let mut target_hit = None;
    let mut target_selection = None;
    let (mut target_symbol, file_abs, mut target_range) = if let Some(handle) =
        intent.target_handle.as_deref()
    {
        let file_hint = if kind == "move_declaration" {
            None
        } else {
            intent.file.as_deref()
        };
        let resolved = resolve_semantic_edit_target_handle(root, scope, handle, file_hint, budget)?;
        if let Some(expected_symbol) = intent.symbol.as_deref()
            && expected_symbol != resolved.target_symbol.name
        {
            bail!(
                "semantic edit target_handle resolved to symbol {:?}, but intent requested symbol {:?}",
                resolved.target_symbol.name,
                expected_symbol
            );
        }
        target_selection = Some(resolved.selection);
        (
            Some(resolved.target_symbol),
            resolved.file_abs,
            Some(resolved.target_range),
        )
    } else if let Some(symbol) = intent.symbol.as_deref() {
        let file_hint = if kind == "move_declaration" {
            None
        } else {
            intent.file.as_deref()
        };
        let (hit, file_abs) = resolve_semantic_edit_symbol(root, scope, symbol, file_hint, budget)?;
        let start = symbol_hit_line(&hit);
        let end = symbol_hit_end_line(&hit).unwrap_or(start).max(start);
        let file_display = semantic_edit_file_display(root, &file_abs);
        target_hit = Some(hit.clone());
        (
            Some(SemanticEditSymbolTarget {
                name: hit.name,
                kind: hit.kind,
                language: hit.language,
                file: file_display,
                line: start,
                end_line: Some(end),
                span: None,
            }),
            file_abs,
            Some(SourceRangePreview {
                start,
                end,
                total_lines: 0,
                truncated_before: false,
                truncated_after: false,
            }),
        )
    } else {
        let file = intent
            .file
            .as_deref()
            .context("semantic edit intent requires `file` when `symbol` is omitted")?;
        (None, resolve_source_file(root, file)?, None)
    };

    let source = fs::read(&file_abs).with_context(|| format!("reading {}", file_abs.display()))?;
    let source_text = String::from_utf8(source.clone());
    let total_lines = String::from_utf8_lossy(&source).lines().count();
    if let (Some(target_symbol), Some(hit)) = (&mut target_symbol, &target_hit)
        && let Some(span) = symbol_hit_ast_span(hit, &source)
    {
        target_symbol.line = span.start_line;
        target_symbol.end_line = Some(span.end_line);
        if let Some(range) = &mut target_range {
            range.start = span.start_line;
            range.end = span.end_line;
        }
        target_symbol.span = Some(span);
    }
    let content_hash = semantic_edit_content_hash(&source);
    if let Some(range) = &mut target_range {
        range.total_lines = total_lines;
    }
    let target_file = semantic_edit_file_display(root, &file_abs);
    let destination_file = destination_file_abs
        .as_deref()
        .map(|file| semantic_edit_file_display(root, file));
    let (call_refs, cross_file_call_ref_total) = if matches!(
        kind.as_str(),
        "rewrite_call_sites" | "update_call_signature"
    ) {
        let target_name = target_symbol
            .as_ref()
            .map(|symbol| symbol.name.as_str())
            .context("call-site rewrite intent requires a resolved target symbol")?;
        resolve_semantic_edit_call_refs(root, scope, target_name, &file_abs)?
    } else {
        (Vec::new(), 0)
    };

    // A rename's blast radius is every file that references the symbol, not the
    // file the declaration happens to live in.
    let rename_scope = if kind == "rename_symbol" {
        match target_symbol.as_ref() {
            Some(symbol) => resolve_semantic_edit_rename_scope(root, scope, &symbol.name, &file_abs)
                .ok()
                .map(|scope| (symbol.name.clone(), scope)),
            None => None,
        }
    } else {
        None
    };

    let conflict = intent
        .expected_content_hash
        .as_deref()
        .is_some_and(|expected| expected != content_hash);

    let language = semantic_edit_target_language(target_symbol.as_ref(), &file_abs);
    let (status, apply_supported, diff, patch_proposal, message) = if conflict {
        (
            "conflict".to_string(),
            semantic_edit_kind_apply_supported(&kind, &language, &file_abs),
            None,
            None,
            "expected_content_hash does not match current file content; intent was not planned for mutation"
                .to_string(),
        )
    } else {
        match source_text {
            Ok(source_text) => {
                if kind == "move_declaration" {
                    let destination_file_abs = destination_file_abs
                        .as_deref()
                        .context("move_declaration requires destination `file`")?;
                    match fs::read_to_string(destination_file_abs)
                        .with_context(|| format!("reading {}", destination_file_abs.display()))
                        .and_then(|destination_text| {
                            let target = target_symbol
                                .as_ref()
                                .context("move_declaration requires a resolved target symbol")?;
                            preview_rust_move_declaration(
                                &source_text,
                                &destination_text,
                                &file_abs,
                                destination_file_abs,
                                &target.name,
                                &target.kind,
                            )
                            .map(|preview| (destination_text, preview))
                        }) {
                        Ok((destination_text, ((source_preview, _), (destination_preview, _)))) => {
                            let destination_language =
                                semantic_edit_language_for_file(destination_file_abs);
                            match semantic_edit_patch_proposal(
                                root,
                                &kind,
                                &[
                                    SemanticEditPatchFileInput {
                                        file_abs: &file_abs,
                                        language: &language,
                                        before: &source_text,
                                        after: &source_preview,
                                    },
                                    SemanticEditPatchFileInput {
                                        file_abs: destination_file_abs,
                                        language: &destination_language,
                                        before: &destination_text,
                                        after: &destination_preview,
                                    },
                                ],
                                budget,
                            ) {
                                Ok(patch_proposal) => {
                                    let mut diff_parts = Vec::new();
                                    if let Some(source_diff) = semantic_edit_diff_preview(
                                        &source_text,
                                        &source_preview,
                                        budget,
                                    ) {
                                        diff_parts.push(format!("{target_file}\n{source_diff}"));
                                    }
                                    if let Some(destination_file) = &destination_file
                                        && let Some(destination_diff) = semantic_edit_diff_preview(
                                            &destination_text,
                                            &destination_preview,
                                            budget,
                                        )
                                    {
                                        diff_parts.push(format!(
                                            "{destination_file}\n{destination_diff}"
                                        ));
                                    }
                                    (
                                        "planned".to_string(),
                                        true,
                                        (!diff_parts.is_empty()).then(|| {
                                            truncate_for_budget(
                                                &diff_parts.join("\n\n"),
                                                budget.preview_bytes(),
                                            )
                                        }),
                                        patch_proposal,
                                        "validated move_declaration intent; Rust executor can apply this edit"
                                            .to_string(),
                                    )
                                }
                                Err(err) => (
                                    "unsupported".to_string(),
                                    false,
                                    None,
                                    None,
                                    format!(
                                        "move_declaration patch proposal was refused by parser validation: {err:#}"
                                    ),
                                ),
                            }
                        }
                        Err(err) => (
                            "unsupported".to_string(),
                            false,
                            None,
                            None,
                            format!(
                                "move_declaration intent is not applyable by the current executor: {err:#}"
                            ),
                        ),
                    }
                } else {
                    match rename_scope
                        .as_ref()
                        .filter(|(_, scope)| scope.is_ambiguous())
                        .map(|(name, scope)| {
                            anyhow::anyhow!(
                                "rename_symbol refuses {name:?}: it is referenced from {}, and the index holds another definition of that name in {}; a call edge cannot say which definition those references belong to",
                                scope
                                    .caller_files
                                    .iter()
                                    .map(|file| semantic_edit_file_display(root, file))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                scope.ambiguous_definition_files.join(", ")
                            )
                        })
                        .map_or_else(
                            || {
                                preview_semantic_edit_content(
                                    &source_text,
                                    &file_abs,
                                    &language,
                                    &kind,
                                    intent,
                                    target_symbol.as_ref(),
                                    SemanticEditCallRefContext {
                                        refs: &call_refs,
                                        cross_file_total: cross_file_call_ref_total,
                                    },
                                )
                            },
                            Err,
                        ) {
                        Ok((preview, _)) => match semantic_edit_rename_patch_inputs(
                            root,
                            rename_scope.as_ref(),
                            intent.new_name.as_deref(),
                            semantic_edit_rename_target(target_symbol.as_ref()),
                        )
                        .and_then(|extra| {
                            let mut inputs = vec![SemanticEditPatchFileInput {
                                file_abs: &file_abs,
                                language: &language,
                                before: &source_text,
                                after: &preview,
                            }];
                            inputs.extend(extra.iter().map(|file| SemanticEditPatchFileInput {
                                file_abs: &file.file_abs,
                                language: &file.language,
                                before: &file.before,
                                after: &file.after,
                            }));
                            semantic_edit_patch_proposal(root, &kind, &inputs, budget)
                        }) {
                            Ok(patch_proposal) => (
                                "planned".to_string(),
                                true,
                                semantic_edit_diff_preview(&source_text, &preview, budget),
                                patch_proposal,
                                format!(
                                    "validated {kind} intent; {} executor can apply this edit",
                                    semantic_edit_executor_name(&language, &file_abs)
                                ),
                            ),
                            Err(err) => (
                                "unsupported".to_string(),
                                false,
                                None,
                                None,
                                format!(
                                    "{kind} patch proposal was refused by parser validation: {err:#}"
                                ),
                            ),
                        },
                        Err(err) => (
                            "unsupported".to_string(),
                            false,
                            None,
                            None,
                            format!(
                                "{kind} intent is not applyable by the current executor: {err:#}"
                            ),
                        ),
                    }
                }
            }
            Err(err) => (
                "unsupported".to_string(),
                false,
                None,
                None,
                format!("semantic edit executor requires UTF-8 source: {err}"),
            ),
        }
    };

    Ok(SemanticEditIntentDraft {
        plan: SemanticEditIntentPlan {
            handle: stable_handle(
                "eintent",
                &format!("{index}:{kind}:{target_file}:{content_hash}"),
            ),
            kind,
            status,
            apply_supported,
            applied: false,
            target_selection,
            target_symbol,
            call_refs,
            cross_file_call_ref_total: (cross_file_call_ref_total > 0)
                .then_some(cross_file_call_ref_total),
            target_file,
            destination_file,
            target_range,
            // Filled in on the apply path, where both revisions of the file
            // exist; a planned-but-not-applied intent has no edited range yet.
            edited_range: None,
            rename_caller_files: rename_scope
                .as_ref()
                .map(|(_, scope)| {
                    scope
                        .caller_files
                        .iter()
                        .map(|file| semantic_edit_file_display(root, file))
                        .collect()
                })
                .unwrap_or_default(),
            content_hash,
            diff,
            patch_proposal,
            formatter: None,
            message: truncate_for_budget(&message, budget.preview_bytes()),
        },
        file_abs,
        destination_file_abs,
        language,
        rename_caller_files: rename_scope
            .map(|(_, scope)| scope.caller_files)
            .unwrap_or_default(),
    })
}

pub(crate) struct SemanticEditFileBuffer {
    pub(crate) original: String,
    pub(crate) current: String,
    pub(crate) language: String,
    pub(crate) intents: usize,
}

fn ensure_semantic_edit_file_buffer(
    files: &mut BTreeMap<PathBuf, SemanticEditFileBuffer>,
    file_abs: &Path,
    language: String,
) -> Result<()> {
    if files.contains_key(file_abs) {
        return Ok(());
    }
    let original =
        fs::read_to_string(file_abs).with_context(|| format!("reading {}", file_abs.display()))?;
    files.insert(
        file_abs.to_path_buf(),
        SemanticEditFileBuffer {
            original: original.clone(),
            current: original,
            language,
            intents: 0,
        },
    );
    Ok(())
}

fn apply_semantic_edit_drafts(
    drafts: &mut [SemanticEditIntentDraft],
    intents: &[SemanticEditIntent],
    budget: ResponseBudget,
) -> Result<usize> {
    let blocked = drafts
        .iter()
        .filter(|draft| draft.plan.status != "planned")
        .map(|draft| {
            format!(
                "{}:{}: {}",
                draft.plan.handle, draft.plan.status, draft.plan.message
            )
        })
        .collect::<Vec<_>>();
    if !blocked.is_empty() {
        bail!(
            "refusing to apply semantic edit intents because some plans are not applyable: {}",
            blocked.join(", ")
        );
    }

    let mut files = BTreeMap::<PathBuf, SemanticEditFileBuffer>::new();
    for (idx, draft) in drafts.iter_mut().enumerate() {
        if draft.plan.kind == "move_declaration" {
            let source_file_abs = draft.file_abs.clone();
            let destination_file_abs = draft
                .destination_file_abs
                .clone()
                .context("move_declaration requires destination file")?;
            ensure_semantic_edit_file_buffer(&mut files, &source_file_abs, draft.language.clone())?;
            ensure_semantic_edit_file_buffer(
                &mut files,
                &destination_file_abs,
                semantic_edit_language_for_file(&destination_file_abs),
            )?;

            let source_current = files
                .get(&source_file_abs)
                .map(|buffer| buffer.current.clone())
                .context("missing source buffer for move_declaration")?;
            let destination_current = files
                .get(&destination_file_abs)
                .map(|buffer| buffer.current.clone())
                .context("missing destination buffer for move_declaration")?;
            let target = draft
                .plan
                .target_symbol
                .as_ref()
                .context("move_declaration requires a resolved target symbol")?;
            let ((updated_source, source_replacements), (updated_destination, dest_replacements)) =
                preview_rust_move_declaration(
                    &source_current,
                    &destination_current,
                    &source_file_abs,
                    &destination_file_abs,
                    &target.name,
                    &target.kind,
                )
                .with_context(|| format!("applying {}", draft.plan.handle))?;

            if let Some(source) = files.get_mut(&source_file_abs) {
                source.current = updated_source;
                source.intents += source_replacements.max(1);
            }
            if let Some(destination) = files.get_mut(&destination_file_abs) {
                destination.current = updated_destination;
                destination.intents += dest_replacements.max(1);
            }
            draft.plan.status = "applied".to_string();
            draft.plan.applied = true;
            draft.plan.message = truncate_for_budget(
                "applied move_declaration intent through the Rust semantic edit executor",
                budget.preview_bytes(),
            );
            continue;
        }

        // A rename that stops at the declaring file leaves every caller
        // referring to a name that no longer exists. These buffers are written
        // together at the end of the loop, so the whole rename lands or none of
        // it does.
        if draft.plan.kind == "rename_symbol" && !draft.rename_caller_files.is_empty() {
            let symbol = draft
                .plan
                .target_symbol
                .as_ref()
                .map(|symbol| symbol.name.clone())
                .context("rename_symbol requires a resolved target symbol")?;
            let rename_target = semantic_edit_rename_target(draft.plan.target_symbol.as_ref());
            let new_name = intents[idx]
                .new_name
                .clone()
                .context("rename_symbol requires new_name")?;
            for caller_file_abs in draft.rename_caller_files.clone() {
                let caller_language = semantic_edit_language_for_file(&caller_file_abs);
                let Some(executor) =
                    semantic_edit_executor_language(&caller_language, &caller_file_abs)
                else {
                    continue;
                };
                let Some(lang) = executor.contract().graph_lang else {
                    continue;
                };
                ensure_semantic_edit_file_buffer(
                    &mut files,
                    &caller_file_abs,
                    caller_language.clone(),
                )?;
                let current = files
                    .get(&caller_file_abs)
                    .map(|buffer| buffer.current.clone())
                    .context("missing caller buffer for rename_symbol")?;
                // An indexed caller that no longer mentions the symbol is stale
                // index state, not a failure: it contributes no edit.
                let Ok((updated, replacements)) = rename_identifier_occurrences(
                    &current,
                    &symbol,
                    &new_name,
                    lang,
                    executor.name(),
                    rename_target,
                ) else {
                    continue;
                };
                if let Some(buffer) = files.get_mut(&caller_file_abs) {
                    buffer.current = updated;
                    buffer.intents += replacements.max(1);
                }
            }
        }

        let file_abs = draft.file_abs.clone();
        ensure_semantic_edit_file_buffer(&mut files, &file_abs, draft.language.clone())?;
        let buffer = files.get_mut(&file_abs).unwrap();
        let (updated, replacements) = preview_semantic_edit_content(
            &buffer.current,
            &draft.file_abs,
            &draft.language,
            &draft.plan.kind,
            &intents[idx],
            draft.plan.target_symbol.as_ref(),
            SemanticEditCallRefContext {
                refs: &draft.plan.call_refs,
                cross_file_total: draft.plan.cross_file_call_ref_total.unwrap_or(0),
            },
        )
        .with_context(|| format!("applying {}", draft.plan.handle))?;
        draft.plan.edited_range = changed_line_range(
            &buffer.current,
            &updated,
            draft
                .plan
                .target_range
                .as_ref()
                .map(|range| range.total_lines)
                .unwrap_or_else(|| updated.lines().count()),
        );
        buffer.current = updated;
        buffer.intents += replacements.max(1);
        draft.plan.status = "applied".to_string();
        draft.plan.applied = true;
        draft.plan.message = truncate_for_budget(
            &format!(
                "applied {} intent through the {} semantic edit executor",
                draft.plan.kind,
                semantic_edit_executor_name(&draft.language, &draft.file_abs)
            ),
            budget.preview_bytes(),
        );
    }

    let mut formatted_total = 0usize;
    let mut edit_plan = Vec::new();
    for (index, (file, buffer)) in files.into_iter().enumerate() {
        if buffer.original == buffer.current {
            continue;
        }
        let (formatted, formatter) =
            format_semantic_edit_content(&file, &buffer.language, &buffer.current)?;
        if formatter.is_some() {
            formatted_total += 1;
        }
        for draft in drafts.iter_mut().filter(|draft| draft.file_abs == file) {
            draft.plan.formatter = formatter.clone();
        }
        edit_plan.push(PlannedEdit {
            index,
            file,
            new_content: formatted,
            replacements: buffer.intents,
        });
    }

    apply_edit_plan_atomically(edit_plan)?;
    Ok(formatted_total)
}

pub(crate) struct SemanticEditVerificationWorktree {
    pub(crate) repo_root: PathBuf,
    pub(crate) worktree_root: PathBuf,
    pub(crate) _tempdir: TempDir,
}

impl Drop for SemanticEditVerificationWorktree {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.worktree_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl SemanticEditVerificationWorktree {
    fn verification_root_for(&self, root: &Path) -> Result<PathBuf> {
        let canonical_root = root
            .canonicalize()
            .with_context(|| format!("canonicalizing project root {}", root.display()))?;
        let rel_root = canonical_root
            .strip_prefix(&self.repo_root)
            .with_context(|| {
                format!(
                    "project root {} is outside git repository {}",
                    canonical_root.display(),
                    self.repo_root.display()
                )
            })?;
        Ok(self.worktree_root.join(rel_root))
    }
}

fn create_semantic_edit_verification_worktree(
    root: &Path,
) -> Result<SemanticEditVerificationWorktree> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("locating git repository for {}", root.display()))?;
    if !output.status.success() {
        bail!(
            "semantic edit verification requires a git worktree rooted at {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let repo_root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        .canonicalize()
        .context("canonicalizing git repository root for semantic edit verification")?;
    let tempdir = tempfile::Builder::new()
        .prefix("tsift-semantic-verify-")
        .tempdir()
        .context("creating semantic edit verification temp directory")?;
    let worktree_root = tempdir.path().join("worktree");
    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .args(["worktree", "add", "--detach"])
        .arg(&worktree_root)
        .arg("HEAD")
        .output()
        .with_context(|| {
            format!(
                "creating semantic edit verification worktree for {}",
                repo_root.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "failed to create semantic edit verification worktree for {}: {}",
            repo_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(SemanticEditVerificationWorktree {
        repo_root,
        worktree_root,
        _tempdir: tempdir,
    })
}

fn run_semantic_edit_verification_reindex(root: &Path) -> Result<()> {
    let output = Command::new(env::current_exe().context("resolving current tsift executable")?)
        .arg("index")
        .arg(root)
        .output()
        .with_context(|| {
            format!(
                "reindexing semantic edit verification root {}",
                root.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "semantic edit verification reindex failed for {}: {}{}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    Ok(())
}

fn semantic_edit_verification_source_windows(
    root: &Path,
    drafts: &[SemanticEditIntentDraft],
) -> Vec<(String, usize, usize)> {
    let mut windows = BTreeMap::<String, (usize, usize)>::new();
    for draft in drafts {
        let mut files = vec![draft.plan.target_file.clone()];
        if let Some(destination) = &draft.plan.destination_file {
            files.push(destination.clone());
        }
        for file in files {
            let (mut start, mut lines) = draft
                .plan
                .target_range
                .as_ref()
                .map(|range| {
                    let line_count = range.end.saturating_sub(range.start).saturating_add(1);
                    (
                        range.start.saturating_sub(2).max(1),
                        line_count.saturating_add(4),
                    )
                })
                .unwrap_or((1, 40));
            lines = lines.clamp(1, 80);
            if let Ok(source) = fs::read_to_string(root.join(&file)) {
                let total_lines = source.lines().count();
                if total_lines > 0 {
                    start = start.min(total_lines).max(1);
                    lines = lines.min(total_lines.saturating_sub(start).saturating_add(1).max(1));
                }
            }
            windows
                .entry(file)
                .and_modify(|existing| {
                    existing.0 = existing.0.min(start);
                    existing.1 = existing.1.max(lines);
                })
                .or_insert((start, lines));
        }
    }
    windows
        .into_iter()
        .map(|(file, (start, lines))| (file, start, lines))
        .collect()
}

fn structured_json_row_count(value: &serde_json::Value) -> usize {
    value.as_array().map_or_else(
        || value["_r"].as_array().map_or(0, |rows| rows.len()),
        |rows| rows.len(),
    )
}

fn run_semantic_edit_verification_source_read(
    root: &Path,
    file: &str,
    start: usize,
    lines: usize,
) -> Result<SemanticEditVerificationSourceRead> {
    let root_display = root.to_string_lossy().to_string();
    let output = Command::new(env::current_exe().context("resolving current tsift executable")?)
        .args([
            "--envelope",
            "source-read",
            file,
            "--path",
            &root_display,
            "--style",
            "window",
            "--start",
            &start.to_string(),
            "--lines",
            &lines.to_string(),
            "--json",
            "--budget",
            "normal",
        ])
        .output()
        .with_context(|| {
            format!(
                "running source-read verification for {} in {}",
                file,
                root.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "semantic edit verification source-read failed for {}: {}",
            file,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing source-read verification JSON")?;
    let report = &json["report"];
    let preview_lines = structured_json_row_count(&report["preview"]);
    let symbol_refs = structured_json_row_count(&report["symbols"]);
    let summary_refs = structured_json_row_count(&report["summaries"]);
    Ok(SemanticEditVerificationSourceRead {
        file: file.to_string(),
        start,
        lines,
        preview_lines,
        symbol_refs,
        summary_refs,
        command: format!(
            "tsift --envelope source-read {} --path {} --style window --start {} --lines {} --budget normal",
            shell_quote(file),
            shell_quote(&root_display),
            start,
            lines
        ),
    })
}

fn run_semantic_edit_verification_command(
    root: &Path,
    command: &str,
    budget: ResponseBudget,
) -> Result<SemanticEditVerificationCommand> {
    let output = Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(root)
        .output()
        .with_context(|| format!("running semantic edit verification command: {command}"))?;
    let stdout = truncate_for_budget(
        String::from_utf8_lossy(&output.stdout).trim(),
        budget.preview_bytes(),
    );
    let stderr = truncate_for_budget(
        String::from_utf8_lossy(&output.stderr).trim(),
        budget.preview_bytes(),
    );
    if !output.status.success() {
        bail!(
            "semantic edit verification command failed ({command}): stdout={stdout:?} stderr={stderr:?}"
        );
    }
    Ok(SemanticEditVerificationCommand {
        command: command.to_string(),
        status: "passed".to_string(),
        stdout,
        stderr,
    })
}

fn verify_semantic_edit_intents(
    root: &Path,
    scope: Option<&str>,
    intents: &[SemanticEditIntent],
    budget: ResponseBudget,
    verify_command: Option<&str>,
) -> Result<SemanticEditVerificationReport> {
    let worktree = create_semantic_edit_verification_worktree(root)?;
    let verify_root = worktree.verification_root_for(root)?;
    run_semantic_edit_verification_reindex(&verify_root)?;
    let mut drafts = intents
        .iter()
        .enumerate()
        .map(|(idx, intent)| plan_semantic_edit_intent(&verify_root, scope, intent, idx, budget))
        .collect::<Result<Vec<_>>>()?;
    let temp_formatted_total = apply_semantic_edit_drafts(&mut drafts, intents, budget)?;
    let temp_applied_total = drafts.iter().filter(|draft| draft.plan.applied).count();
    run_semantic_edit_verification_reindex(&verify_root)?;

    let source_reads = semantic_edit_verification_source_windows(&verify_root, &drafts)
        .into_iter()
        .map(|(file, start, lines)| {
            run_semantic_edit_verification_source_read(&verify_root, &file, start, lines)
        })
        .collect::<Result<Vec<_>>>()?;
    let impact_report = impact::compute(
        &verify_root,
        impact::ImpactOptions {
            cached: false,
            revision: None,
            scope,
            limit: 10,
        },
    )
    .with_context(|| {
        format!(
            "running semantic edit verification impact summary in {}",
            verify_root.display()
        )
    })?;
    let command = verify_command
        .map(|command| run_semantic_edit_verification_command(&verify_root, command, budget))
        .transpose()?;

    Ok(SemanticEditVerificationReport {
        status: "passed".to_string(),
        worktree: "temporary git worktree at HEAD".to_string(),
        reindexed: true,
        temp_applied_total,
        temp_formatted_total,
        source_reads,
        impact: SemanticEditVerificationImpact {
            changed_files: impact_report.changed_files.len(),
            changed_symbols: impact_report.changed_symbols.len(),
            affected_tests: impact_report.affected_tests.len(),
            affected_tests_total: impact_report.affected_tests_total,
            truncated: impact_report.truncated,
            warnings: impact_report.warnings,
        },
        command,
        message: "verified semantic edit intents in a temporary worktree before source mutation"
            .to_string(),
    })
}

pub(crate) fn cmd_edit_intents(
    path: &Path,
    scope: Option<&str>,
    file: Option<PathBuf>,
    apply: bool,
    verify: SemanticEditVerifyOptions<'_>,
    format: OutputFormat,
    budget: ResponseBudget,
) -> Result<()> {
    let input = match file {
        Some(path) => fs::read_to_string(&path)
            .with_context(|| format!("reading semantic edit intent file: {}", path.display()))?,
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading semantic edit intents from stdin")?;
            buf
        }
    };
    let batch: SemanticEditIntentBatch =
        serde_json::from_str(&input).context("parsing semantic edit intent JSON")?;
    let root = lint::resolve_project_root_or_canonical_path(path)?;
    let max_items = if apply || verify.enabled {
        batch.intents.len()
    } else {
        budget.preview_items()
    };
    let mut drafts = batch
        .intents
        .iter()
        .take(max_items)
        .enumerate()
        .map(|(idx, intent)| plan_semantic_edit_intent(&root, scope, intent, idx, budget))
        .collect::<Result<Vec<_>>>()?;
    let verification = if verify.enabled {
        Some(verify_semantic_edit_intents(
            &root,
            scope,
            &batch.intents,
            budget,
            verify.command,
        )?)
    } else {
        None
    };
    let mut formatted_total = 0usize;
    if apply {
        formatted_total = apply_semantic_edit_drafts(&mut drafts, &batch.intents, budget)?;
    }

    let planned_total = drafts
        .iter()
        .filter(|draft| matches!(draft.plan.status.as_str(), "planned" | "applied"))
        .count();
    let applied_total = drafts.iter().filter(|draft| draft.plan.applied).count();
    let conflict_total = drafts
        .iter()
        .filter(|draft| draft.plan.status == "conflict")
        .count();
    let unsupported_total = drafts
        .iter()
        .filter(|draft| draft.plan.status == "unsupported")
        .count();
    let plans = drafts
        .into_iter()
        .map(|draft| draft.plan)
        .collect::<Vec<_>>();
    let report = SemanticEditIntentReport {
        root: root.to_string_lossy().to_string(),
        mode: if apply {
            "apply"
        } else if verify.enabled {
            "verify"
        } else {
            "dry_run"
        }
        .to_string(),
        intents_total: batch.intents.len(),
        planned_total,
        applied_total,
        conflict_total,
        unsupported_total,
        formatted_total,
        plans,
        verification,
        warnings: (batch.intents.len() > max_items)
            .then(|| {
                format!(
                    "truncated semantic edit plans from {} to {} items by response budget",
                    batch.intents.len(),
                    max_items
                )
            })
            .into_iter()
            .collect(),
    };

    if format.json_output {
        let follow_up = report
            .plans
            .iter()
            .filter_map(|plan| {
                plan.target_range.as_ref().map(|range| {
                    source_read_command(
                        &root,
                        &plan.target_file,
                        range.start,
                        range.end.saturating_sub(range.start).saturating_add(1),
                    )
                })
            })
            .collect::<Vec<_>>();
        print_json_or_envelope(
            &report,
            &format,
            "edit-intents",
            if apply {
                "apply"
            } else if verify.enabled {
                "verify"
            } else {
                "dry-run"
            },
            ToolEnvelopeSummary {
                text: format!(
                    "semantic edit intents planned={} applied={} conflicts={} unsupported={}",
                    report.planned_total,
                    report.applied_total,
                    report.conflict_total,
                    report.unsupported_total
                ),
                metrics: vec![
                    envelope_metric("intents", report.intents_total),
                    envelope_metric("planned", report.planned_total),
                    envelope_metric("applied", report.applied_total),
                    envelope_metric("conflicts", report.conflict_total),
                    envelope_metric("unsupported", report.unsupported_total),
                ],
            },
            report.intents_total > max_items || report.conflict_total > 0,
            follow_up,
        )?;
    } else {
        println!(
            "Semantic edit intents: planned={} applied={} conflicts={} unsupported={} mode={}",
            report.planned_total,
            report.applied_total,
            report.conflict_total,
            report.unsupported_total,
            report.mode
        );
        for plan in &report.plans {
            println!(
                "  {} {} {} apply_supported={} applied={} {}",
                plan.handle,
                plan.status,
                plan.kind,
                plan.apply_supported,
                plan.applied,
                plan.target_file
            );
            if let Some(range) = &plan.target_range {
                println!("    target range: {}-{}", range.start, range.end);
            }
            if let Some(range) = &plan.edited_range {
                println!("    edited range: {}-{}", range.start, range.end);
            }
            if !plan.rename_caller_files.is_empty() {
                println!(
                    "    also renamed in: {}",
                    plan.rename_caller_files.join(", ")
                );
            }
            if let Some(formatter) = &plan.formatter {
                println!("    formatter: {formatter}");
            }
            println!("    {}", plan.message);
        }
        if let Some(verification) = &report.verification {
            println!(
                "  verification: {} temp_applied={} source_reads={} affected_tests={}/{}",
                verification.status,
                verification.temp_applied_total,
                verification.source_reads.len(),
                verification.impact.affected_tests,
                verification.impact.affected_tests_total
            );
            if let Some(command) = &verification.command {
                println!("    command: {} {}", command.status, command.command);
            }
        }
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
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

pub(crate) fn build_edit_plan(batch: &EditBatch) -> Result<Vec<PlannedEdit>> {
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

pub(crate) fn apply_edit_plan_atomically(plan: Vec<PlannedEdit>) -> Result<Vec<EditResult>> {
    apply_edit_plan_atomically_inner(plan, |_, _| Ok(()))
}

pub(crate) fn apply_edit_plan_atomically_inner<F>(
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

#[cfg(test)]
mod rename_symbol_tests {
    use super::*;

    const RUST_SRC: &str = r#"/// doc widget_count
pub fn widget_count() -> usize { 3 }

pub fn describe() -> String {
    // widget_count comment
    let label = "widget_count";
    format!("{label}: {}", widget_count())
}
"#;

    /// The rename used to be a substring scan, so it rewrote the doc comment,
    /// the line comment, and the string literal too. The string literal is the
    /// one that matters: that value is data, and renaming it changes behaviour.
    /// Each position is asserted on its own — a test that only counted
    /// replacements would pass while renaming the wrong three.
    #[test]
    fn rust_rename_leaves_comments_and_string_literals_alone() {
        let (out, replacements) =
            replace_rust_identifier(RUST_SRC, "widget_count", "gadget_count", graph::RenameTarget::Callable).unwrap();
        assert_eq!(replacements, 2, "expected the definition and the call");
        assert!(
            out.contains("pub fn gadget_count()"),
            "definition not renamed:\n{out}"
        );
        assert!(
            out.contains("gadget_count())"),
            "call inside format! not renamed:\n{out}"
        );
        assert!(
            out.contains("/// doc widget_count"),
            "doc comment was renamed:\n{out}"
        );
        assert!(
            out.contains("// widget_count comment"),
            "line comment was renamed:\n{out}"
        );
        assert!(
            out.contains("let label = \"widget_count\";"),
            "string literal was renamed:\n{out}"
        );
    }

    /// tree-sitter parses Rust macro arguments as an opaque `token_tree`, which
    /// is why structural patterns under-report inside `assert_eq!`. The
    /// identifiers within it are still named `identifier` nodes, so an AST
    /// rename must not regress against the old scan here.
    #[test]
    fn rust_rename_reaches_calls_inside_macro_arguments() {
        let src = "fn f() { assert_eq!(widget_count(), 3); }\n";
        let (out, replacements) = replace_rust_identifier(src, "widget_count", "gadget_count", graph::RenameTarget::Callable)
            .expect("macro-argument call should be renamable");
        assert_eq!(replacements, 1);
        assert!(out.contains("assert_eq!(gadget_count(), 3)"), "{out}");
    }

    /// A name that appears only in prose is not a symbol, so the rename has
    /// nothing to do and must say so rather than editing the prose.
    #[test]
    fn rust_rename_refuses_a_name_that_only_appears_in_prose() {
        let src = "// widget_count is not defined here\nfn other() {}\n";
        let err = replace_rust_identifier(src, "widget_count", "gadget_count", graph::RenameTarget::Callable)
            .expect_err("a comment mention is not a rename target");
        assert!(
            err.to_string().contains("was not found"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn python_rename_leaves_comments_and_string_literals_alone() {
        let src = "def widget_count():\n    # widget_count comment\n    return \"widget_count\"\n\nwidget_count()\n";
        let (out, replacements) = replace_script_identifier(
            src,
            "widget_count",
            "gadget_count",
            SemanticEditExecutorLanguage::Python,
            graph::RenameTarget::Callable,
        )
        .unwrap();
        assert_eq!(replacements, 2);
        assert!(out.contains("def gadget_count():"), "{out}");
        assert!(out.contains("\ngadget_count()"), "{out}");
        assert!(out.contains("# widget_count comment"), "{out}");
        assert!(out.contains("return \"widget_count\""), "{out}");
    }

    #[test]
    fn typescript_rename_leaves_comments_and_string_literals_alone() {
        let src = "// widgetCount comment\nexport function widgetCount(): number { return 1; }\nconst label = \"widgetCount\";\nwidgetCount();\n";
        let (out, replacements) = replace_script_identifier(
            src,
            "widgetCount",
            "gadgetCount",
            SemanticEditExecutorLanguage::TypeScript,
            graph::RenameTarget::Callable,
        )
        .unwrap();
        assert_eq!(replacements, 2);
        assert!(out.contains("export function gadgetCount()"), "{out}");
        assert!(out.contains("\ngadgetCount();"), "{out}");
        assert!(out.contains("// widgetCount comment"), "{out}");
        assert!(out.contains("const label = \"widgetCount\";"), "{out}");
    }

    /// `target_range` is the resolved symbol's declaration span, so for a
    /// rename it is one line while the edit reaches every call site in the
    /// file. `edited_range` is the one that describes the change.
    #[test]
    fn changed_line_range_spans_the_whole_edit_not_the_declaration() {
        let (after, _) = replace_rust_identifier(RUST_SRC, "widget_count", "gadget_count", graph::RenameTarget::Callable).unwrap();
        let range = changed_line_range(RUST_SRC, &after, RUST_SRC.lines().count())
            .expect("the rename changed the file");
        assert_eq!(range.start, 2, "declaration is on line 2");
        assert_eq!(range.end, 7, "the call inside format! is on line 7");
    }

    #[test]
    fn changed_line_range_is_none_when_nothing_changed() {
        assert!(changed_line_range(RUST_SRC, RUST_SRC, 8).is_none());
    }

    fn rename_fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
        }
        let db = index::IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();
        dir
    }

    fn run_rename(dir: &std::path::Path, intents: &str) -> Result<()> {
        let intent_file = dir.join("intents.json");
        std::fs::write(&intent_file, intents).unwrap();
        cmd_edit_intents(
            dir,
            None,
            Some(intent_file),
            true,
            SemanticEditVerifyOptions {
                enabled: false,
                command: None,
            },
            OutputFormat {
                json_output: true,
                compact: false,
                pretty: false,
                terse: false,
                ultra_terse: false,
                schema: false,
                envelope: false,
            },
            ResponseBudget::default(),
        )
    }

    /// The defect this phase exists for: a rename edited the declaring file,
    /// reported `conflicts=0`, and left every caller in every other file
    /// referring to a name that no longer exists.
    #[test]
    fn rename_reaches_callers_in_other_files() {
        let dir = rename_fixture(&[
            (
                "src/lib.rs",
                "pub mod caller;\npub fn widget_count() -> usize { 3 }\n",
            ),
            (
                "src/caller.rs",
                "use crate::widget_count;\npub fn total() -> usize { widget_count() + 1 }\n",
            ),
        ]);
        run_rename(
            dir.path(),
            r#"{"intents":[{"kind":"rename_symbol","symbol":"widget_count","new_name":"gadget_count"}]}"#,
        )
        .expect("cross-file rename should apply");

        let lib = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
        let caller = std::fs::read_to_string(dir.path().join("src/caller.rs")).unwrap();
        assert!(lib.contains("gadget_count"), "declaration not renamed: {lib}");
        assert!(
            !caller.contains("widget_count"),
            "caller still refers to the old name, so the tree does not build: {caller}"
        );
        assert!(
            caller.contains("use crate::gadget_count;"),
            "import not renamed: {caller}"
        );
        assert!(
            caller.contains("gadget_count()"),
            "call site not renamed: {caller}"
        );
    }

    /// Zig used to have symbol extraction but no call query, so phase 2 could
    /// never discover a `.zig` caller file even though the occurrence walker
    /// knew how to rename an imported namespace member once given that file.
    #[test]
    fn zig_rename_reaches_imported_namespace_callers() {
        let dir = rename_fixture(&[
            (
                "src/lib.zig",
                "pub fn widgetCount() usize { return 3; }\n",
            ),
            (
                "src/caller.zig",
                "const lib = @import(\"lib.zig\");\npub fn total() usize { return lib.widgetCount() + 1; }\n",
            ),
        ]);
        run_rename(
            dir.path(),
            r#"{"intents":[{"kind":"rename_symbol","symbol":"widgetCount","new_name":"gadgetCount","file":"src/lib.zig"}]}"#,
        )
        .expect("cross-file Zig rename should apply");

        let library = std::fs::read_to_string(dir.path().join("src/lib.zig")).unwrap();
        let caller = std::fs::read_to_string(dir.path().join("src/caller.zig")).unwrap();
        assert!(library.contains("fn gadgetCount()"), "{library}");
        assert!(caller.contains("lib.gadgetCount()"), "{caller}");
        assert!(!caller.contains("widgetCount"), "{caller}");
    }

    /// A call edge selects the Kotlin caller file; import binding then proves
    /// both the call and an uncalled qualified access in that file refer through
    /// the external type namespace.
    #[test]
    fn kotlin_rename_keeps_qualified_imported_type_occurrences() {
        let dir = rename_fixture(&[
            (
                "src/Panel.kt",
                "package widgets\n\nclass Panel {\n    companion object {\n        fun widgetCount(): Int = 3\n    }\n}\n",
            ),
            (
                "src/Caller.kt",
                "package app\nimport widgets.Panel\nval ref = Panel.widgetCount\nfun total(): Int = Panel.widgetCount() + 1\n",
            ),
        ]);
        run_rename(
            dir.path(),
            r#"{"intents":[{"kind":"rename_symbol","symbol":"widgetCount","new_name":"gadgetCount","file":"src/Panel.kt"}]}"#,
        )
        .expect("cross-file Kotlin rename should apply");

        let panel = std::fs::read_to_string(dir.path().join("src/Panel.kt")).unwrap();
        let caller = std::fs::read_to_string(dir.path().join("src/Caller.kt")).unwrap();
        assert!(panel.contains("fun gadgetCount()"), "{panel}");
        assert!(caller.contains("val ref = Panel.gadgetCount"), "{caller}");
        assert!(caller.contains("Panel.gadgetCount()"), "{caller}");
        assert!(!caller.contains("widgetCount"), "{caller}");
    }

    /// Both lookups behind a cross-file rename are by name, and a name is not
    /// unique across languages. Scoping them to the file alone made a Python
    /// `beta` block renaming a JavaScript `beta` — and would have let the
    /// rename rewrite the unrelated Python file. Caught by the existing
    /// `edit_intents_apply_mutates_javascript_executor_intents` suite.
    #[test]
    fn a_same_named_symbol_in_another_language_is_neither_ambiguous_nor_renamed() {
        let python_before = "def beta():\n    return 1\n";
        let dir = rename_fixture(&[
            ("src/mod.js", "export function beta() { return 1; }\n"),
            (
                "src/use.js",
                "import { beta } from './mod.js';\nexport function total() { return beta() + 1; }\n",
            ),
            ("src/script.py", python_before),
        ]);
        run_rename(
            dir.path(),
            r#"{"intents":[{"kind":"rename_symbol","symbol":"beta","new_name":"gamma","file":"src/mod.js"}]}"#,
        )
        .expect("a same-named Python function must not block a JavaScript rename");

        assert!(
            std::fs::read_to_string(dir.path().join("src/mod.js"))
                .unwrap()
                .contains("gamma"),
            "javascript declaration not renamed"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/script.py")).unwrap(),
            python_before,
            "an unrelated Python symbol of the same name was rewritten"
        );
    }

    /// Two independent modules each defining `beta` and each calling their own
    /// is ordinary, not ambiguous: with no cross-file reference to attribute,
    /// the rename never leaves the declaring file. Refusing here would block
    /// the common case to guard one that cannot occur.
    #[test]
    fn a_same_named_definition_elsewhere_is_fine_when_nothing_calls_across_files() {
        let other_before = "function beta(value) { return value + 1; }\n";
        let dir = rename_fixture(&[
            (
                "src/app.js",
                "function alpha(value) { return beta(value); }\nfunction beta(value) { return value + 1; }\n",
            ),
            ("src/other.js", other_before),
        ]);
        run_rename(
            dir.path(),
            r#"{"intents":[{"kind":"rename_symbol","symbol":"beta","new_name":"betaRenamed","file":"src/app.js"}]}"#,
        )
        .expect("an unrelated module defining the same name must not block the rename");

        let app = std::fs::read_to_string(dir.path().join("src/app.js")).unwrap();
        assert!(app.contains("betaRenamed(value)"), "{app}");
        assert!(!app.contains(" beta("), "{app}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/other.js")).unwrap(),
            other_before,
            "an unrelated module was rewritten"
        );
    }

    /// Call edges are matched by name, not by resolved binding, so a file that
    /// defines its own `beta` and calls it looks like a caller of *our* `beta`.
    /// Renaming there rewrites an unrelated function. Its own definition
    /// shadows any import, so it is calling itself and is not our caller.
    /// Caught by `edit_intents_apply_mutates_javascript_executor_intents`.
    #[test]
    fn a_file_that_defines_its_own_same_named_function_is_not_our_caller() {
        let other_before =
            "function alpha(value) { return beta(value); }\nfunction beta(value) { return value + 1; }\n";
        let dir = rename_fixture(&[
            (
                "src/app.js",
                "function alpha(value) { return beta(value); }\nfunction beta(value) { return value + 1; }\n",
            ),
            ("src/other.js", other_before),
        ]);
        run_rename(
            dir.path(),
            r#"{"intents":[{"kind":"rename_symbol","symbol":"beta","new_name":"betaRenamed","file":"src/app.js"}]}"#,
        )
        .expect("a file calling its own same-named function must not block the rename");

        assert!(
            std::fs::read_to_string(dir.path().join("src/app.js"))
                .unwrap()
                .contains("betaRenamed(value)"),
            "target file not renamed"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/other.js")).unwrap(),
            other_before,
            "a file calling its own same-named function was rewritten"
        );
    }

    /// Two definitions sharing a name make each reference unattributable from
    /// call edges alone. Renaming the wrong one is worse than declining, so the
    /// intent refuses and names the file that made it ambiguous — and nothing
    /// on disk is touched.
    #[test]
    fn rename_refuses_when_a_second_definition_shares_the_name() {
        let lib_before = "pub mod caller;\npub mod other;\npub fn widget_count() -> usize { 3 }\n";
        let dir = rename_fixture(&[
            ("src/lib.rs", lib_before),
            (
                "src/caller.rs",
                "use crate::widget_count;\npub fn total() -> usize { widget_count() + 1 }\n",
            ),
            ("src/other.rs", "pub fn widget_count() -> usize { 99 }\n"),
        ]);
        let err = run_rename(
            dir.path(),
            r#"{"intents":[{"kind":"rename_symbol","symbol":"widget_count","new_name":"gadget_count","file":"src/lib.rs"}]}"#,
        )
        .expect_err("an ambiguous rename must refuse");
        let message = format!("{err:#}");
        assert!(
            message.contains(r#"rename_symbol refuses "widget_count""#)
                && message.contains("referenced from src/caller.rs"),
            "refusal does not name the symbol and the reference it cannot attribute: {message}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            lib_before,
            "a refused rename must not mutate anything"
        );
        assert!(
            std::fs::read_to_string(dir.path().join("src/caller.rs"))
                .unwrap()
                .contains("widget_count"),
            "a refused rename must not mutate anything"
        );
    }
}

#[cfg(test)]
mod structural_rewrite_tests {
    use super::*;

    const RUST_SRC: &str = "fn main() {\n    foo(1);\n    foo(2);\n}\n";

    fn intent(kind: &str, pattern: Option<&str>, replacement: Option<&str>) -> SemanticEditIntent {
        SemanticEditIntent {
            kind: kind.to_string(),
            target_handle: None,
            symbol: None,
            file: Some(PathBuf::from("main.rs")),
            destination_symbol: None,
            position: None,
            replacement: replacement.map(str::to_string),
            pattern: pattern.map(str::to_string),
            call_replacement: None,
            new_name: None,
            start_line: None,
            end_line: None,
            expected_content_hash: None,
        }
    }

    fn rename_intent(new_name: &str) -> SemanticEditIntent {
        SemanticEditIntent {
            new_name: Some(new_name.to_string()),
            ..intent("rename_symbol", None, None)
        }
    }

    fn semantic_edit_symbol_target(name: &str, kind: &str) -> SemanticEditSymbolTarget {
        SemanticEditSymbolTarget {
            name: name.to_string(),
            kind: kind.to_string(),
            language: String::new(),
            file: String::new(),
            line: 1,
            end_line: None,
            span: None,
        }
    }

    #[test]
    fn rewrites_every_match_not_just_the_first() {
        let (out, replacements) = preview_structural_rewrite(
            RUST_SRC,
            SemanticEditExecutorLanguage::Rust,
            &intent("structural_rewrite", Some("foo($A)"), Some("bar($A)")),
        )
        .unwrap();
        assert_eq!(replacements, 2);
        assert!(out.contains("bar(1)"), "{out}");
        assert!(out.contains("bar(2)"), "{out}");
        assert!(!out.contains("foo("), "{out}");
    }

    const PYTHON_SRC: &str =
        "def outer(base, scale):\n    prefix = base * 2\n    acc = 0\n    for item in range(scale):\n        acc += item * prefix\n    return acc\n";

    fn extract_intent(start_line: usize, end_line: usize, new_name: &str) -> SemanticEditIntent {
        SemanticEditIntent {
            new_name: Some(new_name.to_string()),
            start_line: Some(start_line),
            end_line: Some(end_line),
            file: Some(PathBuf::from("script.py")),
            ..intent("extract_function", None, None)
        }
    }

    #[test]
    fn an_extraction_emits_a_call_and_a_def_that_agree() {
        // The signature is derived, not supplied, so the only assertion worth
        // making is that the call the caller is left with matches the function
        // it now calls — argument for argument, binding for binding.
        let (out, replacements) = preview_extract_function(
            PYTHON_SRC,
            SemanticEditExecutorLanguage::Python,
            &extract_intent(3, 5, "accumulate"),
        )
        .expect("planned");

        assert_eq!(replacements, 1);
        assert!(out.contains("    acc = accumulate(prefix, scale)\n"), "{out}");
        assert!(out.contains("def accumulate(prefix, scale):"), "{out}");
        assert!(out.contains("    acc = 0"), "{out}");
        assert!(out.contains("        acc += item * prefix"), "{out}");
        assert!(out.contains("    return acc"), "{out}");
        // The hoisted statements are gone from `outer`, not duplicated into it.
        assert_eq!(out.matches("for item in range(scale):").count(), 1, "{out}");
        // And the result is still Python.
        parse_semantic_edit_source(&out, SemanticEditExecutorLanguage::Python, "extracted")
            .expect("reparses");
    }

    #[test]
    fn an_extraction_whose_control_flow_escapes_is_refused() {
        let err = preview_extract_function(
            PYTHON_SRC,
            SemanticEditExecutorLanguage::Python,
            &extract_intent(6, 6, "finish"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("`return`"), "{err:#}");
    }

    #[test]
    fn a_line_range_is_rejected_for_every_other_kind() {
        // A stray range must not silently widen an edit that resolved its
        // target by name.
        let err = validate_semantic_edit_intent(
            "rename_symbol",
            &SemanticEditIntent {
                symbol: Some("alpha".to_string()),
                start_line: Some(1),
                end_line: Some(2),
                ..rename_intent("beta")
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("does not support"), "{err:#}");
    }

    #[test]
    fn an_extraction_without_a_range_is_rejected_before_planning() {
        let err = validate_semantic_edit_intent(
            "extract_function",
            &SemanticEditIntent {
                new_name: Some("accumulate".to_string()),
                ..intent("extract_function", None, None)
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("requires `start_line`"), "{err:#}");
    }

    #[test]
    fn a_pattern_that_matches_nothing_is_a_refusal_not_an_empty_plan() {
        // An empty edit that reported "planned" would advertise a codemod the
        // caller could apply and see nothing happen.
        let err = preview_structural_rewrite(
            RUST_SRC,
            SemanticEditExecutorLanguage::Rust,
            &intent("structural_rewrite", Some("nope($A)"), Some("yes($A)")),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("matched nothing"), "{err:#}");
    }

    #[test]
    fn an_identity_template_is_a_refusal() {
        let err = preview_structural_rewrite(
            RUST_SRC,
            SemanticEditExecutorLanguage::Rust,
            &intent("structural_rewrite", Some("foo($A)"), Some("foo($A)")),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("no-op"), "{err:#}");
    }

    #[test]
    fn output_that_does_not_reparse_is_refused_before_planning() {
        // The rewrite template is raw text, so nothing but the output reparse
        // stops a codemod from writing syntactically broken source.
        let err = preview_structural_rewrite(
            RUST_SRC,
            SemanticEditExecutorLanguage::Rust,
            &intent("structural_rewrite", Some("foo($A)"), Some("bar($A")),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("parse errors"), "{err:#}");
    }

    #[test]
    fn rewrites_a_script_language_through_its_own_grammar() {
        let src = "def main():\n    foo(1)\n    foo(2)\n";
        let (out, replacements) = preview_structural_rewrite(
            src,
            SemanticEditExecutorLanguage::Python,
            &intent("structural_rewrite", Some("foo($A)"), Some("bar($A)")),
        )
        .unwrap();
        assert_eq!(replacements, 2);
        assert!(out.contains("bar(1)") && out.contains("bar(2)"), "{out}");
    }

    #[test]
    fn multibyte_source_survives_the_rewrite() {
        let src = "fn main() {\n    let s = \"héllo → wörld\";\n    foo(s);\n}\n";
        let (out, _) = preview_structural_rewrite(
            src,
            SemanticEditExecutorLanguage::Rust,
            &intent("structural_rewrite", Some("foo($A)"), Some("bar($A)")),
        )
        .unwrap();
        assert!(out.contains("héllo → wörld"), "{out}");
        assert!(out.contains("bar(s)"), "{out}");
    }

    #[test]
    fn every_registered_executor_has_exactly_one_conformance_fixture() {
        // Both directions. Without the first, a language can be registered and
        // never exercised; without the second, a fixture can outlive the
        // executor it claims to cover.
        for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
            let expected = usize::from(
                contract
                    .recognized_intents
                    .contains(&"structural_rewrite"),
            );
            let rows = SEMANTIC_EDIT_EXECUTOR_FIXTURES
                .iter()
                .filter(|fixture| fixture.executor == contract.executor)
                .count();
            assert_eq!(
                rows, expected,
                "executor {} has {rows} structural fixtures, expected exactly {expected}",
                contract.id
            );
        }
        for fixture in SEMANTIC_EDIT_EXECUTOR_FIXTURES {
            assert!(
                SEMANTIC_EDIT_LANGUAGE_CONTRACTS
                    .iter()
                    .any(|contract| contract.executor == fixture.executor),
                "fixture {} has no registered executor contract",
                fixture.alias
            );
        }
        // An empty table would satisfy every loop above vacuously.
        assert!(
            SEMANTIC_EDIT_EXECUTOR_FIXTURES.len() >= 20,
            "the executor conformance table has collapsed to {} rows",
            SEMANTIC_EDIT_EXECUTOR_FIXTURES.len()
        );
    }

    #[test]
    fn every_extractable_executor_has_exactly_one_extraction_fixture() {
        // Same both-directions guard as the other two tables. Registering
        // `extract_function` for a language is a claim that its emitter exists;
        // without this, the claim can be made in the kinds table and never
        // tested, which is exactly how `structural_rewrite` would have shipped
        // for a language with no grammar.
        for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
            let expected = usize::from(contract.recognized_intents.contains(&"extract_function"));
            let rows = SEMANTIC_EDIT_EXTRACTION_FIXTURES
                .iter()
                .filter(|fixture| fixture.executor == contract.executor)
                .count();
            assert_eq!(
                rows, expected,
                "executor {} has {rows} extraction fixtures, expected exactly {expected}",
                contract.id
            );
        }
        for fixture in SEMANTIC_EDIT_EXTRACTION_FIXTURES {
            assert!(
                fixture
                    .executor
                    .recognized_intents()
                    .contains(&"extract_function"),
                "extraction fixture {} covers an executor that does not recognize extract_function",
                fixture.alias
            );
        }
        // Python, the four JS-like grammars, and GDScript. A collapsed table
        // would satisfy both loops vacuously.
        assert_eq!(
            SEMANTIC_EDIT_EXTRACTION_FIXTURES.len(),
            6,
            "the extraction conformance table has {} rows",
            SEMANTIC_EDIT_EXTRACTION_FIXTURES.len()
        );
    }

    #[test]
    fn every_extractable_executor_emits_a_function_and_a_call_that_agree() {
        // The row's `sample_path` resolves the executor on its own, so a
        // fixture cannot be run against a language other than the one it
        // claims — and every assertion below is per-row rather than a summed
        // counter, so a runner that planned nothing cannot report green.
        let mut checked = 0usize;
        for fixture in SEMANTIC_EDIT_EXTRACTION_FIXTURES {
            let resolved = semantic_edit_executor_language("", Path::new(fixture.sample_path))
                .unwrap_or_else(|| {
                    panic!("extraction fixture {} resolves no executor", fixture.alias)
                });
            assert_eq!(
                resolved, fixture.executor,
                "extraction fixture {} resolves to {resolved:?}",
                fixture.alias
            );

            let intent = SemanticEditIntent {
                new_name: Some(fixture.new_name.to_string()),
                start_line: Some(fixture.start_line),
                end_line: Some(fixture.end_line),
                file: Some(PathBuf::from(fixture.sample_path)),
                ..intent("extract_function", None, None)
            };
            let (out, replacements) =
                preview_extract_function(fixture.source, fixture.executor, &intent)
                    .unwrap_or_else(|err| {
                        panic!("extraction fixture {} refused: {err:#}", fixture.alias)
                    });

            assert_eq!(
                replacements, 1,
                "extraction fixture {} planned {replacements} rewrites",
                fixture.alias
            );
            assert!(
                out.contains(fixture.call_site),
                "extraction fixture {} left call site {:?} out of\n{out}",
                fixture.alias,
                fixture.call_site
            );
            for expected in fixture.emitted {
                assert!(
                    out.contains(expected),
                    "extraction fixture {} emitted no {expected:?} in\n{out}",
                    fixture.alias
                );
            }
            assert_eq!(
                out.matches(fixture.hoisted_once).count(),
                1,
                "extraction fixture {} did not move {:?} exactly once:\n{out}",
                fixture.alias,
                fixture.hoisted_once
            );
            // `preview_extract_function` reparses its own output, so reaching
            // here means the result is still the language it started as.
            checked += 1;
        }
        assert_eq!(
            checked,
            SEMANTIC_EDIT_EXTRACTION_FIXTURES.len(),
            "the runner exercised {checked} of {} extraction rows",
            SEMANTIC_EDIT_EXTRACTION_FIXTURES.len()
        );
    }

    #[test]
    fn every_renamable_executor_has_exactly_one_rename_fixture() {
        // The same both-directions guard as the structural table, keyed on the
        // contract rather than on a hand-kept list: a `Lang` brought into the
        // renamable tier with no rename row fails here, which is the only thing
        // standing between "registered as renamable" and "proven to rename the
        // right positions".
        for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
            let expected = usize::from(contract.recognized_intents.contains(&"rename_symbol"));
            let rows = SEMANTIC_EDIT_RENAME_FIXTURES
                .iter()
                .filter(|fixture| fixture.executor == contract.executor)
                .count();
            assert_eq!(
                rows, expected,
                "executor {} has {rows} rename fixtures, expected exactly {expected}",
                contract.id
            );
        }
        for fixture in SEMANTIC_EDIT_RENAME_FIXTURES {
            assert!(
                fixture
                    .executor
                    .recognized_intents()
                    .contains(&"rename_symbol"),
                "rename fixture {} covers an executor that does not recognize rename_symbol",
                fixture.alias
            );
        }
        // Every indexed language except Markdown, which renames headings
        // instead. A collapsed table would satisfy both loops vacuously.
        assert_eq!(
            SEMANTIC_EDIT_RENAME_FIXTURES.len(),
            10,
            "the rename conformance table has {} rows",
            SEMANTIC_EDIT_RENAME_FIXTURES.len()
        );
    }

    #[test]
    fn every_renamable_executor_renames_names_and_leaves_prose_and_data_alone() {
        // The compared count is the sum of what the *planner* returned, not a
        // loop counter, and each row asserts its untouched positions
        // individually: a runner that counted replacements would report green
        // while renaming a comment or a string literal.
        let mut applied = 0usize;
        let mut declared = 0usize;
        for fixture in SEMANTIC_EDIT_RENAME_FIXTURES {
            assert!(
                fixture.expected_replacements > 0,
                "rename fixture {} expects no rename, which proves nothing",
                fixture.alias
            );
            assert!(
                !fixture.untouched.is_empty(),
                "rename fixture {} declares nothing that must survive",
                fixture.alias
            );
            declared += fixture.expected_replacements;
            let path = std::path::PathBuf::from(format!(
                "fixture{}",
                fixture.executor.contract().temp_suffix
            ));
            let target = semantic_edit_symbol_target(fixture.symbol, fixture.symbol_kind);
            let (out, replacements) = preview_semantic_edit_content(
                fixture.source,
                &path,
                fixture.executor.contract().id,
                "rename_symbol",
                &rename_intent(fixture.new_name),
                Some(&target),
                SemanticEditCallRefContext {
                    refs: &[],
                    cross_file_total: 0,
                },
            )
            .unwrap_or_else(|err| panic!("{} rename_symbol failed: {err:#}", fixture.alias));
            assert_eq!(
                replacements, fixture.expected_replacements,
                "{}: renamed {replacements} occurrence(s), expected {}",
                fixture.alias, fixture.expected_replacements
            );
            for renamed in fixture.renamed {
                assert!(
                    out.contains(renamed),
                    "{}: expected {renamed:?} after the rename:\n{out}",
                    fixture.alias
                );
            }
            for untouched in fixture.untouched {
                assert!(
                    out.contains(untouched),
                    "{}: rename rewrote {untouched:?}, which is prose or data:\n{out}",
                    fixture.alias
                );
            }
            applied += replacements;
        }
        assert_eq!(
            applied, declared,
            "the planner renamed {applied} occurrences but the table declares {declared}"
        );
    }

    #[test]
    fn every_renamable_executor_refuses_a_name_it_cannot_find() {
        // A rename that quietly plans an empty edit is the failure mode that
        // survives review, so the refusal belongs to every executor's contract.
        for fixture in SEMANTIC_EDIT_RENAME_FIXTURES {
            let path = std::path::PathBuf::from(format!(
                "fixture{}",
                fixture.executor.contract().temp_suffix
            ));
            let target = semantic_edit_symbol_target("zzNoSuchSymbolzz", fixture.symbol_kind);
            let err = preview_semantic_edit_content(
                fixture.source,
                &path,
                fixture.executor.contract().id,
                "rename_symbol",
                &rename_intent("qqReplacementqq"),
                Some(&target),
                SemanticEditCallRefContext {
                    refs: &[],
                    cross_file_total: 0,
                },
            )
            .unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains("was not found as a whole"),
                "{}: expected a refusal, got {message}",
                fixture.alias
            );
        }
    }

    #[test]
    fn indexed_executors_refuse_the_kinds_they_have_no_rewriting_for() {
        // The family split routes anything that is neither markdown, script,
        // nor indexed-generic to the Rust implementations. `rename_symbol` is
        // the only symbol-resolved kind this tier implements; the rest must be
        // refused by name rather than reach Rust's rewriting rules.
        for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
            if contract.family != SemanticEditLanguageFamily::Indexed {
                continue;
            }
            let fixture = SEMANTIC_EDIT_RENAME_FIXTURES
                .iter()
                .find(|fixture| fixture.executor == contract.executor)
                .expect("checked by the rename fixture exhaustiveness guard");
            let path = std::path::PathBuf::from(format!("fixture{}", contract.temp_suffix));
            for kind in ["replace_function_body", "insert_import", "add_method"] {
                let err = preview_semantic_edit_content(
                    fixture.source,
                    &path,
                    contract.id,
                    kind,
                    &intent("structural_rewrite", None, Some("whatever")),
                    None,
                    SemanticEditCallRefContext {
                        refs: &[],
                        cross_file_total: 0,
                    },
                )
                .unwrap_err();
                let message = format!("{err:#}");
                assert!(
                    message.contains(contract.name) && message.contains("not supported"),
                    "executor {} should refuse {kind} by name, got {message}",
                    contract.id
                );
            }
        }
    }

    #[test]
    fn an_indexed_executor_without_an_ast_grep_grammar_refuses_structural_rewrite() {
        // Zig and GDScript are renamable because renaming needs a grammar and
        // an index, which they have. They are *not* structurally matchable,
        // because this build compiles no ast-grep grammar for them. Advertising
        // `structural_rewrite` there would plan an edit that can only fail.
        for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
            if contract.executor.ast_grep_lang().is_some() {
                continue;
            }
            // The invariant is about `structural_rewrite` specifically, not
            // about the whole recognized set: GDScript also has an extraction
            // emitter, which needs a tree-sitter grammar and no ast-grep one.
            // Comparing whole tables here would tie two unrelated facts
            // together and fail the next time either moves on its own.
            assert!(
                !contract
                    .recognized_intents
                    .contains(&"structural_rewrite"),
                "executor {} has no ast-grep grammar but advertises {:?}",
                contract.id,
                contract.recognized_intents
            );
            let path = std::path::PathBuf::from(format!("fixture{}", contract.temp_suffix));
            let err = preview_semantic_edit_content(
                "",
                &path,
                contract.id,
                "structural_rewrite",
                &intent("structural_rewrite", Some("foo($A)"), Some("bar($A)")),
                None,
                SemanticEditCallRefContext {
                    refs: &[],
                    cross_file_total: 0,
                },
            )
            .unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(contract.name) && message.contains("not supported"),
                "executor {} should refuse structural_rewrite by name, got {message}",
                contract.id
            );
        }
    }

    #[test]
    fn a_rename_family_covers_only_languages_that_can_reference_each_other() {
        // Call edges are matched by name, so the rename scope is the only thing
        // stopping a Zig `deploy` from blocking a rename of a Bash `deploy` —
        // the same defect that once had a Python `beta` block a JavaScript one.
        let family = |path: &str| {
            semantic_edit_rename_family(std::path::Path::new(path))
                .expect("fixture path resolves an executor")
        };
        assert_eq!(family("app.ts"), family("app.js"));
        assert_eq!(family("App.tsx"), family("view.jsx"));
        for (left, right) in [
            ("deploy.sh", "main.zig"),
            ("deploy.sh", "player.gd"),
            ("main.zig", "player.gd"),
            ("deploy.sh", "Main.kt"),
            ("app.js", "script.py"),
            ("main.zig", "lib.rs"),
        ] {
            assert_ne!(
                family(left),
                family(right),
                "{left} and {right} share a rename family and cannot reference each other"
            );
        }
    }

    #[test]
    fn every_executor_rewrites_and_reparses_its_own_language() {
        // The count compared at the end is the sum of what the *planner*
        // returned, not a loop counter: a runner that summed its own
        // bookkeeping would report green over a table that rewrote nothing.
        let mut applied = 0usize;
        let mut declared = 0usize;
        for fixture in SEMANTIC_EDIT_EXECUTOR_FIXTURES {
            assert!(
                fixture.expected_replacements > 0,
                "fixture {} expects no rewrite, which proves nothing",
                fixture.alias
            );
            declared += fixture.expected_replacements;
            let (out, replacements) = preview_structural_rewrite(
                fixture.source,
                fixture.executor,
                &intent(
                    "structural_rewrite",
                    Some(fixture.pattern),
                    Some(fixture.replacement),
                ),
            )
            .unwrap_or_else(|err| {
                panic!("{} structural_rewrite failed: {err:#}", fixture.alias)
            });
            assert_eq!(
                replacements, fixture.expected_replacements,
                "{}: rewrote {replacements} match(es), expected {}",
                fixture.alias, fixture.expected_replacements
            );
            assert!(
                out.contains(fixture.marker),
                "{}: rewritten buffer is missing {:?}:\n{out}",
                fixture.alias,
                fixture.marker
            );
            assert_ne!(out, fixture.source, "{}: buffer is unchanged", fixture.alias);
            applied += replacements;
        }
        assert_eq!(
            applied, declared,
            "the planner applied {applied} rewrites but the table declares {declared}"
        );
    }

    #[test]
    fn every_executor_resolves_a_reparse_grammar() {
        // `structural_rewrite` reparses both its input and its output. An
        // executor that cannot resolve a grammar would refuse every plan, and
        // one registered with the *wrong* grammar would validate a file against
        // another language's rules — which is worse, because it looks like it
        // worked.
        for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
            // Structural fixture where there is one; otherwise the rename row,
            // which is the only fixture an executor with no ast-grep grammar
            // has. Every contract has at least one of the two — asserted here
            // rather than in the exhaustiveness guards, which each see only
            // their own table.
            let source = SEMANTIC_EDIT_EXECUTOR_FIXTURES
                .iter()
                .find(|fixture| fixture.executor == contract.executor)
                .map(|fixture| fixture.source)
                .or_else(|| {
                    SEMANTIC_EDIT_RENAME_FIXTURES
                        .iter()
                        .find(|fixture| fixture.executor == contract.executor)
                        .map(|fixture| fixture.source)
                })
                .unwrap_or_else(|| {
                    panic!("executor {} has no conformance fixture at all", contract.id)
                });
            let language = contract.executor.reparse_language().unwrap_or_else(|err| {
                panic!("executor {} has no reparse grammar: {err:#}", contract.id)
            });
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&language).unwrap();
            let tree = parser.parse(source, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "executor {} cannot parse its own fixture with its reparse grammar",
                contract.id
            );
        }
    }

    #[test]
    fn an_indexed_executor_reparses_with_the_grammar_ast_grep_matched_with() {
        // `reparse_language` prefers the `tsift-graph` binding, but a
        // structural rewrite is *matched* with the ast-grep grammar. Those two
        // agree today for every indexed executor that has both — even Kotlin,
        // where ast-grep's `tree-sitter-kotlin` feature resolves to the same
        // `-ng` grammar tsift-graph uses. That agreement is load-bearing and
        // invisible: if a grammar bump ever splits them, the planner would
        // validate its output against a grammar the matcher never used and the
        // indexer will not use either. This fails at that moment rather than
        // silently accepting the mismatch. Zig and GDScript are skipped because
        // they have no second grammar to disagree with.
        for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
            let Some(graph_lang) = contract.graph_lang else {
                continue;
            };
            if !contract.recognized_intents.contains(&"structural_rewrite") {
                continue;
            }
            let indexed = graph_lang.tree_sitter_language();
            let matched = contract
                .executor
                .ast_grep_lang()
                .expect("checked by the ast-grep grammar drift guard")
                .tree_sitter_language();
            assert_eq!(
                indexed.abi_version(),
                matched.abi_version(),
                "executor {} indexes and matches with different grammar ABIs",
                contract.id
            );
            assert_eq!(
                indexed.node_kind_count(),
                matched.node_kind_count(),
                "executor {} indexes and matches with different grammars",
                contract.id
            );
            for id in 0..indexed.node_kind_count() as u16 {
                assert_eq!(
                    indexed.node_kind_for_id(id),
                    matched.node_kind_for_id(id),
                    "executor {} grammars disagree on node kind {id}",
                    contract.id
                );
            }
        }
    }

    #[test]
    fn every_executor_refuses_a_pattern_that_matched_nothing() {
        // A structural language that quietly plans an empty edit is the one
        // failure mode that survives review, so the refusal is part of every
        // executor's contract rather than of the Rust path alone.
        for fixture in SEMANTIC_EDIT_EXECUTOR_FIXTURES {
            let err = preview_structural_rewrite(
                fixture.source,
                fixture.executor,
                &intent(
                    "structural_rewrite",
                    Some("zzNoSuchSymbolzz($A)"),
                    Some("qqReplacementqq($A)"),
                ),
            )
            .unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains("matched nothing") || message.contains("invalid structural pattern"),
                "{}: expected a refusal, got {message}",
                fixture.alias
            );
        }
    }

    #[test]
    fn every_structural_executor_refuses_the_symbol_resolved_kinds() {
        // The family split routes anything that is neither markdown nor script
        // to the Rust implementations, so an unrecognized kind must be refused
        // before it reaches another language's rewriting rules.
        for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
            if contract.family != SemanticEditLanguageFamily::Structural {
                continue;
            }
            assert_eq!(
                contract.recognized_intents, SEMANTIC_EDIT_STRUCTURAL_KINDS,
                "executor {} advertises more than structural rewriting",
                contract.id
            );
            let fixture = SEMANTIC_EDIT_EXECUTOR_FIXTURES
                .iter()
                .find(|fixture| fixture.executor == contract.executor)
                .expect("checked by the fixture exhaustiveness guard");
            let path = std::path::PathBuf::from(fixture.sample_path);
            for kind in ["rename_symbol", "replace_function_body", "insert_import"] {
                let err = preview_semantic_edit_content(
                    fixture.source,
                    &path,
                    contract.id,
                    kind,
                    &intent("structural_rewrite", None, Some("whatever")),
                    None,
                    SemanticEditCallRefContext {
                        refs: &[],
                        cross_file_total: 0,
                    },
                )
                .unwrap_err();
                let message = format!("{err:#}");
                assert!(
                    message.contains(contract.name) && message.contains("not supported"),
                    "executor {} should refuse {kind} by name, got {message}",
                    contract.id
                );
            }
        }
    }

    #[test]
    fn every_executor_language_resolves_an_ast_grep_grammar() {
        // An executor that advertises structural_rewrite must actually have a
        // grammar. A new executor language whose id ast-grep cannot resolve
        // would otherwise only fail at plan time, in the field. The converse —
        // a grammar-less executor that advertises it anyway — is caught by
        // `an_indexed_executor_without_an_ast_grep_grammar_refuses_structural_rewrite`.
        let mut with_grammar = 0usize;
        for contract in SEMANTIC_EDIT_LANGUAGE_CONTRACTS {
            if !contract.recognized_intents.contains(&"structural_rewrite") {
                continue;
            }
            with_grammar += 1;
            assert!(
                contract.executor.ast_grep_lang().is_some(),
                "executor {} advertises structural_rewrite but has no ast-grep grammar",
                contract.id
            );
            assert!(
                contract
                    .apply_supported_intents
                    .contains(&"structural_rewrite"),
                "executor {} should apply-support structural_rewrite",
                contract.id
            );
        }
        assert!(
            with_grammar >= 20,
            "only {with_grammar} executors advertise structural_rewrite"
        );
    }

    #[test]
    fn pattern_is_required_here_and_rejected_everywhere_else() {
        let missing = intent("structural_rewrite", None, Some("bar($A)"));
        let err = validate_semantic_edit_intent("structural_rewrite", &missing).unwrap_err();
        assert!(format!("{err:#}").contains("requires `pattern`"), "{err:#}");

        let stray = intent("insert_import", Some("foo($A)"), Some("std::fmt"));
        let err = validate_semantic_edit_intent("insert_import", &stray).unwrap_err();
        assert!(
            format!("{err:#}").contains("does not support `pattern`"),
            "{err:#}"
        );

        let ok = intent("structural_rewrite", Some("foo($A)"), Some("bar($A)"));
        validate_semantic_edit_intent("structural_rewrite", &ok).unwrap();
    }

    #[test]
    fn structural_rewrite_requires_a_file_and_no_symbol() {
        assert!(semantic_edit_kind_requires_file("structural_rewrite"));
        assert!(!semantic_edit_kind_requires_symbol("structural_rewrite"));
        assert!(semantic_edit_kind_requires_replacement("structural_rewrite"));
        let mut no_file = intent("structural_rewrite", Some("foo($A)"), Some("bar($A)"));
        no_file.file = None;
        let err = validate_semantic_edit_intent("structural_rewrite", &no_file).unwrap_err();
        assert!(format!("{err:#}").contains("requires `file`"), "{err:#}");
    }
}
