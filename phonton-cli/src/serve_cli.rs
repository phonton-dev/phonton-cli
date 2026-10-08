//! JSON-RPC sidecar API for Phonton Desktop (`phonton serve`).
//!
//! Binds a local HTTP server on `127.0.0.1` and exposes methods the desktop
//! shell uses instead of scraping Ratatui output.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use phonton_types::{CostReceipt, EventRecord, GlobalState, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, watch, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::{
    doctor, execute_headless_goal, plan_preview, review, trust, HeadlessGoalHooks,
    HeadlessGoalOptions, HeadlessGoalResult,
};

const DEFAULT_PORT: u16 = 47831;

#[derive(Clone)]
struct AppState {
    runs: Arc<RwLock<HashMap<String, GoalSession>>>,
}

#[derive(Clone)]
struct GoalSession {
    task_id: TaskId,
    state_rx: watch::Receiver<GlobalState>,
    event_tx: broadcast::Sender<EventRecord>,
    done: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

pub async fn run(args: &[String]) -> Result<i32> {
    let mut port = DEFAULT_PORT;
    let mut host = "127.0.0.1".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!(
                    "Usage: phonton serve [--host <addr>] [--port <n>]\n\n\
                     Local JSON-RPC sidecar for Phonton Desktop.\n\
                     POST http://127.0.0.1:{port}/rpc\n\
                     GET  http://127.0.0.1:{port}/events/<task_id> (SSE)\n\n\
                     Methods: ping, plan.preview, doctor.run, review.get, goal.start, goal.status, goal.active,
                     tasks.list, tasks.get, workspace.info, config.get, config.save, trust.list,
                     trust.grant, extensions.list, extensions.read, extensions.write, extensions.validate"
                );
                return Ok(0);
            }
            "--port" => {
                i += 1;
                port = args
                    .get(i)
                    .ok_or_else(|| anyhow!("--port requires a value"))?
                    .parse()
                    .context("invalid --port")?;
            }
            "--host" => {
                i += 1;
                host = args
                    .get(i)
                    .ok_or_else(|| anyhow!("--host requires a value"))?
                    .clone();
            }
            other => return Err(anyhow!("unknown serve option `{other}`")),
        }
        i += 1;
    }

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .context("invalid bind address")?;
    if !addr.ip().is_loopback() {
        return Err(anyhow!(
            "Desktop sidecar must bind to loopback; remote access is not authenticated"
        ));
    }
    let state = AppState {
        runs: Arc::new(RwLock::new(HashMap::new())),
    };
    let app = Router::new()
        .route("/rpc", post(handle_rpc))
        .route("/events/:task_id", get(handle_events_sse))
        .route("/health", get(|| async { "ok" }))
        .layer(middleware::from_fn(add_cors))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("phonton serve: listening on http://{addr}/rpc");
    axum::serve(listener, app).await?;
    Ok(0)
}

async fn handle_rpc(
    State(state): State<AppState>,
    Json(req): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    let id = req.id.clone();
    let method = req.method.clone();
    let params = req.params.clone();
    // Model operations outlive the request. Keep them on the server runtime;
    // dropping a per-request runtime would cancel a newly spawned download.
    let rpc_result = if method.starts_with("models.") || method.starts_with("local.run.") {
        if method.starts_with("models.") {
            crate::models_cli::rpc(&method, params).await
        } else {
            crate::local_goal_cli::rpc(&method, params).await
        }
    } else {
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("phonton serve rpc runtime");
            rt.block_on(dispatch_rpc(state, &method, params))
        })
        .await
        .unwrap_or_else(|e| Err(anyhow!("rpc worker panicked: {e}")))
    };

    match rpc_result {
        Ok(result) => Json(JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }),
        Err(e) => Json(JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32000,
                message: e.to_string(),
            }),
        }),
    }
}

async fn dispatch_rpc(state: AppState, method: &str, params: Value) -> Result<Value> {
    match method {
        method if method.starts_with("models.") => crate::models_cli::rpc(method, params).await,
        "ping" => Ok(serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "handoff_schema": phonton_types::HANDOFF_PACKET_SCHEMA_VERSION,
            "local_models_schema": 2,
            "local_model_operation_schema": 1,
            "local_catalog_snapshot_schema": 2,
            "local_endpoint_schema": 1,
            "local_storage_schema": 2,
            "local_run_schema": 2,
            "local_creation_schema": 1,
        })),
        "record.read" => Ok(serde_json::to_value(crate::record::load())?),
        "plan.preview" => {
            let goal = params
                .get("goal")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("plan.preview requires params.goal"))?;
            let use_memory = params
                .get("use_memory")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let no_tests = params
                .get("no_tests")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let report = plan_preview::build_plan_for_goal(goal, use_memory, no_tests).await?;
            Ok(serde_json::to_value(report)?)
        }
        "doctor.run" => {
            let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
            let with_provider = params
                .get("provider")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let args = if with_provider {
                vec!["--json".into(), "--provider".into()]
            } else {
                vec!["--json".into()]
            };
            let opts = doctor::parse_options(&args)?;
            let report = doctor::build_report(&workspace, opts).await;
            Ok(serde_json::to_value(report)?)
        }
        "review.get" => {
            let task_ref = params.get("task_id").and_then(|v| v.as_str());
            let report = review::fetch_report(task_ref).await?;
            Ok(serde_json::to_value(report)?)
        }
        "goal.start" => goal_start(state, params).await,
        "goal.status" => goal_status(state, params).await,
        "goal.active" => {
            let runs = state.runs.read().await;
            let task_ids = active_goal_task_ids(&runs);
            Ok(serde_json::json!({ "running": !task_ids.is_empty(), "task_ids": task_ids }))
        }
        "tasks.list" => crate::serve_desktop::tasks_list(params).await,
        "tasks.get" => crate::serve_desktop::tasks_get(params).await,
        "workspace.info" => Ok(crate::serve_desktop::workspace_info()?),
        "config.get" => crate::serve_desktop::config_get().await,
        "config.path" => crate::serve_desktop::config_path().await,
        "config.save" => crate::serve_desktop::config_save(params).await,
        "trust.list" => Ok(crate::serve_desktop::trust_list()?),
        "trust.grant" => Ok(crate::serve_desktop::trust_grant(params)?),
        "extensions.list" => Ok(crate::serve_desktop::extensions_list()?),
        "extensions.read" => Ok(crate::serve_desktop::extensions_read(params)?),
        "extensions.write" => Ok(crate::serve_desktop::extensions_write(params)?),
        "extensions.validate" => crate::serve_desktop::extensions_validate().await,
        other => Err(anyhow!("unknown method `{other}`")),
    }
}

async fn goal_start(state: AppState, params: Value) -> Result<Value> {
    let goal = params
        .get("goal")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("goal.start requires params.goal"))?;
    if goal.trim().is_empty() || goal.len() > 64 * 1024 {
        return Err(anyhow!("Goal must contain text and fit within 64 KiB"));
    }
    let direct_task = params
        .get("task")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let timeout_seconds = params
        .get("timeout_seconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(900);
    if !(1..=3600).contains(&timeout_seconds) {
        return Err(anyhow!("Goal timeout must be between 1 and 3600 seconds"));
    }
    let workspace = std::env::current_dir().context("Could not resolve Desktop workspace")?;
    if !trust::is_trusted(&workspace) {
        return Err(anyhow!(
            "Trust this project in Phonton before running a goal"
        ));
    }

    let task_id = TaskId::new();
    let (state_tx, state_rx) = watch::channel(GlobalState {
        task_status: phonton_types::TaskStatus::Queued,
        goal_contract: None,
        plan_graph: None,
        index_backend: None,
        handoff_packet: None,
        active_workers: Vec::new(),
        tokens_used: 0,
        tokens_budget: None,
        estimated_naive_tokens: 0,
        checkpoints: Vec::new(),
        resume_checkpoint: None,
        cost_receipt: CostReceipt::default(),
    });
    let (event_tx, _) = broadcast::channel::<EventRecord>(2048);
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let session = GoalSession {
        task_id,
        state_rx: state_rx.clone(),
        event_tx: event_tx.clone(),
        done: Arc::clone(&done),
    };
    state
        .runs
        .write()
        .await
        .insert(task_id.to_string(), session);

    let goal_text = goal.trim().to_string();
    let display_text = summarize_goal_display(&goal_text);
    let opts = HeadlessGoalOptions {
        goal_text,
        display_text,
        json: true,
        yes: false,
        host_checks_approved: false,
        direct_task,
        timeout_seconds,
        resume_task_id: None,
    };
    let failure_tx = state_tx.clone();
    let hooks = HeadlessGoalHooks {
        fixed_task_id: Some(task_id),
        state_tx: Some(state_tx),
        event_tx: Some(event_tx),
        skip_trust_prompt: true,
    };

    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(execute_headless_goal(opts, hooks)));
        finish_goal_session(task_id, result, &failure_tx, &done);
    });

    Ok(serde_json::json!({
        "task_id": task_id.to_string(),
        "status": "started",
    }))
}

fn finish_goal_session(
    task_id: TaskId,
    result: Result<HeadlessGoalResult>,
    state_tx: &watch::Sender<GlobalState>,
    done: &std::sync::atomic::AtomicBool,
) {
    match result {
        Ok(outcome) if outcome.task_id == task_id => {
            state_tx.send_modify(|state| *state = outcome.final_state);
        }
        Ok(_) => {
            state_tx.send_modify(|state| {
                state.task_status = phonton_types::TaskStatus::Failed {
                    reason: "Goal returned a different task identity".into(),
                    failed_subtask: None,
                };
            });
        }
        Err(error) => {
            state_tx.send_modify(|state| {
                state.task_status = phonton_types::TaskStatus::Failed {
                    reason: error.to_string(),
                    failed_subtask: None,
                };
            });
        }
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
}

async fn goal_status(state: AppState, params: Value) -> Result<Value> {
    let task_id = params
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("goal.status requires params.task_id"))?;
    let runs = state.runs.read().await;
    let Some(session) = runs.get(task_id) else {
        return Err(anyhow!("unknown task_id `{task_id}`"));
    };
    let global = session.state_rx.borrow().clone();
    Ok(serde_json::json!({
        "task_id": session.task_id.to_string(),
        "done": session.done.load(std::sync::atomic::Ordering::SeqCst),
        "state": global,
    }))
}

fn active_goal_task_ids(runs: &HashMap<String, GoalSession>) -> Vec<String> {
    let mut ids: Vec<String> = runs
        .iter()
        .filter(|(_, session)| !session.done.load(std::sync::atomic::Ordering::SeqCst))
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    ids
}

async fn handle_events_sse(
    State(state): State<AppState>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> Response {
    let event_tx = {
        let runs = state.runs.read().await;
        match runs.get(&task_id) {
            Some(session) => session.event_tx.clone(),
            None => return (StatusCode::NOT_FOUND, "unknown task_id").into_response(),
        }
    };

    let stream = BroadcastStream::new(event_tx.subscribe()).filter_map(|item| match item {
        Ok(rec) => {
            let payload = serde_json::to_string(&rec).ok()?;
            Some(Ok::<Event, Infallible>(Event::default().data(payload)))
        }
        Err(_) => None,
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn add_cors(request: Request<Body>, next: Next) -> Response {
    // Protect all methods, including non-browser callers with a rebound Host.
    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(local_rpc_host);
    let origin = request.headers().get(header::ORIGIN).cloned();
    let origin_ok = origin
        .as_ref()
        .map(|v| v.to_str().ok().is_some_and(local_rpc_origin))
        .unwrap_or(true);
    if !host_ok || !origin_ok {
        return (StatusCode::FORBIDDEN, "Untrusted sidecar host or origin").into_response();
    }
    if request.method() == Method::OPTIONS {
        let mut response = Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type")
            .body(Body::empty())
            .expect("cors preflight response");
        if let Some(origin) = origin {
            response
                .headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        }
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Origin"));
        return response;
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if let Some(origin) = origin {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    headers.insert(header::VARY, HeaderValue::from_static("Origin"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type"),
    );
    response
}

fn local_rpc_origin(origin: &str) -> bool {
    matches!(
        origin,
        "tauri://localhost"
            | "http://tauri.localhost"
            | "https://tauri.localhost"
            | "http://localhost:1420"
            | "http://127.0.0.1:1420"
    )
}

fn local_rpc_host(host: &str) -> bool {
    host.parse::<SocketAddr>()
        .is_ok_and(|addr| addr.ip().is_loopback())
        || host
            .strip_prefix("localhost:")
            .is_some_and(|port| port.parse::<u16>().is_ok())
}

fn summarize_goal_display(text: &str) -> String {
    let first_line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("goal");
    if first_line.chars().count() > 96 {
        format!("{}…", first_line.chars().take(95).collect::<String>())
    } else {
        first_line.to_string()
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    #[test]
    fn arbitrary_browser_origins_are_refused() {
        for origin in [
            "https://attacker.test",
            "http://localhost.evil:1420",
            "null",
            "http://127.0.0.1:9999",
        ] {
            assert!(!local_rpc_origin(origin));
        }
        assert!(local_rpc_origin("http://localhost:1420"));
        assert!(local_rpc_origin("http://tauri.localhost"));
    }
    #[test]
    fn dns_rebinding_hosts_are_refused() {
        assert!(!local_rpc_host("attacker.test:47831"));
        assert!(!local_rpc_host("127.0.0.1.evil:47831"));
        assert!(local_rpc_host("127.0.0.1:47831"));
        assert!(local_rpc_host("[::1]:47831"));
    }
    #[test]
    fn unicode_goal_display_truncates_on_character_boundaries() {
        let goal = format!("{}🙂 more text", "a".repeat(94));
        let display = summarize_goal_display(&goal);
        assert!(display.ends_with('…'));
        assert!(display.contains('🙂'));
    }
    #[test]
    fn early_headless_failure_is_visible_before_done() {
        let task_id = TaskId::new();
        let initial = GlobalState {
            task_status: phonton_types::TaskStatus::Queued,
            goal_contract: None,
            plan_graph: None,
            index_backend: None,
            handoff_packet: None,
            active_workers: Vec::new(),
            tokens_used: 0,
            tokens_budget: None,
            estimated_naive_tokens: 0,
            checkpoints: Vec::new(),
            resume_checkpoint: None,
            cost_receipt: CostReceipt::default(),
        };
        let (tx, rx) = watch::channel(initial.clone());
        let done = std::sync::atomic::AtomicBool::new(false);
        let mut failed = initial;
        failed.task_status = phonton_types::TaskStatus::Failed {
            reason: "planning failed".into(),
            failed_subtask: None,
        };
        finish_goal_session(
            task_id,
            Ok(HeadlessGoalResult {
                task_id,
                final_state: failed,
                exit_code: 1,
            }),
            &tx,
            &done,
        );
        assert!(done.load(std::sync::atomic::Ordering::SeqCst));
        assert!(matches!(
            &rx.borrow().task_status,
            phonton_types::TaskStatus::Failed { .. }
        ));
    }
    #[test]
    fn active_goal_inventory_survives_a_disconnected_desktop_view() {
        let task_id = TaskId::new();
        let (tx, rx) = watch::channel(GlobalState {
            task_status: phonton_types::TaskStatus::Queued,
            goal_contract: None,
            plan_graph: None,
            index_backend: None,
            handoff_packet: None,
            active_workers: Vec::new(),
            tokens_used: 0,
            tokens_budget: None,
            estimated_naive_tokens: 0,
            checkpoints: Vec::new(),
            resume_checkpoint: None,
            cost_receipt: CostReceipt::default(),
        });
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let session = GoalSession {
            task_id,
            state_rx: rx,
            event_tx: broadcast::channel(4).0,
            done: Arc::clone(&done),
        };
        let mut runs = HashMap::new();
        runs.insert(task_id.to_string(), session);
        let state = AppState {
            runs: Arc::new(RwLock::new(runs)),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let active = runtime
            .block_on(dispatch_rpc(
                state.clone(),
                "goal.active",
                serde_json::json!({}),
            ))
            .unwrap();
        assert_eq!(active["running"], true);
        assert_eq!(active["task_ids"], serde_json::json!([task_id.to_string()]));
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        let finished = runtime
            .block_on(dispatch_rpc(state, "goal.active", serde_json::json!({})))
            .unwrap();
        assert_eq!(finished["running"], false);
        assert_eq!(finished["task_ids"], serde_json::json!([]));
        drop(tx);
    }
    #[test]
    fn desktop_cannot_trust_a_different_workspace() {
        let unrelated = tempfile::tempdir().unwrap();
        let result = crate::serve_desktop::trust_grant(serde_json::json!({
            "path": unrelated.path().display().to_string(),
        }));
        assert!(result.is_err());
    }
}
