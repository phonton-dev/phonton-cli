//! JSON-RPC handlers for Phonton Desktop (config, tasks, workspace, extensions, trust).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::config::{self, Config};
use crate::store_util;
use crate::trust;

fn mask_config(cfg: Config) -> Value {
    let mut value = serde_json::to_value(&cfg).unwrap_or(json!({}));
    if let Some(provider) = value.get_mut("provider").and_then(|p| p.as_object_mut()) {
        if provider.get("api_key").and_then(|v| v.as_str()).is_some() {
            provider.insert("api_key".into(), json!(""));
            provider.insert("has_api_key".into(), json!(true));
        }
        if let Some(keys) = provider.get_mut("keys").and_then(|k| k.as_object_mut()) {
            let names: Vec<String> = keys.keys().cloned().collect();
            keys.clear();
            for name in names {
                keys.insert(name, "***".into());
            }
        }
    }
    value
}

pub async fn config_get() -> Result<Value> {
    let cfg = config::load()?;
    Ok(json!({
        "path": config::config_path().map(|p| p.display().to_string()),
        "config": mask_config(cfg),
    }))
}

pub async fn config_path() -> Result<Value> {
    Ok(json!({
        "path": config::config_path().map(|p| p.display().to_string()),
    }))
}

pub async fn config_save(params: Value) -> Result<Value> {
    let patch = params
        .get("config")
        .ok_or_else(|| anyhow!("config.save requires params.config"))?;
    let mut cfg = config::load()?;
    merge_config_patch(&mut cfg, patch)?;
    config::save(&cfg)?;
    Ok(json!({ "ok": true, "path": config::config_path().map(|p| p.display().to_string()) }))
}

fn merge_config_patch(cfg: &mut Config, patch: &Value) -> Result<()> {
    if let Some(provider) = patch.get("provider") {
        if let Some(name) = provider.get("name").and_then(|v| v.as_str()) {
            cfg.provider.name = name.to_string();
        }
        if let Some(model) = provider.get("model") {
            cfg.provider.model = model.as_str().map(|s| s.to_string());
        }
        if let Some(key) = provider.get("api_key").and_then(|v| v.as_str()) {
            if !key.is_empty() && key != "***" {
                cfg.provider.api_key = Some(key.to_string());
            }
        }
        if let Some(url) = provider.get("base_url") {
            cfg.provider.base_url = url.as_str().map(|s| s.to_string());
        }
        if let Some(id) = provider.get("account_id") {
            cfg.provider.account_id = id.as_str().map(|s| s.to_string());
        }
        if let Some(keys) = provider.get("keys").and_then(|v| v.as_object()) {
            for (k, v) in keys {
                if let Some(s) = v.as_str() {
                    if !s.is_empty() && s != "***" {
                        cfg.provider.keys.insert(k.clone(), s.to_string());
                    }
                }
            }
        }
    }
    if let Some(budget) = patch.get("budget") {
        if let Some(v) = budget.get("max_tokens") {
            cfg.budget.max_tokens = v.as_u64();
        }
        if let Some(v) = budget.get("max_usd_cents") {
            cfg.budget.max_usd_cents = v.as_u64();
        }
    }
    if let Some(index) = patch.get("index") {
        if let Some(v) = index.get("backend").and_then(|v| v.as_str()) {
            cfg.index.backend = v.to_string();
        }
        if let Some(v) = index.get("qdrant_url") {
            cfg.index.qdrant_url = v.as_str().map(|s| s.to_string());
        }
        if let Some(v) = index.get("qdrant_collection") {
            cfg.index.qdrant_collection = v.as_str().map(|s| s.to_string());
        }
    }
    if let Some(perms) = patch.get("permissions") {
        if let Some(v) = perms.get("mode") {
            cfg.permissions.mode = v.as_str().map(|s| s.to_string());
        }
    }
    if let Some(general) = patch.get("general") {
        if let Some(v) = general.get("enable_auto_update").and_then(|v| v.as_bool()) {
            cfg.general.enable_auto_update = v;
        }
    }
    Ok(())
}

pub async fn tasks_list(params: Value) -> Result<Value> {
    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
    let store = store_util::open_persistent_store()?;
    let tasks = store.list_tasks(limit).await?;
    let items: Vec<Value> = tasks
        .into_iter()
        .map(|t| {
            json!({
                "task_id": t.id.to_string(),
                "goal_text": t.goal_text,
                "status": t.status,
                "created_at": t.created_at,
                "total_tokens": t.total_tokens,
            })
        })
        .collect();
    Ok(json!({ "tasks": items }))
}

pub async fn tasks_get(params: Value) -> Result<Value> {
    let task_id = params
        .get("task_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("tasks.get requires params.task_id"))?;
    let store = store_util::open_persistent_store()?;
    let tasks = store.list_tasks(200).await?;
    let Some(task) = tasks.into_iter().find(|t| t.id.to_string() == task_id) else {
        return Err(anyhow!("unknown task_id `{task_id}`"));
    };
    let events = store.list_events(task.id, 50)?;
    let event_lines: Vec<Value> = events
        .into_iter()
        .map(|e| {
            json!({
                "kind": e.kind(),
                "timestamp_ms": e.timestamp_ms,
                "body": serde_json::to_value(&e.event).unwrap_or(json!(null)),
            })
        })
        .collect();
    Ok(json!({
        "task_id": task.id.to_string(),
        "goal_text": task.goal_text,
        "status": task.status,
        "created_at": task.created_at,
        "total_tokens": task.total_tokens,
        "events": event_lines,
    }))
}

pub fn workspace_info() -> Result<Value> {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let canonical = workspace.canonicalize().unwrap_or(workspace.clone());
    Ok(json!({
        "path": canonical.display().to_string(),
        "trusted": trust::is_trusted(&canonical),
        "config_path": config::config_path().map(|p| p.display().to_string()),
        "store_path": store_util::default_store_path().map(|p| p.display().to_string()),
    }))
}

pub fn trust_list() -> Result<Value> {
    let path = dirs::home_dir().map(|h| h.join(".phonton").join("trusted_workspaces.json"));
    let trusted = if let Some(p) = path {
        if let Ok(raw) = std::fs::read_to_string(&p) {
            serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|v| v.get("trusted").cloned())
                .unwrap_or(json!([]))
        } else {
            json!([])
        }
    } else {
        json!([])
    };
    Ok(json!({ "trusted": trusted }))
}

pub fn trust_grant(params: Value) -> Result<Value> {
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("trust.grant requires params.path"))?;
    let workspace = PathBuf::from(path);
    trust::record_trust(&workspace)?;
    Ok(json!({ "ok": true, "trusted": trust::is_trusted(&workspace) }))
}

fn extension_dir(scope: &str, workspace: &Path) -> Option<PathBuf> {
    match scope {
        "user" => dirs::home_dir().map(|h| h.join(".phonton")),
        "workspace" => Some(workspace.join(".phonton")),
        _ => None,
    }
}

pub fn extensions_list() -> Result<Value> {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut scopes = Vec::new();
    for scope in ["user", "workspace"] {
        let Some(dir) = extension_dir(scope, &workspace) else {
            continue;
        };
        scopes.push(json!({
            "scope": scope,
            "path": dir.display().to_string(),
            "exists": dir.is_dir(),
            "files": extension_files(&dir),
        }));
    }
    Ok(json!({ "scopes": scopes }))
}

fn extension_files(dir: &Path) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    for name in ["steering.toml", "mcp.toml", "profiles.toml"] {
        out.insert(name.to_string(), dir.join(name).is_file());
    }
    let skills = dir.join("skills");
    out.insert("skills".to_string(), skills.is_dir());
    out
}

pub fn extensions_read(params: Value) -> Result<Value> {
    let scope = params
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("user");
    let file = params
        .get("file")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("extensions.read requires params.file"))?;
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dir = extension_dir(scope, &workspace).ok_or_else(|| anyhow!("invalid scope"))?;
    let path = dir.join(file);
    if !path.is_file() {
        return Ok(json!({ "path": path.display().to_string(), "content": "", "exists": false }));
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(json!({
        "path": path.display().to_string(),
        "content": content,
        "exists": true,
    }))
}

pub fn extensions_write(params: Value) -> Result<Value> {
    let scope = params
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("user");
    let file = params
        .get("file")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("extensions.write requires params.file"))?;
    let content = params
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("extensions.write requires params.content"))?;
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dir = extension_dir(scope, &workspace).ok_or_else(|| anyhow!("invalid scope"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(file);
    std::fs::write(&path, content)?;
    Ok(json!({ "ok": true, "path": path.display().to_string() }))
}

pub async fn extensions_validate() -> Result<Value> {
    let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let options = phonton_extensions::ExtensionLoadOptions::for_workspace(&workspace);
    let set = phonton_extensions::load_extensions(&options);
    Ok(json!({
        "ok": !set.has_errors(),
        "steering_rules": set.steering.len(),
        "mcp_servers": set.mcp_servers.len(),
        "profiles": set.profiles.len(),
        "skills": set.skills.len(),
        "diagnostics": set.diagnostics.len(),
    }))
}
