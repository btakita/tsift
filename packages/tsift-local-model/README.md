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

## Per-call profile preference (#gctrl2)

`ProfilePreference` lets a single call pin or downgrade the local model
without mutating global state. It is the per-call dial an agent-doc cycle uses
to request a small/hash profile during low-stakes phases of a long run.

- `ProfilePreference::Auto` — rank by free VRAM (existing behavior).
- `ProfilePreference::Pinned(id)` — pin to a specific profile id. The resolver
  still checks VRAM fit; if the pinned profile does not fit or does not
  support the requested role, the resolver falls back to the hash profile and
  reports `PinnedUnselectable`.
- `ProfilePreference::ForceHash` — force the deterministic CPU/hash fallback
  even when a GPU profile would fit. Shortcut on the CLI: `--profile hash`.

`resolve_profile_preference(preference, role, probe) -> ProfileResolution`
returns the chosen profile, whether it is selectable, a `ProfileResolutionSource`
(`auto_ranked` / `pinned` / `pinned_unselectable` / `forced_hash`), and a
human-readable reason. The hash profile is always a guaranteed-selectable
fallback so a call never has to abort because of a bad pin.

CLI surface:
- `tsift local-model resolve [--profile <id|hash>] [--role extract|embed|rerank] [--no-probe] [--json]`
  — the decision seam. Callers run this once to learn which profile a
  subsequent command would use.
- `tsift semantic --profile <id|hash>` and `tsift summarize --extract --profile <id|hash>`
  — informational plumbing. The resolved preference is recorded as a warning
  note on the response envelope (`profile preference <kind> -> <id> (<reason>)`)
  so a future provider seam can consume it. Until that seam lands, calls
  still use the existing cached/hash code paths.

## Profile swap lifecycle (#gctrl3)

`tsift local-model swap --from <id> --to <id>` is the one-command mid-run
downgrade path. It combines a source unload cleanup proof with a target
profile resolution against the post-unload probe, so a caller can decide in
one step whether it is safe to load the next profile (typically
`qwen3-32b-q4` → `qwen3-embedding-0.6b` or the hash fallback) without
orchestrating `unload` + `status` separately.

`SwapStatus` captures the combined outcome:

- `Swapped` — source unload cleanup proven AND target profile fits the post-unload probe.
- `SwappedToHash` — target is the CPU/hash profile; permitted once unload is proven.
- `UnloadProvenTargetUnselectable` — unload ok but target does not fit; caller should pick a smaller profile or hash.
- `UnloadNotProven` — source unload cleanup NOT proven; caller MUST NOT load the target. The report's `notes` include `DO NOT load target — source unload did not prove VRAM cleanup`.
- `NoOpSameProfile` — source and target are the same id.

Lease coordination stays the caller's job — they hold the holder-pid context
and can chain `tsift local-model lease release --profile <from>` → `swap` →
`tsift local-model lease acquire --profile <to>` in a script.

CLI surface: `tsift local-model swap --from <id> --to <id> [--provider-endpoint URL] [--provider-pid PID] [--idle-ttl-seconds S] [--no-probe] [--pre-used-mib M] [--post-used-mib M] [--tolerance-mib M] [--strict] [--json]`. `--strict` exits non-zero on `UnloadNotProven` or `UnloadProvenTargetUnselectable` so caller scripts fail closed.
