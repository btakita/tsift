use anyhow::Result;
use lazily::{Computed, Context as LazyContext, Source};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tsift_index::config;
use tsift_index::index::{
    IndexDb, ReadOnlyInspectResult, WriterLockProbe, probe_writer_lock, writer_lock_path,
};
use tsift_index::init::{self, InstructionStatus};
use tsift_sqlite::{
    ReadOnlyRecovery, rollback_journal_path, shared_memory_sidecar_path, wal_sidecar_path,
};
use tsift_summarize::summarize::SummaryDb;

type CachedInspectResult = std::result::Result<ReadOnlyInspectResult, String>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StatusInspectKey {
    db_path: PathBuf,
    root: PathBuf,
    prune: bool,
}

pub struct StatusCheckCache {
    ctx: LazyContext,
    epoch: Source<u64>,
    inspect_slots: RefCell<HashMap<StatusInspectKey, Computed<CachedInspectResult>>>,
}

impl Default for StatusCheckCache {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusCheckCache {
    pub fn new() -> Self {
        let ctx = LazyContext::new();
        let epoch = ctx.source(0u64);
        Self {
            ctx,
            epoch,
            inspect_slots: RefCell::new(HashMap::new()),
        }
    }

    pub fn invalidate_all(&self) {
        let epoch = self.ctx.get(&self.epoch);
        self.ctx.set(&self.epoch, epoch.wrapping_add(1));
    }

    fn inspect_read_only(
        &self,
        db_path: &Path,
        root: &Path,
        prune: bool,
    ) -> Result<ReadOnlyInspectResult> {
        let key = StatusInspectKey {
            db_path: db_path.to_path_buf(),
            root: root.to_path_buf(),
            prune,
        };
        let slot = {
            let mut slots = self.inspect_slots.borrow_mut();
            if let Some(slot) = slots.get(&key) {
                *slot
            } else {
                let slot_key = key.clone();
                let epoch = self.epoch;
                let slot = self.ctx.slot(move |ctx| {
                    let _epoch = ctx.get(&epoch);
                    IndexDb::inspect_read_only(&slot_key.db_path, &slot_key.root, slot_key.prune)
                        .map_err(|err| format!("{err:#}"))
                });
                slots.insert(key, slot);
                slot
            }
        };
        self.ctx
            .get(&slot)
            .map_err(|message| anyhow::anyhow!("{message}"))
    }
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub index: IndexStatus,
    pub summaries: SummaryStatus,
    pub instructions: InstructionStatus,
    /// Per-scope instruction state (`#wsinit`). Index freshness is already
    /// reported per scope; collapsing six scopes into one `instructions:` line
    /// hid submodules stuck on releases-old text — the very files AGENTS.md
    /// tells an agent to prefer.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub scope_instructions: Vec<ScopeInstructionStatus>,
    pub recommendations: Recommendations,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub reminders: Vec<String>,
    /// Scopes where the walk dropped a meaningful share of the files it saw
    /// (`#goindex`). Without this, a scope indexing 8 of its 26 tracked files
    /// still printed `fresh`, and the gap surfaced only as confident empty
    /// search results.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub language_coverage: Vec<LanguageCoverageGap>,
}

#[derive(Debug, Serialize)]
pub struct ScopeInstructionStatus {
    pub scope: String,
    pub instructions: InstructionStatus,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LanguageCoverageGap {
    /// `None` for a single-root index; the scope id in a workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub indexed_files: usize,
    pub skipped_files: usize,
    /// The extension that cost the most files, and its count.
    pub dominant_extension: String,
    pub dominant_extension_files: usize,
    /// Every skipped extension, most-costly first.
    pub skipped_by_extension: Vec<(String, usize)>,
}

impl LanguageCoverageGap {
    /// A gap worth printing: the skipped files are a real share of the walk, and
    /// the dominant skipped extension is not a rounding error. A repo with two
    /// stray `.txt` files next to 600 indexed sources is not a coverage gap.
    fn is_reportable(&self) -> bool {
        let walked = self.indexed_files + self.skipped_files;
        walked > 0 && self.dominant_extension_files >= 3 && self.skipped_files * 4 >= walked
    }

    fn from_summary(
        scope: Option<String>,
        indexed_files: usize,
        skipped: &tsift_index::walk::SkipStats,
    ) -> Option<Self> {
        let (dominant_extension, dominant_extension_files) = skipped.dominant_extension()?;
        let gap = Self {
            scope,
            indexed_files,
            skipped_files: skipped.files,
            dominant_extension: dominant_extension.to_string(),
            dominant_extension_files,
            skipped_by_extension: skipped
                .ranked_extensions()
                .into_iter()
                .map(|(ext, count)| (ext.to_string(), count))
                .collect(),
        };
        gap.is_reportable().then_some(gap)
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "state")]
pub enum IndexStatus {
    #[serde(rename = "fresh")]
    Fresh {
        total_files: usize,
        stale_files: usize,
        last_indexed_secs_ago: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        recovery: Option<ReadOnlyRecovery>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        workspace_scopes: Vec<WorkspaceScopeStatus>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        missing_scopes: Vec<MissingWorkspaceScopeStatus>,
    },
    #[serde(rename = "stale")]
    Stale {
        total_files: usize,
        stale_files: usize,
        last_indexed_secs_ago: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        recovery: Option<ReadOnlyRecovery>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        workspace_scopes: Vec<WorkspaceScopeStatus>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        missing_scopes: Vec<MissingWorkspaceScopeStatus>,
    },
    #[serde(rename = "missing")]
    Missing {
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        missing_scopes: Vec<MissingWorkspaceScopeStatus>,
    },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct WorkspaceScopeStatus {
    pub scope: String,
    pub db_path: PathBuf,
    pub total_files: usize,
    pub stale_files: usize,
    pub last_indexed_secs_ago: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ReadOnlyRecovery>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MissingWorkspaceScopeStatus {
    pub scope: String,
    pub db_path: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(tag = "state")]
pub enum SummaryStatus {
    #[serde(rename = "available")]
    Available {
        cached_files: usize,
        total_indexed_files: usize,
        terminal_failure_files: usize,
        non_candidate_files: usize,
        coverage_pct: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        recovery: Option<ReadOnlyRecovery>,
    },
    #[serde(rename = "none")]
    None {
        #[serde(skip_serializing_if = "Option::is_none")]
        recovery: Option<ReadOnlyRecovery>,
    },
    #[serde(rename = "unavailable")]
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct Recommendations {
    #[serde(rename = "use")]
    pub use_commands: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LockReport {
    pub label: String,
    pub source_root: PathBuf,
    pub db_path: PathBuf,
    pub writer_lock: WriterLockStatus,
    pub rollback_journal: SidecarStatus,
    pub wal_sidecar: SidecarStatus,
    pub shared_memory_sidecar: SidecarStatus,
    pub reindex_command: String,
    pub recommended_action: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WriterLockStatus {
    Absent { path: PathBuf },
    Live { path: PathBuf, pid: Option<u32> },
    Stale { path: PathBuf, pid: Option<u32> },
    Unknown { path: PathBuf },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SidecarStatus {
    pub path: PathBuf,
    pub present: bool,
}

pub fn check_status(root: &Path) -> Result<StatusReport> {
    let cache = StatusCheckCache::new();
    check_status_with_cache(root, &cache)
}

pub fn check_status_with_cache(root: &Path, cache: &StatusCheckCache) -> Result<StatusReport> {
    let workspace_discovery = config::Config::workspace_discovery(root)?;
    let workspace_scopes = workspace_discovery.scopes;
    let workspace = !workspace_scopes.is_empty();
    let summaries_db_path = root.join(".tsift/summaries.db");

    let index = check_index(root, cache)?;
    let summaries = check_summaries(root, &summaries_db_path, &index, cache)?;
    let summarize_extract = recommended_summarize_extract_path(root, &index, &workspace_scopes);
    let instructions = init::check_instruction_version(root);
    let scope_instructions = collect_scope_instructions(root, &workspace_scopes)?;
    let kg_present = root.join(".tsift/graph.db").exists();
    let recommendations = build_recommendations(
        &index,
        &summaries,
        &instructions,
        &scope_instructions,
        workspace,
        &summarize_extract,
        kg_present,
    );
    let mut reminders = build_reminders(&index, &summaries, &recommendations, &summarize_extract);
    if !workspace_discovery.unresolvable.is_empty() {
        let details = workspace_discovery
            .unresolvable
            .iter()
            .map(|scope| format!("{} — no gitlink and path absent", scope.relative_path))
            .collect::<Vec<_>>()
            .join(", ");
        let count = workspace_discovery.unresolvable.len();
        let resolution = if workspace {
            "ignored stale `.gitmodules` declaration"
        } else if matches!(index, IndexStatus::Missing { .. }) {
            "run `tsift index .` to index the root tree instead; remove stale config with `git rm .gitmodules`"
        } else {
            "using the root index instead; remove stale config with `git rm .gitmodules`"
        };
        reminders.insert(
            0,
            format!(
                "workspace: {count} declared scope{} unresolvable ({details}); {resolution}",
                if count == 1 { "" } else { "s" }
            ),
        );
    }
    let language_coverage = collect_language_coverage_gaps(root, cache)?;

    Ok(StatusReport {
        index,
        summaries,
        instructions,
        scope_instructions,
        recommendations,
        reminders,
        language_coverage,
    })
}

/// Instruction state for each workspace scope (`#wsinit`).
///
/// Scopes that opt out with `instructions = false` are omitted: an opted-out
/// scope has no expected block, so reporting it as `missing` would be noise.
fn collect_scope_instructions(
    root: &Path,
    workspace_scopes: &[config::WorkspaceScope],
) -> Result<Vec<ScopeInstructionStatus>> {
    if workspace_scopes.is_empty() {
        return Ok(Vec::new());
    }
    let cfg = config::Config::load(root)?;
    let mut statuses = Vec::new();
    for scope in workspace_scopes {
        if !scope.source_root.exists() || !cfg.instructions_for_scope(scope) {
            continue;
        }
        statuses.push(ScopeInstructionStatus {
            scope: scope.id.clone(),
            instructions: init::check_instruction_version(&scope.source_root),
        });
    }
    statuses.sort_by(|left, right| left.scope.cmp(&right.scope));
    Ok(statuses)
}

fn scope_instruction_label(status: &InstructionStatus) -> String {
    match status {
        InstructionStatus::Current { version } => format!("current (v{version})"),
        InstructionStatus::Stale {
            found: Some(found), ..
        } => format!("stale (v{found})"),
        InstructionStatus::Stale { found: None, .. } => "stale (pre-versioned)".to_string(),
        InstructionStatus::Missing => "missing".to_string(),
    }
}

/// Re-walk each indexed scope's source root through the same cached inspection
/// `check_index` uses, and report the scopes whose walk dropped a meaningful
/// share of files for want of an indexer language (`#goindex`).
fn collect_language_coverage_gaps(
    root: &Path,
    cache: &StatusCheckCache,
) -> Result<Vec<LanguageCoverageGap>> {
    let scopes = config::Config::submodule_dirs(root)?;
    let mut gaps = Vec::new();

    if scopes.is_empty() {
        let db_path = root.join(".tsift/index.db");
        if db_path.exists()
            && let Ok(inspection) = cache.inspect_read_only(&db_path, root, false)
            && let Some(gap) = LanguageCoverageGap::from_summary(
                None,
                inspection.total_files,
                &inspection.summary.skipped,
            )
        {
            gaps.push(gap);
        }
        return Ok(gaps);
    }

    let cfg = config::Config::load(root)?;
    let root_db_path = root.join(".tsift/index.db");
    let excluded_roots = scopes
        .iter()
        .map(|scope| scope.source_root.clone())
        .collect::<Vec<_>>();
    if root_db_path.exists()
        && let Ok(inspection) =
            IndexDb::inspect_read_only_excluding(&root_db_path, root, false, &excluded_roots)
        && let Some(gap) = LanguageCoverageGap::from_summary(
            Some(config::WORKSPACE_ROOT_SCOPE_ID.to_string()),
            inspection.total_files,
            &inspection.summary.skipped,
        )
    {
        gaps.push(gap);
    }
    for scope in scopes {
        let db_path = cfg.db_path_for(root, &scope.id);
        if !scope.source_root.exists() || !db_path.exists() {
            continue;
        }
        let Ok(inspection) = cache.inspect_read_only(&db_path, &scope.source_root, false) else {
            continue;
        };
        if let Some(gap) = LanguageCoverageGap::from_summary(
            Some(scope.id.clone()),
            inspection.total_files,
            &inspection.summary.skipped,
        ) {
            gaps.push(gap);
        }
    }
    gaps.sort_by(|left, right| left.scope.cmp(&right.scope));
    Ok(gaps)
}

fn check_index(root: &Path, cache: &StatusCheckCache) -> Result<IndexStatus> {
    if !config::Config::submodule_dirs(root)?.is_empty() {
        return check_workspace_index(root, cache);
    }

    check_single_index(root, cache)
}

fn check_single_index(root: &Path, cache: &StatusCheckCache) -> Result<IndexStatus> {
    let db_path = root.join(".tsift/index.db");
    if !db_path.exists() {
        return check_workspace_index(root, cache);
    }

    let last_indexed_secs_ago = db_path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let inspection = cache.inspect_read_only(&db_path, root, false)?;
    let stale_files =
        inspection.summary.new + inspection.summary.modified + inspection.summary.deleted;

    if stale_files > 0 {
        Ok(IndexStatus::Stale {
            total_files: inspection.total_files,
            stale_files,
            last_indexed_secs_ago,
            recovery: inspection.recovery,
            workspace_scopes: Vec::new(),
            missing_scopes: Vec::new(),
        })
    } else {
        Ok(IndexStatus::Fresh {
            total_files: inspection.total_files,
            stale_files: 0,
            last_indexed_secs_ago,
            recovery: inspection.recovery,
            workspace_scopes: Vec::new(),
            missing_scopes: Vec::new(),
        })
    }
}

fn check_workspace_index(root: &Path, cache: &StatusCheckCache) -> Result<IndexStatus> {
    let cfg = config::Config::load(root)?;
    let workspace_scopes = config::Config::submodule_dirs(root)?;
    if workspace_scopes.is_empty() {
        return Ok(IndexStatus::Missing {
            missing_scopes: Vec::new(),
        });
    }
    let excluded_roots = workspace_scopes
        .iter()
        .map(|scope| scope.source_root.clone())
        .collect::<Vec<_>>();
    let mut scopes = Vec::new();
    let mut missing_scopes = Vec::new();

    let root_db_path = root.join(".tsift/index.db");
    if root_db_path.exists() {
        let last_indexed_secs_ago = root_db_path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let inspection =
            IndexDb::inspect_read_only_excluding(&root_db_path, root, false, &excluded_roots)?;
        let stale_files =
            inspection.summary.new + inspection.summary.modified + inspection.summary.deleted;
        scopes.push(WorkspaceScopeStatus {
            scope: config::WORKSPACE_ROOT_SCOPE_ID.to_string(),
            db_path: root_db_path,
            total_files: inspection.total_files,
            stale_files,
            last_indexed_secs_ago,
            recovery: inspection.recovery,
        });
    } else {
        missing_scopes.push(MissingWorkspaceScopeStatus {
            scope: config::WORKSPACE_ROOT_SCOPE_ID.to_string(),
            db_path: root_db_path,
        });
    }

    for scope in workspace_scopes {
        let db_path = cfg.db_path_for(root, &scope.id);
        if !scope.source_root.exists() || !db_path.exists() {
            missing_scopes.push(MissingWorkspaceScopeStatus {
                scope: scope.id,
                db_path,
            });
            continue;
        }

        let last_indexed_secs_ago = db_path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let inspection = cache.inspect_read_only(&db_path, &scope.source_root, false)?;
        let stale_files =
            inspection.summary.new + inspection.summary.modified + inspection.summary.deleted;
        scopes.push(WorkspaceScopeStatus {
            scope: scope.id,
            db_path,
            total_files: inspection.total_files,
            stale_files,
            last_indexed_secs_ago,
            recovery: inspection.recovery,
        });
    }

    if scopes.is_empty() {
        return Ok(IndexStatus::Missing { missing_scopes });
    }

    scopes.sort_by(|left, right| left.scope.cmp(&right.scope));
    let total_files = scopes.iter().map(|scope| scope.total_files).sum();
    let stale_files = scopes.iter().map(|scope| scope.stale_files).sum();
    let last_indexed_secs_ago = scopes
        .iter()
        .map(|scope| scope.last_indexed_secs_ago)
        .min()
        .unwrap_or(0);
    let recovery = scopes.iter().find_map(|scope| scope.recovery);

    if stale_files > 0 || !missing_scopes.is_empty() {
        Ok(IndexStatus::Stale {
            total_files,
            stale_files,
            last_indexed_secs_ago,
            recovery,
            workspace_scopes: scopes,
            missing_scopes,
        })
    } else {
        Ok(IndexStatus::Fresh {
            total_files,
            stale_files: 0,
            last_indexed_secs_ago,
            recovery,
            workspace_scopes: scopes,
            missing_scopes,
        })
    }
}

pub fn check_locks(
    root: &Path,
    path_hint: Option<&Path>,
    scope: Option<&str>,
) -> Result<LockReport> {
    let (label, source_root, db_path, reindex_command) =
        resolve_lock_target(root, path_hint, scope)?;
    let lock_path = writer_lock_path(&db_path);
    let writer_lock = match probe_writer_lock(&lock_path)? {
        WriterLockProbe::Absent { path } => WriterLockStatus::Absent { path },
        WriterLockProbe::Live { path, pid } => WriterLockStatus::Live { path, pid },
        WriterLockProbe::Stale { path, pid } => WriterLockStatus::Stale { path, pid },
        WriterLockProbe::Unknown { path } => WriterLockStatus::Unknown { path },
    };
    let rollback_journal = SidecarStatus {
        path: rollback_journal_path(&db_path),
        present: rollback_journal_path(&db_path).exists(),
    };
    let wal_sidecar = SidecarStatus {
        path: wal_sidecar_path(&db_path),
        present: wal_sidecar_path(&db_path).exists(),
    };
    let shared_memory_sidecar = SidecarStatus {
        path: shared_memory_sidecar_path(&db_path),
        present: shared_memory_sidecar_path(&db_path).exists(),
    };
    let recommended_action = build_lock_recommendation(
        &writer_lock,
        &rollback_journal,
        &wal_sidecar,
        &shared_memory_sidecar,
        &reindex_command,
    );

    Ok(LockReport {
        label,
        source_root,
        db_path,
        writer_lock,
        rollback_journal,
        wal_sidecar,
        shared_memory_sidecar,
        reindex_command,
        recommended_action,
    })
}

fn check_summaries(
    root: &Path,
    db_path: &Path,
    index: &IndexStatus,
    cache: &StatusCheckCache,
) -> Result<SummaryStatus> {
    if matches!(index, IndexStatus::Missing { .. }) {
        return Ok(SummaryStatus::Unavailable);
    }
    let live_indexed_files = live_indexed_summary_paths(root, index, cache)?;
    let extractable_files = live_indexed_files
        .iter()
        .filter(|path| tsift_summarize::summarize::is_extraction_candidate_path(Path::new(path)))
        .cloned()
        .collect::<HashSet<_>>();
    let non_candidate_files = live_indexed_files
        .len()
        .saturating_sub(extractable_files.len());
    let total_indexed_files = extractable_files.len();
    if !db_path.exists() {
        if total_indexed_files == 0 {
            return Ok(SummaryStatus::Available {
                cached_files: 0,
                total_indexed_files,
                terminal_failure_files: 0,
                non_candidate_files,
                coverage_pct: 100,
                recovery: None,
            });
        }
        return Ok(SummaryStatus::None { recovery: None });
    }

    let read_only = SummaryDb::open_read_only_with_recovery(db_path)?;
    let recovery = read_only.recovery;
    let db = read_only.db;
    let cached_summary_paths = db.cached_file_paths()?.into_iter().collect::<HashSet<_>>();
    let cached_files = cached_summary_paths
        .intersection(&extractable_files)
        .count();
    let terminal_failure_files = db
        .current_terminal_failure_paths(root)?
        .into_iter()
        .filter(|path| extractable_files.contains(path))
        .count();

    if cached_files == 0 && terminal_failure_files == 0 && total_indexed_files > 0 {
        return Ok(SummaryStatus::None { recovery });
    }

    let coverage_pct = if total_indexed_files > 0 {
        ((cached_files as f64 / total_indexed_files as f64) * 100.0).min(100.0) as u8
    } else {
        100
    };

    Ok(SummaryStatus::Available {
        cached_files,
        total_indexed_files,
        terminal_failure_files,
        non_candidate_files,
        coverage_pct,
        recovery,
    })
}

fn live_indexed_summary_paths(
    root: &Path,
    index: &IndexStatus,
    cache: &StatusCheckCache,
) -> Result<HashSet<String>> {
    match index {
        IndexStatus::Fresh {
            workspace_scopes, ..
        }
        | IndexStatus::Stale {
            workspace_scopes, ..
        } => {
            if workspace_scopes.is_empty() {
                tracked_summary_paths_from_inspection(
                    cache,
                    &root.join(".tsift/index.db"),
                    root,
                    root,
                )
            } else {
                let mut paths = HashSet::new();
                let source_roots = config::Config::submodule_dirs(root)?
                    .into_iter()
                    .map(|scope| (scope.id, scope.source_root))
                    .collect::<HashMap<_, _>>();
                for scope in workspace_scopes {
                    let source_root = source_roots
                        .get(&scope.scope)
                        .map(PathBuf::as_path)
                        .unwrap_or(root);
                    paths.extend(tracked_summary_paths_from_inspection(
                        cache,
                        &scope.db_path,
                        root,
                        source_root,
                    )?);
                }
                Ok(paths)
            }
        }
        IndexStatus::Missing { .. } => Ok(HashSet::new()),
    }
}

fn tracked_summary_paths_from_inspection(
    cache: &StatusCheckCache,
    db_path: &Path,
    report_root: &Path,
    inspect_root: &Path,
) -> Result<HashSet<String>> {
    let inspection = cache.inspect_read_only(db_path, inspect_root, false)?;
    Ok(inspection
        .tracked_file_paths
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .map(|path| {
            path.strip_prefix(report_root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .to_string()
        })
        .collect())
}

fn build_recommendations(
    index: &IndexStatus,
    summaries: &SummaryStatus,
    instructions: &InstructionStatus,
    scope_instructions: &[ScopeInstructionStatus],
    workspace: bool,
    summarize_extract: &str,
    kg_present: bool,
) -> Recommendations {
    // #wsinit: a superproject block can be current while three submodules sit
    // two releases behind, so scope drift has to reach the `run:` line too.
    let refresh = !matches!(instructions, InstructionStatus::Current { .. })
        || scope_instructions
            .iter()
            .any(|scope| !matches!(scope.instructions, InstructionStatus::Current { .. }));
    let index_cmd = if workspace {
        "tsift index --workspace ."
    } else {
        "tsift index ."
    };
    let init_cmd = if workspace {
        "tsift init --workspace"
    } else {
        "tsift init"
    };

    match index {
        IndexStatus::Missing { missing_scopes } => Recommendations {
            use_commands: vec![],
            run: if refresh {
                Some(format!(
                    "{init_cmd} && {}",
                    format_index_run_with_gap(index_cmd, 0, missing_scopes.len())
                ))
            } else {
                Some(format_index_run_with_gap(
                    index_cmd,
                    0,
                    missing_scopes.len(),
                ))
            },
        },
        IndexStatus::Stale {
            stale_files,
            missing_scopes,
            ..
        } => {
            let mut use_cmds = vec![
                "search".to_string(),
                "explain".to_string(),
                "graph".to_string(),
            ];
            if kg_present {
                use_cmds.push("kg".to_string());
            }
            if matches!(summaries, SummaryStatus::Available { .. }) {
                use_cmds.push("summarize".to_string());
            }
            let run_msg = format_index_run_with_gap(index_cmd, *stale_files, missing_scopes.len());
            let run_msg = if refresh {
                format!("{init_cmd} && {run_msg}")
            } else {
                run_msg
            };
            Recommendations {
                use_commands: use_cmds,
                run: Some(run_msg),
            }
        }
        IndexStatus::Fresh { .. } => {
            let mut use_cmds = vec![
                "search".to_string(),
                "explain".to_string(),
                "graph".to_string(),
            ];
            if kg_present {
                use_cmds.push("kg".to_string());
            }
            let mut run = match summaries {
                SummaryStatus::Available {
                    cached_files,
                    total_indexed_files,
                    terminal_failure_files,
                    ..
                } => {
                    if *cached_files > 0 {
                        use_cmds.push("summarize".to_string());
                    }
                    let uncached = total_indexed_files
                        .saturating_sub(*cached_files)
                        .saturating_sub(*terminal_failure_files);
                    if uncached > 0 {
                        Some(format!(
                            "tsift summarize --extract {}  ({} uncached file{})",
                            summarize_extract,
                            uncached,
                            if uncached == 1 { "" } else { "s" }
                        ))
                    } else {
                        None
                    }
                }
                SummaryStatus::None { .. } => {
                    Some(format!("tsift summarize --extract {}", summarize_extract))
                }
                SummaryStatus::Unavailable => None,
            };
            if refresh {
                run = Some(match run {
                    Some(existing) => format!("{init_cmd} && {existing}"),
                    None => init_cmd.to_string(),
                });
            }
            Recommendations {
                use_commands: use_cmds,
                run,
            }
        }
    }
}

fn build_reminders(
    index: &IndexStatus,
    summaries: &SummaryStatus,
    recommendations: &Recommendations,
    summarize_extract: &str,
) -> Vec<String> {
    let IndexStatus::Stale {
        stale_files,
        missing_scopes,
        ..
    } = index
    else {
        return Vec::new();
    };

    let run =
        status_recommendation_command(recommendations.run.as_deref().unwrap_or("tsift index ."));
    let mut reminder = format!(
        "index stale: run `{}` before relying on tsift search/explain/graph",
        run
    );
    if *stale_files > 0 {
        reminder.push_str(&format!(
            " ({} stale file{})",
            stale_files,
            if *stale_files == 1 { "" } else { "s" }
        ));
    }
    if !missing_scopes.is_empty() {
        reminder.push_str(&format!(
            " ({} missing workspace scope{})",
            missing_scopes.len(),
            if missing_scopes.len() == 1 { "" } else { "s" }
        ));
    }
    if matches!(summaries, SummaryStatus::None { .. }) {
        reminder.push_str(&format!(
            "; no summaries are cached, so run `tsift summarize --extract {}` after the index is fresh when summary refs are needed",
            summarize_extract
        ));
    }
    vec![reminder]
}

fn status_recommendation_command(run: &str) -> &str {
    run.split_once("  (")
        .map(|(command, _)| command)
        .unwrap_or(run)
}

fn recommended_summarize_extract_path(
    root: &Path,
    index: &IndexStatus,
    workspace_scopes: &[config::WorkspaceScope],
) -> String {
    if !workspace_scopes.is_empty() {
        return common_extract_scope(
            workspace_scopes
                .iter()
                .map(|scope| Path::new(&scope.relative_path)),
            false,
        )
        .unwrap_or_else(|| ".".to_string());
    }

    match index {
        IndexStatus::Fresh { .. } | IndexStatus::Stale { .. } => common_extract_scope(
            IndexDb::file_paths_read_only(&root.join(".tsift/index.db"))
                .ok()
                .into_iter()
                .flatten()
                .map(PathBuf::from)
                .map(|path| path.strip_prefix(root).map(PathBuf::from).unwrap_or(path)),
            true,
        )
        .unwrap_or_else(|| ".".to_string()),
        IndexStatus::Missing { .. } => ".".to_string(),
    }
}

fn common_extract_scope<I, P>(paths: I, treat_inputs_as_files: bool) -> Option<String>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut common: Option<Vec<String>> = None;

    for raw_path in paths {
        let path = raw_path.as_ref();
        let scope = if treat_inputs_as_files {
            path.parent().unwrap_or_else(|| Path::new("."))
        } else {
            path
        };
        let components = scope
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(value) => Some(value.to_string_lossy().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();

        match &mut common {
            None => common = Some(components),
            Some(existing) => {
                let shared_len = existing
                    .iter()
                    .zip(components.iter())
                    .take_while(|(left, right)| left == right)
                    .count();
                existing.truncate(shared_len);
            }
        }
    }

    Some(match common {
        None => ".".to_string(),
        Some(components) if components.is_empty() => ".".to_string(),
        Some(components) => format!("{}/", components.join("/")),
    })
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn resolve_lock_target(
    root: &Path,
    path_hint: Option<&Path>,
    scope: Option<&str>,
) -> Result<(String, PathBuf, PathBuf, String)> {
    let cfg = config::Config::load(root)?;
    if let Some(scope_name) = scope {
        let scope = config::Config::resolve_submodule(root, scope_name)?;
        Ok((
            format!("submodule `{}` index", scope.id),
            scope.source_root.clone(),
            cfg.db_path_for(root, &scope.id),
            format!("tsift index --submodule {} {}", scope.id, root.display()),
        ))
    } else if let Some(path_hint) = path_hint {
        if let Some(scope) = config::Config::infer_submodule_from_path(root, path_hint)? {
            return Ok((
                format!("submodule `{}` index", scope.id),
                scope.source_root.clone(),
                cfg.db_path_for(root, &scope.id),
                format!("tsift index --submodule {} {}", scope.id, root.display()),
            ));
        }
        Ok((
            "index".to_string(),
            root.to_path_buf(),
            root.join(".tsift/index.db"),
            format!("tsift index {}", root.display()),
        ))
    } else {
        Ok((
            "index".to_string(),
            root.to_path_buf(),
            root.join(".tsift/index.db"),
            format!("tsift index {}", root.display()),
        ))
    }
}

fn build_lock_recommendation(
    writer_lock: &WriterLockStatus,
    rollback_journal: &SidecarStatus,
    wal_sidecar: &SidecarStatus,
    shared_memory_sidecar: &SidecarStatus,
    reindex_command: &str,
) -> String {
    let has_live_wal_state = wal_sidecar.present || shared_memory_sidecar.present;
    match writer_lock {
        WriterLockStatus::Live { pid, .. } => {
            let pid_hint = pid
                .map(|value| format!(" (pid {})", value))
                .unwrap_or_default();
            if has_live_wal_state {
                format!(
                    "wait for the active tsift writer{} to finish, then run `{}` to rebuild a clean WAL-mode index after the live sidecars clear.",
                    pid_hint, reindex_command
                )
            } else if rollback_journal.present {
                format!(
                    "wait for the active tsift writer{} to finish, then run `{}` to rebuild a clean WAL-mode index.",
                    pid_hint, reindex_command
                )
            } else {
                format!(
                    "wait for the active tsift writer{} to finish before rerunning `{}`.",
                    pid_hint, reindex_command
                )
            }
        }
        WriterLockStatus::Stale { path, .. } | WriterLockStatus::Unknown { path } => {
            format!(
                "the lock sidecar at `{}` is stale metadata only; rerun `{}` and tsift will reuse it automatically.",
                path.display(),
                reindex_command
            )
        }
        WriterLockStatus::Absent { .. } if has_live_wal_state => {
            format!(
                "inspect the host for a wedged writer holding live WAL sidecars, then run `{}` once writes are healthy. Read-only status checks can use snapshot fallback in the meantime.",
                reindex_command
            )
        }
        WriterLockStatus::Absent { .. } if rollback_journal.present => {
            format!(
                "inspect the host for a wedged rollback-journal writer, then run `{}` once writes are healthy. Read-only status checks can use snapshot fallback in the meantime.",
                reindex_command
            )
        }
        WriterLockStatus::Absent { .. } => "no lock remediation needed".to_string(),
    }
}

fn format_recovery_line(recovery: ReadOnlyRecovery, compact: bool) -> String {
    match (recovery, compact) {
        (ReadOnlyRecovery::SnapshotFallback, true) => "recovery:snapshot_fallback\n".to_string(),
        (ReadOnlyRecovery::SnapshotFallback, false) => {
            "recovery: snapshot fallback (rollback-journal lock on live index)\n".to_string()
        }
        (ReadOnlyRecovery::SnapshotFallbackWal, true) => {
            "recovery:snapshot_fallback_wal\n".to_string()
        }
        (ReadOnlyRecovery::SnapshotFallbackWal, false) => {
            "recovery: snapshot fallback (copied live WAL sidecars from index db)\n".to_string()
        }
    }
}

fn index_recovery(index: &IndexStatus) -> Option<ReadOnlyRecovery> {
    match index {
        IndexStatus::Fresh { recovery, .. } | IndexStatus::Stale { recovery, .. } => *recovery,
        IndexStatus::Missing { .. } => None,
    }
}

fn summary_recovery(summaries: &SummaryStatus) -> Option<ReadOnlyRecovery> {
    match summaries {
        SummaryStatus::Available { recovery, .. } | SummaryStatus::None { recovery } => *recovery,
        SummaryStatus::Unavailable => None,
    }
}

fn format_summary_recovery_line(recovery: ReadOnlyRecovery, compact: bool) -> String {
    match (recovery, compact) {
        (ReadOnlyRecovery::SnapshotFallback, true) => {
            "summaries_recovery:snapshot_fallback\n".to_string()
        }
        (ReadOnlyRecovery::SnapshotFallback, false) => {
            "summaries recovery: snapshot fallback (rollback-journal lock on live summaries db)\n"
                .to_string()
        }
        (ReadOnlyRecovery::SnapshotFallbackWal, true) => {
            "summaries_recovery:snapshot_fallback_wal\n".to_string()
        }
        (ReadOnlyRecovery::SnapshotFallbackWal, false) => {
            "summaries recovery: snapshot fallback (copied live WAL sidecars from summaries db)\n"
                .to_string()
        }
    }
}

fn workspace_scopes(index: &IndexStatus) -> &[WorkspaceScopeStatus] {
    match index {
        IndexStatus::Fresh {
            workspace_scopes, ..
        }
        | IndexStatus::Stale {
            workspace_scopes, ..
        } => workspace_scopes.as_slice(),
        IndexStatus::Missing { .. } => &[],
    }
}

fn missing_workspace_scopes(index: &IndexStatus) -> &[MissingWorkspaceScopeStatus] {
    match index {
        IndexStatus::Fresh { missing_scopes, .. }
        | IndexStatus::Stale { missing_scopes, .. }
        | IndexStatus::Missing { missing_scopes } => missing_scopes.as_slice(),
    }
}

fn format_workspace_scope_line(scope: &WorkspaceScopeStatus, compact: bool) -> String {
    let state = if scope.stale_files > 0 {
        "stale"
    } else {
        "fresh"
    };
    if compact {
        format!(
            "scope:{} state:{} tracked:{} stale:{} age:{}\n",
            scope.scope,
            state,
            scope.total_files,
            scope.stale_files,
            format_duration(scope.last_indexed_secs_ago)
        )
    } else if scope.stale_files > 0 {
        format!(
            "  scope {}: stale (last indexed {}, {} files tracked, {} stale)\n",
            scope.scope,
            format_duration(scope.last_indexed_secs_ago),
            scope.total_files,
            scope.stale_files
        )
    } else {
        format!(
            "  scope {}: fresh (last indexed {}, {} files tracked)\n",
            scope.scope,
            format_duration(scope.last_indexed_secs_ago),
            scope.total_files
        )
    }
}

fn format_missing_workspace_scope_line(
    scope: &MissingWorkspaceScopeStatus,
    compact: bool,
) -> String {
    if compact {
        format!("scope:{} state:missing\n", scope.scope)
    } else {
        format!(
            "  scope {}: missing index ({})\n",
            scope.scope,
            scope.db_path.display()
        )
    }
}

fn format_index_run_with_gap(index_cmd: &str, stale_files: usize, missing_scopes: usize) -> String {
    let mut notes = Vec::new();
    if stale_files > 0 {
        notes.push(format!(
            "{} stale file{}",
            stale_files,
            if stale_files == 1 { "" } else { "s" }
        ));
    }
    if missing_scopes > 0 {
        notes.push(format!(
            "{} missing scope{}",
            missing_scopes,
            if missing_scopes == 1 { "" } else { "s" }
        ));
    }
    if notes.is_empty() {
        index_cmd.to_string()
    } else {
        format!("{}  ({})", index_cmd, notes.join(", "))
    }
}

pub fn format_human(report: &StatusReport, compact: bool) -> String {
    let mut out = String::new();

    match &report.index {
        IndexStatus::Missing { missing_scopes } => {
            if compact {
                if missing_scopes.is_empty() {
                    out.push_str("index: missing\n");
                } else {
                    out.push_str(&format!(
                        "index: missing workspace_missing:{}\n",
                        missing_scopes.len()
                    ));
                }
            } else if missing_scopes.is_empty() {
                out.push_str("index: missing\n");
            } else {
                out.push_str(&format!(
                    "index: missing (workspace, {} scope{} not indexed)\n",
                    missing_scopes.len(),
                    if missing_scopes.len() == 1 { "" } else { "s" }
                ));
            }
        }
        IndexStatus::Fresh {
            total_files,
            last_indexed_secs_ago,
            workspace_scopes,
            ..
        } => {
            if compact {
                if workspace_scopes.is_empty() {
                    out.push_str(&format!(
                        "index: fresh tracked:{} age:{}\n",
                        total_files,
                        format_duration(*last_indexed_secs_ago)
                    ));
                } else {
                    out.push_str(&format!(
                        "index: fresh workspace:{} tracked:{} age:{}\n",
                        workspace_scopes.len(),
                        total_files,
                        format_duration(*last_indexed_secs_ago)
                    ));
                }
            } else {
                if workspace_scopes.is_empty() {
                    out.push_str(&format!(
                        "index: fresh (last indexed {}, {} files tracked)\n",
                        format_duration(*last_indexed_secs_ago),
                        total_files
                    ));
                } else {
                    out.push_str(&format!(
                        "index: fresh (workspace, {} scopes, last indexed {}, {} files tracked)\n",
                        workspace_scopes.len(),
                        format_duration(*last_indexed_secs_ago),
                        total_files
                    ));
                }
            }
        }
        IndexStatus::Stale {
            total_files,
            stale_files,
            last_indexed_secs_ago,
            workspace_scopes,
            missing_scopes,
            ..
        } => {
            if compact {
                if workspace_scopes.is_empty() {
                    out.push_str(&format!(
                        "index: stale tracked:{} stale:{} age:{}\n",
                        total_files,
                        stale_files,
                        format_duration(*last_indexed_secs_ago)
                    ));
                } else {
                    let missing_suffix = if missing_scopes.is_empty() {
                        String::new()
                    } else {
                        format!(" missing:{}", missing_scopes.len())
                    };
                    out.push_str(&format!(
                        "index: stale workspace:{}{} tracked:{} stale:{} age:{}\n",
                        workspace_scopes.len(),
                        missing_suffix,
                        total_files,
                        stale_files,
                        format_duration(*last_indexed_secs_ago)
                    ));
                }
            } else {
                if workspace_scopes.is_empty() {
                    out.push_str(&format!(
                        "index: stale (last indexed {}, {} files tracked, {} stale)\n",
                        format_duration(*last_indexed_secs_ago),
                        total_files,
                        stale_files
                    ));
                } else {
                    out.push_str(&format!(
                        "index: stale (workspace, {} indexed scope{}, {} missing scope{}, last indexed {}, {} files tracked, {} stale)\n",
                        workspace_scopes.len(),
                        if workspace_scopes.len() == 1 { "" } else { "s" },
                        missing_scopes.len(),
                        if missing_scopes.len() == 1 { "" } else { "s" },
                        format_duration(*last_indexed_secs_ago),
                        total_files,
                        stale_files
                    ));
                }
            }
        }
    }

    for scope in workspace_scopes(&report.index) {
        out.push_str(&format_workspace_scope_line(scope, compact));
    }
    for scope in missing_workspace_scopes(&report.index) {
        out.push_str(&format_missing_workspace_scope_line(scope, compact));
    }

    if let Some(recovery) = index_recovery(&report.index) {
        out.push_str(&format_recovery_line(recovery, compact));
    }

    match &report.instructions {
        InstructionStatus::Current { version } => {
            if compact {
                out.push_str(&format!("instructions: current v={}\n", version));
            } else {
                out.push_str(&format!("instructions: current (v{})\n", version));
            }
        }
        InstructionStatus::Stale {
            found: Some(v),
            expected,
        } => {
            if compact {
                out.push_str(&format!(
                    "instructions: stale v={} expected={}\n",
                    v, expected
                ));
            } else {
                out.push_str(&format!(
                    "instructions: stale (v{} installed, v{} available — run tsift init)\n",
                    v, expected
                ));
            }
        }
        InstructionStatus::Stale {
            found: None,
            expected,
        } => {
            if compact {
                out.push_str(&format!(
                    "instructions: stale pre-versioned expected={}\n",
                    expected
                ));
            } else {
                out.push_str(&format!(
                    "instructions: stale (pre-versioned, v{} available — run tsift init)\n",
                    expected
                ));
            }
        }
        InstructionStatus::Missing => {
            out.push_str("instructions: missing (run tsift init)\n");
        }
    }

    // #wsinit: index freshness is reported per scope; instruction state was not,
    // so a submodule two releases behind was invisible from the workspace root.
    if !report.scope_instructions.is_empty() {
        let drifted = report
            .scope_instructions
            .iter()
            .filter(|scope| !matches!(scope.instructions, InstructionStatus::Current { .. }))
            .collect::<Vec<_>>();
        if !drifted.is_empty() {
            if compact {
                for scope in &drifted {
                    out.push_str(&format!(
                        "scope_instructions:{} {}\n",
                        scope.scope,
                        scope_instruction_label(&scope.instructions)
                    ));
                }
            } else {
                out.push_str(&format!(
                    "instructions: stale in {} of {} scopes (run tsift init --workspace)\n",
                    drifted.len(),
                    report.scope_instructions.len()
                ));
                for scope in &drifted {
                    out.push_str(&format!(
                        "  scope {}: {}\n",
                        scope.scope,
                        scope_instruction_label(&scope.instructions)
                    ));
                }
            }
        }
    }

    match &report.summaries {
        SummaryStatus::Available {
            cached_files,
            total_indexed_files,
            terminal_failure_files,
            non_candidate_files,
            coverage_pct,
            ..
        } => {
            if compact {
                out.push_str(&format!(
                    "summaries: {}/{} ({}%) terminal:{} noncandidate:{}\n",
                    cached_files,
                    total_indexed_files,
                    coverage_pct,
                    terminal_failure_files,
                    non_candidate_files
                ));
            } else {
                out.push_str(&format!(
                    "summaries: {}/{} extraction candidates cached ({}%)",
                    cached_files, total_indexed_files, coverage_pct
                ));
                if *terminal_failure_files > 0 {
                    out.push_str(&format!(", {} terminal failure", terminal_failure_files));
                    if *terminal_failure_files != 1 {
                        out.push('s');
                    }
                }
                if *non_candidate_files > 0 {
                    out.push_str(&format!(", {} indexed file", non_candidate_files));
                    if *non_candidate_files != 1 {
                        out.push('s');
                    }
                    out.push_str(" not extractable");
                }
                out.push('\n');
            }
        }
        SummaryStatus::None { .. } => {
            out.push_str("summaries: none\n");
        }
        SummaryStatus::Unavailable => {
            out.push_str("summaries: unavailable (no index)\n");
        }
    }

    if let Some(recovery) = summary_recovery(&report.summaries) {
        out.push_str(&format_summary_recovery_line(recovery, compact));
    }

    // #goindex: name the languages the index does not cover. `fresh` plus a
    // file count reads as "the repo is indexed"; without this line a scope that
    // skipped its dominant language answers every search confidently and empty.
    if compact {
        for gap in &report.language_coverage {
            out.push_str(&format!(
                "coverage:{} indexed:{} skipped:{} top:{}={}\n",
                gap.scope.as_deref().unwrap_or("."),
                gap.indexed_files,
                gap.skipped_files,
                gap.dominant_extension,
                gap.dominant_extension_files
            ));
        }
    } else if !report.language_coverage.is_empty() {
        out.push_str("language coverage:\n");
        for gap in &report.language_coverage {
            let breakdown = gap
                .skipped_by_extension
                .iter()
                .take(6)
                .map(|(ext, count)| format!("{ext} {count}"))
                .collect::<Vec<_>>()
                .join(", ");
            match &gap.scope {
                Some(scope) => out.push_str(&format!(
                    "  scope {scope}: indexed {} of {} walked files — skipped {}\n",
                    gap.indexed_files,
                    gap.indexed_files + gap.skipped_files,
                    breakdown
                )),
                None => out.push_str(&format!(
                    "  indexed {} of {} walked files — skipped {}\n",
                    gap.indexed_files,
                    gap.indexed_files + gap.skipped_files,
                    breakdown
                )),
            }
        }
    }

    if compact {
        for reminder in &report.reminders {
            out.push_str(&format!("reminder: {}\n", reminder));
        }
        if report.recommendations.use_commands.is_empty() {
            out.push_str("use: none\n");
        } else {
            out.push_str(&format!(
                "use: {}\n",
                report.recommendations.use_commands.join(", ")
            ));
        }
        if let Some(run) = &report.recommendations.run {
            out.push_str(&format!("run: {}\n", run));
        }
    } else {
        if !report.reminders.is_empty() {
            out.push_str("reminders:\n");
            for reminder in &report.reminders {
                out.push_str(&format!("  - {}\n", reminder));
            }
        }
        out.push_str("recommendations:\n");
        if report.recommendations.use_commands.is_empty() {
            out.push_str("  use: (none — run tsift index first)\n");
        } else {
            out.push_str(&format!(
                "  use: {}\n",
                report.recommendations.use_commands.join(", ")
            ));
        }
        if let Some(run) = &report.recommendations.run {
            out.push_str(&format!("  run: {}\n", run));
        }
    }

    out
}

pub fn format_locks_human(report: &LockReport, compact: bool) -> String {
    let lock_line = match &report.writer_lock {
        WriterLockStatus::Absent { path } => format!("lock: absent {}\n", path.display()),
        WriterLockStatus::Live { path, pid } => match pid {
            Some(value) => format!("lock: live pid:{} {}\n", value, path.display()),
            None => format!("lock: live {}\n", path.display()),
        },
        WriterLockStatus::Stale { path, pid } => match pid {
            Some(value) => format!("lock: stale pid:{} {}\n", value, path.display()),
            None => format!("lock: stale {}\n", path.display()),
        },
        WriterLockStatus::Unknown { path } => format!("lock: unknown {}\n", path.display()),
    };
    let journal_line = if report.rollback_journal.present {
        format!(
            "journal: present {}\n",
            report.rollback_journal.path.display()
        )
    } else {
        format!(
            "journal: absent {}\n",
            report.rollback_journal.path.display()
        )
    };
    let wal_line = if report.wal_sidecar.present {
        format!("wal: present {}\n", report.wal_sidecar.path.display())
    } else {
        format!("wal: absent {}\n", report.wal_sidecar.path.display())
    };
    let shm_line = if report.shared_memory_sidecar.present {
        format!(
            "shm: present {}\n",
            report.shared_memory_sidecar.path.display()
        )
    } else {
        format!(
            "shm: absent {}\n",
            report.shared_memory_sidecar.path.display()
        )
    };

    let mut out = String::new();
    if compact {
        out.push_str(&format!(
            "target:{} db:{}\n",
            report.label,
            report.db_path.display()
        ));
        out.push_str(&lock_line);
        out.push_str(&journal_line);
        out.push_str(&wal_line);
        out.push_str(&shm_line);
        out.push_str(&format!("run:{}\n", report.reindex_command));
        out.push_str(&format!("next:{}\n", report.recommended_action));
    } else {
        out.push_str(&format!("target: {}\n", report.label));
        out.push_str(&format!("source: {}\n", report.source_root.display()));
        out.push_str(&format!("db: {}\n", report.db_path.display()));
        out.push_str(&lock_line);
        out.push_str(&journal_line);
        out.push_str(&wal_line);
        out.push_str(&shm_line);
        out.push_str(&format!("run: {}\n", report.reindex_command));
        out.push_str(&format!("next: {}\n", report.recommended_action));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs4::fs_std::FileExt;
    use rusqlite::Connection;
    use std::fs::OpenOptions;
    use tempfile::TempDir;
    use tsift_index::config::Config;
    use tsift_sqlite::wal_sidecar_path;

    fn setup_workspace() -> TempDir {
        let dir = TempDir::new().unwrap();
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
        std::fs::create_dir_all(dir.path().join("src/alpha")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/beta")).unwrap();
        std::fs::write(
            dir.path().join("src/alpha/lib.rs"),
            "fn alpha_helper() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/beta/lib.rs"), "fn beta_helper() {}\n").unwrap();
        dir
    }

    fn index_workspace(root: &Path) {
        let scopes = Config::submodule_dirs(root).unwrap();
        let excluded_roots = scopes
            .iter()
            .map(|scope| scope.source_root.clone())
            .collect::<Vec<_>>();
        let root_db = IndexDb::open(&root.join(".tsift/index.db")).unwrap();
        root_db
            .apply_changes_excluding(root, &excluded_roots)
            .unwrap();
        let cfg = Config::load(root).unwrap();
        for scope in scopes {
            let db = IndexDb::open(&cfg.db_path_for(root, &scope.id)).unwrap();
            db.apply_changes(&scope.source_root).unwrap();
        }
    }

    // #wsinit regression: `status` collapsed every scope into one
    // `instructions:` line, so submodules left two releases behind by
    // `init --workspace` were invisible from the workspace root — and the
    // superproject block that was current is the one AGENTS.md deliberately
    // shadows.
    #[test]
    fn status_reports_instruction_drift_per_scope() {
        let dir = setup_workspace();
        // Root gets a current block; the scopes get nothing.
        init::init(dir.path(), false, false).unwrap();

        let report = check_status(dir.path()).unwrap();
        assert!(
            matches!(report.instructions, InstructionStatus::Current { .. }),
            "root block is current: {:?}",
            report.instructions
        );
        assert_eq!(report.scope_instructions.len(), 2);
        assert!(
            report
                .scope_instructions
                .iter()
                .all(|scope| matches!(scope.instructions, InstructionStatus::Missing)),
            "both scopes lack a block: {:?}",
            report.scope_instructions
        );

        let human = format_human(&report, false);
        assert!(
            human.contains("instructions: stale in 2 of 2 scopes"),
            "{human}"
        );
        assert!(human.contains("scope alpha: missing"), "{human}");
        assert!(
            report
                .recommendations
                .run
                .as_deref()
                .is_some_and(|run| run.contains("tsift init --workspace")),
            "scope drift must reach the run line: {:?}",
            report.recommendations.run
        );
    }

    #[test]
    fn status_omits_scopes_that_opt_out_of_instructions() {
        let dir = setup_workspace();
        init::init(dir.path(), false, false).unwrap();
        std::fs::create_dir_all(dir.path().join(".tsift")).unwrap();
        std::fs::write(
            dir.path().join(".tsift/config.toml"),
            "[overrides.alpha]\ninstructions = false\n",
        )
        .unwrap();

        let report = check_status(dir.path()).unwrap();
        assert_eq!(report.scope_instructions.len(), 1);
        assert_eq!(report.scope_instructions[0].scope, "beta");
    }

    // #goindex regression: `status` reported a scope as `fresh (… 8 files
    // tracked)` while the walk had silently dropped most of the repo for want
    // of an indexer language, so the failure surfaced only as confident empty
    // search results.
    #[test]
    fn status_reports_a_scope_whose_dominant_language_is_unindexable() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
        for idx in 0..8 {
            std::fs::write(
                dir.path().join(format!("data{idx}.parquetish")),
                "not a language tsift indexes\n",
            )
            .unwrap();
        }
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let report = check_status(dir.path()).unwrap();
        let gap = report
            .language_coverage
            .first()
            .expect("a scope that skipped 8 of 9 walked files is a coverage gap");
        assert_eq!(gap.skipped_files, 8);
        assert_eq!(gap.dominant_extension, ".parquetish");
        assert_eq!(gap.dominant_extension_files, 8);

        let human = format_human(&report, false);
        assert!(
            human.contains("language coverage:") && human.contains(".parquetish 8"),
            "status must name the skipped extension: {human}"
        );
    }

    // The mirror image: a repo whose files tsift does index must not grow a
    // coverage warning just because a stray unsupported file sits next to them.
    #[test]
    fn status_does_not_report_coverage_gap_for_incidental_skips() {
        let dir = TempDir::new().unwrap();
        for idx in 0..10 {
            std::fs::write(
                dir.path().join(format!("lib{idx}.rs")),
                format!("fn helper{idx}() {{}}\n"),
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("notes.txt"), "prose\n").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let report = check_status(dir.path()).unwrap();
        assert!(
            report.language_coverage.is_empty(),
            "one stray .txt is not a coverage gap: {:?}",
            report.language_coverage
        );
    }

    #[test]
    fn status_reports_workspace_root_language_coverage_gap() {
        let dir = setup_workspace();
        std::fs::write(dir.path().join("keep.rs"), "fn keep() {}\n").unwrap();
        for idx in 0..8 {
            std::fs::write(
                dir.path().join(format!("root-data{idx}.parquetish")),
                "not a language tsift indexes\n",
            )
            .unwrap();
        }
        index_workspace(dir.path());

        let report = check_status(dir.path()).unwrap();
        let gap = report
            .language_coverage
            .iter()
            .find(|gap| gap.scope.as_deref() == Some(config::WORKSPACE_ROOT_SCOPE_ID))
            .expect("workspace-root skips should be reported as the <root> scope");
        assert_eq!(gap.dominant_extension, ".parquetish");
        assert_eq!(gap.dominant_extension_files, 8);
    }

    fn hold_wal_lock(db_path: &Path) -> Connection {
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
        assert!(wal_sidecar_path(db_path).exists());
        conn
    }

    #[test]
    fn status_no_index() {
        let dir = TempDir::new().unwrap();
        let report = check_status(dir.path()).unwrap();
        assert!(matches!(
            report.index,
            IndexStatus::Missing { ref missing_scopes } if missing_scopes.is_empty()
        ));
        assert!(matches!(report.summaries, SummaryStatus::Unavailable));
        assert!(matches!(report.instructions, InstructionStatus::Missing));
        assert!(report.recommendations.use_commands.is_empty());
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init && tsift index .")
        );
    }

    #[test]
    fn status_fresh_index_no_summaries() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let report = check_status(dir.path()).unwrap();
        assert!(matches!(
            report.index,
            IndexStatus::Fresh { stale_files: 0, .. }
        ));
        assert!(matches!(report.summaries, SummaryStatus::None { .. }));
        let cmds = &report.recommendations.use_commands;
        assert!(cmds.contains(&"search".to_string()));
        assert!(cmds.contains(&"explain".to_string()));
        assert!(cmds.contains(&"graph".to_string()));
        assert!(!cmds.contains(&"summarize".to_string()));
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init && tsift summarize --extract .")
        );
    }

    #[test]
    fn status_fresh_index_with_graph_db_recommends_kg() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();
        // A present graph.db is the "KG in use" signal that promotes `kg`.
        std::fs::write(dir.path().join(".tsift/graph.db"), b"").unwrap();

        let report = check_status(dir.path()).unwrap();
        let cmds = &report.recommendations.use_commands;
        assert!(cmds.contains(&"kg".to_string()));
        // kg follows graph in the ordering
        let graph_idx = cmds.iter().position(|c| c == "graph").unwrap();
        let kg_idx = cmds.iter().position(|c| c == "kg").unwrap();
        assert!(kg_idx > graph_idx);
    }

    #[test]
    fn status_fresh_index_without_graph_db_omits_kg() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let report = check_status(dir.path()).unwrap();
        assert!(
            !report
                .recommendations
                .use_commands
                .contains(&"kg".to_string())
        );
    }

    #[test]
    fn status_cache_reuses_index_inspection_until_invalidated() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db_path = dir.path().join(".tsift/index.db");
        let db = IndexDb::open(&db_path).unwrap();
        db.apply_changes(dir.path()).unwrap();
        drop(db);

        let cache = StatusCheckCache::new();
        let report = check_status_with_cache(dir.path(), &cache).unwrap();
        assert!(matches!(
            report.index,
            IndexStatus::Fresh { recovery: None, .. }
        ));

        let _lock = hold_wal_lock(&db_path);
        let cached_report = check_status_with_cache(dir.path(), &cache).unwrap();
        assert!(matches!(
            cached_report.index,
            IndexStatus::Fresh { recovery: None, .. }
        ));

        cache.invalidate_all();
        let refreshed_report = check_status_with_cache(dir.path(), &cache).unwrap();
        assert!(matches!(
            refreshed_report.index,
            IndexStatus::Fresh {
                recovery: Some(ReadOnlyRecovery::SnapshotFallbackWal),
                ..
            }
        ));
    }

    #[test]
    fn status_fresh_src_layout_recommends_src_extract() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn alpha() {}").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let report = check_status(dir.path()).unwrap();
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init && tsift summarize --extract src/")
        );
    }

    #[test]
    fn status_workspace_scoped_indexes_report_fresh() {
        let dir = setup_workspace();
        index_workspace(dir.path());

        let report = check_status(dir.path()).unwrap();
        match &report.index {
            IndexStatus::Fresh {
                total_files,
                workspace_scopes,
                ..
            } => {
                assert_eq!(*total_files, 2);
                assert_eq!(workspace_scopes.len(), 3);
                assert_eq!(workspace_scopes[0].scope, config::WORKSPACE_ROOT_SCOPE_ID);
                assert_eq!(workspace_scopes[1].scope, "alpha");
                assert_eq!(workspace_scopes[2].scope, "beta");
            }
            other => panic!("expected fresh workspace status, got {other:?}"),
        }
        assert!(matches!(report.summaries, SummaryStatus::None { .. }));
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init --workspace && tsift summarize --extract src/")
        );
    }

    #[test]
    fn status_workspace_non_src_layout_recommends_dot_extract() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join(".gitmodules"),
            r#"[submodule "alpha"]
	path = alpha
	url = https://example.com/alpha
[submodule "crates/beta"]
	path = crates/beta
	url = https://example.com/beta
"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("alpha/src")).unwrap();
        std::fs::create_dir_all(dir.path().join("crates/beta/src")).unwrap();
        std::fs::write(
            dir.path().join("alpha/src/lib.rs"),
            "fn alpha_helper() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("crates/beta/src/lib.rs"),
            "fn beta_helper() {}\n",
        )
        .unwrap();

        index_workspace(dir.path());

        let report = check_status(dir.path()).unwrap();
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init --workspace && tsift summarize --extract .")
        );
    }

    #[test]
    fn status_workspace_missing_recommends_workspace_index() {
        let dir = setup_workspace();

        let report = check_status(dir.path()).unwrap();
        match &report.index {
            IndexStatus::Missing { missing_scopes } => {
                assert_eq!(missing_scopes.len(), 3);
                assert_eq!(missing_scopes[0].scope, config::WORKSPACE_ROOT_SCOPE_ID);
                assert_eq!(missing_scopes[1].scope, "alpha");
                assert_eq!(missing_scopes[2].scope, "beta");
            }
            other => panic!("expected missing workspace status, got {other:?}"),
        }
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init --workspace && tsift index --workspace .  (3 missing scopes)")
        );
    }

    #[test]
    fn status_workspace_partial_indexes_report_missing_scopes() {
        let dir = setup_workspace();
        let cfg = Config::load(dir.path()).unwrap();
        let alpha = Config::resolve_submodule(dir.path(), "alpha").unwrap();
        let db = IndexDb::open(&cfg.db_path_for(dir.path(), &alpha.id)).unwrap();
        db.apply_changes(&alpha.source_root).unwrap();

        let report = check_status(dir.path()).unwrap();
        match &report.index {
            IndexStatus::Stale {
                total_files,
                stale_files,
                workspace_scopes,
                missing_scopes,
                ..
            } => {
                assert_eq!(*total_files, 1);
                assert_eq!(*stale_files, 0);
                assert_eq!(workspace_scopes.len(), 1);
                assert_eq!(workspace_scopes[0].scope, "alpha");
                assert_eq!(missing_scopes.len(), 2);
                assert_eq!(missing_scopes[0].scope, config::WORKSPACE_ROOT_SCOPE_ID);
                assert_eq!(missing_scopes[1].scope, "beta");
            }
            other => panic!("expected partial workspace status, got {other:?}"),
        }
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init --workspace && tsift index --workspace .  (2 missing scopes)")
        );
    }

    #[test]
    fn status_workspace_reports_filtered_root_and_scoped_indexes() {
        let dir = setup_workspace();
        std::fs::write(dir.path().join("root.rs"), "fn root_helper() {}\n").unwrap();
        let excluded_roots = [dir.path().join("src/alpha"), dir.path().join("src/beta")];
        let root_db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        root_db
            .apply_changes_excluding(dir.path(), &excluded_roots)
            .unwrap();

        let cfg = Config::load(dir.path()).unwrap();
        let alpha = Config::resolve_submodule(dir.path(), "alpha").unwrap();
        let alpha_db = IndexDb::open(&cfg.db_path_for(dir.path(), &alpha.id)).unwrap();
        alpha_db.apply_changes(&alpha.source_root).unwrap();

        let report = check_status(dir.path()).unwrap();
        match &report.index {
            IndexStatus::Stale {
                total_files,
                stale_files,
                workspace_scopes,
                missing_scopes,
                ..
            } => {
                assert_eq!(*total_files, 2);
                assert_eq!(*stale_files, 0);
                assert_eq!(workspace_scopes.len(), 2);
                assert_eq!(workspace_scopes[0].scope, config::WORKSPACE_ROOT_SCOPE_ID);
                assert_eq!(workspace_scopes[0].total_files, 1);
                assert_eq!(workspace_scopes[1].scope, "alpha");
                assert_eq!(missing_scopes.len(), 1);
                assert_eq!(missing_scopes[0].scope, "beta");
            }
            other => panic!("expected mixed workspace status to stay scope-aware, got {other:?}"),
        }
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init --workspace && tsift index --workspace .  (1 missing scope)")
        );
    }

    #[test]
    fn status_workspace_scoped_indexes_report_stale() {
        let dir = setup_workspace();
        index_workspace(dir.path());
        std::fs::write(dir.path().join("src/beta/new.rs"), "fn late() {}\n").unwrap();

        let report = check_status(dir.path()).unwrap();
        match &report.index {
            IndexStatus::Stale {
                total_files,
                stale_files,
                workspace_scopes,
                ..
            } => {
                assert_eq!(*total_files, 2);
                assert_eq!(*stale_files, 1);
                assert_eq!(workspace_scopes.len(), 3);
                assert_eq!(
                    workspace_scopes
                        .iter()
                        .find(|scope| scope.scope == "beta")
                        .unwrap()
                        .stale_files,
                    1
                );
            }
            other => panic!("expected stale workspace status, got {other:?}"),
        }
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init --workspace && tsift index --workspace .  (1 stale file)")
        );
    }

    #[test]
    fn status_stale_index() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        std::fs::write(dir.path().join("lib.rs"), "fn helper() {}").unwrap();

        let report = check_status(dir.path()).unwrap();
        assert!(matches!(
            report.index,
            IndexStatus::Stale { stale_files: 1, .. }
        ));
        assert!(
            report
                .recommendations
                .run
                .as_deref()
                .unwrap()
                .contains("tsift index")
        );
    }

    #[test]
    fn status_fresh_index_with_summaries() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let sdb = SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        sdb.insert(&tsift_summarize::summarize::Summary {
            id: 0,
            symbol_name: "main".to_string(),
            file_path: "main.rs".to_string(),
            content_hash: "abc123".to_string(),
            summary: "Entry point".to_string(),
            entities: None,
            relationships: None,
            concept_labels: None,
            extracted_at: "2026-01-01".to_string(),
            model: "test".to_string(),
            tokens_input: Some(100),
            tokens_output: Some(50),
        })
        .unwrap();

        let report = check_status(dir.path()).unwrap();
        assert!(matches!(report.index, IndexStatus::Fresh { .. }));
        assert!(matches!(report.summaries, SummaryStatus::Available { .. }));
        assert!(
            report
                .recommendations
                .use_commands
                .contains(&"summarize".to_string())
        );
    }

    #[test]
    fn status_summaries_use_snapshot_fallback_when_rollback_journal_is_locked() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let db_path = dir.path().join(".tsift/summaries.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=DELETE;
             CREATE TABLE summaries (
                 id INTEGER PRIMARY KEY,
                 symbol_name TEXT NOT NULL,
                 file_path TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 summary TEXT NOT NULL,
                 entities TEXT,
                 relationships TEXT,
                 concept_labels TEXT,
                 extracted_at TEXT NOT NULL,
                 model TEXT NOT NULL,
                 tokens_input INTEGER,
                 tokens_output INTEGER
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO summaries
             (symbol_name, file_path, content_hash, summary, entities, relationships, concept_labels, extracted_at, model, tokens_input, tokens_output)
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5, ?6, NULL, NULL)",
            rusqlite::params![
                "main",
                "main.rs",
                "abc123",
                "Entry point",
                "2026-01-01",
                "test",
            ],
        )
        .unwrap();
        conn.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        std::fs::write(rollback_journal_path(&db_path), "locked").unwrap();

        let report = check_status(dir.path()).unwrap();

        match report.summaries {
            SummaryStatus::Available {
                cached_files,
                total_indexed_files,
                coverage_pct,
                recovery,
                ..
            } => {
                assert_eq!(cached_files, 1);
                assert_eq!(total_indexed_files, 1);
                assert_eq!(coverage_pct, 100);
                assert_eq!(recovery, Some(ReadOnlyRecovery::SnapshotFallback));
            }
            other => panic!("expected available summaries, got {other:?}"),
        }
    }

    #[test]
    fn status_summaries_report_wal_snapshot_recovery_when_wal_db_is_locked() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let db_path = dir.path().join(".tsift/summaries.db");
        let sdb = SummaryDb::open(&db_path).unwrap();
        sdb.insert(&tsift_summarize::summarize::Summary {
            id: 0,
            symbol_name: "main".to_string(),
            file_path: "main.rs".to_string(),
            content_hash: "abc123".to_string(),
            summary: "Entry point".to_string(),
            entities: None,
            relationships: None,
            concept_labels: None,
            extracted_at: "2026-01-01".to_string(),
            model: "test".to_string(),
            tokens_input: None,
            tokens_output: None,
        })
        .unwrap();
        drop(sdb);

        let _lock = hold_wal_lock(&db_path);

        let report = check_status(dir.path()).unwrap();
        match report.summaries {
            SummaryStatus::Available { recovery, .. } => {
                assert_eq!(recovery, Some(ReadOnlyRecovery::SnapshotFallbackWal));
            }
            other => panic!("expected available summaries, got {other:?}"),
        }
    }

    #[test]
    fn status_summary_coverage_ignores_deleted_summary_rows() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let sdb = SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        sdb.insert(&tsift_summarize::summarize::Summary {
            id: 0,
            symbol_name: "main".to_string(),
            file_path: "main.rs".to_string(),
            content_hash: "abc123".to_string(),
            summary: "Entry point".to_string(),
            entities: None,
            relationships: None,
            concept_labels: None,
            extracted_at: "2026-01-01".to_string(),
            model: "test".to_string(),
            tokens_input: Some(100),
            tokens_output: Some(50),
        })
        .unwrap();
        sdb.insert(&tsift_summarize::summarize::Summary {
            id: 0,
            symbol_name: "ghost".to_string(),
            file_path: "removed.rs".to_string(),
            content_hash: "def456".to_string(),
            summary: "Stale summary".to_string(),
            entities: None,
            relationships: None,
            concept_labels: None,
            extracted_at: "2026-01-01".to_string(),
            model: "test".to_string(),
            tokens_input: Some(100),
            tokens_output: Some(50),
        })
        .unwrap();

        let report = check_status(dir.path()).unwrap();
        match report.summaries {
            SummaryStatus::Available {
                cached_files,
                total_indexed_files,
                coverage_pct,
                ..
            } => {
                assert_eq!(cached_files, 1);
                assert_eq!(total_indexed_files, 1);
                assert_eq!(coverage_pct, 100);
            }
            other => panic!("expected available summaries, got {other:?}"),
        }
    }

    #[test]
    fn status_summary_coverage_counts_only_extraction_candidates() {
        let dir = TempDir::new().unwrap();
        let source = b"fn main() {}\n";
        std::fs::write(dir.path().join("main.rs"), source).unwrap();
        std::fs::write(dir.path().join("README.md"), "# Indexed documentation\n").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let sdb = SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        sdb.insert(&tsift_summarize::summarize::Summary {
            id: 0,
            symbol_name: "main".to_string(),
            file_path: "main.rs".to_string(),
            content_hash: tsift_summarize::summarize::content_hash(source),
            summary: "Entry point".to_string(),
            entities: None,
            relationships: None,
            concept_labels: None,
            extracted_at: "2026-01-01".to_string(),
            model: "test".to_string(),
            tokens_input: None,
            tokens_output: None,
        })
        .unwrap();

        let report = check_status(dir.path()).unwrap();
        match report.summaries {
            SummaryStatus::Available {
                cached_files,
                total_indexed_files,
                non_candidate_files,
                coverage_pct,
                ..
            } => {
                assert_eq!(cached_files, 1);
                assert_eq!(total_indexed_files, 1);
                assert_eq!(non_candidate_files, 1);
                assert_eq!(coverage_pct, 100);
            }
            other => panic!("expected available summaries, got {other:?}"),
        }
    }

    #[test]
    fn status_does_not_recommend_retrying_current_terminal_failures() {
        let dir = TempDir::new().unwrap();
        let source = b"fn main() {}\n";
        std::fs::write(dir.path().join("main.rs"), source).unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let sdb = SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        sdb.record_terminal_failure(
            "main.rs",
            &tsift_summarize::summarize::content_hash(source),
            tsift_summarize::summarize::ExtractionFailureKind::TooLarge,
            "raise --max-file-tokens",
        )
        .unwrap();

        let report = check_status(dir.path()).unwrap();
        match report.summaries {
            SummaryStatus::Available {
                cached_files,
                total_indexed_files,
                terminal_failure_files,
                ..
            } => {
                assert_eq!(cached_files, 0);
                assert_eq!(total_indexed_files, 1);
                assert_eq!(terminal_failure_files, 1);
            }
            other => panic!("expected terminal failure status, got {other:?}"),
        }
        assert!(
            report
                .recommendations
                .run
                .as_deref()
                .is_none_or(|run| !run.contains("summarize")),
            "terminal failures must not be recommended for automatic retry: {:?}",
            report.recommendations.run
        );
        assert!(
            !report
                .recommendations
                .use_commands
                .contains(&"summarize".to_string())
        );
    }

    #[test]
    fn status_json_roundtrip() {
        let dir = TempDir::new().unwrap();
        let report = check_status(dir.path()).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"state\""));
        assert!(json.contains("\"missing\""));
    }

    #[test]
    fn status_reports_stale_index_reminder() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            dir.path().join("main.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .unwrap();

        let report = check_status(dir.path()).unwrap();

        assert_eq!(report.reminders.len(), 1);
        assert!(report.reminders[0].contains("index stale"));
        assert!(report.reminders[0].contains("tsift index ."));
        assert!(report.reminders[0].contains("no summaries are cached"));
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"reminders\""));
    }

    #[test]
    fn status_human_format_missing() {
        let report = StatusReport {
            index: IndexStatus::Missing {
                missing_scopes: Vec::new(),
            },
            summaries: SummaryStatus::Unavailable,
            instructions: InstructionStatus::Missing,
            scope_instructions: Vec::new(),
            recommendations: Recommendations {
                use_commands: vec![],
                run: Some("tsift init && tsift index .".to_string()),
            },
            reminders: Vec::new(),
            language_coverage: Vec::new(),
        };
        let output = format_human(&report, false);
        assert!(output.contains("index: missing"));
        assert!(output.contains("instructions: missing"));
        assert!(output.contains("summaries: unavailable"));
        assert!(output.contains("use: (none"));
    }

    #[test]
    fn status_human_format_fresh() {
        let report = StatusReport {
            index: IndexStatus::Fresh {
                total_files: 42,
                stale_files: 0,
                last_indexed_secs_ago: 120,
                recovery: None,
                workspace_scopes: Vec::new(),
                missing_scopes: Vec::new(),
            },
            summaries: SummaryStatus::Available {
                cached_files: 30,
                total_indexed_files: 42,
                terminal_failure_files: 0,
                non_candidate_files: 0,
                coverage_pct: 71,
                recovery: None,
            },
            instructions: InstructionStatus::Current {
                version: "0.1.0".to_string(),
            },
            scope_instructions: Vec::new(),
            recommendations: Recommendations {
                use_commands: vec![
                    "search".to_string(),
                    "explain".to_string(),
                    "graph".to_string(),
                    "summarize".to_string(),
                ],
                run: None,
            },
            reminders: Vec::new(),
            language_coverage: Vec::new(),
        };
        let output = format_human(&report, false);
        assert!(output.contains("index: fresh"));
        assert!(output.contains("42 files"));
        assert!(output.contains("instructions: current (v0.1.0)"));
        assert!(output.contains("30/42 extraction candidates cached (71%)"));
        assert!(output.contains("use: search, explain, graph, summarize"));
    }

    #[test]
    fn status_human_format_compact() {
        let report = StatusReport {
            index: IndexStatus::Stale {
                total_files: 42,
                stale_files: 3,
                last_indexed_secs_ago: 120,
                recovery: None,
                workspace_scopes: Vec::new(),
                missing_scopes: Vec::new(),
            },
            summaries: SummaryStatus::None { recovery: None },
            instructions: InstructionStatus::Stale {
                found: Some("0.0.9".to_string()),
                expected: "0.1.0".to_string(),
            },
            scope_instructions: Vec::new(),
            recommendations: Recommendations {
                use_commands: vec![
                    "search".to_string(),
                    "explain".to_string(),
                    "graph".to_string(),
                ],
                run: Some("tsift init && tsift index .".to_string()),
            },
            reminders: build_reminders(
                &IndexStatus::Stale {
                    total_files: 42,
                    stale_files: 3,
                    last_indexed_secs_ago: 120,
                    recovery: None,
                    workspace_scopes: Vec::new(),
                    missing_scopes: Vec::new(),
                },
                &SummaryStatus::None { recovery: None },
                &Recommendations {
                    use_commands: vec![
                        "search".to_string(),
                        "explain".to_string(),
                        "graph".to_string(),
                    ],
                    run: Some("tsift init && tsift index .".to_string()),
                },
                ".",
            ),
            language_coverage: Vec::new(),
        };
        let output = format_human(&report, true);
        assert!(output.contains("index: stale tracked:42 stale:3"));
        assert!(output.contains("instructions: stale v=0.0.9 expected=0.1.0"));
        assert!(output.contains("reminder: index stale"));
        assert!(output.contains("use: search, explain, graph"));
        assert!(!output.contains("recommendations:"));
    }

    #[test]
    fn status_human_format_mentions_snapshot_recovery() {
        let report = StatusReport {
            index: IndexStatus::Fresh {
                total_files: 3,
                stale_files: 0,
                last_indexed_secs_ago: 5,
                recovery: Some(ReadOnlyRecovery::SnapshotFallback),
                workspace_scopes: Vec::new(),
                missing_scopes: Vec::new(),
            },
            summaries: SummaryStatus::None { recovery: None },
            instructions: InstructionStatus::Current {
                version: "0.1.0".to_string(),
            },
            scope_instructions: Vec::new(),
            recommendations: Recommendations {
                use_commands: vec!["search".to_string()],
                run: None,
            },
            reminders: Vec::new(),
            language_coverage: Vec::new(),
        };
        let output = format_human(&report, false);
        assert!(output.contains("recovery: snapshot fallback"));
    }

    #[test]
    fn status_human_format_mentions_wal_snapshot_recovery() {
        let report = StatusReport {
            index: IndexStatus::Fresh {
                total_files: 3,
                stale_files: 0,
                last_indexed_secs_ago: 5,
                recovery: Some(ReadOnlyRecovery::SnapshotFallbackWal),
                workspace_scopes: Vec::new(),
                missing_scopes: Vec::new(),
            },
            summaries: SummaryStatus::None { recovery: None },
            instructions: InstructionStatus::Current {
                version: "0.1.0".to_string(),
            },
            scope_instructions: Vec::new(),
            recommendations: Recommendations {
                use_commands: vec!["search".to_string()],
                run: None,
            },
            reminders: Vec::new(),
            language_coverage: Vec::new(),
        };
        let output = format_human(&report, false);
        assert!(output.contains("copied live WAL sidecars"));
    }

    #[test]
    fn status_json_includes_recovery_when_snapshot_fallback_is_used() {
        let report = StatusReport {
            index: IndexStatus::Fresh {
                total_files: 1,
                stale_files: 0,
                last_indexed_secs_ago: 1,
                recovery: Some(ReadOnlyRecovery::SnapshotFallback),
                workspace_scopes: Vec::new(),
                missing_scopes: Vec::new(),
            },
            summaries: SummaryStatus::None { recovery: None },
            instructions: InstructionStatus::Current {
                version: "0.1.0".to_string(),
            },
            scope_instructions: Vec::new(),
            recommendations: Recommendations {
                use_commands: vec!["search".to_string()],
                run: None,
            },
            reminders: Vec::new(),
            language_coverage: Vec::new(),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"recovery\":\"snapshot_fallback\""));
    }

    #[test]
    fn status_human_format_mentions_summary_snapshot_recovery() {
        let report = StatusReport {
            index: IndexStatus::Fresh {
                total_files: 3,
                stale_files: 0,
                last_indexed_secs_ago: 5,
                recovery: None,
                workspace_scopes: Vec::new(),
                missing_scopes: Vec::new(),
            },
            summaries: SummaryStatus::Available {
                cached_files: 2,
                total_indexed_files: 3,
                terminal_failure_files: 0,
                non_candidate_files: 0,
                coverage_pct: 66,
                recovery: Some(ReadOnlyRecovery::SnapshotFallback),
            },
            instructions: InstructionStatus::Current {
                version: "0.1.0".to_string(),
            },
            scope_instructions: Vec::new(),
            recommendations: Recommendations {
                use_commands: vec!["search".to_string(), "summarize".to_string()],
                run: None,
            },
            reminders: Vec::new(),
            language_coverage: Vec::new(),
        };
        let output = format_human(&report, false);
        assert!(output.contains("summaries recovery: snapshot fallback"));
    }

    #[test]
    fn status_json_includes_summary_recovery_when_snapshot_fallback_is_used() {
        let report = StatusReport {
            index: IndexStatus::Fresh {
                total_files: 1,
                stale_files: 0,
                last_indexed_secs_ago: 1,
                recovery: None,
                workspace_scopes: Vec::new(),
                missing_scopes: Vec::new(),
            },
            summaries: SummaryStatus::None {
                recovery: Some(ReadOnlyRecovery::SnapshotFallback),
            },
            instructions: InstructionStatus::Current {
                version: "0.1.0".to_string(),
            },
            scope_instructions: Vec::new(),
            recommendations: Recommendations {
                use_commands: vec!["search".to_string()],
                run: None,
            },
            reminders: Vec::new(),
            language_coverage: Vec::new(),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"state\":\"none\""));
        assert!(json.contains("\"recovery\":\"snapshot_fallback\""));
    }

    #[test]
    fn lock_report_marks_live_writer_and_journal() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".tsift/index.lock");
        let journal_path = dir.path().join(".tsift/index.db-journal");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let mut lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        assert!(lock_file.try_lock_exclusive().unwrap());
        use std::io::Write;
        writeln!(lock_file, "{}", std::process::id()).unwrap();
        std::fs::write(&journal_path, "locked").unwrap();

        let report = check_locks(dir.path(), None, None).unwrap();
        assert!(matches!(
            report.writer_lock,
            WriterLockStatus::Live { pid: Some(_), .. }
        ));
        assert!(report.rollback_journal.present);
        assert!(
            report
                .recommended_action
                .contains("wait for the active tsift writer")
        );
        assert!(report.recommended_action.contains("tsift index"));
    }

    #[test]
    fn lock_report_marks_live_writer_and_wal_sidecars() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join(".tsift/index.db");
        let db = IndexDb::open(&db_path).unwrap();
        drop(db);

        let _lock = hold_wal_lock(&db_path);

        let report = check_locks(dir.path(), None, None).unwrap();
        assert!(report.wal_sidecar.present);
        assert!(
            report
                .recommended_action
                .contains("wedged writer holding live WAL sidecars")
        );
    }

    #[test]
    fn lock_report_marks_stale_writer_lock() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".tsift/index.lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, "999999").unwrap();

        let report = check_locks(dir.path(), None, None).unwrap();
        assert!(matches!(
            report.writer_lock,
            WriterLockStatus::Stale {
                pid: Some(999999),
                ..
            }
        ));
        assert!(report.recommended_action.contains("reuse it automatically"));
        assert!(report.recommended_action.contains("tsift index"));
    }

    #[test]
    fn status_instructions_stale_recommends_init() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "<!-- tsift:code-navigation -->\n## Code Navigation\nOld.\n<!-- /tsift:code-navigation -->\n",
        )
        .unwrap();
        let report = check_status(dir.path()).unwrap();
        assert!(matches!(
            report.instructions,
            InstructionStatus::Stale { found: None, .. }
        ));
        assert!(
            report
                .recommendations
                .run
                .as_deref()
                .unwrap()
                .contains("tsift init")
        );
    }

    #[test]
    fn status_instructions_current_after_init() {
        let dir = TempDir::new().unwrap();
        init::init(dir.path(), false, false).unwrap();
        let report = check_status(dir.path()).unwrap();
        assert!(matches!(
            report.instructions,
            InstructionStatus::Current { .. }
        ));
    }
}
