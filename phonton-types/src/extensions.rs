//! Shared extension-system types.
//!
//! Skills, MCP servers, steering rules, and profiles are all represented as
//! extension records so the planner, worker, verifier, CLI, and review
//! surfaces can share one provenance and trust vocabulary.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::TaskClass;

/// Stable string identifier for a Phonton extension record.
///
/// Unlike task and subtask ids, extension ids are human-authored and should
/// stay stable across machines, for example `rust-no-panics` or
/// `github-mcp`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExtensionId(pub String);

impl ExtensionId {
    /// Construct an extension id from a string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the raw id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExtensionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ExtensionId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ExtensionId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// Kind of extension record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionKind {
    /// Persistent project or user instruction.
    Steering,
    /// Text-first reusable instruction bundle.
    Skill,
    /// External tool server using the Model Context Protocol.
    McpServer,
    /// Activation bundle for multiple extension records.
    Profile,
}

impl fmt::Display for ExtensionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ExtensionKind::Steering => "steering",
            ExtensionKind::Skill => "skill",
            ExtensionKind::McpServer => "mcp-server",
            ExtensionKind::Profile => "profile",
        };
        f.write_str(s)
    }
}

/// Where an extension record was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionSource {
    /// Compiled into Phonton.
    BuiltIn,
    /// Loaded from `~/.phonton`.
    UserHome,
    /// Loaded from `<repo>/.phonton`.
    Workspace,
    /// Supplied for the current process, goal, or interactive session.
    Session,
}

impl fmt::Display for ExtensionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ExtensionSource::BuiltIn => "built-in",
            ExtensionSource::UserHome => "user-home",
            ExtensionSource::Workspace => "workspace",
            ExtensionSource::Session => "session",
        };
        f.write_str(s)
    }
}

/// Applicability scope for an extension record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionScope {
    /// May apply anywhere.
    Global,
    /// Applies only inside a workspace root.
    Workspace {
        /// Workspace root if known at load time.
        root: PathBuf,
    },
    /// Applies only to the current session.
    Session,
}

/// Trust class for an extension record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustLevel {
    /// Context-only. Cannot request tool permissions.
    TextOnly,
    /// Can request read-only tool calls.
    ReadOnlyTool,
    /// Can request mutating local tool calls.
    MutatingTool,
    /// Can request networked tool calls.
    NetworkedTool,
}

impl fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TrustLevel::TextOnly => "text-only",
            TrustLevel::ReadOnlyTool => "read-only-tool",
            TrustLevel::MutatingTool => "mutating-tool",
            TrustLevel::NetworkedTool => "networked-tool",
        };
        f.write_str(s)
    }
}

/// Permission an extension may request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// Read files inside the trusted workspace.
    #[serde(alias = "fs.read.workspace")]
    FsReadWorkspace,
    /// Read files outside the trusted workspace.
    #[serde(alias = "fs.read.outside-workspace")]
    FsReadOutsideWorkspace,
    /// Write files inside the trusted workspace.
    #[serde(alias = "fs.write.workspace")]
    FsWriteWorkspace,
    /// Write files outside the trusted workspace.
    #[serde(alias = "fs.write.outside-workspace")]
    FsWriteOutsideWorkspace,
    /// Run a process.
    #[serde(alias = "process.run")]
    ProcessRun,
    /// Make a network request.
    #[serde(alias = "network.request")]
    NetworkRequest,
    /// Mutate git state.
    #[serde(alias = "git.write")]
    GitWrite,
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Permission::FsReadWorkspace => "fs.read.workspace",
            Permission::FsReadOutsideWorkspace => "fs.read.outside-workspace",
            Permission::FsWriteWorkspace => "fs.write.workspace",
            Permission::FsWriteOutsideWorkspace => "fs.write.outside-workspace",
            Permission::ProcessRun => "process.run",
            Permission::NetworkRequest => "network.request",
            Permission::GitWrite => "git.write",
        };
        f.write_str(s)
    }
}

/// Conditions that decide whether an extension applies to a goal/subtask.
///
/// Empty vectors mean "all" for that dimension. Pattern matching is owned by
/// the future loader/resolver crate; this type only carries the data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliesTo {
    /// Path globs or path-like patterns such as `**/*.rs`.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Language labels such as `rust`, `python`, or `typescript`.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Task classes this record applies to.
    #[serde(default)]
    pub task_classes: Vec<TaskClass>,
}

/// Generic manifest common to every extension kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionManifest {
    /// Stable extension id.
    pub id: ExtensionId,
    /// Extension kind.
    pub kind: ExtensionKind,
    /// Human-readable name.
    pub name: String,
    /// Extension version string. Interpreted by the loader, not this crate.
    pub version: String,
    /// Where this record came from.
    pub source: ExtensionSource,
    /// Scope that owns this record.
    pub scope: ExtensionScope,
    /// Trust class.
    pub trust: TrustLevel,
    /// Permissions this record may request.
    #[serde(default)]
    pub permissions: Vec<Permission>,
    /// Applicability rules.
    #[serde(default)]
    pub applies_to: AppliesTo,
    /// Resolver precedence. Higher wins, but safety cannot be weakened.
    #[serde(default)]
    pub precedence: u32,
    /// Optional checksum of the source material.
    #[serde(default)]
    pub checksum: Option<String>,
    /// Whether the record is enabled after config parsing.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// Severity of a steering rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteeringSeverity {
    /// Prompt guidance only.
    Advise,
    /// Visible warning if violated or ambiguous.
    Warn,
    /// Verification should fail when the violation can be detected.
    Fail,
}

impl fmt::Display for SteeringSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SteeringSeverity::Advise => "advise",
            SteeringSeverity::Warn => "warn",
            SteeringSeverity::Fail => "fail",
        };
        f.write_str(s)
    }
}

/// Persistent rule that can steer planning, context, workers, or verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringRule {
    /// Rule id.
    pub id: ExtensionId,
    /// Rule severity.
    pub severity: SteeringSeverity,
    /// Applicability rules.
    #[serde(default)]
    pub applies_to: AppliesTo,
    /// User-facing instruction or constraint.
    pub text: String,
    /// Source scope used for audit output.
    pub source: ExtensionSource,
}

/// Text-first skill definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDefinition {
    /// Skill id.
    pub id: ExtensionId,
    /// Human-readable skill name.
    pub name: String,
    /// Skill version string.
    pub version: String,
    /// Entry file, usually `SKILL.md`.
    pub entry: PathBuf,
    /// Applicability rules.
    #[serde(default)]
    pub applies_to: AppliesTo,
    /// Optional suggested verification commands surfaced in plan/review.
    #[serde(default)]
    pub recommended_verify: Vec<String>,
}

/// Transport used by an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    /// Local process speaking over stdio.
    Stdio {
        /// Program to launch.
        command: String,
        /// Arguments passed to the program.
        #[serde(default)]
        args: Vec<String>,
    },
    /// HTTP endpoint.
    Http {
        /// Endpoint URL.
        url: String,
    },
}

/// MCP server declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerDefinition {
    /// Server id.
    pub id: ExtensionId,
    /// Human-readable server name.
    pub name: String,
    /// Source scope used for audit output and precedence resolution.
    pub source: ExtensionSource,
    /// Transport config.
    pub transport: McpTransport,
    /// Trust class.
    pub trust: TrustLevel,
    /// Permissions this server can request.
    #[serde(default)]
    pub permissions: Vec<Permission>,
    /// Applicability rules.
    #[serde(default)]
    pub applies_to: AppliesTo,
    /// Environment variables the server needs, stored as names only.
    #[serde(default)]
    pub env: Vec<String>,
    /// Whether the server is enabled after parsing.
    #[serde(default)]
    pub enabled: bool,
}

/// Activation bundle for extension records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDefinition {
    /// Profile id.
    pub id: ExtensionId,
    /// Human-readable name.
    pub name: String,
    /// Source scope used for audit output and precedence resolution.
    pub source: ExtensionSource,
    /// Extension ids activated by this profile.
    #[serde(default)]
    pub activates: Vec<ExtensionId>,
    /// Optional budget token ceiling.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Optional budget USD ceiling in micro-dollars.
    #[serde(default)]
    pub max_usd_micros: Option<u64>,
}

/// A conflict discovered while resolving extension records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionConflict {
    /// Extension id involved in the conflict.
    pub id: ExtensionId,
    /// Lower-precedence source.
    pub lower_source: ExtensionSource,
    /// Higher-precedence source.
    pub higher_source: ExtensionSource,
    /// Human-readable detail.
    pub detail: String,
}

/// Review-safe record of an extension that influenced a task or subtask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionInfluence {
    /// Extension id.
    pub id: ExtensionId,
    /// Extension kind.
    pub kind: ExtensionKind,
    /// Source scope.
    pub source: ExtensionSource,
    /// Version string if known.
    #[serde(default)]
    pub version: Option<String>,
    /// Checksum if known.
    #[serde(default)]
    pub checksum: Option<String>,
    /// What the extension did, for review and flight-log output.
    pub action: ExtensionAction,
}

/// Review-safe description of how an extension affected a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionAction {
    /// Manifest was loaded.
    Loaded,
    /// Manifest was skipped.
    Skipped {
        /// Why it was skipped.
        reason: String,
    },
    /// Steering rule was applied.
    SteeringApplied {
        /// Severity at application time.
        severity: SteeringSeverity,
    },
    /// Skill context was injected.
    SkillApplied,
    /// MCP server was available to the run.
    McpServerAvailable,
    /// MCP tool was requested.
    McpToolRequested {
        /// Tool name reported by the server.
        tool_name: String,
        /// Permissions requested by the tool call.
        permissions: Vec<Permission>,
    },
    /// MCP tool request was approved.
    McpToolApproved {
        /// Tool name reported by the server.
        tool_name: String,
    },
    /// MCP tool request was denied.
    McpToolDenied {
        /// Tool name reported by the server.
        tool_name: String,
        /// Why the request was denied.
        reason: String,
    },
    /// MCP tool completed.
    McpToolCompleted {
        /// Tool name reported by the server.
        tool_name: String,
        /// Whether the call succeeded.
        success: bool,
    },
    /// Resolver found a conflict.
    Conflict {
        /// Conflict detail.
        detail: String,
    },
}
