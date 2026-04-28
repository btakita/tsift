use crate::config;
use crate::index::{
    IndexDb, ReadOnlyRecovery, process_exists, read_lock_pid, rollback_journal_path,
    writer_lock_path,
};
use crate::summarize::SummaryDb;
use anyhow::{Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub index: IndexStatus,
    pub summaries: SummaryStatus,
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
    },
    #[serde(rename = "stale")]
    Stale {
        total_files: usize,
        stale_files: usize,
        last_indexed_secs_ago: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        recovery: Option<ReadOnlyRecovery>,
    },
    #[serde(rename = "missing")]
    Missing,
}

#[derive(Debug, Serialize)]
#[serde(tag = "state")]
pub enum SummaryStatus {
    #[serde(rename = "available")]
    Available {
        cached_files: usize,
        total_indexed_files: usize,
        coverage_pct: u8,
    },
    #[serde(rename = "none")]
    None,
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
    let index_db_path = root.join(".tsift/index.db");
    let summaries_db_path = root.join(".tsift/summaries.db");

    let index = check_index(&index_db_path, root)?;
    let summaries = check_summaries(&summaries_db_path, &index)?;
    let recommendations = build_recommendations(&index, &summaries);

    Ok(StatusReport {
        index,
        summaries,
        recommendations,
    })
}

fn check_index(db_path: &Path, root: &Path) -> Result<IndexStatus> {
    if !db_path.exists() {
        return Ok(IndexStatus::Missing);
    }

    let last_indexed_secs_ago = db_path
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let inspection = IndexDb::inspect_read_only(db_path, root, false)?;
    let stale_files =
        inspection.summary.new + inspection.summary.modified + inspection.summary.deleted;

    if stale_files > 0 {
        Ok(IndexStatus::Stale {
            total_files: inspection.total_files,
            stale_files,
            last_indexed_secs_ago,
            recovery: inspection.recovery,
        })
    } else {
        Ok(IndexStatus::Fresh {
            total_files: inspection.total_files,
            stale_files: 0,
            last_indexed_secs_ago,
            recovery: inspection.recovery,
        })
    }
}

pub fn check_locks(root: &Path, scope: Option<&str>) -> Result<LockReport> {
    let (label, source_root, db_path, reindex_command) = resolve_lock_target(root, scope)?;
    let lock_path = writer_lock_path(&db_path);
    let writer_lock = if !lock_path.exists() {
        WriterLockStatus::Absent { path: lock_path }
    } else {
        match read_lock_pid(&lock_path) {
            Some(pid) if process_exists(pid) => WriterLockStatus::Live {
                path: lock_path,
                pid: Some(pid),
            },
            Some(pid) => WriterLockStatus::Stale {
                path: lock_path,
                pid: Some(pid),
            },
            None => WriterLockStatus::Unknown { path: lock_path },
        }
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

fn check_summaries(db_path: &Path, index: &IndexStatus) -> Result<SummaryStatus> {
    let total_indexed_files = match index {
        IndexStatus::Fresh { total_files, .. } | IndexStatus::Stale { total_files, .. } => {
            *total_files
        }
        IndexStatus::Missing => return Ok(SummaryStatus::Unavailable),
    };

    if !db_path.exists() {
        return Ok(SummaryStatus::None);
    }

    let db = SummaryDb::open_read_only(db_path)?;
    let stats = db.stats()?;
    let cached_files = stats.total_files;

    if cached_files == 0 {
        return Ok(SummaryStatus::None);
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
    })
}

fn build_recommendations(index: &IndexStatus, summaries: &SummaryStatus) -> Recommendations {
    match index {
        IndexStatus::Missing => Recommendations {
            use_commands: vec![],
            run: Some("tsift index .".to_string()),
        },
        IndexStatus::Stale { stale_files, .. } => {
            let mut use_cmds = vec![
                "search".to_string(),
                "explain".to_string(),
                "graph".to_string(),
            ];
            if matches!(summaries, SummaryStatus::Available { .. }) {
                use_cmds.push("summarize".to_string());
            }
            Recommendations {
                use_commands: use_cmds,
                run: Some(format!(
                    "tsift index .  ({} stale file{})",
                    stale_files,
                    if *stale_files == 1 { "" } else { "s" }
                )),
            }
        }
        IndexStatus::Fresh { .. } => {
            let mut use_cmds = vec![
                "search".to_string(),
                "explain".to_string(),
                "graph".to_string(),
            ];
            let run = match summaries {
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
                SummaryStatus::None => Some("tsift summarize --extract src/".to_string()),
                SummaryStatus::Unavailable => None,
            };
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
    scope: Option<&str>,
) -> Result<(String, PathBuf, PathBuf, String)> {
    if let Some(scope_name) = scope {
        let cfg = config::Config::load(root)?;
        let Some(source_root) = config::Config::submodule_dirs(root)?
            .into_iter()
            .find(|(name, _)| name == scope_name)
            .map(|(_, path)| path)
        else {
            bail!(
                "no submodule named `{}` found under {}",
                scope_name,
                root.display()
            );
        };
        Ok((
            format!("submodule `{}` index", scope_name),
            source_root,
            cfg.db_path_for(root, scope_name),
            format!("tsift index --submodule {} {}", scope_name, root.display()),
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
                "if no tsift writer is still active, remove `{}` and then run `{}`.",
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
        IndexStatus::Missing => None,
    }
}

pub fn format_human(report: &StatusReport, compact: bool) -> String {
    let mut out = String::new();

    match &report.index {
        IndexStatus::Missing => {
            out.push_str("index: missing\n");
        }
        IndexStatus::Fresh {
            total_files,
            last_indexed_secs_ago,
            ..
        } => {
            if compact {
                out.push_str(&format!(
                    "index: fresh tracked:{} age:{}\n",
                    total_files,
                    format_duration(*last_indexed_secs_ago)
                ));
            } else {
                out.push_str(&format!(
                    "index: fresh (last indexed {}, {} files tracked)\n",
                    format_duration(*last_indexed_secs_ago),
                    total_files
                ));
            }
        }
        IndexStatus::Stale {
            total_files,
            stale_files,
            last_indexed_secs_ago,
            ..
        } => {
            if compact {
                out.push_str(&format!(
                    "index: stale tracked:{} stale:{} age:{}\n",
                    total_files,
                    stale_files,
                    format_duration(*last_indexed_secs_ago)
                ));
            } else {
                out.push_str(&format!(
                    "index: stale (last indexed {}, {} files tracked, {} stale)\n",
                    format_duration(*last_indexed_secs_ago),
                    total_files,
                    stale_files
                ));
            }
        }
    }

    if let Some(recovery) = index_recovery(&report.index) {
        out.push_str(&format_recovery_line(recovery, compact));
    }

    match &report.summaries {
        SummaryStatus::Available {
            cached_files,
            total_indexed_files,
            coverage_pct,
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
        SummaryStatus::None => {
            out.push_str("summaries: none\n");
        }
        SummaryStatus::Unavailable => {
            out.push_str("summaries: unavailable (no index)\n");
        }
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
    use tempfile::TempDir;

    #[test]
    fn status_no_index() {
        let dir = TempDir::new().unwrap();
        let report = check_status(dir.path()).unwrap();
        assert!(matches!(report.index, IndexStatus::Missing));
        assert!(matches!(report.summaries, SummaryStatus::Unavailable));
        assert!(report.recommendations.use_commands.is_empty());
        assert_eq!(report.recommendations.run.as_deref(), Some("tsift index ."));
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
        assert!(matches!(report.summaries, SummaryStatus::None));
        let cmds = &report.recommendations.use_commands;
        assert!(cmds.contains(&"search".to_string()));
        assert!(cmds.contains(&"explain".to_string()));
        assert!(cmds.contains(&"graph".to_string()));
        assert!(!cmds.contains(&"summarize".to_string()));
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
            index: IndexStatus::Missing,
            summaries: SummaryStatus::Unavailable,
            recommendations: Recommendations {
                use_commands: vec![],
                run: Some("tsift index .".to_string()),
            },
        };
        let output = format_human(&report, false);
        assert!(output.contains("index: missing"));
        assert!(output.contains("summaries: unavailable"));
        assert!(output.contains("use: (none"));
        assert!(output.contains("run: tsift index ."));
    }

    #[test]
    fn status_human_format_fresh() {
        let report = StatusReport {
            index: IndexStatus::Fresh {
                total_files: 42,
                stale_files: 0,
                last_indexed_secs_ago: 120,
                recovery: None,
            },
            summaries: SummaryStatus::Available {
                cached_files: 30,
                total_indexed_files: 42,
                coverage_pct: 71,
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
            },
            summaries: SummaryStatus::None,
            recommendations: Recommendations {
                use_commands: vec![
                    "search".to_string(),
                    "explain".to_string(),
                    "graph".to_string(),
                ],
                run: Some("tsift index .".to_string()),
            },
        };
        let output = format_human(&report, true);
        assert!(output.contains("index: stale tracked:42 stale:3"));
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
            },
            summaries: SummaryStatus::None,
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
            },
            summaries: SummaryStatus::None,
            recommendations: Recommendations {
                use_commands: vec!["search".to_string()],
                run: None,
            },
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"recovery\":\"snapshot_fallback\""));
    }

    #[test]
    fn lock_report_marks_live_writer_and_journal() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".tsift/index.lock");
        let journal_path = dir.path().join(".tsift/index.db-journal");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, std::process::id().to_string()).unwrap();
        std::fs::write(&journal_path, "locked").unwrap();

        let report = check_locks(dir.path(), None).unwrap();
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

        let report = check_locks(dir.path(), None).unwrap();
        assert!(matches!(
            report.writer_lock,
            WriterLockStatus::Stale {
                pid: Some(999999),
                ..
            }
        ));
        assert!(report.recommended_action.contains("remove"));
        assert!(report.recommended_action.contains("tsift index"));
    }
}
