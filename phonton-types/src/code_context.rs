//! Exact source locations and inspectable local context selection.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A parsed symbol's inclusive one-based source line range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolSpan {
    /// Parsed source identifier, not a generated summary.
    pub name: String,
    /// First included source line, starting at one.
    pub start_line: usize,
    /// Last included source line.
    pub end_line: usize,
}

/// Exact captured source sent to a model, without invented summaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceExcerpt {
    /// Repository-relative path inside the approved scope.
    pub path: PathBuf,
    /// First included source line, starting at one.
    pub start_line: usize,
    /// Last included source line.
    pub end_line: usize,
    /// Unmodified source bytes decoded as UTF-8.
    pub text: String,
    /// Hash of the complete captured file from which this excerpt came.
    pub source_sha256: String,
    /// Deterministic selection explanation.
    pub reason: String,
}

/// Context admission evidence. Byte bounds are not measured token counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextEvidence {
    /// Exact source sections included in this model prompt.
    pub excerpts: Vec<SourceExcerpt>,
    /// Bytes inspected within approved source files.
    pub source_bytes_scanned: usize,
    /// Source bytes actually included in the selected excerpts.
    pub selected_source_bytes: usize,
    /// Combined system and user prompt size in UTF-8 bytes.
    pub prompt_bytes: usize,
    /// Serialized edit-constraint schema size in UTF-8 bytes.
    pub constraint_bytes: usize,
    /// Calibrated model context token ceiling used for admission.
    pub context_capacity: u32,
    /// Maximum generation tokens reserved before inference.
    pub output_tokens_reserved: u32,
    /// True when some captured source was omitted from this prompt.
    pub omitted_source: bool,
    /// Measured retrieval and assembly duration.
    pub elapsed_ms: u64,
    /// Exact bounded prior-candidate/check feedback included in this prompt.
    #[serde(default)]
    pub feedback: String,
    /// True if feedback was reduced to admit source within the context ceiling.
    #[serde(default)]
    pub feedback_omitted: bool,
}
