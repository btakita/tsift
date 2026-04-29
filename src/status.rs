use crate::config;
use crate::index::{
    IndexDb, ReadOnlyRecovery, WriterLockProbe, probe_writer_lock, rollback_journal_path,
    writer_lock_path,
};
use crate::init::{self, InstructionStatus};
use crate::summarize::SummaryDb;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub index: IndexStatus,
    pub summaries: SummaryStatus,
    pub instructions: InstructionStatus,
    pub recommendations: Recommendations,
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
    pub rollback_journal: RollbackJournalStatus,
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
pub struct RollbackJournalStatus {
    pub path: PathBuf,
    pub present: bool,
}

pub fn check_status(root: &Path) -> Result<StatusReport> {
    let workspace = !config::Config::submodule_dirs(root)?.is_empty();
    let summaries_db_path = root.join(".tsift/summaries.db");

    let index = check_index(root)?;
    let summaries = check_summaries(root, &summaries_db_path, &index)?;
    let instructions = init::check_instruction_version(root);
    let recommendations = build_recommendations(&index, &summaries, &instructions, workspace);

    Ok(StatusReport {
        index,
        summaries,
        instructions,
        recommendations,
    })
}

fn check_index(root: &Path) -> Result<IndexStatus> {
    if !config::Config::submodule_dirs(root)?.is_empty() {
        return check_workspace_index(root);
    }

    check_single_index(root)
}

fn check_single_index(root: &Path) -> Result<IndexStatus> {
    let db_path = root.join(".tsift/index.db");
    if !db_path.exists() {
        return check_workspace_index(root);
    }

    let last_indexed_secs_ago = db_path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let inspection = IndexDb::inspect_read_only(&db_path, root, false)?;
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

fn check_workspace_index(root: &Path) -> Result<IndexStatus> {
    let cfg = config::Config::load(root)?;
    let mut scopes = Vec::new();
    let mut missing_scopes = Vec::new();
    for scope in config::Config::submodule_dirs(root)? {
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
        let inspection = IndexDb::inspect_read_only(&db_path, &scope.source_root, false)?;
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
    let rollback_journal = RollbackJournalStatus {
        path: rollback_journal_path(&db_path),
        present: rollback_journal_path(&db_path).exists(),
    };
    let recommended_action =
        build_lock_recommendation(&writer_lock, &rollback_journal, &reindex_command);

    Ok(LockReport {
        label,
        source_root,
        db_path,
        writer_lock,
        rollback_journal,
        reindex_command,
        recommended_action,
    })
}

fn check_summaries(root: &Path, db_path: &Path, index: &IndexStatus) -> Result<SummaryStatus> {
    if matches!(index, IndexStatus::Missing { .. }) {
        return Ok(SummaryStatus::Unavailable);
    }
    if !db_path.exists() {
        return Ok(SummaryStatus::None { recovery: None });
    }

    let read_only = SummaryDb::open_read_only_with_recovery(db_path)?;
    let recovery = read_only.recovery;
    let db = read_only.db;
    let cached_summary_paths = db.cached_file_paths()?.into_iter().collect::<HashSet<_>>();
    let live_indexed_files = live_indexed_summary_paths(root, index)?;
    let total_indexed_files = live_indexed_files.len();
    let cached_files = cached_summary_paths
        .intersection(&live_indexed_files)
        .count();

    if cached_files == 0 {
        return Ok(SummaryStatus::None { recovery });
    }

    let coverage_pct = if total_indexed_files > 0 {
        ((cached_files as f64 / total_indexed_files as f64) * 100.0).min(100.0) as u8
    } else {
        0
    };

    Ok(SummaryStatus::Available {
        cached_files,
        total_indexed_files,
        coverage_pct,
        recovery,
    })
}

fn live_indexed_summary_paths(root: &Path, index: &IndexStatus) -> Result<HashSet<String>> {
    match index {
        IndexStatus::Fresh {
            workspace_scopes, ..
        }
        | IndexStatus::Stale {
            workspace_scopes, ..
        } => {
            if workspace_scopes.is_empty() {
                tracked_summary_paths_from_index(&root.join(".tsift/index.db"), root)
            } else {
                let mut paths = HashSet::new();
                for scope in workspace_scopes {
                    paths.extend(tracked_summary_paths_from_index(&scope.db_path, root)?);
                }
                Ok(paths)
            }
        }
        IndexStatus::Missing { .. } => Ok(HashSet::new()),
    }
}

fn tracked_summary_paths_from_index(db_path: &Path, root: &Path) -> Result<HashSet<String>> {
    let tracked = IndexDb::file_paths_read_only(db_path)?;
    Ok(tracked
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .map(|path| {
            path.strip_prefix(root)
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
    workspace: bool,
) -> Recommendations {
    let refresh = !matches!(instructions, InstructionStatus::Current { .. });
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
            let mut run = match summaries {
                SummaryStatus::Available {
                    cached_files,
                    total_indexed_files,
                    ..
                } => {
                    use_cmds.push("summarize".to_string());
                    let uncached = total_indexed_files.saturating_sub(*cached_files);
                    if uncached > 0 {
                        Some(format!(
                            "tsift summarize --extract src/  ({} uncached file{})",
                            uncached,
                            if uncached == 1 { "" } else { "s" }
                        ))
                    } else {
                        None
                    }
                }
                SummaryStatus::None { .. } => Some("tsift summarize --extract src/".to_string()),
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
    rollback_journal: &RollbackJournalStatus,
    reindex_command: &str,
) -> String {
    match writer_lock {
        WriterLockStatus::Live { pid, .. } => {
            let pid_hint = pid
                .map(|value| format!(" (pid {})", value))
                .unwrap_or_default();
            if rollback_journal.present {
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
        WriterLockStatus::Absent { .. } if rollback_journal.present => format!(
            "inspect the host for a wedged writer, then run `{}` once writes are healthy. Read-only status checks can use snapshot fallback in the meantime.",
            reindex_command
        ),
        WriterLockStatus::Absent { .. } => "no lock remediation needed".to_string(),
    }
}

fn format_recovery_line(recovery: ReadOnlyRecovery, compact: bool) -> String {
    match (recovery, compact) {
        (ReadOnlyRecovery::SnapshotFallback, true) => "recovery:snapshot_fallback\n".to_string(),
        (ReadOnlyRecovery::SnapshotFallback, false) => {
            "recovery: snapshot fallback (rollback-journal lock on live index)\n".to_string()
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

    match &report.summaries {
        SummaryStatus::Available {
            cached_files,
            total_indexed_files,
            coverage_pct,
            ..
        } => {
            if compact {
                out.push_str(&format!(
                    "summaries: {}/{} ({}%)\n",
                    cached_files, total_indexed_files, coverage_pct
                ));
            } else {
                out.push_str(&format!(
                    "summaries: {}/{} files cached ({}%)\n",
                    cached_files, total_indexed_files, coverage_pct
                ));
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

    if compact {
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

    let mut out = String::new();
    if compact {
        out.push_str(&format!(
            "target:{} db:{}\n",
            report.label,
            report.db_path.display()
        ));
        out.push_str(&lock_line);
        out.push_str(&journal_line);
        out.push_str(&format!("run:{}\n", report.reindex_command));
        out.push_str(&format!("next:{}\n", report.recommended_action));
    } else {
        out.push_str(&format!("target: {}\n", report.label));
        out.push_str(&format!("source: {}\n", report.source_root.display()));
        out.push_str(&format!("db: {}\n", report.db_path.display()));
        out.push_str(&lock_line);
        out.push_str(&journal_line);
        out.push_str(&format!("run: {}\n", report.reindex_command));
        out.push_str(&format!("next: {}\n", report.recommended_action));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use fs4::fs_std::FileExt;
    use rusqlite::Connection;
    use std::fs::OpenOptions;
    use tempfile::TempDir;

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
    }

    #[test]
    fn status_workspace_scoped_indexes_report_fresh() {
        let dir = setup_workspace();
        let cfg = Config::load(dir.path()).unwrap();
        for scope in Config::submodule_dirs(dir.path()).unwrap() {
            let db = IndexDb::open(&cfg.db_path_for(dir.path(), &scope.id)).unwrap();
            db.apply_changes(&scope.source_root).unwrap();
        }

        let report = check_status(dir.path()).unwrap();
        match &report.index {
            IndexStatus::Fresh {
                total_files,
                workspace_scopes,
                ..
            } => {
                assert_eq!(*total_files, 2);
                assert_eq!(workspace_scopes.len(), 2);
                assert_eq!(workspace_scopes[0].scope, "alpha");
                assert_eq!(workspace_scopes[1].scope, "beta");
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
    fn status_workspace_missing_recommends_workspace_index() {
        let dir = setup_workspace();

        let report = check_status(dir.path()).unwrap();
        match &report.index {
            IndexStatus::Missing { missing_scopes } => {
                assert_eq!(missing_scopes.len(), 2);
                assert_eq!(missing_scopes[0].scope, "alpha");
                assert_eq!(missing_scopes[1].scope, "beta");
            }
            other => panic!("expected missing workspace status, got {other:?}"),
        }
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init --workspace && tsift index --workspace .  (2 missing scopes)")
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
                assert_eq!(missing_scopes.len(), 1);
                assert_eq!(missing_scopes[0].scope, "beta");
            }
            other => panic!("expected partial workspace status, got {other:?}"),
        }
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift init --workspace && tsift index --workspace .  (1 missing scope)")
        );
    }

    #[test]
    fn status_workspace_prefers_scoped_indexes_when_root_index_also_exists() {
        let dir = setup_workspace();
        let root_db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        root_db.apply_changes(dir.path()).unwrap();

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
                assert_eq!(*total_files, 1);
                assert_eq!(*stale_files, 0);
                assert_eq!(workspace_scopes.len(), 1);
                assert_eq!(workspace_scopes[0].scope, "alpha");
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
        let cfg = Config::load(dir.path()).unwrap();
        for scope in Config::submodule_dirs(dir.path()).unwrap() {
            let db = IndexDb::open(&cfg.db_path_for(dir.path(), &scope.id)).unwrap();
            db.apply_changes(&scope.source_root).unwrap();
        }
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
                assert_eq!(workspace_scopes.len(), 2);
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
        sdb.insert(&crate::summarize::Summary {
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
    fn status_summary_coverage_ignores_deleted_summary_rows() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        let db = IndexDb::open(&dir.path().join(".tsift/index.db")).unwrap();
        db.apply_changes(dir.path()).unwrap();

        let sdb = SummaryDb::open(&dir.path().join(".tsift/summaries.db")).unwrap();
        sdb.insert(&crate::summarize::Summary {
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
        sdb.insert(&crate::summarize::Summary {
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
    fn status_json_roundtrip() {
        let dir = TempDir::new().unwrap();
        let report = check_status(dir.path()).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"state\""));
        assert!(json.contains("\"missing\""));
    }

    #[test]
    fn status_human_format_missing() {
        let report = StatusReport {
            index: IndexStatus::Missing {
                missing_scopes: Vec::new(),
            },
            summaries: SummaryStatus::Unavailable,
            instructions: InstructionStatus::Missing,
            recommendations: Recommendations {
                use_commands: vec![],
                run: Some("tsift init && tsift index .".to_string()),
            },
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
                coverage_pct: 71,
                recovery: None,
            },
            instructions: InstructionStatus::Current {
                version: "0.1.0".to_string(),
            },
            recommendations: Recommendations {
                use_commands: vec![
                    "search".to_string(),
                    "explain".to_string(),
                    "graph".to_string(),
                    "summarize".to_string(),
                ],
                run: None,
            },
        };
        let output = format_human(&report, false);
        assert!(output.contains("index: fresh"));
        assert!(output.contains("42 files"));
        assert!(output.contains("instructions: current (v0.1.0)"));
        assert!(output.contains("30/42 files cached (71%)"));
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
            recommendations: Recommendations {
                use_commands: vec![
                    "search".to_string(),
                    "explain".to_string(),
                    "graph".to_string(),
                ],
                run: Some("tsift init && tsift index .".to_string()),
            },
        };
        let output = format_human(&report, true);
        assert!(output.contains("index: stale tracked:42 stale:3"));
        assert!(output.contains("instructions: stale v=0.0.9 expected=0.1.0"));
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
            recommendations: Recommendations {
                use_commands: vec!["search".to_string()],
                run: None,
            },
        };
        let output = format_human(&report, false);
        assert!(output.contains("recovery: snapshot fallback"));
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
            recommendations: Recommendations {
                use_commands: vec!["search".to_string()],
                run: None,
            },
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
                coverage_pct: 66,
                recovery: Some(ReadOnlyRecovery::SnapshotFallback),
            },
            instructions: InstructionStatus::Current {
                version: "0.1.0".to_string(),
            },
            recommendations: Recommendations {
                use_commands: vec!["search".to_string(), "summarize".to_string()],
                run: None,
            },
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
            recommendations: Recommendations {
                use_commands: vec!["search".to_string()],
                run: None,
            },
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
