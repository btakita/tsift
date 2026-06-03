use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::{Builder as TempFileBuilder, NamedTempFile, TempDir};
use tsift_graph as graph;
use tsift_index::index;
use tsift_quality::lint;
use tsift_search::impact;
use tree_sitter::StreamingIterator as _;

use crate::output::{OutputFormat, ResponseBudget, ToolEnvelopeSummary};
use crate::{
    envelope_metric, markdown_ast_projection, print_json_or_envelope, relativize_pathbuf,
    resolve_query_db_path, resolve_source_file, shell_quote, source_read_command,
    stable_handle, symbol_hit_ast_span, symbol_hit_end_line, symbol_hit_line,
    truncate_for_budget, SourceRangePreview,
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
    pub(crate) symbol: Option<String>,
    #[serde(default)]
    pub(crate) file: Option<PathBuf>,
    #[serde(default)]
    pub(crate) destination_symbol: Option<String>,
    #[serde(default)]
    pub(crate) position: Option<String>,
    #[serde(default)]
    pub(crate) replacement: Option<String>,
    #[serde(default)]
    pub(crate) call_replacement: Option<String>,
    #[serde(default)]
    pub(crate) new_name: Option<String>,
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
    pub(crate) content_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) diff: Option<String>,
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
];
pub(crate) const SEMANTIC_EDIT_SCRIPT_KINDS: &[&str] =
    &["rename_symbol", "replace_function_body", "insert_import"];
pub(crate) const SEMANTIC_EDIT_MARKDOWN_KINDS: &[&str] = &[
    "rename_heading",
    "replace_section_body",
    "insert_section",
    "move_section",
    "insert_list_item",
    "rewrite_code_fence",
];
pub(crate) const SEMANTIC_EDIT_MARKDOWN_APPLY_KINDS: &[&str] = &[
    "rename_heading",
    "replace_section_body",
    "insert_section",
    "move_section",
    "insert_list_item",
    "rewrite_code_fence",
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
    )
}

fn semantic_edit_kind_requires_new_name(kind: &str) -> bool {
    matches!(kind, "rename_symbol" | "rename_heading")
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
    )
}

fn validate_semantic_edit_intent(kind: &str, intent: &SemanticEditIntent) -> Result<()> {
    if !SEMANTIC_EDIT_KINDS.contains(&kind) {
        bail!(
            "unknown semantic edit kind {kind:?}; expected one of {}",
            SEMANTIC_EDIT_KINDS.join(", ")
        );
    }
    if semantic_edit_kind_requires_symbol(kind)
        && intent.symbol.as_deref().is_none_or(str::is_empty)
    {
        bail!("semantic edit kind {kind:?} requires `symbol`");
    }
    if semantic_edit_kind_requires_file(kind) && intent.file.is_none() {
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SemanticEditLanguageFamily {
    Rust,
    Python,
    JsLike,
    Markdown,
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
    pub(crate) graph_lang: graph::Lang,
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
        graph_lang: graph::Lang::Rust,
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
        graph_lang: graph::Lang::Python,
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
        graph_lang: graph::Lang::TypeScript,
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
        graph_lang: graph::Lang::Tsx,
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
        graph_lang: graph::Lang::JavaScript,
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
        graph_lang: graph::Lang::Jsx,
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
        graph_lang: graph::Lang::Markdown,
        temp_suffix: ".md",
        aliases: &["markdown", "md", "mdx"],
        extensions: &["md", "mdx"],
        recognized_intents: SEMANTIC_EDIT_MARKDOWN_KINDS,
        apply_supported_intents: SEMANTIC_EDIT_MARKDOWN_APPLY_KINDS,
        family: SemanticEditLanguageFamily::Markdown,
        formatter: SemanticEditFormatterContract::None,
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

    fn graph_lang(self) -> graph::Lang {
        self.contract().graph_lang
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

    fn is_script(self) -> bool {
        matches!(
            self.contract().family,
            SemanticEditLanguageFamily::Python | SemanticEditLanguageFamily::JsLike
        )
    }

    fn is_markdown(self) -> bool {
        self.contract().family == SemanticEditLanguageFamily::Markdown
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
    let cases = [
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
    ];

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

fn replace_rust_identifier(content: &str, old: &str, new: &str) -> Result<(String, usize)> {
    validate_rust_identifier(old, "symbol")?;
    validate_rust_identifier(new, "new_name")?;
    if old == new {
        bail!("old and new identifiers are identical");
    }

    let mut out = String::with_capacity(content.len());
    let mut last = 0;
    let mut replacements = 0;
    for (idx, _) in content.match_indices(old) {
        let before_is_ident = content[..idx]
            .chars()
            .next_back()
            .is_some_and(rust_ident_char);
        let after_idx = idx + old.len();
        let after_is_ident = content[after_idx..]
            .chars()
            .next()
            .is_some_and(rust_ident_char);
        if before_is_ident || after_is_ident {
            continue;
        }
        out.push_str(&content[last..idx]);
        out.push_str(new);
        last = after_idx;
        replacements += 1;
    }
    if replacements == 0 {
        bail!("identifier {old:?} was not found as a whole Rust identifier");
    }
    out.push_str(&content[last..]);
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
    let language = executor.graph_lang().tree_sitter_language();
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
) -> Result<(String, usize)> {
    validate_script_identifier(old, "symbol", executor)?;
    validate_script_identifier(new, "new_name", executor)?;
    if old == new {
        bail!("old and new identifiers are identical");
    }
    parse_semantic_edit_source(content, executor, "rename_symbol input")?;

    let mut out = String::with_capacity(content.len());
    let mut last = 0usize;
    let mut replacements = 0usize;
    for (idx, _) in content.match_indices(old) {
        let before_is_ident = content[..idx]
            .chars()
            .next_back()
            .is_some_and(|ch| script_ident_char(ch, executor));
        let after_idx = idx + old.len();
        let after_is_ident = content[after_idx..]
            .chars()
            .next()
            .is_some_and(|ch| script_ident_char(ch, executor));
        if before_is_ident || after_is_ident {
            continue;
        }
        out.push_str(&content[last..idx]);
        out.push_str(new);
        last = after_idx;
        replacements += 1;
    }
    if replacements == 0 {
        bail!(
            "identifier {old:?} was not found as a whole {} identifier",
            executor.name()
        );
    }
    out.push_str(&content[last..]);
    parse_semantic_edit_source(&out, executor, "rename_symbol")?;
    Ok((out, replacements))
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
    let language = executor.graph_lang().tree_sitter_language();
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

fn find_matching_rust_brace(content: &str, open_idx: usize) -> Result<usize> {
    let mut depth = 0usize;
    for (idx, ch) in content[open_idx..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(open_idx + idx);
                }
            }
            _ => {}
        }
    }
    bail!("could not find matching Rust function body brace")
}

fn find_rust_function_open_brace(content: &str, name: &str) -> Result<usize> {
    validate_rust_identifier(name, "symbol")?;
    let needle = format!("fn {name}");
    let mut search_start = 0;
    while let Some(relative) = content[search_start..].find(&needle) {
        let start = search_start + relative;
        let name_end = start + needle.len();
        let after_name_is_ident = content[name_end..]
            .chars()
            .next()
            .is_some_and(rust_ident_char);
        if !after_name_is_ident && let Some(brace_relative) = content[name_end..].find('{') {
            return Ok(name_end + brace_relative);
        }
        search_start = name_end;
    }
    bail!("could not find Rust function {name:?}")
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

fn replace_rust_function_body(
    content: &str,
    name: &str,
    replacement: &str,
) -> Result<(String, usize)> {
    let open_idx = find_rust_function_open_brace(content, name)?;
    let close_idx = find_matching_rust_brace(content, open_idx)?;
    let base_indent = line_indent_at(content, open_idx);
    let mut out = String::with_capacity(content.len() + replacement.len());
    out.push_str(&content[..=open_idx]);
    out.push_str(&rust_function_body_replacement(replacement, &base_indent));
    out.push_str(&content[close_idx..]);
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

fn insert_rust_import(content: &str, replacement: &str) -> Result<(String, usize)> {
    let import = normalize_rust_import(replacement)?;
    if content.lines().any(|line| line.trim() == import) {
        return Ok((content.to_string(), 0));
    }

    let mut offset = 0usize;
    let mut insert_at = 0usize;
    for line in content.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("use ")
            || trimmed.starts_with("pub use ")
            || trimmed.starts_with("extern crate ")
            || (insert_at == 0 && (trimmed.is_empty() || trimmed.starts_with("#!")))
        {
            insert_at = offset + line.len();
            offset += line.len();
            continue;
        }
        break;
    }

    let mut out = String::with_capacity(content.len() + import.len() + 1);
    out.push_str(&content[..insert_at]);
    out.push_str(&import);
    out.push('\n');
    out.push_str(&content[insert_at..]);
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

pub(crate) fn markdown_block_spans(content: &str, kind: &str) -> Result<Vec<MarkdownBlockEditSpan>> {
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
    if executor.is_markdown() {
        return preview_markdown_edit_content(content, kind, intent, target_symbol);
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
        ),
        "replace_function_body" => replace_rust_function_body(
            content,
            target_symbol_name(target_symbol, kind)?,
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

pub(crate) fn semantic_edit_diff_preview(before: &str, after: &str, budget: ResponseBudget) -> Option<String> {
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

    let before_start = prefix.saturating_sub(2);
    let after_start = before_start;
    let before_end = before_lines.len().saturating_sub(suffix).min(prefix + 8);
    let after_end = after_lines.len().saturating_sub(suffix).min(prefix + 8);
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
    Some(truncate_for_budget(
        &lines.join("\n"),
        budget.preview_bytes(),
    ))
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
    let (mut target_symbol, file_abs, mut target_range) = if let Some(symbol) =
        intent.symbol.as_deref()
    {
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
    let conflict = intent
        .expected_content_hash
        .as_deref()
        .is_some_and(|expected| expected != content_hash);

    let language = semantic_edit_target_language(target_symbol.as_ref(), &file_abs);
    let (status, apply_supported, diff, message) = if conflict {
        (
            "conflict".to_string(),
            semantic_edit_kind_apply_supported(&kind, &language, &file_abs),
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
                            let mut diff_parts = Vec::new();
                            if let Some(source_diff) =
                                semantic_edit_diff_preview(&source_text, &source_preview, budget)
                            {
                                diff_parts.push(format!("{target_file}\n{source_diff}"));
                            }
                            if let Some(destination_file) = &destination_file
                                && let Some(destination_diff) = semantic_edit_diff_preview(
                                    &destination_text,
                                    &destination_preview,
                                    budget,
                                )
                            {
                                diff_parts.push(format!("{destination_file}\n{destination_diff}"));
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
                                "validated move_declaration intent; Rust executor can apply this edit"
                                    .to_string(),
                            )
                        }
                        Err(err) => (
                            "unsupported".to_string(),
                            false,
                            None,
                            format!(
                                "move_declaration intent is not applyable by the current executor: {err:#}"
                            ),
                        ),
                    }
                } else {
                    match preview_semantic_edit_content(
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
                    ) {
                        Ok((preview, _)) => (
                            "planned".to_string(),
                            true,
                            semantic_edit_diff_preview(&source_text, &preview, budget),
                            format!(
                                "validated {kind} intent; {} executor can apply this edit",
                                semantic_edit_executor_name(&language, &file_abs)
                            ),
                        ),
                        Err(err) => (
                            "unsupported".to_string(),
                            false,
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
            target_symbol,
            call_refs,
            cross_file_call_ref_total: (cross_file_call_ref_total > 0)
                .then_some(cross_file_call_ref_total),
            target_file,
            destination_file,
            target_range,
            content_hash,
            diff,
            formatter: None,
            message: truncate_for_budget(&message, budget.preview_bytes()),
        },
        file_abs,
        destination_file_abs,
        language,
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
    let preview_lines = report["preview"].as_array().map_or(0, |lines| lines.len());
    let symbol_refs = report["symbols"]
        .as_array()
        .map_or(0, |symbols| symbols.len());
    let summary_refs = report["summaries"]
        .as_array()
        .map_or(0, |summaries| summaries.len());
    Ok(SemanticEditVerificationSourceRead {
        file: file.to_string(),
        start,
        lines,
        preview_lines,
        symbol_refs,
        summary_refs,
        command: format!(
            "tsift --envelope source-read {} --path {} --start {} --lines {} --budget normal",
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
                println!("    range: {}-{}", range.start, range.end);
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
