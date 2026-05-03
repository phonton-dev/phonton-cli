//! `phonton mcp` commands.
//!
//! Listing configured servers is read-only and never starts a server. Listing
//! tools or calling a tool uses `phonton-mcp`, so process/network startup and
//! server permissions go through the approval bridge.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use phonton_extensions::{load_extensions, ExtensionLoadOptions};
use phonton_mcp::{DenyByDefaultApprover, ExplicitApproveAll, McpApprover, McpRuntime};
use phonton_sandbox::ExecutionGuard;
use phonton_types::{ExtensionId, ExtensionSource, McpServerDefinition, McpTransport, Permission};
use serde::Serialize;
use serde_json::Value;

use crate::trust;

#[derive(Debug, Clone, Copy, Default)]
struct Options {
    json: bool,
    yes: bool,
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
        "list" => list_servers(workspace, &args[1..]).await,
        "tools" => list_tools(workspace, &args[1..]).await,
        "call" => call_tool(workspace, &args[1..]).await,
        other => {
            eprintln!("phonton mcp: unknown subcommand `{other}`");
            print_usage();
            Ok(2)
        }
    }
}

async fn list_servers(workspace: &Path, args: &[String]) -> Result<i32> {
    let opts = parse_options(args)?;
    let set = load_extensions(&ExtensionLoadOptions::for_workspace(workspace));
    if opts.json {
        let rows: Vec<ServerRow> = set.mcp_servers.iter().map(ServerRow::from).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }

    if set.mcp_servers.is_empty() {
        println!("No active MCP servers configured.");
        return Ok(0);
    }

    println!("Active MCP servers");
    for server in &set.mcp_servers {
        println!(
            "- {} ({}) [{}] trust={} permissions={}",
            server.id,
            server.name,
            render_transport(&server.transport),
            server.trust,
            render_permissions(&server.permissions)
        );
    }
    Ok(0)
}

async fn list_tools(workspace: &Path, args: &[String]) -> Result<i32> {
    let (opts, positional) = parse_options_and_positionals(args)?;
    let Some(server_id) = positional.first() else {
        eprintln!("phonton mcp tools: missing <server-id>");
        return Ok(2);
    };

    let runtime = build_runtime(workspace, opts)?;
    let server_id = ExtensionId::new(server_id.clone());
    let tools = match runtime.list_tools(&server_id).await {
        Ok(tools) => tools,
        Err(e) => {
            eprintln!("phonton mcp tools: {e}");
            if !opts.yes {
                eprintln!("      pass --yes to approve this one MCP operation");
            }
            return Ok(1);
        }
    };

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&tools)?);
    } else if tools.is_empty() {
        println!("No tools reported by {server_id}.");
    } else {
        println!("Tools from {server_id}");
        for tool in tools {
            let label = tool.title.as_deref().unwrap_or(&tool.name);
            let detail = tool.description.unwrap_or_default();
            if detail.is_empty() {
                println!("- {} ({})", tool.name, label);
            } else {
                println!("- {} ({}) - {}", tool.name, label, detail);
            }
        }
    }
    Ok(0)
}

async fn call_tool(workspace: &Path, args: &[String]) -> Result<i32> {
    let (opts, positional) = parse_options_and_positionals(args)?;
    if positional.len() < 2 {
        eprintln!("phonton mcp call: missing <server-id> <tool-name> [json-args]");
        return Ok(2);
    }
    let server_id = ExtensionId::new(positional[0].clone());
    let tool_name = positional[1].clone();
    let arguments = if let Some(raw) = positional.get(2) {
        serde_json::from_str::<Value>(raw)
            .map_err(|e| anyhow!("json-args must be a JSON object/value: {e}"))?
    } else {
        Value::Object(Default::default())
    };

    let runtime = build_runtime(workspace, opts)?;
    let result = match runtime.call_tool(&server_id, &tool_name, arguments).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("phonton mcp call: {e}");
            if !opts.yes {
                eprintln!("      pass --yes to approve this one MCP operation");
            }
            return Ok(1);
        }
    };

    let is_error = result.is_error;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if let Some(structured) = &result.structured_content {
        println!("{}", serde_json::to_string_pretty(&structured)?);
    } else {
        for block in &result.content {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                println!("{text}");
            } else {
                println!("{}", serde_json::to_string_pretty(&block)?);
            }
        }
    }
    Ok(if is_error { 1 } else { 0 })
}

fn build_runtime(workspace: &Path, opts: Options) -> Result<McpRuntime> {
    let set = load_extensions(&ExtensionLoadOptions::for_workspace(workspace));
    let workspace_servers: Vec<&McpServerDefinition> = set
        .mcp_servers
        .iter()
        .filter(|server| server.source == ExtensionSource::Workspace)
        .collect();
    if !workspace_servers.is_empty() && !trust::is_trusted(workspace) {
        return Err(anyhow!(
            "workspace MCP config cannot be used until this workspace is trusted"
        ));
    }

    let approver: Arc<dyn McpApprover> = if opts.yes {
        Arc::new(ExplicitApproveAll)
    } else {
        Arc::new(DenyByDefaultApprover)
    };
    Ok(McpRuntime::new(
        set.mcp_servers,
        ExecutionGuard::new(workspace.to_path_buf()),
    )
    .with_approver(approver))
}

fn parse_options(args: &[String]) -> Result<Options> {
    let (opts, positional) = parse_options_and_positionals(args)?;
    if let Some(extra) = positional.first() {
        return Err(anyhow!("unexpected argument `{extra}`"));
    }
    Ok(opts)
}

fn parse_options_and_positionals(args: &[String]) -> Result<(Options, Vec<String>)> {
    let mut opts = Options::default();
    let mut positional = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--json" => opts.json = true,
            "--yes" | "-y" => opts.yes = true,
            other if other.starts_with('-') => return Err(anyhow!("unknown option `{other}`")),
            _ => positional.push(arg.clone()),
        }
    }
    Ok((opts, positional))
}

#[derive(Debug, Serialize)]
struct ServerRow {
    id: String,
    name: String,
    source: String,
    transport: String,
    trust: String,
    permissions: Vec<String>,
}

impl From<&McpServerDefinition> for ServerRow {
    fn from(server: &McpServerDefinition) -> Self {
        Self {
            id: server.id.to_string(),
            name: server.name.clone(),
            source: server.source.to_string(),
            transport: render_transport(&server.transport),
            trust: server.trust.to_string(),
            permissions: server.permissions.iter().map(ToString::to_string).collect(),
        }
    }
}

fn render_transport(transport: &McpTransport) -> String {
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

fn print_usage() {
    println!(
        "usage:\n  \
         phonton mcp list [--json]\n  \
         phonton mcp tools <server-id> [--json] [--yes]\n  \
         phonton mcp call <server-id> <tool-name> [json-args] [--json] [--yes]\n\n  \
         `list` never starts servers. `tools` and `call` start the target server lazily.\n  \
         Use --yes to approve that single MCP operation."
    );
}
