# tsift-local-model

Local model profile, GPU probe, and lifecycle contracts for tsift Knowledge
Graph extraction.

This crate owns provider-neutral local model substrate types used by future
`tsift-kg`, `tsift-memgraphrag`, and `tsift-agent-doc` integration. It provides
RTX 5090-aware model ranking, best-effort `nvidia-smi` probing, and status
reports without binding callers to Ollama, llama.cpp, or vLLM.

Lifecycle support is provider-neutral:

- `LocalModelLease` records the selected profile, lease mode, pre-load GPU
  probe, provider endpoint or worker pid, idle TTL, and unload strategy.
- `build_unload_actions` describes provider-native cleanup hooks such as
  llama.cpp router unload, Ollama `keep_alive: 0` or `ollama stop`, vLLM sleep,
  and process-exit fallback.
- `evaluate_vram_cleanup` compares pre-load and post-unload GPU probes and
  reports cleanup failure when VRAM stays above the baseline tolerance without
  a known non-tsift GPU process accounting for the increase.

## Cooperative GPU lease registry (#gctrl1)

Single-machine cooperative (no daemon) registry that tracks who currently
holds a GPU-bound local model profile. Producers check the file before
probing the GPU, prune stale leases, and either acquire the slot or report a
conflict with the live holder.

- File location: `$TSIFT_LEASE_FILE`, then `$XDG_STATE_HOME/tsift/gpu-lease.json`,
  then `~/.tsift/gpu-lease.json`, then `./.tsift/gpu-lease.json` (override on
  every CLI command via `--lease-file`).
- Schema: `{ version, leases: { profile_id: [GpuLeaseRecord, ...] } }`.
  `GpuLeaseRecord` records `holder_pid`, `holder_command`,
  `acquired_at_unix_seconds`, `lease_mode`, `vram_baseline_mib`,
  `idle_ttl_seconds`, and free-form `notes`.
- Concurrency rules match `LeaseMode`: `Exclusive` (one holder per profile),
  `Shared` (multiple holders per profile), `CpuOrHash` (bypass — no registry
  entry required).
- A holder is stale when its pid is no longer alive (`kill -0`) or when
  `idle_ttl_seconds > 0` and the lease age exceeds the TTL. Stale holders are
  pruned on every acquire/release/show and are reclaimable by a new acquirer.
- Writes are atomic (temp file + `fsync` + rename); concurrent acquires rely
  on cooperative file rotation. There is intentionally no advisory lock — the
  registry is best-effort and a stale entry just gets reclaimed on the next
  acquire.

Pure logic lives in `apply_acquire` / `apply_release` / `prune_stale_leases`,
each taking an `is_alive: impl Fn(u32) -> bool` closure so tests can inject a
deterministic liveness check. File-backed wrappers `acquire_lease`,
`release_lease`, and `show_registry` use the real `is_pid_alive` (which shells
out to `kill -0`).

CLI surface: `tsift local-model lease acquire|release|show`. `acquire --strict`
exits non-zero on conflict so caller scripts can fail closed.
