# tsift Spec - Local Model Knowledge Graph Support

Part of the [tsift spec](../SPEC.md). This file owns the contract for local
LLM-backed Knowledge Graph extraction shared by tsift, agent-doc, and
MemGraphRAG surfaces.

## Scope

The local KG path must add semantic entity/relation extraction without forking
the graph substrate or making agent-doc call model servers directly. All graph
writes continue to flow through tsift `GraphProjection` rows and `GraphStore`
backends.

Non-goals for the first implementation:

- training or fine-tuning local models,
- cloud-only providers,
- parallel 30B/32B model serving on a single RTX 5090,
- replacing deterministic tree-sitter graph extraction.

## Crate Boundaries

- `tsift-local-model` owns GPU probing, model profile ranking, provider clients,
  request/response envelopes, runtime leases, and unload/sleep hooks.
- `tsift-kg` owns chunking, extraction prompts, schema validation, run manifests,
  source watermarks, and conversion from extracted facts to `GraphProjection`.
- `tsift-memgraphrag` consumes `tsift-kg` providers for semantic extraction and
  embedding while retaining `tsift-local-hash-v1` as the deterministic fallback.
- `tsift-agent-doc` consumes graph evidence from `.tsift/graph.db`; it must not
  maintain a separate KG extraction or local-model lifecycle path.
- `tsift-cli` exposes the operational surface: `tsift kg extract`,
  `tsift kg status`, `tsift kg unload`, and smoke-test commands.

## Runtime Provider Contract

The provider-neutral local model layer must expose these concepts:

- `GpuProbe`: timestamp, GPU name, total/free/used VRAM MiB, driver/CUDA
  versions when available, and current GPU process rows.
- `ModelProfile`: stable profile id, provider kind, model reference,
  quantization, supported roles (`extract`, `embed`, `rerank`), context tokens,
  estimated weights/KV/cache MiB, concurrency class, and unload strategy.
- `LocalModelLease`: exclusive or shared lease id, selected profile,
  pre-load probe, load report, provider endpoint/process id, idle TTL, and an
  idempotent unload operation.
- `LocalModelProvider`: probes availability, ranks profiles for a requested
  role, acquires a lease, executes structured requests, and releases VRAM.

Provider implementations may target Ollama, llama.cpp router mode, vLLM, or a
test hash provider, but callers must depend on the trait contract rather than a
specific server.

## RTX 5090 Model Ranking

For an RTX 5090 with about 32 GiB VRAM, the default ranking is:

1. `qwen3-32b-q4`: default KG extractor/reasoner profile for quality.
2. `qwen3-30b-a3b-instruct-2507-q4`: throughput and long-context extractor
   candidate.
3. `qwen3-embedding-0.6b`, then `qwen3-embedding-4b`, then
   `qwen3-embedding-8b`: embedding/rerank companion profiles.
4. `qwen3.5-35b-a3b-q4`: benchmark-only until a reduced-context single-GPU
   profile is proven.

No profile may be selected if `estimated_weights_mib + estimated_kv_mib +
runtime_margin_mib` exceeds probed free VRAM. The default runtime margin for a
desktop RTX 5090 profile is 4096 MiB to leave room for the display server,
driver allocations, and transient buffers.

## Structured Extraction Schema

Every extraction request must include:

- `schema_version`,
- `run_id`,
- `source_watermark`,
- `prompt_hash`,
- `model_profile_id`,
- chunk handles with source ranges and content hashes,
- requested entity and relation kind allowlists.

Every accepted entity must include:

- `stable_id_seed`: deterministic seed derived from source handle, normalized
  label, kind, and source span,
- `kind`,
- `label`,
- `source_handle`,
- optional `source_span`,
- `confidence` in `0.0..=1.0`,
- string properties only.

Every accepted relation must include:

- `from_stable_id_seed`,
- `to_stable_id_seed`,
- `kind`,
- `evidence_source_handle`,
- optional `evidence_span`,
- `confidence` in `0.0..=1.0`,
- string properties only.

Invalid JSON, unknown kinds, missing endpoints, or out-of-range confidence
values are rejected into the run report and must not be partially written.

## Run Manifest

Each run writes a manifest with:

- `run_id`,
- `started_at_unix` and `completed_at_unix`,
- `provider`,
- `model_id`,
- `model_quant`,
- `model_profile_id`,
- `schema_version`,
- `source_watermark`,
- `prompt_hash`,
- `content_hash`,
- `input_chunk_count`,
- `accepted_entity_count`,
- `accepted_relation_count`,
- `rejected_record_count`,
- `pre_load_gpu_probe`,
- `post_unload_gpu_probe`.

The manifest is the comparison boundary for multiple runs. Re-running over the
same source should preserve source-derived graph ids while producing a distinct
run manifest and metrics.

## Graph Provenance

KG nodes and edges must be emitted as normal `GraphProjection` rows:

- extracted fact ids are stable by source fact, not by run id;
- each fact carries `GraphProvenance { source_system: "tsift-kg", source_ref }`;
- `source_ref` points to the source handle and run manifest id;
- `content_hash` covers schema version, model profile id, prompt hash, source
  handle, and normalized extracted fact;
- run-specific metadata is represented by `kg_run` nodes and
  `extracted_in_run` edges rather than baking the run id into every fact id.

Backends must be able to compare repeated runs, mark superseded facts, and keep
the latest accepted projection current without duplicating unchanged facts.

## VRAM Cleanup Acceptance

The local model layer must prove cleanup for large model profiles:

- capture `GpuProbe` before load and after unload,
- unload through provider-native APIs when available,
- terminate the worker process when unload is not proven,
- serialize large extractor leases on a single RTX 5090,
- fail the run if post-unload used VRAM remains materially above the pre-load
  baseline without a known non-tsift process accounting for the difference.

## Multi-Session Reference-Counted Lease Lifetime

A KG extractor model loaded into VRAM is a shared resource that may be
referenced by several concurrent sessions. The cooperative GPU lease registry
(`gpu-lease.json`, resolved via `resolve_lease_file`) is the reference count: a
profile stays "loaded" while at least one live holder references it, and is a
candidate for unload once the last reference is gone. The deterministic `tsift`
binary is the single arbiter — sessions never mutate the registry directly.

- **Robust read-modify-write.** Every acquire / release / renew / reap holds an
  OS advisory file lock on the registry's sidecar (`<registry>.lock`, via
  `registry_lock_path`) for the whole read → apply → write cycle. The atomic
  temp-file + rename write keeps each write atomic; the advisory lock prevents
  concurrent processes from interleaving and losing an update (TOCTOU). The
  kernel releases the lock if a holder dies mid-cycle, so a crash cannot wedge
  the registry.
- **Crash reclamation (primary).** Holders record `holder_pid`; a holder whose
  pid is no longer alive (`kill -0`) is reclaimed. This is the primary liveness
  signal — a crashed session that never released its lease is unclaimed on the
  next acquire/reap with no false positives.
- **Heartbeat / timeout (backstop).** `acquired_at_unix_seconds` doubles as the
  last-heartbeat timestamp. A live session slides it forward with `renew`
  (`tsift lease renew`); a holder whose `idle_ttl_seconds > 0` and whose age
  since its last heartbeat exceeds the TTL is reclaimed as stale even if a pid
  check is unavailable or ambiguous.
- **Reference-counted unload.** `release` reports `remaining_holders`; when it
  reaches zero the model is no longer referenced. `reap` reports
  `emptied_profiles` — profiles whose last live holder was just reclaimed.
  `tsift lease release --unload-on-last-release` and `tsift lease reap
  --unload-empty` POST the provider-native `keep_alive:0` unload only for
  profiles that dropped to zero references, so VRAM is freed exactly when no
  session still references the model.
- **Extraction consumes the lease.** `tsift kg extract` acquires an exclusive
  lease for the resolved profile before loading the model (so concurrent
  extracts serialize on one GPU; a conflict fails with the holding pid unless
  `--no-lease` is passed), and on success releases it, unloading the model when
  it dropped the last reference (unless `--keep-loaded`). An extract that bails
  leaves a pid-dead holder reclaimed by the next acquire/`reap` — the crash
  path is the reference count's own reclamation, not a leak. An explicit
  `--model` (bypassing profile resolution) runs without lease coordination.

## On-Demand Extraction Refresh

`.tsift/graph.db` is a point-in-time extraction snapshot; without a refresh
signal it silently goes stale as sources change. The refresh trigger is
**on-demand staleness detection**:

- Every `kg extract` records a `source_content_hash` (blake3 of the document
  text) on the `kg_source` node.
- `tsift kg refresh` reads each `kg_source`'s `source_ref` + recorded hash and
  compares against the current file content, classifying each as `unchanged`,
  `stale` (content differs), `missing` (no longer a readable file), or
  `no_recorded_hash` (extracted before hashing — refresh recommended). It prints
  the exact `kg extract` command to re-extract each source that needs it. The
  check is read-only and model-free, so it is cheap to run on demand or from a
  hook; re-extraction reuses the lease-aware `kg extract` path (#kgleasewire)
  and stable fact ids reconcile rather than duplicate (Run Manifest contract).

## Evidence Surfacing in Session Digest

The `tsift-agent-doc` graph-evidence read seam must be wired into an active
planning workflow rather than reachable only from the `tsift kg evidence` CLI.
The session digest (`tsift session-digest`, and the digest-backed
`session-review` / `context-pack` planning surfaces) is that consumer:

- Every digest computed in a workspace that has a `.tsift/graph.db` surfaces a
  bounded `graph_evidence` section: the graph node/edge totals plus the most
  connected KG entities, scoped to the session's top touched symbol when one
  exists (its `incident_edge_count` is the connectivity rank).
- A missing `.tsift/graph.db` yields no `graph_evidence` section — workspaces
  that have not run KG extraction are not cluttered.
- The lookup is guarded for cost. It checks the cheap `graph_counts()` query
  first and, when the store exceeds the bounded-scan cap
  (`DEFAULT_EVIDENCE_MAX_SCAN_NODES`), reports `scanned: false` with node/edge
  totals only — it must not load hundreds of thousands of nodes into memory on
  a per-cycle digest. Operators get full detail from `tsift kg evidence`.
- KG read failures degrade to a digest `warnings` entry; a session digest must
  never fail because the graph store is unreadable.

This keeps agent-doc a read-only evidence consumer (it does not own extraction
or a local-model lifecycle) while ensuring the seam runs on real planning
cycles instead of staying dormant.
