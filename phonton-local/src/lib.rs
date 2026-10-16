//! Local model lifecycle, observed hardware and measured compatibility.
//!
//! Inference transport accepts literal loopback addresses only, ignores system
//! proxies and refuses redirects. Registry downloads are a separate explicit
//! operation. No cloud provider fallback is part of this subsystem.

pub mod disk;
pub mod edit;
pub mod hardware;
#[cfg(managed_runtime)]
pub mod managed_store;
pub mod provision;
pub mod runtime;
pub mod storage;

use phonton_types::local::{FitStatus, HardwareSnapshot, ModelFit, ResidentModel};

/// Errors are actionable at both CLI and Desktop boundaries.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("{0}")]
    Invalid(String),
    #[error("local runtime request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("local state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid runtime or state JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("resident model became unavailable before inference: {0}")]
    ResidentUnavailable(String),
    #[error("operation cancelled")]
    Cancelled,
}

/// Result for local model operations.
pub type Result<T> = std::result::Result<T, LocalError>;

/// One curated catalog model and the memory tier it is recommended for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Exact Ollama library tag.
    pub name: &'static str,
    /// Smallest [`model_memory_budget`], in decimal GB, this model is
    /// recommended for.
    pub tier_gb: u64,
    /// Listed as an alternative; never the tier's recommendation.
    pub alternative: bool,
}

const fn entry(name: &'static str, tier_gb: u64, alternative: bool) -> CatalogEntry {
    CatalogEntry {
        name,
        tier_gb,
        alternative,
    }
}

/// Curated local coding models, smallest tier first. Tags checked against
/// registry.ollama.ai on 2026-10-15 (downloads 3.3, 6.6, 8.0, 13.8, 17.8
/// and 22.6 GB).
pub const CATALOG: [CatalogEntry; 6] = [
    entry("qwen3.5:4b", 0, false),
    entry("qwen3.5:9b", 6, false),
    entry("gemma4:12b", 10, true),
    entry("gpt-oss:20b", 16, false),
    entry("qwen3.6:27b", 24, true),
    entry("qwen3.6:35b-a3b", 24, false),
];

/// Memory a local model can use on this machine: the largest GPU's total
/// VRAM, or half of system RAM when that is larger (unified memory, CPU
/// offload). Total, not free: closing apps frees RAM, nothing grows a GPU.
pub fn model_memory_budget(hardware: &HardwareSnapshot) -> Option<u64> {
    let gpu = hardware.gpus.iter().map(|gpu| gpu.total_bytes).max();
    gpu.max(hardware.ram_total_bytes.map(|bytes| bytes / 2))
}

/// The catalog model recommended for this machine's memory tier, or `None`
/// without a memory reading. A size-based starting point, not a
/// coding-quality result; calibration still decides whether it is usable.
pub fn recommended_model(hardware: &HardwareSnapshot) -> Option<&'static str> {
    let budget = model_memory_budget(hardware)?;
    CATALOG
        .iter()
        .rev()
        .find(|entry| !entry.alternative && budget >= entry.tier_gb * 1_000_000_000)
        .map(|entry| entry.name)
}

/// Host RAM a GPU-resident model needs free: the runtime process only. Matches
/// the calibration reserve, so a calibrated model is admitted under the same
/// conditions it was measured in.
pub const GPU_HOST_RESERVE_BYTES: u64 = 1536 * 1024 * 1024;

const GIB: u64 = 1024 * 1024 * 1024;

/// Memory the runtime needs beyond weights and KV cache: GPU context, compute
/// graph and a vision projector's image buffers. On Ollama 0.34.2 (2026-10-16,
/// RTX 4050) qwen3.5:4b at 16384 tokens held 4224 MiB of VRAM: its 3233 MiB
/// download, a 512 MiB KV cache and 479 MiB more, before any image input.
// ponytail: fixed reserve, scale with embedding width if 30B+ models OOM.
const RUNTIME_RESERVE_BYTES: u64 = GIB;

/// KV cache per token when the runtime does not report the model's attention
/// shape: 1 GiB per 4096 tokens, above every catalog model's real cache.
const FALLBACK_KV_BYTES_PER_TOKEN: u64 = 256 * 1024;

/// Estimate a 4K-context cold load without the model's attention shape, as
/// for a catalog model that is not downloaded yet. The model's context
/// ceiling is unknown here, so this is not an automatic recommendation.
pub fn estimate_fit(weights_bytes: u64, hardware: &HardwareSnapshot) -> ModelFit {
    estimate_fit_for_context(weights_bytes, None, hardware, 4096)
}

/// Estimate a cold load at `context` tokens: weights, KV cache and a fixed
/// runtime reserve. `kv_bytes_per_token` comes from
/// [`runtime::model_kv_bytes_per_token`]; `None` uses a conservative 256 KiB.
/// A model that does not fit the largest GPU may still load with the rest
/// offloaded to RAM. Not a substitute for runtime measurement.
pub fn estimate_fit_for_context(
    weights_bytes: u64,
    kv_bytes_per_token: Option<u64>,
    hardware: &HardwareSnapshot,
    context: u32,
) -> ModelFit {
    let required = weights_bytes
        .saturating_add(
            kv_bytes_per_token
                .unwrap_or(FALLBACK_KV_BYTES_PER_TOKEN)
                .saturating_mul(u64::from(context)),
        )
        .saturating_add(RUNTIME_RESERVE_BYTES);
    // Independent GPUs are not summed; the runtime offloads to one GPU + RAM.
    let largest_gpu = hardware
        .gpus
        .iter()
        .map(|gpu| gpu.available_bytes)
        .max()
        .unwrap_or(0);
    let gpu_fit = largest_gpu >= required;
    // A model that fits in VRAM needs host memory only for the runtime
    // process, so it uses the same reserve calibration measures under.
    // CPU/offload keeps the 2 GiB host reserve.
    let host_floor = if gpu_fit {
        GPU_HOST_RESERVE_BYTES
    } else {
        2 * GIB
    };
    let host_headroom = hardware.ram_available_bytes.map(|n| n >= host_floor);
    let (status, explanation) = if weights_bytes == 0 || host_headroom.is_none() {
        (
            FitStatus::Unknown,
            "Memory or model size is unknown. Measure before loading.",
        )
    } else if host_headroom == Some(false) {
        (
            FitStatus::InsufficientMemory,
            if gpu_fit {
                "Less than 1.5 GiB host RAM is available for the runtime. Close other workloads before loading a model."
            } else {
                "Less than 2 GiB host RAM is available. Close other workloads before loading a model."
            },
        )
    } else if gpu_fit {
        (FitStatus::LikelyFitsGpu, "Weights, estimated KV cache and a 1 GiB runtime reserve fit one observed GPU. Calibrate before use.")
    } else if hardware
        .ram_available_bytes
        .is_some_and(|ram| largest_gpu.saturating_add(ram.saturating_sub(2 * GIB)) >= required)
    {
        (FitStatus::CpuOrOffload, "Estimated working set fits the largest GPU plus available RAM with a 2 GiB host reserve; the runtime offloads the rest to the CPU. Offload speed is unmeasured.")
    } else {
        (FitStatus::InsufficientMemory, "Estimated working set exceeds available memory: the largest GPU plus available RAM, less reserves.")
    };
    ModelFit {
        status,
        estimated_required_bytes: required,
        context_tokens: context,
        suggested_context: None,
        explanation: explanation.into(),
    }
}

/// Choose the largest estimated GPU context, falling back to CPU/offload only
/// when no supported GPU context fits. A missing model limit or memory reading
/// never becomes an invented recommendation.
pub fn recommend_context(
    weights_bytes: u64,
    kv_bytes_per_token: Option<u64>,
    hardware: &HardwareSnapshot,
    model_ceiling: Option<u32>,
) -> Option<u32> {
    let ceiling = model_ceiling?;
    let candidates = [32768, 16384, 8192, 4096, 2048];
    for desired in [FitStatus::LikelyFitsGpu, FitStatus::CpuOrOffload] {
        for context in candidates {
            if context <= ceiling
                && estimate_fit_for_context(weights_bytes, kv_bytes_per_token, hardware, context)
                    .status
                    == desired
            {
                return Some(context);
            }
        }
    }
    None
}

/// Hardware as a cold load of `resident`'s model sees it: the runtime unloads
/// the loaded copy before reloading it at another context, so its memory
/// counts as free. Only the exact model (name and digest) may be passed. VRAM
/// is credited only with a single observed GPU, where the copy must live.
pub fn credit_resident(hardware: &HardwareSnapshot, resident: &ResidentModel) -> HardwareSnapshot {
    let mut credited = hardware.clone();
    if let [gpu] = credited.gpus.as_mut_slice() {
        gpu.available_bytes = gpu
            .available_bytes
            .saturating_add(resident.size_vram_bytes)
            .min(gpu.total_bytes);
    }
    credited.ram_available_bytes = credited.ram_available_bytes.map(|ram| {
        ram.saturating_add(resident.size_bytes.saturating_sub(resident.size_vram_bytes))
    });
    credited
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::local::GpuSnapshot;

    #[test]
    fn unknown_memory_is_not_a_recommendation() {
        let fit = estimate_fit(500_000_000, &HardwareSnapshot::default());
        assert_eq!(fit.status, FitStatus::Unknown);
        assert_eq!(fit.context_tokens, 4096);
        assert_eq!(fit.suggested_context, None);
        assert_eq!(
            recommend_context(500_000_000, None, &HardwareSnapshot::default(), Some(32768)),
            None
        );
    }

    #[test]
    fn gpu_resident_models_need_only_the_runtime_reserve() {
        let gpu = |free: u64| GpuSnapshot {
            name: "GPU".into(),
            total_bytes: 6 << 30,
            available_bytes: free,
        };
        let h = HardwareSnapshot {
            ram_available_bytes: Some(1800 << 20),
            gpus: vec![gpu(5 << 30)],
            ..Default::default()
        };
        assert_eq!(estimate_fit(1 << 30, &h).status, FitStatus::LikelyFitsGpu);
        let no_gpu = HardwareSnapshot {
            gpus: vec![gpu(1 << 30)],
            ..h
        };
        assert_eq!(
            estimate_fit(1 << 30, &no_gpu).status,
            FitStatus::InsufficientMemory
        );
    }

    #[test]
    fn host_pressure_blocks_even_with_free_gpu() {
        let h = HardwareSnapshot {
            ram_available_bytes: Some(1024),
            gpus: vec![GpuSnapshot {
                name: "GPU".into(),
                total_bytes: 24 << 30,
                available_bytes: 24 << 30,
            }],
            ..Default::default()
        };
        assert_eq!(
            estimate_fit(1 << 30, &h).status,
            FitStatus::InsufficientMemory
        );
    }

    #[test]
    fn independent_gpu_memory_is_not_summed() {
        let h = HardwareSnapshot {
            ram_available_bytes: Some(3 << 30),
            gpus: vec![
                GpuSnapshot {
                    name: "GPU".into(),
                    total_bytes: 4 << 30,
                    available_bytes: 4 << 30
                };
                2
            ],
            ..Default::default()
        };
        assert_eq!(
            estimate_fit(5 << 30, &h).status,
            FitStatus::InsufficientMemory
        );
    }

    #[test]
    fn automatic_context_respects_headroom_and_model_ceiling() {
        let h = HardwareSnapshot {
            ram_available_bytes: Some(3 << 30),
            gpus: vec![GpuSnapshot {
                name: "GPU".into(),
                total_bytes: 3 << 30,
                available_bytes: 3 << 30,
            }],
            ..Default::default()
        };
        assert_eq!(recommend_context(1 << 30, None, &h, None), None);
        assert_eq!(recommend_context(1 << 30, None, &h, Some(1024)), None);
        assert_eq!(
            recommend_context(1 << 30, None, &h, Some(32768)),
            Some(4096)
        );
        assert_eq!(recommend_context(1 << 30, None, &h, Some(2048)), Some(2048));
        // 4 GiB: the GPU plus the RAM above the 2 GiB host reserve.
        assert_eq!(
            estimate_fit_for_context(1 << 30, None, &h, 8192).status,
            FitStatus::CpuOrOffload
        );
        assert_eq!(
            estimate_fit_for_context(1 << 30, None, &h, 16384).status,
            FitStatus::InsufficientMemory
        );
    }

    #[test]
    fn automatic_context_prefers_gpu_to_larger_cpu_offload() {
        let h = HardwareSnapshot {
            ram_available_bytes: Some(20 << 30),
            gpus: vec![GpuSnapshot {
                name: "GPU".into(),
                total_bytes: 3 << 30,
                available_bytes: 3 << 30,
            }],
            ..Default::default()
        };
        assert_eq!(
            recommend_context(1 << 30, None, &h, Some(32768)),
            Some(4096)
        );
        assert_eq!(recommend_context(0, None, &h, Some(32768)), None);
    }

    #[test]
    fn recommendation_follows_total_memory_tiers() {
        const GB: u64 = 1_000_000_000;
        let machine = |gpu_gb: Option<u64>, ram_gb: u64| HardwareSnapshot {
            ram_total_bytes: Some(ram_gb * GB),
            // Free memory never changes the tier.
            ram_available_bytes: Some(GB),
            gpus: gpu_gb
                .map(|gb| GpuSnapshot {
                    name: "GPU".into(),
                    total_bytes: gb * GB,
                    available_bytes: 0,
                })
                .into_iter()
                .collect(),
            ..Default::default()
        };
        assert_eq!(recommended_model(&HardwareSnapshot::default()), None);
        // RTX 4050 laptop: 6 GB VRAM, 16 GB RAM.
        assert_eq!(recommended_model(&machine(Some(6), 16)), Some("qwen3.5:9b"));
        assert_eq!(recommended_model(&machine(None, 8)), Some("qwen3.5:4b"));
        assert_eq!(
            recommended_model(&machine(Some(16), 32)),
            Some("gpt-oss:20b")
        );
        assert_eq!(recommended_model(&machine(None, 36)), Some("gpt-oss:20b"));
        assert_eq!(
            recommended_model(&machine(Some(24), 32)),
            Some("qwen3.6:35b-a3b")
        );
        assert_eq!(
            recommended_model(&machine(None, 64)),
            Some("qwen3.6:35b-a3b")
        );
    }

    #[test]
    fn qwen35_on_a_6_gb_laptop_gpu() {
        // RTX 4050 Laptop as nvidia-smi reported it on 2026-10-16, and the
        // 32 KiB-per-token KV cache Ollama reports for qwen3.5 4b and 9b.
        const KV: Option<u64> = Some(32 * 1024);
        const QWEN35_4B: u64 = 3_389_983_735;
        const QWEN35_9B: u64 = 6_600_000_000;
        let laptop = |ram_free: u64| HardwareSnapshot {
            ram_total_bytes: Some(15_556 << 20),
            ram_available_bytes: Some(ram_free),
            gpus: vec![GpuSnapshot {
                name: "RTX 4050".into(),
                total_bytes: 6141 << 20,
                available_bytes: 5815 << 20,
            }],
            ..Default::default()
        };
        let busy = laptop(2 << 30);
        assert_eq!(
            estimate_fit_for_context(QWEN35_4B, KV, &busy, 16384).status,
            FitStatus::LikelyFitsGpu
        );
        assert_eq!(
            recommend_context(QWEN35_4B, KV, &busy, Some(262_144)),
            Some(32768)
        );
        // Without the attention shape the fallback still refuses 16384.
        assert_eq!(
            estimate_fit_for_context(QWEN35_4B, None, &busy, 16384).status,
            FitStatus::InsufficientMemory
        );
        // The tier pick for this laptop loads partly offloaded once ~4 GiB
        // of RAM is free; it never fits the GPU alone.
        assert_eq!(recommended_model(&busy), Some("qwen3.5:9b"));
        assert_eq!(
            estimate_fit_for_context(QWEN35_9B, KV, &busy, 16384).status,
            FitStatus::InsufficientMemory
        );
        assert_eq!(
            estimate_fit_for_context(QWEN35_9B, KV, &laptop(4 << 30), 16384).status,
            FitStatus::CpuOrOffload
        );
    }

    #[test]
    fn a_loaded_copy_of_the_model_counts_as_free_memory() {
        let resident = ResidentModel {
            name: "qwen3.5:4b".into(),
            digest: "digest".into(),
            size_bytes: 4 << 30,
            size_vram_bytes: 3 << 30,
            context_length: 4096,
            expires_at_unix: 0,
        };
        // The loaded copy holds most of the GPU and RAM is below the 2 GiB
        // offload floor, so a reload at 16384 tokens looks impossible.
        let h = HardwareSnapshot {
            ram_available_bytes: Some(1800 << 20),
            gpus: vec![GpuSnapshot {
                name: "GPU".into(),
                total_bytes: 6 << 30,
                available_bytes: 2 << 30,
            }],
            ..Default::default()
        };
        let kv = Some(32 * 1024);
        assert_eq!(
            estimate_fit_for_context(3 << 30, kv, &h, 16384).status,
            FitStatus::InsufficientMemory
        );
        let credited = credit_resident(&h, &resident);
        assert_eq!(credited.gpus[0].available_bytes, 5 << 30);
        assert_eq!(credited.ram_available_bytes, Some((1800 << 20) + (1 << 30)));
        assert_eq!(
            estimate_fit_for_context(3 << 30, kv, &credited, 16384).status,
            FitStatus::LikelyFitsGpu
        );
        // With two GPUs the copy's VRAM is credited to neither.
        let two = HardwareSnapshot {
            gpus: vec![h.gpus[0].clone(); 2],
            ..h
        };
        assert!(credit_resident(&two, &resident)
            .gpus
            .iter()
            .all(|gpu| gpu.available_bytes == 2 << 30));
    }
}
