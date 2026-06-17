# tsift-local-model

Local model profile, GPU probe, and lifecycle contracts for tsift Knowledge
Graph extraction.

This crate owns provider-neutral local model substrate types used by future
`tsift-kg`, `tsift-memgraphrag`, and `tsift-agent-doc` integration. It provides
RTX 5090-aware model ranking, best-effort `nvidia-smi` probing, and status
reports without binding callers to Ollama, llama.cpp, or vLLM.
