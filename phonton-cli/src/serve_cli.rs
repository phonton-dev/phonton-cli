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
use phonton_types::{EventRecord, GlobalState, TaskId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, watch, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::{
    doctor, execute_headless_goal, plan_preview, review, HeadlessGoalHooks, HeadlessGoalOptions,
};

const DEFAULT_PORT: u16 = 47831;

#[derive(Clone)]
struct AppState {
    runs: Arc<RwLock<HashMap<String, GoalSession>>>,
}

#[derive(Clone)]
struct GoalSession {
    #[allow(dead_code)]
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
                     Methods: ping, plan.preview, doctor.run, review.get, goal.start, goal.status,
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
    let rpc_result = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("phonton serve rpc runtime");
        rt.block_on(dispatch_rpc(state, &method, params))
    })
    .await
    .unwrap_or_else(|e| Err(anyhow!("rpc worker panicked: {e}")));

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
        "ping" => Ok(serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "handoff_schema": phonton_types::HANDOFF_PACKET_SCHEMA_VERSION,
        })),
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
    let direct_task = params
        .get("task")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let timeout_seconds = params
        .get("timeout_seconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(900);

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
        yes: true,
        direct_task,
        timeout_seconds,
        resume_task_id: None,
    };
    let hooks = HeadlessGoalHooks {
        fixed_task_id: Some(task_id),
        state_tx: Some(state_tx),
        event_tx: Some(event_tx),
        skip_trust_prompt: true,
    };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("phonton serve goal runtime");
        let _ = rt.block_on(execute_headless_goal(opts, hooks));
        done.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    Ok(serde_json::json!({
        "task_id": task_id.to_string(),
        "status": "started",
    }))
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
        "task_id": task_id,
        "done": session.done.load(std::sync::atomic::Ordering::SeqCst),
        "state": global,
    }))
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
    if request.method() == Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS")
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type")
            .body(Body::empty())
            .expect("cors preflight response");
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
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

fn summarize_goal_display(text: &str) -> String {
    let first_line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("goal");
    if first_line.len() > 96 {
        format!("{}…", &first_line[..95])
    } else {
        first_line.to_string()
    }
}
