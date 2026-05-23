//! Local extension discovery and resolution.
//!
//! This crate only parses local Phonton extension config. It does not start
//! MCP servers, execute skills, make network calls, or grant permissions.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use phonton_types::{
    AppliesTo, ExtensionConflict, ExtensionId, ExtensionKind, ExtensionManifest, ExtensionScope,
    ExtensionSource, McpServerDefinition, McpTransport, Permission, ProfileDefinition,
    SkillDefinition, SteeringRule, SteeringSeverity, TaskClass, TrustLevel,
};
use serde::Deserialize;

/// User-level extension directory under the home directory.
pub const USER_EXTENSION_DIR: &str = ".phonton";

/// Workspace-level extension directory.
pub const WORKSPACE_EXTENSION_DIR: &str = ".phonton";

/// Options for local extension loading.
#[derive(Debug, Clone, Default)]
pub struct ExtensionLoadOptions {
    /// User config directory, normally `~/.phonton`.
    pub user_dir: Option<PathBuf>,
    /// Workspace root whose `.phonton` directory should be loaded.
    pub workspace_root: Option<PathBuf>,
}

impl ExtensionLoadOptions {
    /// Build options for a workspace, including the default user directory
    /// when a home directory can be resolved.
    pub fn for_workspace(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            user_dir: default_user_dir(),
            workspace_root: Some(workspace_root.into()),
        }
    }

    /// Use an explicit user directory. Useful for tests and scripted runs.
    pub fn with_user_dir(mut self, user_dir: impl Into<PathBuf>) -> Self {
        self.user_dir = Some(user_dir.into());
        self
    }

    /// Disable user-level extension loading.
    pub fn without_user_dir(mut self) -> Self {
        self.user_dir = None;
        self
    }
}

fn default_user_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(USER_EXTENSION_DIR))
}

/// Result of local extension loading and resolution.
#[derive(Debug, Clone, Default)]
pub struct ExtensionSet {
    /// All loaded manifests, including disabled and overridden records.
    pub manifests: Vec<ExtensionManifest>,
    /// Active steering rules after precedence resolution.
    pub steering: Vec<SteeringRule>,
    /// Active skills after precedence resolution.
    pub skills: Vec<LoadedSkill>,
    /// Active MCP server definitions after precedence resolution.
    pub mcp_servers: Vec<McpServerDefinition>,
    /// Active profiles after precedence resolution.
    pub profiles: Vec<ProfileDefinition>,
    /// Conflicts discovered during resolution.
    pub conflicts: Vec<ExtensionConflict>,
    /// Non-fatal diagnostics from loading config files.
    pub diagnostics: Vec<ExtensionDiagnostic>,
}

impl ExtensionSet {
    /// True when any diagnostic is an error.
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error)
    }

    /// Render text-only extension guidance that is safe to inject into
    /// worker prompts. MCP definitions are intentionally excluded here:
    /// discovering a server does not mean its tools have been approved or
    /// started for the run.
    pub fn render_prompt_preamble(&self) -> String {
        let mut sections = Vec::new();

        if !self.steering.is_empty() {
            let mut steering = String::from("# Phonton steering\n");
            for rule in &self.steering {
                steering.push_str(&format!(
                    "- [{}:{}] {}\n",
                    rule.source,
                    rule.severity,
                    rule.text.trim()
                ));
            }
            sections.push(steering.trim_end().to_string());
        }

        let active_skills: Vec<_> = self
            .skills
            .iter()
            .filter(|skill| !skill.content.trim().is_empty())
            .collect();
        if !active_skills.is_empty() {
            let mut skills = String::from("# Phonton skills\n");
            for skill in active_skills {
                skills.push_str(&format!(
                    "## {} ({}@{}, {})\n{}\n\n",
                    skill.definition.name,
                    skill.definition.id,
                    skill.definition.version,
                    skill.manifest.source,
                    skill.content.trim()
                ));
            }
            sections.push(skills.trim_end().to_string());
        }

        sections.join("\n\n")
    }
}

/// Skill content loaded from disk with its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    /// Skill definition parsed from `skill.toml`.
    pub definition: SkillDefinition,
    /// Common manifest fields for resolution and review metadata.
    pub manifest: ExtensionManifest,
    /// Entry file content. Empty when the file could not be read.
    pub content: String,
}

/// Diagnostic emitted while loading extension config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionDiagnostic {
    /// Severity.
    pub severity: DiagnosticSeverity,
    /// Source scope being loaded.
    pub source: ExtensionSource,
    /// File path involved, if known.
    pub path: Option<PathBuf>,
    /// Human-readable message.
    pub message: String,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    /// Non-fatal warning.
    Warn,
    /// Invalid config or unreadable source material.
    Error,
}

/// Load and resolve local extensions from the configured user/workspace
/// directories.
pub fn load_extensions(options: &ExtensionLoadOptions) -> ExtensionSet {
    let mut set = ExtensionSet::default();

    if let Some(user_dir) = &options.user_dir {
        load_source(
            &mut set,
            user_dir,
            ExtensionSource::UserHome,
            ExtensionScope::Global,
            10,
        );
    }

    if let Some(workspace_root) = &options.workspace_root {
        load_source(
            &mut set,
            &workspace_root.join(WORKSPACE_EXTENSION_DIR),
            ExtensionSource::Workspace,
            ExtensionScope::Workspace {
                root: workspace_root.clone(),
            },
            20,
        );
    }

    resolve(set)
}

fn load_source(
    set: &mut ExtensionSet,
    dir: &Path,
    source: ExtensionSource,
    scope: ExtensionScope,
    precedence: u32,
) {
    if !dir.exists() {
        return;
    }
    load_steering(
        set,
        &dir.join("steering.toml"),
        source,
        scope.clone(),
        precedence,
    );
    load_skills(set, &dir.join("skills"), source, scope.clone(), precedence);
    load_mcp(
        set,
        &dir.join("mcp.toml"),
        source,
        scope.clone(),
        precedence,
    );
    load_profiles(set, &dir.join("profiles.toml"), source, scope, precedence);
}

fn load_steering(
    set: &mut ExtensionSet,
    path: &Path,
    source: ExtensionSource,
    scope: ExtensionScope,
    precedence: u32,
) {
    let Some(file) = read_toml::<SteeringFile>(set, path, source) else {
        return;
    };

    for rule in file.rules {
        let applies_to = rule.applies_to();
        let id = ExtensionId::new(rule.id);
        let text = rule.text;
        let manifest = ExtensionManifest {
            id: id.clone(),
            kind: ExtensionKind::Steering,
            name: rule.name.unwrap_or_else(|| id.to_string()),
            version: "0.1.0".into(),
            source,
            scope: scope.clone(),
            trust: TrustLevel::TextOnly,
            permissions: Vec::new(),
            applies_to: applies_to.clone(),
            precedence,
            checksum: None,
            enabled: rule.enabled,
        };
        set.manifests.push(manifest);
        set.steering.push(SteeringRule {
            id,
            severity: rule.severity,
            applies_to,
            text,
            source,
        });
    }
}

fn load_skills(
    set: &mut ExtensionSet,
    dir: &Path,
    source: ExtensionSource,
    scope: ExtensionScope,
    precedence: u32,
) {
    if !dir.exists() {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            diagnostic(
                set,
                DiagnosticSeverity::Error,
                source,
                Some(dir.to_path_buf()),
                format!("failed to read skills directory: {e}"),
            );
            return;
        }
    };

    for entry in entries.flatten() {
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }
        let manifest_path = skill_dir.join("skill.toml");
        let Some(file) = read_toml::<SkillFile>(set, &manifest_path, source) else {
            continue;
        };
        let raw = file.skill;
        let applies_to = raw.applies_to();
        let id = ExtensionId::new(raw.id);
        let entry = raw.entry.unwrap_or_else(default_skill_entry);
        let entry_path = skill_dir.join(&entry);
        let mut enabled = raw.enabled;
        let content = match std::fs::read_to_string(&entry_path) {
            Ok(content) => content,
            Err(e) => {
                diagnostic(
                    set,
                    DiagnosticSeverity::Error,
                    source,
                    Some(entry_path.clone()),
                    format!("failed to read skill entry: {e}"),
                );
                enabled = false;
                String::new()
            }
        };
        let trust = raw.trust.unwrap_or(TrustLevel::TextOnly);
        let permissions = raw.permissions;
        if trust == TrustLevel::TextOnly && !permissions.is_empty() {
            diagnostic(
                set,
                DiagnosticSeverity::Warn,
                source,
                Some(manifest_path.clone()),
                format!("text-only skill {id} declares permissions; permissions ignored by policy"),
            );
        }
        let definition = SkillDefinition {
            id: id.clone(),
            name: raw.name.unwrap_or_else(|| id.to_string()),
            version: raw.version.unwrap_or_else(default_version),
            entry: entry_path,
            applies_to: applies_to.clone(),
            recommended_verify: raw.recommended_verify,
        };
        let manifest = ExtensionManifest {
            id,
            kind: ExtensionKind::Skill,
            name: definition.name.clone(),
            version: definition.version.clone(),
            source,
            scope: scope.clone(),
            trust,
            permissions,
            applies_to,
            precedence,
            checksum: None,
            enabled,
        };
        set.manifests.push(manifest.clone());
        set.skills.push(LoadedSkill {
            definition,
            manifest,
            content,
        });
    }
}

fn load_mcp(
    set: &mut ExtensionSet,
    path: &Path,
    source: ExtensionSource,
    scope: ExtensionScope,
    precedence: u32,
) {
    let Some(file) = read_toml::<McpFile>(set, path, source) else {
        return;
    };

    for server in file.servers {
        let applies_to = server.applies_to();
        let transport = match server.transport(path, set, source) {
            Some(transport) => transport,
            None => continue,
        };
        let id = ExtensionId::new(server.id);
        let permissions = server.permissions;
        let trust = server.trust.unwrap_or_else(|| infer_trust(&permissions));
        let definition = McpServerDefinition {
            id: id.clone(),
            name: server.name.unwrap_or_else(|| id.to_string()),
            source,
            transport,
            trust,
            permissions: permissions.clone(),
            applies_to: applies_to.clone(),
            env: server.env,
            enabled: server.enabled,
        };
        let manifest = ExtensionManifest {
            id,
            kind: ExtensionKind::McpServer,
            name: definition.name.clone(),
            version: "0.1.0".into(),
            source,
            scope: scope.clone(),
            trust,
            permissions,
            applies_to,
            precedence,
            checksum: None,
            enabled: definition.enabled,
        };
        set.manifests.push(manifest);
        set.mcp_servers.push(definition);
    }
}

fn load_profiles(
    set: &mut ExtensionSet,
    path: &Path,
    source: ExtensionSource,
    scope: ExtensionScope,
    precedence: u32,
) {
    let Some(file) = read_toml::<ProfileFile>(set, path, source) else {
        return;
    };

    for profile in file.profiles {
        let id = ExtensionId::new(profile.id);
        let definition = ProfileDefinition {
            id: id.clone(),
            name: profile.name.unwrap_or_else(|| id.to_string()),
            source,
            activates: profile
                .activates
                .into_iter()
                .map(ExtensionId::new)
                .collect(),
            max_tokens: profile.max_tokens,
            max_usd_micros: profile
                .max_usd_micros
                .or_else(|| profile.max_usd_cents.map(|c| c.saturating_mul(10_000))),
        };
        let manifest = ExtensionManifest {
            id,
            kind: ExtensionKind::Profile,
            name: definition.name.clone(),
            version: "0.1.0".into(),
            source,
            scope: scope.clone(),
            trust: TrustLevel::TextOnly,
            permissions: Vec::new(),
            applies_to: AppliesTo::default(),
            precedence,
            checksum: None,
            enabled: profile.enabled,
        };
        set.manifests.push(manifest);
        set.profiles.push(definition);
    }
}

fn read_toml<T>(set: &mut ExtensionSet, path: &Path, source: ExtensionSource) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return None;
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) => {
            diagnostic(
                set,
                DiagnosticSeverity::Error,
                source,
                Some(path.to_path_buf()),
                format!("failed to read config: {e}"),
            );
            return None;
        }
    };
    match toml::from_str(&raw) {
        Ok(parsed) => Some(parsed),
        Err(e) => {
            diagnostic(
                set,
                DiagnosticSeverity::Error,
                source,
                Some(path.to_path_buf()),
                format!("failed to parse config: {e}"),
            );
            None
        }
    }
}

fn resolve(mut set: ExtensionSet) -> ExtensionSet {
    let mut winners: HashMap<(ExtensionKind, ExtensionId), usize> = HashMap::new();
    for idx in 0..set.manifests.len() {
        if !set.manifests[idx].enabled {
            continue;
        }
        let key = (set.manifests[idx].kind, set.manifests[idx].id.clone());
        if let Some(previous) = winners.get(&key).copied() {
            let previous_precedence = set.manifests[previous].precedence;
            let current_precedence = set.manifests[idx].precedence;
            let (winner, loser) = if current_precedence >= previous_precedence {
                winners.insert(key.clone(), idx);
                (idx, previous)
            } else {
                (previous, idx)
            };
            let conflict = ExtensionConflict {
                id: set.manifests[winner].id.clone(),
                lower_source: set.manifests[loser].source,
                higher_source: set.manifests[winner].source,
                detail: format!(
                    "{} overrides {} for {}",
                    set.manifests[winner].source,
                    set.manifests[loser].source,
                    set.manifests[winner].kind
                ),
            };
            set.conflicts.push(conflict);
            set.manifests[loser].enabled = false;
        } else {
            winners.insert(key, idx);
        }
    }

    let active: HashSet<(ExtensionKind, ExtensionId, ExtensionSource)> = set
        .manifests
        .iter()
        .filter(|manifest| manifest.enabled)
        .map(|manifest| (manifest.kind, manifest.id.clone(), manifest.source))
        .collect();

    set.steering
        .retain(|rule| active.contains(&(ExtensionKind::Steering, rule.id.clone(), rule.source)));
    set.skills.retain(|skill| {
        active.contains(&(
            ExtensionKind::Skill,
            skill.definition.id.clone(),
            skill.manifest.source,
        ))
    });
    set.mcp_servers.retain(|server| {
        active.contains(&(ExtensionKind::McpServer, server.id.clone(), server.source))
    });
    set.profiles.retain(|profile| {
        active.contains(&(ExtensionKind::Profile, profile.id.clone(), profile.source))
    });

    set
}

fn infer_trust(permissions: &[Permission]) -> TrustLevel {
    if permissions
        .iter()
        .any(|p| matches!(p, Permission::NetworkRequest))
    {
        TrustLevel::NetworkedTool
    } else if permissions.iter().any(|p| {
        matches!(
            p,
            Permission::FsWriteWorkspace
                | Permission::FsWriteOutsideWorkspace
                | Permission::ProcessRun
                | Permission::GitWrite
        )
    }) {
        TrustLevel::MutatingTool
    } else {
        TrustLevel::ReadOnlyTool
    }
}

fn diagnostic(
    set: &mut ExtensionSet,
    severity: DiagnosticSeverity,
    source: ExtensionSource,
    path: Option<PathBuf>,
    message: String,
) {
    set.diagnostics.push(ExtensionDiagnostic {
        severity,
        source,
        path,
        message,
    });
}

fn default_version() -> String {
    "0.1.0".into()
}

fn default_skill_entry() -> PathBuf {
    PathBuf::from("SKILL.md")
}

#[derive(Debug, Deserialize)]
struct SteeringFile {
    #[serde(default)]
    rules: Vec<RawSteeringRule>,
}

#[derive(Debug, Deserialize)]
struct RawSteeringRule {
    id: String,
    #[serde(default)]
    name: Option<String>,
    severity: SteeringSeverity,
    text: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    applies_to: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    task_classes: Vec<TaskClass>,
}

impl RawSteeringRule {
    fn applies_to(&self) -> AppliesTo {
        applies_to(
            &self.applies_to,
            &self.paths,
            &self.languages,
            &self.task_classes,
        )
    }
}

#[derive(Debug, Deserialize)]
struct SkillFile {
    skill: RawSkill,
}

#[derive(Debug, Deserialize)]
struct RawSkill {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    entry: Option<PathBuf>,
    #[serde(default)]
    trust: Option<TrustLevel>,
    #[serde(default)]
    permissions: Vec<Permission>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    applies_to: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    task_classes: Vec<TaskClass>,
    #[serde(default)]
    recommended_verify: Vec<String>,
}

impl RawSkill {
    fn applies_to(&self) -> AppliesTo {
        applies_to(
            &self.applies_to,
            &self.paths,
            &self.languages,
            &self.task_classes,
        )
    }
}

#[derive(Debug, Deserialize)]
struct McpFile {
    #[serde(default)]
    servers: Vec<RawMcpServer>,
}

#[derive(Debug, Deserialize)]
struct RawMcpServer {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    trust: Option<TrustLevel>,
    #[serde(default)]
    permissions: Vec<Permission>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    applies_to: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    task_classes: Vec<TaskClass>,
    #[serde(default)]
    env: Vec<String>,
}

impl RawMcpServer {
    fn applies_to(&self) -> AppliesTo {
        applies_to(
            &self.applies_to,
            &self.paths,
            &self.languages,
            &self.task_classes,
        )
    }

    fn transport(
        &self,
        path: &Path,
        set: &mut ExtensionSet,
        source: ExtensionSource,
    ) -> Option<McpTransport> {
        match (&self.command, &self.url) {
            (Some(command), None) => Some(McpTransport::Stdio {
                command: command.clone(),
                args: self.args.clone(),
            }),
            (None, Some(url)) => Some(McpTransport::Http { url: url.clone() }),
            (Some(_), Some(_)) => {
                diagnostic(
                    set,
                    DiagnosticSeverity::Error,
                    source,
                    Some(path.to_path_buf()),
                    format!("mcp server {} must use command or url, not both", self.id),
                );
                None
            }
            (None, None) => {
                diagnostic(
                    set,
                    DiagnosticSeverity::Error,
                    source,
                    Some(path.to_path_buf()),
                    format!("mcp server {} is missing command or url", self.id),
                );
                None
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    profiles: Vec<RawProfile>,
}

#[derive(Debug, Deserialize)]
struct RawProfile {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    activates: Vec<String>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    max_usd_micros: Option<u64>,
    #[serde(default)]
    max_usd_cents: Option<u64>,
}

fn applies_to(
    shorthand_paths: &[String],
    paths: &[String],
    languages: &[String],
    task_classes: &[TaskClass],
) -> AppliesTo {
    let mut merged_paths = shorthand_paths.to_vec();
    merged_paths.extend(paths.iter().cloned());
    AppliesTo {
        paths: merged_paths,
        languages: languages.to_vec(),
        task_classes: task_classes.to_vec(),
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write test file");
    }

    #[test]
    fn loads_workspace_steering_and_skill() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            &root.join(".phonton/steering.toml"),
            r#"
[[rules]]
id = "rust-no-panics"
severity = "fail"
applies_to = ["**/*.rs"]
text = "No panics in library code."
"#,
        );
        write(
            &root.join(".phonton/skills/rust-errors/skill.toml"),
            r#"
[skill]
id = "rust-errors"
name = "Rust errors"
version = "0.1.0"
entry = "SKILL.md"
trust = "text-only"
applies_to = ["**/*.rs"]
"#,
        );
        write(
            &root.join(".phonton/skills/rust-errors/SKILL.md"),
            "Prefer thiserror in libraries.",
        );

        let options = ExtensionLoadOptions::for_workspace(root).without_user_dir();
        let set = load_extensions(&options);

        assert!(!set.has_errors());
        assert_eq!(set.steering.len(), 1);
        assert_eq!(set.skills.len(), 1);
        assert_eq!(set.skills[0].content, "Prefer thiserror in libraries.");
        assert_eq!(set.manifests.len(), 2);
    }

    #[test]
    fn render_prompt_preamble_includes_only_text_extensions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            &root.join(".phonton/steering.toml"),
            r#"
[[rules]]
id = "reviewable"
severity = "warn"
text = "Keep diffs reviewable."
"#,
        );
        write(
            &root.join(".phonton/skills/review/skill.toml"),
            r#"
[skill]
id = "review"
name = "Review skill"
entry = "SKILL.md"
"#,
        );
        write(
            &root.join(".phonton/skills/review/SKILL.md"),
            "Prefer small, verified changes.",
        );
        write(
            &root.join(".phonton/mcp.toml"),
            r#"
[[servers]]
id = "docs"
command = "docs-mcp"
permissions = ["network.request"]
"#,
        );

        let options = ExtensionLoadOptions::for_workspace(root).without_user_dir();
        let set = load_extensions(&options);
        let preamble = set.render_prompt_preamble();

        assert!(preamble.contains("# Phonton steering"));
        assert!(preamble.contains("Keep diffs reviewable."));
        assert!(preamble.contains("# Phonton skills"));
        assert!(preamble.contains("Prefer small, verified changes."));
        assert!(!preamble.contains("docs-mcp"));
    }

    #[test]
    fn unreadable_skill_entry_is_not_active() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            &root.join(".phonton/skills/missing/skill.toml"),
            r#"
[skill]
id = "missing"
entry = "MISSING.md"
"#,
        );

        let options = ExtensionLoadOptions::for_workspace(root).without_user_dir();
        let set = load_extensions(&options);

        assert!(set.has_errors());
        assert!(set.skills.is_empty());
        assert_eq!(set.manifests.len(), 1);
        assert!(!set.manifests[0].enabled);
        assert!(set.render_prompt_preamble().is_empty());
    }

    #[test]
    fn workspace_overrides_user_record_with_same_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let user = tmp.path().join("user");
        let root = tmp.path().join("workspace");
        write(
            &user.join("steering.toml"),
            r#"
[[rules]]
id = "style"
severity = "advise"
text = "User style."
"#,
        );
        write(
            &root.join(".phonton/steering.toml"),
            r#"
[[rules]]
id = "style"
severity = "warn"
text = "Workspace style."
"#,
        );

        let options = ExtensionLoadOptions::for_workspace(&root).with_user_dir(&user);
        let set = load_extensions(&options);

        assert_eq!(set.steering.len(), 1);
        assert_eq!(set.steering[0].source, ExtensionSource::Workspace);
        assert_eq!(set.conflicts.len(), 1);
        assert_eq!(
            set.manifests.iter().filter(|m| m.enabled).count(),
            1,
            "only the higher-precedence record should stay active"
        );
    }

    #[test]
    fn disabled_mcp_server_is_not_active() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            &root.join(".phonton/mcp.toml"),
            r#"
[[servers]]
id = "github"
name = "GitHub"
enabled = false
command = "github-mcp-server"
args = ["stdio"]
permissions = ["network.request"]
"#,
        );

        let options = ExtensionLoadOptions::for_workspace(root).without_user_dir();
        let set = load_extensions(&options);

        assert!(!set.has_errors());
        assert!(set.mcp_servers.is_empty());
        assert_eq!(set.manifests.len(), 1);
        assert!(!set.manifests[0].enabled);
    }

    #[test]
    fn malformed_mcp_server_reports_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            &root.join(".phonton/mcp.toml"),
            r#"
[[servers]]
id = "bad"
enabled = true
"#,
        );

        let options = ExtensionLoadOptions::for_workspace(root).without_user_dir();
        let set = load_extensions(&options);

        assert!(set.has_errors());
        assert!(set.mcp_servers.is_empty());
    }
}
