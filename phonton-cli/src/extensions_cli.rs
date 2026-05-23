//! `phonton extensions` inspection commands.
//!
//! These commands are read-only. They load the same local extension set the
//! runtime uses, but never start MCP servers or execute skills.

use std::path::Path;

use anyhow::{anyhow, Result};
use phonton_extensions::{
    load_extensions, DiagnosticSeverity, ExtensionDiagnostic, ExtensionLoadOptions, ExtensionSet,
};
use phonton_types::{
    ExtensionConflict, ExtensionKind, ExtensionManifest, ExtensionSource, McpServerDefinition,
    McpTransport, Permission, ProfileDefinition, SteeringRule,
};
use serde::Serialize;

use crate::trust;

#[derive(Debug, Clone, Copy, Default)]
struct Options {
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    All,
    Skills,
    Steering,
    Mcp,
    Profiles,
}

impl View {
    fn matches(self, kind: ExtensionKind) -> bool {
        match self {
            View::All => true,
            View::Skills => kind == ExtensionKind::Skill,
            View::Steering => kind == ExtensionKind::Steering,
            View::Mcp => kind == ExtensionKind::McpServer,
            View::Profiles => kind == ExtensionKind::Profile,
        }
    }

    fn title(self) -> &'static str {
        match self {
            View::All => "Extensions",
            View::Skills => "Skills",
            View::Steering => "Steering",
            View::Mcp => "MCP servers",
            View::Profiles => "Profiles",
        }
    }
}

pub async fn run(workspace: &Path, args: &[String]) -> Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        print_usage();
        return Ok(0);
    };
    if matches!(command, "-h" | "--help" | "help")
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        print_usage();
        return Ok(0);
    }

    match command {
        "list" => list(workspace, &args[1..], View::All),
        "doctor" => doctor(workspace, &args[1..]),
        "skills" => list(workspace, &args[1..], View::Skills),
        "steering" => list(workspace, &args[1..], View::Steering),
        "mcp" => list(workspace, &args[1..], View::Mcp),
        "profiles" => list(workspace, &args[1..], View::Profiles),
        other => {
            eprintln!("phonton extensions: unknown subcommand `{other}`");
            print_usage();
            Ok(2)
        }
    }
}

pub async fn run_skills(workspace: &Path, args: &[String]) -> Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        return list(workspace, args, View::Skills);
    };
    match command {
        "list" => list(workspace, &args[1..], View::Skills),
        "-h" | "--help" | "help" => {
            println!("usage: phonton skills list [--json]");
            Ok(0)
        }
        other => {
            eprintln!("phonton skills: unknown subcommand `{other}`");
            Ok(2)
        }
    }
}

pub async fn run_steering(workspace: &Path, args: &[String]) -> Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        return list(workspace, args, View::Steering);
    };
    match command {
        "list" => list(workspace, &args[1..], View::Steering),
        "-h" | "--help" | "help" => {
            println!("usage: phonton steering list [--json]");
            Ok(0)
        }
        other => {
            eprintln!("phonton steering: unknown subcommand `{other}`");
            Ok(2)
        }
    }
}

fn list(workspace: &Path, args: &[String], view: View) -> Result<i32> {
    let opts = parse_options(args)?;
    let set = load_extensions(&ExtensionLoadOptions::for_workspace(workspace));
    let report = build_inventory_report(workspace, &set, view);
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_inventory_report(&report);
    }
    Ok(0)
}

fn doctor(workspace: &Path, args: &[String]) -> Result<i32> {
    let opts = parse_options(args)?;
    let set = load_extensions(&ExtensionLoadOptions::for_workspace(workspace));
    let report = build_doctor_report(workspace, &set);
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_doctor_report(&report);
    }
    Ok(if report.has_failures { 1 } else { 0 })
}

fn parse_options(args: &[String]) -> Result<Options> {
    let mut opts = Options::default();
    for arg in args {
        match arg.as_str() {
            "--json" => opts.json = true,
            other if other.starts_with('-') => return Err(anyhow!("unknown option `{other}`")),
            other => return Err(anyhow!("unexpected argument `{other}`")),
        }
    }
    Ok(opts)
}

#[derive(Debug, Serialize)]
struct InventoryReport {
    workspace: String,
    view: String,
    counts: Counts,
    extensions: Vec<ExtensionRow>,
    steering: Vec<SteeringRow>,
    skills: Vec<SkillRow>,
    mcp_servers: Vec<McpRow>,
    profiles: Vec<ProfileRow>,
    conflicts: Vec<ConflictRow>,
    diagnostics: Vec<DiagnosticRow>,
}

#[derive(Debug, Default, Serialize)]
struct Counts {
    manifests: usize,
    active: usize,
    disabled: usize,
    diagnostics: usize,
    conflicts: usize,
}

#[derive(Debug, Serialize)]
struct ExtensionRow {
    id: String,
    kind: String,
    name: String,
    version: String,
    source: String,
    status: String,
    trust: String,
    permissions: Vec<String>,
    applies_to: AppliesRow,
}

#[derive(Debug, Serialize)]
struct SteeringRow {
    id: String,
    source: String,
    severity: String,
    text: String,
    applies_to: AppliesRow,
}

#[derive(Debug, Serialize)]
struct SkillRow {
    id: String,
    name: String,
    version: String,
    source: String,
    entry: String,
    content_loaded: bool,
    recommended_verify: Vec<String>,
    applies_to: AppliesRow,
}

#[derive(Debug, Serialize)]
struct McpRow {
    id: String,
    name: String,
    source: String,
    enabled: bool,
    transport: String,
    trust: String,
    permissions: Vec<String>,
    env: Vec<EnvRow>,
    approval_required: bool,
    workspace_trust_required: bool,
}

#[derive(Debug, Serialize)]
struct EnvRow {
    name: String,
    present: bool,
}

#[derive(Debug, Serialize)]
struct ProfileRow {
    id: String,
    name: String,
    source: String,
    activates: Vec<String>,
    max_tokens: Option<u64>,
    max_usd_micros: Option<u64>,
}

#[derive(Debug, Serialize)]
struct AppliesRow {
    paths: Vec<String>,
    languages: Vec<String>,
    task_classes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ConflictRow {
    id: String,
    lower_source: String,
    higher_source: String,
    detail: String,
}

#[derive(Debug, Serialize)]
struct DiagnosticRow {
    severity: String,
    source: String,
    path: Option<String>,
    message: String,
}

fn build_inventory_report(workspace: &Path, set: &ExtensionSet, view: View) -> InventoryReport {
    let manifests: Vec<ExtensionRow> = set
        .manifests
        .iter()
        .filter(|manifest| view.matches(manifest.kind))
        .map(extension_row)
        .collect();
    let active = manifests
        .iter()
        .filter(|row| row.status == "active")
        .count();
    let disabled = manifests.len().saturating_sub(active);

    InventoryReport {
        workspace: workspace.display().to_string(),
        view: view.title().to_string(),
        counts: Counts {
            manifests: manifests.len(),
            active,
            disabled,
            diagnostics: set.diagnostics.len(),
            conflicts: set.conflicts.len(),
        },
        extensions: manifests,
        steering: if matches!(view, View::All | View::Steering) {
            set.steering.iter().map(steering_row).collect()
        } else {
            Vec::new()
        },
        skills: if matches!(view, View::All | View::Skills) {
            set.skills
                .iter()
                .map(|skill| SkillRow {
                    id: skill.definition.id.to_string(),
                    name: skill.definition.name.clone(),
                    version: skill.definition.version.clone(),
                    source: skill.manifest.source.to_string(),
                    entry: skill.definition.entry.display().to_string(),
                    content_loaded: !skill.content.trim().is_empty(),
                    recommended_verify: skill.definition.recommended_verify.clone(),
                    applies_to: applies_row(&skill.definition.applies_to),
                })
                .collect()
        } else {
            Vec::new()
        },
        mcp_servers: if matches!(view, View::All | View::Mcp) {
            set.mcp_servers
                .iter()
                .map(|server| mcp_row(workspace, server))
                .collect()
        } else {
            Vec::new()
        },
        profiles: if matches!(view, View::All | View::Profiles) {
            set.profiles.iter().map(profile_row).collect()
        } else {
            Vec::new()
        },
        conflicts: set.conflicts.iter().map(conflict_row).collect(),
        diagnostics: set.diagnostics.iter().map(diagnostic_row).collect(),
    }
}

#[derive(Debug, Serialize)]
struct ExtensionDoctorReport {
    workspace: String,
    checks: Vec<ExtensionCheck>,
    has_failures: bool,
    warn_count: usize,
    fail_count: usize,
}

#[derive(Debug, Serialize)]
struct ExtensionCheck {
    id: String,
    severity: String,
    detail: String,
    next_step: Option<String>,
}

fn build_doctor_report(workspace: &Path, set: &ExtensionSet) -> ExtensionDoctorReport {
    let mut checks = Vec::new();
    let error_count = set
        .diagnostics
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::Error)
        .count();
    let warn_count = set
        .diagnostics
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::Warn)
        .count();
    if error_count > 0 {
        checks.push(check(
            "extensions.load",
            "fail",
            format!("{error_count} error(s), {warn_count} warning(s)"),
            Some("Fix ~/.phonton or <workspace>/.phonton extension config.".into()),
        ));
    } else if warn_count > 0 {
        checks.push(check(
            "extensions.load",
            "warn",
            format!("{warn_count} warning(s)"),
            Some("Review extension diagnostics before a release run.".into()),
        ));
    } else {
        checks.push(check(
            "extensions.load",
            "ok",
            format!("{} manifest(s)", set.manifests.len()),
            None,
        ));
    }

    if set.conflicts.is_empty() {
        checks.push(check("extensions.conflicts", "ok", "no conflicts", None));
    } else {
        checks.push(check(
            "extensions.conflicts",
            "warn",
            format!("{} conflict(s)", set.conflicts.len()),
            Some("Run `phonton extensions list` to inspect overridden records.".into()),
        ));
    }

    let active_mcp = set.mcp_servers.len();
    let disabled_mcp = set
        .manifests
        .iter()
        .filter(|manifest| manifest.kind == ExtensionKind::McpServer && !manifest.enabled)
        .count();
    if disabled_mcp == 0 {
        checks.push(check("mcp.disabled", "ok", "no disabled MCP servers", None));
    } else {
        checks.push(check(
            "mcp.disabled",
            "warn",
            format!("{disabled_mcp} disabled MCP server manifest(s)"),
            Some("Run `phonton extensions mcp` to inspect disabled server records.".into()),
        ));
    }

    let workspace_mcp = set
        .mcp_servers
        .iter()
        .filter(|server| server.source == ExtensionSource::Workspace)
        .count();
    if workspace_mcp > 0 && !trust::is_trusted(workspace) {
        checks.push(check(
            "mcp.workspace-trust",
            "fail",
            format!("{workspace_mcp} workspace MCP server(s) require trusted workspace"),
            Some("Approve workspace trust before running workspace MCP servers.".into()),
        ));
    } else if workspace_mcp > 0 {
        checks.push(check(
            "mcp.workspace-trust",
            "ok",
            format!("{workspace_mcp} workspace MCP server(s) allowed by trust"),
            None,
        ));
    }

    let missing_env: Vec<String> = set
        .mcp_servers
        .iter()
        .flat_map(|server| {
            server
                .env
                .iter()
                .filter(|name| std::env::var_os(name).is_none())
                .map(|name| format!("{}.{}", server.id, name))
                .collect::<Vec<_>>()
        })
        .collect();
    if missing_env.is_empty() {
        checks.push(check("mcp.env", "ok", "no missing MCP env vars", None));
    } else {
        checks.push(check(
            "mcp.env",
            "warn",
            format!("missing: {}", missing_env.join(", ")),
            Some("Set the required environment variables before using those servers.".into()),
        ));
    }

    let approval_gated: Vec<String> = set
        .mcp_servers
        .iter()
        .filter(|server| server.permissions.iter().any(permission_requires_approval))
        .map(|server| server.id.to_string())
        .collect();
    if active_mcp == 0 {
        checks.push(check(
            "mcp.permissions",
            "ok",
            "no active MCP servers",
            None,
        ));
    } else if approval_gated.is_empty() {
        checks.push(check(
            "mcp.permissions",
            "ok",
            format!("{active_mcp} active MCP server(s), none approval-gated"),
            None,
        ));
    } else {
        checks.push(check(
            "mcp.permissions",
            "warn",
            format!("approval-gated MCP servers: {}", approval_gated.join(", ")),
            Some("Goal runs will prompt before these operations execute.".into()),
        ));
    }

    let fail_count = checks
        .iter()
        .filter(|check| check.severity == "fail")
        .count();
    let warn_count = checks
        .iter()
        .filter(|check| check.severity == "warn")
        .count();
    ExtensionDoctorReport {
        workspace: workspace.display().to_string(),
        checks,
        has_failures: fail_count > 0,
        warn_count,
        fail_count,
    }
}

fn check(
    id: &str,
    severity: &str,
    detail: impl Into<String>,
    next_step: Option<String>,
) -> ExtensionCheck {
    ExtensionCheck {
        id: id.into(),
        severity: severity.into(),
        detail: detail.into(),
        next_step,
    }
}

fn extension_row(manifest: &ExtensionManifest) -> ExtensionRow {
    ExtensionRow {
        id: manifest.id.to_string(),
        kind: manifest.kind.to_string(),
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        source: manifest.source.to_string(),
        status: if manifest.enabled {
            "active".into()
        } else {
            "disabled".into()
        },
        trust: manifest.trust.to_string(),
        permissions: permissions(&manifest.permissions),
        applies_to: applies_row(&manifest.applies_to),
    }
}

fn steering_row(rule: &SteeringRule) -> SteeringRow {
    SteeringRow {
        id: rule.id.to_string(),
        source: rule.source.to_string(),
        severity: rule.severity.to_string(),
        text: rule.text.clone(),
        applies_to: applies_row(&rule.applies_to),
    }
}

fn mcp_row(workspace: &Path, server: &McpServerDefinition) -> McpRow {
    McpRow {
        id: server.id.to_string(),
        name: server.name.clone(),
        source: server.source.to_string(),
        enabled: server.enabled,
        transport: transport(&server.transport),
        trust: server.trust.to_string(),
        permissions: permissions(&server.permissions),
        env: server
            .env
            .iter()
            .map(|name| EnvRow {
                name: name.clone(),
                present: std::env::var_os(name).is_some(),
            })
            .collect(),
        approval_required: server.permissions.iter().any(permission_requires_approval),
        workspace_trust_required: server.source == ExtensionSource::Workspace
            && !trust::is_trusted(workspace),
    }
}

fn profile_row(profile: &ProfileDefinition) -> ProfileRow {
    ProfileRow {
        id: profile.id.to_string(),
        name: profile.name.clone(),
        source: profile.source.to_string(),
        activates: profile.activates.iter().map(ToString::to_string).collect(),
        max_tokens: profile.max_tokens,
        max_usd_micros: profile.max_usd_micros,
    }
}

fn applies_row(applies_to: &phonton_types::AppliesTo) -> AppliesRow {
    AppliesRow {
        paths: applies_to.paths.clone(),
        languages: applies_to.languages.clone(),
        task_classes: applies_to
            .task_classes
            .iter()
            .map(|class| format!("{class:?}"))
            .collect(),
    }
}

fn conflict_row(conflict: &ExtensionConflict) -> ConflictRow {
    ConflictRow {
        id: conflict.id.to_string(),
        lower_source: conflict.lower_source.to_string(),
        higher_source: conflict.higher_source.to_string(),
        detail: conflict.detail.clone(),
    }
}

fn diagnostic_row(diagnostic: &ExtensionDiagnostic) -> DiagnosticRow {
    DiagnosticRow {
        severity: match diagnostic.severity {
            DiagnosticSeverity::Warn => "warn",
            DiagnosticSeverity::Error => "error",
        }
        .into(),
        source: diagnostic.source.to_string(),
        path: diagnostic.path.as_ref().map(|p| p.display().to_string()),
        message: diagnostic.message.clone(),
    }
}

fn permissions(permissions: &[Permission]) -> Vec<String> {
    permissions.iter().map(ToString::to_string).collect()
}

fn permission_requires_approval(permission: &Permission) -> bool {
    !matches!(permission, Permission::FsReadWorkspace)
}

fn transport(transport: &McpTransport) -> String {
    match transport {
        McpTransport::Stdio { command, args } => {
            if args.is_empty() {
                format!("stdio:{command}")
            } else {
                format!("stdio:{} {}", command, args.join(" "))
            }
        }
        McpTransport::Http { url } => format!("http:{url}"),
    }
}

fn print_inventory_report(report: &InventoryReport) {
    println!("{}", report.view);
    if report.extensions.is_empty() {
        println!("  none");
    } else {
        for row in &report.extensions {
            let permissions = if row.permissions.is_empty() {
                "none".into()
            } else {
                row.permissions.join(",")
            };
            println!(
                "- {} [{}] {} source={} status={} trust={} permissions={}",
                row.id, row.kind, row.name, row.source, row.status, row.trust, permissions
            );
        }
    }

    if !report.steering.is_empty() {
        println!("\nActive steering");
        for row in &report.steering {
            println!(
                "- {} [{}] source={} {}",
                row.id, row.severity, row.source, row.text
            );
        }
    }

    if !report.skills.is_empty() {
        println!("\nActive skills");
        for row in &report.skills {
            let loaded = if row.content_loaded {
                "loaded"
            } else {
                "empty"
            };
            println!(
                "- {} {}@{} source={} entry={} {}",
                row.id, row.name, row.version, row.source, row.entry, loaded
            );
        }
    }

    if !report.mcp_servers.is_empty() {
        println!("\nActive MCP servers");
        for row in &report.mcp_servers {
            let approval = if row.approval_required {
                "approval"
            } else {
                "auto"
            };
            let trust = if row.workspace_trust_required {
                "untrusted-workspace"
            } else {
                "trusted"
            };
            println!(
                "- {} ({}) {} trust={} perms={} {} {}",
                row.id,
                row.name,
                row.transport,
                row.trust,
                if row.permissions.is_empty() {
                    "none".into()
                } else {
                    row.permissions.join(",")
                },
                approval,
                trust
            );
        }
    }

    if !report.profiles.is_empty() {
        println!("\nActive profiles");
        for row in &report.profiles {
            println!(
                "- {} ({}) source={} activates={}",
                row.id,
                row.name,
                row.source,
                row.activates.join(",")
            );
        }
    }

    if !report.conflicts.is_empty() {
        println!("\nConflicts");
        for conflict in &report.conflicts {
            println!(
                "- {} {} -> {}: {}",
                conflict.id, conflict.lower_source, conflict.higher_source, conflict.detail
            );
        }
    }

    if !report.diagnostics.is_empty() {
        println!("\nDiagnostics");
        for diagnostic in &report.diagnostics {
            let path = diagnostic.path.as_deref().unwrap_or("(no path)");
            println!(
                "- {} {} {}: {}",
                diagnostic.severity, diagnostic.source, path, diagnostic.message
            );
        }
    }
}

fn print_doctor_report(report: &ExtensionDoctorReport) {
    println!("Extension doctor");
    println!("workspace: {}", report.workspace);
    for check in &report.checks {
        println!("- [{}] {}: {}", check.severity, check.id, check.detail);
        if let Some(next) = &check.next_step {
            println!("  next: {next}");
        }
    }
    println!(
        "summary: {} failure(s), {} warning(s)",
        report.fail_count, report.warn_count
    );
}

fn print_usage() {
    println!(
        "usage:\n  \
         phonton extensions list [--json]\n  \
         phonton extensions doctor [--json]\n  \
         phonton extensions skills [--json]\n  \
         phonton extensions steering [--json]\n  \
         phonton extensions mcp [--json]\n  \
         phonton extensions profiles [--json]\n\n  \
         Convenience aliases:\n  \
         phonton skills list [--json]\n  \
         phonton steering list [--json]\n\n  \
         These commands never start MCP servers."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{AppliesTo, ExtensionScope, TrustLevel};

    #[test]
    fn inventory_includes_disabled_manifest_and_active_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = ExtensionManifest {
            id: "rust-style".into(),
            kind: ExtensionKind::Skill,
            name: "Rust Style".into(),
            version: "0.1.0".into(),
            source: ExtensionSource::Workspace,
            scope: ExtensionScope::Workspace {
                root: tmp.path().to_path_buf(),
            },
            trust: TrustLevel::TextOnly,
            permissions: Vec::new(),
            applies_to: AppliesTo::default(),
            precedence: 20,
            checksum: None,
            enabled: false,
        };
        let set = ExtensionSet {
            manifests: vec![manifest],
            ..ExtensionSet::default()
        };
        let report = build_inventory_report(tmp.path(), &set, View::Skills);
        assert_eq!(report.counts.manifests, 1);
        assert_eq!(report.counts.disabled, 1);
        assert_eq!(report.extensions[0].status, "disabled");
    }

    #[test]
    fn doctor_fails_untrusted_workspace_mcp() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServerDefinition {
            id: "fs".into(),
            name: "FS".into(),
            source: ExtensionSource::Workspace,
            transport: McpTransport::Stdio {
                command: "node".into(),
                args: Vec::new(),
            },
            trust: TrustLevel::ReadOnlyTool,
            permissions: vec![Permission::FsReadWorkspace],
            applies_to: AppliesTo::default(),
            env: Vec::new(),
            enabled: true,
        };
        let set = ExtensionSet {
            mcp_servers: vec![server],
            ..ExtensionSet::default()
        };
        let report = build_doctor_report(tmp.path(), &set);
        assert!(report.has_failures);
        assert!(report
            .checks
            .iter()
            .any(|check| check.id == "mcp.workspace-trust"));
    }

    #[test]
    fn doctor_warns_on_disabled_mcp_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = ExtensionManifest {
            id: "disabled-mcp".into(),
            kind: ExtensionKind::McpServer,
            name: "Disabled MCP".into(),
            version: "0.1.0".into(),
            source: ExtensionSource::Workspace,
            scope: ExtensionScope::Workspace {
                root: tmp.path().to_path_buf(),
            },
            trust: TrustLevel::ReadOnlyTool,
            permissions: vec![Permission::FsReadWorkspace],
            applies_to: AppliesTo::default(),
            precedence: 20,
            checksum: None,
            enabled: false,
        };
        let set = ExtensionSet {
            manifests: vec![manifest],
            ..ExtensionSet::default()
        };
        let report = build_doctor_report(tmp.path(), &set);
        assert_eq!(report.fail_count, 0);
        assert!(report
            .checks
            .iter()
            .any(|check| check.id == "mcp.disabled" && check.severity == "warn"));
    }
}
