//! Observations and profiles shared by local model management and user surfaces.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Physical resources observed on this host. Missing measurements stay unknown.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HardwareSnapshot {
    pub cpu: Option<String>,
    pub logical_cpus: usize,
    pub ram_total_bytes: Option<u64>,
    pub ram_available_bytes: Option<u64>,
    pub gpus: Vec<GpuSnapshot>,
    pub warnings: Vec<String>,
}

/// Dedicated GPU memory; never added to RAM to imply pooled capacity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuSnapshot {
    pub name: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// Conservative admission estimate, not a performance or compatibility score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FitStatus {
    LikelyFitsGpu,
    CpuOrOffload,
    InsufficientMemory,
    Unknown,
}

/// Explicit assumptions behind a resource estimate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFit {
    pub status: FitStatus,
    pub estimated_required_bytes: u64,
    /// Context used for this cold-load estimate, not a measured throughput result.
    pub context_tokens: u32,
    /// Bounded automatic choice, absent when model limits or memory are unknown.
    pub suggested_context: Option<u32>,
    pub explanation: String,
}

/// Runtime-reported installed model identity and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalModel {
    pub name: String,
    pub digest: String,
    pub size_bytes: u64,
    pub parameter_size: Option<String>,
    pub quantization: Option<String>,
}

/// Usable installed models and diagnostics for incomplete runtime entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledInventory {
    pub models: Vec<LocalModel>,
    pub warnings: Vec<String>,
}

/// Runtime-observed model already loaded for inference. This is a point-in-time
/// observation, not a reservation of RAM or VRAM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResidentModel {
    pub name: String,
    pub digest: String,
    pub size_bytes: u64,
    pub size_vram_bytes: u64,
    pub context_length: u32,
    pub expires_at_unix: i64,
}

/// A real registry entry. Availability and sizes must come from its manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub name: String,
    pub source: String,
    pub download_bytes: Option<u64>,
    pub fit: Option<ModelFit>,
    pub error: Option<String>,
    /// Resource-only starting point, absent without usable manifest and memory evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_try_reason: Option<String>,
    /// Conservative drive planning before a fresh Phonton-managed runtime setup.
    /// Absent when the storage choice is fixed or disk/manifest readings are unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_setup_storage: Option<PreSetupStoragePlan>,
}

/// Point-in-time allowance for runtime staging plus one manifest-backed model.
/// It does not reserve disk space; setup and model pull repeat live admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreSetupStoragePlan {
    pub root: PathBuf,
    pub available_bytes: u64,
    pub runtime_setup_min_free_bytes: u64,
    pub model_download_bytes: u64,
    pub pull_reserve_bytes: u64,
    pub required_bytes: u64,
    pub shortfall_bytes: u64,
}

/// Catalog entries and the exact hardware reading used for their fit estimates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub hardware: HardwareSnapshot,
    pub models: Vec<CatalogModel>,
}

/// A download event reported by the runtime, never a simulated percentage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub status: String,
    pub digest: Option<String>,
    pub completed: Option<u64>,
    pub total: Option<u64>,
}

/// Point-in-time managed-store disk estimate before an Ollama model pull.
/// Only complete blobs whose size and SHA-256 match the fetched manifest are
/// credited; an interrupted partial blob is never treated as downloaded space.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelDownloadAdmission {
    pub manifest_bytes: u64,
    pub credited_existing_bytes: u64,
    pub remaining_bytes: u64,
    pub reserve_bytes: u64,
    pub available_bytes: u64,
}

/// Verification vocabulary that keeps an absent check distinct from a pass.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    NotRun,
    Unavailable,
}

/// Name of the constrained JSON new-file transport probe in a model profile.
pub const CREATION_PROBE_NAME: &str = "Structured create";

/// Name of the unified-diff new-file transport probe in a model profile.
pub const DIFF_CREATION_PROBE_NAME: &str = "UnifiedDiff create";

/// Editing wire protocol demonstrated by a compatibility probe.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditProtocol {
    UnifiedDiff,
    SearchReplace,
}

/// Ollama thinking request used by the probes and every coding inference.
/// RuntimeDefault preserves the behavior of profiles saved before this field.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalThinkingMode {
    #[default]
    RuntimeDefault,
    Off,
}

/// One observable compatibility probe; raw output makes the result inspectable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProbe {
    pub name: String,
    pub status: CheckStatus,
    pub output: String,
    pub detail: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub elapsed_ms: u64,
}

/// Durable, explicitly incomplete calibration evidence. It is never a
/// selectable profile; a retry starts fresh and replaces this attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationAttempt {
    pub schema: u32,
    pub model: String,
    /// Digest observed before probing; incomplete runs do not certify that
    /// every response came from unchanged weights.
    pub starting_digest: String,
    pub runtime_version: String,
    pub endpoint: String,
    pub context_tokens: u32,
    pub hardware: HardwareSnapshot,
    pub started_at_unix: u64,
    pub probes: Vec<ModelProbe>,
}

/// Durable identity of an install request that has not been confirmed complete.
/// Admission may fail before transfer, or a runtime may retain partial layers;
/// this record never proves installation or reusable bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallAttempt {
    pub schema: u32,
    pub model: String,
    pub endpoint: String,
    pub started_at_unix: u64,
}

/// Digest-bound measured settings. This is probe evidence, not a coding benchmark.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfile {
    pub schema: u32,
    pub model: String,
    pub digest: String,
    pub runtime_version: String,
    pub endpoint: String,
    pub context_tokens: u32,
    /// Per-goal generation ceiling derived from context; probe limits are separate.
    pub output_tokens: u32,
    pub protocol: Option<EditProtocol>,
    /// Schema-2 measured request mode. None on schema-1 legacy profiles means
    /// omit the Ollama think parameter, matching their original calibration.
    /// A schema-2 profile missing this field is invalid, not silently defaulted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<LocalThinkingMode>,
    pub probes: Vec<ModelProbe>,
    pub hardware: HardwareSnapshot,
    pub measured_at_unix: u64,
}

impl ModelProfile {
    /// Keep absent probes on older profiles distinct from measured success.
    pub fn creation_status(&self) -> CheckStatus {
        let name = match self.protocol {
            Some(EditProtocol::SearchReplace) => CREATION_PROBE_NAME,
            Some(EditProtocol::UnifiedDiff) => DIFF_CREATION_PROBE_NAME,
            None => return CheckStatus::NotRun,
        };
        self.probes
            .iter()
            .find(|probe| probe.name == name)
            .map(|probe| probe.status.clone())
            .unwrap_or(CheckStatus::NotRun)
    }
}

/// Windows volume and directory identity for a Phonton-managed folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDirectoryIdentity {
    /// The serial number reported by the volume that owns this directory.
    pub volume_serial: u64,
    /// The directory's 128-bit file ID on that volume.
    pub file_id: [u8; 16],
}

/// Persisted local model settings, independent of any repository or cloud key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSettings {
    pub schema: u32,
    pub endpoint: String,
    pub active_model: Option<String>,
    pub profiles: Vec<ModelProfile>,
    /// Last in-progress or interrupted attempt, kept outside valid profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration_attempt: Option<CalibrationAttempt>,
    /// Recent install requests not yet confirmed complete. Schema 5 keeps
    /// older engines from silently erasing their retry identities on writes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub install_attempts: Vec<InstallAttempt>,
    /// Optional dedicated directory for the managed runtime, models, and new
    /// local coding-run evidence.
    /// Legacy state and explicit PHONTON_LOCAL_STATE installations keep their
    /// state-adjacent runtime directory.
    #[serde(default)]
    pub managed_root: Option<std::path::PathBuf>,
    /// Identity of the selected directory; a reused drive letter or replaced
    /// folder must not be mistaken for the original model store.
    #[serde(default)]
    pub managed_root_identity: Option<LocalDirectoryIdentity>,
    /// Setup or a coding run claimed the chosen folder. An unavailable drive
    /// must not make its existing files look safe to relocate.
    #[serde(default)]
    pub managed_root_used: bool,
    /// A verified portable runtime was installed or a prior Phonton-managed
    /// launch was observed here. Run evidence alone does not set this marker.
    #[serde(default)]
    pub managed_runtime_installed: bool,
}

impl Default for LocalSettings {
    fn default() -> Self {
        Self {
            schema: 1,
            endpoint: "http://127.0.0.1:11434".into(),
            active_model: None,
            profiles: Vec::new(),
            calibration_attempt: None,
            install_attempts: Vec::new(),
            managed_root: None,
            managed_root_identity: None,
            managed_root_used: false,
            managed_runtime_installed: false,
        }
    }
}
