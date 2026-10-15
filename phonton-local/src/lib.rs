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

use phonton_types::local::{FitStatus, HardwareSnapshot, ModelFit};

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

/// Estimate a conservative 4K-context cold-load working set. The model's
/// context ceiling is unknown here, so this is not an automatic recommendation.
/// Host RAM a GPU-resident model needs free: the runtime process only. Matches
/// the calibration reserve, so a calibrated model is admitted under the same
/// conditions it was measured in.
pub const GPU_HOST_RESERVE_BYTES: u64 = 1536 * 1024 * 1024;

pub fn estimate_fit(weights_bytes: u64, hardware: &HardwareSnapshot) -> ModelFit {
    estimate_fit_for_context(weights_bytes, hardware, 4096)
}

/// Increase the KV/runtime reserve with requested context. This deliberately
/// conservative estimate is not a substitute for runtime measurement.
pub fn estimate_fit_for_context(
    weights_bytes: u64,
    hardware: &HardwareSnapshot,
    context: u32,
) -> ModelFit {
    const GIB: u64 = 1024 * 1024 * 1024;
    let required = weights_bytes
        .saturating_add(weights_bytes / 4)
        .saturating_add(GIB.saturating_mul(u64::from(context).div_ceil(4096).max(1)));
    let gpu_fit = hardware
        .gpus
        .iter()
        .any(|gpu| gpu.available_bytes >= required);
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
        (FitStatus::LikelyFitsGpu, "Weights plus 25% and a context-scaled reserve fit one observed GPU. KV cache is estimated; calibrate before use.")
    } else if hardware
        .ram_available_bytes
        .is_some_and(|n| n >= required.saturating_add(2 * GIB))
    {
        (FitStatus::CpuOrOffload, "Estimated working set fits available RAM with 2 GiB host reserve. CPU/offload speed is unmeasured.")
    } else {
        (FitStatus::InsufficientMemory, "Estimated working set exceeds available memory with reserves. RAM and VRAM are not treated as pooled capacity.")
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
    hardware: &HardwareSnapshot,
    model_ceiling: Option<u32>,
) -> Option<u32> {
    let ceiling = model_ceiling?;
    let candidates = [32768, 16384, 8192, 4096, 2048];
    for desired in [FitStatus::LikelyFitsGpu, FitStatus::CpuOrOffload] {
        for context in candidates {
            if context <= ceiling
                && estimate_fit_for_context(weights_bytes, hardware, context).status == desired
            {
                return Some(context);
            }
        }
    }
    None
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
            recommend_context(500_000_000, &HardwareSnapshot::default(), Some(32768)),
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
        assert_eq!(recommend_context(1 << 30, &h, None), None);
        assert_eq!(recommend_context(1 << 30, &h, Some(1024)), None);
        assert_eq!(recommend_context(1 << 30, &h, Some(32768)), Some(4096));
        assert_eq!(recommend_context(1 << 30, &h, Some(2048)), Some(2048));
        assert_eq!(
            estimate_fit_for_context(1 << 30, &h, 8192).status,
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
        assert_eq!(recommend_context(1 << 30, &h, Some(32768)), Some(4096));
        assert_eq!(recommend_context(0, &h, Some(32768)), None);
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
}
