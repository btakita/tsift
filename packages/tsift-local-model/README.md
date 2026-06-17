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
