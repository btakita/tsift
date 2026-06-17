use anyhow::{Context, Result};
use serde::Serialize;
use std::process::Command;

pub const DEFAULT_DESKTOP_RUNTIME_MARGIN_MIB: u64 = 4096;

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

fn parse_gpu_query(line: &str) -> Result<GpuProbe> {
    let parts = line.split(',').map(str::trim).collect::<Vec<_>>();
    if parts.len() != 5 {
        anyhow::bail!("unexpected nvidia-smi gpu query row: {line}");
    }
    Ok(GpuProbe {
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
        assert_eq!(probe.gpu_name.as_deref(), Some("NVIDIA GeForce RTX 5090"));
        assert_eq!(probe.total_vram_mib, Some(32_607));
        assert_eq!(probe.used_vram_mib, Some(179));
        assert_eq!(probe.free_vram_mib, Some(32_428));
    }
}
