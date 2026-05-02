#![cfg(feature = "integration-tests")]

use anyhow::Result;
use async_trait::async_trait;
use phonton_orchestrator::{Orchestrator, WorkerDispatcher};
use phonton_planner::{decompose_with_memory, Goal};
use phonton_store::Store;
use phonton_types::{
    DiffHunk, DiffLine, OrchestratorMessage, ProviderKind, Subtask, SubtaskResult, SubtaskStatus,
    TokenUsage, VerifyLayer, VerifyResult,
};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

pub struct StubDispatcher;

#[async_trait]
impl WorkerDispatcher for StubDispatcher {
    async fn dispatch(
        &self,
        subtask: Subtask,
        _prior_errors: Vec<String>,
        _attempt: u8,
        _msg_tx: Option<tokio::sync::mpsc::Sender<OrchestratorMessage>>,
    ) -> Result<SubtaskResult> {
        let hunks = vec![DiffHunk {
            file_path: format!("src/stub_{}.rs", subtask.id).into(),
            old_start: 1,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Added("fn stub() -> u32 { 0 }".into())],
        }];
        Ok(SubtaskResult {
            id: subtask.id,
            status: SubtaskStatus::Done {
                tokens_used: 120,
                diff_hunk_count: hunks.len(),
            },
            diff_hunks: hunks,
            model_tier: subtask.model_tier,
            verify_result: VerifyResult::Pass {
                layer: VerifyLayer::Syntax,
            },
            provider: ProviderKind::Anthropic,
            model_name: "test-stub".into(),
            token_usage: TokenUsage::estimated(120),
        })
    }
}

#[tokio::test]
async fn smoke_test_full_pipeline() -> Result<()> {
    // 1. Create a temp git repo with a single Rust file
    let dir = tempdir()?;
    let repo = git2::Repository::init(dir.path())?;

    let src_dir = dir.path().join("src");
    std::fs::create_dir_all(&src_dir)?;
    std::fs::write(
        src_dir.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }",
    )?;
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"phonton-cli\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\nmembers = [ \".\" ]\n")?;

    // Commit to git
    let mut index = repo.index()?;
    index.add_path(std::path::Path::new("src/lib.rs"))?;
    index.add_path(std::path::Path::new("Cargo.toml"))?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let sig = git2::Signature::now("Test User", "test@example.com")?;
    repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])?;

    // 2. Open a Store::in_memory()
    let store = Store::in_memory()?;

    // 3. Call decompose_with_memory
    let goal = Goal::new("add a multiply function");
    let plan = decompose_with_memory(&goal, &store, None).await?;

    // 4. Assert plan has at least 1 subtask with "multiply"
    assert!(plan
        .subtasks
        .iter()
        .any(|s| s.description.to_lowercase().contains("multiply")));

    // 5. Create Orchestrator with StubDispatcher
    let dispatcher = Arc::new(StubDispatcher);
    let orchestrator = Orchestrator::new(dispatcher).with_working_dir(dir.path().to_path_buf());

    use phonton_types::{GlobalState, TaskStatus};
    use tokio::sync::watch;
    let (state_tx, state_rx) = watch::channel(GlobalState {
        task_status: TaskStatus::Planning,
        active_workers: vec![],
        tokens_used: 0,
        tokens_budget: None,
        estimated_naive_tokens: 0,
        checkpoints: Vec::new(),
    });

    // 6. Run orchestrator with the plan via timeout
    let final_status = tokio::time::timeout(
        Duration::from_secs(30),
        orchestrator.run_task(plan, state_tx),
    )
    .await??;

    let state = state_rx.borrow().clone();

    // 7. Assert terminal status is Reviewing or Done
    match final_status {
        TaskStatus::Reviewing { .. } | TaskStatus::Done { .. } => {}
        _ => panic!("Unexpected terminal status: {:?}", final_status),
    }

    // 8. Assert tokens_used > 0
    assert!(state.tokens_used > 0);

    // 9. Check Store has no crashes when querying task history
    let _history = store.list_tasks(100).await?;

    Ok(())
}
