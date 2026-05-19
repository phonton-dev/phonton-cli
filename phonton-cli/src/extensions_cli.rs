//! `phonton extensions` inspection commands.
//!
//! Inspection commands load the same local extension set the runtime uses.
//! Install and scaffold commands write `.phonton` records, but never start MCP
//! servers or execute skills.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
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

#[derive(Debug, Clone, Copy)]
struct CatalogEntry {
    id: &'static str,
    name: &'static str,
    repo: &'static str,
    license: &'static str,
    kind: &'static str,
    trust: &'static str,
    permissions: &'static [&'static str],
    summary: &'static str,
    manifest: &'static str,
}

fn catalog_entries() -> &'static [CatalogEntry] {
    &[
        CatalogEntry {
            id: "github",
            name: "GitHub MCP Server",
            repo: "github/github-mcp-server",
            license: "MIT",
            kind: "mcp-server",
            trust: "networked-tool",
            permissions: &["network.request", "process.run"],
            summary: "Official GitHub MCP server for repository, issue, pull request, Actions, and code security workflows.",
            manifest: r#"[[servers]]
id = "github"
name = "GitHub MCP Server"
command = "docker"
args = [
  "run",
  "-i",
  "--rm",
  "-e",
  "GITHUB_PERSONAL_ACCESS_TOKEN",
  "ghcr.io/github/github-mcp-server",
]
env = ["GITHUB_PERSONAL_ACCESS_TOKEN"]
trust = "networked-tool"
permissions = ["network.request", "process.run"]
enabled = true
"#,
        },
        CatalogEntry {
            id: "context7",
            name: "Context7",
            repo: "upstash/context7",
            license: "MIT",
            kind: "mcp-server",
            trust: "networked-tool",
            permissions: &["network.request"],
            summary: "Current library docs and code examples through Context7's MCP endpoint.",
            manifest: r#"[[servers]]
id = "context7"
name = "Context7"
url = "https://mcp.context7.com/mcp"
trust = "networked-tool"
permissions = ["network.request"]
enabled = true
"#,
        },
        CatalogEntry {
            id: "chrome-devtools",
            name: "Chrome DevTools MCP",
            repo: "ChromeDevTools/chrome-devtools-mcp",
            license: "Apache-2.0",
            kind: "mcp-server",
            trust: "networked-tool",
            permissions: &["network.request", "process.run"],
            summary: "Chrome inspection, automation, tracing, performance, and page debugging.",
            manifest: r#"[[servers]]
id = "chrome-devtools"
name = "Chrome DevTools MCP"
command = "npx"
args = ["-y", "chrome-devtools-mcp@latest", "--no-usage-statistics"]
trust = "networked-tool"
permissions = ["network.request", "process.run"]
enabled = true
"#,
        },
        CatalogEntry {
            id: "playwright",
            name: "Playwright MCP",
            repo: "microsoft/playwright-mcp",
            license: "Apache-2.0",
            kind: "mcp-server",
            trust: "networked-tool",
            permissions: &["network.request", "process.run"],
            summary: "Browser automation and QA flows for validating pages and screenshots.",
            manifest: r#"[[servers]]
id = "playwright"
name = "Playwright MCP"
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
trust = "networked-tool"
permissions = ["network.request", "process.run"]
enabled = true
"#,
        },
        CatalogEntry {
            id: "firecrawl",
            name: "Firecrawl MCP",
            repo: "firecrawl/firecrawl-mcp-server",
            license: "MIT",
            kind: "mcp-server",
            trust: "networked-tool",
            permissions: &["network.request", "process.run"],
            summary: "Web crawl, search, scrape, and extraction tools for research-heavy goals.",
            manifest: r#"[[servers]]
id = "firecrawl"
name = "Firecrawl MCP"
command = "npx"
args = ["-y", "firecrawl-mcp"]
env = ["FIRECRAWL_API_KEY"]
trust = "networked-tool"
permissions = ["network.request", "process.run"]
enabled = true
"#,
        },
        CatalogEntry {
            id: "supabase",
            name: "Supabase MCP",
            repo: "supabase-community/supabase-mcp",
            license: "Apache-2.0",
            kind: "mcp-server",
            trust: "networked-tool",
            permissions: &["network.request"],
            summary: "Supabase project, database, docs, and local development context with read-only defaults.",
            manifest: r#"[[servers]]
id = "supabase"
name = "Supabase MCP"
url = "https://mcp.supabase.com/mcp?read_only=true"
trust = "networked-tool"
permissions = ["network.request"]
enabled = true
"#,
        },
        CatalogEntry {
            id: "mongodb",
            name: "MongoDB MCP Server",
            repo: "mongodb-js/mongodb-mcp-server",
            license: "Apache-2.0",
            kind: "mcp-server",
            trust: "networked-tool",
            permissions: &["network.request", "process.run"],
            summary: "MongoDB and Atlas inspection with read-only mode enabled by default.",
            manifest: r#"[[servers]]
id = "mongodb"
name = "MongoDB MCP Server"
command = "npx"
args = ["-y", "mongodb-mcp-server@latest", "--readOnly"]
env = ["MDB_MCP_CONNECTION_STRING"]
trust = "networked-tool"
permissions = ["network.request", "process.run"]
enabled = true
"#,
        },
        CatalogEntry {
            id: "figma",
            name: "Framelink MCP for Figma",
            repo: "GLips/Figma-Context-MCP",
            license: "MIT",
            kind: "mcp-server",
            trust: "networked-tool",
            permissions: &["network.request", "process.run"],
            summary: "Figma layout context for implementation work through an auditable MCP manifest.",
            manifest: r#"[[servers]]
id = "figma"
name = "Framelink MCP for Figma"
command = "npx"
args = ["-y", "figma-developer-mcp", "--stdio"]
env = ["FIGMA_API_KEY"]
trust = "networked-tool"
permissions = ["network.request", "process.run"]
enabled = true
"#,
        },
    ]
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
        "catalog" | "gallery" => catalog(&args[1..]),
        "install" => install(workspace, &args[1..]),
        "new" => new_extension(&args[1..]),
        "doctor" => doctor(workspace, &args[1..]),
        "validate" => doctor(workspace, &args[1..]),
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

fn catalog(args: &[String]) -> Result<i32> {
    let opts = parse_options(args)?;
    let report = CatalogReport {
        extensions: catalog_entries()
            .iter()
            .map(|entry| CatalogRow {
                id: entry.id.into(),
                name: entry.name.into(),
                repo: entry.repo.into(),
                license: entry.license.into(),
                kind: entry.kind.into(),
                trust: entry.trust.into(),
                permissions: entry
                    .permissions
                    .iter()
                    .map(|permission| (*permission).to_string())
                    .collect(),
                summary: entry.summary.into(),
                install: format!("phonton extensions install {}", entry.id),
            })
            .collect(),
    };
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_catalog_report(&report);
    }
    Ok(0)
}

fn install(workspace: &Path, args: &[String]) -> Result<i32> {
    let opts = parse_install_options(args)?;
    let target_dir = opts.scope.target_dir(workspace)?;
    let report = if let Some(entry) = catalog_entry_for_source(&opts.source) {
        install_catalog_entry(entry, &target_dir, &opts)?
    } else {
        install_pack(workspace, &target_dir, &opts)?
    };

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_install_report(&report);
    }
    Ok(0)
}

fn new_extension(args: &[String]) -> Result<i32> {
    let opts = parse_new_options(args)?;
    let root = PathBuf::from(&opts.path);
    if root.exists() && root.read_dir()?.next().is_some() && !opts.force {
        bail!(
            "{} already exists and is not empty; pass --force to add template files",
            root.display()
        );
    }

    let files = match opts.template.as_str() {
        "skill" | "context" => vec![
            (
                ".phonton/skills/example/skill.toml",
                r#"[skill]
id = "example"
name = "Example Skill"
version = "0.1.0"
entry = "SKILL.md"
trust = "text-only"
recommended_verify = ["phonton extensions validate"]
"#,
            ),
            (
                ".phonton/skills/example/SKILL.md",
                "# Example Skill\n\nUse this skill to describe repeatable project guidance.\n",
            ),
        ],
        "steering" => vec![(
            ".phonton/steering.toml",
            r#"[[rules]]
id = "example.review"
name = "Example review rule"
severity = "warn"
text = "Broad changes should report verification commands and known gaps."
"#,
        )],
        "mcp" | "mcp-server" => vec![(
            ".phonton/mcp.toml",
            r#"[[servers]]
id = "workspace-filesystem-readonly"
name = "Workspace filesystem readonly"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "."]
trust = "read-only-tool"
permissions = ["fs.read.workspace", "process.run"]
enabled = true
"#,
        )],
        "profile" => vec![(
            ".phonton/profiles.toml",
            r#"[[profiles]]
id = "example"
name = "Example Profile"
activates = ["example"]
max_tokens = 120000
"#,
        )],
        other => bail!(
            "unknown extension template `{other}`; use skill, steering, mcp-server, or profile"
        ),
    };

    let mut written = Vec::new();
    for (relative, content) in files {
        let path = root.join(relative);
        if path.exists() && !opts.force {
            bail!(
                "{} already exists; pass --force to overwrite",
                path.display()
            );
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
        written.push(path.display().to_string());
    }

    let report = NewExtensionReport {
        path: root.display().to_string(),
        template: opts.template,
        files: written,
        next_steps: vec![
            "Review the generated .phonton files.".into(),
            "Run `phonton extensions validate` from a workspace that uses them.".into(),
        ],
    };

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Created Phonton extension template at {}", report.path);
        for file in &report.files {
            println!("- {file}");
        }
        for step in &report.next_steps {
            println!("next: {step}");
        }
    }
    Ok(0)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallScope {
    Workspace,
    User,
}

impl InstallScope {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "workspace" | "project" => Ok(Self::Workspace),
            "user" | "global" => Ok(Self::User),
            other => bail!("unknown extension scope `{other}`; use workspace or user"),
        }
    }

    fn target_dir(self, workspace: &Path) -> Result<PathBuf> {
        match self {
            Self::Workspace => Ok(workspace.join(".phonton")),
            Self::User => dirs::home_dir()
                .map(|home| home.join(".phonton"))
                .ok_or_else(|| anyhow!("could not resolve home directory for user scope")),
        }
    }
}

impl std::fmt::Display for InstallScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Workspace => f.write_str("workspace"),
            Self::User => f.write_str("user"),
        }
    }
}

#[derive(Debug)]
struct InstallOptions {
    source: String,
    scope: InstallScope,
    git_ref: Option<String>,
    force: bool,
    dry_run: bool,
    json: bool,
}

#[derive(Debug)]
struct NewOptions {
    path: String,
    template: String,
    force: bool,
    json: bool,
}

#[derive(Debug, Serialize)]
struct CatalogReport {
    extensions: Vec<CatalogRow>,
}

#[derive(Debug, Serialize)]
struct CatalogRow {
    id: String,
    name: String,
    repo: String,
    license: String,
    kind: String,
    trust: String,
    permissions: Vec<String>,
    summary: String,
    install: String,
}

#[derive(Debug, Serialize)]
struct InstallReport {
    source: String,
    source_kind: String,
    scope: String,
    target: String,
    dry_run: bool,
    installed_files: Vec<String>,
    message: String,
    next_steps: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NewExtensionReport {
    path: String,
    template: String,
    files: Vec<String>,
    next_steps: Vec<String>,
}

fn parse_install_options(args: &[String]) -> Result<InstallOptions> {
    let mut source = None;
    let mut scope = InstallScope::Workspace;
    let mut git_ref = None;
    let mut force = false;
    let mut dry_run = false;
    let mut json = false;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" | "help" => {
                print_install_usage();
                std::process::exit(0);
            }
            "--json" => json = true,
            "--force" => force = true,
            "--dry-run" => dry_run = true,
            "--scope" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow!("--scope requires workspace or user"))?;
                scope = InstallScope::parse(value)?;
            }
            arg if arg.starts_with("--scope=") => {
                scope = InstallScope::parse(arg.trim_start_matches("--scope="))?;
            }
            "--ref" => {
                index += 1;
                git_ref = Some(
                    args.get(index)
                        .ok_or_else(|| anyhow!("--ref requires a git ref"))?
                        .clone(),
                );
            }
            arg if arg.starts_with("--ref=") => {
                git_ref = Some(arg.trim_start_matches("--ref=").to_string());
            }
            other if other.starts_with('-') => bail!("unknown install option `{other}`"),
            value => {
                if source.replace(value.to_string()).is_some() {
                    bail!("phonton extensions install accepts one source");
                }
            }
        }
        index += 1;
    }

    let source = source.ok_or_else(|| anyhow!("missing extension source"))?;
    Ok(InstallOptions {
        source,
        scope,
        git_ref,
        force,
        dry_run,
        json,
    })
}

fn parse_new_options(args: &[String]) -> Result<NewOptions> {
    let mut path = None;
    let mut template = None;
    let mut force = false;
    let mut json = false;

    for arg in args {
        match arg.as_str() {
            "-h" | "--help" | "help" => {
                println!("usage: phonton extensions new <path> [skill|steering|mcp-server|profile] [--force] [--json]");
                std::process::exit(0);
            }
            "--force" => force = true,
            "--json" => json = true,
            other if other.starts_with('-') => bail!("unknown new option `{other}`"),
            value => {
                if path.is_none() {
                    path = Some(value.to_string());
                } else if template.is_none() {
                    template = Some(value.to_string());
                } else {
                    bail!("phonton extensions new accepts a path and optional template");
                }
            }
        }
    }

    Ok(NewOptions {
        path: path.ok_or_else(|| anyhow!("missing extension template path"))?,
        template: template.unwrap_or_else(|| "skill".into()),
        force,
        json,
    })
}

fn catalog_entry_for_source(source: &str) -> Option<&'static CatalogEntry> {
    let normalized = normalize_source(source);
    catalog_entries().iter().find(|entry| {
        normalized == entry.id
            || normalized == normalize_source(entry.repo)
            || normalized == normalize_source(&format!("https://github.com/{}", entry.repo))
            || normalized == normalize_source(&format!("https://github.com/{}.git", entry.repo))
    })
}

fn normalize_source(source: &str) -> String {
    source
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase()
}

fn install_catalog_entry(
    entry: &CatalogEntry,
    target_dir: &Path,
    opts: &InstallOptions,
) -> Result<InstallReport> {
    if opts.git_ref.is_some() {
        bail!("--ref is only supported for GitHub or local .phonton extension packs");
    }
    let target = target_dir.join("mcp.d").join(format!("{}.toml", entry.id));
    let installed_files = vec![target.display().to_string()];
    let message = if target.exists() && !opts.force {
        format!("{} is already installed", entry.id)
    } else if opts.dry_run {
        format!("would install {} into {}", entry.id, target.display())
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&target, entry.manifest)
            .with_context(|| format!("failed to write {}", target.display()))?;
        format!("installed {} into {}", entry.id, target.display())
    };

    Ok(InstallReport {
        source: opts.source.clone(),
        source_kind: "catalog".into(),
        scope: opts.scope.to_string(),
        target: target_dir.display().to_string(),
        dry_run: opts.dry_run,
        installed_files,
        message,
        next_steps: vec![
            "Run `phonton extensions validate` to inspect trust, env, and permissions.".into(),
            format!(
                "Run `phonton mcp tools {} --yes` to inspect exposed tools.",
                entry.id
            ),
        ],
    })
}

fn install_pack(
    workspace: &Path,
    target_dir: &Path,
    opts: &InstallOptions,
) -> Result<InstallReport> {
    let mut cleanup_dir = None;
    let source_root = if let Some(local) = local_source_path(workspace, &opts.source) {
        if opts.git_ref.is_some() {
            bail!("--ref is only supported for GitHub extension sources");
        }
        local
    } else {
        let url = github_source_url(&opts.source)
            .ok_or_else(|| anyhow!("unknown extension source `{}`", opts.source))?;
        let cloned = clone_extension_source(&url, opts.git_ref.as_deref())?;
        cleanup_dir = Some(cloned.clone());
        cloned
    };

    let phonton_dir = source_phonton_dir(&source_root)?;
    let files = collect_files(&phonton_dir)?;
    if files.is_empty() {
        bail!(
            "{} does not contain installable .phonton files",
            phonton_dir.display()
        );
    }

    let mut installed_files = Vec::new();
    for file in &files {
        let relative = file
            .strip_prefix(&phonton_dir)
            .with_context(|| format!("failed to relativize {}", file.display()))?;
        let target = target_dir.join(relative);
        if target.exists() && !opts.force {
            bail!(
                "{} already exists; pass --force to overwrite or inspect the pack first",
                target.display()
            );
        }
        installed_files.push(target.display().to_string());
    }

    if !opts.dry_run {
        for file in &files {
            let relative = file
                .strip_prefix(&phonton_dir)
                .with_context(|| format!("failed to relativize {}", file.display()))?;
            let target = target_dir.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::copy(file, &target).with_context(|| {
                format!("failed to copy {} to {}", file.display(), target.display())
            })?;
        }
    }

    if let Some(dir) = cleanup_dir {
        let _ = fs::remove_dir_all(dir);
    }

    Ok(InstallReport {
        source: opts.source.clone(),
        source_kind: "phonton-pack".into(),
        scope: opts.scope.to_string(),
        target: target_dir.display().to_string(),
        dry_run: opts.dry_run,
        installed_files,
        message: if opts.dry_run {
            "validated extension pack; no files written".into()
        } else {
            "installed extension pack".into()
        },
        next_steps: vec![
            "Run `phonton extensions validate` to validate loaded records.".into(),
            "Run `phonton extensions list` to inspect the active extension set.".into(),
        ],
    })
}

fn local_source_path(workspace: &Path, source: &str) -> Option<PathBuf> {
    let path = PathBuf::from(source);
    let candidate = if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    };
    candidate.exists().then_some(candidate)
}

fn github_source_url(source: &str) -> Option<String> {
    if source.starts_with("https://") || source.starts_with("http://") || source.starts_with("git@")
    {
        return Some(source.to_string());
    }
    let slash_count = source.matches('/').count();
    if slash_count == 1 && !source.contains('\\') && !source.starts_with('.') {
        return Some(format!("https://github.com/{source}.git"));
    }
    None
}

fn clone_extension_source(url: &str, git_ref: Option<&str>) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let clone_dir =
        std::env::temp_dir().join(format!("phonton-extension-{}-{stamp}", std::process::id()));
    if clone_dir.exists() {
        fs::remove_dir_all(&clone_dir)
            .with_context(|| format!("failed to clear {}", clone_dir.display()))?;
    }
    let status = Command::new("git")
        .args(["clone", "--quiet", url])
        .arg(&clone_dir)
        .status()
        .context("failed to run git clone")?;
    if !status.success() {
        bail!("git clone failed for {url}");
    }
    if let Some(reference) = git_ref {
        let status = Command::new("git")
            .arg("-C")
            .arg(&clone_dir)
            .args(["checkout", "--quiet", reference])
            .status()
            .context("failed to run git checkout")?;
        if !status.success() {
            bail!("git checkout failed for ref {reference}");
        }
    }
    Ok(clone_dir)
}

fn source_phonton_dir(source_root: &Path) -> Result<PathBuf> {
    let nested = source_root.join(".phonton");
    if nested.is_dir() {
        return Ok(nested);
    }
    if source_root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == ".phonton")
    {
        return Ok(source_root.to_path_buf());
    }
    bail!(
        "{} does not contain a .phonton directory",
        source_root.display()
    )
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(root, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files_inner(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
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
        .filter(|server| server_requires_approval(server))
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
        approval_required: server_requires_approval(server),
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

fn server_requires_approval(server: &McpServerDefinition) -> bool {
    matches!(
        server.trust,
        phonton_types::TrustLevel::MutatingTool | phonton_types::TrustLevel::NetworkedTool
    ) || server.permissions.iter().any(permission_requires_approval)
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

fn print_catalog_report(report: &CatalogReport) {
    println!("Extension catalog");
    for row in &report.extensions {
        let permissions = if row.permissions.is_empty() {
            "none".into()
        } else {
            row.permissions.join(",")
        };
        println!(
            "- {} ({}) repo={} license={} trust={} permissions={}",
            row.id, row.name, row.repo, row.license, row.trust, permissions
        );
        println!("  {}", row.summary);
        println!("  install: {}", row.install);
    }
}

fn print_install_report(report: &InstallReport) {
    println!("{}", report.message);
    println!("source: {} ({})", report.source, report.source_kind);
    println!("scope: {}", report.scope);
    println!("target: {}", report.target);
    if report.dry_run {
        println!("dry-run: true");
    }
    for file in &report.installed_files {
        println!("- {file}");
    }
    for step in &report.next_steps {
        println!("next: {step}");
    }
}

fn print_install_usage() {
    println!(
        "usage:\n  \
         phonton extensions install <source> [--scope workspace|user] [--ref <ref>] [--dry-run] [--force] [--json]\n\n  \
         Sources can be built-in catalog ids, open-source MCP repo URLs from the catalog, local paths, or GitHub .phonton extension pack repos.\n  \
         Examples:\n  \
         phonton extensions install context7\n  \
         phonton extensions install https://github.com/phonton-dev/phonton-review-gate-extension\n  \
         phonton extensions install ./my-extension --scope user\n"
    );
}

fn print_usage() {
    println!(
        "usage:\n  \
         phonton extensions list [--json]\n  \
         phonton extensions catalog [--json]\n  \
         phonton extensions install <source> [--scope workspace|user] [--ref <ref>] [--dry-run] [--force] [--json]\n  \
         phonton extensions new <path> [skill|steering|mcp-server|profile] [--force] [--json]\n  \
         phonton extensions doctor [--json]\n  \
         phonton extensions validate [--json]\n  \
         phonton extensions skills [--json]\n  \
         phonton extensions steering [--json]\n  \
         phonton extensions mcp [--json]\n  \
         phonton extensions profiles [--json]\n\n  \
         Convenience aliases:\n  \
         phonton skills list [--json]\n  \
         phonton steering list [--json]\n\n  \
         Install writes local .phonton records but never starts MCP servers."
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

    #[test]
    fn doctor_warns_on_networked_or_mutating_mcp_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServerDefinition {
            id: "github".into(),
            name: "GitHub".into(),
            source: ExtensionSource::UserHome,
            transport: McpTransport::Http {
                url: "https://example.invalid/mcp".into(),
            },
            trust: TrustLevel::NetworkedTool,
            permissions: Vec::new(),
            applies_to: AppliesTo::default(),
            env: Vec::new(),
            enabled: true,
        };
        let set = ExtensionSet {
            mcp_servers: vec![server],
            ..ExtensionSet::default()
        };

        let report = build_doctor_report(tmp.path(), &set);
        let inventory = build_inventory_report(tmp.path(), &set, View::Mcp);

        assert!(report.checks.iter().any(|check| {
            check.id == "mcp.permissions"
                && check.severity == "warn"
                && check.detail.contains("github")
        }));
        assert!(inventory.mcp_servers[0].approval_required);
    }

    #[test]
    fn catalog_install_writes_mcp_d_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join(".phonton");
        let entry = catalog_entry_for_source("context7").unwrap();
        let opts = InstallOptions {
            source: "context7".into(),
            scope: InstallScope::Workspace,
            git_ref: None,
            force: false,
            dry_run: false,
            json: false,
        };

        let report = install_catalog_entry(entry, &target, &opts).unwrap();
        assert_eq!(report.source_kind, "catalog");
        assert!(target.join("mcp.d/context7.toml").exists());

        let set =
            load_extensions(&ExtensionLoadOptions::for_workspace(tmp.path()).without_user_dir());
        assert_eq!(set.mcp_servers.len(), 1);
        assert_eq!(set.mcp_servers[0].id.to_string(), "context7");
    }

    #[test]
    fn local_pack_install_copies_phonton_records() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let pack = tmp.path().join("pack");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(pack.join(".phonton/skills/review")).unwrap();
        std::fs::write(
            pack.join(".phonton/skills/review/skill.toml"),
            r#"[skill]
id = "review"
entry = "SKILL.md"
"#,
        )
        .unwrap();
        std::fs::write(
            pack.join(".phonton/skills/review/SKILL.md"),
            "Review verified output.",
        )
        .unwrap();
        let opts = InstallOptions {
            source: pack.display().to_string(),
            scope: InstallScope::Workspace,
            git_ref: None,
            force: false,
            dry_run: false,
            json: false,
        };

        let report = install_pack(&workspace, &workspace.join(".phonton"), &opts).unwrap();
        assert_eq!(report.source_kind, "phonton-pack");
        assert!(workspace.join(".phonton/skills/review/skill.toml").exists());
    }

    #[test]
    fn parse_install_accepts_scope_and_ref() {
        let args = vec![
            "phonton-dev/phonton-review-gate-extension".into(),
            "--scope".into(),
            "user".into(),
            "--ref=v0.1.0".into(),
            "--dry-run".into(),
        ];
        let opts = parse_install_options(&args).unwrap();
        assert_eq!(opts.scope, InstallScope::User);
        assert_eq!(opts.git_ref.as_deref(), Some("v0.1.0"));
        assert!(opts.dry_run);
    }
}
