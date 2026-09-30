//! Local model lifecycle, observed hardware and measured compatibility.
//!
//! Inference transport accepts literal loopback addresses only, ignores system
//! proxies and refuses redirects. Registry downloads are a separate explicit
//! operation. No cloud provider fallback is part of this subsystem.

pub mod disk;
pub mod edit;
pub mod hardware;
#[cfg(all(windows, target_arch = "x86_64"))]
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

/// Estimate a conservative 4K-context cold-load working set. The model's
/// context ceiling is unknown here, so this is not an automatic recommendation.
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
    let host_headroom = hardware.ram_available_bytes.map(|n| n >= 2 * GIB);
    let (status, explanation) = if weights_bytes == 0 || host_headroom.is_none() {
        (
            FitStatus::Unknown,
            "Memory or model size is unknown. Measure before loading.",
        )
    } else if host_headroom == Some(false) {
        (
            FitStatus::InsufficientMemory,
            "Less than 2 GiB host RAM is available. Close other workloads before loading a model.",
        )
    } else if hardware
        .gpus
        .iter()
        .any(|gpu| gpu.available_bytes >= required)
    {
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
}
