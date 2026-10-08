//! Execution authority for verification, separate from model-generated content.
use serde::{Deserialize, Serialize};

/// Permission captured by a trusted caller before project checks run.
/// Opening a repository or receiving a model response never grants host access.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationExecution {
    /// Refuse execution unless a filesystem/network isolation backend exists.
    #[default]
    RequireIsolation,
    /// Explicit permission to execute project code with host access.
    /// Process supervision is still applied; this is not containment.
    HostApproved,
}
