//! Cycle-scoped packet reuse cache (#gpackreuse).
//!
//! Caches graph/context packets across a single agent-doc cycle so that
//! repeated tsift CLI invocations sharing the same source/document/staged-diff
//! watermarks skip redundant computation and report stable packet ids.
//!
//! Packet kinds:
//! - `evidence`: graph-db evidence reports keyed by `packet_id`
//! - `context_pack`: context-pack reports keyed by watermark triple
//! - `impact`: impact reports keyed by source watermark + revision
//! - `conflict_matrix`: conflict-matrix reports keyed by prepared inputs + targets
//!
//! Spec: see specs/graph.md § "Cycle Packet Cache".

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const CYCLE_PACKET_CACHE_VERSION: &str = "cycle-packet-cache-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CyclePacketKind {
    Evidence,
    ContextPack,
    Impact,
    ConflictMatrix,
}

impl CyclePacketKind {
    pub fn dir_name(&self) -> &'static str {
        match self {
            CyclePacketKind::Evidence => "evidence",
            CyclePacketKind::ContextPack => "context-pack",
            CyclePacketKind::Impact => "impact",
            CyclePacketKind::ConflictMatrix => "conflict-matrix",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CyclePacketCacheEntry {
    pub version: String,
    pub kind: CyclePacketKind,
    pub key: String,
    pub packet_id: String,
    pub source_watermark: String,
    pub document_watermark: String,
    pub staged_diff_watermark: String,
    pub skipped_phases: Vec<String>,
    pub compute_micros: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CyclePacketCacheHitReport {
    pub kind: CyclePacketKind,
    pub key: String,
    pub packet_id: String,
    pub hit_status: String,
    pub skipped_phases: Vec<String>,
    pub lookup_micros: u128,
}

pub fn cycle_packet_cache_dir(root: &Path) -> PathBuf {
    root.join(".tsift/cycle-packet-cache")
}

pub fn cycle_packet_cache_path(root: &Path, kind: CyclePacketKind, key: &str) -> PathBuf {
    cycle_packet_cache_dir(root)
        .join(kind.dir_name())
        .join(format!("{key}.json"))
}

pub fn cycle_packet_read_cache<T: for<'de> Deserialize<'de>>(
    root: &Path,
    kind: CyclePacketKind,
    key: &str,
) -> Option<T> {
    let path = cycle_packet_cache_path(root, kind, key);
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn cycle_packet_write_cache<T: Serialize>(
    root: &Path,
    kind: CyclePacketKind,
    key: &str,
    value: &T,
) {
    let path = cycle_packet_cache_path(root, kind, key);
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(value) {
        let _ = fs::write(path, bytes);
    }
}

pub fn cycle_packet_watermark_key(
    source_watermark: &str,
    document_watermark: &str,
    staged_diff_watermark: &str,
    extra: &[&str],
) -> String {
    let mut parts = vec![
        format!("version:{CYCLE_PACKET_CACHE_VERSION}"),
        format!("source:{source_watermark}"),
        format!("document:{document_watermark}"),
        format!("staged_diff:{staged_diff_watermark}"),
    ];
    for e in extra {
        parts.push(e.to_string());
    }
    blake3::hash(parts.join("\n").as_bytes())
        .to_hex()
        .to_string()
}

pub fn cycle_packet_evidence_key(packet_id: &str) -> String {
    cycle_packet_watermark_key("evidence", "evidence", "evidence", &[packet_id])
}

pub fn build_cache_hit_report(
    kind: CyclePacketKind,
    key: &str,
    packet_id: &str,
    status: &str,
    skipped_phases: &[&str],
    lookup_micros: u128,
) -> CyclePacketCacheHitReport {
    CyclePacketCacheHitReport {
        kind,
        key: key.to_string(),
        packet_id: packet_id.to_string(),
        hit_status: status.to_string(),
        skipped_phases: skipped_phases.iter().map(|s| s.to_string()).collect(),
        lookup_micros,
    }
}

pub const CYCLE_PACKET_CACHE_DEFAULT_TTL_SECS: u64 = 24 * 60 * 60;
pub const CYCLE_PACKET_CACHE_DEFAULT_MAX_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CyclePacketCacheEvictionReport {
    pub scanned_entries: usize,
    pub evicted_entries: usize,
    pub evicted_bytes: u64,
    pub remaining_entries: usize,
    pub remaining_bytes: u64,
    pub ttl_secs: u64,
    pub max_bytes: u64,
}

pub fn cycle_packet_cache_stats(root: &Path) -> (usize, u64) {
    let cache_dir = cycle_packet_cache_dir(root);
    if !cache_dir.exists() {
        return (0, 0);
    }
    let mut count = 0usize;
    let mut total_bytes = 0u64;
    if let Ok(entries) = fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Ok(files) = fs::read_dir(entry.path()) {
                    for file in files.flatten() {
                        if file.path().extension().is_some_and(|ext| ext == "json")
                            && let Ok(meta) = file.metadata() {
                                count += 1;
                                total_bytes += meta.len();
                            }
                    }
                }
        }
    }
    (count, total_bytes)
}

pub fn cycle_packet_cache_evict(
    root: &Path,
    ttl_secs: u64,
    max_bytes: u64,
) -> CyclePacketCacheEvictionReport {
    let cache_dir = cycle_packet_cache_dir(root);
    if !cache_dir.exists() {
        return CyclePacketCacheEvictionReport {
            scanned_entries: 0,
            evicted_entries: 0,
            evicted_bytes: 0,
            remaining_entries: 0,
            remaining_bytes: 0,
            ttl_secs,
            max_bytes,
        };
    }
    let now = SystemTime::now();
    let cutoff = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(ttl_secs);
    let mut all_files: Vec<(PathBuf, u64, u64)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && let Ok(files) = fs::read_dir(entry.path()) {
                    for file in files.flatten() {
                        let path = file.path();
                        if path.extension().is_some_and(|ext| ext == "json")
                            && let Ok(meta) = file.metadata() {
                                let size = meta.len();
                                let mtime_secs = meta
                                    .modified()
                                    .ok()
                                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                                    .map(|d| d.as_secs())
                                    .unwrap_or(u64::MAX);
                                all_files.push((path, size, mtime_secs));
                            }
                    }
                }
        }
    }
    let scanned = all_files.len();
    all_files.sort_by_key(|(_, _, mtime)| *mtime);
    let mut evicted = 0usize;
    let mut evicted_bytes = 0u64;
    for (path, size, mtime) in &all_files {
        if *mtime < cutoff {
            let _ = fs::remove_file(path);
            evicted += 1;
            evicted_bytes += size;
        }
    }
    let remaining: u64 = all_files
        .iter()
        .filter(|(_, _, mtime)| *mtime >= cutoff)
        .map(|(_, size, _)| *size)
        .sum();
    if remaining > max_bytes {
        let expired: Vec<_> = all_files
            .iter()
            .filter(|(_, _, mtime)| *mtime >= cutoff)
            .collect();
        let mut kept_bytes = 0u64;
        for (path, size, _) in expired {
            if kept_bytes.saturating_add(*size) > max_bytes {
                let _ = fs::remove_file(path);
                evicted += 1;
                evicted_bytes += size;
            } else {
                kept_bytes = kept_bytes.saturating_add(*size);
            }
        }
    }
    let (remaining_count, remaining_bytes) = cycle_packet_cache_stats(root);
    CyclePacketCacheEvictionReport {
        scanned_entries: scanned,
        evicted_entries: evicted,
        evicted_bytes,
        remaining_entries: remaining_count,
        remaining_bytes,
        ttl_secs,
        max_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_packet_watermark_key_is_stable() {
        let a = cycle_packet_watermark_key("s1", "d1", "sd1", &["extra"]);
        let b = cycle_packet_watermark_key("s1", "d1", "sd1", &["extra"]);
        assert_eq!(a, b);
    }

    #[test]
    fn cycle_packet_watermark_key_differs_for_different_inputs() {
        let a = cycle_packet_watermark_key("s1", "d1", "sd1", &["extra"]);
        let b = cycle_packet_watermark_key("s2", "d1", "sd1", &["extra"]);
        assert_ne!(a, b);
    }

    #[test]
    fn cycle_packet_evidence_key_is_stable() {
        let a = cycle_packet_evidence_key("gevd:abc123");
        let b = cycle_packet_evidence_key("gevd:abc123");
        assert_eq!(a, b);
    }

    #[test]
    fn cycle_packet_evidence_key_differs_for_different_ids() {
        let a = cycle_packet_evidence_key("gevd:abc123");
        let b = cycle_packet_evidence_key("gevd:def456");
        assert_ne!(a, b);
    }

    #[test]
    fn cache_dir_uses_tsift_subdirectory() {
        let dir = cycle_packet_cache_dir(Path::new("/project"));
        assert_eq!(
            dir,
            PathBuf::from("/project/.tsift/cycle-packet-cache")
        );
    }

    #[test]
    fn cache_path_includes_kind_and_key() {
        let path = cycle_packet_cache_path(
            Path::new("/project"),
            CyclePacketKind::Evidence,
            "abc123",
        );
        assert_eq!(
            path,
            PathBuf::from("/project/.tsift/cycle-packet-cache/evidence/abc123.json")
        );
    }

    #[test]
    fn disk_roundtrip_preserves_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let entry = CyclePacketCacheEntry {
            version: CYCLE_PACKET_CACHE_VERSION.to_string(),
            kind: CyclePacketKind::Evidence,
            key: "test-key".to_string(),
            packet_id: "gevd:abc123".to_string(),
            source_watermark: "sw1".to_string(),
            document_watermark: "dw1".to_string(),
            staged_diff_watermark: "sdw1".to_string(),
            skipped_phases: vec!["graph_db_evidence".to_string()],
            compute_micros: 1234,
        };
        cycle_packet_write_cache(root, CyclePacketKind::Evidence, "test-key", &entry);
        let loaded: CyclePacketCacheEntry =
            cycle_packet_read_cache(root, CyclePacketKind::Evidence, "test-key").unwrap();
        assert_eq!(loaded.version, entry.version);
        assert_eq!(loaded.kind, entry.kind);
        assert_eq!(loaded.key, entry.key);
        assert_eq!(loaded.packet_id, entry.packet_id);
        assert_eq!(loaded.source_watermark, entry.source_watermark);
        assert_eq!(loaded.document_watermark, entry.document_watermark);
        assert_eq!(loaded.staged_diff_watermark, entry.staged_diff_watermark);
        assert_eq!(loaded.skipped_phases, entry.skipped_phases);
        assert_eq!(loaded.compute_micros, entry.compute_micros);
    }

    #[test]
    fn disk_read_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let result: Option<CyclePacketCacheEntry> =
            cycle_packet_read_cache(root, CyclePacketKind::Evidence, "nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn kind_dir_names_are_lowercase_with_hyphens() {
        assert_eq!(CyclePacketKind::Evidence.dir_name(), "evidence");
        assert_eq!(CyclePacketKind::ContextPack.dir_name(), "context-pack");
        assert_eq!(CyclePacketKind::Impact.dir_name(), "impact");
        assert_eq!(CyclePacketKind::ConflictMatrix.dir_name(), "conflict-matrix");
    }

    #[test]
    fn build_cache_hit_report_captures_fields() {
        let report = build_cache_hit_report(
            CyclePacketKind::Evidence,
            "key1",
            "gevd:abc",
            "disk_hit",
            &["phase_a", "phase_b"],
            500,
        );
        assert_eq!(report.kind, CyclePacketKind::Evidence);
        assert_eq!(report.key, "key1");
        assert_eq!(report.packet_id, "gevd:abc");
        assert_eq!(report.hit_status, "disk_hit");
        assert_eq!(report.skipped_phases, vec!["phase_a", "phase_b"]);
        assert_eq!(report.lookup_micros, 500);
    }

    #[test]
    fn cache_stats_returns_zero_for_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (count, bytes) = cycle_packet_cache_stats(dir.path());
        assert_eq!(count, 0);
        assert_eq!(bytes, 0);
    }

    #[test]
    fn cache_stats_counts_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        cycle_packet_write_cache(
            root,
            CyclePacketKind::Evidence,
            "key1",
            &serde_json::json!({"test": 1}),
        );
        cycle_packet_write_cache(
            root,
            CyclePacketKind::Evidence,
            "key2",
            &serde_json::json!({"test": 2}),
        );
        cycle_packet_write_cache(
            root,
            CyclePacketKind::ContextPack,
            "key3",
            &serde_json::json!({"test": 3}),
        );
        let (count, bytes) = cycle_packet_cache_stats(root);
        assert_eq!(count, 3);
        assert!(bytes > 0);
    }

    #[test]
    fn evict_removes_expired_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let old_entry = serde_json::json!({"old": true});
        cycle_packet_write_cache(root, CyclePacketKind::Evidence, "old-key", &old_entry);
        let old_path = cycle_packet_cache_path(root, CyclePacketKind::Evidence, "old-key");
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        let file_time = filetime::FileTime::from_system_time(old_time);
        filetime::set_file_mtime(&old_path, file_time).unwrap();

        let new_entry = serde_json::json!({"new": true});
        cycle_packet_write_cache(root, CyclePacketKind::Evidence, "new-key", &new_entry);

        let report = cycle_packet_cache_evict(root, 3600, 1024 * 1024 * 1024);
        assert_eq!(report.evicted_entries, 1);
        assert_eq!(report.remaining_entries, 1);
        assert!(
            cycle_packet_read_cache::<serde_json::Value>(root, CyclePacketKind::Evidence, "old-key")
                .is_none(),
            "old entry should be evicted"
        );
        assert!(
            cycle_packet_read_cache::<serde_json::Value>(root, CyclePacketKind::Evidence, "new-key")
                .is_some(),
            "new entry should survive"
        );
    }

    #[test]
    fn evict_enforces_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..5 {
            let data = serde_json::json!({"payload": "x".repeat(200), "idx": i});
            cycle_packet_write_cache(
                root,
                CyclePacketKind::Evidence,
                &format!("key-{i}"),
                &data,
            );
        }
        let (count, bytes) = cycle_packet_cache_stats(root);
        assert_eq!(count, 5);
        assert!(bytes > 500);
        let max_bytes = 500u64;
        let report = cycle_packet_cache_evict(root, 0, max_bytes);
        assert!(
            report.evicted_entries > 0,
            "expected evictions for max_bytes={max_bytes}, got {report:?}"
        );
        let (_, remaining_bytes) = cycle_packet_cache_stats(root);
        assert!(
            remaining_bytes <= max_bytes + 300,
            "remaining bytes should be near max_bytes, got {remaining_bytes}"
        );
    }

    #[test]
    fn evict_noop_on_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let report = cycle_packet_cache_evict(dir.path(), 3600, 1024);
        assert_eq!(report.scanned_entries, 0);
        assert_eq!(report.evicted_entries, 0);
    }
}
