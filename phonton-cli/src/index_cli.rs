//! `phonton index watch` — incremental semantic re-indexing.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use phonton_index::{index_workspace_using_embedder, watch_and_reindex, Embedder};
use tokio::sync::Mutex;

pub async fn run(args: &[String]) -> Result<i32> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        println!(
            "Usage:\n  phonton index watch [--json]\n\nWatches the current workspace and incrementally updates the local semantic index."
        );
        return Ok(0);
    }
    if args[0] != "watch" {
        eprintln!("phonton index: unknown subcommand `{}`", args[0]);
        return Ok(2);
    }

    let workspace = std::env::current_dir().context("resolve workspace directory")?;
    run_watch(&workspace).await
}

async fn run_watch(workspace: &Path) -> Result<i32> {
    println!(
        "phonton index: watching {} (Ctrl+C to stop)",
        workspace.display()
    );
    let embedder = Embedder::new().context("load embedding model for semantic index")?;
    let index = index_workspace_using_embedder(workspace, &embedder)
        .await
        .context("build initial semantic index")?;
    let shared = Arc::new(Mutex::new(index));
    let root = workspace.to_path_buf();
    let handle = {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let mut guard = shared.lock().await;
            watch_and_reindex(&mut guard, &root).await;
        })
    };
    tokio::signal::ctrl_c().await?;
    handle.abort();
    let guard = shared.lock().await;
    println!(
        "phonton index: stopped ({} file hashes tracked)",
        guard.len()
    );
    Ok(0)
}
