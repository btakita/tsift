use crate::index::IndexDb;
use crate::summarize::SummaryDb;
use anyhow::Result;
use serde::Serialize;
use std::path::Path;
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
    },
    #[serde(rename = "stale")]
    Stale {
        total_files: usize,
        stale_files: usize,
        last_indexed_secs_ago: u64,
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

    let db = IndexDb::open(db_path)?;
    let total_files = db.file_count()?;
    let summary = db.compute_changes(root)?;
    let stale_files = summary.new + summary.modified + summary.deleted;

    if stale_files > 0 {
        Ok(IndexStatus::Stale {
            total_files,
            stale_files,
            last_indexed_secs_ago,
        })
    } else {
        Ok(IndexStatus::Fresh {
            total_files,
            stale_files: 0,
            last_indexed_secs_ago,
        })
    }
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

    let db = SummaryDb::open(db_path)?;
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
        IndexStatus::Stale {
            stale_files, ..
        } => {
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
                SummaryStatus::None => {
                    Some("tsift summarize --extract src/".to_string())
                }
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

pub fn format_human(report: &StatusReport) -> String {
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
            out.push_str(&format!(
                "index: fresh (last indexed {}, {} files tracked)\n",
                format_duration(*last_indexed_secs_ago),
                total_files
            ));
        }
        IndexStatus::Stale {
            total_files,
            stale_files,
            last_indexed_secs_ago,
        } => {
            out.push_str(&format!(
                "index: stale (last indexed {}, {} files tracked, {} stale)\n",
                format_duration(*last_indexed_secs_ago),
                total_files,
                stale_files
            ));
        }
    }

    match &report.summaries {
        SummaryStatus::Available {
            cached_files,
            total_indexed_files,
            coverage_pct,
        } => {
            out.push_str(&format!(
                "summaries: {}/{} files cached ({}%)\n",
                cached_files, total_indexed_files, coverage_pct
            ));
        }
        SummaryStatus::None => {
            out.push_str("summaries: none\n");
        }
        SummaryStatus::Unavailable => {
            out.push_str("summaries: unavailable (no index)\n");
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
        assert_eq!(
            report.recommendations.run.as_deref(),
            Some("tsift index .")
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
            IndexStatus::Fresh {
                stale_files: 0,
                ..
            }
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
        assert!(matches!(report.index, IndexStatus::Stale { stale_files: 1, .. }));
        assert!(report.recommendations.run.as_deref().unwrap().contains("tsift index"));
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
        }).unwrap();

        let report = check_status(dir.path()).unwrap();
        assert!(matches!(report.index, IndexStatus::Fresh { .. }));
        assert!(matches!(report.summaries, SummaryStatus::Available { .. }));
        assert!(report.recommendations.use_commands.contains(&"summarize".to_string()));
    }

    #[test]
    fn status_json_roundtrip() {
        let dir = TempDir::new().unwrap();
        let report = check_status(dir.path()).unwrap();
        let json = serde_json::to_string_pretty(&report).unwrap();
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
        let output = format_human(&report);
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
        let output = format_human(&report);
        assert!(output.contains("index: fresh"));
        assert!(output.contains("42 files"));
        assert!(output.contains("30/42 files cached (71%)"));
        assert!(output.contains("use: search, explain, graph, summarize"));
    }
}
