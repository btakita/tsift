use tsift_cache::cycle_packet_cache;
use cycle_packet_cache::{
    CyclePacketKind, cycle_packet_cache_dir, cycle_packet_evidence_key,
    cycle_packet_read_cache, cycle_packet_watermark_key, cycle_packet_write_cache,
    CYCLE_PACKET_CACHE_VERSION,
};

#[test]
fn evidence_cache_key_is_stable_across_calls() {
    let key_a = cycle_packet_evidence_key("gevd:target-abc:node123:hash456");
    let key_b = cycle_packet_evidence_key("gevd:target-abc:node123:hash456");
    assert_eq!(key_a, key_b, "evidence cache key must be deterministic for the same packet_id");
}

#[test]
fn evidence_cache_key_differs_for_different_targets() {
    let key_a = cycle_packet_evidence_key("gevd:target-abc:node123:hash456");
    let key_b = cycle_packet_evidence_key("gevd:target-def:node456:hash789");
    assert_ne!(key_a, key_b, "different packet_ids must produce different cache keys");
}

#[test]
fn watermark_key_includes_all_watermark_components() {
    let key_a = cycle_packet_watermark_key("sw1", "dw1", "sdw1", &["extra:1"]);
    let key_b = cycle_packet_watermark_key("sw2", "dw1", "sdw1", &["extra:1"]);
    let key_c = cycle_packet_watermark_key("sw1", "dw2", "sdw1", &["extra:1"]);
    let key_d = cycle_packet_watermark_key("sw1", "dw1", "sdw2", &["extra:1"]);
    let key_e = cycle_packet_watermark_key("sw1", "dw1", "sdw1", &["extra:2"]);
    assert_ne!(key_a, key_b);
    assert_ne!(key_a, key_c);
    assert_ne!(key_a, key_d);
    assert_ne!(key_a, key_e);
}

#[test]
fn evidence_packet_disk_cache_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let entry = serde_json::json!({
        "version": CYCLE_PACKET_CACHE_VERSION,
        "kind": "evidence",
        "key": "test-evidence-key",
        "packet_id": "gevd:target:node:hash",
        "report": {
            "packet_id": "gevd:target:node:hash",
            "target_node": { "id": "n1", "kind": "backlog", "label": "#test" }
        }
    });

    let cache_key = cycle_packet_evidence_key("gevd:target:node:hash");
    cycle_packet_write_cache(root, CyclePacketKind::Evidence, &cache_key, &entry);

    let loaded: serde_json::Value =
        cycle_packet_read_cache(root, CyclePacketKind::Evidence, &cache_key).unwrap();
    assert_eq!(loaded["packet_id"], entry["packet_id"]);
    assert_eq!(loaded["version"], CYCLE_PACKET_CACHE_VERSION);
}

#[test]
fn evidence_cache_miss_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let result: Option<serde_json::Value> =
        cycle_packet_read_cache(root, CyclePacketKind::Evidence, "nonexistent");
    assert!(result.is_none());
}

#[test]
fn dispatch_trace_cache_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let report = serde_json::json!({
        "contract_version": "dispatch-trace-v1",
        "root": "/project",
        "targets": ["#gpackreuse"],
        "evidence_packet_ids": ["gevd:target:node:hash"],
        "worker_prompt_packets": [],
        "nodes": [],
        "edges": []
    });

    let cache_key = cycle_packet_watermark_key("sw1", "dw1", "sdw1", &[
        "targets:#gpackreuse",
        "depth:3",
        "limit:8",
    ]);
    cycle_packet_write_cache(root, CyclePacketKind::ConflictMatrix, &cache_key, &report);

    let loaded: serde_json::Value =
        cycle_packet_read_cache(root, CyclePacketKind::ConflictMatrix, &cache_key).unwrap();
    assert_eq!(loaded["contract_version"], "dispatch-trace-v1");
    assert_eq!(loaded["evidence_packet_ids"][0], "gevd:target:node:hash");
}

#[test]
fn evidence_packet_id_survives_cache_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let packet_id = "gevd:abc123:def456:hash789";
    let evidence = serde_json::json!({
        "packet_id": packet_id,
        "target_node": { "id": "n1", "kind": "backlog", "label": "#test" },
        "source_handles": [],
        "worker_context": []
    });

    let cache_key = cycle_packet_evidence_key(packet_id);
    cycle_packet_write_cache(root, CyclePacketKind::Evidence, &cache_key, &evidence);

    let loaded: serde_json::Value =
        cycle_packet_read_cache(root, CyclePacketKind::Evidence, &cache_key).unwrap();
    assert_eq!(loaded["packet_id"], packet_id);
}

#[test]
fn cache_directory_structure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    cycle_packet_write_cache(
        root,
        CyclePacketKind::Evidence,
        "key1",
        &serde_json::json!({"test": true}),
    );
    cycle_packet_write_cache(
        root,
        CyclePacketKind::ContextPack,
        "key2",
        &serde_json::json!({"test": true}),
    );
    cycle_packet_write_cache(
        root,
        CyclePacketKind::ConflictMatrix,
        "key3",
        &serde_json::json!({"test": true}),
    );

    let cache_dir = cycle_packet_cache_dir(root);
    assert!(cache_dir.join("evidence/key1.json").exists());
    assert!(cache_dir.join("context-pack/key2.json").exists());
    assert!(cache_dir.join("conflict-matrix/key3.json").exists());
}

#[test]
fn cache_overwrite_replaces_stale_entry() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let cache_key = "overwrite-test";
    cycle_packet_write_cache(
        root,
        CyclePacketKind::Evidence,
        cache_key,
        &serde_json::json!({"version": 1}),
    );
    cycle_packet_write_cache(
        root,
        CyclePacketKind::Evidence,
        cache_key,
        &serde_json::json!({"version": 2}),
    );

    let loaded: serde_json::Value =
        cycle_packet_read_cache(root, CyclePacketKind::Evidence, cache_key).unwrap();
    assert_eq!(loaded["version"], 2);
}
