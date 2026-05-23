//! First-run diagnostics for the Phonton CLI.
//!
//! `phonton doctor` intentionally runs before the TUI. It checks the local
//! environment, config, workspace trust, persistent store, git/cargo presence,
//! and Nexus config shape, then prints actionable next steps.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use phonton_extensions::{load_extensions, DiagnosticSeverity, ExtensionLoadOptions};
use phonton_providers::provider_for;
use phonton_types::{ExtensionKind, Permission};
use serde::Serialize;

use crate::{
    cloudflare_base_url, config, default_model_for, default_store_path, make_api_provider_config,
    provider_requires_key, trust,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Ok,
    Warn,
    Fail,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub id: &'static str,
    pub severity: Severity,
    pub title: String,
    pub detail: String,
    pub next_step: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub workspace: String,
    pub config_path: Option<String>,
    pub store_path: Option<String>,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.severity == Severity::Fail)
    }

    pub fn warn_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.severity == Severity::Warn)
            .count()
    }

    pub fn fail_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.severity == Severity::Fail)
            .count()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DoctorOptions {
    pub json: bool,
    pub check_provider: bool,
}

pub fn parse_options(args: &[String]) -> Result<DoctorOptions> {
    let mut opts = DoctorOptions::default();
    for arg in args {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--provider" | "--network" => opts.check_provider = true,
            "-h" | "--help" => {
                return Err(anyhow::anyhow!(
                    "usage: phonton doctor [--json] [--provider]\n\n  --json      Emit machine-readable JSON\n  --provider  Also probe the configured provider models endpoint and completion adapter"
                ));
            }
            other => return Err(anyhow::anyhow!("unknown doctor option `{other}`")),
        }
    }
    Ok(opts)
}

pub async fn build_report(workspace: &Path, opts: DoctorOptions) -> DoctorReport {
    let workspace_display = std::fs::canonicalize(workspace)
        .unwrap_or_else(|_| workspace.to_path_buf())
        .display()
        .to_string();
    let config_path = config::config_path();
    let store_path = default_store_path();
    let mut checks = Vec::new();

    check_workspace(workspace, &mut checks);
    check_config(&mut checks, opts).await;
    check_store(&mut checks, store_path.as_deref());
    check_trust(workspace, &mut checks);
    check_command(
        "git",
        &["--version"],
        "git",
        "Install Git and make sure `git --version` works.",
        &mut checks,
    );
    check_command(
        "cargo",
        &["--version"],
        "cargo",
        "Install the Rust toolchain from rustup.rs and make sure `cargo --version` works.",
        &mut checks,
    );
    check_cargo_manifest(workspace, &mut checks);
    check_nexus(workspace, &mut checks);
    check_extensions(workspace, &mut checks);

    DoctorReport {
        workspace: workspace_display,
        config_path: config_path.map(path_string),
        store_path: store_path.map(path_string),
        checks,
    }
}

pub async fn run(workspace: &Path, args: &[String]) -> Result<i32> {
    let opts = match parse_options(args) {
        Ok(opts) => opts,
        Err(e) => {
            let msg = e.to_string();
            if msg.starts_with("usage:") {
                println!("{msg}");
                return Ok(0);
            }
            eprintln!("phonton doctor: {msg}");
            eprintln!("Run `phonton doctor --help` for usage.");
            return Ok(2);
        }
    };

    let report = build_report(workspace, opts).await;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text_report(&report, opts);
    }

    Ok(if report.has_failures() { 1 } else { 0 })
}

fn check_workspace(workspace: &Path, checks: &mut Vec<DoctorCheck>) {
    if workspace.exists() {
        push(
            checks,
            "workspace",
            Severity::Ok,
            "Workspace found",
            format!("{}", workspace.display()),
            None,
        );
    } else {
        push(
            checks,
            "workspace",
            Severity::Fail,
            "Workspace path does not exist",
            format!("{}", workspace.display()),
            Some("Run Phonton from an existing project directory.".into()),
        );
    }
}

async fn check_config(checks: &mut Vec<DoctorCheck>, opts: DoctorOptions) {
    let path = config::config_path();
    let cfg = match config::load() {
        Ok(cfg) => {
            let detail = match &path {
                Some(p) if p.exists() => format!("loaded {}", p.display()),
                Some(p) => format!("no config file yet; using defaults at {}", p.display()),
                None => "HOME is not available; using defaults".into(),
            };
            push(
                checks,
                "config.load",
                Severity::Ok,
                "Config loads",
                detail,
                None,
            );
            cfg
        }
        Err(e) => {
            push(
                checks,
                "config.load",
                Severity::Fail,
                "Config cannot be parsed",
                e.to_string(),
                Some("Fix ~/.phonton/config.toml or run `phonton config edit`.".into()),
            );
            return;
        }
    };

    let provider = cfg.provider.name.as_str();
    if config::KNOWN_PROVIDERS.contains(&provider) {
        push(
            checks,
            "provider.name",
            Severity::Ok,
            "Provider is recognized",
            provider.to_string(),
            None,
        );
    } else {
        push(
            checks,
            "provider.name",
            Severity::Fail,
            "Provider is unknown",
            provider.to_string(),
            Some(format!(
                "Use one of: {}.",
                config::KNOWN_PROVIDERS.join(", ")
            )),
        );
        return;
    }

    let key = config::resolve_api_key(&cfg.provider);
    if provider_requires_key(provider) && key.is_none() {
        push(
            checks,
            "provider.key",
            Severity::Fail,
            "Provider API key is missing",
            format!("{provider} requires a key for network-backed runs"),
            Some(provider_key_hint(provider)),
        );
    } else if key.is_some() {
        push(
            checks,
            "provider.key",
            Severity::Ok,
            "Provider API key resolves",
            "key present; value hidden",
            None,
        );
    } else {
        push(
            checks,
            "provider.key",
            Severity::Ok,
            "Provider does not require an API key",
            provider.to_string(),
            None,
        );
    }

    if matches!(provider, "custom" | "openai-compatible") && cfg.provider.base_url.is_none() {
        push(
            checks,
            "provider.base_url",
            Severity::Fail,
            "Custom provider needs a base_url",
            "no base_url configured",
            Some("Set provider.base_url in ~/.phonton/config.toml.".into()),
        );
    }

    if provider == "cloudflare"
        && cloudflare_base_url(
            cfg.provider.account_id.clone(),
            cfg.provider.base_url.clone(),
        )
        .is_none()
    {
        push(
            checks,
            "provider.cloudflare_account",
            Severity::Fail,
            "Cloudflare account ID is missing",
            "no provider.account_id, provider.base_url, or CLOUDFLARE_ACCOUNT_ID value",
            Some(
                "Set CLOUDFLARE_ACCOUNT_ID, provider.account_id, or provider.base_url to the full Workers AI base URL."
                    .into(),
            ),
        );
    }

    let model = cfg
        .provider
        .model
        .clone()
        .unwrap_or_else(|| default_model_for(provider));
    let custom_missing_model =
        matches!(provider, "custom" | "openai-compatible") && cfg.provider.model.is_none();
    let model_severity = if custom_missing_model {
        Severity::Fail
    } else if cfg.provider.model.is_some() {
        Severity::Ok
    } else {
        Severity::Warn
    };
    let model_for_probe = model.clone();
    push(
        checks,
        "provider.model",
        model_severity,
        if custom_missing_model {
            "Custom provider needs a model"
        } else if cfg.provider.model.is_some() {
            "Model is configured"
        } else {
            "Model is using CLI default"
        },
        model,
        if custom_missing_model {
            Some("Set provider.model in ~/.phonton/config.toml.".into())
        } else if cfg.provider.model.is_some() {
            None
        } else {
            Some(
                "Run `phonton` once with a key configured to auto-detect, or set provider.model."
                    .into(),
            )
        },
    );

    if custom_missing_model {
        return;
    }

    if opts.check_provider {
        check_provider_endpoint(
            checks,
            provider,
            &cfg.provider,
            key.as_deref(),
            &model_for_probe,
        )
        .await;
    } else {
        push(
            checks,
            "provider.probe",
            Severity::Warn,
            "Provider network probe skipped",
            "local-only doctor run",
            Some(
                "Run `phonton doctor --provider` to validate model discovery and a tiny completion call."
                    .into(),
            ),
        );
    }
}

async fn check_provider_endpoint(
    checks: &mut Vec<DoctorCheck>,
    provider: &str,
    cfg: &config::ProviderConfig,
    key: Option<&str>,
    model: &str,
) {
    let effective_key = match key {
        Some(key) => key,
        None if !provider_requires_key(provider) => "",
        None => {
            push(
                checks,
                "provider.probe",
                Severity::Fail,
                "Provider network probe cannot run",
                "missing API key",
                Some(provider_key_hint(provider)),
            );
            return;
        }
    };

    let probe_base = if provider == "cloudflare" {
        cloudflare_base_url(cfg.account_id.clone(), cfg.base_url.clone())
    } else {
        cfg.base_url.clone()
    };
    probe_models(checks, provider, effective_key, probe_base.as_deref()).await;
    probe_completion(
        checks,
        provider,
        effective_key,
        model,
        cfg.account_id.as_deref(),
        cfg.base_url.as_deref(),
    )
    .await;
}

async fn probe_models(
    checks: &mut Vec<DoctorCheck>,
    provider: &str,
    key: &str,
    base_url: Option<&str>,
) {
    let probe = tokio::time::timeout(
        Duration::from_secs(10),
        phonton_providers::discover_models(provider, key, base_url),
    )
    .await;

    match probe {
        Ok(Ok(models)) if !models.is_empty() => push(
            checks,
            "provider.probe",
            Severity::Ok,
            "Provider models endpoint works",
            format!("{} models visible", models.len()),
            None,
        ),
        Ok(Ok(_)) => push(
            checks,
            "provider.probe",
            Severity::Warn,
            "Provider responded with no models",
            provider.to_string(),
            Some(
                "Set provider.model manually or verify the provider account has model access."
                    .into(),
            ),
        ),
        Ok(Err(e)) => push(
            checks,
            "provider.probe",
            Severity::Fail,
            "Provider probe failed",
            e.to_string(),
            Some(
                "Check the API key, base_url, account model access, and network connection.".into(),
            ),
        ),
        Err(_) => push(
            checks,
            "provider.probe",
            Severity::Fail,
            "Provider probe timed out",
            "no response within 10 seconds",
            Some(
                "Check network access or try again with a more reliable provider endpoint.".into(),
            ),
        ),
    }
}

async fn probe_completion(
    checks: &mut Vec<DoctorCheck>,
    provider: &str,
    key: &str,
    model: &str,
    account_id: Option<&str>,
    base_url: Option<&str>,
) {
    let Some(cfg) = make_api_provider_config(
        provider,
        key.to_string(),
        model.to_string(),
        account_id.map(str::to_string),
        base_url.map(str::to_string),
    ) else {
        push(
            checks,
            "provider.completion",
            Severity::Fail,
            "Provider completion call cannot be configured",
            format!("{provider} with model {model}"),
            Some("Check provider.name, provider.model, and provider.base_url.".into()),
        );
        return;
    };

    let provider_impl = provider_for(cfg);
    let probe = tokio::time::timeout(
        Duration::from_secs(60),
        provider_impl.call(
            "You are a terse assistant. Respond only with JSON.",
            "Return exactly {\"ok\":true} as JSON.",
            &[],
        ),
    )
    .await;

    match probe {
        Ok(Ok(resp)) if !resp.content.trim().is_empty() => push(
            checks,
            "provider.completion",
            Severity::Ok,
            "Provider completion adapter works",
            format!(
                "{} via {} (input {}, output {}, cached {}, cache_create {})",
                resp.model_name,
                resp.provider,
                resp.input_tokens,
                resp.output_tokens,
                resp.cached_tokens,
                resp.cache_creation_tokens
            ),
            None,
        ),
        Ok(Ok(resp)) => push(
            checks,
            "provider.completion",
            Severity::Fail,
            "Provider completion returned empty content",
            format!("{} via {}", resp.model_name, resp.provider),
            Some("Check the configured model name and provider account access.".into()),
        ),
        Ok(Err(e)) => push(
            checks,
            "provider.completion",
            Severity::Fail,
            "Provider completion call failed",
            e.to_string(),
            Some(
                "Check model name, API key, account quota, and chat/completions compatibility."
                    .into(),
            ),
        ),
        Err(_) => push(
            checks,
            "provider.completion",
            Severity::Fail,
            "Provider completion call timed out",
            "no response within 60 seconds",
            Some("Check network access or try a faster model/provider endpoint.".into()),
        ),
    }
}

fn check_store(checks: &mut Vec<DoctorCheck>, store_path: Option<&Path>) {
    let Some(path) = store_path else {
        push(
            checks,
            "store",
            Severity::Fail,
            "Persistent store path is unavailable",
            "HOME is not set",
            Some("Set HOME/USERPROFILE so Phonton can create ~/.phonton/store.sqlite3.".into()),
        );
        return;
    };

    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            push(
                checks,
                "store",
                Severity::Fail,
                "Persistent store directory is not writable",
                format!("{}: {e}", parent.display()),
                Some("Fix directory permissions or set a writable home directory.".into()),
            );
            return;
        }
    }

    match phonton_store::Store::open(path) {
        Ok(_) => push(
            checks,
            "store",
            Severity::Ok,
            "Persistent store opens",
            path.display().to_string(),
            None,
        ),
        Err(e) => push(
            checks,
            "store",
            Severity::Fail,
            "Persistent store cannot open",
            e.to_string(),
            Some("Delete or repair ~/.phonton/store.sqlite3 after backing it up if needed.".into()),
        ),
    }
}

fn check_trust(workspace: &Path, checks: &mut Vec<DoctorCheck>) {
    if trust::is_trusted(workspace) {
        push(
            checks,
            "workspace.trust",
            Severity::Ok,
            "Workspace is trusted",
            "TUI can start without prompting",
            None,
        );
    } else {
        push(
            checks,
            "workspace.trust",
            Severity::Warn,
            "Workspace is not trusted yet",
            "first TUI launch will ask for consent",
            Some("Run `phonton` and accept the workspace trust prompt.".into()),
        );
    }
}

fn check_command(
    id: &'static str,
    args: &[&str],
    command: &str,
    next_step: &str,
    checks: &mut Vec<DoctorCheck>,
) {
    match Command::new(command).args(args).output() {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let first = stdout.lines().next().unwrap_or(command).trim();
            push(
                checks,
                id,
                Severity::Ok,
                format!("{command} is available"),
                first.to_string(),
                None,
            );
        }
        Ok(out) => push(
            checks,
            id,
            Severity::Fail,
            format!("{command} returned a failure"),
            format!("exit status {}", out.status),
            Some(next_step.into()),
        ),
        Err(e) => push(
            checks,
            id,
            Severity::Fail,
            format!("{command} is not available"),
            e.to_string(),
            Some(next_step.into()),
        ),
    }
}

fn check_cargo_manifest(workspace: &Path, checks: &mut Vec<DoctorCheck>) {
    let manifest = find_upwards(workspace, "Cargo.toml");
    match manifest {
        Some(path) => push(
            checks,
            "workspace.cargo",
            Severity::Ok,
            "Cargo manifest found",
            path.display().to_string(),
            None,
        ),
        None => push(
            checks,
            "workspace.cargo",
            Severity::Warn,
            "Cargo manifest not found",
            "Rust verification may be limited outside Cargo workspaces",
            Some("Run Phonton from a Rust workspace for the strongest initial workflow.".into()),
        ),
    }
}

fn check_nexus(workspace: &Path, checks: &mut Vec<DoctorCheck>) {
    match phonton_index::discover_nexus_config(workspace) {
        Ok(Some(cfg)) => {
            let missing: Vec<String> = cfg
                .resolved_repos()
                .into_iter()
                .filter(|(_, path)| !path.exists())
                .map(|(name, path)| format!("{name} ({})", path.display()))
                .collect();
            if missing.is_empty() {
                push(
                    checks,
                    "nexus",
                    Severity::Ok,
                    "Nexus config is valid",
                    format!("{} sibling repos configured", cfg.repos.len()),
                    None,
                );
            } else {
                push(
                    checks,
                    "nexus",
                    Severity::Warn,
                    "Nexus config has missing repos",
                    missing.join(", "),
                    Some("Fix paths in nexus.json or remove stale repo entries.".into()),
                );
            }
        }
        Ok(None) => push(
            checks,
            "nexus",
            Severity::Warn,
            "Nexus config not found",
            "single-workspace indexing only",
            Some("Add nexus.json when this repo needs sibling-repo context.".into()),
        ),
        Err(e) => push(
            checks,
            "nexus",
            Severity::Fail,
            "Nexus config is malformed",
            e.to_string(),
            Some("Fix nexus.json before relying on cross-repo context.".into()),
        ),
    }
}

fn check_extensions(workspace: &Path, checks: &mut Vec<DoctorCheck>) {
    let set = load_extensions(&ExtensionLoadOptions::for_workspace(workspace));
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
        let first = set
            .diagnostics
            .iter()
            .find(|d| d.severity == DiagnosticSeverity::Error)
            .map(|d| d.message.clone())
            .unwrap_or_else(|| "extension config has errors".into());
        push(
            checks,
            "extensions.load",
            Severity::Fail,
            "Extension config has errors",
            format!("{error_count} error(s), {warn_count} warning(s): {first}"),
            Some("Fix ~/.phonton or <workspace>/.phonton extension config.".into()),
        );
    } else if warn_count > 0 {
        push(
            checks,
            "extensions.load",
            Severity::Warn,
            "Extension config has warnings",
            format!("{warn_count} warning(s)"),
            Some("Review extension diagnostics before a release run.".into()),
        );
    } else {
        push(
            checks,
            "extensions.load",
            Severity::Ok,
            "Extension config loads",
            format!("{} manifest(s)", set.manifests.len()),
            None,
        );
    }

    let disabled_mcp = set
        .manifests
        .iter()
        .filter(|m| m.kind == ExtensionKind::McpServer && !m.enabled)
        .count();
    let active_mcp = set.mcp_servers.len();
    let approval_gated: Vec<String> = set
        .mcp_servers
        .iter()
        .filter(|server| {
            server
                .permissions
                .iter()
                .any(|permission| !matches!(permission, Permission::FsReadWorkspace))
        })
        .map(|server| {
            format!(
                "{} ({})",
                server.id,
                render_permissions(&server.permissions)
            )
        })
        .collect();

    if active_mcp == 0 && disabled_mcp == 0 {
        push(
            checks,
            "mcp.config",
            Severity::Ok,
            "No MCP servers configured",
            "nothing will be started",
            None,
        );
    } else if !approval_gated.is_empty() {
        push(
            checks,
            "mcp.config",
            Severity::Warn,
            "MCP servers require approval",
            format!(
                "{} active, {} disabled; approval-gated: {}",
                active_mcp,
                disabled_mcp,
                approval_gated.join(", ")
            ),
            Some("Use `phonton mcp list`; tool operations require explicit approval.".into()),
        );
    } else {
        push(
            checks,
            "mcp.config",
            Severity::Ok,
            "MCP config is conservative",
            format!("{active_mcp} active, {disabled_mcp} disabled"),
            None,
        );
    }
}

fn render_permissions(permissions: &[Permission]) -> String {
    if permissions.is_empty() {
        "none".into()
    } else {
        permissions
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn print_text_report(report: &DoctorReport, opts: DoctorOptions) {
    println!("Phonton doctor");
    println!("workspace: {}", report.workspace);
    if let Some(path) = &report.config_path {
        println!("config:    {path}");
    }
    if let Some(path) = &report.store_path {
        println!("store:     {path}");
    }
    println!();

    for check in &report.checks {
        println!(
            "[{}] {}: {}",
            check.severity.label(),
            check.title,
            check.detail
        );
        if let Some(next) = &check.next_step {
            println!("      next: {next}");
        }
    }

    println!();
    if report.has_failures() {
        println!(
            "Result: {} failing check(s), {} warning(s). Fix failures before launch or real repo tasks.",
            report.fail_count(),
            report.warn_count()
        );
    } else if report.warn_count() > 0 {
        println!(
            "Result: usable with {} warning(s). Tighten these before a trusted release run.",
            report.warn_count()
        );
    } else {
        println!("Result: ready for a trusted Phonton run.");
    }

    if !opts.check_provider {
        println!("Provider was not probed. Use `phonton doctor --provider` for the network check.");
    }
}

fn push(
    checks: &mut Vec<DoctorCheck>,
    id: &'static str,
    severity: Severity,
    title: impl Into<String>,
    detail: impl Into<String>,
    next_step: Option<String>,
) {
    checks.push(DoctorCheck {
        id,
        severity,
        title: title.into(),
        detail: detail.into(),
        next_step,
    });
}

fn provider_key_hint(provider: &str) -> String {
    match provider {
        "anthropic" => {
            "Set ANTHROPIC_API_KEY or provider.api_key in ~/.phonton/config.toml.".into()
        }
        "openai" => "Set OPENAI_API_KEY or provider.api_key in ~/.phonton/config.toml.".into(),
        "openrouter" => {
            "Set OPENROUTER_API_KEY or provider.api_key in ~/.phonton/config.toml.".into()
        }
        "gemini" => "Set GEMINI_API_KEY, GOOGLE_API_KEY, or provider.api_key.".into(),
        "agentrouter" => "Set AGENTROUTER_API_KEY or provider.api_key.".into(),
        "cloudflare" => {
            "Set CLOUDFLARE_API_TOKEN plus CLOUDFLARE_ACCOUNT_ID, or provider.api_key plus provider.base_url.".into()
        }
        "deepseek" => "Set DEEPSEEK_API_KEY or provider.api_key.".into(),
        "xai" | "grok" => "Set XAI_API_KEY, GROK_API_KEY, or provider.api_key.".into(),
        "groq" => "Set GROQ_API_KEY or provider.api_key.".into(),
        "together" => "Set TOGETHER_API_KEY, TOGETHER_AI_API_KEY, or provider.api_key.".into(),
        _ => "Set provider.api_key in ~/.phonton/config.toml.".into(),
    }
}

fn find_upwards(start: &Path, filename: &str) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let candidate = dir.join(filename);
        if candidate.exists() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

fn path_string(path: PathBuf) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_options_accepts_json_and_provider_probe() {
        let args = vec!["--json".into(), "--provider".into()];
        let opts = parse_options(&args).unwrap();
        assert!(opts.json);
        assert!(opts.check_provider);
    }

    #[test]
    fn report_failure_summary_detects_failures() {
        let report = DoctorReport {
            workspace: ".".into(),
            config_path: None,
            store_path: None,
            checks: vec![DoctorCheck {
                id: "x",
                severity: Severity::Fail,
                title: "bad".into(),
                detail: "broken".into(),
                next_step: None,
            }],
        };
        assert!(report.has_failures());
        assert_eq!(report.fail_count(), 1);
    }
}
