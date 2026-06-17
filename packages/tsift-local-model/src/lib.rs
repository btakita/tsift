use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_DESKTOP_RUNTIME_MARGIN_MIB: u64 = 4096;
pub const DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB: u64 = 768;
pub const DEFAULT_IDLE_TTL_SECONDS: u64 = 0;

/// Default llama.cpp router unload endpoint. Also the value llama-server
/// listens on by default. Override via `--provider-endpoint` or the
/// `TSIFT_LLAMA_CPP_ENDPOINT` env var when this port is taken (e.g. by a
/// local WordPress instance at 8080).
pub const DEFAULT_LLAMA_CPP_ENDPOINT: &str = "http://127.0.0.1:8080/models/unload";
/// Default Ollama generate endpoint. Override via `--provider-endpoint` or
/// the `TSIFT_OLLAMA_ENDPOINT` env var.
pub const DEFAULT_OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434/api/generate";
/// Default vLLM sleep endpoint. Override via `--provider-endpoint` or the
/// `TSIFT_VLLM_ENDPOINT` env var.
pub const DEFAULT_VLLM_ENDPOINT: &str = "http://127.0.0.1:8000/sleep";

/// Env var override for the llama.cpp router unload endpoint.
pub const LLAMA_CPP_ENDPOINT_ENV_VAR: &str = "TSIFT_LLAMA_CPP_ENDPOINT";
/// Env var override for the Ollama generate endpoint.
pub const OLLAMA_ENDPOINT_ENV_VAR: &str = "TSIFT_OLLAMA_ENDPOINT";
/// Env var override for the vLLM sleep endpoint.
pub const VLLM_ENDPOINT_ENV_VAR: &str = "TSIFT_VLLM_ENDPOINT";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ModelRole {
    Extract,
    Embed,
    Rerank,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ProviderKind {
    LlamaCpp,
    Ollama,
    Vllm,
    HashFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum UnloadStrategy {
    ProcessExit,
    OllamaKeepAliveZero,
    LlamaCppRouterUnload,
    VllmSleep,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ConcurrencyClass {
    ExclusiveLargeGpu,
    SharedSmallGpu,
    CpuOrHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseMode {
    Exclusive,
    Shared,
    CpuOrHash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum UnloadActionKind {
    ProviderApi,
    ProcessExit,
    Sleep,
    Noop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelProfile {
    pub id: &'static str,
    pub label: &'static str,
    pub provider: ProviderKind,
    pub model_ref: &'static str,
    pub quantization: &'static str,
    pub roles: Vec<ModelRole>,
    pub context_tokens: u32,
    pub estimated_weights_mib: u64,
    pub estimated_kv_mib: u64,
    pub runtime_margin_mib: u64,
    pub concurrency: ConcurrencyClass,
    pub unload_strategy: UnloadStrategy,
    pub notes: &'static str,
}

impl ModelProfile {
    pub fn estimated_total_mib(&self) -> u64 {
        self.estimated_weights_mib + self.estimated_kv_mib + self.runtime_margin_mib
    }

    pub fn supports_role(&self, role: &ModelRole) -> bool {
        self.roles.iter().any(|candidate| candidate == role)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuProcess {
    pub pid: Option<u32>,
    pub process_name: String,
    pub used_memory_mib: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuProbe {
    pub timestamp_unix_seconds: Option<u64>,
    pub available: bool,
    pub gpu_name: Option<String>,
    pub driver_version: Option<String>,
    pub total_vram_mib: Option<u64>,
    pub used_vram_mib: Option<u64>,
    pub free_vram_mib: Option<u64>,
    pub processes: Vec<GpuProcess>,
    pub error: Option<String>,
}

impl GpuProbe {
    pub fn unavailable(error: impl Into<String>) -> Self {
        Self {
            timestamp_unix_seconds: Some(current_unix_seconds()),
            available: false,
            gpu_name: None,
            driver_version: None,
            total_vram_mib: None,
            used_vram_mib: None,
            free_vram_mib: None,
            processes: Vec::new(),
            error: Some(error.into()),
        }
    }

    pub fn synthetic_vram(used_vram_mib: u64) -> Self {
        Self {
            timestamp_unix_seconds: Some(current_unix_seconds()),
            available: true,
            gpu_name: Some("synthetic GPU".to_string()),
            driver_version: None,
            total_vram_mib: None,
            used_vram_mib: Some(used_vram_mib),
            free_vram_mib: None,
            processes: Vec::new(),
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileSelection {
    pub profile: ModelProfile,
    pub selectable: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalModelStatusReport {
    pub gpu_probe: GpuProbe,
    pub extractor_profiles: Vec<ProfileSelection>,
    pub embedding_profiles: Vec<ProfileSelection>,
    pub recommended_extractor: Option<String>,
    pub recommended_embedding: Option<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderUnloadAction {
    pub kind: UnloadActionKind,
    pub label: String,
    pub command: Option<Vec<String>>,
    pub http_method: Option<String>,
    pub endpoint: Option<String>,
    pub body_json: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalModelLease {
    pub lease_id: String,
    pub mode: LeaseMode,
    pub profile: ModelProfile,
    pub pre_load_gpu_probe: GpuProbe,
    pub provider_endpoint: Option<String>,
    pub provider_pid: Option<u32>,
    pub idle_ttl_seconds: u64,
    pub unload_strategy: UnloadStrategy,
    pub unload_actions: Vec<ProviderUnloadAction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum VramCleanupStatus {
    Proven,
    ProvenByExternalAccounting,
    NotProven,
    ProbeUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VramCleanupEvaluation {
    pub status: VramCleanupStatus,
    pub cleanup_proven: bool,
    pub pre_used_mib: Option<u64>,
    pub post_used_mib: Option<u64>,
    pub allowed_post_used_mib: Option<u64>,
    pub used_delta_mib: Option<i64>,
    pub external_process_delta_mib: u64,
    pub blocking_processes: Vec<GpuProcess>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalModelLifecycleReport {
    pub lease: LocalModelLease,
    pub post_unload_gpu_probe: GpuProbe,
    pub cleanup: VramCleanupEvaluation,
    pub notes: Vec<String>,
}

pub fn default_model_profiles() -> Vec<ModelProfile> {
    vec![
        ModelProfile {
            id: "qwen3-32b-q4",
            label: "Qwen3-32B 4-bit",
            provider: ProviderKind::LlamaCpp,
            model_ref: "Qwen/Qwen3-32B-GGUF",
            quantization: "q4",
            roles: vec![ModelRole::Extract],
            context_tokens: 32_768,
            estimated_weights_mib: 20_500,
            estimated_kv_mib: 4_096,
            runtime_margin_mib: DEFAULT_DESKTOP_RUNTIME_MARGIN_MIB,
            concurrency: ConcurrencyClass::ExclusiveLargeGpu,
            unload_strategy: UnloadStrategy::LlamaCppRouterUnload,
            notes: "default quality extractor/reasoner for a clear RTX 5090",
        },
        ModelProfile {
            id: "qwen3-30b-a3b-instruct-2507-q4",
            label: "Qwen3-30B-A3B-Instruct-2507 4-bit",
            provider: ProviderKind::LlamaCpp,
            model_ref: "Qwen/Qwen3-30B-A3B-Instruct-2507",
            quantization: "q4",
            roles: vec![ModelRole::Extract],
            context_tokens: 262_144,
            estimated_weights_mib: 19_000,
            estimated_kv_mib: 4_096,
            runtime_margin_mib: DEFAULT_DESKTOP_RUNTIME_MARGIN_MIB,
            concurrency: ConcurrencyClass::ExclusiveLargeGpu,
            unload_strategy: UnloadStrategy::LlamaCppRouterUnload,
            notes: "throughput and long-context extractor fallback",
        },
        ModelProfile {
            id: "qwen3-embedding-0.6b",
            label: "Qwen3-Embedding-0.6B",
            provider: ProviderKind::LlamaCpp,
            model_ref: "Qwen/Qwen3-Embedding-0.6B-GGUF",
            quantization: "q8_or_f16",
            roles: vec![ModelRole::Embed, ModelRole::Rerank],
            context_tokens: 32_768,
            estimated_weights_mib: 1_200,
            estimated_kv_mib: 512,
            runtime_margin_mib: 1_024,
            concurrency: ConcurrencyClass::SharedSmallGpu,
            unload_strategy: UnloadStrategy::LlamaCppRouterUnload,
            notes: "default low-pressure embedding companion",
        },
        ModelProfile {
            id: "qwen3-embedding-4b",
            label: "Qwen3-Embedding-4B",
            provider: ProviderKind::LlamaCpp,
            model_ref: "Qwen/Qwen3-Embedding-4B",
            quantization: "q4_or_q8",
            roles: vec![ModelRole::Embed, ModelRole::Rerank],
            context_tokens: 32_768,
            estimated_weights_mib: 4_200,
            estimated_kv_mib: 1_024,
            runtime_margin_mib: 1_024,
            concurrency: ConcurrencyClass::SharedSmallGpu,
            unload_strategy: UnloadStrategy::LlamaCppRouterUnload,
            notes: "higher-quality embedding candidate",
        },
        ModelProfile {
            id: "qwen3-embedding-8b",
            label: "Qwen3-Embedding-8B",
            provider: ProviderKind::LlamaCpp,
            model_ref: "Qwen/Qwen3-Embedding-8B",
            quantization: "q4_or_q8",
            roles: vec![ModelRole::Embed, ModelRole::Rerank],
            context_tokens: 32_768,
            estimated_weights_mib: 8_200,
            estimated_kv_mib: 2_048,
            runtime_margin_mib: 2_048,
            concurrency: ConcurrencyClass::SharedSmallGpu,
            unload_strategy: UnloadStrategy::LlamaCppRouterUnload,
            notes: "benchmark when vector quality matters",
        },
        ModelProfile {
            id: "qwen3.5-35b-a3b-q4",
            label: "Qwen3.5-35B-A3B 4-bit",
            provider: ProviderKind::LlamaCpp,
            model_ref: "Qwen/Qwen3.5-35B-A3B",
            quantization: "q4",
            roles: vec![ModelRole::Extract],
            context_tokens: 128_000,
            estimated_weights_mib: 24_000,
            estimated_kv_mib: 8_192,
            runtime_margin_mib: DEFAULT_DESKTOP_RUNTIME_MARGIN_MIB,
            concurrency: ConcurrencyClass::ExclusiveLargeGpu,
            unload_strategy: UnloadStrategy::LlamaCppRouterUnload,
            notes: "benchmark-only until a reduced-context single-5090 profile is proven",
        },
        ModelProfile {
            id: "tsift-local-hash-v1",
            label: "tsift local hash fallback",
            provider: ProviderKind::HashFallback,
            model_ref: "builtin",
            quantization: "none",
            roles: vec![ModelRole::Embed],
            context_tokens: 0,
            estimated_weights_mib: 0,
            estimated_kv_mib: 0,
            runtime_margin_mib: 0,
            concurrency: ConcurrencyClass::CpuOrHash,
            unload_strategy: UnloadStrategy::None,
            notes: "deterministic fallback for tests and offline runs",
        },
    ]
}

pub fn probe_nvidia_smi() -> GpuProbe {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,driver_version,memory.total,memory.used,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output();

    let output = match output {
        Ok(output) => output,
        Err(error) => return GpuProbe::unavailable(format!("nvidia-smi unavailable: {error}")),
    };

    if !output.status.success() {
        return GpuProbe::unavailable(format!(
            "nvidia-smi failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_gpu_query(stdout.lines().next().unwrap_or_default()) {
        Ok(mut probe) => {
            probe.processes = query_nvidia_compute_processes();
            probe
        }
        Err(error) => GpuProbe::unavailable(error.to_string()),
    }
}

pub fn build_status_report(probe_gpu: bool) -> LocalModelStatusReport {
    let gpu_probe = if probe_gpu {
        probe_nvidia_smi()
    } else {
        GpuProbe::unavailable("gpu probe skipped")
    };
    build_status_report_with_probe(gpu_probe)
}

pub fn build_status_report_with_probe(gpu_probe: GpuProbe) -> LocalModelStatusReport {
    let profiles = default_model_profiles();
    let extractor_profiles = rank_profiles_for_role(&profiles, &gpu_probe, ModelRole::Extract);
    let embedding_profiles = rank_profiles_for_role(&profiles, &gpu_probe, ModelRole::Embed);
    let recommended_extractor = extractor_profiles
        .iter()
        .find(|selection| selection.selectable)
        .map(|selection| selection.profile.id.to_string());
    let recommended_embedding = embedding_profiles
        .iter()
        .find(|selection| selection.selectable)
        .map(|selection| selection.profile.id.to_string());

    let mut notes = vec![
        "large 30B/32B extractor profiles are single-lease on one RTX 5090".to_string(),
        "use provider unload hooks or process exit after each batch to clear VRAM".to_string(),
    ];
    if !gpu_probe.available {
        notes.push("GPU probe unavailable; profile fit is conservative".to_string());
    }

    LocalModelStatusReport {
        gpu_probe,
        extractor_profiles,
        embedding_profiles,
        recommended_extractor,
        recommended_embedding,
        notes,
    }
}

pub fn profile_by_id(profile_id: &str) -> Option<ModelProfile> {
    default_model_profiles()
        .into_iter()
        .find(|profile| profile.id == profile_id)
}

pub fn lease_mode_for_profile(profile: &ModelProfile) -> LeaseMode {
    match profile.concurrency {
        ConcurrencyClass::ExclusiveLargeGpu => LeaseMode::Exclusive,
        ConcurrencyClass::SharedSmallGpu => LeaseMode::Shared,
        ConcurrencyClass::CpuOrHash => LeaseMode::CpuOrHash,
    }
}

pub fn build_local_model_lease(
    profile: ModelProfile,
    pre_load_gpu_probe: GpuProbe,
    provider_endpoint: Option<String>,
    provider_pid: Option<u32>,
    idle_ttl_seconds: u64,
) -> LocalModelLease {
    let timestamp = pre_load_gpu_probe
        .timestamp_unix_seconds
        .unwrap_or_else(current_unix_seconds);
    let lease_id = format!("{}-{timestamp}", profile.id);
    let unload_actions = build_unload_actions(&profile, provider_endpoint.as_deref(), provider_pid);

    LocalModelLease {
        lease_id,
        mode: lease_mode_for_profile(&profile),
        unload_strategy: profile.unload_strategy.clone(),
        profile,
        pre_load_gpu_probe,
        provider_endpoint,
        provider_pid,
        idle_ttl_seconds,
        unload_actions,
    }
}

pub fn build_unload_actions(
    profile: &ModelProfile,
    provider_endpoint: Option<&str>,
    provider_pid: Option<u32>,
) -> Vec<ProviderUnloadAction> {
    match profile.unload_strategy {
        UnloadStrategy::LlamaCppRouterUnload => {
            let endpoint = resolve_provider_endpoint(&profile.unload_strategy, provider_endpoint);
            let mut actions = vec![ProviderUnloadAction {
                kind: UnloadActionKind::ProviderApi,
                label: "llama.cpp router unload".to_string(),
                command: None,
                http_method: Some("POST".to_string()),
                endpoint: Some(endpoint),
                body_json: Some(format!(r#"{{"model":"{}"}}"#, profile.model_ref)),
                required: true,
            }];
            if let Some(pid) = provider_pid {
                actions.push(process_exit_action(
                    pid,
                    "terminate llama.cpp worker if unload is not proven",
                ));
            }
            actions
        }
        UnloadStrategy::OllamaKeepAliveZero => vec![
            ProviderUnloadAction {
                kind: UnloadActionKind::ProviderApi,
                label: "ollama keep_alive zero".to_string(),
                command: None,
                http_method: Some("POST".to_string()),
                endpoint: Some(resolve_provider_endpoint(
                    &profile.unload_strategy,
                    provider_endpoint,
                )),
                body_json: Some(format!(
                    r#"{{"model":"{}","prompt":"","keep_alive":0}}"#,
                    profile.model_ref
                )),
                required: true,
            },
            ProviderUnloadAction {
                kind: UnloadActionKind::ProviderApi,
                label: "ollama stop fallback".to_string(),
                command: Some(vec![
                    "ollama".to_string(),
                    "stop".to_string(),
                    profile.model_ref.to_string(),
                ]),
                http_method: None,
                endpoint: None,
                body_json: None,
                required: false,
            },
        ],
        UnloadStrategy::VllmSleep => {
            let endpoint = resolve_provider_endpoint(&profile.unload_strategy, provider_endpoint);
            vec![ProviderUnloadAction {
                kind: UnloadActionKind::Sleep,
                label: "vLLM sleep mode".to_string(),
                command: None,
                http_method: Some("POST".to_string()),
                endpoint: Some(endpoint),
                body_json: None,
                required: true,
            }]
        }
        UnloadStrategy::ProcessExit => provider_pid
            .map(|pid| vec![process_exit_action(pid, "terminate isolated model worker")])
            .unwrap_or_else(|| {
                vec![ProviderUnloadAction {
                    kind: UnloadActionKind::ProcessExit,
                    label: "terminate isolated model worker".to_string(),
                    command: None,
                    http_method: None,
                    endpoint: None,
                    body_json: None,
                    required: true,
                }]
            }),
        UnloadStrategy::None => vec![ProviderUnloadAction {
            kind: UnloadActionKind::Noop,
            label: "no GPU unload required".to_string(),
            command: None,
            http_method: None,
            endpoint: None,
            body_json: None,
            required: false,
        }],
    }
}

/// Resolve a provider endpoint for a given unload strategy.
///
/// Precedence (highest first): explicit `--provider-endpoint` value →
/// strategy-specific env var (`TSIFT_LLAMA_CPP_ENDPOINT` /
/// `TSIFT_OLLAMA_ENDPOINT` / `TSIFT_VLLM_ENDPOINT`) → compile-time default.
///
/// Returns an empty string for strategies that do not use an HTTP endpoint
/// (`ProcessExit`, `None`); callers should not consult the value in those arms.
pub fn resolve_provider_endpoint(strategy: &UnloadStrategy, explicit: Option<&str>) -> String {
    if let Some(explicit) = explicit
        && !explicit.trim().is_empty()
    {
        return explicit.to_string();
    }
    let (env_var, default): (&str, &str) = match strategy {
        UnloadStrategy::LlamaCppRouterUnload => {
            (LLAMA_CPP_ENDPOINT_ENV_VAR, DEFAULT_LLAMA_CPP_ENDPOINT)
        }
        UnloadStrategy::OllamaKeepAliveZero => (OLLAMA_ENDPOINT_ENV_VAR, DEFAULT_OLLAMA_ENDPOINT),
        UnloadStrategy::VllmSleep => (VLLM_ENDPOINT_ENV_VAR, DEFAULT_VLLM_ENDPOINT),
        UnloadStrategy::ProcessExit | UnloadStrategy::None => return String::new(),
    };
    if let Ok(value) = std::env::var(env_var)
        && !value.trim().is_empty()
    {
        return value;
    }
    default.to_string()
}

pub fn build_lifecycle_report(
    profile: ModelProfile,
    pre_load_gpu_probe: GpuProbe,
    post_unload_gpu_probe: GpuProbe,
    provider_endpoint: Option<String>,
    provider_pid: Option<u32>,
    idle_ttl_seconds: u64,
    tolerance_mib: u64,
) -> LocalModelLifecycleReport {
    let lease = build_local_model_lease(
        profile,
        pre_load_gpu_probe.clone(),
        provider_endpoint,
        provider_pid,
        idle_ttl_seconds,
    );
    let cleanup = evaluate_vram_cleanup(&pre_load_gpu_probe, &post_unload_gpu_probe, tolerance_mib);
    let mut notes = vec![match lease.mode {
        LeaseMode::Exclusive => {
            "large extractor profile requires an exclusive local-model lease".to_string()
        }
        LeaseMode::Shared => "small model profile can share GPU when the margin fits".to_string(),
        LeaseMode::CpuOrHash => "profile does not require GPU VRAM".to_string(),
    }];
    if !cleanup.cleanup_proven {
        notes.push(
            "future KG runs should fail if cleanup remains unproven after required unload actions"
                .to_string(),
        );
    }

    LocalModelLifecycleReport {
        lease,
        post_unload_gpu_probe,
        cleanup,
        notes,
    }
}

pub fn evaluate_vram_cleanup(
    pre_load_gpu_probe: &GpuProbe,
    post_unload_gpu_probe: &GpuProbe,
    tolerance_mib: u64,
) -> VramCleanupEvaluation {
    let pre_used_mib = pre_load_gpu_probe.used_vram_mib;
    let post_used_mib = post_unload_gpu_probe.used_vram_mib;
    let allowed_post_used_mib = pre_used_mib.map(|used| used.saturating_add(tolerance_mib));
    let used_delta_mib = match (pre_used_mib, post_used_mib) {
        (Some(pre), Some(post)) => Some(post as i64 - pre as i64),
        _ => None,
    };

    if !pre_load_gpu_probe.available
        || !post_unload_gpu_probe.available
        || pre_used_mib.is_none()
        || post_used_mib.is_none()
    {
        return VramCleanupEvaluation {
            status: VramCleanupStatus::ProbeUnavailable,
            cleanup_proven: false,
            pre_used_mib,
            post_used_mib,
            allowed_post_used_mib,
            used_delta_mib,
            external_process_delta_mib: 0,
            blocking_processes: Vec::new(),
            reason: "pre-load or post-unload GPU probe is unavailable".to_string(),
        };
    }

    let pre_used = pre_used_mib.unwrap();
    let post_used = post_used_mib.unwrap();
    let allowed = allowed_post_used_mib.unwrap();
    if post_used <= allowed {
        return VramCleanupEvaluation {
            status: VramCleanupStatus::Proven,
            cleanup_proven: true,
            pre_used_mib,
            post_used_mib,
            allowed_post_used_mib,
            used_delta_mib,
            external_process_delta_mib: 0,
            blocking_processes: Vec::new(),
            reason: format!(
                "post-unload VRAM {post_used} MiB is within {tolerance_mib} MiB of baseline {pre_used} MiB"
            ),
        };
    }

    let blocking_processes = post_unload_gpu_probe
        .processes
        .iter()
        .filter(|process| is_tsift_model_process(process))
        .cloned()
        .collect::<Vec<_>>();
    let external_process_delta_mib =
        external_process_delta_mib(pre_load_gpu_probe, post_unload_gpu_probe);

    if blocking_processes.is_empty()
        && post_used <= allowed.saturating_add(external_process_delta_mib)
    {
        return VramCleanupEvaluation {
            status: VramCleanupStatus::ProvenByExternalAccounting,
            cleanup_proven: true,
            pre_used_mib,
            post_used_mib,
            allowed_post_used_mib,
            used_delta_mib,
            external_process_delta_mib,
            blocking_processes,
            reason: format!(
                "post-unload VRAM increase is accounted for by {external_process_delta_mib} MiB of non-tsift GPU processes"
            ),
        };
    }

    VramCleanupEvaluation {
        status: VramCleanupStatus::NotProven,
        cleanup_proven: false,
        pre_used_mib,
        post_used_mib,
        allowed_post_used_mib,
        used_delta_mib,
        external_process_delta_mib,
        blocking_processes,
        reason: format!(
            "post-unload VRAM {post_used} MiB exceeds allowed {allowed} MiB and cleanup is not externally accounted for"
        ),
    }
}

pub fn rank_profiles_for_role(
    profiles: &[ModelProfile],
    probe: &GpuProbe,
    role: ModelRole,
) -> Vec<ProfileSelection> {
    profiles
        .iter()
        .filter(|profile| profile.supports_role(&role))
        .map(|profile| selection_for_profile(profile, probe))
        .collect()
}

pub fn format_status_human(report: &LocalModelStatusReport) -> String {
    let mut out = String::new();
    out.push_str("Local model status\n");
    if report.gpu_probe.available {
        out.push_str(&format!(
            "GPU: {} | VRAM: {} MiB used / {} MiB total ({} MiB free)\n",
            report.gpu_probe.gpu_name.as_deref().unwrap_or("unknown"),
            format_optional_u64(report.gpu_probe.used_vram_mib),
            format_optional_u64(report.gpu_probe.total_vram_mib),
            format_optional_u64(report.gpu_probe.free_vram_mib)
        ));
    } else {
        out.push_str(&format!(
            "GPU: unavailable ({})\n",
            report.gpu_probe.error.as_deref().unwrap_or("unknown error")
        ));
    }
    out.push_str(&format!(
        "Recommended extractor: {}\n",
        report
            .recommended_extractor
            .as_deref()
            .unwrap_or("none selectable")
    ));
    out.push_str(&format!(
        "Recommended embedding: {}\n",
        report
            .recommended_embedding
            .as_deref()
            .unwrap_or("none selectable")
    ));
    out.push_str("\nExtractor profiles:\n");
    for selection in &report.extractor_profiles {
        out.push_str(&format!(
            "- {} [{} MiB est]: {} ({})\n",
            selection.profile.id,
            selection.profile.estimated_total_mib(),
            if selection.selectable {
                "selectable"
            } else {
                "blocked"
            },
            selection.reason
        ));
    }
    out.push_str("\nEmbedding profiles:\n");
    for selection in &report.embedding_profiles {
        out.push_str(&format!(
            "- {} [{} MiB est]: {} ({})\n",
            selection.profile.id,
            selection.profile.estimated_total_mib(),
            if selection.selectable {
                "selectable"
            } else {
                "blocked"
            },
            selection.reason
        ));
    }
    out
}

pub fn format_lifecycle_human(report: &LocalModelLifecycleReport) -> String {
    let mut out = String::new();
    out.push_str("Local model lifecycle\n");
    out.push_str(&format!(
        "Profile: {} ({})\n",
        report.lease.profile.id, report.lease.profile.label
    ));
    out.push_str(&format!(
        "Lease: {} | mode: {:?} | idle TTL: {}s\n",
        report.lease.lease_id, report.lease.mode, report.lease.idle_ttl_seconds
    ));
    out.push_str(&format!(
        "Pre-load VRAM: {} MiB used\n",
        format_optional_u64(report.lease.pre_load_gpu_probe.used_vram_mib)
    ));
    out.push_str(&format!(
        "Post-unload VRAM: {} MiB used\n",
        format_optional_u64(report.post_unload_gpu_probe.used_vram_mib)
    ));
    out.push_str(&format!(
        "Cleanup: {:?} ({})\n",
        report.cleanup.status, report.cleanup.reason
    ));
    out.push_str("\nRequired unload actions:\n");
    for action in &report.lease.unload_actions {
        out.push_str(&format!(
            "- {}: {}{}\n",
            if action.required {
                "required"
            } else {
                "fallback"
            },
            action.label,
            format_action_detail(action)
        ));
    }
    if !report.cleanup.blocking_processes.is_empty() {
        out.push_str("\nBlocking GPU processes:\n");
        for process in &report.cleanup.blocking_processes {
            out.push_str(&format!(
                "- pid={} name={} used={} MiB\n",
                process
                    .pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                process.process_name,
                format_optional_u64(process.used_memory_mib)
            ));
        }
    }
    out
}

fn format_action_detail(action: &ProviderUnloadAction) -> String {
    if let Some(command) = &action.command {
        return format!(" | command: {}", command.join(" "));
    }
    if let Some(endpoint) = &action.endpoint {
        return format!(
            " | {} {}{}",
            action.http_method.as_deref().unwrap_or("POST"),
            endpoint,
            action
                .body_json
                .as_ref()
                .map(|body| format!(" body={body}"))
                .unwrap_or_default()
        );
    }
    String::new()
}

fn selection_for_profile(profile: &ModelProfile, probe: &GpuProbe) -> ProfileSelection {
    if profile.concurrency == ConcurrencyClass::CpuOrHash {
        return ProfileSelection {
            profile: profile.clone(),
            selectable: true,
            reason: "does not require GPU VRAM".to_string(),
        };
    }

    let Some(free_vram_mib) = probe.free_vram_mib else {
        return ProfileSelection {
            profile: profile.clone(),
            selectable: false,
            reason: "free VRAM unknown".to_string(),
        };
    };

    let required = profile.estimated_total_mib();
    if required <= free_vram_mib {
        ProfileSelection {
            profile: profile.clone(),
            selectable: true,
            reason: format!("estimated {required} MiB fits in {free_vram_mib} MiB free"),
        }
    } else {
        ProfileSelection {
            profile: profile.clone(),
            selectable: false,
            reason: format!("estimated {required} MiB exceeds {free_vram_mib} MiB free"),
        }
    }
}

fn process_exit_action(pid: u32, label: &str) -> ProviderUnloadAction {
    ProviderUnloadAction {
        kind: UnloadActionKind::ProcessExit,
        label: label.to_string(),
        command: Some(vec![
            "kill".to_string(),
            "-TERM".to_string(),
            pid.to_string(),
        ]),
        http_method: None,
        endpoint: None,
        body_json: None,
        required: false,
    }
}

fn external_process_delta_mib(
    pre_load_gpu_probe: &GpuProbe,
    post_unload_gpu_probe: &GpuProbe,
) -> u64 {
    post_unload_gpu_probe
        .processes
        .iter()
        .filter(|process| !is_tsift_model_process(process))
        .map(|process| {
            let before = matching_pre_process(pre_load_gpu_probe, process)
                .and_then(|pre| pre.used_memory_mib)
                .unwrap_or(0);
            process.used_memory_mib.unwrap_or(0).saturating_sub(before)
        })
        .sum()
}

fn matching_pre_process<'a>(
    pre_load_gpu_probe: &'a GpuProbe,
    post_process: &GpuProcess,
) -> Option<&'a GpuProcess> {
    if let Some(pid) = post_process.pid
        && let Some(process) = pre_load_gpu_probe
            .processes
            .iter()
            .find(|candidate| candidate.pid == Some(pid))
    {
        return Some(process);
    }
    pre_load_gpu_probe
        .processes
        .iter()
        .find(|candidate| candidate.process_name == post_process.process_name)
}

fn is_tsift_model_process(process: &GpuProcess) -> bool {
    let name = process.process_name.to_ascii_lowercase();
    name.contains("tsift")
        || name.contains("llama")
        || name.contains("ollama")
        || name.contains("vllm")
        || name.contains("ggml")
}

pub fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn parse_gpu_query(line: &str) -> Result<GpuProbe> {
    let parts = line.split(',').map(str::trim).collect::<Vec<_>>();
    if parts.len() != 5 {
        anyhow::bail!("unexpected nvidia-smi gpu query row: {line}");
    }
    Ok(GpuProbe {
        timestamp_unix_seconds: Some(current_unix_seconds()),
        available: true,
        gpu_name: Some(parts[0].to_string()),
        driver_version: Some(parts[1].to_string()),
        total_vram_mib: Some(parse_optional_u64(parts[2]).context("parse total VRAM")?),
        used_vram_mib: Some(parse_optional_u64(parts[3]).context("parse used VRAM")?),
        free_vram_mib: Some(parse_optional_u64(parts[4]).context("parse free VRAM")?),
        processes: Vec::new(),
        error: None,
    })
}

fn query_nvidia_compute_processes() -> Vec<GpuProcess> {
    let Ok(output) = Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,process_name,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_query)
        .collect()
}

fn parse_process_query(line: &str) -> Option<GpuProcess> {
    let parts = line.split(',').map(str::trim).collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().all(|part| part.is_empty()) {
        return None;
    }
    Some(GpuProcess {
        pid: parts[0].parse::<u32>().ok(),
        process_name: parts[1].to_string(),
        used_memory_mib: parse_optional_u64(parts[2]).ok(),
    })
}

fn parse_optional_u64(input: &str) -> Result<u64> {
    let cleaned = input.trim().trim_end_matches("MiB").trim();
    cleaned
        .parse::<u64>()
        .with_context(|| format!("parse integer from {input:?}"))
}

fn format_optional_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

// ============================================================================
// Cooperative GPU lease registry (#gctrl1)
//
// A file-backed registry of who currently holds a GPU-bound local model
// profile. Cooperative (no daemon): producers check the file before probing
// the GPU, prune stale leases (dead pid or past idle TTL), and either acquire
// the slot or report a conflict with the live holder.
//
// The registry is keyed by `profile_id` and holds a list of `GpuLeaseRecord`
// holders. `Exclusive` profiles allow at most one live holder; `Shared`
// profiles allow many; `CpuOrHash` profiles bypass the registry entirely
// because they do not consume GPU VRAM.
// ============================================================================

/// Cooperative GPU lease registry file format version.
pub const LEASE_REGISTRY_VERSION: u32 = 1;
/// Default idle TTL (0 = no TTL-based staleness, only pid-dead pruning).
pub const DEFAULT_LEASE_TTL_SECONDS: u64 = 0;
/// Environment variable override for the lease registry file path.
pub const LEASE_FILE_ENV_VAR: &str = "TSIFT_LEASE_FILE";

/// One held lease on a profile, written to the cooperative registry file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuLeaseRecord {
    pub profile_id: String,
    pub holder_pid: u32,
    pub holder_command: String,
    pub acquired_at_unix_seconds: u64,
    pub lease_mode: LeaseMode,
    pub vram_baseline_mib: u64,
    pub idle_ttl_seconds: u64,
    pub notes: Vec<String>,
}

/// File-backed cooperative registry: `{ version, leases: { profile_id: [record, ...] } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuLeaseRegistry {
    pub version: u32,
    pub leases: BTreeMap<String, Vec<GpuLeaseRecord>>,
}

impl Default for GpuLeaseRegistry {
    fn default() -> Self {
        Self {
            version: LEASE_REGISTRY_VERSION,
            leases: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum GpuLeaseAcquisitionStatus {
    /// Fresh acquire on a free slot.
    Acquired,
    /// Same holder pid already held the slot; timestamp/baseline refreshed.
    Refreshed,
    /// Previous holder was stale (pid gone or TTL expired); slot reclaimed.
    ReclaimedStale,
    /// Profile is `CpuOrHash`; no registry entry needed.
    CpuOrHashBypass,
    /// Another live holder owns the slot.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuLeaseConflict {
    pub profile_id: String,
    pub holder_pid: u32,
    pub holder_command: String,
    pub acquired_at_unix_seconds: u64,
    pub lease_mode: LeaseMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuLeaseAcquisition {
    pub profile_id: String,
    pub holder_pid: u32,
    pub status: GpuLeaseAcquisitionStatus,
    pub record: Option<GpuLeaseRecord>,
    pub conflict: Option<GpuLeaseConflict>,
    /// Stale records pruned during this acquire (cleared from the registry).
    pub reclaimed: Vec<GpuLeaseRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum GpuLeaseReleaseOutcome {
    /// This holder's lease was removed.
    Released,
    /// Profile exists but this pid was not among its holders.
    NotHeld,
    /// No entry for the profile at all.
    ProfileAbsent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GpuLeaseRelease {
    pub profile_id: String,
    pub holder_pid: u32,
    pub outcome: GpuLeaseReleaseOutcome,
    /// Number of remaining live holders for the profile after release.
    pub remaining_holders: u32,
}

/// Resolve the cooperative lease registry file path.
///
/// Order: explicit `override_path` → `$TSIFT_LEASE_FILE` →
/// `$XDG_STATE_HOME/tsift/gpu-lease.json` → `~/.tsift/gpu-lease.json` →
/// `./.tsift/gpu-lease.json` if no home directory can be resolved.
pub fn resolve_lease_file(override_path: Option<&Path>) -> PathBuf {
    if let Some(path) = override_path {
        return path.to_path_buf();
    }
    if let Ok(path) = std::env::var(LEASE_FILE_ENV_VAR) {
        return PathBuf::from(path);
    }
    if let Ok(state_dir) = std::env::var("XDG_STATE_HOME")
        && !state_dir.is_empty()
    {
        return PathBuf::from(state_dir)
            .join("tsift")
            .join("gpu-lease.json");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(".tsift").join("gpu-lease.json");
    }
    PathBuf::from("./.tsift/gpu-lease.json")
}

/// Best-effort pid-liveness check via `kill -0`.
///
/// A pid equal to the current process is always considered alive. Pid 0 is
/// treated as missing/unknown and reported as not alive so callers can use 0
/// as a sentinel for "no pid recorded".
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    match Command::new("kill").arg("-0").arg(pid.to_string()).output() {
        Ok(output) => output.status.success(),
        Err(_) => false,
    }
}

/// Read the lease registry, returning an empty default when the file is missing.
pub fn read_lease_registry(path: &Path) -> Result<GpuLeaseRegistry> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            if contents.trim().is_empty() {
                return Ok(GpuLeaseRegistry::default());
            }
            serde_json::from_str(&contents).context("parse gpu lease registry")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(GpuLeaseRegistry::default())
        }
        Err(error) => Err(error).context("read gpu lease registry"),
    }
}

/// Atomically write the lease registry (temp file + rename).
pub fn write_lease_registry(path: &Path, registry: &GpuLeaseRegistry) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).context("create lease registry parent")?;
    }
    let payload = serde_json::to_string_pretty(registry).context("serialize lease registry")?;
    let temp_path = path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        current_unix_seconds()
    ));
    let mut handle = fs::File::create(&temp_path).context("create lease registry temp file")?;
    handle
        .write_all(payload.as_bytes())
        .context("write lease registry temp file")?;
    handle.sync_all().context("sync lease registry temp file")?;
    drop(handle);
    fs::rename(&temp_path, path).context("rename lease registry into place")?;
    Ok(())
}

/// Prune stale holders from the registry in place.
///
/// A holder is stale when its pid is no longer alive, or when its
/// `idle_ttl_seconds > 0` and the lease age exceeds the TTL.
///
/// Returns the records that were pruned.
pub fn prune_stale_leases(
    registry: &mut GpuLeaseRegistry,
    now: u64,
    is_alive: impl Fn(u32) -> bool,
) -> Vec<GpuLeaseRecord> {
    let mut pruned = Vec::new();
    let mut empty_keys = Vec::new();
    for (profile_id, holders) in registry.leases.iter_mut() {
        let mut kept = Vec::with_capacity(holders.len());
        for record in holders.drain(..) {
            let pid_dead = !is_alive(record.holder_pid);
            let ttl_expired = record.idle_ttl_seconds > 0
                && now.saturating_sub(record.acquired_at_unix_seconds) > record.idle_ttl_seconds;
            if pid_dead || ttl_expired {
                pruned.push(record);
            } else {
                kept.push(record);
            }
        }
        if kept.is_empty() {
            empty_keys.push(profile_id.clone());
        }
        *holders = kept;
    }
    for key in empty_keys {
        registry.leases.remove(&key);
    }
    pruned
}

/// Apply an acquire to the registry in place.
///
/// Pure logic; the file I/O wrapper is `acquire_lease`. The `is_alive` closure
/// lets tests inject a deterministic liveness check.
#[allow(clippy::too_many_arguments)]
pub fn apply_acquire(
    registry: &mut GpuLeaseRegistry,
    profile: &ModelProfile,
    holder_pid: u32,
    holder_command: &str,
    vram_baseline_mib: u64,
    idle_ttl_seconds: u64,
    now: u64,
    is_alive: impl Fn(u32) -> bool,
) -> GpuLeaseAcquisition {
    if profile.concurrency == ConcurrencyClass::CpuOrHash {
        return GpuLeaseAcquisition {
            profile_id: profile.id.to_string(),
            holder_pid,
            status: GpuLeaseAcquisitionStatus::CpuOrHashBypass,
            record: None,
            conflict: None,
            reclaimed: Vec::new(),
        };
    }

    let reclaimed = prune_stale_leases(registry, now, &is_alive);
    let mode = lease_mode_for_profile(profile);
    let entry = registry.leases.entry(profile.id.to_string()).or_default();
    let already_held = entry
        .iter()
        .position(|record| record.holder_pid == holder_pid);

    let record = GpuLeaseRecord {
        profile_id: profile.id.to_string(),
        holder_pid,
        holder_command: holder_command.to_string(),
        acquired_at_unix_seconds: now,
        lease_mode: mode.clone(),
        vram_baseline_mib,
        idle_ttl_seconds,
        notes: Vec::new(),
    };

    let status = if let Some(index) = already_held {
        entry[index] = record.clone();
        GpuLeaseAcquisitionStatus::Refreshed
    } else {
        match mode {
            LeaseMode::Exclusive => {
                if let Some(blocker) = entry.first() {
                    return GpuLeaseAcquisition {
                        profile_id: profile.id.to_string(),
                        holder_pid,
                        status: GpuLeaseAcquisitionStatus::Conflict,
                        record: None,
                        conflict: Some(GpuLeaseConflict {
                            profile_id: profile.id.to_string(),
                            holder_pid: blocker.holder_pid,
                            holder_command: blocker.holder_command.clone(),
                            acquired_at_unix_seconds: blocker.acquired_at_unix_seconds,
                            lease_mode: blocker.lease_mode.clone(),
                        }),
                        reclaimed,
                    };
                }
                entry.push(record.clone());
                if reclaimed
                    .iter()
                    .any(|pruned| pruned.profile_id == profile.id)
                {
                    GpuLeaseAcquisitionStatus::ReclaimedStale
                } else {
                    GpuLeaseAcquisitionStatus::Acquired
                }
            }
            LeaseMode::Shared => {
                entry.push(record.clone());
                if reclaimed
                    .iter()
                    .any(|pruned| pruned.profile_id == profile.id)
                {
                    GpuLeaseAcquisitionStatus::ReclaimedStale
                } else {
                    GpuLeaseAcquisitionStatus::Acquired
                }
            }
            LeaseMode::CpuOrHash => GpuLeaseAcquisitionStatus::CpuOrHashBypass,
        }
    };

    GpuLeaseAcquisition {
        profile_id: profile.id.to_string(),
        holder_pid,
        status,
        record: Some(record),
        conflict: None,
        reclaimed,
    }
}

/// Apply a release to the registry in place.
pub fn apply_release(
    registry: &mut GpuLeaseRegistry,
    profile_id: &str,
    holder_pid: u32,
    now: u64,
    is_alive: impl Fn(u32) -> bool,
) -> GpuLeaseRelease {
    let _ = prune_stale_leases(registry, now, &is_alive);
    let Some(holders) = registry.leases.get_mut(profile_id) else {
        return GpuLeaseRelease {
            profile_id: profile_id.to_string(),
            holder_pid,
            outcome: GpuLeaseReleaseOutcome::ProfileAbsent,
            remaining_holders: 0,
        };
    };
    let before = holders.len();
    holders.retain(|record| record.holder_pid != holder_pid);
    let removed = before - holders.len();
    let remaining = holders.len() as u32;
    if holders.is_empty() {
        // Borrow on `holders` ends here; safe to mutate the map again.
        registry.leases.remove(profile_id);
    }
    let outcome = if removed == 0 {
        GpuLeaseReleaseOutcome::NotHeld
    } else {
        GpuLeaseReleaseOutcome::Released
    };
    GpuLeaseRelease {
        profile_id: profile_id.to_string(),
        holder_pid,
        outcome,
        remaining_holders: remaining,
    }
}

/// High-level acquire: read file, prune stale, apply, write file.
pub fn acquire_lease(
    profile_id: &str,
    holder_pid: u32,
    holder_command: &str,
    vram_baseline_mib: u64,
    idle_ttl_seconds: u64,
    now: u64,
    path: &Path,
) -> Result<GpuLeaseAcquisition> {
    let profile = profile_by_id(profile_id)
        .with_context(|| format!("unknown local model profile {profile_id:?}"))?;
    let mut registry = read_lease_registry(path)?;
    let acquisition = apply_acquire(
        &mut registry,
        &profile,
        holder_pid,
        holder_command,
        vram_baseline_mib,
        idle_ttl_seconds,
        now,
        is_pid_alive,
    );
    // CpuOrHash bypass intentionally does not touch the registry file.
    if acquisition.status != GpuLeaseAcquisitionStatus::CpuOrHashBypass {
        write_lease_registry(path, &registry)?;
    }
    Ok(acquisition)
}

/// High-level release: read file, prune, drop this holder, write file.
pub fn release_lease(
    profile_id: &str,
    holder_pid: u32,
    now: u64,
    path: &Path,
) -> Result<GpuLeaseRelease> {
    let mut registry = read_lease_registry(path)?;
    let release = apply_release(&mut registry, profile_id, holder_pid, now, is_pid_alive);
    write_lease_registry(path, &registry)?;
    Ok(release)
}

/// Read the registry and return the pruned view. `include_stale` skips the
/// pruning pass so the caller can inspect raw state for diagnostics.
pub fn show_registry(path: &Path, now: u64, include_stale: bool) -> Result<GpuLeaseRegistry> {
    let mut registry = read_lease_registry(path)?;
    if !include_stale {
        prune_stale_leases(&mut registry, now, is_pid_alive);
    }
    Ok(registry)
}

/// Human-readable summary of the lease registry.
pub fn format_lease_show_human(registry: &GpuLeaseRegistry, now: u64) -> String {
    let mut out = String::new();
    out.push_str("GPU lease registry\n");
    out.push_str(&format!("version: {}\n", registry.version));
    if registry.leases.is_empty() {
        out.push_str("leases: none\n");
        return out;
    }
    out.push_str(&format!("profiles held: {}\n", registry.leases.len()));
    for (profile_id, holders) in &registry.leases {
        out.push_str(&format!("\n{profile_id} ({} holder(s)):\n", holders.len()));
        for record in holders {
            let age = now.saturating_sub(record.acquired_at_unix_seconds);
            out.push_str(&format!(
                "  pid={} cmd={} mode={:?} acquired={}s ago baseline={} MiB ttl={}s",
                record.holder_pid,
                record.holder_command,
                record.lease_mode,
                age,
                record.vram_baseline_mib,
                record.idle_ttl_seconds
            ));
            if record.notes.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&format!(" notes={}\n", record.notes.join("; ")));
            }
        }
    }
    out
}

// ============================================================================
// Per-call profile preference (#gctrl2)
//
// Callers (agent-doc cycles, scripts) that want to pin or downgrade the local
// model for a single call — without mutating global state — express that as a
// `ProfilePreference`. The resolver turns the preference + the live GPU probe
// into a concrete `ProfileSelection` plus a `ProfileResolutionSource` saying
// how the choice was made. Commands that touch the local model accept the
// preference via `--profile`, record it in their response envelope, and will
// hand it to the real provider seam once one is wired in.
// ============================================================================

/// Caller-supplied preference for which local model profile a single call
/// should use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ProfilePreference {
    /// No pin — rank by free VRAM (existing behavior).
    Auto,
    /// Pin to a specific profile id. The resolver still checks VRAM fit and
    /// reports `PinnedUnselectable` if the profile would not fit the probe.
    Pinned(String),
    /// Force the deterministic CPU/hash fallback even if a GPU profile would
    /// fit. Use during low-stakes phases of a long agent-doc run.
    ForceHash,
}

impl ProfilePreference {
    /// Parse the `--profile <Option<String>>` CLI value.
    ///
    /// `None` / empty → `Auto`. The literal `"hash"` or the hash profile id
    /// (`tsift-local-hash-v1`) → `ForceHash`. Anything else → `Pinned(id)`.
    pub fn from_cli(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") => ProfilePreference::Auto,
            Some("hash") | Some("tsift-local-hash-v1") => ProfilePreference::ForceHash,
            Some(other) => ProfilePreference::Pinned(other.to_string()),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            ProfilePreference::Auto => "auto".to_string(),
            ProfilePreference::Pinned(id) => format!("pinned:{id}"),
            ProfilePreference::ForceHash => "force-hash".to_string(),
        }
    }
}

/// How a resolved profile was chosen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileResolutionSource {
    /// `Auto` preference; ranked against the live probe.
    AutoRanked,
    /// `Pinned` preference and the profile is selectable on this probe.
    Pinned,
    /// `Pinned` preference but the profile is not selectable (unknown id or
    /// VRAM does not fit). Falls back to the hash profile so the call can
    /// still proceed deterministically.
    PinnedUnselectable,
    /// `ForceHash` preference; hash fallback selected regardless of probe.
    ForcedHash,
}

/// Result of resolving a `ProfilePreference` against the live GPU probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileResolution {
    pub preference: ProfilePreference,
    pub role: ModelRole,
    pub source: ProfileResolutionSource,
    pub profile: ModelProfile,
    pub selectable: bool,
    pub reason: String,
}

/// Resolve a caller preference to a concrete profile for a given role.
///
/// Pure function — pass a synthetic `GpuProbe` for tests. The hash profile is
/// the guaranteed-selectable fallback for any non-`ForceHash` preference when
/// the pinned/auto-ranked profile is not selectable.
pub fn resolve_profile_preference(
    preference: &ProfilePreference,
    role: ModelRole,
    probe: &GpuProbe,
) -> ProfileResolution {
    let profiles = default_model_profiles();
    let hash_profile = profiles
        .iter()
        .find(|profile| profile.id == "tsift-local-hash-v1")
        .cloned()
        .expect("hash fallback profile is always present");

    match preference {
        ProfilePreference::ForceHash => ProfileResolution {
            preference: preference.clone(),
            role,
            source: ProfileResolutionSource::ForcedHash,
            profile: hash_profile,
            selectable: true,
            reason: "caller forced the CPU/hash fallback".to_string(),
        },
        ProfilePreference::Auto => {
            let ranked = rank_profiles_for_role(&profiles, probe, role);
            let pick = ranked
                .iter()
                .find(|selection| selection.selectable)
                .cloned()
                .or_else(|| {
                    ranked.into_iter().next().map(|selection| ProfileSelection {
                        selectable: false,
                        ..selection
                    })
                });
            match pick {
                Some(selection) if selection.selectable => ProfileResolution {
                    preference: preference.clone(),
                    role,
                    source: ProfileResolutionSource::AutoRanked,
                    profile: selection.profile.clone(),
                    selectable: true,
                    reason: format!("auto-ranked: {}", selection.reason),
                },
                Some(selection) => ProfileResolution {
                    preference: preference.clone(),
                    role,
                    source: ProfileResolutionSource::AutoRanked,
                    profile: hash_profile,
                    selectable: true,
                    reason: format!(
                        "auto-ranked but no GPU profile selectable ({}); using hash fallback",
                        selection.reason
                    ),
                },
                None => ProfileResolution {
                    preference: preference.clone(),
                    role,
                    source: ProfileResolutionSource::AutoRanked,
                    profile: hash_profile,
                    selectable: true,
                    reason: "no profile matches the requested role; using hash fallback"
                        .to_string(),
                },
            }
        }
        ProfilePreference::Pinned(id) => match profile_by_id(id) {
            Some(profile) if profile.supports_role(&role) => {
                let selection = selection_for_profile(&profile, probe);
                if selection.selectable {
                    ProfileResolution {
                        preference: preference.clone(),
                        role,
                        source: ProfileResolutionSource::Pinned,
                        profile,
                        selectable: true,
                        reason: format!("pinned: {}", selection.reason),
                    }
                } else {
                    ProfileResolution {
                        preference: preference.clone(),
                        role,
                        source: ProfileResolutionSource::PinnedUnselectable,
                        profile: hash_profile,
                        selectable: true,
                        reason: format!(
                            "pinned {} is not selectable ({}); using hash fallback",
                            id, selection.reason
                        ),
                    }
                }
            }
            Some(_) => ProfileResolution {
                preference: preference.clone(),
                role,
                source: ProfileResolutionSource::PinnedUnselectable,
                profile: hash_profile,
                selectable: true,
                reason: format!(
                    "pinned {id} does not support role {:?}; using hash fallback",
                    role
                ),
            },
            None => ProfileResolution {
                preference: preference.clone(),
                role,
                source: ProfileResolutionSource::PinnedUnselectable,
                profile: hash_profile,
                selectable: true,
                reason: format!("pinned profile id {id:?} is unknown; using hash fallback"),
            },
        },
    }
}

// ============================================================================
// Profile swap lifecycle (#gctrl3)
//
// `tsift local-model swap --from <id> --to <id>` is the one-command mid-run
// downgrade path. It combines an unload cleanup proof for the source profile
// with a `ProfileResolution` for the target against the post-unload probe, so
// a caller can decide in one step whether it is safe to load the next profile
// (typically qwen3-32b-q4 -> qwen3-embedding-0.6b or the hash fallback).
//
// Lease coordination stays the caller's job (they hold the holder-pid context
// and can chain `lease release --from` -> `swap` -> `lease acquire --to`).
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapStatus {
    /// Source unload cleanup proven and target profile fits the post-unload probe.
    Swapped,
    /// Target was the CPU/hash profile; swap is always permitted once the
    /// source unload is proven.
    SwappedToHash,
    /// Source unload cleanup proven but the target profile does not fit the
    /// post-unload probe. Caller should fall back to a smaller profile or hash.
    UnloadProvenTargetUnselectable,
    /// Source unload cleanup NOT proven — caller MUST NOT load the target
    /// because VRAM has not been returned to baseline.
    UnloadNotProven,
    /// Source and target are the same profile id; no-op.
    NoOpSameProfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalModelSwapReport {
    pub from_profile_id: String,
    pub to_profile_id: String,
    pub unload: LocalModelLifecycleReport,
    pub target_resolution: ProfileResolution,
    pub swap_status: SwapStatus,
    pub notes: Vec<String>,
}

/// Build a combined swap report: source unload lifecycle + target resolution.
///
/// Reuses `build_lifecycle_report` for the unload proof and
/// `resolve_profile_preference` for the target so semantics stay aligned with
/// the rest of the substrate.
#[allow(clippy::too_many_arguments)]
pub fn build_swap_report(
    from_profile: ModelProfile,
    to_profile: ModelProfile,
    pre_load_probe: GpuProbe,
    post_unload_probe: GpuProbe,
    provider_endpoint: Option<String>,
    provider_pid: Option<u32>,
    idle_ttl_seconds: u64,
    tolerance_mib: u64,
) -> LocalModelSwapReport {
    let unload = build_lifecycle_report(
        from_profile.clone(),
        pre_load_probe,
        post_unload_probe.clone(),
        provider_endpoint,
        provider_pid,
        idle_ttl_seconds,
        tolerance_mib,
    );

    let target_role = to_profile
        .roles
        .first()
        .copied()
        .unwrap_or(ModelRole::Extract);
    let target_resolution = resolve_profile_preference(
        &ProfilePreference::Pinned(to_profile.id.to_string()),
        target_role,
        &post_unload_probe,
    );

    let swap_status = if from_profile.id == to_profile.id {
        SwapStatus::NoOpSameProfile
    } else if !unload.cleanup.cleanup_proven {
        SwapStatus::UnloadNotProven
    } else if to_profile.concurrency == ConcurrencyClass::CpuOrHash {
        SwapStatus::SwappedToHash
    } else if target_resolution.selectable && target_resolution.profile.id == to_profile.id {
        SwapStatus::Swapped
    } else {
        SwapStatus::UnloadProvenTargetUnselectable
    };

    let mut notes = vec![
        format!("swapping from {} to {}", from_profile.id, to_profile.id),
        format!("unload cleanup: {:?}", unload.cleanup.status),
        format!("target resolution: {:?}", target_resolution.source),
    ];
    if swap_status == SwapStatus::UnloadNotProven {
        notes.push("DO NOT load target — source unload did not prove VRAM cleanup".to_string());
    }
    if swap_status == SwapStatus::UnloadProvenTargetUnselectable {
        notes.push(format!(
            "target {} is not selectable on the post-unload probe; consider the hash fallback or a smaller profile",
            to_profile.id
        ));
    }

    LocalModelSwapReport {
        from_profile_id: from_profile.id.to_string(),
        to_profile_id: to_profile.id.to_string(),
        unload,
        target_resolution,
        swap_status,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtx_5090_probe() -> GpuProbe {
        GpuProbe {
            timestamp_unix_seconds: Some(1_781_000_000),
            available: true,
            gpu_name: Some("NVIDIA GeForce RTX 5090".to_string()),
            driver_version: Some("610.43.02".to_string()),
            total_vram_mib: Some(32_607),
            used_vram_mib: Some(179),
            free_vram_mib: Some(32_428),
            processes: Vec::new(),
            error: None,
        }
    }

    fn probe_with_used_vram(used_vram_mib: u64) -> GpuProbe {
        let mut probe = rtx_5090_probe();
        probe.used_vram_mib = Some(used_vram_mib);
        probe.free_vram_mib = probe
            .total_vram_mib
            .map(|total| total.saturating_sub(used_vram_mib));
        probe
    }

    #[test]
    fn qwen3_32b_is_default_extractor_for_clear_5090() {
        let report = build_status_report_with_probe(rtx_5090_probe());
        assert_eq!(
            report.recommended_extractor.as_deref(),
            Some("qwen3-32b-q4")
        );
        assert!(
            report
                .extractor_profiles
                .iter()
                .any(|selection| selection.profile.id == "qwen3.5-35b-a3b-q4"
                    && !selection.selectable)
        );
    }

    #[test]
    fn hash_fallback_selects_without_gpu_probe() {
        let report = build_status_report_with_probe(GpuProbe::unavailable("missing"));
        assert_eq!(
            report.recommended_embedding.as_deref(),
            Some("tsift-local-hash-v1")
        );
        assert_eq!(report.recommended_extractor, None);
    }

    #[test]
    fn parses_nvidia_smi_gpu_query_row() {
        let probe =
            parse_gpu_query("NVIDIA GeForce RTX 5090, 610.43.02, 32607, 179, 32428").unwrap();
        assert!(probe.timestamp_unix_seconds.is_some());
        assert_eq!(probe.gpu_name.as_deref(), Some("NVIDIA GeForce RTX 5090"));
        assert_eq!(probe.total_vram_mib, Some(32_607));
        assert_eq!(probe.used_vram_mib, Some(179));
        assert_eq!(probe.free_vram_mib, Some(32_428));
    }

    #[test]
    fn lifecycle_report_plans_llamacpp_unload_and_proves_cleanup() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        let report = build_lifecycle_report(
            profile,
            probe_with_used_vram(200),
            probe_with_used_vram(820),
            Some("http://127.0.0.1:8080/models/unload".to_string()),
            Some(42),
            DEFAULT_IDLE_TTL_SECONDS,
            DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB,
        );

        assert_eq!(report.lease.mode, LeaseMode::Exclusive);
        assert!(report.cleanup.cleanup_proven);
        assert_eq!(report.cleanup.status, VramCleanupStatus::Proven);
        assert!(report.lease.unload_actions.iter().any(|action| {
            action.kind == UnloadActionKind::ProviderApi
                && action.endpoint.as_deref() == Some("http://127.0.0.1:8080/models/unload")
        }));
        assert!(report.lease.unload_actions.iter().any(|action| {
            action.kind == UnloadActionKind::ProcessExit
                && action.command.as_ref().is_some_and(|command| {
                    command == &vec!["kill".to_string(), "-TERM".to_string(), "42".to_string()]
                })
        }));
    }

    #[test]
    fn vram_cleanup_fails_when_provider_process_remains_loaded() {
        let pre = probe_with_used_vram(200);
        let mut post = probe_with_used_vram(4_000);
        post.processes.push(GpuProcess {
            pid: Some(42),
            process_name: "llama-server".to_string(),
            used_memory_mib: Some(3_000),
        });

        let cleanup = evaluate_vram_cleanup(&pre, &post, DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB);

        assert!(!cleanup.cleanup_proven);
        assert_eq!(cleanup.status, VramCleanupStatus::NotProven);
        assert_eq!(cleanup.blocking_processes.len(), 1);
    }

    #[test]
    fn vram_cleanup_accepts_external_process_accounting() {
        let pre = probe_with_used_vram(200);
        let mut post = probe_with_used_vram(2_000);
        post.processes.push(GpuProcess {
            pid: Some(77),
            process_name: "python-training-job".to_string(),
            used_memory_mib: Some(1_600),
        });

        let cleanup = evaluate_vram_cleanup(&pre, &post, DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB);

        assert!(cleanup.cleanup_proven);
        assert_eq!(
            cleanup.status,
            VramCleanupStatus::ProvenByExternalAccounting
        );
        assert_eq!(cleanup.external_process_delta_mib, 1_600);
    }

    #[test]
    fn interrupted_run_cleanup_fails_with_orphaned_provider_process() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        let pre = probe_with_used_vram(200);
        let mut post = probe_with_used_vram(8_000);
        post.processes.push(GpuProcess {
            pid: Some(1234),
            process_name: "ollama runner".to_string(),
            used_memory_mib: Some(7_000),
        });

        let report = build_lifecycle_report(
            profile,
            pre,
            post,
            None,
            Some(1234),
            DEFAULT_IDLE_TTL_SECONDS,
            DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB,
        );

        assert!(!report.cleanup.cleanup_proven);
        assert_eq!(report.cleanup.status, VramCleanupStatus::NotProven);
        assert_eq!(report.cleanup.blocking_processes.len(), 1);
        assert!(report.lease.unload_actions.iter().any(|action| {
            action.kind == UnloadActionKind::ProcessExit
                && action.command.as_ref().is_some_and(|command| {
                    command == &vec!["kill".to_string(), "-TERM".to_string(), "1234".to_string()]
                })
        }));
    }

    // ---- Cooperative GPU lease registry (#gctrl1) ----

    fn all_alive(_pid: u32) -> bool {
        true
    }
    fn alive_set(alive: &[u32]) -> impl Fn(u32) -> bool + '_ {
        move |pid| alive.contains(&pid)
    }

    #[test]
    fn resolve_lease_file_prefers_explicit_override() {
        let path = resolve_lease_file(Some(Path::new("/custom/lease.json")));
        assert_eq!(path, PathBuf::from("/custom/lease.json"));
    }

    #[test]
    fn resolve_lease_file_returns_env_value_when_set() {
        // SAFETY: env mutation is unsafe in edition 2024 because of potential
        // data races in multi-threaded programs. Tests run single-threaded
        // inside this test function and the value is restored afterwards.
        unsafe {
            std::env::set_var(LEASE_FILE_ENV_VAR, "/env/lease.json");
        }
        let path = resolve_lease_file(None);
        unsafe {
            std::env::remove_var(LEASE_FILE_ENV_VAR);
        }
        assert_eq!(path, PathBuf::from("/env/lease.json"));
    }

    #[test]
    fn lease_registry_round_trips_through_json() {
        let mut registry = GpuLeaseRegistry::default();
        registry.leases.insert(
            "qwen3-32b-q4".to_string(),
            vec![GpuLeaseRecord {
                profile_id: "qwen3-32b-q4".to_string(),
                holder_pid: 4242,
                holder_command: "tsift".to_string(),
                acquired_at_unix_seconds: 100,
                lease_mode: LeaseMode::Exclusive,
                vram_baseline_mib: 200,
                idle_ttl_seconds: 0,
                notes: vec!["baseline".to_string()],
            }],
        );
        let payload = serde_json::to_string(&registry).unwrap();
        let back: GpuLeaseRegistry = serde_json::from_str(&payload).unwrap();
        assert_eq!(registry, back);
        assert_eq!(back.version, LEASE_REGISTRY_VERSION);
    }

    #[test]
    fn acquire_exclusive_profile_succeeds_when_free() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        let mut registry = GpuLeaseRegistry::default();
        let acquisition = apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            200,
            0,
            1_000,
            all_alive,
        );
        assert_eq!(acquisition.status, GpuLeaseAcquisitionStatus::Acquired);
        assert!(acquisition.conflict.is_none());
        assert_eq!(registry.leases["qwen3-32b-q4"].len(), 1);
        assert_eq!(registry.leases["qwen3-32b-q4"][0].holder_pid, 100);
    }

    #[test]
    fn acquire_exclusive_profile_conflicts_with_live_holder() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        let mut registry = GpuLeaseRegistry::default();
        apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            200,
            0,
            1_000,
            all_alive,
        );
        let second = apply_acquire(
            &mut registry,
            &profile,
            200,
            "corky",
            250,
            0,
            1_050,
            alive_set(&[100, 200]),
        );
        assert_eq!(second.status, GpuLeaseAcquisitionStatus::Conflict);
        let conflict = second.conflict.unwrap();
        assert_eq!(conflict.holder_pid, 100);
        assert_eq!(conflict.holder_command, "tsift");
        // The conflict must not overwrite the existing holder.
        assert_eq!(registry.leases["qwen3-32b-q4"].len(), 1);
        assert_eq!(registry.leases["qwen3-32b-q4"][0].holder_pid, 100);
    }

    #[test]
    fn acquire_exclusive_profile_reclaims_when_holder_pid_dead() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        let mut registry = GpuLeaseRegistry::default();
        apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            200,
            0,
            1_000,
            all_alive,
        );
        // pid 100 is gone now; only pid 200 is alive.
        let reclaimed = apply_acquire(
            &mut registry,
            &profile,
            200,
            "corky",
            250,
            0,
            1_050,
            alive_set(&[200]),
        );
        assert_eq!(reclaimed.status, GpuLeaseAcquisitionStatus::ReclaimedStale);
        assert_eq!(reclaimed.reclaimed.len(), 1);
        assert_eq!(registry.leases["qwen3-32b-q4"].len(), 1);
        assert_eq!(registry.leases["qwen3-32b-q4"][0].holder_pid, 200);
    }

    #[test]
    fn acquire_shared_profile_allows_multiple_live_holders() {
        let profile = profile_by_id("qwen3-embedding-0.6b").unwrap();
        let mut registry = GpuLeaseRegistry::default();
        apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            200,
            0,
            1_000,
            all_alive,
        );
        let second = apply_acquire(
            &mut registry,
            &profile,
            200,
            "headroom",
            250,
            0,
            1_050,
            alive_set(&[100, 200]),
        );
        assert_eq!(second.status, GpuLeaseAcquisitionStatus::Acquired);
        assert_eq!(registry.leases["qwen3-embedding-0.6b"].len(), 2);
    }

    #[test]
    fn acquire_refreshes_when_same_holder_requests_again() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        let mut registry = GpuLeaseRegistry::default();
        apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            200,
            0,
            1_000,
            all_alive,
        );
        let again = apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            180,
            0,
            1_500,
            all_alive,
        );
        assert_eq!(again.status, GpuLeaseAcquisitionStatus::Refreshed);
        assert_eq!(registry.leases["qwen3-32b-q4"].len(), 1);
        assert_eq!(
            registry.leases["qwen3-32b-q4"][0].acquired_at_unix_seconds,
            1_500
        );
        assert_eq!(registry.leases["qwen3-32b-q4"][0].vram_baseline_mib, 180);
    }

    #[test]
    fn acquire_cpu_or_hash_profile_bypasses_registry() {
        let profile = profile_by_id("tsift-local-hash-v1").unwrap();
        let mut registry = GpuLeaseRegistry::default();
        let bypass = apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            0,
            0,
            1_000,
            all_alive,
        );
        assert_eq!(bypass.status, GpuLeaseAcquisitionStatus::CpuOrHashBypass);
        assert!(registry.leases.is_empty());
    }

    #[test]
    fn idle_ttl_expires_even_when_pid_still_alive() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        let mut registry = GpuLeaseRegistry::default();
        apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            200,
            60,
            1_000,
            all_alive,
        );
        // 120s later, the 60s TTL has expired; pid 100 is still alive but stale.
        let reclaimed = apply_acquire(
            &mut registry,
            &profile,
            200,
            "corky",
            250,
            0,
            1_120,
            all_alive,
        );
        assert_eq!(reclaimed.status, GpuLeaseAcquisitionStatus::ReclaimedStale);
        assert_eq!(registry.leases["qwen3-32b-q4"][0].holder_pid, 200);
    }

    #[test]
    fn release_removes_holder_and_drops_empty_profile() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        let mut registry = GpuLeaseRegistry::default();
        apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            200,
            0,
            1_000,
            all_alive,
        );
        let release = apply_release(&mut registry, "qwen3-32b-q4", 100, 1_050, all_alive);
        assert_eq!(release.outcome, GpuLeaseReleaseOutcome::Released);
        assert_eq!(release.remaining_holders, 0);
        assert!(registry.leases.is_empty());
    }

    #[test]
    fn release_by_non_holder_reports_not_held() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        let mut registry = GpuLeaseRegistry::default();
        apply_acquire(
            &mut registry,
            &profile,
            100,
            "tsift",
            200,
            0,
            1_000,
            all_alive,
        );
        let release = apply_release(&mut registry, "qwen3-32b-q4", 999, 1_050, all_alive);
        assert_eq!(release.outcome, GpuLeaseReleaseOutcome::NotHeld);
        assert_eq!(registry.leases["qwen3-32b-q4"].len(), 1);
    }

    #[test]
    fn acquire_and_release_round_trip_through_file() {
        let dir = tempfile_dir();
        let path = dir.join("gpu-lease.json");
        let profile = profile_by_id("qwen3-32b-q4").unwrap();

        let mut registry = GpuLeaseRegistry::default();
        let acquisition = apply_acquire(
            &mut registry,
            &profile,
            4242,
            "tsift",
            220,
            0,
            1_000,
            all_alive,
        );
        assert_eq!(acquisition.status, GpuLeaseAcquisitionStatus::Acquired);
        write_lease_registry(&path, &registry).unwrap();

        let read_back = read_lease_registry(&path).unwrap();
        assert_eq!(read_back, registry);
        assert_eq!(read_back.leases["qwen3-32b-q4"][0].holder_pid, 4242);

        let release = apply_release(&mut registry, "qwen3-32b-q4", 4242, 1_050, all_alive);
        assert_eq!(release.outcome, GpuLeaseReleaseOutcome::Released);
        write_lease_registry(&path, &registry).unwrap();

        let after = read_lease_registry(&path).unwrap();
        assert!(after.leases.is_empty());
    }

    #[test]
    fn read_lease_registry_returns_default_for_missing_file() {
        let path = Path::new("/definitely/not/a/real/path/lease.json");
        let registry = read_lease_registry(path).unwrap();
        assert_eq!(registry, GpuLeaseRegistry::default());
    }

    #[test]
    fn prune_stale_leaves_healthy_entries_alone() {
        let mut registry = GpuLeaseRegistry::default();
        registry.leases.insert(
            "qwen3-32b-q4".to_string(),
            vec![GpuLeaseRecord {
                profile_id: "qwen3-32b-q4".to_string(),
                holder_pid: 100,
                holder_command: "tsift".to_string(),
                acquired_at_unix_seconds: 1_000,
                lease_mode: LeaseMode::Exclusive,
                vram_baseline_mib: 200,
                idle_ttl_seconds: 0,
                notes: Vec::new(),
            }],
        );
        let pruned = prune_stale_leases(&mut registry, 1_010, alive_set(&[100]));
        assert!(pruned.is_empty());
        assert!(registry.leases.contains_key("qwen3-32b-q4"));
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tsift-lease-test-{}-{}",
            std::process::id(),
            current_unix_seconds()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---- Per-call profile preference (#gctrl2) ----

    #[test]
    fn profile_preference_parses_cli_value() {
        assert_eq!(ProfilePreference::from_cli(None), ProfilePreference::Auto);
        assert_eq!(
            ProfilePreference::from_cli(Some("")),
            ProfilePreference::Auto
        );
        assert_eq!(
            ProfilePreference::from_cli(Some("hash")),
            ProfilePreference::ForceHash
        );
        assert_eq!(
            ProfilePreference::from_cli(Some("tsift-local-hash-v1")),
            ProfilePreference::ForceHash
        );
        assert_eq!(
            ProfilePreference::from_cli(Some("qwen3-32b-q4")),
            ProfilePreference::Pinned("qwen3-32b-q4".to_string())
        );
    }

    #[test]
    fn resolve_auto_picks_recommended_gpu_profile_on_clear_5090() {
        let probe = rtx_5090_probe();
        let resolution =
            resolve_profile_preference(&ProfilePreference::Auto, ModelRole::Extract, &probe);
        assert_eq!(resolution.source, ProfileResolutionSource::AutoRanked);
        assert!(resolution.selectable);
        assert_eq!(resolution.profile.id, "qwen3-32b-q4");
    }

    #[test]
    fn resolve_auto_falls_back_to_hash_when_gpu_unavailable() {
        let probe = GpuProbe::unavailable("missing");
        let resolution =
            resolve_profile_preference(&ProfilePreference::Auto, ModelRole::Extract, &probe);
        assert_eq!(resolution.source, ProfileResolutionSource::AutoRanked);
        assert_eq!(resolution.profile.id, "tsift-local-hash-v1");
        assert!(resolution.selectable);
        assert!(resolution.reason.contains("no GPU profile selectable"));
    }

    #[test]
    fn resolve_pinned_selectable_profile_is_used_as_is() {
        let probe = rtx_5090_probe();
        let resolution = resolve_profile_preference(
            &ProfilePreference::Pinned("qwen3-embedding-0.6b".to_string()),
            ModelRole::Embed,
            &probe,
        );
        assert_eq!(resolution.source, ProfileResolutionSource::Pinned);
        assert_eq!(resolution.profile.id, "qwen3-embedding-0.6b");
        assert!(resolution.selectable);
    }

    #[test]
    fn resolve_pinned_profile_with_wrong_role_falls_back_to_hash() {
        let probe = rtx_5090_probe();
        let resolution = resolve_profile_preference(
            &ProfilePreference::Pinned("qwen3-embedding-0.6b".to_string()),
            ModelRole::Extract,
            &probe,
        );
        assert_eq!(
            resolution.source,
            ProfileResolutionSource::PinnedUnselectable
        );
        assert_eq!(resolution.profile.id, "tsift-local-hash-v1");
        assert!(resolution.reason.contains("does not support role"));
    }

    #[test]
    fn resolve_pinned_profile_that_does_not_fit_vram_falls_back_to_hash() {
        // 30 GiB used → only ~2.6 GiB free → qwen3-32b-q4 (~28 GiB) won't fit.
        let probe = probe_with_used_vram(30_000);
        let resolution = resolve_profile_preference(
            &ProfilePreference::Pinned("qwen3-32b-q4".to_string()),
            ModelRole::Extract,
            &probe,
        );
        assert_eq!(
            resolution.source,
            ProfileResolutionSource::PinnedUnselectable
        );
        assert_eq!(resolution.profile.id, "tsift-local-hash-v1");
        assert!(resolution.selectable);
        assert!(resolution.reason.contains("not selectable"));
    }

    #[test]
    fn resolve_force_hash_always_uses_hash_profile() {
        let probe = rtx_5090_probe();
        let resolution =
            resolve_profile_preference(&ProfilePreference::ForceHash, ModelRole::Extract, &probe);
        assert_eq!(resolution.source, ProfileResolutionSource::ForcedHash);
        assert_eq!(resolution.profile.id, "tsift-local-hash-v1");
        assert!(resolution.selectable);
        assert!(resolution.reason.contains("forced"));
    }

    #[test]
    fn resolve_pinned_unknown_profile_id_falls_back_to_hash() {
        let probe = rtx_5090_probe();
        let resolution = resolve_profile_preference(
            &ProfilePreference::Pinned("not-a-real-profile".to_string()),
            ModelRole::Embed,
            &probe,
        );
        assert_eq!(
            resolution.source,
            ProfileResolutionSource::PinnedUnselectable
        );
        assert_eq!(resolution.profile.id, "tsift-local-hash-v1");
        assert!(resolution.reason.contains("unknown"));
    }

    // ---- Provider endpoint configurability (#portconf) ----

    #[test]
    fn resolve_endpoint_returns_explicit_override_for_any_strategy() {
        for strategy in [
            UnloadStrategy::LlamaCppRouterUnload,
            UnloadStrategy::OllamaKeepAliveZero,
            UnloadStrategy::VllmSleep,
            UnloadStrategy::ProcessExit,
            UnloadStrategy::None,
        ] {
            let resolved = resolve_provider_endpoint(&strategy, Some("http://custom:9999/path"));
            assert_eq!(
                resolved, "http://custom:9999/path",
                "explicit override should win for {strategy:?}"
            );
        }
    }

    #[test]
    fn resolve_endpoint_uses_compile_time_default_when_no_env_no_explicit() {
        // SAFETY: tests run single-threaded inside this function; the env is
        // not consulted by other code paths while we hold this scope and the
        // vars are cleared before returning.
        unsafe {
            std::env::remove_var(LLAMA_CPP_ENDPOINT_ENV_VAR);
            std::env::remove_var(OLLAMA_ENDPOINT_ENV_VAR);
            std::env::remove_var(VLLM_ENDPOINT_ENV_VAR);
        }
        assert_eq!(
            resolve_provider_endpoint(&UnloadStrategy::LlamaCppRouterUnload, None),
            DEFAULT_LLAMA_CPP_ENDPOINT
        );
        assert_eq!(
            resolve_provider_endpoint(&UnloadStrategy::OllamaKeepAliveZero, None),
            DEFAULT_OLLAMA_ENDPOINT
        );
        assert_eq!(
            resolve_provider_endpoint(&UnloadStrategy::VllmSleep, None),
            DEFAULT_VLLM_ENDPOINT
        );
        assert_eq!(
            resolve_provider_endpoint(&UnloadStrategy::ProcessExit, None),
            ""
        );
        assert_eq!(resolve_provider_endpoint(&UnloadStrategy::None, None), "");
    }

    #[test]
    fn resolve_endpoint_env_var_overrides_default_for_llama_cpp() {
        // SAFETY: see note in the previous test.
        unsafe {
            std::env::set_var(
                LLAMA_CPP_ENDPOINT_ENV_VAR,
                "http://127.0.0.1:8081/models/unload",
            );
        }
        let resolved = resolve_provider_endpoint(&UnloadStrategy::LlamaCppRouterUnload, None);
        unsafe {
            std::env::remove_var(LLAMA_CPP_ENDPOINT_ENV_VAR);
        }
        assert_eq!(resolved, "http://127.0.0.1:8081/models/unload");
    }

    #[test]
    fn resolve_endpoint_blank_env_var_falls_back_to_default() {
        // SAFETY: see note above.
        unsafe {
            std::env::set_var(LLAMA_CPP_ENDPOINT_ENV_VAR, "   ");
        }
        let resolved = resolve_provider_endpoint(&UnloadStrategy::LlamaCppRouterUnload, None);
        unsafe {
            std::env::remove_var(LLAMA_CPP_ENDPOINT_ENV_VAR);
        }
        assert_eq!(resolved, DEFAULT_LLAMA_CPP_ENDPOINT);
    }

    #[test]
    fn build_unload_actions_picks_up_env_var_for_llama_cpp_endpoint() {
        let profile = profile_by_id("qwen3-32b-q4").unwrap();
        // SAFETY: see note above.
        unsafe {
            std::env::set_var(
                LLAMA_CPP_ENDPOINT_ENV_VAR,
                "http://127.0.0.1:8081/models/unload",
            );
        }
        let actions = build_unload_actions(&profile, None, Some(42));
        unsafe {
            std::env::remove_var(LLAMA_CPP_ENDPOINT_ENV_VAR);
        }
        let unload_action = actions
            .iter()
            .find(|action| action.kind == UnloadActionKind::ProviderApi)
            .expect("provider api action present");
        assert_eq!(
            unload_action.endpoint.as_deref(),
            Some("http://127.0.0.1:8081/models/unload")
        );
    }

    // ---- Profile swap lifecycle (#gctrl3) ----

    fn probe_pair(pre_used: u64, post_used: u64) -> (GpuProbe, GpuProbe) {
        (
            probe_with_used_vram(pre_used),
            probe_with_used_vram(post_used),
        )
    }

    #[test]
    fn swap_to_same_profile_is_noop() {
        let from = profile_by_id("qwen3-32b-q4").unwrap();
        let to = profile_by_id("qwen3-32b-q4").unwrap();
        let (pre, post) = probe_pair(200, 200);
        let report = build_swap_report(
            from,
            to,
            pre,
            post,
            None,
            None,
            DEFAULT_IDLE_TTL_SECONDS,
            DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB,
        );
        assert_eq!(report.swap_status, SwapStatus::NoOpSameProfile);
    }

    #[test]
    fn swap_from_big_to_small_embedding_when_cleanup_proven_is_swapped() {
        let from = profile_by_id("qwen3-32b-q4").unwrap();
        let to = profile_by_id("qwen3-embedding-0.6b").unwrap();
        // Source was using ~28 GiB; after unload it returns to ~200 MiB.
        let (pre, post) = probe_pair(28_000, 200);
        let report = build_swap_report(
            from,
            to,
            pre,
            post,
            None,
            Some(42),
            DEFAULT_IDLE_TTL_SECONDS,
            DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB,
        );
        assert_eq!(report.swap_status, SwapStatus::Swapped);
        assert!(report.unload.cleanup.cleanup_proven);
        assert_eq!(report.target_resolution.profile.id, "qwen3-embedding-0.6b");
    }

    #[test]
    fn swap_to_hash_fallback_is_swapped_to_hash_when_cleanup_proven() {
        let from = profile_by_id("qwen3-32b-q4").unwrap();
        let to = profile_by_id("tsift-local-hash-v1").unwrap();
        let (pre, post) = probe_pair(28_000, 200);
        let report = build_swap_report(
            from,
            to,
            pre,
            post,
            None,
            Some(42),
            DEFAULT_IDLE_TTL_SECONDS,
            DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB,
        );
        assert_eq!(report.swap_status, SwapStatus::SwappedToHash);
        assert!(report.unload.cleanup.cleanup_proven);
    }

    #[test]
    fn swap_blocks_when_source_unload_not_proven() {
        let from = profile_by_id("qwen3-32b-q4").unwrap();
        let to = profile_by_id("qwen3-embedding-0.6b").unwrap();
        // Baseline VRAM is ~200 MiB before load. Orphaned llama-server process
        // holds ~7 GiB after "unload", so cleanup is NOT proven.
        let pre = probe_with_used_vram(200);
        let mut post = probe_with_used_vram(8_000);
        post.processes.push(GpuProcess {
            pid: Some(42),
            process_name: "llama-server".to_string(),
            used_memory_mib: Some(7_000),
        });
        let report = build_swap_report(
            from,
            to,
            pre,
            post,
            None,
            Some(42),
            DEFAULT_IDLE_TTL_SECONDS,
            DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB,
        );
        assert_eq!(report.swap_status, SwapStatus::UnloadNotProven);
        assert!(!report.unload.cleanup.cleanup_proven);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("DO NOT load target"))
        );
    }

    #[test]
    fn swap_reports_target_unselectable_when_post_unload_vram_still_high() {
        let from = profile_by_id("qwen3-32b-q4").unwrap();
        let to = profile_by_id("qwen3-32b-q4").unwrap();
        // Source is qwen3-32b-q4 itself; after unload, only ~3 GiB free — the
        // target 32B footprint (28.7 GiB) does not fit. Cleanup is proven
        // (post <= pre + tolerance), but the target cannot reload.
        let pre = probe_with_used_vram(29_500);
        let post = probe_with_used_vram(29_600);
        let report = build_swap_report(
            from,
            to,
            pre,
            post,
            None,
            None,
            DEFAULT_IDLE_TTL_SECONDS,
            DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB,
        );
        // from == to is the NoOpSameProfile path; pick distinct ids instead.
        assert_eq!(report.swap_status, SwapStatus::NoOpSameProfile);
        // Re-run with a distinct target to exercise UnloadProvenTargetUnselectable.
        let from = profile_by_id("qwen3-32b-q4").unwrap();
        let to = profile_by_id("qwen3-embedding-8b").unwrap();
        let pre = probe_with_used_vram(30_000);
        let post = probe_with_used_vram(30_500);
        let report = build_swap_report(
            from,
            to,
            pre,
            post,
            None,
            None,
            DEFAULT_IDLE_TTL_SECONDS,
            DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB,
        );
        assert_eq!(
            report.swap_status,
            SwapStatus::UnloadProvenTargetUnselectable
        );
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("not selectable on the post-unload probe"))
        );
    }
}
