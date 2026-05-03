//! Minimal MCP runtime for Phonton.
//!
//! The runtime is intentionally lazy: configured servers are not started
//! when extension config is loaded. A server starts only when a caller lists
//! tools or invokes one tool, and every operation is checked against the
//! declared Phonton permissions before the JSON-RPC request is sent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use phonton_sandbox::{ExecutionGuard, GuardDecision, ToolCall};
use phonton_types::{
    EventRecord, ExtensionId, McpServerDefinition, McpTransport, OrchestratorEvent, Permission,
    TaskId, TrustLevel,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, Mutex};

/// Latest MCP protocol revision supported by this client.
///
/// The protocol is negotiated during initialize; this value is the client
/// side's preferred version.
pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

/// One tool exposed by one MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpTool {
    /// Owning server id.
    pub server_id: ExtensionId,
    /// Programmatic tool name.
    pub name: String,
    /// Optional human-friendly title.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional description supplied by the server.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON schema for arguments.
    #[serde(default)]
    pub input_schema: Value,
}

/// Result returned by `tools/call`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpCallResult {
    /// Raw content blocks returned by the server.
    #[serde(default)]
    pub content: Vec<Value>,
    /// Structured output when the server supports it.
    #[serde(default)]
    pub structured_content: Option<Value>,
    /// True when the tool reports a domain-level error.
    #[serde(default)]
    pub is_error: bool,
}

/// Approval request created when an MCP operation needs user consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpApprovalRequest {
    /// Server id.
    pub server_id: ExtensionId,
    /// Tool name, or `tools/list` / `server/start` for protocol setup.
    pub tool_name: String,
    /// Permissions declared by the server.
    pub permissions: Vec<Permission>,
    /// Human-readable reason for asking.
    pub reason: String,
}

/// Approval outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpApprovalDecision {
    /// Proceed with the operation.
    Approved,
    /// Deny the operation.
    Denied,
}

/// Approval bridge used by the CLI/TUI.
#[async_trait]
pub trait McpApprover: Send + Sync {
    /// Decide whether an approval-required MCP operation may proceed.
    async fn approve(&self, request: McpApprovalRequest) -> McpApprovalDecision;
}

/// Conservative default: deny approval-required operations.
#[derive(Debug, Default)]
pub struct DenyByDefaultApprover;

#[async_trait]
impl McpApprover for DenyByDefaultApprover {
    async fn approve(&self, _request: McpApprovalRequest) -> McpApprovalDecision {
        McpApprovalDecision::Denied
    }
}

/// Explicit approval policy used by one-shot CLI commands and tests.
#[derive(Debug, Default)]
pub struct ExplicitApproveAll;

#[async_trait]
impl McpApprover for ExplicitApproveAll {
    async fn approve(&self, _request: McpApprovalRequest) -> McpApprovalDecision {
        McpApprovalDecision::Approved
    }
}

/// Lazy MCP runtime.
pub struct McpRuntime {
    servers: HashMap<ExtensionId, McpServerDefinition>,
    sessions: Mutex<HashMap<ExtensionId, McpClient>>,
    guard: ExecutionGuard,
    approver: Arc<dyn McpApprover>,
    task_id: Option<TaskId>,
    event_tx: Option<broadcast::Sender<EventRecord>>,
}

impl McpRuntime {
    /// Construct a runtime for active server definitions.
    pub fn new(servers: Vec<McpServerDefinition>, guard: ExecutionGuard) -> Self {
        let servers = servers
            .into_iter()
            .map(|server| (server.id.clone(), server))
            .collect();
        Self {
            servers,
            sessions: Mutex::new(HashMap::new()),
            guard,
            approver: Arc::new(DenyByDefaultApprover),
            task_id: None,
            event_tx: None,
        }
    }

    /// Attach an approval bridge.
    pub fn with_approver(mut self, approver: Arc<dyn McpApprover>) -> Self {
        self.approver = approver;
        self
    }

    /// Attach event output. Events are best-effort and never affect control
    /// flow.
    pub fn with_event_sink(
        mut self,
        task_id: TaskId,
        sender: broadcast::Sender<EventRecord>,
    ) -> Self {
        self.task_id = Some(task_id);
        self.event_tx = Some(sender);
        self
    }

    /// Return configured active servers without starting anything.
    pub fn servers(&self) -> Vec<&McpServerDefinition> {
        self.servers.values().collect()
    }

    /// List tools exposed by `server_id`.
    pub async fn list_tools(&self, server_id: &ExtensionId) -> Result<Vec<McpTool>> {
        let server = self.server(server_id)?;
        self.authorize_operation(server, "tools/list").await?;
        let result = self
            .request(server, "tools/list", json!({}))
            .await
            .inspect_err(|_| self.emit_completed(server, "tools/list", false))?;
        self.emit_completed(server, "tools/list", true);
        parse_tools(server, result)
    }

    /// Invoke one MCP tool.
    pub async fn call_tool(
        &self,
        server_id: &ExtensionId,
        tool_name: &str,
        arguments: Value,
    ) -> Result<McpCallResult> {
        let server = self.server(server_id)?;
        self.authorize_operation(server, tool_name).await?;
        let params = json!({
            "name": tool_name,
            "arguments": arguments,
        });
        let result = self
            .request(server, "tools/call", params)
            .await
            .inspect_err(|_| self.emit_completed(server, tool_name, false))?;
        let result = parse_call_result(result)?;
        self.emit_completed(server, tool_name, !result.is_error);
        Ok(result)
    }

    fn server(&self, server_id: &ExtensionId) -> Result<&McpServerDefinition> {
        self.servers
            .get(server_id)
            .ok_or_else(|| anyhow!("unknown MCP server `{server_id}`"))
    }

    async fn authorize_operation(
        &self,
        server: &McpServerDefinition,
        tool_name: &str,
    ) -> Result<()> {
        self.emit_requested(server, tool_name);
        match permission_decision(server) {
            PermissionDecision::Allow => {
                self.emit_approved(server, tool_name);
                Ok(())
            }
            PermissionDecision::Deny(reason) => {
                self.emit_denied(server, tool_name, &reason);
                bail!("{reason}");
            }
            PermissionDecision::Approve(reason) => {
                let request = McpApprovalRequest {
                    server_id: server.id.clone(),
                    tool_name: tool_name.to_string(),
                    permissions: server.permissions.clone(),
                    reason: reason.clone(),
                };
                match self.approver.approve(request).await {
                    McpApprovalDecision::Approved => {
                        self.emit_approved(server, tool_name);
                        Ok(())
                    }
                    McpApprovalDecision::Denied => {
                        self.emit_denied(server, tool_name, &reason);
                        bail!("approval required: {reason}");
                    }
                }
            }
        }
    }

    async fn authorize_start(&self, server: &McpServerDefinition) -> Result<()> {
        self.emit_requested(server, "server/start");
        let decision = match &server.transport {
            McpTransport::Stdio { command, args } => self.guard.evaluate(&ToolCall::Run {
                program: command.clone(),
                args: args.clone(),
            }),
            McpTransport::Http { url } => {
                self.guard.evaluate(&ToolCall::Network { url: url.clone() })
            }
        };
        match decision {
            GuardDecision::Allow => {
                self.emit_approved(server, "server/start");
                Ok(())
            }
            GuardDecision::Block { reason } => {
                self.emit_denied(server, "server/start", &reason);
                bail!("{reason}")
            }
            GuardDecision::Approve { reason } => {
                let request = McpApprovalRequest {
                    server_id: server.id.clone(),
                    tool_name: "server/start".into(),
                    permissions: server.permissions.clone(),
                    reason: reason.clone(),
                };
                match self.approver.approve(request).await {
                    McpApprovalDecision::Approved => {
                        self.emit_approved(server, "server/start");
                        Ok(())
                    }
                    McpApprovalDecision::Denied => {
                        self.emit_denied(server, "server/start", &reason);
                        bail!("approval required: {reason}")
                    }
                }
            }
        }
    }

    async fn request(
        &self,
        server: &McpServerDefinition,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let mut sessions = self.sessions.lock().await;
        if !sessions.contains_key(&server.id) {
            self.authorize_start(server).await?;
            let client =
                McpClient::connect(server, self.guard.project_root().to_path_buf()).await?;
            sessions.insert(server.id.clone(), client);
        }
        let client = sessions
            .get_mut(&server.id)
            .expect("session was inserted or already present");
        client.request(method, params).await
    }

    fn emit_requested(&self, server: &McpServerDefinition, tool_name: &str) {
        self.emit(OrchestratorEvent::McpToolRequested {
            server_id: server.id.clone(),
            tool_name: tool_name.to_string(),
            permissions: server.permissions.clone(),
        });
    }

    fn emit_approved(&self, server: &McpServerDefinition, tool_name: &str) {
        self.emit(OrchestratorEvent::McpToolApproved {
            server_id: server.id.clone(),
            tool_name: tool_name.to_string(),
        });
    }

    fn emit_denied(&self, server: &McpServerDefinition, tool_name: &str, reason: &str) {
        self.emit(OrchestratorEvent::McpToolDenied {
            server_id: server.id.clone(),
            tool_name: tool_name.to_string(),
            reason: reason.to_string(),
        });
    }

    fn emit_completed(&self, server: &McpServerDefinition, tool_name: &str, success: bool) {
        self.emit(OrchestratorEvent::McpToolCompleted {
            server_id: server.id.clone(),
            tool_name: tool_name.to_string(),
            success,
        });
    }

    fn emit(&self, event: OrchestratorEvent) {
        let (Some(task_id), Some(tx)) = (self.task_id, &self.event_tx) else {
            return;
        };
        let record = EventRecord {
            task_id,
            timestamp_ms: now_ms(),
            event,
        };
        let _ = tx.send(record);
    }
}

enum McpClient {
    Stdio(Box<StdioClient>),
    Http(HttpClient),
}

impl McpClient {
    async fn connect(server: &McpServerDefinition, project_root: PathBuf) -> Result<Self> {
        match &server.transport {
            McpTransport::Stdio { command, args } => {
                let client = StdioClient::connect(server, command, args, project_root).await?;
                Ok(Self::Stdio(Box::new(client)))
            }
            McpTransport::Http { url } => {
                let client = HttpClient::connect(url).await?;
                Ok(Self::Http(client))
            }
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        match self {
            McpClient::Stdio(client) => client.request(method, params).await,
            McpClient::Http(client) => client.request(method, params).await,
        }
    }
}

struct StdioClient {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl StdioClient {
    async fn connect(
        server: &McpServerDefinition,
        command: &str,
        args: &[String],
        project_root: PathBuf,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd.current_dir(project_root);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::null());
        cmd.kill_on_drop(true);
        cmd.env_clear();
        for key in ["PATH", "HOME", "USERPROFILE", "SYSTEMROOT"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        for key in &server.env {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting MCP server `{}`", server.id))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP server `{}` did not expose stdin", server.id))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP server `{}` did not expose stdout", server.id))?;
        let mut client = Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&mut self) -> Result<()> {
        let _ = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "phonton",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        self.notification("notifications/initialized", json!({}))
            .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(&request).await?;
        self.read_response(id).await
    }

    async fn notification(&mut self, method: &str, params: Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&notification).await
    }

    async fn write_message(&mut self, message: &Value) -> Result<()> {
        let raw = serde_json::to_string(message)?;
        self.stdin.write_all(raw.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_response(&mut self, id: u64) -> Result<Value> {
        let wanted_id = json!(id);
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).await?;
            if n == 0 {
                bail!("MCP server closed stdout before responding");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(trimmed)
                .with_context(|| format!("parsing MCP JSON-RPC message: {trimmed}"))?;

            if message.get("id") == Some(&wanted_id) {
                return response_result(message);
            }

            if message.get("method").is_some() && message.get("id").is_some() {
                self.reject_server_request(&message).await?;
            }
        }
    }

    async fn reject_server_request(&mut self, message: &Value) -> Result<()> {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": "Phonton MCP client does not support server-initiated requests yet"
            }
        });
        self.write_message(&response).await
    }
}

struct HttpClient {
    url: String,
    http: reqwest::Client,
    next_id: u64,
    protocol_version: String,
}

impl HttpClient {
    async fn connect(url: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        let mut client = Self {
            url: url.to_string(),
            http,
            next_id: 1,
            protocol_version: MCP_PROTOCOL_VERSION.into(),
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&mut self) -> Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "phonton",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        if let Some(version) = result.get("protocolVersion").and_then(Value::as_str) {
            self.protocol_version = version.to_string();
        }
        self.notification("notifications/initialized", json!({}))
            .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let message = self.post(request).await?;
        response_result(message)
    }

    async fn notification(&mut self, method: &str, params: Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let _ = self.post(notification).await?;
        Ok(())
    }

    async fn post(&self, body: Value) -> Result<Value> {
        let resp = self
            .http
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .header("MCP-Protocol-Version", &self.protocol_version)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("posting MCP request to {}", self.url))?;
        if resp.status() == reqwest::StatusCode::ACCEPTED {
            return Ok(json!({"jsonrpc": "2.0", "result": {}}));
        }
        if !resp.status().is_success() {
            bail!("MCP HTTP server returned {}", resp.status());
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp.text().await?;
        if content_type.contains("text/event-stream") {
            parse_sse_json(&text)
        } else if text.trim().is_empty() {
            Ok(json!({"jsonrpc": "2.0", "result": {}}))
        } else {
            Ok(serde_json::from_str(&text)?)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PermissionDecision {
    Allow,
    Approve(String),
    Deny(String),
}

fn permission_decision(server: &McpServerDefinition) -> PermissionDecision {
    if !server.enabled {
        return PermissionDecision::Deny(format!("MCP server `{}` is disabled", server.id));
    }
    if server.trust == TrustLevel::TextOnly && !server.permissions.is_empty() {
        return PermissionDecision::Deny(format!(
            "MCP server `{}` is text-only but declares tool permissions",
            server.id
        ));
    }
    for permission in &server.permissions {
        if !trust_allows(server.trust, *permission) {
            return PermissionDecision::Deny(format!(
                "MCP server `{}` trust {} cannot use permission {}",
                server.id, server.trust, permission
            ));
        }
    }

    let approval_permissions: Vec<Permission> = server
        .permissions
        .iter()
        .copied()
        .filter(permission_requires_approval)
        .collect();
    if approval_permissions.is_empty() {
        PermissionDecision::Allow
    } else {
        let labels = approval_permissions
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        PermissionDecision::Approve(format!(
            "MCP server `{}` requests approval-gated permission(s): {labels}",
            server.id
        ))
    }
}

fn trust_allows(trust: TrustLevel, permission: Permission) -> bool {
    match trust {
        TrustLevel::TextOnly => false,
        TrustLevel::ReadOnlyTool => matches!(
            permission,
            Permission::FsReadWorkspace | Permission::FsReadOutsideWorkspace
        ),
        TrustLevel::MutatingTool => !matches!(permission, Permission::NetworkRequest),
        TrustLevel::NetworkedTool => true,
    }
}

fn permission_requires_approval(permission: &Permission) -> bool {
    !matches!(permission, Permission::FsReadWorkspace)
}

fn parse_tools(server: &McpServerDefinition, result: Value) -> Result<Vec<McpTool>> {
    #[derive(Deserialize)]
    struct ToolList {
        #[serde(default)]
        tools: Vec<RawTool>,
    }
    #[derive(Deserialize)]
    struct RawTool {
        name: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default, rename = "inputSchema")]
        input_schema: Value,
    }

    let list: ToolList = serde_json::from_value(result)?;
    Ok(list
        .tools
        .into_iter()
        .map(|tool| McpTool {
            server_id: server.id.clone(),
            name: tool.name,
            title: tool.title,
            description: tool.description,
            input_schema: tool.input_schema,
        })
        .collect())
}

fn parse_call_result(result: Value) -> Result<McpCallResult> {
    #[derive(Deserialize)]
    struct RawResult {
        #[serde(default)]
        content: Vec<Value>,
        #[serde(default, rename = "structuredContent")]
        structured_content: Option<Value>,
        #[serde(default, rename = "isError")]
        is_error: bool,
    }
    let raw: RawResult = serde_json::from_value(result)?;
    Ok(McpCallResult {
        content: raw.content,
        structured_content: raw.structured_content,
        is_error: raw.is_error,
    })
}

fn response_result(message: Value) -> Result<Value> {
    if let Some(error) = message.get("error") {
        let msg = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("MCP JSON-RPC error");
        bail!("{msg}");
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("MCP response missing result"))
}

fn parse_sse_json(raw: &str) -> Result<Value> {
    for line in raw.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        return Ok(serde_json::from_str(data)?);
    }
    bail!("MCP SSE response did not contain a JSON data event")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{AppliesTo, ExtensionSource};

    fn server(trust: TrustLevel, permissions: Vec<Permission>) -> McpServerDefinition {
        McpServerDefinition {
            id: ExtensionId::new("test"),
            name: "Test".into(),
            source: ExtensionSource::Workspace,
            transport: McpTransport::Stdio {
                command: "node".into(),
                args: vec!["server.js".into()],
            },
            trust,
            permissions,
            applies_to: AppliesTo::default(),
            env: Vec::new(),
            enabled: true,
        }
    }

    #[test]
    fn read_workspace_permission_is_auto_allowed() {
        let s = server(TrustLevel::ReadOnlyTool, vec![Permission::FsReadWorkspace]);
        assert_eq!(permission_decision(&s), PermissionDecision::Allow);
    }

    #[test]
    fn network_permission_requires_networked_trust_and_approval() {
        let low = server(TrustLevel::MutatingTool, vec![Permission::NetworkRequest]);
        assert!(matches!(
            permission_decision(&low),
            PermissionDecision::Deny(_)
        ));

        let high = server(TrustLevel::NetworkedTool, vec![Permission::NetworkRequest]);
        assert!(matches!(
            permission_decision(&high),
            PermissionDecision::Approve(_)
        ));
    }

    #[test]
    fn text_only_server_cannot_request_tool_permissions() {
        let s = server(TrustLevel::TextOnly, vec![Permission::FsReadWorkspace]);
        assert!(matches!(
            permission_decision(&s),
            PermissionDecision::Deny(_)
        ));
    }

    #[test]
    fn parses_tools_list_result() {
        let s = server(TrustLevel::ReadOnlyTool, vec![Permission::FsReadWorkspace]);
        let tools = parse_tools(
            &s,
            json!({
                "tools": [{
                    "name": "search",
                    "title": "Search",
                    "description": "Search docs",
                    "inputSchema": { "type": "object" }
                }]
            }),
        )
        .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].input_schema["type"], "object");
    }

    #[test]
    fn parses_sse_json_data_event() {
        let value = parse_sse_json("event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{}}\n\n")
            .unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
    }
}
