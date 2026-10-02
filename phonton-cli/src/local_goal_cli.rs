//! Shared adapters for bounded local goals, with persistent inspectable receipts.
use anyhow::{anyhow, bail, Result};
use phonton_types::local::EditProtocol;
use phonton_types::local_run::{
    LocalApplyReceipt, LocalPlan, LocalRunAttempt, LocalRunList, LocalRunReceipt, LocalRunRequest,
    LocalRunSummary, ReviewedLocalPlan, ReviewedModelSelection,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::{IsTerminal, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

const MAX_RECENT_RUNS: usize = 12;
const MAX_SCANNED_RUN_ENTRIES: usize = 4096;
const MAX_DIRECTORY_ENTRIES: usize = 20_000;
const MAX_FALLBACK_RECEIPTS: usize = 12;
const MAX_FALLBACK_MARKERS: usize = 128;
const INTERRUPTED_VERIFICATION_GAP: &str = "Verification ended before complete check evidence was saved. The candidate diff, hash and elapsed time describe a pre-check observation; host checks may have changed its saved directory. Inspect current candidate bytes and separate command journals. Apply remains unavailable.";

const LOCAL_GOAL_USAGE: &str = r#"Usage: phonton goal --local [--plan] "coding goal" [--repo PATH] [--files a,b] [--new-file path] [--edit-existing a,b] [--check '["node","--test"]'] [--yes] [--allow-host-checks] [--allow-unverified-runtime]
Or: phonton goal --local --reviewed-plan plan.json --sha256 REVIEWED_FILE_SHA256 --yes [--allow-host-checks] [--allow-unverified-runtime]
Or: phonton goal --local --request request.json [--allow-host-checks] [--allow-unverified-runtime]
Or: phonton goal --local apply RUN_ID --yes
Or: phonton goal --local rollback RUN_ID --yes
Or: phonton goal --local list
Or: phonton goal --local show RUN_ID"#;

#[derive(Default)]
struct State {
    id: String,
    running: bool,
    receipt: Option<LocalRunReceipt>,
    error: Option<String>,
    cancel: Option<tokio::sync::watch::Sender<bool>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    id: uuid::Uuid,
    request: LocalRunRequest,
    expected_model_selection: ReviewedModelSelection,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyRequest {
    id: uuid::Uuid,
    candidate_number: u32,
    expected_candidate_sha256: String,
    expected_repository: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryMatchRequest {
    id: uuid::Uuid,
    expected_repository: PathBuf,
}
fn repositories_match(saved: &Path, active: &Path) -> Result<bool> {
    Ok(std::fs::canonicalize(saved)? == std::fs::canonicalize(active)?)
}
fn require_active_repository(saved: &Path, active: &Path) -> Result<()> {
    if !repositories_match(saved, active)? {
        bail!(
            "Saved run targets {}; current repository is {}. Open the saved run repository before Apply or Rollback",
            saved.display(),
            active.display()
        );
    }
    Ok(())
}
fn state() -> Arc<Mutex<State>> {
    static STATE: OnceLock<Arc<Mutex<State>>> = OnceLock::new();
    STATE
        .get_or_init(|| Arc::new(Mutex::new(State::default())))
        .clone()
}
fn legacy_root(path: &Path) -> Result<PathBuf> {
    Ok(path
        .parent()
        .ok_or_else(|| anyhow!("Missing local state directory"))?
        .join("runs"))
}

fn runs_root_at(path: &Path, settings: &phonton_types::local::LocalSettings) -> Result<PathBuf> {
    Ok(crate::models_cli::chosen_run_root_at(
        path,
        settings,
        std::env::var_os("PHONTON_LOCAL_STATE").is_some(),
    )?
    .unwrap_or(legacy_root(path)?))
}

fn check_runs_root(request: &LocalRunRequest, path: &Path, runs: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        bail!("Local goal state path must be absolute without . or .. components");
    }
    let repository = std::fs::canonicalize(&request.repository)?;
    // The lease and settings file remain here even when large run copies use
    // the chosen local storage folder.
    let state_parent = path
        .parent()
        .ok_or_else(|| anyhow!("Local model state needs a parent directory"))?;
    for candidate in [state_parent, runs] {
        check_directory_outside_repository(candidate, &repository)?;
    }
    Ok(())
}

fn check_directory_outside_repository(directory: &Path, repository: &Path) -> Result<()> {
    if !directory.is_absolute()
        || directory
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        bail!("Local goal evidence path must be absolute without . or .. components");
    }
    let mut existing = directory;
    let mut missing = Vec::new();
    while !existing.exists() {
        missing.push(
            existing
                .file_name()
                .ok_or_else(|| anyhow!("Cannot resolve local model state directory"))?
                .to_os_string(),
        );
        existing = existing
            .parent()
            .ok_or_else(|| anyhow!("Cannot resolve local model state directory"))?;
    }
    let mut resolved = std::fs::canonicalize(existing)?;
    for part in missing.iter().rev() {
        resolved.push(part);
    }
    if resolved.starts_with(repository) {
        bail!("Local run state and evidence must stay outside the repository");
    }
    Ok(())
}

fn checked_runs_root_with_settings(
    request: &LocalRunRequest,
    path: &Path,
    settings: &phonton_types::local::LocalSettings,
) -> Result<PathBuf> {
    // Check the small state/lease path before resolving saved storage. A
    // project-local PHONTON_LOCAL_STATE must never create a lease in source.
    check_runs_root(request, path, &legacy_root(path)?)?;
    let runs = runs_root_at(path, settings)?;
    check_runs_root(request, path, &runs)?;
    Ok(runs)
}

fn checked_runs_root(request: &LocalRunRequest, path: &Path) -> Result<PathBuf> {
    let settings = phonton_local::storage::load(path)?;
    checked_runs_root_with_settings(request, path, &settings)
}

fn saved_directory_at(path: &Path, id: &str) -> Result<PathBuf> {
    let legacy = legacy_root(path)?.join(id);
    if has_evidence(&legacy) {
        return Ok(legacy);
    }
    let settings = phonton_local::storage::load(path)?;
    Ok(runs_root_at(path, &settings)?.join(id))
}

fn saved_directory(id: &str) -> Result<PathBuf> {
    saved_directory_at(&crate::models_cli::state_path()?, id)
}

fn read_saved_run_at(path: &Path, id: uuid::Uuid, live_here: bool) -> Result<Value> {
    let id = id.to_string();
    let saved = read_saved_receipt(&saved_directory_at(path, &id)?, live_here)?;
    if saved["id"].as_str() != Some(id.as_str()) {
        bail!("Saved run identity does not match requested run ID");
    }
    Ok(saved)
}

fn read_apply_status_at(path: &Path, id: uuid::Uuid) -> Result<Option<LocalApplyReceipt>> {
    let id_text = id.to_string();
    let saved =
        phonton_worker::local_run::apply::read_status(&saved_directory_at(path, &id_text)?)?;
    if saved
        .as_ref()
        .is_some_and(|status| status.run_id != id_text)
    {
        bail!("Saved Apply journal identity does not match requested run ID");
    }
    Ok(saved)
}

fn recent_index_path(path: &Path) -> PathBuf {
    path.with_extension("recent-runs.json")
}

fn evidence_time(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn read_recent_index(path: &Path) -> Vec<LocalRunSummary> {
    let Ok(file) = std::fs::File::open(recent_index_path(path)) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    if file.take(1024 * 1024 + 1).read_to_end(&mut bytes).is_err() || bytes.len() > 1024 * 1024 {
        return Vec::new();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_recent_index(path: &Path, attempt: &LocalRunAttempt) -> Result<()> {
    let index_path = recent_index_path(path);
    let mut runs = read_recent_index(path);
    runs.retain(|run| run.id != attempt.id);
    runs.insert(
        0,
        LocalRunSummary {
            id: attempt.id.clone(),
            goal: attempt.goal.clone(),
            model: attempt.model.clone(),
            recorded_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
        },
    );
    runs.truncate(MAX_RECENT_RUNS);
    let parent = index_path
        .parent()
        .ok_or_else(|| anyhow!("Local run index needs a parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(&serde_json::to_vec(&runs)?)?;
    staged.as_file().sync_all()?;
    staged.persist(index_path).map_err(|error| error.error)?;
    Ok(())
}

fn list_saved_runs_at(path: &Path) -> Result<LocalRunList> {
    list_saved_runs_at_with_limits(path, MAX_SCANNED_RUN_ENTRIES, MAX_DIRECTORY_ENTRIES)
}

fn list_saved_runs_at_with_limits(
    path: &Path,
    candidate_limit: usize,
    directory_limit: usize,
) -> Result<LocalRunList> {
    let legacy = legacy_root(path)?;
    let settings = phonton_local::storage::load(path)?;
    let chosen = runs_root_at(path, &settings)?;
    let mut seen = HashSet::new();
    let mut runs = Vec::new();
    for run in read_recent_index(path).into_iter().take(MAX_RECENT_RUNS) {
        if uuid::Uuid::parse_str(&run.id).is_ok_and(|id| id.to_string() == run.id)
            && (has_evidence(&legacy.join(&run.id)) || has_evidence(&chosen.join(&run.id)))
            && (legacy == chosen
                || !has_evidence(&legacy.join(&run.id))
                || !has_evidence(&chosen.join(&run.id)))
            && seen.insert(run.id.clone())
        {
            runs.push(run);
        }
    }
    if runs.len() == MAX_RECENT_RUNS {
        runs.sort_by(|a, b| {
            b.recorded_at_unix_ms
                .cmp(&a.recorded_at_unix_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        return Ok(LocalRunList {
            runs,
            limited: true,
        });
    }
    // Gather only names and timestamps before reading receipts. A saved index
    // keeps newly admitted runs discoverable even when a large old folder hits
    // the bounded fallback scan.
    let mut candidates: Vec<(u64, String, PathBuf, bool)> = Vec::new();
    let mut visited = 0;
    let mut truncated = false;
    for (index, root) in [chosen.clone(), legacy.clone()].into_iter().enumerate() {
        if index == 1 && root == chosen {
            continue;
        }
        if !root.exists() {
            continue;
        }
        for entry in std::fs::read_dir(root)? {
            if visited == directory_limit || candidates.len() == candidate_limit {
                truncated = true;
                break;
            }
            visited += 1;
            let entry = entry?;
            let name = entry.file_name();
            let file_type = entry.file_type()?;
            if file_type.is_file() {
                let Some(raw_id) = name
                    .to_str()
                    .and_then(|name| name.strip_suffix(".attempt.json"))
                else {
                    continue;
                };
                let Ok(id) = uuid::Uuid::parse_str(raw_id) else {
                    continue;
                };
                let metadata = entry.metadata()?;
                if metadata.len() <= 64 * 1024 {
                    candidates.push((
                        evidence_time(&metadata),
                        id.to_string(),
                        entry.path(),
                        false,
                    ));
                }
            } else if file_type.is_dir() {
                let Some(raw_id) = name.to_str() else {
                    continue;
                };
                let Ok(id) = uuid::Uuid::parse_str(raw_id) else {
                    continue;
                };
                let directory = entry.path();
                for filename in ["end.json", "interruption.json", "receipt.json"] {
                    let receipt = directory.join(filename);
                    let Ok(metadata) = std::fs::symlink_metadata(&receipt) else {
                        continue;
                    };
                    if metadata.file_type().is_file() && metadata.len() <= 16 * 1024 * 1024 {
                        candidates.push((
                            evidence_time(&metadata),
                            id.to_string(),
                            directory,
                            true,
                        ));
                    }
                    break;
                }
            }
        }
        if truncated {
            break;
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut receipt_reads = 0;
    let mut marker_reads = 0;
    for (recorded_at_unix_ms, id, source, is_receipt) in candidates {
        if seen.contains(&id) {
            continue;
        }
        // UUID reopening gives legacy evidence priority. A chosen-root copy
        // must never advertise different content for the same saved ID.
        if chosen != legacy
            && has_evidence(&legacy.join(&id))
            && source.parent() != Some(legacy.as_path())
        {
            continue;
        }
        let summary = if is_receipt {
            if receipt_reads == MAX_FALLBACK_RECEIPTS {
                truncated = true;
                break;
            }
            receipt_reads += 1;
            let Ok(saved) = read_saved_receipt_metadata(&source, false) else {
                continue;
            };
            if saved["id"].as_str() != Some(id.as_str()) {
                continue;
            }
            let (Some(goal), Some(model)) = (
                saved["request"]["goal"].as_str(),
                saved["profile"]["model"].as_str(),
            ) else {
                continue;
            };
            LocalRunSummary {
                id: id.clone(),
                goal: goal.into(),
                model: model.into(),
                recorded_at_unix_ms,
            }
        } else {
            if marker_reads == MAX_FALLBACK_MARKERS {
                truncated = true;
                break;
            }
            marker_reads += 1;
            let Some(root) = source.parent() else {
                continue;
            };
            let directory = root.join(&id);
            // A receipt takes precedence when reopening. If one exists, its
            // candidate above must supply the summary or the run is omitted.
            if ["end.json", "interruption.json", "receipt.json"]
                .iter()
                .any(|name| directory.join(name).exists())
            {
                continue;
            }
            let Ok(saved) = read_saved_receipt(&directory, false) else {
                continue;
            };
            if saved["id"].as_str() != Some(id.as_str()) {
                continue;
            }
            let (Some(goal), Some(model)) = (saved["goal"].as_str(), saved["model"].as_str())
            else {
                continue;
            };
            LocalRunSummary {
                id: id.clone(),
                goal: goal.into(),
                model: model.into(),
                recorded_at_unix_ms,
            }
        };
        seen.insert(id);
        runs.push(summary);
        if runs.len() > MAX_RECENT_RUNS {
            truncated = true;
            break;
        }
    }
    runs.sort_by(|a, b| {
        b.recorded_at_unix_ms
            .cmp(&a.recorded_at_unix_ms)
            .then_with(|| b.id.cmp(&a.id))
    });
    truncated |= runs.len() > MAX_RECENT_RUNS;
    runs.truncate(MAX_RECENT_RUNS);
    Ok(LocalRunList {
        runs,
        limited: truncated,
    })
}

fn checked_saved_directory(request: &LocalRunRequest, path: &Path, id: &str) -> Result<PathBuf> {
    let directory = saved_directory_at(path, id)?;
    let runs = directory
        .parent()
        .ok_or_else(|| anyhow!("Saved run has no evidence directory"))?;
    check_runs_root(request, path, runs)?;
    Ok(directory)
}
fn selected_profile(
    settings: phonton_types::local::LocalSettings,
) -> Result<Option<phonton_types::local::ModelProfile>> {
    let Some(name) = settings.active_model else {
        return Ok(None);
    };
    let name = phonton_local::runtime::canonical_model_name(&name)?;
    let mut profiles = settings.profiles.into_iter().filter(|profile| {
        profile.endpoint == settings.endpoint
            && phonton_local::runtime::canonical_model_name(&profile.model)
                .is_ok_and(|candidate| candidate.eq_ignore_ascii_case(&name))
    });
    let profile = profiles.next().ok_or_else(|| {
        anyhow!("Selected local model profile is unavailable; select or recalibrate a model")
    })?;
    if profiles.next().is_some() {
        bail!("Multiple saved calibrations identify the selected model; recalibrate before running a goal");
    }
    Ok(Some(profile))
}

fn reviewed_selection(
    profile: &phonton_types::local::ModelProfile,
) -> Result<ReviewedModelSelection> {
    Ok(ReviewedModelSelection {
        model: profile.model.clone(),
        digest: profile.digest.clone(),
        runtime_version: profile.runtime_version.clone(),
        endpoint: profile.endpoint.clone(),
        context_tokens: profile.context_tokens,
        output_tokens: profile.output_tokens,
        protocol: profile.protocol,
        profile_sha256: crate::models_cli::profile_sha256(profile)?,
    })
}

async fn current_model_selection_at(path: &Path) -> Result<Option<ReviewedModelSelection>> {
    let settings = phonton_local::storage::load(path)?;
    let Some(profile) = selected_profile(settings)? else {
        return Ok(None);
    };
    let runtime = phonton_local::runtime::LocalRuntime::new(&profile.endpoint)?;
    // Plan review and run admission must observe the installed identity, not
    // just the calibration saved when the model was last selected. The worker
    // repeats these checks before sending repository context.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let version = runtime.version().await?;
        let installed = runtime.installed().await?;
        let model = phonton_local::runtime::find_installed_model(&installed, &profile.model)?
            .ok_or_else(|| anyhow!("Selected local model is no longer installed; install and calibrate it again"))?;
        phonton_local::runtime::validate_profile(&profile, model, runtime.endpoint(), &version)?;
        let metadata = runtime.show_local(&model.name).await?;
        phonton_local::runtime::validate_profile_context(&profile, &metadata)?;
        reviewed_selection(&profile).map(Some)
    })
    .await
    .map_err(|_| anyhow!("Local runtime did not confirm the selected model within 15 seconds; retry or inspect Local models"))?
}

pub(crate) async fn current_model_selection() -> Result<Option<ReviewedModelSelection>> {
    current_model_selection_at(&crate::models_cli::state_path()?).await
}

async fn plan_model_selection_at(
    path: &Path,
    plan: &mut LocalPlan,
) -> Option<ReviewedModelSelection> {
    match current_model_selection_at(path).await {
        Ok(selection) => selection,
        Err(error) => {
            plan.warnings.push(format!(
                "Selected local model is not ready: {error}. Refresh Local models or recalibrate, then review this plan again."
            ));
            None
        }
    }
}

async fn plan_model_selection(plan: &mut LocalPlan) -> Result<Option<ReviewedModelSelection>> {
    Ok(plan_model_selection_at(&crate::models_cli::state_path()?, plan).await)
}

fn runtime_guard_at(
    path: &Path,
    settings: &phonton_types::local::LocalSettings,
    allow_unverified: bool,
) -> Result<phonton_worker::local_run::RuntimeGuard> {
    let root = crate::models_cli::managed_root_at(
        path,
        settings,
        std::env::var_os("PHONTON_LOCAL_STATE").is_some(),
    )?;
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    let _ = &root;
    if settings.endpoint == crate::models_cli::MANAGED_MODEL_ENDPOINT {
        #[cfg(all(windows, target_arch = "x86_64"))]
        match phonton_local::managed_store::bind(&root, &settings.endpoint) {
            Ok(Some(binding)) => {
                return Ok(phonton_worker::local_run::RuntimeGuard::managed_verified(
                    settings.endpoint.clone(),
                    move || {
                        binding
                            .available_bytes()
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    },
                ));
            }
            Ok(None) => {}
            Err(error) => {
                bail!("Managed Ollama process or listener could not be verified: {error}. If a service is still using the managed loopback port, stop it; reconnect the original managed folder if needed, then rerun managed setup before sending repository context");
            }
        }
    }
    if !allow_unverified {
        bail!("This loopback runtime is external or unverified and may relay repository context. Use Phonton-managed setup for verified local inference, or explicitly allow an unverified runtime for this goal");
    }
    Ok(phonton_worker::local_run::RuntimeGuard::external_unverified(settings.endpoint.clone()))
}

fn prepare_at(
    request: &LocalRunRequest,
    requested_id: Option<uuid::Uuid>,
    expected_model_selection: &ReviewedModelSelection,
    path: &Path,
) -> Result<(
    phonton_local::storage::StateLease,
    phonton_types::local::ModelProfile,
    phonton_worker::local_run::RuntimeGuard,
    PathBuf,
)> {
    let root = checked_runs_root(request, path)?;
    let lease = phonton_local::storage::acquire(path)?;
    let mut settings = phonton_local::storage::load(path)?;
    if checked_runs_root_with_settings(request, path, &settings)? != root {
        bail!("Local run storage changed during admission. Review the plan and retry");
    }
    let profile = selected_profile(settings.clone())?
        .ok_or_else(|| anyhow!("Calibrate and select a local model first"))?;
    if reviewed_selection(&profile)? != *expected_model_selection {
        bail!("The selected local model or its calibration changed after plan review. Review the plan again before running");
    }
    let id = requested_id.unwrap_or_else(uuid::Uuid::new_v4);
    let directory = root.join(id.to_string());
    // Saved-run lookup still honors evidence written beside the old state path.
    // Reject an ID in either location so a new run cannot be hidden by that
    // legacy receipt, including when the ID came from a Desktop RPC request.
    if has_evidence(&directory) || has_evidence(&legacy_root(path)?.join(id.to_string())) {
        bail!("Run ID already has saved evidence");
    }
    let runtime_guard = runtime_guard_at(path, &settings, request.allow_unverified_runtime)?;
    std::fs::create_dir_all(&root)?;
    if std::fs::canonicalize(&root)?.starts_with(std::fs::canonicalize(&request.repository)?) {
        bail!("Local run evidence resolved inside the repository");
    }
    if settings.managed_root.is_some()
        && std::env::var_os("PHONTON_LOCAL_STATE").is_none()
        && crate::models_cli::chosen_run_root_at(path, &settings, false)?.as_deref()
            != Some(root.as_path())
    {
        bail!("Chosen run evidence folder changed during admission");
    }
    if settings.managed_root.is_some()
        && std::env::var_os("PHONTON_LOCAL_STATE").is_none()
        && !settings.managed_root_used
    {
        settings.managed_root_used = true;
        phonton_local::storage::save(path, &settings)?;
    }
    Ok((lease, profile, runtime_guard, directory))
}
async fn prepare_live_at(
    request: &LocalRunRequest,
    requested_id: Option<uuid::Uuid>,
    expected_model_selection: &ReviewedModelSelection,
    path: &Path,
) -> Result<(
    phonton_local::storage::StateLease,
    phonton_types::local::ModelProfile,
    phonton_worker::local_run::RuntimeGuard,
    PathBuf,
)> {
    let current = current_model_selection_at(path)
        .await?
        .ok_or_else(|| anyhow!("Calibrate and select a local model first"))?;
    if current != *expected_model_selection {
        bail!("The selected local model or its calibration changed after plan review. Review the plan again before running");
    }
    prepare_at(request, requested_id, expected_model_selection, path)
}

async fn prepare(
    request: &LocalRunRequest,
    requested_id: Option<uuid::Uuid>,
    expected_model_selection: &ReviewedModelSelection,
) -> Result<(
    phonton_local::storage::StateLease,
    phonton_types::local::ModelProfile,
    phonton_worker::local_run::RuntimeGuard,
    PathBuf,
)> {
    prepare_live_at(
        request,
        requested_id,
        expected_model_selection,
        &crate::models_cli::state_path()?,
    )
    .await
}
fn attempt_path(directory: &std::path::Path) -> PathBuf {
    directory.with_extension("attempt.json")
}
fn ended_path(directory: &std::path::Path) -> PathBuf {
    directory.with_extension("ended.json")
}
fn has_evidence(directory: &Path) -> bool {
    directory.exists() || attempt_path(directory).exists() || ended_path(directory).exists()
}
fn save_attempt(path: &std::path::Path, attempt: &LocalRunAttempt) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(attempt)?)?;
    std::fs::rename(temp, path)?;
    Ok(())
}
fn write_end_receipt(directory: &Path, receipt: &LocalRunReceipt) -> Result<()> {
    let mut staged = tempfile::NamedTempFile::new_in(directory)?;
    staged.write_all(&serde_json::to_vec_pretty(receipt)?)?;
    staged.as_file().sync_all()?;
    staged
        .persist(directory.join("end.json"))
        .map_err(|error| error.error)?;
    Ok(())
}
fn begin_attempt(
    directory: &std::path::Path,
    request: &LocalRunRequest,
    profile: &phonton_types::local::ModelProfile,
    state_path: &Path,
) -> Result<LocalRunAttempt> {
    if directory.exists() || attempt_path(directory).exists() || ended_path(directory).exists() {
        bail!("Run ID already has saved evidence");
    }
    if std::fs::canonicalize(
        directory
            .parent()
            .ok_or_else(|| anyhow!("Missing run directory parent"))?,
    )?
    .starts_with(std::fs::canonicalize(&request.repository)?)
    {
        bail!("Local run evidence resolved inside the repository");
    }
    let attempt = LocalRunAttempt {
        schema: 1,
        id: directory
            .file_name()
            .ok_or_else(|| anyhow!("Missing run id"))?
            .to_string_lossy()
            .into(),
        goal: request.goal.clone(),
        model: profile.model.clone(),
        state: "started".into(),
        error: None,
    };
    save_attempt(&attempt_path(directory), &attempt)?;
    if let Err(error) = save_recent_index(state_path, &attempt) {
        eprintln!(
            "phonton local goal: recent-run index unavailable; keep the run ID from its receipt to reopen it: {error}"
        );
    }
    Ok(attempt)
}
fn read_attempt(directory: &std::path::Path, live: bool) -> Result<LocalRunAttempt> {
    let ended = ended_path(directory);
    let path = if ended.exists() {
        ended
    } else {
        attempt_path(directory)
    };
    if std::fs::metadata(&path)?.len() > 64 * 1024 {
        bail!("Run attempt exceeds read limit");
    }
    let mut attempt: LocalRunAttempt = serde_json::from_slice(&std::fs::read(path)?)?;
    if attempt.state == "started" && !live {
        attempt.state = "interrupted_before_receipt".into();
        attempt.error = Some("The engine stopped before it saved a receipt. No project check or model generation is recorded for this attempt.".into());
    }
    Ok(attempt)
}
fn record_end(
    directory: &std::path::Path,
    attempt: &LocalRunAttempt,
    message: &str,
    cancelled: bool,
    elapsed_ms: u64,
) -> Option<LocalRunReceipt> {
    let path = directory.join("receipt.json");
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(mut receipt) = serde_json::from_slice::<LocalRunReceipt>(&bytes) {
            let verification_interrupted = receipt.state.starts_with("verifying_");
            receipt.state = if cancelled {
                "interrupted"
            } else {
                "run_failed"
            }
            .into();
            receipt.selected_candidate = None;
            receipt.known_gaps.push(message.into());
            receipt.elapsed_ms = receipt.elapsed_ms.max(elapsed_ms);
            if verification_interrupted {
                receipt.known_gaps.push(INTERRUPTED_VERIFICATION_GAP.into());
            }
            // A check reservation is durable before its process starts, even if
            // interruption prevented the next full receipt from being written.
            for number in 1..=receipt.request.budget.check_runs {
                if directory.join(format!("check-{number}.json")).exists() {
                    receipt.checks_used = receipt.checks_used.max(number);
                }
            }
            let _ = write_end_receipt(directory, &receipt);
            return Some(receipt);
        }
    }
    let mut ended = attempt.clone();
    ended.state = if cancelled {
        "interrupted_before_receipt"
    } else {
        "ended_before_receipt"
    }
    .into();
    ended.error = Some(message.into());
    let _ = save_attempt(&ended_path(directory), &ended);
    None
}

fn adjust_reviewed_plan_budget(
    plan: &mut LocalPlan,
    selection: Option<&ReviewedModelSelection>,
    explicit_budget: bool,
) {
    if selection.and_then(|selected| selected.protocol) != Some(EditProtocol::SearchReplace) {
        return;
    }
    let editable = if plan.request.new_file.is_some() {
        1 + plan.request.editable_existing.len()
    } else {
        plan.request.files.len()
    } as u32;
    if editable <= 1 {
        return;
    }
    let checks_per_candidate =
        plan.request.checks.len() as u32 + u32::from(plan.request.preparation.is_some());
    let per_call = selection.map_or(0, |selected| {
        selected.output_tokens.min(selected.context_tokens / 4)
    });
    if explicit_budget {
        if plan.request.budget.generations < editable {
            plan.warnings.push(format!(
                "SearchReplace changes one path per model call. The explicit budget has {} calls for {editable} editable paths, so it cannot cover a straight one-edit-per-path sequence. Increase generations or narrow the scope.",
                plan.request.budget.generations
            ));
        }
        let required_checks = (editable + 1).saturating_mul(checks_per_candidate);
        if plan.request.budget.check_runs < required_checks {
            plan.warnings.push(format!(
                "Baseline plus {editable} SearchReplace candidates can require {required_checks} check/setup slots; the explicit budget reserves {}. Narrow the scope or checks if this sequence is needed.",
                plan.request.budget.check_runs
            ));
        }
        if plan.request.budget.generations >= editable
            && !minimum_edit_allocations_fit(
                plan.request.budget.generated_tokens,
                plan.request.budget.generations,
                editable,
                per_call,
            )
        {
            plan.warnings.push(format!(
                "The explicit {}-token output reserve cannot allocate a minimum 128-token edit response for each of {editable} SearchReplace paths. Increase generated_tokens or narrow the scope.",
                plan.request.budget.generated_tokens
            ));
        }
        return;
    }
    let budget = &mut plan.request.budget;
    let original_calls = budget.generations;
    budget.generations = budget.generations.max(editable).min(8);
    let required_checks = (budget.generations + 1).saturating_mul(checks_per_candidate);
    budget.check_runs = budget.check_runs.max(required_checks.min(32));
    let required_tokens = u64::from(per_call).saturating_mul(u64::from(budget.generations));
    budget.generated_tokens = budget.generated_tokens.max(required_tokens.min(16_384));
    if budget.generations > original_calls {
        plan.warnings.push(format!(
            "SearchReplace changes one path per model call. This {editable}-path plan reserves {} calls, {} check/setup slots and {} output tokens for the best-case sequence. Repairs or restarts can consume calls before every path is changed; narrow the scope if needed.",
            budget.generations, budget.check_runs, budget.generated_tokens
        ));
    }
    if required_checks > 32 {
        plan.warnings.push(format!(
            "Baseline plus {} SearchReplace candidates with the selected checks can require {required_checks} check/setup slots, above the 32-slot cap. Narrow the file or check scope if every path needs an edit.",
            budget.generations
        ));
    }
    if !minimum_edit_allocations_fit(
        budget.generated_tokens,
        budget.generations,
        editable,
        per_call,
    ) {
        plan.warnings.push("The output reserve cap cannot allocate a minimum 128-token response for every scoped SearchReplace edit. Narrow the scope or use a model calibrated with a smaller output allowance.".into());
    } else if required_tokens > 16_384 {
        plan.warnings.push(
            "The calibrated per-call output allowance across this plan exceeds the 16,384-token reserve cap. Later calls may receive smaller outputs.".into(),
        );
    }
}

fn minimum_edit_allocations_fit(
    mut tokens: u64,
    generations: u32,
    editable: u32,
    per_call: u32,
) -> bool {
    if generations < editable {
        return false;
    }
    for used in 0..editable {
        let remaining_calls = generations - used;
        let later_reserve = if remaining_calls > 1 { tokens / 2 } else { 0 };
        let output = u64::from(per_call).min(tokens.saturating_sub(later_reserve));
        if output < 128 {
            return false;
        }
        tokens -= output;
    }
    true
}

pub async fn rpc(method: &str, params: Value) -> Result<Value> {
    let shared = state();
    match method {
        "local.run.plan" => {
            let explicit_budget = params.get("budget").is_some();
            let request: LocalRunRequest = serde_json::from_value(params)?;
            let mut plan = phonton_worker::local_run::plan::preview(request).await?;
            let selection = plan_model_selection(&mut plan).await?;
            adjust_reviewed_plan_budget(&mut plan, selection.as_ref(), explicit_budget);
            Ok(serde_json::to_value(ReviewedLocalPlan {
                plan,
                model_selection: selection,
            })?)
        }
        "local.run.start" => {
            let start: StartRequest = serde_json::from_value(params)?;
            let request = start.request;
            let (lease, profile, runtime_guard, directory) =
                prepare(&request, Some(start.id), &start.expected_model_selection).await?;
            let attempt = begin_attempt(
                &directory,
                &request,
                &profile,
                &crate::models_cli::state_path()?,
            )?;
            let id = directory
                .file_name()
                .ok_or_else(|| anyhow!("Missing run id"))?
                .to_string_lossy()
                .to_string();
            let (cancel, mut rx) = tokio::sync::watch::channel(false);
            {
                let mut state = shared
                    .lock()
                    .map_err(|_| anyhow!("Run state unavailable"))?;
                if state.running {
                    bail!("A local goal is already running");
                }
                *state = State {
                    id: id.clone(),
                    running: true,
                    cancel: Some(cancel),
                    ..Default::default()
                };
            }
            tokio::spawn(async move {
                let _lease = lease;
                let events = shared.clone();
                let run_started = std::time::Instant::now();
                let (result, cancelled) = tokio::select! {
                    _ = rx.changed() => (Err(anyhow!("Local goal cancelled; candidate evidence was retained")), true),
                    result = phonton_worker::local_run::run(request, profile, runtime_guard, &directory, |receipt| {
                        if let Ok(mut state) = events.lock() {
                            let mut visible = receipt.clone();
                            if matches!(visible.state.as_str(), "review_ready" | "review_unverified") {
                                visible.state = "finalizing".into();
                                visible.selected_candidate = None;
                            }
                            state.receipt = Some(visible);
                        }
                    }) => (result.map_err(anyhow::Error::from), false),
                };
                // Completion is durable only after the worker's final receipt
                // has a separate end record. A restart must not mistake an
                // older premature review claim for this completed run.
                let result = result.and_then(|receipt| {
                    write_end_receipt(&directory, &receipt)?;
                    Ok(receipt)
                });
                let interrupted = result.as_ref().err().and_then(|error| {
                    record_end(
                        &directory,
                        &attempt,
                        &error.to_string(),
                        cancelled,
                        run_started.elapsed().as_millis() as u64,
                    )
                });
                if let Ok(mut state) = shared.lock() {
                    state.running = false;
                    state.cancel = None;
                    match result {
                        Ok(receipt) => state.receipt = Some(receipt),
                        Err(error) => {
                            state.receipt = interrupted;
                            state.error = Some(error.to_string());
                        }
                    }
                }
            });
            Ok(json!({"id":id}))
        }
        "local.run.status" => {
            let (id, running, mut receipt, error) = {
                let state = shared
                    .lock()
                    .map_err(|_| anyhow!("Run state unavailable"))?;
                (
                    state.id.clone(),
                    state.running,
                    state.receipt.clone(),
                    state.error.clone(),
                )
            };
            if !running {
                if let Some(saved) = receipt.as_mut() {
                    if saved.selected_candidate.is_some() {
                        match saved_directory(&id) {
                            Ok(directory) => revalidate_loaded_review(&directory, saved),
                            Err(error) => demote_saved_review(saved, &error.to_string()),
                        }
                    }
                }
            }
            Ok(json!({"id":id,"running":running,"receipt":receipt,"error":error}))
        }
        "local.run.list" => Ok(serde_json::to_value(list_saved_runs_at(
            &crate::models_cli::state_path()?,
        )?)?),
        "local.run.cancel" => {
            let state = shared
                .lock()
                .map_err(|_| anyhow!("Run state unavailable"))?;
            if params["id"].as_str() != Some(&state.id) {
                bail!("Run changed; refresh before cancellation");
            }
            if let Some(cancel) = &state.cancel {
                let _ = cancel.send(true);
            }
            Ok(json!({"cancel_requested":state.running}))
        }
        "local.run.read" => {
            let id = params["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Run id required"))?;
            let id = uuid::Uuid::parse_str(id)?;
            let live = shared
                .lock()
                .map_err(|_| anyhow!("Run state unavailable"))?;
            let live_here = live.running && live.id == id.to_string();
            drop(live);
            read_saved_run_at(&crate::models_cli::state_path()?, id, live_here)
        }
        "local.run.has_evidence" => {
            let id = params["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Run id required"))?;
            let id = uuid::Uuid::parse_str(id)?;
            Ok(json!(has_evidence(&saved_directory(&id.to_string())?)))
        }
        "local.run.apply_status" => {
            let id = params["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Run id required"))?;
            let id = uuid::Uuid::parse_str(id)?;
            Ok(serde_json::to_value(read_apply_status_at(
                &crate::models_cli::state_path()?,
                id,
            )?)?)
        }
        "local.run.repository_match" => {
            let request: RepositoryMatchRequest = serde_json::from_value(params)?;
            let saved =
                Box::pin(rpc("local.run.read", json!({"id":request.id.to_string()}))).await?;
            if saved.get("request").is_none() {
                bail!("This goal has no final candidate receipt");
            }
            let receipt: LocalRunReceipt = serde_json::from_value(saved)?;
            Ok(json!({"matches": repositories_match(
                &receipt.request.repository,
                &request.expected_repository
            )?}))
        }
        "local.run.apply" => {
            let request: ApplyRequest = serde_json::from_value(params)?;
            let id = request.id.to_string();
            let saved = Box::pin(rpc("local.run.read", json!({"id":id.clone()}))).await?;
            if saved.get("request").is_none() {
                bail!("This goal has no final candidate receipt to apply");
            }
            let receipt: LocalRunReceipt = serde_json::from_value(saved)?;
            require_active_repository(&receipt.request.repository, &request.expected_repository)?;
            let state_path = crate::models_cli::state_path()?;
            let _lease = phonton_local::storage::acquire(&state_path)?;
            let directory = checked_saved_directory(&receipt.request, &state_path, &id)?;
            let result = phonton_worker::local_run::apply::apply_selected(
                &receipt,
                &directory,
                request.candidate_number,
                &request.expected_candidate_sha256,
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        "local.run.rollback" => {
            let request: ApplyRequest = serde_json::from_value(params)?;
            let id = request.id.to_string();
            let saved = Box::pin(rpc("local.run.read", json!({"id":id.clone()}))).await?;
            if saved.get("request").is_none() {
                bail!("This goal has no final candidate receipt to roll back");
            }
            let receipt: LocalRunReceipt = serde_json::from_value(saved)?;
            require_active_repository(&receipt.request.repository, &request.expected_repository)?;
            let state_path = crate::models_cli::state_path()?;
            let _lease = phonton_local::storage::acquire(&state_path)?;
            let directory = checked_saved_directory(&receipt.request, &state_path, &id)?;
            let result = phonton_worker::local_run::apply::rollback_selected(
                &receipt,
                &directory,
                request.candidate_number,
                &request.expected_candidate_sha256,
            )
            .await?;
            Ok(serde_json::to_value(result)?)
        }
        _ => bail!("Unknown local run method"),
    }
}

fn read_saved_receipt(directory: &Path, live_here: bool) -> Result<Value> {
    read_saved_receipt_internal(directory, live_here, true)
}

fn read_saved_receipt_metadata(directory: &Path, live_here: bool) -> Result<Value> {
    read_saved_receipt_internal(directory, live_here, false)
}

fn read_saved_receipt_internal(
    directory: &Path,
    live_here: bool,
    revalidate_review: bool,
) -> Result<Value> {
    let path = if directory.join("end.json").exists() {
        directory.join("end.json")
    } else if directory.join("interruption.json").exists() {
        directory.join("interruption.json")
    } else {
        directory.join("receipt.json")
    };
    if !path.exists() {
        return Ok(serde_json::to_value(read_attempt(directory, live_here)?)?);
    }
    if std::fs::metadata(&path)?.len() > 16 * 1024 * 1024 {
        bail!("Run receipt exceeds read limit");
    }
    let completed_file = path.ends_with("end.json");
    let mut receipt: LocalRunReceipt = serde_json::from_slice(&std::fs::read(path)?)?;
    recover_saved_receipt(directory, &mut receipt, live_here, completed_file);
    if completed_file && revalidate_review {
        revalidate_loaded_review(directory, &mut receipt);
    }
    Ok(serde_json::to_value(receipt)?)
}

fn demote_saved_review(receipt: &mut LocalRunReceipt, detail: &str) {
    receipt.state = "saved_review_unavailable".into();
    receipt.selected_candidate = None;
    receipt.known_gaps.push(format!(
        "Saved review could not be revalidated: {detail}. Apply is unavailable until a current run verifies the goal."
    ));
}

fn revalidate_loaded_review(directory: &Path, receipt: &mut LocalRunReceipt) {
    if receipt.selected_candidate.is_some() {
        if let Err(error) = phonton_worker::local_run::validate_saved_review(receipt, directory) {
            demote_saved_review(receipt, &error.to_string());
        }
    }
}

fn recover_saved_receipt(
    directory: &Path,
    receipt: &mut LocalRunReceipt,
    live_here: bool,
    completed_file: bool,
) {
    if !completed_file && matches!(receipt.state.as_str(), "review_ready" | "review_unverified") {
        receipt.state = "finalizing".into();
        receipt.selected_candidate = None;
    }
    let unfinished = receipt.state.starts_with("generating_")
        || receipt.state.starts_with("hypothesizing_")
        || receipt.state.starts_with("verifying_")
        || matches!(receipt.state.as_str(), "baseline" | "finalizing");
    if unfinished && !live_here {
        if receipt.state.starts_with("verifying_") {
            receipt.known_gaps.push(INTERRUPTED_VERIFICATION_GAP.into());
        }
        receipt.state = "interrupted".into();
        receipt.selected_candidate = None;
        for number in 1..=receipt.request.budget.check_runs {
            if directory.join(format!("check-{number}.json")).exists() {
                receipt.checks_used = receipt.checks_used.max(number);
            }
        }
    }
}

fn fresh_permissions(args: &[String]) -> Result<(bool, bool)> {
    let mut host = false;
    let mut unverified = false;
    for flag in args {
        match flag.as_str() {
            "--allow-host-checks" if !host => host = true,
            "--allow-unverified-runtime" if !unverified => unverified = true,
            _ => bail!("Unknown or duplicate local goal permission: {flag}"),
        }
    }
    Ok((host, unverified))
}

fn request_from_file_args(args: &[String]) -> Result<LocalRunRequest> {
    if args.len() < 2 {
        bail!("Use --request request.json [--allow-host-checks] [--allow-unverified-runtime]");
    }
    let (host_now, unverified_now) = fresh_permissions(&args[2..])?;
    if std::fs::metadata(&args[1])?.len() > 64 * 1024 {
        bail!("Local goal request exceeds 64 KiB");
    }
    let mut request: LocalRunRequest = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    if request.approve_host_execution && !host_now {
        bail!("Request file approval is not current CLI authorization; rerun with --allow-host-checks to execute host setup or checks");
    }
    if request.allow_unverified_runtime && !unverified_now {
        bail!("Request file runtime consent is not current CLI authorization; rerun with --allow-unverified-runtime to send repository context to an unverified service");
    }
    request.approve_host_execution = host_now;
    request.allow_unverified_runtime = unverified_now;
    Ok(request)
}

fn request_from_reviewed_plan_args(
    args: &[String],
) -> Result<(LocalRunRequest, ReviewedModelSelection)> {
    if args.len() < 5 || args[2] != "--sha256" || args[4] != "--yes" {
        bail!("Use --reviewed-plan plan.json --sha256 REVIEWED_FILE_SHA256 --yes [--allow-host-checks] [--allow-unverified-runtime]");
    }
    let (host_now, unverified_now) = fresh_permissions(&args[5..])?;
    if std::fs::metadata(&args[1])?.len() > 256 * 1024 {
        bail!("Reviewed local goal plan exceeds 256 KiB");
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&args[1])?
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 256 * 1024 {
        bail!("Reviewed local goal plan exceeds 256 KiB");
    }
    if args[3].len() != 64 || !args[3].bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Reviewed plan SHA-256 must be exactly 64 hexadecimal characters");
    }
    let actual_hash = format!("{:x}", Sha256::digest(&bytes));
    if !actual_hash.eq_ignore_ascii_case(&args[3]) {
        bail!("Reviewed plan file changed after its SHA-256 was recorded; inspect it and review again");
    }
    let reviewed: ReviewedLocalPlan = serde_json::from_slice(&bytes)?;
    let selection = reviewed.model_selection.ok_or_else(|| {
        anyhow!("Reviewed plan has no selected model; calibrate, select, and review again")
    })?;
    let expected_hashes: std::collections::BTreeMap<PathBuf, String> = reviewed
        .plan
        .files
        .iter()
        .map(|file| (file.path.clone(), file.source_sha256.clone()))
        .collect();
    if reviewed.plan.request.expected_baseline_sha256.is_none()
        || expected_hashes.len() != reviewed.plan.files.len()
        || expected_hashes != reviewed.plan.request.expected_source_hashes
        || reviewed.plan.request.files.len() != reviewed.plan.files.len()
        || reviewed
            .plan
            .request
            .files
            .iter()
            .any(|path| !expected_hashes.contains_key(path))
    {
        bail!(
            "Reviewed plan is missing or has inconsistent source identity; review the plan again"
        );
    }
    let mut request = reviewed.plan.request;
    if request.allow_unverified_runtime && !unverified_now {
        bail!("Reviewed plan runtime consent is not current CLI authorization; rerun with --allow-unverified-runtime");
    }
    request.approve_host_execution = host_now;
    request.allow_unverified_runtime = unverified_now;
    Ok((request, selection))
}

pub async fn run(args: &[String]) -> Result<i32> {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h" | "help") {
        println!("{LOCAL_GOAL_USAGE}");
        return Ok(0);
    }
    if args.first().is_some_and(|value| value == "list") {
        if args.len() != 1 {
            bail!("Usage: phonton goal --local list");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&list_saved_runs_at(&crate::models_cli::state_path()?)?)?
        );
        return Ok(0);
    }
    if args.first().is_some_and(|value| value == "show") {
        if args.len() != 2 {
            bail!("Usage: phonton goal --local show RUN_ID");
        }
        let id = uuid::Uuid::parse_str(&args[1])?;
        let state_path = crate::models_cli::state_path()?;
        let directory = saved_directory_at(&state_path, &id.to_string())?;
        let terminal_recorded =
            directory.join("end.json").exists() || ended_path(&directory).exists();
        // A separate CLI process cannot know whether another engine is still
        // running. Without a terminal marker, preserve provisional evidence
        // and label its completion state as unknown.
        let evidence = read_saved_run_at(&state_path, id, !terminal_recorded)?;
        let (apply_status, apply_status_error) = match read_apply_status_at(&state_path, id) {
            Ok(status) => (status, None),
            Err(error) => (None, Some(error.to_string())),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": id,
                "terminal_recorded": terminal_recorded,
                "evidence": evidence,
                "apply_status": apply_status,
                "apply_status_error": apply_status_error,
                "note": if terminal_recorded {
                    None
                } else {
                    Some("No terminal record exists. This run may still be active or may have stopped before completion; do not treat a provisional candidate as verified.")
                },
            }))?
        );
        return Ok(0);
    }
    if args
        .first()
        .is_some_and(|value| value == "apply" || value == "rollback")
    {
        let action = args[0].as_str();
        if args.len() != 3 || args[2] != "--yes" {
            bail!("Usage: phonton goal --local {action} RUN_ID --yes");
        }
        let id = uuid::Uuid::parse_str(&args[1])?.to_string();
        let saved = rpc("local.run.read", json!({"id":id.clone()})).await?;
        if saved.get("request").is_none() {
            bail!("This goal has no final candidate receipt to apply");
        }
        let receipt: LocalRunReceipt = serde_json::from_value(saved)?;
        let number = receipt
            .selected_candidate
            .ok_or_else(|| anyhow!("No selected candidate to apply"))?;
        let candidate = receipt
            .candidates
            .iter()
            .find(|candidate| candidate.number == number)
            .ok_or_else(|| anyhow!("Selected candidate is missing"))?;
        let hash = candidate
            .content_sha256
            .as_ref()
            .ok_or_else(|| anyhow!("Selected candidate has no saved hash"))?;
        eprintln!(
            "{} reviewed candidate {number} from run {id} in {}",
            if action == "apply" {
                "Applying"
            } else {
                "Rolling back"
            },
            receipt.request.repository.display()
        );
        eprintln!("{}", candidate.diff);
        let applied: LocalApplyReceipt = serde_json::from_value(
            rpc(
                if action == "apply" {
                    "local.run.apply"
                } else {
                    "local.run.rollback"
                },
                json!({"id":id.clone(),"candidate_number":number,"expected_candidate_sha256":hash,"expected_repository":std::env::current_dir()?}),
            )
            .await?,
        )?;
        println!("{}", serde_json::to_string_pretty(&applied)?);
        return Ok(0);
    }
    let (request, expected_model_selection): (LocalRunRequest, ReviewedModelSelection) = if args
        .first()
        .is_some_and(|s| s == "--reviewed-plan")
    {
        request_from_reviewed_plan_args(args)?
    } else if args.first().is_some_and(|s| s == "--request") {
        let request = request_from_file_args(args)?;
        let selection = current_model_selection()
            .await?
            .ok_or_else(|| anyhow!("Calibrate and select a local model first"))?;
        (request, selection)
    } else {
        let options = parse_goal(args)?;
        let mut plan = phonton_worker::local_run::plan::preview(options.request).await?;
        let selection = plan_model_selection(&mut plan).await?;
        adjust_reviewed_plan_budget(&mut plan, selection.as_ref(), false);
        if options.preview_only {
            println!(
                "{}",
                serde_json::to_string_pretty(&ReviewedLocalPlan {
                    plan,
                    model_selection: selection,
                })?
            );
            return Ok(0);
        }
        eprintln!(
            "Goal: {}\nRepository: {}",
            plan.request.goal,
            plan.request.repository.display()
        );
        for file in &plan.files {
            eprintln!("  {} — {}", file.path.display(), file.reason);
        }
        if let Some(creation) = &plan.creation {
            eprintln!("  CREATE {} — {}", creation.path.display(), creation.reason);
        }
        if let Some(preparation) = &plan.request.preparation {
            eprintln!(
                "Offline setup proposal (not verification): {:?} {:?}",
                preparation.program, preparation.args
            );
        }
        for check in &plan.request.checks {
            eprintln!("Check proposal: {:?} {:?}", check.program, check.args);
        }
        for warning in &plan.warnings {
            eprintln!("{warning}");
        }
        let selection =
            selection.ok_or_else(|| anyhow!("Calibrate and select a local model first"))?;
        eprintln!(
            "Selected local model: {} (digest {}, runtime {}, context {} tokens, profile SHA-256 {})",
            selection.model,
            selection.digest,
            selection.runtime_version,
            selection.context_tokens,
            selection.profile_sha256
        );
        if options.host_approved {
            eprintln!("Host setup/checks explicitly approved. Search edits stay in copies, but unisolated project commands may affect the host; original source and Git index are checked afterward.");
        } else {
            eprintln!("Host setup/checks not approved; review will be unverified. Search edits stay in copies, and no project command runs.");
        }
        if options.runtime_approved {
            eprintln!("Unverified loopback runtime explicitly allowed. It may relay repository context outside this machine; the receipt will label its origin unverified.");
        }
        if !options.approve_plan {
            if !std::io::stdin().is_terminal() {
                bail!("Review with --plan first, then use --yes to accept the proposed scope. Host checks separately require --allow-host-checks.");
            }
            eprint!("Run this plan? [y/N] ");
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                return Ok(0);
            }
        }
        plan.request.approve_host_execution = options.host_approved;
        plan.request.allow_unverified_runtime = options.runtime_approved;
        (plan.request, selection)
    };
    eprintln!(
        "Plan contract: {}",
        serde_json::to_string(&phonton_worker::local_run::plan::contract(&request))?
    );
    let (_lease, profile, runtime_guard, directory) =
        prepare(&request, None, &expected_model_selection).await?;
    let attempt = begin_attempt(
        &directory,
        &request,
        &profile,
        &crate::models_cli::state_path()?,
    )?;
    eprintln!("Local goal evidence: {}", directory.display());
    eprintln!(
        "Source context: {:?}; create: {:?}; preparation: {:?}; checks: {:?}; host execution approved: {}",
        request.files, request.new_file, request.preparation, request.checks, request.approve_host_execution
    );
    let run_started = std::time::Instant::now();
    let (result, cancelled) = tokio::select! {
        _ = tokio::signal::ctrl_c() => (Err(anyhow!("Local goal cancelled")), true),
        result = phonton_worker::local_run::run(request, profile, runtime_guard, &directory, |receipt| eprintln!("{}: {} candidates, {} check/setup slots reserved", receipt.state, receipt.candidates.len(), receipt.checks_used)) => (result.map_err(anyhow::Error::from), false),
    };
    let result = result.and_then(|receipt| {
        write_end_receipt(&directory, &receipt)?;
        Ok(receipt)
    });
    match result {
        Ok(receipt) => {
            println!("{}", serde_json::to_string_pretty(&receipt)?);
            Ok(if receipt.state == "review_ready" {
                0
            } else if receipt.selected_candidate.is_some() {
                3
            } else {
                1
            })
        }
        Err(error) => {
            record_end(
                &directory,
                &attempt,
                &error.to_string(),
                cancelled,
                run_started.elapsed().as_millis() as u64,
            );
            Err(error)
        }
    }
}

/// One local goal for the TUI: preview the plan, admit it against the
/// selected calibrated model, run it, and save the receipt. Mirrors `run`
/// without terminal prompts; `on_plan` sees the reviewed scope first and
/// `progress` sees every intermediate receipt.
pub(crate) async fn run_goal(
    goal: String,
    repository: PathBuf,
    host_approved: bool,
    on_plan: impl FnOnce(&LocalPlan),
    progress: impl FnMut(&LocalRunReceipt),
) -> Result<LocalRunReceipt> {
    let request = LocalRunRequest {
        goal,
        repository,
        files: vec![],
        new_file: None,
        editable_existing: vec![],
        checks: vec![],
        preparation: None,
        approve_host_execution: false,
        allow_unverified_runtime: false,
        budget: Default::default(),
        expected_source_hashes: Default::default(),
        expected_baseline_sha256: None,
    };
    let mut plan = phonton_worker::local_run::plan::preview(request).await?;
    let selection = plan_model_selection(&mut plan).await?;
    adjust_reviewed_plan_budget(&mut plan, selection.as_ref(), false);
    on_plan(&plan);
    let selection = selection.ok_or_else(|| {
        anyhow!("No calibrated local model is selected. Run `phonton models` to pick one")
    })?;
    plan.request.approve_host_execution = host_approved;
    let request = plan.request;
    let (_lease, profile, runtime_guard, directory) = prepare(&request, None, &selection).await?;
    let attempt = begin_attempt(
        &directory,
        &request,
        &profile,
        &crate::models_cli::state_path()?,
    )?;
    let started = std::time::Instant::now();
    match phonton_worker::local_run::run(request, profile, runtime_guard, &directory, progress)
        .await
    {
        Ok(receipt) => {
            write_end_receipt(&directory, &receipt)?;
            Ok(receipt)
        }
        Err(error) => {
            record_end(
                &directory,
                &attempt,
                &error.to_string(),
                false,
                started.elapsed().as_millis() as u64,
            );
            Err(error.into())
        }
    }
}

/// Apply a reviewed local candidate to `repository` through the same RPC
/// the CLI and desktop use (hash-checked against the saved review).
pub(crate) async fn apply_selected(
    receipt: &LocalRunReceipt,
    repository: &Path,
) -> Result<LocalApplyReceipt> {
    let number = receipt
        .selected_candidate
        .ok_or_else(|| anyhow!("No selected candidate to apply"))?;
    let hash = receipt
        .candidates
        .iter()
        .find(|c| c.number == number)
        .and_then(|c| c.content_sha256.clone())
        .ok_or_else(|| anyhow!("Selected candidate has no saved hash"))?;
    Ok(serde_json::from_value(
        rpc(
            "local.run.apply",
            json!({"id": receipt.id, "candidate_number": number, "expected_candidate_sha256": hash, "expected_repository": repository}),
        )
        .await?,
    )?)
}

struct GoalOptions {
    request: LocalRunRequest,
    preview_only: bool,
    approve_plan: bool,
    host_approved: bool,
    runtime_approved: bool,
}
fn parse_goal(args: &[String]) -> Result<GoalOptions> {
    let preview_only = args.first().is_some_and(|s| s == "--plan");
    let start = usize::from(preview_only);
    let goal = args
        .get(start)
        .filter(|s| !s.starts_with("--") && !s.trim().is_empty())
        .ok_or_else(|| anyhow!("{LOCAL_GOAL_USAGE}"))?;
    let mut options = GoalOptions {
        request: LocalRunRequest {
            goal: goal.clone(),
            repository: std::env::current_dir()?,
            files: vec![],
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: false,
            allow_unverified_runtime: false,
            budget: Default::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        },
        preview_only,
        approve_plan: false,
        host_approved: false,
        runtime_approved: false,
    };
    let mut index = start + 1;
    while let Some(flag) = args.get(index) {
        match flag.as_str() {
            "--yes" => options.approve_plan = true,
            "--allow-host-checks" => options.host_approved = true,
            "--allow-unverified-runtime" => options.runtime_approved = true,
            "--repo" | "--files" | "--new-file" | "--edit-existing" | "--check" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow!("{flag} requires a value"))?;
                match flag.as_str() {
                    "--repo" => options.request.repository = value.into(),
                    "--files" => {
                        options.request.files = value
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(PathBuf::from)
                            .collect()
                    }
                    "--new-file" => options.request.new_file = Some(value.into()),
                    "--edit-existing" => {
                        options.request.editable_existing = value
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(PathBuf::from)
                            .collect()
                    }
                    _ => {
                        let parts: Vec<String> = serde_json::from_str(value)?;
                        let (program, rest) = parts
                            .split_first()
                            .ok_or_else(|| anyhow!("Check command cannot be empty"))?;
                        options
                            .request
                            .checks
                            .push(phonton_types::local_run::LocalCheck {
                                program: program.clone(),
                                args: rest.to_vec(),
                            });
                    }
                }
            }
            _ => bail!("Unknown local goal option: {flag}"),
        }
        index += 1;
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State as AxumState,
        routing::{get, post},
        Json, Router,
    };

    struct FixtureRuntime {
        name: String,
        digest: String,
        version: String,
        context_tokens: u32,
    }

    async fn fixture_version(
        AxumState(state): AxumState<Arc<Mutex<FixtureRuntime>>>,
    ) -> Json<Value> {
        let version = state.lock().unwrap().version.clone();
        Json(json!({"version": version}))
    }

    async fn fixture_tags(AxumState(state): AxumState<Arc<Mutex<FixtureRuntime>>>) -> Json<Value> {
        let observed = state.lock().unwrap();
        Json(
            json!({"models": [{"name": observed.name.clone(), "digest": observed.digest.clone(), "size": 1048576}]}),
        )
    }

    async fn fixture_show(AxumState(state): AxumState<Arc<Mutex<FixtureRuntime>>>) -> Json<Value> {
        let context_tokens = state.lock().unwrap().context_tokens;
        Json(
            json!({"model_info": {"general.architecture": "fixture", "fixture.context_length": context_tokens}, "details": {"format": "gguf"}}),
        )
    }

    #[test]
    fn search_replace_review_budget_covers_best_case_scope_within_caps() {
        let selection = ReviewedModelSelection {
            model: "fixture:latest".into(),
            digest: "sha256:fixture".into(),
            runtime_version: "0.34.2".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            context_tokens: 4096,
            output_tokens: 1024,
            protocol: Some(EditProtocol::SearchReplace),
            profile_sha256: "fixture".into(),
        };
        let make_plan = |files: usize, checks: usize| {
            let request: LocalRunRequest = serde_json::from_value(json!({
                "goal":"Update the scoped files",
                "repository":".",
                "files":(0..files).map(|n| format!("src/file-{n}.rs")).collect::<Vec<_>>(),
                "checks":(0..checks).map(|_| json!({"program":"cargo","args":["test"]})).collect::<Vec<_>>()
            }))
            .unwrap();
            LocalPlan {
                contract: phonton_worker::local_run::plan::contract(&request),
                request,
                files: Vec::new(),
                creation: None,
                warnings: Vec::new(),
            }
        };

        let mut five = make_plan(5, 4);
        adjust_reviewed_plan_budget(&mut five, Some(&selection), false);
        assert_eq!(five.request.budget.generations, 5);
        assert_eq!(five.request.budget.check_runs, 24);
        assert_eq!(five.request.budget.generated_tokens, 5120);
        assert!(five
            .warnings
            .iter()
            .any(|warning| warning.contains("best-case")));

        let mut eight = make_plan(8, 2);
        adjust_reviewed_plan_budget(&mut eight, Some(&selection), false);
        assert_eq!(eight.request.budget.generations, 8);
        assert_eq!(eight.request.budget.check_runs, 18);
        assert_eq!(eight.request.budget.generated_tokens, 8192);

        let mut capped = make_plan(8, 4);
        adjust_reviewed_plan_budget(&mut capped, Some(&selection), false);
        assert_eq!(capped.request.budget.check_runs, 32);
        assert!(capped
            .warnings
            .iter()
            .any(|warning| warning.contains("above the 32-slot cap")));

        let mut one = make_plan(1, 1);
        adjust_reviewed_plan_budget(&mut one, Some(&selection), false);
        assert_eq!(one.request.budget.generations, 4);
        assert_eq!(one.request.budget.generated_tokens, 4096);

        let mut explicit = make_plan(5, 1);
        adjust_reviewed_plan_budget(&mut explicit, Some(&selection), true);
        assert_eq!(explicit.request.budget.generations, 4);
        assert_eq!(explicit.request.budget.generated_tokens, 4096);
        assert!(explicit
            .warnings
            .iter()
            .any(|warning| warning.contains("explicit budget")));

        let mut tiny_explicit = make_plan(5, 1);
        tiny_explicit.request.budget.generations = 5;
        tiny_explicit.request.budget.generated_tokens = 128;
        adjust_reviewed_plan_budget(&mut tiny_explicit, Some(&selection), true);
        assert!(tiny_explicit
            .warnings
            .iter()
            .any(|warning| warning.contains("minimum 128-token")));

        let mut surplus_calls = make_plan(5, 1);
        surplus_calls.request.budget.generations = 8;
        surplus_calls.request.budget.generated_tokens = 2048;
        adjust_reviewed_plan_budget(&mut surplus_calls, Some(&selection), true);
        assert!(surplus_calls
            .warnings
            .iter()
            .any(|warning| warning.contains("minimum 128-token")));

        let mut diff = make_plan(5, 4);
        let mut diff_selection = selection;
        diff_selection.protocol = Some(EditProtocol::UnifiedDiff);
        adjust_reviewed_plan_budget(&mut diff, Some(&diff_selection), false);
        assert_eq!(diff.request.budget.generations, 4);
        assert!(diff.warnings.is_empty());
    }

    #[test]
    fn saved_run_mutation_requires_the_active_repository() {
        let fixture = tempfile::tempdir().unwrap();
        let saved = fixture.path().join("saved");
        let other = fixture.path().join("other");
        std::fs::create_dir(&saved).unwrap();
        std::fs::create_dir(&other).unwrap();
        assert!(require_active_repository(&saved, &saved.join(".")).is_ok());
        assert!(repositories_match(&saved, &saved.join(".")).unwrap());
        assert!(!repositories_match(&saved, &other).unwrap());
        assert!(require_active_repository(&saved, &other)
            .unwrap_err()
            .to_string()
            .contains("Open the saved run repository"));
        assert!(serde_json::from_value::<ApplyRequest>(json!({
            "id": uuid::Uuid::new_v4(),
            "candidate_number": 1,
            "expected_candidate_sha256": "sha"
        }))
        .is_err());
    }

    #[test]
    fn recent_saved_runs_are_bounded_and_ignore_invalid_markers() {
        let fixture = tempfile::tempdir().unwrap();
        let state_path = fixture.path().join("models.json");
        let root = legacy_root(&state_path).unwrap();
        std::fs::create_dir(&root).unwrap();
        let mut expected = HashSet::new();
        for number in 0..=MAX_RECENT_RUNS {
            let id = uuid::Uuid::new_v4().to_string();
            expected.insert(id.clone());
            let attempt = LocalRunAttempt {
                schema: 1,
                id: id.clone(),
                goal: format!("Fix issue {number}"),
                model: "local-model".into(),
                state: "started".into(),
                error: None,
            };
            save_attempt(&attempt_path(&root.join(id)), &attempt).unwrap();
        }
        let invalid_id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            root.join(format!("{invalid_id}.attempt.json")),
            b"truncated{",
        )
        .unwrap();
        let mismatched_id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            root.join(format!("{mismatched_id}.attempt.json")),
            serde_json::to_vec(&LocalRunAttempt {
                schema: 1,
                id: uuid::Uuid::new_v4().to_string(),
                goal: "Wrong identity".into(),
                model: "local-model".into(),
                state: "started".into(),
                error: None,
            })
            .unwrap(),
        )
        .unwrap();
        let list = list_saved_runs_at(&state_path).unwrap();
        assert_eq!(list.runs.len(), MAX_RECENT_RUNS);
        assert!(list.limited);
        assert!(list.runs.iter().all(|run| expected.contains(&run.id)));
        assert!(list
            .runs
            .windows(2)
            .all(|pair| pair[0].recorded_at_unix_ms >= pair[1].recorded_at_unix_ms));
    }
    #[test]
    fn recent_index_keeps_new_attempts_visible_when_fallback_scan_is_limited() {
        let fixture = tempfile::tempdir().unwrap();
        let state_path = fixture.path().join("models.json");
        let root = legacy_root(&state_path).unwrap();
        std::fs::create_dir(&root).unwrap();
        let mut ids = Vec::new();
        for number in 0..2 {
            let id = uuid::Uuid::new_v4().to_string();
            let attempt = LocalRunAttempt {
                schema: 1,
                id: id.clone(),
                goal: format!("Goal {number}"),
                model: "local-model".into(),
                state: "started".into(),
                error: None,
            };
            save_attempt(&attempt_path(&root.join(&id)), &attempt).unwrap();
            save_recent_index(&state_path, &attempt).unwrap();
            ids.push(id);
        }
        let indexed = read_recent_index(&state_path);
        assert_eq!(indexed.len(), 2);
        assert_eq!(indexed[0].id, ids[1]);
        let list = list_saved_runs_at_with_limits(&state_path, 0, 0).unwrap();
        assert!(list.limited);
        assert_eq!(list.runs.len(), 2);
        assert!(list.runs.iter().any(|run| run.id == ids[0]));
        assert!(list.runs.iter().any(|run| run.id == ids[1]));
        for number in 2..MAX_RECENT_RUNS {
            let id = uuid::Uuid::new_v4().to_string();
            let attempt = LocalRunAttempt {
                schema: 1,
                id: id.clone(),
                goal: format!("Goal {number}"),
                model: "local-model".into(),
                state: "started".into(),
                error: None,
            };
            save_attempt(&attempt_path(&root.join(id)), &attempt).unwrap();
            save_recent_index(&state_path, &attempt).unwrap();
        }
        let full = list_saved_runs_at(&state_path).unwrap();
        assert_eq!(full.runs.len(), MAX_RECENT_RUNS);
        // Limited means the display cap was filled, even if no 13th run exists.
        assert!(full.limited);
    }
    #[test]
    fn corrupt_receipt_is_not_advertised_from_an_attempt_marker() {
        let fixture = tempfile::tempdir().unwrap();
        let state_path = fixture.path().join("models.json");
        let root = legacy_root(&state_path).unwrap();
        std::fs::create_dir(&root).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let directory = root.join(&id);
        std::fs::create_dir(&directory).unwrap();
        save_attempt(
            &attempt_path(&directory),
            &LocalRunAttempt {
                schema: 1,
                id,
                goal: "Unopenable candidate".into(),
                model: "local-model".into(),
                state: "started".into(),
                error: None,
            },
        )
        .unwrap();
        std::fs::write(directory.join("receipt.json"), b"truncated{").unwrap();
        assert!(list_saved_runs_at(&state_path).unwrap().runs.is_empty());
    }
    #[test]
    fn accepting_a_plan_does_not_approve_host_checks() {
        let options = parse_goal(&["Fix parsePort".into(), "--yes".into()]).unwrap();
        assert!(options.approve_plan);
        assert!(!options.host_approved);
        assert!(!options.runtime_approved);
        assert!(!options.request.approve_host_execution);
        let options = parse_goal(&[
            "--plan".into(),
            "Fix parsePort".into(),
            "--allow-host-checks".into(),
        ])
        .unwrap();
        assert!(options.preview_only);
        assert!(!options.request.approve_host_execution);
        let external = parse_goal(&[
            "Fix parsePort".into(),
            "--yes".into(),
            "--allow-unverified-runtime".into(),
        ])
        .unwrap();
        assert!(external.runtime_approved);
        assert!(parse_goal(&["goal".into(), "--check".into(), "[]".into()]).is_err());
        let creation = parse_goal(&[
            "Create add".into(),
            "--new-file".into(),
            "src/add.mjs".into(),
            "--yes".into(),
        ])
        .unwrap();
        assert_eq!(creation.request.new_file, Some("src/add.mjs".into()));
        assert!(creation.request.files.is_empty());
        assert!(!creation.request.approve_host_execution);
        let mixed = parse_goal(&[
            "Create helper and wire caller".into(),
            "--files".into(),
            "src/caller.py,src/context.py".into(),
            "--new-file".into(),
            "src/helper.py".into(),
            "--edit-existing".into(),
            "src/caller.py".into(),
        ])
        .unwrap();
        assert_eq!(
            mixed.request.editable_existing,
            vec![PathBuf::from("src/caller.py")]
        );
    }
    #[test]
    fn request_file_needs_current_invocation_approval_for_host_checks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("request.json");
        let mut request = LocalRunRequest {
            goal: "Fix arithmetic".into(),
            repository: dir.path().into(),
            files: vec!["code.py".into()],
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: true,
            allow_unverified_runtime: false,
            budget: SearchBudget::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        };
        std::fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
        let file = path.to_string_lossy().into_owned();
        assert!(request_from_file_args(&["--request".into(), file.clone()])
            .unwrap_err()
            .to_string()
            .contains("--allow-host-checks"));
        assert!(
            request_from_file_args(&[
                "--request".into(),
                file.clone(),
                "--allow-host-checks".into()
            ])
            .unwrap()
            .approve_host_execution
        );
        request.approve_host_execution = false;
        std::fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
        assert!(
            !request_from_file_args(&["--request".into(), file.clone()])
                .unwrap()
                .approve_host_execution
        );
        assert!(
            request_from_file_args(&["--request".into(), file, "--allow-host-checks".into()])
                .unwrap()
                .approve_host_execution
        );
        request.allow_unverified_runtime = true;
        std::fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
        let file = path.to_string_lossy().into_owned();
        assert!(request_from_file_args(&["--request".into(), file.clone()])
            .unwrap_err()
            .to_string()
            .contains("--allow-unverified-runtime"));
        assert!(
            request_from_file_args(&[
                "--request".into(),
                file,
                "--allow-unverified-runtime".into()
            ])
            .unwrap()
            .allow_unverified_runtime
        );
    }
    #[test]
    fn reviewed_plan_retains_source_and_model_binding_but_requires_fresh_host_approval() {
        use phonton_types::local_run::{LocalPlan, ScopeEvidence};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.json");
        let request = LocalRunRequest {
            goal: "Fix arithmetic".into(),
            repository: dir.path().into(),
            files: vec!["code.py".into()],
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: true,
            allow_unverified_runtime: false,
            budget: SearchBudget::default(),
            expected_source_hashes: [(PathBuf::from("code.py"), "source-hash".into())]
                .into_iter()
                .collect(),
            expected_baseline_sha256: Some("baseline-hash".into()),
        };
        let selection = ReviewedModelSelection {
            model: "fixture".into(),
            digest: "digest".into(),
            runtime_version: "runtime".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            context_tokens: 4096,
            output_tokens: 512,
            protocol: None,
            profile_sha256: "profile-hash".into(),
        };
        let mut reviewed = ReviewedLocalPlan {
            plan: LocalPlan {
                contract: phonton_worker::local_run::plan::contract(&request),
                request,
                files: vec![ScopeEvidence {
                    path: "code.py".into(),
                    reason: "fixture".into(),
                    source_sha256: "source-hash".into(),
                }],
                creation: None,
                warnings: vec![],
            },
            model_selection: Some(selection.clone()),
        };
        std::fs::write(&path, serde_json::to_vec(&reviewed).unwrap()).unwrap();
        let filename = path.to_string_lossy().into_owned();
        let file_hash = || format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
        let pinned_hash = file_hash();
        let (unapproved, saved_model) = request_from_reviewed_plan_args(&[
            "--reviewed-plan".into(),
            filename.clone(),
            "--sha256".into(),
            pinned_hash.clone(),
            "--yes".into(),
        ])
        .unwrap();
        assert!(!unapproved.approve_host_execution);
        assert_eq!(saved_model, selection);
        assert_eq!(
            unapproved.expected_baseline_sha256.as_deref(),
            Some("baseline-hash")
        );
        let (approved, _) = request_from_reviewed_plan_args(&[
            "--reviewed-plan".into(),
            filename.clone(),
            "--sha256".into(),
            pinned_hash.clone(),
            "--yes".into(),
            "--allow-host-checks".into(),
        ])
        .unwrap();
        assert!(approved.approve_host_execution);
        reviewed.plan.request.allow_unverified_runtime = true;
        std::fs::write(&path, serde_json::to_vec(&reviewed).unwrap()).unwrap();
        let runtime_hash = file_hash();
        assert!(request_from_reviewed_plan_args(&[
            "--reviewed-plan".into(),
            filename.clone(),
            "--sha256".into(),
            runtime_hash.clone(),
            "--yes".into(),
        ])
        .err()
        .unwrap()
        .to_string()
        .contains("--allow-unverified-runtime"));
        assert!(
            request_from_reviewed_plan_args(&[
                "--reviewed-plan".into(),
                filename.clone(),
                "--sha256".into(),
                runtime_hash,
                "--yes".into(),
                "--allow-unverified-runtime".into(),
            ])
            .unwrap()
            .0
            .allow_unverified_runtime
        );
        reviewed.plan.request.allow_unverified_runtime = false;
        std::fs::write(&path, serde_json::to_vec(&reviewed).unwrap()).unwrap();
        reviewed.plan.request.checks = vec![phonton_types::local_run::LocalCheck {
            program: "python".into(),
            args: vec!["-c".into(), "print(1)".into()],
        }];
        std::fs::write(&path, serde_json::to_vec(&reviewed).unwrap()).unwrap();
        assert!(request_from_reviewed_plan_args(&[
            "--reviewed-plan".into(),
            filename.clone(),
            "--sha256".into(),
            pinned_hash.clone(),
            "--yes".into(),
            "--allow-host-checks".into(),
        ])
        .unwrap_err()
        .to_string()
        .contains("changed after"));
        reviewed.plan.request.checks.clear();
        reviewed.plan.request.expected_source_hashes.clear();
        std::fs::write(&path, serde_json::to_vec(&reviewed).unwrap()).unwrap();
        assert!(request_from_reviewed_plan_args(&[
            "--reviewed-plan".into(),
            filename.clone(),
            "--sha256".into(),
            file_hash(),
            "--yes".into(),
        ])
        .is_err());
        reviewed.plan.request.expected_source_hashes =
            [(PathBuf::from("code.py"), "source-hash".into())]
                .into_iter()
                .collect();
        reviewed.model_selection = None;
        std::fs::write(&path, serde_json::to_vec(&reviewed).unwrap()).unwrap();
        assert!(request_from_reviewed_plan_args(&[
            "--reviewed-plan".into(),
            filename,
            "--sha256".into(),
            file_hash(),
            "--yes".into(),
        ])
        .is_err());
        std::fs::write(&path, vec![b' '; 256 * 1024 + 1]).unwrap();
        assert!(request_from_reviewed_plan_args(&[
            "--reviewed-plan".into(),
            path.to_string_lossy().into_owned(),
            "--sha256".into(),
            file_hash(),
            "--yes".into(),
        ])
        .unwrap_err()
        .to_string()
        .contains("256 KiB"));
    }
    use phonton_types::local::{HardwareSnapshot, ModelProfile};
    use phonton_types::local_run::SearchBudget;

    #[test]
    fn interruption_retains_reserved_checks_and_clears_selected_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = LocalRunReceipt {
            schema: 1,
            id: "fixture".into(),
            state: "verifying_1".into(),
            request: LocalRunRequest {
                goal: "fixture".into(),
                repository: ".".into(),
                files: vec!["code.py".into()],
                new_file: None,
                editable_existing: vec![],
                checks: vec![],
                preparation: None,
                approve_host_execution: false,
                allow_unverified_runtime: false,
                budget: SearchBudget::default(),
                expected_source_hashes: Default::default(),
                expected_baseline_sha256: None,
            },
            profile: ModelProfile {
                schema: 1,
                model: "fixture".into(),
                digest: "fixture".into(),
                runtime_version: "fixture".into(),
                endpoint: "http://127.0.0.1:11434".into(),
                context_tokens: 4096,
                output_tokens: 512,
                protocol: None,
                thinking: None,
                probes: vec![],
                hardware: HardwareSnapshot::default(),
                measured_at_unix: 0,
            },
            runtime_origin: phonton_types::local_run::RuntimeOrigin::Unknown,
            hardware: HardwareSnapshot::default(),
            resident_reuse: None,
            baseline_sha256: "fixture".into(),
            baseline_checks: vec![],
            candidates: vec![],
            hypotheses: vec![],
            selected_candidate: Some(1),
            checks_used: 1,
            generated_tokens_reserved: 512,
            model_calls_reserved: 0,
            elapsed_ms: 0,
            known_gaps: vec![],
            contract: None,
            git_index: None,
        };
        for unfinished_state in ["finalizing", "review_ready", "review_unverified"] {
            let mut saved = receipt.clone();
            saved.state = unfinished_state.into();
            recover_saved_receipt(dir.path(), &mut saved, false, false);
            assert_eq!(saved.state, "interrupted", "{unfinished_state}");
            assert_eq!(saved.selected_candidate, None, "{unfinished_state}");
        }
        let mut complete = receipt.clone();
        complete.state = "review_ready".into();
        recover_saved_receipt(dir.path(), &mut complete, false, true);
        assert_eq!(complete.state, "review_ready");
        assert_eq!(complete.selected_candidate, Some(1));
        let success = tempfile::tempdir().unwrap();
        std::fs::write(
            success.path().join("receipt.json"),
            serde_json::to_vec(&complete).unwrap(),
        )
        .unwrap();
        assert_eq!(
            read_saved_receipt(success.path(), false).unwrap()["state"],
            "interrupted"
        );
        let live_pending = read_saved_receipt(success.path(), true).unwrap();
        assert_eq!(live_pending["state"], "finalizing");
        assert!(live_pending["selected_candidate"].is_null());
        write_end_receipt(success.path(), &complete).unwrap();
        let reopened = read_saved_receipt(success.path(), false).unwrap();
        // An end marker is durable, but this fixture has no saved candidate
        // directory. Reopening must not claim that the missing bytes passed.
        assert_eq!(reopened["state"], "saved_review_unavailable");
        assert!(reopened["selected_candidate"].is_null());
        let index = tempfile::tempdir().unwrap();
        let state_path = index.path().join("models.json");
        let run_id = uuid::Uuid::new_v4().to_string();
        let saved_dir = legacy_root(&state_path).unwrap().join(&run_id);
        std::fs::create_dir_all(&saved_dir).unwrap();
        let mut saved = complete.clone();
        saved.id = run_id.clone();
        saved.request.goal = "Review this candidate later".into();
        write_end_receipt(&saved_dir, &saved).unwrap();
        save_attempt(
            &attempt_path(&saved_dir),
            &LocalRunAttempt {
                schema: 1,
                id: run_id.clone(),
                goal: saved.request.goal.clone(),
                model: saved.profile.model.clone(),
                state: "started".into(),
                error: None,
            },
        )
        .unwrap();
        let older_id = uuid::Uuid::new_v4().to_string();
        let older_dir = legacy_root(&state_path).unwrap().join(&older_id);
        std::fs::create_dir(&older_dir).unwrap();
        let mut older = complete.clone();
        older.id = older_id.clone();
        older.request.goal = "Receipt before attempt markers".into();
        write_end_receipt(&older_dir, &older).unwrap();
        let recent = list_saved_runs_at(&state_path).unwrap();
        assert_eq!(recent.runs.len(), 2);
        assert!(recent
            .runs
            .iter()
            .any(|run| run.id == run_id && run.goal == "Review this candidate later"));
        assert!(recent
            .runs
            .iter()
            .any(|run| run.id == older_id && run.goal == "Receipt before attempt markers"));
        let reopened_dir = saved_directory_at(&state_path, &run_id).unwrap();
        let reopened = read_saved_receipt(&reopened_dir, false).unwrap();
        assert_eq!(reopened["state"], "saved_review_unavailable");
        assert!(reopened["selected_candidate"].is_null());
        let mut pending_receipt = receipt.clone();
        pending_receipt.elapsed_ms = 100;
        pending_receipt
            .candidates
            .push(phonton_types::local_run::CandidateEvidence {
                number: 1,
                stage: phonton_types::local_run::CandidateStage::Complete,
                approach: "Replace the arithmetic branch".into(),
                directory: dir.path().join("candidate-1"),
                content_sha256: Some("pre-check-hash".into()),
                raw_output: "edit".into(),
                input_tokens: Some(10),
                output_tokens: Some(5),
                elapsed_ms: 20,
                checks: vec![phonton_types::local_run::CheckEvidence {
                    check: None,
                    purpose: phonton_types::local_run::CheckPurpose::Verification,
                    status: phonton_types::local::CheckStatus::Unavailable,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    detail: "Verification pending".into(),
                    elapsed_ms: 0,
                }],
                rejection: None,
                diff: "--- a/code.py\n+++ b/code.py\n@@ -1,1 +1,1 @@\n-old\n+new\n".into(),
                context: None,
                decision: None,
            });
        std::fs::write(
            dir.path().join("receipt.json"),
            serde_json::to_vec(&pending_receipt).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("check-2.json"),
            r#"{"number":2,"state":"started"}"#,
        )
        .unwrap();
        let crashed = read_saved_receipt(dir.path(), false).unwrap();
        assert_eq!(crashed["state"], "interrupted");
        assert_eq!(crashed["candidates"][0]["content_sha256"], "pre-check-hash");
        assert_eq!(
            crashed["candidates"][0]["checks"][0]["status"],
            "unavailable"
        );
        assert!(crashed["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap
                .as_str()
                .unwrap_or_default()
                .contains("pre-check observation")));
        let mut legacy = complete.clone();
        recover_saved_receipt(dir.path(), &mut legacy, false, false);
        assert_eq!(legacy.state, "interrupted");
        assert_eq!(legacy.checks_used, 2);
        let attempt = LocalRunAttempt {
            schema: 1,
            id: "fixture".into(),
            goal: receipt.request.goal.clone(),
            model: receipt.profile.model.clone(),
            state: "started".into(),
            error: None,
        };
        let interrupted = record_end(dir.path(), &attempt, "Cancelled by user", true, 300).unwrap();
        assert_eq!(interrupted.state, "interrupted");
        assert_eq!(interrupted.selected_candidate, None);
        assert_eq!(interrupted.checks_used, 2);
        assert_eq!(interrupted.elapsed_ms, 300);
        assert_eq!(
            interrupted.candidates[0].content_sha256.as_deref(),
            Some("pre-check-hash")
        );
        assert!(interrupted.candidates[0].diff.contains("+new"));
        assert_eq!(
            interrupted.candidates[0].checks[0].status,
            phonton_types::local::CheckStatus::Unavailable
        );
        let saved: LocalRunReceipt =
            serde_json::from_slice(&std::fs::read(dir.path().join("end.json")).unwrap()).unwrap();
        assert_eq!(saved.checks_used, 2);
        let reopened = read_saved_receipt(dir.path(), false).unwrap();
        assert_eq!(
            reopened["candidates"][0]["content_sha256"],
            "pre-check-hash"
        );
        assert_eq!(
            reopened["candidates"][0]["checks"][0]["status"],
            "unavailable"
        );
        assert_eq!(reopened["elapsed_ms"], 300);
        assert!(reopened["known_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|gap| gap
                .as_str()
                .unwrap_or_default()
                .contains("pre-check observation")));
        assert!(saved
            .known_gaps
            .iter()
            .any(|gap| gap == "Cancelled by user"));
    }

    #[test]
    fn preflight_refusal_and_crash_remain_readable_without_receipt() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join(uuid::Uuid::new_v4().to_string());
        let request = LocalRunRequest {
            goal: "Fix arithmetic".into(),
            repository: ".".into(),
            files: vec!["code.py".into()],
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: false,
            allow_unverified_runtime: false,
            budget: SearchBudget::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        };
        let profile = ModelProfile {
            schema: 1,
            model: "local-model".into(),
            digest: "digest".into(),
            runtime_version: "runtime".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            context_tokens: 4096,
            output_tokens: 512,
            protocol: None,
            thinking: None,
            probes: vec![],
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 0,
        };
        assert!(!has_evidence(&directory));
        let state_path = root.path().join("models.json");
        let blocked_index = recent_index_path(&state_path);
        std::fs::create_dir(&blocked_index).unwrap();
        let attempt = begin_attempt(&directory, &request, &profile, &state_path).unwrap();
        assert!(has_evidence(&directory));
        std::fs::remove_dir(&blocked_index).unwrap();
        assert!(read_recent_index(&state_path).is_empty());
        let blocked_attempt = root.path().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(attempt_path(&blocked_attempt).with_extension("json.tmp")).unwrap();
        assert!(begin_attempt(&blocked_attempt, &request, &profile, &state_path).is_err());
        assert!(!has_evidence(&blocked_attempt));
        assert!(read_recent_index(&state_path).is_empty());
        assert!(begin_attempt(&directory, &request, &profile, &state_path).is_err());
        assert!(!directory.exists());
        assert_eq!(read_attempt(&directory, true).unwrap().state, "started");
        let crashed = read_attempt(&directory, false).unwrap();
        assert_eq!(crashed.state, "interrupted_before_receipt");
        assert!(crashed.error.unwrap().contains("No project check"));
        assert!(record_end(&directory, &attempt, "RAM below 2 GiB", false, 0).is_none());
        let ended = read_attempt(&directory, false).unwrap();
        assert_eq!(ended.state, "ended_before_receipt");
        assert_eq!(ended.goal, "Fix arithmetic");
        assert_eq!(ended.error.as_deref(), Some("RAM below 2 GiB"));
    }

    #[test]
    fn run_state_inside_repository_is_refused_before_any_state_write() {
        let fixture = tempfile::tempdir().unwrap();
        let repository = fixture.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        let request = LocalRunRequest {
            goal: "Change source".into(),
            repository: repository.clone(),
            files: vec!["source.py".into()],
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: false,
            allow_unverified_runtime: false,
            budget: SearchBudget::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        };
        let inside = repository.join(".phonton").join("models.json");
        assert!(checked_runs_root(&request, &inside)
            .unwrap_err()
            .to_string()
            .contains("outside the repository"));
        assert!(!inside.parent().unwrap().exists());
        let outside = fixture.path().join("state").join("models.json");
        assert_eq!(
            checked_runs_root(&request, &outside).unwrap(),
            outside.parent().unwrap().join("runs")
        );
    }

    #[test]
    fn reviewed_model_change_is_refused_before_run_evidence() {
        use phonton_types::local::{EditProtocol, LocalSettings};
        let fixture = tempfile::tempdir().unwrap();
        let repository = fixture.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        let state_path = fixture.path().join("state/models.json");
        let request = LocalRunRequest {
            goal: "Fix arithmetic".into(),
            repository,
            files: vec!["code.py".into()],
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: false,
            allow_unverified_runtime: true,
            budget: SearchBudget::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        };
        let original = ModelProfile {
            schema: 1,
            model: "fixture:small".into(),
            digest: "first-digest".into(),
            runtime_version: "fixture-version".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            context_tokens: 4096,
            output_tokens: 512,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: None,
            probes: vec![],
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 1,
        };
        let mut settings = LocalSettings {
            schema: 1,
            endpoint: original.endpoint.clone(),
            active_model: Some(original.model.clone()),
            profiles: vec![original.clone()],
            calibration_attempt: None,
            install_attempts: Vec::new(),
            managed_root: None,
            managed_root_identity: None,
            managed_root_used: false,
            managed_runtime_installed: false,
        };
        phonton_local::storage::save(&state_path, &settings).unwrap();
        let reviewed = reviewed_selection(&original).unwrap();
        assert!(serde_json::from_value::<StartRequest>(json!({
            "id": uuid::Uuid::new_v4(), "request": request
        }))
        .is_err());

        settings.profiles[0].context_tokens = 2048;
        phonton_local::storage::save(&state_path, &settings).unwrap();
        assert!(prepare_at(&request, None, &reviewed, &state_path)
            .err()
            .unwrap()
            .to_string()
            .contains("Review the plan again"));
        assert!(!fixture.path().join("state/runs").exists());

        settings.profiles[0] = original.clone();
        settings.active_model = Some("other-model".into());
        settings.profiles.push(ModelProfile {
            model: "other-model".into(),
            digest: "other-digest".into(),
            ..original.clone()
        });
        phonton_local::storage::save(&state_path, &settings).unwrap();
        assert!(prepare_at(&request, None, &reviewed, &state_path)
            .err()
            .unwrap()
            .to_string()
            .contains("Review the plan again"));
        assert!(!fixture.path().join("state/runs").exists());

        settings.active_model = Some(original.model.clone());
        phonton_local::storage::save(&state_path, &settings).unwrap();
        let mut without_runtime_consent = request.clone();
        without_runtime_consent.allow_unverified_runtime = false;
        assert!(
            prepare_at(&without_runtime_consent, None, &reviewed, &state_path)
                .err()
                .unwrap()
                .to_string()
                .contains("external or unverified")
        );
        assert!(!fixture.path().join("state/runs").exists());
        let (lease, selected, guard, directory) =
            prepare_at(&request, None, &reviewed, &state_path).unwrap();
        assert_eq!(selected.model, original.model);
        assert_eq!(
            guard.origin(),
            phonton_types::local_run::RuntimeOrigin::ExternalUnverified
        );
        assert!(!directory.exists());
        assert!(!attempt_path(&directory).exists());
        drop(lease);

        #[cfg(windows)]
        {
            let chosen = fixture.path().join("chosen");
            std::fs::create_dir(&chosen).unwrap();
            let chosen = std::fs::canonicalize(chosen).unwrap();
            settings.schema = 2;
            settings.managed_root = Some(chosen.clone());
            settings.managed_root_identity =
                Some(phonton_local::disk::directory_identity(&chosen).unwrap());
            phonton_local::storage::save(&state_path, &settings).unwrap();
            let (lease, _, _, on_chosen_drive) =
                prepare_at(&request, None, &reviewed, &state_path).unwrap();
            assert_eq!(
                on_chosen_drive.parent(),
                Some(chosen.join("runs").as_path())
            );
            assert!(
                phonton_local::storage::load(&state_path)
                    .unwrap()
                    .managed_root_used
            );
            drop(lease);

            let old_id = uuid::Uuid::new_v4();
            std::fs::write(
                legacy_root(&state_path)
                    .unwrap()
                    .join(format!("{old_id}.attempt.json")),
                b"old run",
            )
            .unwrap();
            assert_eq!(
                saved_directory_at(&state_path, &old_id.to_string()).unwrap(),
                legacy_root(&state_path).unwrap().join(old_id.to_string())
            );
            assert!(prepare_at(&request, Some(old_id), &reviewed, &state_path)
                .err()
                .unwrap()
                .to_string()
                .contains("Run ID already has saved evidence"));
            assert!(!chosen.join("runs").join(old_id.to_string()).exists());
            let new_id = uuid::Uuid::new_v4().to_string();
            assert_eq!(
                saved_directory_at(&state_path, &new_id).unwrap(),
                chosen.join("runs").join(new_id)
            );
            let duplicate_id = uuid::Uuid::new_v4().to_string();
            let legacy_attempt = LocalRunAttempt {
                schema: 1,
                id: duplicate_id.clone(),
                goal: "Original legacy goal".into(),
                model: "legacy-model".into(),
                state: "started".into(),
                error: None,
            };
            let chosen_attempt = LocalRunAttempt {
                goal: "Copied chosen goal".into(),
                model: "chosen-model".into(),
                ..legacy_attempt.clone()
            };
            save_attempt(
                &attempt_path(&legacy_root(&state_path).unwrap().join(&duplicate_id)),
                &legacy_attempt,
            )
            .unwrap();
            save_attempt(
                &attempt_path(&chosen.join("runs").join(&duplicate_id)),
                &chosen_attempt,
            )
            .unwrap();
            save_recent_index(&state_path, &chosen_attempt).unwrap();
            let listed = list_saved_runs_at(&state_path).unwrap();
            let listed = listed
                .runs
                .iter()
                .find(|run| run.id == duplicate_id)
                .unwrap();
            let reopened = read_saved_receipt(
                &saved_directory_at(&state_path, &duplicate_id).unwrap(),
                false,
            )
            .unwrap();
            assert_eq!(listed.goal, reopened["goal"]);
            assert_eq!(listed.model, reopened["model"]);
        }
    }

    #[tokio::test]
    async fn stale_runtime_model_is_removed_from_plan_and_refused_before_run_evidence() {
        use phonton_types::local::{
            CheckStatus, LocalSettings, LocalThinkingMode, ModelProbe, ModelProfile,
        };
        let fixture = tempfile::tempdir().unwrap();
        let repository = fixture.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        std::fs::write(repository.join("code.py"), "def add(a, b): return a - b\n").unwrap();
        let state_path = fixture.path().join("state/models.json");
        let observed = Arc::new(Mutex::new(FixtureRuntime {
            name: "fixture:small".into(),
            digest: "sha256:first".into(),
            version: "0.1.0".into(),
            context_tokens: 8192,
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/api/version", get(fixture_version))
            .route("/api/tags", get(fixture_tags))
            .route("/api/show", post(fixture_show))
            .with_state(observed.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let profile = ModelProfile {
            schema: 2,
            model: "fixture:small".into(),
            digest: "sha256:first".into(),
            runtime_version: "0.1.0".into(),
            endpoint: endpoint.clone(),
            context_tokens: 4096,
            output_tokens: 512,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: Some(LocalThinkingMode::Off),
            probes: vec![ModelProbe {
                name: "SearchReplace edit".into(),
                status: CheckStatus::Passed,
                output: String::new(),
                detail: String::new(),
                input_tokens: Some(10),
                output_tokens: Some(10),
                elapsed_ms: 1,
            }],
            hardware: Default::default(),
            measured_at_unix: 1,
        };
        phonton_local::storage::save(
            &state_path,
            &LocalSettings {
                endpoint,
                active_model: Some(profile.model.clone()),
                profiles: vec![profile.clone()],
                ..Default::default()
            },
        )
        .unwrap();
        let reviewed = reviewed_selection(&profile).unwrap();
        assert_eq!(
            current_model_selection_at(&state_path).await.unwrap(),
            Some(reviewed.clone())
        );
        observed.lock().unwrap().name = "library/fixture:small".into();
        let mut aliased_settings = phonton_local::storage::load(&state_path).unwrap();
        aliased_settings.active_model = Some("library/fixture:small".into());
        phonton_local::storage::save(&state_path, &aliased_settings).unwrap();
        assert_eq!(
            current_model_selection_at(&state_path).await.unwrap(),
            Some(reviewed.clone())
        );
        let request = LocalRunRequest {
            goal: "Fix arithmetic".into(),
            repository,
            files: vec!["code.py".into()],
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: false,
            allow_unverified_runtime: true,
            budget: SearchBudget::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        };
        let mut plan = LocalPlan {
            contract: phonton_worker::local_run::plan::contract(&request),
            request: request.clone(),
            files: vec![],
            creation: None,
            warnings: vec![],
        };

        observed.lock().unwrap().digest = "sha256:replaced".into();
        assert!(plan_model_selection_at(&state_path, &mut plan)
            .await
            .is_none());
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("changed since calibration")));
        let id = uuid::Uuid::new_v4();
        assert!(prepare_live_at(&request, Some(id), &reviewed, &state_path)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("changed since calibration"));
        assert!(!fixture.path().join("state/runs").exists());

        observed.lock().unwrap().digest = profile.digest.clone();
        observed.lock().unwrap().version = "0.2.0".into();
        assert!(prepare_live_at(&request, Some(id), &reviewed, &state_path)
            .await
            .is_err());
        observed.lock().unwrap().version = profile.runtime_version.clone();
        observed.lock().unwrap().context_tokens = 2048;
        assert!(prepare_live_at(&request, Some(id), &reviewed, &state_path)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("Calibrated context exceeds"));
        assert!(!fixture.path().join("state/runs").exists());

        observed.lock().unwrap().context_tokens = 8192;
        let (lease, selected, _, directory) =
            prepare_live_at(&request, Some(id), &reviewed, &state_path)
                .await
                .unwrap();
        assert_eq!(selected.digest, profile.digest);
        assert!(!directory.exists());
        assert!(!attempt_path(&directory).exists());
        drop(lease);
        server.abort();
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn stale_managed_launch_cannot_be_overridden_by_runtime_consent() {
        let fixture = tempfile::tempdir().unwrap();
        let state_path = fixture.path().join("state").join("models.json");
        let root = state_path.parent().unwrap().join("runtime");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("managed-process.json"), b"truncated{").unwrap();
        let settings = phonton_types::local::LocalSettings::default();
        let error = runtime_guard_at(&state_path, &settings, true)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("could not be verified"));
        assert!(error.contains("stop it"));
        assert!(!fixture.path().join("state").join("runs").exists());
    }
}
