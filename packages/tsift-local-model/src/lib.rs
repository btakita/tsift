use anyhow::{Context, Result};
use serde::Serialize;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_DESKTOP_RUNTIME_MARGIN_MIB: u64 = 4096;
pub const DEFAULT_VRAM_CLEANUP_TOLERANCE_MIB: u64 = 768;
pub const DEFAULT_IDLE_TTL_SECONDS: u64 = 0;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
            let endpoint = provider_endpoint
                .unwrap_or("http://127.0.0.1:8080/models/unload")
                .to_string();
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
                endpoint: Some(
                    provider_endpoint
                        .unwrap_or("http://127.0.0.1:11434/api/generate")
                        .to_string(),
                ),
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
            let endpoint = provider_endpoint
                .unwrap_or("http://127.0.0.1:8000/sleep")
                .to_string();
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
    if let Some(pid) = post_process.pid {
        if let Some(process) = pre_load_gpu_probe
            .processes
            .iter()
            .find(|candidate| candidate.pid == Some(pid))
        {
            return Some(process);
        }
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

fn current_unix_seconds() -> u64 {
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
}
