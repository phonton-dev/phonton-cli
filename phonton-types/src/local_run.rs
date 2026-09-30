//! Immutable local run requests and persistent candidate evidence.
use crate::local::{CheckStatus, EditProtocol, HardwareSnapshot, ModelProfile, ResidentModel};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// User-selected check; model output can never change these arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalCheck {
    pub program: String,
    pub args: Vec<String>,
}

/// One resource ledger governs every generation, repair, and verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchBudget {
    /// Maximum reserved model-call attempts, including edits, repairs and strategies.
    pub generations: u32,
    pub check_runs: u32,
    pub wall_seconds: u64,
    pub generated_tokens: u64,
}
impl Default for SearchBudget {
    fn default() -> Self {
        Self {
            generations: 4,
            check_runs: 16,
            wall_seconds: 600,
            generated_tokens: 4096,
        }
    }
}

/// Explicit scope and verification contract captured before inference starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRunRequest {
    pub goal: String,
    pub repository: PathBuf,
    #[serde(default)]
    pub files: Vec<PathBuf>,
    /// One explicitly requested absent path. Never inferred from goal text.
    #[serde(default)]
    pub new_file: Option<PathBuf>,
    /// Existing paths explicitly approved for edits alongside one new file.
    /// Empty preserves the creation-only meaning of older saved requests.
    #[serde(default)]
    pub editable_existing: Vec<PathBuf>,
    #[serde(default)]
    pub checks: Vec<LocalCheck>,
    /// Optional reviewed dependency preparation. The worker accepts only its
    /// bounded offline npm-ci form and records it separately from test success.
    #[serde(default)]
    pub preparation: Option<LocalCheck>,
    /// Explicit permission for these checks on the host; never implied by open.
    #[serde(default)]
    pub approve_host_execution: bool,
    /// Separate, explicit consent to send repository context to a loopback
    /// runtime whose process and cloud settings Phonton cannot verify.
    #[serde(default)]
    pub allow_unverified_runtime: bool,
    #[serde(default)]
    pub budget: SearchBudget,
    /// Optional hashes bound to a reviewed plan; every scoped file must match.
    #[serde(default)]
    pub expected_source_hashes: std::collections::BTreeMap<PathBuf, String>,
    /// Captured repository identity from plan review, including check definitions.
    #[serde(default)]
    pub expected_baseline_sha256: Option<String>,
}

/// Exact source evidence supporting a proposed edit scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeEvidence {
    /// Repository-relative source path.
    pub path: PathBuf,
    /// Reason this file was proposed or retained.
    pub reason: String,
    /// Source hash observed during read-only planning.
    pub source_sha256: String,
}

/// Planning evidence that an explicit creation target was absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreationEvidence {
    /// Repository-relative path, absent at plan time.
    pub path: PathBuf,
    /// Why this path appears in the plan; never model-inferred.
    pub reason: String,
}

/// Read-only plan preview; accepting it never implies host-execution approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalPlan {
    /// Proposed bounded request, suitable for CLI or Desktop execution.
    pub request: LocalRunRequest,
    /// Existing Phonton plan contract vocabulary, shared across surfaces.
    pub contract: crate::GoalContract,
    /// Inspectable source selection evidence.
    pub files: Vec<ScopeEvidence>,
    /// Explicit absent target, if creation is part of this plan.
    #[serde(default)]
    pub creation: Option<CreationEvidence>,
    /// Limitations or inferred choices the user should review.
    pub warnings: Vec<String>,
}

/// Calibrated model identity shown with a reviewed plan and required at start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewedModelSelection {
    pub model: String,
    pub digest: String,
    pub runtime_version: String,
    pub endpoint: String,
    pub context_tokens: u32,
    pub output_tokens: u32,
    pub protocol: Option<EditProtocol>,
    /// Hash of the complete saved profile, including probes and calibration.
    pub profile_sha256: String,
}

/// Read-only source plan paired with the selected calibrated model, if any.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewedLocalPlan {
    #[serde(flatten)]
    pub plan: LocalPlan,
    pub model_selection: Option<ReviewedModelSelection>,
}

/// Preparation is recorded separately from a command that tests behavior.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckPurpose {
    /// A command that can supply evidence about the edited behavior.
    #[default]
    Verification,
    /// Dependency setup; a pass never demonstrates task correctness.
    Preparation,
}

/// Actual result of one immutable command on one candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckEvidence {
    pub check: Option<LocalCheck>,
    #[serde(default)]
    pub purpose: CheckPurpose,
    pub status: CheckStatus,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub detail: String,
    pub elapsed_ms: u64,
}

/// A harness-owned action in the bounded candidate search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchAction {
    /// Propose the first change against captured source.
    Initial,
    /// Finish a staged creation by editing the reviewed existing source.
    Continue,
    /// Revise an identity-stable failed candidate.
    Repair,
    /// Return to captured source with evidence about rejected approaches.
    Restart,
    /// Spend no more inference on this search.
    Stop,
}

/// Whether a candidate is complete enough for checks and selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStage {
    /// A complete proposal, including older receipts without a stage field.
    #[default]
    Complete,
    /// Created bytes are saved but the required existing edit has not run.
    CreationPendingEdit,
}

/// Harness-owned search choice; never supplied by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchDecision {
    /// Evidence-driven next action.
    pub action: SearchAction,
    /// Candidate whose exact bytes form a repair's input.
    pub parent_candidate: Option<u32>,
    /// Human-readable evidence supporting this choice.
    pub reason: String,
}

/// State of a separate, bounded model proposal for a baseline restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisStatus {
    /// Inference was reserved and may have been interrupted before a reply.
    Pending,
    /// The proposal is anchored to reviewed source; its target and mechanism are not repeated.
    Accepted,
    /// The reply was invalid, repetitive, or outside the reviewed source scope.
    Rejected,
}

/// Model-authored strategy data, never an edit, permission, or verification result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HypothesisEvidence {
    /// Candidate number this proposal would guide.
    pub candidate_number: u32,
    pub status: HypothesisStatus,
    pub path: Option<String>,
    pub search: Option<String>,
    pub mechanism: Option<String>,
    pub difference: Option<String>,
    pub raw_output: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub elapsed_ms: u64,
    pub detail: String,
    /// Exact bounded source shown while asking for this strategy.
    #[serde(default)]
    pub context: Option<crate::code_context::ContextEvidence>,
}

/// An attempted approach with its identity, measured inference, and evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateEvidence {
    pub number: u32,
    /// Incomplete creation stages cannot be verified, selected, or applied.
    #[serde(default)]
    pub stage: CandidateStage,
    pub approach: String,
    pub directory: PathBuf,
    pub content_sha256: Option<String>,
    pub raw_output: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub elapsed_ms: u64,
    pub checks: Vec<CheckEvidence>,
    pub rejection: Option<String>,
    pub diff: String,
    /// Exact bounded context and admission accounting for this generation.
    #[serde(default)]
    pub context: Option<crate::code_context::ContextEvidence>,
    /// Controller choice and lineage. Absent in older saved receipts.
    #[serde(default)]
    pub decision: Option<SearchDecision>,
}

/// Original Git staging identity, observed separately from source snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitIndexEvidence {
    /// Resolved original index path, including linked-worktree indirection.
    pub path: PathBuf,
    /// None means the index did not exist before checks started.
    pub before_sha256: Option<String>,
    /// Latest observed bytes; consult status to distinguish absent from unreadable.
    pub after_sha256: Option<String>,
    /// Equality evidence at the named stage, not filesystem containment.
    pub status: CheckStatus,
    /// Stage at which the comparison was made.
    pub stage: String,
    /// Exact observation or failure, never an inferred rollback guarantee.
    pub detail: String,
}

/// Origin evidence for inference, separate from the loopback transport address.
/// Old receipts remain unknown rather than inheriting a local-only claim.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOrigin {
    /// No origin observation was saved by an older run.
    #[default]
    Unknown,
    /// The Phonton-started process and exact listener were checked around chat.
    ManagedVerified,
    /// The user explicitly allowed an external loopback service; its routing is unknown.
    ExternalUnverified,
}

/// Persisted after each stage. Interrupted state is visible on reopen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRunReceipt {
    /// Schema 3 adds JS/TS source inclusion; schema 4 adds Python inclusion.
    pub schema: u32,
    pub id: String,
    pub state: String,
    pub request: LocalRunRequest,
    pub profile: ModelProfile,
    /// Point-in-time process provenance, not an authenticated channel.
    #[serde(default)]
    pub runtime_origin: RuntimeOrigin,
    pub hardware: HardwareSnapshot,
    /// Exact resident model used to admit a run below the cold-load reserve.
    /// Rechecked before each generation; absence means ordinary cold-fit admission.
    #[serde(default)]
    pub resident_reuse: Option<ResidentModel>,
    pub baseline_sha256: String,
    pub baseline_checks: Vec<CheckEvidence>,
    pub candidates: Vec<CandidateEvidence>,
    /// Separate restart proposals, including rejected and interrupted ones.
    #[serde(default)]
    pub hypotheses: Vec<HypothesisEvidence>,
    pub selected_candidate: Option<u32>,
    pub checks_used: u32,
    /// Missing runtime counters are unknown; admission reserves max output.
    pub generated_tokens_reserved: u64,
    /// All reserved model attempts, including those stopped before chat transport.
    #[serde(default, alias = "inference_calls_used")]
    pub model_calls_reserved: u32,
    pub elapsed_ms: u64,
    pub known_gaps: Vec<String>,
    /// Execution contract derived from the accepted request; absent in old receipts.
    #[serde(default)]
    pub contract: Option<crate::GoalContract>,
    /// Absent in older receipts; never infer index integrity from source hashes.
    #[serde(default)]
    pub git_index: Option<GitIndexEvidence>,
}

/// Durable identity for a goal that may end before its first full receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRunAttempt {
    /// Attempt record schema version.
    pub schema: u32,
    /// Run identifier shared with the eventual receipt.
    pub id: String,
    /// Goal text accepted for this attempt.
    pub goal: String,
    /// Selected local model at the start of the attempt.
    pub model: String,
    /// Started, ended_before_receipt, or interrupted_before_receipt.
    pub state: String,
    /// Exact failure or cancellation cause, when known.
    pub error: Option<String>,
}

/// A small read-only index entry for finding a saved local goal again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRunSummary {
    /// The durable UUID used to open the full saved receipt.
    pub id: String,
    /// Goal text saved when the attempt was admitted.
    pub goal: String,
    /// Local model selected for the attempt.
    pub model: String,
    /// Filesystem timestamp of the indexed evidence; not a model/runtime clock.
    pub recorded_at_unix_ms: u64,
}

/// Bounded results from saved local goal discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRunList {
    /// Most recently started discoverable attempts first.
    pub runs: Vec<LocalRunSummary>,
    /// The display limit was filled or a fallback scan stopped at a safety bound.
    /// Additional evidence may exist; this does not assert that it does.
    pub limited: bool,
}

/// One source path in a guarded apply batch. Backup and temporary paths are
/// relative to the run directory and repository respectively.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalApplyFile {
    /// Existing scoped source path.
    pub path: PathBuf,
    /// Captured source bytes before this run.
    pub before_sha256: String,
    /// Selected candidate bytes after verification.
    pub after_sha256: String,
    /// Original-byte backup in the run directory.
    pub backup: PathBuf,
    /// Same-parent replacement file, recorded before creation.
    pub temporary: PathBuf,
}

/// Schema-3 or schema-4 journal entry for one previously absent, scoped file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalApplyCreate {
    /// Reviewed creation target in the repository.
    pub path: PathBuf,
    /// Exact selected candidate bytes.
    pub after_sha256: String,
    /// Same-parent staged file, recorded before any project mutation.
    pub temporary: PathBuf,
    /// Retained hard link in run evidence or same-volume Git metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<PathBuf>,
    /// Identity of the staged file, saved before publication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<LocalFileIdentity>,
    /// Publication may have happened; an absent target then needs manual review.
    #[serde(default)]
    pub publication_attempted: bool,
    /// Rollback journal reached the deletion attempt; an absent target can be
    /// recovered without deleting another file.
    #[serde(default)]
    pub rollback_deletion_attempted: bool,
}

/// Platform file identity captured while the run retains a hard-link anchor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalFileIdentity {
    /// `windows_file_id_128` or `unix_dev_inode`.
    pub kind: String,
    /// Windows volume serial number or Unix device number.
    pub device: u64,
    /// Full Windows file ID or Unix inode in lowercase hexadecimal.
    pub file_id: String,
}

/// Durable result of explicitly applying a selected, verified local candidate.
/// Schema 1 records one file; schema 2 records existing changed files; schema 3
/// records one creation target; schema 4 records creation plus existing edits.
/// `prepared` may contain a mix of before/after files after interruption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalApplyReceipt {
    /// Apply record schema version.
    pub schema: u32,
    /// Source run whose selected candidate was reviewed.
    pub run_id: String,
    /// Exact selected candidate number.
    pub candidate_number: u32,
    /// Schema-1 path. Absent from schema-2 records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Schema-1 original-byte hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sha256: Option<String>,
    /// Schema-1 candidate-byte hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sha256: Option<String>,
    /// Schema-2 or schema-4 complete, sorted changed existing source paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<LocalApplyFile>,
    /// Schema-3 or schema-4 explicit creation; absent from older journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_file: Option<LocalApplyCreate>,
    /// Schema-2 baseline tree identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_sha256: Option<String>,
    /// Schema-2 selected candidate tree identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_sha256: Option<String>,
    /// `prepared`, `applied`, `rollback_prepared`, or `rolled_back`.
    pub state: String,
    /// Same-parent restore temporaries, saved before rollback mutates the project.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollback_temporaries: Vec<PathBuf>,
    /// Honest action and recovery detail.
    pub detail: String,
}
