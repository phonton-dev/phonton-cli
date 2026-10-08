//! CLI and Desktop adapters for the shared local model subsystem.

use anyhow::{anyhow, bail, Result};
use phonton_local::{
    hardware,
    runtime::{
        local_endpoint, model_context_ceiling, validate_profile, validate_profile_context,
        LocalRuntime,
    },
    storage,
};
use phonton_types::local::{
    CatalogModel, CatalogSnapshot, CheckStatus, DownloadProgress, HardwareSnapshot, InstallAttempt,
    LocalModel, LocalSettings, ModelFit, ModelProfile,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) const MANAGED_MODEL_ENDPOINT: &str = "http://127.0.0.1:11434";

pub(crate) fn profile_sha256(profile: &ModelProfile) -> Result<String> {
    let hash = Sha256::digest(serde_json::to_vec(profile)?);
    Ok(format!("{hash:x}"))
}

/// Coalesce display updates only; transport and persisted operations keep their
/// actual events. Never smooth bytes or hide a restart/regression.
#[derive(Default)]
struct ProgressPrinter {
    last: Option<DownloadProgress>,
    at: Duration,
}

impl ProgressPrinter {
    fn line(&mut self, event: DownloadProgress, elapsed: Duration) -> Option<String> {
        if let Some(previous) = &self.last {
            let metadata_changed = previous.status != event.status
                || previous.digest != event.digest
                || previous.total != event.total;
            if !metadata_changed && previous.completed == event.completed {
                return None;
            }
            let backwards = event
                .completed
                .zip(previous.completed)
                .is_some_and(|(now, before)| now < before);
            let complete = event
                .completed
                .zip(event.total)
                .is_some_and(|(done, total)| done >= total);
            if !metadata_changed
                && !backwards
                && !complete
                && elapsed.saturating_sub(self.at) < Duration::from_millis(250)
            {
                return None;
            }
        }
        let count = |n: Option<u64>| n.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
        let digest = event
            .digest
            .as_ref()
            .map(|digest| format!(" {digest}"))
            .unwrap_or_default();
        // A stage with no byte counts ("verifying digest") prints as a stage.
        let line = if event.completed.is_none() && event.total.is_none() {
            format!("{}{digest}", event.status)
        } else {
            format!(
                "{}{digest}: {} / {} bytes",
                event.status,
                count(event.completed),
                count(event.total)
            )
        };
        self.last = Some(event);
        self.at = elapsed;
        Some(line)
    }
}

// Serializes inference/model mutations across sidecar requests. Hardware/status
// reads remain available while a model is downloading or being measured.
static MUTATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct Operation {
    id: String,
    kind: String,
    model: String,
    running: bool,
    progress: Option<DownloadProgress>,
    result: Option<Value>,
    error: Option<String>,
    cancel: Option<tokio::sync::watch::Sender<bool>>,
}

struct OperationExpectation {
    endpoint: String,
    root: PathBuf,
}

impl OperationExpectation {
    fn from_params(params: &Value) -> Result<Self> {
        let endpoint = params["expected_endpoint"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Refresh local model status before starting an operation"))?;
        let root = params["expected_storage_root"]
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Refresh local model status before starting an operation"))?;
        Ok(Self {
            endpoint: endpoint.to_owned(),
            root: PathBuf::from(root),
        })
    }

    fn check(&self, path: &Path, settings: &LocalSettings) -> Result<()> {
        if settings.endpoint != self.endpoint {
            bail!("Local model endpoint changed since status was displayed. Refresh Local models before retrying");
        }
        if managed_root(path, settings)? != self.root {
            bail!("Local model storage changed since status was displayed. Refresh Local models before retrying");
        }
        Ok(())
    }
}

fn operation() -> Arc<Mutex<Operation>> {
    static STATE: OnceLock<Arc<Mutex<Operation>>> = OnceLock::new();
    STATE
        .get_or_init(|| Arc::new(Mutex::new(Operation::default())))
        .clone()
}

pub fn state_path() -> Result<PathBuf> {
    // Explicit override keeps fixture tests and portable installations isolated.
    if let Some(path) = std::env::var_os("PHONTON_LOCAL_STATE") {
        return Ok(PathBuf::from(path));
    }
    Ok(phonton_extensions::phonton_home()
        .ok_or_else(|| anyhow!("Cannot locate user home directory"))?
        .join("local-models.json"))
}

pub fn settings() -> Result<LocalSettings> {
    Ok(storage::load(&state_path()?)?)
}

/// Start the Phonton-managed runtime when it is installed but not running
/// (after a reboot, for example), exactly as `phonton models setup` would.
/// Returns whether it was started; an external endpoint is never touched.
pub(crate) async fn ensure_managed_runtime() -> Result<bool> {
    let settings = settings()?;
    if settings.endpoint != MANAGED_MODEL_ENDPOINT || !settings.managed_runtime_installed {
        return Ok(false);
    }
    if LocalRuntime::new(&settings.endpoint)?
        .version()
        .await
        .is_ok()
    {
        return Ok(false);
    }
    mutate("setup", "", None, |_| {}).await?;
    Ok(true)
}

pub(crate) fn managed_root_at(
    path: &Path,
    settings: &LocalSettings,
    isolated_state: bool,
) -> Result<PathBuf> {
    let default = path
        .parent()
        .ok_or_else(|| anyhow!("Local model state has no parent directory"))?
        .join("runtime");
    if isolated_state {
        return Ok(default);
    }
    match &settings.managed_root {
        Some(root) => {
            if matches!(std::fs::symlink_metadata(root), Err(ref error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                validate_managed_root_location(root)?;
                return Ok(root.clone());
            }
            match validate_managed_root(root) {
                Ok(canonical) if canonical == *root => Ok(canonical),
                _ if !settings.managed_root_used && !current_root_has_data(root)? => {
                    validate_managed_root_location(root)?;
                    Ok(root.clone())
                }
                _ => bail!("Configured managed storage was redirected or is no longer a regular folder; inspect it before continuing"),
            }
        }
        None => Ok(default),
    }
}

fn managed_root(path: &Path, settings: &LocalSettings) -> Result<PathBuf> {
    managed_root_at(
        path,
        settings,
        std::env::var_os("PHONTON_LOCAL_STATE").is_some(),
    )
}

pub(crate) fn chosen_run_root_at(
    path: &Path,
    settings: &LocalSettings,
    isolated_state: bool,
) -> Result<Option<PathBuf>> {
    if isolated_state || settings.managed_root.is_none() {
        return Ok(None);
    }
    let root = managed_root_at(path, settings, false)?;
    let canonical = validate_managed_root(&root).map_err(|error| {
        anyhow!("Chosen local storage is unavailable for run evidence: {error}. Reconnect the original folder or choose another unused empty folder")
    })?;
    if canonical != root {
        bail!("Chosen local storage was redirected; run evidence cannot use it");
    }
    verify_managed_root_identity(&root, settings)?;
    let runs = root.join("runs");
    match std::fs::symlink_metadata(&runs) {
        Ok(_) => {
            let actual = validate_managed_root(&runs)
                .map_err(|error| anyhow!("Chosen run evidence folder is unsafe: {error}"))?;
            if actual != runs {
                bail!(
                    "Chosen run evidence folder was redirected; inspect it before running a goal"
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(Some(runs))
}

fn validate_managed_root_location(root: &Path) -> Result<()> {
    if !root.is_absolute()
        || root
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        bail!("Choose an absolute managed storage folder without . or .. components");
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        if !matches!(root.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        {
            bail!("Managed storage requires a local drive folder, not a network or device path");
        }
        if root.components().count() <= 2 {
            bail!("Choose a dedicated folder on the drive, not the drive root");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        bail!("Managed runtime storage selection currently supports Windows only")
    }
}

fn validate_managed_root(root: &Path) -> Result<PathBuf> {
    validate_managed_root_location(root)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use std::path::Prefix;
        const REPARSE_POINT: u32 = 0x400;
        let metadata = std::fs::symlink_metadata(root)?;
        if !metadata.is_dir() || metadata.file_attributes() & REPARSE_POINT != 0 {
            bail!(
                "Managed storage must be an ordinary local directory, not a file or linked folder"
            );
        }
        let canonical = std::fs::canonicalize(root)?;
        if !matches!(canonical.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        {
            bail!("Managed storage resolved outside a local drive");
        }
        if canonical.components().count() <= 2 {
            bail!("Choose a dedicated folder on the drive, not the drive root");
        }
        phonton_local::disk::require_local_drive_directory(&canonical)?;
        Ok(canonical)
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        bail!("Managed runtime storage selection currently supports Windows only")
    }
}

fn verify_managed_root_identity(root: &Path, settings: &LocalSettings) -> Result<()> {
    if settings.managed_root.is_none() {
        return Ok(());
    }
    let expected = settings.managed_root_identity.ok_or_else(|| {
        anyhow!(
            "Chosen managed folder has no saved identity. Re-select the empty folder before setup"
        )
    })?;
    #[cfg(windows)]
    {
        let actual = phonton_local::disk::directory_identity(root)?;
        if actual != expected {
            bail!("Chosen managed folder is a different directory or volume than the saved one. Reconnect the original folder; an unused empty choice can be changed");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (root, expected);
        bail!("Managed runtime storage identity is supported on Windows only");
    }
}

fn folder_has_data(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
        Ok(metadata) if !metadata.is_dir() => Ok(true),
        Ok(_) => Ok(std::fs::read_dir(path)?.next().transpose()?.is_some()),
    }
}

// An interrupted setup can leave only its empty, unlocked runtime lease file.
// It is bookkeeping, not a downloaded runtime or a model store. A held lease,
// a linked file, or any other entry still protects the current root from moves.
fn current_root_has_data(path: &Path) -> Result<bool> {
    runtime_root_has_data(path, false)
}

fn runtime_root_has_data(path: &Path, owns_install_lease: bool) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
        Ok(metadata) => metadata,
    };
    if !metadata.is_dir() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Ok(true);
        }
    }
    let mut entries = std::fs::read_dir(path)?;
    let Some(entry) = entries.next().transpose()? else {
        return Ok(false);
    };
    if entry.file_name() != "runtime-install-state.lock" || entries.next().transpose()?.is_some() {
        return Ok(true);
    }
    let lock_path = entry.path();
    let lock_metadata = std::fs::symlink_metadata(&lock_path)?;
    if !lock_metadata.is_file() || lock_metadata.len() != 0 {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if lock_metadata.file_attributes() & 0x400 != 0 {
            return Ok(true);
        }
    }
    if owns_install_lease {
        return Ok(false);
    }
    let lock = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
    {
        Ok(lock) => lock,
        Err(_) => return Ok(true),
    };
    if lock.try_lock().is_err() {
        return Ok(true);
    }
    // The directory may have changed before the lease was acquired.
    runtime_root_has_data(path, true)
}

fn storage_status(path: &Path, settings: &LocalSettings) -> Result<Value> {
    let isolated = std::env::var_os("PHONTON_LOCAL_STATE").is_some();
    let root = managed_root_at(path, settings, isolated)?;
    let source = if isolated {
        "override"
    } else if settings.managed_root.is_some() {
        "chosen"
    } else {
        "default"
    };
    let missing = settings.managed_root.is_some() && !isolated && !root.exists();
    let changed_identity = settings.managed_root.is_some()
        && !isolated
        && root.exists()
        && (validate_managed_root(&root).map_or(true, |canonical| canonical != root)
            || verify_managed_root_identity(&root, settings).is_err());
    let changeable = cfg!(all(windows, target_arch = "x86_64"))
        && !isolated
        && !settings.managed_root_used
        && !settings.managed_runtime_installed
        && !current_root_has_data(&root)?;
    let runs_path = if !isolated && settings.managed_root.is_some() {
        root.join("runs")
    } else {
        path.parent()
            .ok_or_else(|| anyhow!("Local model state has no parent directory"))?
            .join("runs")
    };
    let reason = if isolated {
        Some("PHONTON_LOCAL_STATE isolates this process; its runtime stays beside that state file.")
    } else if !cfg!(all(windows, target_arch = "x86_64")) {
        Some("Managed runtime setup currently supports Windows x64.")
    } else if missing && (settings.managed_root_used || settings.managed_runtime_installed) {
        Some("This previously used managed folder is unavailable. Reconnect it before setup; Phonton will not abandon its files.")
    } else if missing {
        Some("This unused managed folder is unavailable. Choose another empty folder before setup.")
    } else if changed_identity && (settings.managed_root_used || settings.managed_runtime_installed)
    {
        Some("This previously used managed folder changed identity. Reconnect the original folder before continuing.")
    } else if changed_identity {
        Some("This unused managed folder changed identity. Choose another empty folder before setup.")
    } else if settings.managed_root_used || settings.managed_runtime_installed {
        Some("Phonton has claimed this folder for managed files or run evidence. It will not switch automatically; inspect the folder before retrying or recovering it.")
    } else if !changeable {
        Some("This managed folder has files. Phonton will not move or hide an existing runtime or model store.")
    } else {
        None
    };
    let free = root
        .ancestors()
        .find(|candidate| candidate.is_dir())
        .and_then(|directory| phonton_local::disk::available_directory_bytes(directory).ok());
    Ok(
        json!({"root":root, "models_path":root.join("models"), "runs_path":runs_path, "source":source,
        "available_bytes":free, "runtime_setup_min_free_bytes":phonton_local::provision::MIN_INSTALL_FREE_BYTES,
        "runtime_installed":settings.managed_runtime_installed,
        "changeable":changeable, "reason":reason}),
    )
}

fn attach_pre_setup_storage(models: &mut [CatalogModel], storage: &Value, endpoint: &str) {
    for model in models.iter_mut() {
        model.pre_setup_storage = None;
    }
    if endpoint != MANAGED_MODEL_ENDPOINT
        || storage["changeable"] != true
        || storage["runtime_installed"] != false
        || !storage["reason"].is_null()
    {
        return;
    }
    let (Some(root), Some(available), Some(setup_min)) = (
        storage["root"].as_str(),
        storage["available_bytes"].as_u64(),
        storage["runtime_setup_min_free_bytes"].as_u64(),
    ) else {
        return;
    };
    for model in models {
        if model.error.is_none() {
            model.pre_setup_storage = model.download_bytes.and_then(|bytes| {
                phonton_local::disk::pre_setup_storage_plan(
                    PathBuf::from(root),
                    available,
                    setup_min,
                    bytes,
                )
                .ok()
            });
        }
    }
}

async fn catalog_snapshot() -> Result<CatalogSnapshot> {
    let hardware = hardware::detect().await;
    let mut models = LocalRuntime::catalog(&hardware).await?;
    // Catalog browsing is useful even when saved storage is unavailable. In
    // that case omit the optional disk plan and let models status explain it.
    if let Ok(path) = state_path() {
        if let Ok(settings) = storage::load(&path) {
            if let Ok(report) = storage_status(&path, &settings) {
                attach_pre_setup_storage(&mut models, &report, &settings.endpoint);
            }
        }
    }
    Ok(CatalogSnapshot { hardware, models })
}

fn catalog_cli_output(snapshot: CatalogSnapshot, include_hardware: bool) -> Result<Value> {
    if include_hardware {
        Ok(serde_json::to_value(snapshot)?)
    } else {
        Ok(serde_json::to_value(snapshot.models)?)
    }
}

fn set_managed_storage_at(path: &Path, requested: &Path, isolated_state: bool) -> Result<Value> {
    if isolated_state {
        bail!("PHONTON_LOCAL_STATE is set; managed storage follows that isolated state file");
    }
    let _lease = storage::acquire(path)?;
    let mut settings = storage::load(path)?;
    if settings.managed_root_used || settings.managed_runtime_installed {
        bail!("This local storage folder has been used. Phonton will not move or hide its runtime, models, or run evidence");
    }
    let current = managed_root_at(path, &settings, false)?;
    if current_root_has_data(&current)? {
        bail!("Current local storage folder has files. Phonton will not move or hide an existing runtime, model store, or run evidence");
    }
    let selected = validate_managed_root(requested)?;
    let selected_identity = phonton_local::disk::directory_identity(&selected)?;
    if settings.managed_root.is_some()
        && selected == current
        && settings.managed_root_identity == Some(selected_identity)
    {
        return Ok(json!({"root":current, "changed":false}));
    }
    if folder_has_data(&selected)? {
        bail!("Choose an empty dedicated folder for managed runtime, models, and run evidence");
    }
    // The state-file lease only coordinates callers sharing this state path.
    // Hold the runtime-root lease through the save so another state file using
    // this same root cannot begin an install between our check and switch.
    // A missing chosen root is left missing so its recorded identity is not
    // replaced with an empty folder on a different drive.
    let _runtime_lease = if current.exists() || settings.managed_root.is_none() {
        Some(storage::acquire(&current.join("runtime-install-state"))?)
    } else {
        None
    };
    if runtime_root_has_data(&current, _runtime_lease.is_some())? {
        bail!("Current local storage folder has files. Phonton will not move or hide an existing runtime, model store, or run evidence");
    }
    // Older engines do not know this field and would erase it on their next
    // state write. Schema 2 makes them refuse the state instead.
    settings.schema = settings.schema.max(2);
    settings.managed_root = Some(selected.clone());
    settings.managed_root_identity = Some(selected_identity);
    settings.managed_root_used = false;
    settings.managed_runtime_installed = false;
    storage::save(path, &settings)?;
    Ok(json!({"root":selected, "changed":true}))
}

fn set_managed_storage(requested: &str) -> Result<Value> {
    set_managed_storage_guarded(
        &state_path()?,
        Path::new(requested),
        std::env::var_os("PHONTON_LOCAL_STATE").is_some(),
        &operation(),
    )
}

fn set_managed_storage_guarded(
    path: &Path,
    requested: &Path,
    isolated_state: bool,
    shared: &Arc<Mutex<Operation>>,
) -> Result<Value> {
    let state = shared
        .lock()
        .map_err(|_| anyhow!("Model operation state unavailable"))?;
    if state.running {
        bail!("Another model operation is running. Wait or cancel it first.");
    }
    let _guard = MUTATION
        .try_lock()
        .map_err(|_| anyhow!("Another model operation is running. Wait or cancel it first."))?;
    set_managed_storage_at(path, requested, isolated_state)
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn previous_managed_runtime(root: &Path, installed: bool) -> bool {
    if installed {
        return true;
    }
    // Schema-2 state predating the installed marker can still contain a
    // verified portable runtime after its process receipt was lost.
    std::fs::read_dir(root).is_ok_and(|entries| {
        entries.filter_map(std::result::Result::ok).any(|entry| {
            entry.file_name().to_string_lossy().starts_with("ollama-")
                && std::fs::symlink_metadata(entry.path().join(".archive-sha256")).is_ok()
        })
    })
}

fn model_store_status(root: &Path, endpoint: &str, managed_runtime_installed: bool) -> Value {
    if endpoint != MANAGED_MODEL_ENDPOINT {
        return json!({"status":"unverified", "goal_run_blocked":false, "reason":"This explicitly configured loopback endpoint is external to Phonton's managed runtime; its model-store path and free space are unknown."});
    }
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        match phonton_local::managed_store::bind(root, endpoint) {
            Ok(Some(binding)) => match binding.available_bytes() {
                Ok(available) => {
                    json!({"status":"verified_managed", "goal_run_blocked":false, "pid":binding.pid(), "available_bytes":available})
                }
                Err(error) => {
                    json!({"status":"unverified", "recovery_required":true, "goal_run_blocked":true, "setup_retryable":phonton_local::managed_store::setup_retryable(root, endpoint), "reason":error.to_string()})
                }
            },
            Ok(None) if previous_managed_runtime(root, managed_runtime_installed) => {
                json!({"status":"unverified", "recovery_required":true, "goal_run_blocked":false, "setup_retryable":phonton_local::managed_store::setup_retryable(root, endpoint), "reason":"This previously used managed folder has no verifiable launch receipt at its saved path. Restore the original folder and rerun managed setup before downloading."})
            }
            Ok(None) => {
                json!({"status":"unverified", "goal_run_blocked":false, "reason":"No Phonton-managed launch receipt matches this service. Its model-store path and free space are unknown."})
            }
            Err(error) => {
                json!({"status":"unverified", "recovery_required":true, "goal_run_blocked":true, "setup_retryable":phonton_local::managed_store::setup_retryable(root, endpoint), "reason":error.to_string()})
            }
        }
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        let _ = (root, endpoint, managed_runtime_installed);
        json!({"status":"unverified", "goal_run_blocked":false, "reason":"Managed model-store verification is not implemented on this platform."})
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn require_setup_recovery_allowed(root: &Path, managed_runtime_installed: bool) -> Result<()> {
    let store = model_store_status(root, MANAGED_MODEL_ENDPOINT, managed_runtime_installed);
    if store["recovery_required"] == true && store["setup_retryable"] != true {
        bail!(
            "Managed setup cannot safely replace the saved launch receipt or model store: {}. Reconnect the original storage or repair the unsafe path, then retry. If you moved this folder on purpose, delete {} and rerun phonton models setup.",
            store["reason"].as_str().unwrap_or("storage verification failed"),
            root.join("managed-process.json").display()
        );
    }
    Ok(())
}

fn managed_local_only(path: &Path, settings: &LocalSettings) -> bool {
    let Ok(root) = managed_root_at(
        path,
        settings,
        std::env::var_os("PHONTON_LOCAL_STATE").is_some(),
    ) else {
        return false;
    };
    if settings.managed_root.is_some() && verify_managed_root_identity(&root, settings).is_err() {
        return false;
    }
    model_store_status(
        &root,
        &settings.endpoint,
        settings.managed_runtime_installed,
    )["status"]
        == "verified_managed"
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn existing_runtime_setup_at(
    path: &Path,
    settings: &mut LocalSettings,
    root: &Path,
    version: &str,
) -> Result<Value> {
    match phonton_local::managed_store::bind(root, &settings.endpoint) {
        Ok(Some(binding)) => {
            if !settings.managed_runtime_installed {
                settings.schema = settings.schema.max(3);
                if settings.managed_root.is_some() {
                    settings.managed_root_used = true;
                }
                settings.managed_runtime_installed = true;
                storage::save(path, settings)?;
            }
            Ok(json!({
                "runtime_version":version, "existing":true,
                "managed_origin":"verified_previous_launch", "pid":binding.pid(),
                "detail":"The responding Ollama process matches an earlier Phonton-managed launch and its model store is currently verified."
            }))
        }
        Ok(None) if previous_managed_runtime(root, settings.managed_runtime_installed) => bail!("A service answered on 127.0.0.1:11434, but Phonton's previous managed launch receipt is missing. Stop the process using that port, reconnect the original managed folder if it moved, then rerun models setup. Phonton will not stop an unverified process or discard its managed files automatically"),
        Ok(None) => Ok(json!({
            "runtime_version":version,
            "existing":true,
            "managed_origin":"unverified",
            "detail":"An existing Ollama-compatible service answered on the loopback endpoint. Phonton did not install or start it and has not verified which process owns the listener."
        })),
        Err(error) => bail!("A service answered on 127.0.0.1:11434, but Phonton's saved managed launch cannot be verified: {error}. Stop the process using that port, reconnect the original managed folder if it moved, then rerun models setup. Phonton will not stop an unverified process or discard its launch receipt automatically"),
    }
}

async fn install_model(
    runtime: &LocalRuntime,
    root: &Path,
    endpoint: &str,
    model: &str,
    managed_runtime_installed: bool,
    mut progress: impl FnMut(DownloadProgress),
) -> Result<Value> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    if endpoint == MANAGED_MODEL_ENDPOINT {
        let binding = phonton_local::managed_store::bind(root, endpoint)
            .map_err(|error| anyhow!("A previously managed Ollama service can no longer be verified: {error}. Stop that service and rerun models setup, or configure a different loopback endpoint for an external runtime."))?;
        if let Some(binding) = binding {
            let admission = LocalRuntime::managed_download_admission(model, &binding).await?;
            let reserve = admission.reserve_bytes;
            progress(DownloadProgress {
                status: format!(
                    "Managed store verified; {} manifest bytes, {} verified complete blob bytes credited, {} remaining bytes and {} reserve bytes",
                    admission.manifest_bytes, admission.credited_existing_bytes,
                    admission.remaining_bytes, reserve
                ),
                ..Default::default()
            });
            let installed = runtime
                    .pull_checked(model, progress, || {
                        let free = binding.available_bytes()?;
                        if free < reserve {
                            return Err(phonton_local::LocalError::Invalid(format!(
                                "Managed model store dropped below its {} byte safety reserve during download. The partial pull may be resumed after freeing space.", reserve
                            )));
                        }
                        Ok(())
                    })
                    .await?;
            return Ok(
                json!({"installed": installed.name, "digest": installed.digest, "size_bytes": installed.size_bytes, "model_store":"verified_managed", "manifest_bytes":admission.manifest_bytes, "reserve_bytes":reserve, "download_admission":admission}),
            );
        }
        if previous_managed_runtime(root, managed_runtime_installed) {
            bail!("The previously used managed model store has no verifiable launch receipt. Restore its folder and rerun managed setup before downloading, or explicitly configure an external loopback endpoint");
        }
    }
    let store = model_store_status(root, endpoint, managed_runtime_installed);
    progress(DownloadProgress {
        status: "Model store unverified; Ollama will report download or disk errors".into(),
        ..Default::default()
    });
    let installed = runtime.pull(model, progress).await?;
    Ok(
        json!({"installed": installed.name, "digest": installed.digest, "size_bytes": installed.size_bytes, "model_store":"unverified", "store_reason":store["reason"]}),
    )
}

fn change_endpoint(settings: &mut LocalSettings, origin: &str) -> Result<bool> {
    let normalized = local_endpoint(origin)?;
    if normalized == settings.endpoint {
        return Ok(false);
    }
    settings.endpoint = normalized;
    // A model selected on a different runtime is not a selection here. Keep
    // its endpoint-bound profile as inspectable evidence.
    settings.active_model = None;
    Ok(true)
}

fn set_endpoint(origin: &str) -> Result<Value> {
    let path = state_path()?;
    set_endpoint_guarded(&path, origin, &operation())
}

fn set_endpoint_guarded(
    path: &Path,
    origin: &str,
    shared: &Arc<Mutex<Operation>>,
) -> Result<Value> {
    // Admission is serialized with models.start, which marks running before it
    // spawns mutate. Checking MUTATION alone would miss that short interval.
    let operation = shared
        .lock()
        .map_err(|_| anyhow!("Model operation state unavailable"))?;
    if operation.running {
        bail!("Another model operation is running. Wait or cancel it first.");
    }
    let _guard = MUTATION
        .try_lock()
        .map_err(|_| anyhow!("Another model operation is running. Wait or cancel it first."))?;
    set_endpoint_at(path, origin)
}

fn set_endpoint_at(path: &Path, origin: &str) -> Result<Value> {
    let _lease = storage::acquire(path)?;
    let mut settings = storage::load(path)?;
    let changed = change_endpoint(&mut settings, origin)?;
    if changed {
        storage::save(path, &settings)?;
    }
    Ok(
        json!({"endpoint": settings.endpoint, "changed": changed, "active_model": settings.active_model, "local_only": managed_local_only(path, &settings), "loopback_only": true}),
    )
}

fn profile_for_status<'a>(
    settings: &'a LocalSettings,
    model: &LocalModel,
    version: &str,
) -> (Option<&'a ModelProfile>, Option<String>) {
    let mut matching = settings
        .profiles
        .iter()
        .filter(|p| model_matches(&p.model, &model.name) && p.endpoint == settings.endpoint);
    let Some(profile) = matching.next() else {
        return (
            None,
            settings
                .profiles
                .iter()
                .any(|p| model_matches(&p.model, &model.name))
                .then(|| "This model was calibrated at another local endpoint. Calibrate it here before selection.".to_string()),
        );
    };
    if matching.next().is_some() {
        return (
            None,
            Some(
                "Multiple saved calibrations identify this model; recalibrate before selection."
                    .into(),
            ),
        );
    }
    match validate_profile(profile, model, &settings.endpoint, version) {
        Ok(()) => (Some(profile), None),
        Err(error) => (None, Some(error.to_string())),
    }
}

fn unusable_calibration_evidence<'a>(
    settings: &'a LocalSettings,
    model: &LocalModel,
    ready_profile: Option<&ModelProfile>,
) -> Option<&'a ModelProfile> {
    if ready_profile.is_some() {
        return None;
    }
    // Keep readiness strict while preserving raw probe output from this origin.
    // A changed digest/runtime stays diagnostic evidence, never a valid profile.
    settings
        .profiles
        .iter()
        .find(|p| model_matches(&p.model, &model.name) && p.endpoint == settings.endpoint)
}

fn calibration_exit_code(verb: &str, value: &Value) -> i32 {
    if verb == "calibrate" && !value["protocol"].is_string() {
        2
    } else {
        0
    }
}

fn operation_cancellable(kind: &str) -> bool {
    matches!(kind, "setup" | "install" | "calibrate")
}

fn cancellation_message(kind: &str) -> &'static str {
    match kind {
        "setup" => "Setup cancellation requested. A download or runtime start may already have completed; refresh model status before retrying.",
        "install" => "Install cancellation requested. Ollama may retain partial or completed layers; refresh installed models before retrying.",
        "calibrate" => "Calibration cancellation requested. Refresh model status for any saved incomplete probe evidence before retrying.",
        _ => "Cancellation requested. Refresh model status before retrying.",
    }
}

async fn await_model_mutation<F, C>(kind: &str, work: F, cancel: C) -> Result<Value>
where
    F: Future<Output = Result<Value>>,
    C: Future<Output = ()>,
{
    tokio::pin!(work);
    tokio::pin!(cancel);
    tokio::select! {
        biased;
        result = &mut work => result,
        _ = &mut cancel => {
            if operation_cancellable(kind) {
                Err(anyhow!(cancellation_message(kind)))
            } else {
                // Selection, deselection and removal may already have changed
                // saved state or the runtime. Observe their actual outcome.
                eprintln!("{kind} is finishing; waiting for its result before reporting completion.");
                work.await
            }
        }
    }
}

async fn ctrl_c_requested() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

fn request_cancel(shared: &Arc<Mutex<Operation>>, id: &str) -> Result<Value> {
    let state = shared
        .lock()
        .map_err(|_| anyhow!("Model operation state unavailable"))?;
    if id != state.id {
        bail!("Operation changed; refresh before cancelling");
    }
    if !state.running {
        return Ok(json!({"cancel_requested": false, "reason": "Operation already finished"}));
    }
    if !operation_cancellable(&state.kind) {
        return Ok(
            json!({"cancel_requested": false, "reason": "Selection, deselection and removal finish before their result is reported. Refresh model status afterward."}),
        );
    }
    let requested = state
        .cancel
        .as_ref()
        .is_some_and(|cancel| cancel.send(true).is_ok());
    Ok(json!({"cancel_requested": requested}))
}

fn context_param(params: &Value) -> Result<Option<u32>> {
    match params.get("context") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            Ok(Some(u32::try_from(value.as_u64().ok_or_else(|| {
                anyhow!("Context must be an integer token count")
            })?)?))
        }
    }
}

fn fit_for_status(
    model: &LocalModel,
    hardware: &HardwareSnapshot,
    profile: Option<&ModelProfile>,
    requested_context: Option<u32>,
    model_ceiling: Option<u32>,
) -> ModelFit {
    let recommended = phonton_local::recommend_context(model.size_bytes, hardware, model_ceiling);
    let context = requested_context
        .or_else(|| profile.map(|p| p.context_tokens))
        .or(recommended)
        .unwrap_or_else(|| model_ceiling.unwrap_or(4096).clamp(2048, 4096));
    let mut fit = phonton_local::estimate_fit_for_context(model.size_bytes, hardware, context);
    fit.suggested_context = recommended;
    fit
}

#[derive(Clone, Debug)]
enum ModelMetadataError {
    Rejected(String),
    BudgetExpired(String),
}

impl ModelMetadataError {
    fn message(&self) -> &str {
        match self {
            Self::Rejected(message) | Self::BudgetExpired(message) => message,
        }
    }
}

type ModelMetadata = std::result::Result<Value, ModelMetadataError>;

fn profile_and_context_for_status<'a>(
    settings: &'a LocalSettings,
    model: &LocalModel,
    version: &str,
    metadata: &ModelMetadata,
) -> (
    Option<&'a ModelProfile>,
    Option<String>,
    Option<u32>,
    Option<String>,
) {
    let (mut profile, mut profile_error) = profile_for_status(settings, model, version);
    match metadata {
        Ok(metadata) => {
            let limit = model_context_ceiling(metadata);
            if let Some(selected) = profile {
                if let Err(error) = validate_profile_context(selected, metadata) {
                    profile = None;
                    profile_error = Some(error.to_string());
                }
            }
            let context_error = limit.is_none().then(|| {
                "Runtime did not report a supported model context limit; automatic calibration is unavailable.".to_string()
            });
            (profile, profile_error, limit, context_error)
        }
        // A shared status budget expiring does not undo digest/version/probe
        // evidence. Selection and goal execution revalidate current metadata.
        Err(error @ ModelMetadataError::BudgetExpired(_)) => (
            profile,
            profile_error,
            None,
            Some(error.message().to_string()),
        ),
        Err(error @ ModelMetadataError::Rejected(_)) => {
            let message = error.message().to_string();
            (
                None,
                profile_error.or_else(|| Some(message.clone())),
                None,
                Some(message),
            )
        }
    }
}

fn spawn_status_metadata(
    tasks: &mut tokio::task::JoinSet<(usize, ModelMetadata)>,
    runtime: &LocalRuntime,
    model: &LocalModel,
    index: usize,
) {
    let runtime = runtime.clone();
    let name = model.name.clone();
    tasks.spawn(async move {
        let metadata = runtime
            .show_local(&name)
            .await
            .map_err(|e| ModelMetadataError::Rejected(e.to_string()));
        (index, metadata)
    });
}

async fn fetch_model_metadata(
    runtime: &LocalRuntime,
    models: &[LocalModel],
    active_model: Option<&str>,
    budget: Duration,
) -> Vec<ModelMetadata> {
    const MAX_IN_FLIGHT: usize = 4;
    let mut order: Vec<usize> = (0..models.len()).collect();
    if let Some(active) = active_model {
        if let Some(position) = order
            .iter()
            .position(|&index| model_matches(active, &models[index].name))
        {
            let active_index = order.remove(position);
            order.insert(0, active_index);
        }
    }
    let mut results: Vec<Option<ModelMetadata>> = vec![None; models.len()];
    let mut tasks = tokio::task::JoinSet::new();
    let mut next = 0;
    while next < order.len() && tasks.len() < MAX_IN_FLIGHT {
        let index = order[next];
        spawn_status_metadata(&mut tasks, runtime, &models[index], index);
        next += 1;
    }
    let deadline = Instant::now() + budget;
    while !tasks.is_empty() {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match tokio::time::timeout(remaining, tasks.join_next()).await {
            Ok(Some(Ok((index, metadata)))) => results[index] = Some(metadata),
            Ok(Some(Err(_))) => {}
            Ok(None) | Err(_) => break,
        }
        if next < order.len() {
            let index = order[next];
            spawn_status_metadata(&mut tasks, runtime, &models[index], index);
            next += 1;
        }
    }
    tasks.abort_all();
    let timeout = format!(
        "Local model metadata did not finish within the {} ms status budget.",
        budget.as_millis()
    );
    results
        .into_iter()
        .map(|result| {
            result.unwrap_or_else(|| Err(ModelMetadataError::BudgetExpired(timeout.clone())))
        })
        .collect()
}

pub async fn status(requested_context: Option<u32>) -> Result<Value> {
    if requested_context.is_some_and(|context| !(2048..=32768).contains(&context)) {
        bail!("Context must be 2048..32768 tokens");
    }
    let path = state_path()?;
    let settings = storage::load(&path)?;
    let root = managed_root(&path, &settings)?;
    let model_store = if settings.managed_root.is_some()
        && settings.endpoint == MANAGED_MODEL_ENDPOINT
    {
        match verify_managed_root_identity(&root, &settings) {
            Ok(()) => model_store_status(
                &root,
                &settings.endpoint,
                settings.managed_runtime_installed,
            ),
            Err(error) => {
                json!({"status":"unverified", "recovery_required":true, "goal_run_blocked":true, "setup_retryable":false, "reason":error.to_string()})
            }
        }
    } else {
        model_store_status(
            &root,
            &settings.endpoint,
            settings.managed_runtime_installed,
        )
    };
    let managed_storage = storage_status(&path, &settings)?;
    let runtime = LocalRuntime::new(&settings.endpoint)?;
    let hardware = hardware::detect().await;
    let version = runtime.version().await;
    let (runtime_version, error, models, inventory_warnings) = match version {
        Ok(version) => match runtime.inventory().await {
            Ok(inventory) => (Some(version), None, inventory.models, inventory.warnings),
            Err(e) => (Some(version), Some(e.to_string()), Vec::new(), Vec::new()),
        },
        Err(e) => (None, Some(e.to_string()), Vec::new(), Vec::new()),
    };
    let install_attempts: Vec<Value> = settings
        .install_attempts
        .iter()
        .map(|attempt| {
            json!({
                "attempt": attempt,
                "reconciliation": reconcile_install_attempt(
                    attempt,
                    &settings.endpoint,
                    &models,
                    error.is_none() && inventory_warnings.is_empty(),
                ),
            })
        })
        .collect();
    let metadata = fetch_model_metadata(
        &runtime,
        &models,
        settings.active_model.as_deref(),
        Duration::from_secs(5),
    )
    .await;
    let mut rows = Vec::with_capacity(models.len());
    for (model, metadata) in models.iter().zip(metadata) {
        let (mut profile, mut profile_error, model_context_limit, context_error) =
            profile_and_context_for_status(
                &settings,
                model,
                runtime_version.as_deref().unwrap_or(""),
                &metadata,
            );
        if let Err(error) = phonton_local::runtime::find_installed_model(&models, &model.name) {
            profile = None;
            profile_error = Some(error.to_string());
        }
        let profile_sha256 = profile.map(profile_sha256).transpose()?;
        let mut fit = fit_for_status(
            model,
            &hardware,
            profile,
            requested_context,
            model_context_limit,
        );
        // A model already loaded in VRAM makes free VRAM look short of a cold
        // load. Say what is true: it is resident and reusable while loaded.
        if fit.status != phonton_types::local::FitStatus::LikelyFitsGpu && runtime_version.is_some()
        {
            if let Ok(Some(resident)) = runtime
                .resident(&model.name, &model.digest, fit.context_tokens)
                .await
            {
                if resident.size_vram_bytes >= resident.size_bytes && resident.size_bytes > 0 {
                    fit.status = phonton_types::local::FitStatus::LikelyFitsGpu;
                    fit.explanation = format!(
                        "Loaded in VRAM now with {} context tokens. Goals reuse it while it stays loaded; a later cold load is estimated again.",
                        resident.context_length
                    );
                }
            }
        }
        rows.push(json!({
            "model": model,
            "fit": fit,
            "model_context_limit": model_context_limit,
            "context_error": context_error,
            "profile": profile,
            "profile_sha256": profile_sha256,
            "profile_error": profile_error,
            "calibration_evidence": unusable_calibration_evidence(&settings, model, profile),
            "creation_status": profile.map(ModelProfile::creation_status).unwrap_or(CheckStatus::NotRun),
        }));
    }
    let local_only = model_store["status"] == "verified_managed";
    Ok(
        json!({ "schema": 2, "hardware": hardware, "endpoint": settings.endpoint, "runtime_version": runtime_version,
        "runtime_error": error, "inventory_warnings": inventory_warnings, "active_model": settings.active_model, "models": rows,
        "calibration_attempt": settings.calibration_attempt,
        "install_attempts": install_attempts,
        "runtime_install_url": "https://ollama.com/download", "managed_runtime_supported": cfg!(all(windows, target_arch = "x86_64")), "local_only": local_only, "loopback_only": true, "model_store":model_store, "managed_storage":managed_storage }),
    )
}

async fn mutate(
    kind: &str,
    model: &str,
    context: Option<u32>,
    progress: impl FnMut(DownloadProgress),
) -> Result<Value> {
    let path = state_path()?;
    let lease = storage::acquire(&path)?;
    mutate_with_lease(kind, model, context, path, lease, progress).await
}

fn operation_model(kind: &str, model: &str) -> Result<String> {
    if kind == "setup" || (kind == "deselect" && model.is_empty()) {
        return Ok(String::new());
    }
    Ok(phonton_local::runtime::canonical_model_name(model)?)
}

fn model_matches(candidate: &str, canonical: &str) -> bool {
    phonton_local::runtime::canonical_model_name(candidate).is_ok_and(|name| {
        phonton_local::runtime::canonical_model_name(canonical)
            .is_ok_and(|requested| name.eq_ignore_ascii_case(&requested))
    })
}

fn active_model_matches(active: Option<&str>, canonical: &str) -> Result<bool> {
    active
        .map(|name| {
            Ok(phonton_local::runtime::canonical_model_name(name)?
                .eq_ignore_ascii_case(&phonton_local::runtime::canonical_model_name(canonical)?))
        })
        .transpose()
        .map(|matched| matched.unwrap_or(false))
}

fn reconcile_install_attempt(
    attempt: &InstallAttempt,
    endpoint: &str,
    models: &[LocalModel],
    inventory_complete: bool,
) -> &'static str {
    if attempt.endpoint != endpoint {
        return "endpoint_changed";
    }
    if !inventory_complete {
        return "inventory_unavailable";
    }
    match phonton_local::runtime::find_installed_model(models, &attempt.model) {
        Ok(Some(_)) => "installed",
        Err(_) => "ambiguous",
        Ok(None) => "not_installed",
    }
}

fn begin_install_attempt(path: &Path, settings: &mut LocalSettings, model: &str) -> Result<()> {
    settings.schema = settings.schema.max(5);
    let endpoint = settings.endpoint.clone();
    settings
        .install_attempts
        .retain(|attempt| attempt.endpoint != endpoint || !model_matches(&attempt.model, model));
    settings.install_attempts.push(InstallAttempt {
        schema: 1,
        model: model.to_owned(),
        endpoint,
        started_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    });
    if settings.install_attempts.len() > 8 {
        settings.install_attempts.remove(0);
    }
    storage::save(path, settings)?;
    Ok(())
}

async fn install_with_attempt(
    runtime: &LocalRuntime,
    root: &Path,
    path: &Path,
    settings: &mut LocalSettings,
    model: &str,
    progress: impl FnMut(DownloadProgress),
) -> Result<Value> {
    // Save before admission: a rejected request is still an unconfirmed
    // request, but never evidence of transferred or reusable bytes.
    begin_install_attempt(path, settings, model)?;
    let result = install_model(
        runtime,
        root,
        &settings.endpoint,
        model,
        settings.managed_runtime_installed,
        progress,
    )
    .await?;
    let endpoint = settings.endpoint.as_str();
    settings
        .install_attempts
        .retain(|attempt| attempt.endpoint != endpoint || !model_matches(&attempt.model, model));
    storage::save(path, settings)?;
    Ok(result)
}

fn record_calibration(settings: &mut LocalSettings, profile: ModelProfile) -> Result<()> {
    let model = profile.model.as_str();
    if active_model_matches(settings.active_model.as_deref(), model)? {
        settings.active_model = profile.protocol.map(|_| model.to_owned());
    }
    settings
        .profiles
        .retain(|prior| !model_matches(&prior.model, model) || prior.endpoint != settings.endpoint);
    settings.profiles.push(profile);
    Ok(())
}

async fn mutate_with_lease(
    kind: &str,
    model: &str,
    context: Option<u32>,
    path: PathBuf,
    lease: storage::StateLease,
    progress: impl FnMut(DownloadProgress),
) -> Result<Value> {
    let _guard = MUTATION
        .try_lock()
        .map_err(|_| anyhow!("Another model operation is running. Wait or cancel it first."))?;
    let _lease = lease;
    let canonical_model = operation_model(kind, model)?;
    let model = canonical_model.as_str();
    let mut settings = storage::load(&path)?;
    if kind == "deselect" {
        if !model.is_empty() && !active_model_matches(settings.active_model.as_deref(), model)? {
            bail!("Selected model changed; refresh status before deselecting");
        }
        let previous = settings.active_model.take();
        let changed = previous.is_some();
        if changed {
            storage::save(&path, &settings)?;
        }
        return Ok(json!({"active_model":null, "previous_model":previous, "changed":changed}));
    }
    let root = managed_root(&path, &settings)?;
    if settings.managed_root.is_some()
        && std::env::var_os("PHONTON_LOCAL_STATE").is_none()
        && settings.endpoint == MANAGED_MODEL_ENDPOINT
    {
        verify_managed_root_identity(&root, &settings)?;
    }
    if kind == "setup"
        && settings.managed_root.is_some()
        && std::env::var_os("PHONTON_LOCAL_STATE").is_none()
    {
        let canonical = validate_managed_root(&root).map_err(|error| anyhow!("Chosen managed folder is unavailable or unsafe: {error}. Reconnect it or choose another empty folder before setup"))?;
        if canonical != root {
            bail!("Chosen managed folder was redirected. Inspect it or choose another empty folder before setup");
        }
    }
    let runtime = LocalRuntime::new(&settings.endpoint)?;
    match kind {
        "setup" => {
            if settings.endpoint != "http://127.0.0.1:11434" {
                bail!("Managed setup uses 127.0.0.1:11434; connect or repair the custom runtime separately");
            }
            if let Ok(version) = runtime.version().await {
                #[cfg(all(windows, target_arch = "x86_64"))]
                return existing_runtime_setup_at(&path, &mut settings, &root, &version);
                #[cfg(not(all(windows, target_arch = "x86_64")))]
                return Ok(json!({
                    "runtime_version": version, "existing": true,
                    "managed_origin": "unverified",
                    "detail": "An existing Ollama-compatible service answered on the loopback endpoint. Phonton did not install or start it and has not verified which process owns the listener."
                }));
            }
            #[cfg(all(windows, target_arch = "x86_64"))]
            if phonton_local::provision::default_listener_unavailable()? {
                bail!("The default loopback port 127.0.0.1:11434 is unavailable but did not answer the Ollama version check. Stop the process using it or use a different loopback endpoint before managed setup.");
            }
            #[cfg(all(windows, target_arch = "x86_64"))]
            require_setup_recovery_allowed(&root, settings.managed_runtime_installed)?;
            let executable = phonton_local::provision::install(&root, progress).await?;
            if !settings.managed_runtime_installed {
                settings.schema = settings.schema.max(3);
                if settings.managed_root.is_some() {
                    settings.managed_root_used = true;
                }
                settings.managed_runtime_installed = true;
                storage::save(&path, &settings)?;
            }
            let version = phonton_local::provision::start(&executable, &root).await?;
            Ok(
                json!({"runtime_version": version, "managed_path": executable, "managed_origin": "started_by_phonton", "local_only": true, "loopback_only": true}),
            )
        }
        "install" => {
            // Replaced weights invalidate calibration by digest; keep evidence
            // until the next calibration instead of silently claiming compatibility.
            install_with_attempt(&runtime, &root, &path, &mut settings, model, progress).await
        }
        "calibrate" => {
            let profile = runtime
                .calibrate_with_progress(model, context, hardware::detect().await, |attempt| {
                    settings.schema = settings.schema.max(4);
                    settings.calibration_attempt = Some(attempt.clone());
                    storage::save(&path, &settings)
                })
                .await?;
            record_calibration(&mut settings, profile.clone())?;
            settings.calibration_attempt = None;
            storage::save(&path, &settings)?;
            Ok(serde_json::to_value(profile)?)
        }
        "select" => {
            let installed = runtime.installed().await?;
            let installed = phonton_local::runtime::find_installed_model(&installed, model)?
                .ok_or_else(|| anyhow!("Model is not installed"))?;
            let model = installed.name.as_str();
            let mut profiles = settings
                .profiles
                .iter()
                .filter(|p| model_matches(&p.model, model) && p.endpoint == settings.endpoint);
            let profile = profiles
                .next()
                .ok_or_else(|| anyhow!("Calibrate the model before selecting it"))?;
            if profiles.next().is_some() {
                bail!(
                    "Multiple saved calibrations identify this model; recalibrate before selection"
                );
            }
            validate_profile(
                profile,
                installed,
                &settings.endpoint,
                &runtime.version().await?,
            )?;
            let metadata = runtime.show_local(model).await?;
            validate_profile_context(profile, &metadata)?;
            settings.active_model = Some(model.into());
            storage::save(&path, &settings)?;
            Ok(
                json!({"active_model": model, "local_only": managed_local_only(&path, &settings), "loopback_only": true}),
            )
        }
        "remove" => {
            let installed = runtime.installed().await?;
            let installed = phonton_local::runtime::find_installed_model(&installed, model)?
                .ok_or_else(|| anyhow!("Model is not installed"))?;
            let model = installed.name.as_str();
            if active_model_matches(settings.active_model.as_deref(), model)? {
                bail!("Select a different model before removing the active model");
            }
            runtime.remove(model).await?;
            settings
                .profiles
                .retain(|p| !model_matches(&p.model, model) || p.endpoint != settings.endpoint);
            storage::save(&path, &settings)?;
            Ok(json!({"removed": model}))
        }
        _ => bail!("Unknown model operation {kind}"),
    }
}

fn admit_model_operation(
    path: &Path,
    shared: &Arc<Mutex<Operation>>,
    accepted: Operation,
    expected: &OperationExpectation,
) -> Result<storage::StateLease> {
    let mut state = shared
        .lock()
        .map_err(|_| anyhow!("Model operation state unavailable"))?;
    if state.running {
        bail!("Another model operation is running");
    }
    // Hold the cross-process lease from admission through the worker. Without
    // this, a separate CLI could change storage after start returned an ID but
    // before the spawned worker acquired its own state lease.
    let lease = storage::acquire(path)?;
    expected.check(path, &storage::load(path)?)?;
    *state = accepted;
    Ok(lease)
}

pub async fn rpc(method: &str, params: Value) -> Result<Value> {
    match method {
        "models.status" => status(context_param(&params)?).await,
        "models.storage.set" => set_managed_storage(
            params["path"]
                .as_str()
                .ok_or_else(|| anyhow!("Managed storage folder required"))?,
        ),
        "models.endpoint.set" => set_endpoint(
            params["endpoint"]
                .as_str()
                .ok_or_else(|| anyhow!("Endpoint origin required"))?,
        ),
        "models.catalog" => Ok(serde_json::to_value(catalog_snapshot().await?.models)?),
        "models.catalog.snapshot" => Ok(serde_json::to_value(catalog_snapshot().await?)?),
        "models.operation" => {
            let state = operation();
            let state = state
                .lock()
                .map_err(|_| anyhow!("Model operation state unavailable"))?;
            Ok(
                json!({"id": state.id, "kind": state.kind, "model": state.model, "running": state.running,
                "cancelable": state.running && state.cancel.is_some(),
                "progress": state.progress, "result": state.result, "error": state.error}),
            )
        }
        "models.cancel" => {
            let state = operation();
            let id = params["id"]
                .as_str()
                .ok_or_else(|| anyhow!("Cancellation requires the operation id"))?;
            request_cancel(&state, id)
        }
        "models.start" => {
            let kind = params["kind"]
                .as_str()
                .ok_or_else(|| anyhow!("Operation kind required"))?
                .to_owned();
            if !matches!(
                kind.as_str(),
                "setup" | "install" | "calibrate" | "select" | "deselect" | "remove"
            ) {
                bail!("Unsupported model operation");
            }
            let model = if kind == "setup" {
                String::new()
            } else {
                let model = params["model"]
                    .as_str()
                    .ok_or_else(|| anyhow!("Model name required"))?;
                operation_model(&kind, model)?
            };
            let context = context_param(&params)?;
            let expected = OperationExpectation::from_params(&params)?;
            let shared = operation();
            let id = uuid::Uuid::new_v4().to_string();
            let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
            let cancelable = operation_cancellable(&kind);
            let path = state_path()?;
            let lease = admit_model_operation(
                &path,
                &shared,
                Operation {
                    id: id.clone(),
                    kind: kind.clone(),
                    model: model.clone(),
                    running: true,
                    cancel: cancelable.then_some(cancel_tx),
                    ..Default::default()
                },
                &expected,
            )?;
            tokio::spawn(async move {
                let progress_state = shared.clone();
                let result = await_model_mutation(
                    &kind,
                    mutate_with_lease(&kind, &model, context, path, lease, |event| {
                        if let Ok(mut state) = progress_state.lock() {
                            state.progress = Some(event);
                        }
                    }),
                    async move {
                        while cancel_rx.changed().await.is_ok() {
                            if *cancel_rx.borrow() {
                                return;
                            }
                        }
                        std::future::pending::<()>().await;
                    },
                )
                .await;
                if let Ok(mut state) = shared.lock() {
                    state.running = false;
                    state.cancel = None;
                    match result {
                        Ok(value) => state.result = Some(value),
                        Err(e) => state.error = Some(e.to_string()),
                    }
                }
            });
            Ok(json!({"id": id, "cancelable": cancelable}))
        }
        _ => bail!("Unknown models method"),
    }
}

fn parse_status_context(args: &[String]) -> Result<Option<u32>> {
    let mut context = None;
    let mut json = false;
    for arg in args {
        if arg == "--json" {
            if json {
                bail!("Duplicate --json flag for models status");
            }
            json = true;
        } else if arg.starts_with('-') {
            bail!("Unknown models status option: {arg}");
        } else if context.is_some() {
            bail!("Usage: phonton models status [CONTEXT] [--json]");
        } else {
            context = Some(arg.parse::<u32>().map_err(|_| {
                anyhow!("Invalid models status context: expected a numeric token count")
            })?);
        }
    }
    Ok(context)
}

pub async fn run(args: &[String]) -> Result<i32> {
    let started = Instant::now();
    let mut printer = ProgressPrinter::default();
    let verb = args.first().map(String::as_str).unwrap_or("status");
    if matches!(verb, "--help" | "-h" | "help")
        || (verb == "status" && args.len() == 2 && matches!(args[1].as_str(), "--help" | "-h"))
    {
        println!("phonton models status [CONTEXT] [--json]|storage [EMPTY_FOLDER]|endpoint [ORIGIN]|setup [MODEL]|catalog [--snapshot]|install MODEL|calibrate MODEL [CONTEXT]|select MODEL|deselect|remove MODEL\n\nSetup MODEL runs setup, install, calibrate and select in order and stops at the first failure.\nStatus always prints JSON; --json is accepted for consistency. Storage chooses an existing, empty dedicated local-drive folder for managed runtime and model files before setup; the choice is saved for CLI and Desktop. The PHONTON_LOCAL_STATE override keeps its own isolated runtime beside that state file. Existing managed files are never moved by this command. Endpoint accepts only a loopback HTTP(S) origin. Changing it clears the selected model; it can be saved before that runtime starts. Managed setup uses the default 127.0.0.1:11434 origin.\nSetup labels a responding service unverified only when no conflicting saved managed launch exists. A stale receipt requires stopping the port owner and retrying. Otherwise, on Windows x64 setup downloads a hash-checked portable Ollama runtime (about 1.5 GB).\nCatalog and install access the public registry. Catalog --snapshot also prints the exact hardware reading used for model fit and first-try guidance; default catalog output remains an array. A model without a tag resolves to the installed :latest identity for calibration and selection. Deselect keeps installed weights and calibration so the model can be selected again or explicitly removed later. Phonton sends inference requests to loopback; an existing service's cloud settings are not verified.\nOmitted calibration context uses observed memory and the installed model limit; an explicit context overrides that choice. Calibration records measured edit, creation and tool-call formats, not general coding quality. If no edit format passes, the probe JSON is saved and printed but the command exits 2; the model cannot be selected.");
        return Ok(0);
    }
    let value = match verb {
        "storage" if args.len() == 1 => {
            let path = state_path()?;
            storage_status(&path, &storage::load(&path)?)?
        }
        "storage" if args.len() == 2 => set_managed_storage(&args[1])?,
        "endpoint" if args.len() == 1 => {
            let settings = settings()?;
            let path = state_path()?;
            json!({"endpoint": settings.endpoint, "active_model": settings.active_model, "local_only": managed_local_only(&path, &settings), "loopback_only": true})
        }
        "endpoint" if args.len() == 2 => set_endpoint(&args[1])?,
        "status" => status(parse_status_context(args.get(1..).unwrap_or_default())?).await?,
        "catalog" if args.len() == 1 || (args.len() == 2 && args[1] == "--snapshot") => {
            catalog_cli_output(catalog_snapshot().await?, args.len() == 2)?
        }
        "setup" if args.len() == 1 => {
            await_model_mutation(
                "setup",
                mutate("setup", "", None, |event| {
                    if let Some(line) = printer.line(event, started.elapsed()) {
                        eprintln!("{line}");
                    }
                }),
                ctrl_c_requested(),
            )
            .await?
        }
        // One command from nothing to a selected model: runtime, weights,
        // calibration, selection. Stops at the first step that fails.
        "setup" if args.len() == 2 => {
            let model = args[1].as_str();
            let mut steps = serde_json::Map::new();
            for step in ["setup", "install", "calibrate", "select"] {
                let target = if step == "setup" { "" } else { model };
                eprintln!("phonton models {}", format!("{step} {target}").trim_end());
                let value = await_model_mutation(
                    step,
                    mutate(step, target, None, |event| {
                        if let Some(line) = printer.line(event, started.elapsed()) {
                            eprintln!("{line}");
                        }
                    }),
                    ctrl_c_requested(),
                )
                .await?;
                let failed = calibration_exit_code(step, &value) != 0;
                steps.insert(step.into(), value);
                if failed {
                    println!("{}", serde_json::to_string_pretty(&steps)?);
                    eprintln!("Calibration evidence was saved, but no edit format passed. Inspect the probe outputs above; {model} cannot be selected. Try another model from `phonton models catalog`.");
                    return Ok(2);
                }
            }
            println!("{}", serde_json::to_string_pretty(&steps)?);
            eprintln!("Ready: {model} is calibrated and selected. Run `phonton` and type a goal, or `phonton goal \"<goal>\" --yes --allow-host-checks`.");
            return Ok(0);
        }
        "deselect" if args.len() == 1 => {
            await_model_mutation(
                "deselect",
                mutate("deselect", "", None, |_| {}),
                ctrl_c_requested(),
            )
            .await?
        }
        "install" | "calibrate" | "select" | "remove" => {
            if args.len() < 2 || args.len() > (if verb == "calibrate" { 3 } else { 2 }) {
                bail!(
                    "Usage: phonton models {verb} MODEL{}",
                    if verb == "calibrate" {
                        " [CONTEXT]"
                    } else {
                        ""
                    }
                );
            }
            let context = args.get(2).map(|v| v.parse::<u32>()).transpose()?;
            await_model_mutation(
                verb,
                mutate(verb, &args[1], context, |event| {
                    if let Some(line) = printer.line(event, started.elapsed()) {
                        eprintln!("{line}");
                    }
                }),
                ctrl_c_requested(),
            )
            .await?
        }
        _ => bail!("Unknown models command. Run phonton models --help"),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    if verb == "setup" {
        eprintln!("Next: `phonton models setup MODEL` installs, calibrates and selects a model; `phonton models catalog` lists the ones that fit this machine.");
    }
    let exit_code = calibration_exit_code(verb, &value);
    if exit_code != 0 {
        eprintln!("Calibration evidence was saved, but no edit format passed. Inspect the probe outputs above; this model cannot be selected yet.");
    }
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::local::{
        CalibrationAttempt, EditProtocol, GpuSnapshot, ModelProbe, CREATION_PROBE_NAME,
        DIFF_CREATION_PROBE_NAME,
    };

    #[test]
    fn catalog_snapshot_preserves_the_fit_reading_without_changing_default_output() {
        let snapshot = CatalogSnapshot {
            hardware: HardwareSnapshot {
                cpu: Some("Fixture CPU".into()),
                ram_available_bytes: Some(3_000_000_000),
                ..Default::default()
            },
            models: vec![CatalogModel {
                name: "fixture:latest".into(),
                source: "https://example.invalid/fixture".into(),
                download_bytes: Some(1_000_000_000),
                fit: None,
                error: None,
                first_try_reason: Some("Fits the observed memory allowance".into()),
                pre_setup_storage: None,
            }],
        };
        let default = catalog_cli_output(snapshot.clone(), false).unwrap();
        let with_snapshot = catalog_cli_output(snapshot, true).unwrap();

        assert!(default.is_array());
        assert_eq!(default[0]["name"], "fixture:latest");
        assert_eq!(with_snapshot["models"], default);
        assert_eq!(with_snapshot["hardware"]["cpu"], "Fixture CPU");
        assert_eq!(
            with_snapshot["hardware"]["ram_available_bytes"],
            3_000_000_000_u64
        );
    }

    #[tokio::test]
    async fn failed_pull_keeps_exact_attempt_for_reopen_and_inventory_reconciliation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("local-models.json");
        let mut settings = LocalSettings {
            endpoint: "http://127.0.0.1:9".into(),
            ..Default::default()
        };
        storage::save(&path, &settings).unwrap();
        let runtime = LocalRuntime::new(&settings.endpoint).unwrap();
        let root = managed_root(&path, &settings).unwrap();
        assert!(install_with_attempt(
            &runtime,
            &root,
            &path,
            &mut settings,
            "fixture:latest",
            |_| {}
        )
        .await
        .is_err());
        assert!(
            install_with_attempt(&runtime, &root, &path, &mut settings, "other:tag", |_| {})
                .await
                .is_err()
        );
        let reopened = storage::load(&path).unwrap();
        let attempt = &reopened.install_attempts[0];
        assert_eq!(reopened.schema, 5);
        assert_eq!(reopened.install_attempts.len(), 2);
        assert_eq!(attempt.model, "fixture:latest");
        assert_eq!(reopened.install_attempts[1].model, "other:tag");
        assert_eq!(attempt.endpoint, reopened.endpoint);
        assert!(reopened.profiles.is_empty());
        assert_eq!(
            reconcile_install_attempt(attempt, &reopened.endpoint, &[], false),
            "inventory_unavailable"
        );
        assert_eq!(
            reconcile_install_attempt(attempt, &reopened.endpoint, &[], true),
            "not_installed"
        );
        assert_eq!(
            reconcile_install_attempt(attempt, MANAGED_MODEL_ENDPOINT, &[], true),
            "endpoint_changed"
        );
        let installed = LocalModel {
            name: "library/fixture:latest".into(),
            digest: "sha256:fixture".into(),
            size_bytes: 1024,
            parameter_size: None,
            quantization: None,
        };
        assert_eq!(
            reconcile_install_attempt(
                attempt,
                &reopened.endpoint,
                std::slice::from_ref(&installed),
                true
            ),
            "installed"
        );
        assert_eq!(
            reconcile_install_attempt(
                attempt,
                &reopened.endpoint,
                &[installed.clone(), installed],
                true
            ),
            "ambiguous"
        );
    }

    #[tokio::test]
    async fn midstream_pull_abort_keeps_exact_request_without_claiming_installation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let event = b"{\"status\":\"downloading\",\"completed\":1,\"total\":10}\n";
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n",
                event.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(event).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("local-models.json");
        let mut settings = LocalSettings {
            endpoint,
            ..Default::default()
        };
        let runtime = LocalRuntime::new(&settings.endpoint).unwrap();
        let root = managed_root(&path, &settings).unwrap();
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        let worker_path = path.clone();
        let worker = tokio::spawn(async move {
            let mut observed_tx = Some(observed_tx);
            install_with_attempt(
                &runtime,
                &root,
                &worker_path,
                &mut settings,
                "fixture:latest",
                |progress| {
                    if progress.status == "downloading" {
                        if let Some(sender) = observed_tx.take() {
                            let _ = sender.send(());
                        }
                    }
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), observed_rx)
            .await
            .unwrap()
            .unwrap();
        worker.abort();
        let _ = worker.await;
        server.abort();

        let reopened = storage::load(&path).unwrap();
        assert_eq!(reopened.schema, 5);
        assert_eq!(reopened.install_attempts.len(), 1);
        assert_eq!(reopened.install_attempts[0].model, "fixture:latest");
        assert_eq!(reopened.install_attempts[0].endpoint, reopened.endpoint);
        assert!(reopened.profiles.is_empty());
    }

    #[tokio::test]
    async fn verified_install_clears_only_its_own_saved_request() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 4096];
                let count = stream.read(&mut request).await.unwrap();
                assert!(count > 0);
                let request = String::from_utf8_lossy(&request[..count]);
                let body = if request.starts_with("POST /api/pull ") {
                    "{\"status\":\"success\"}\n"
                } else if request.starts_with("GET /api/tags ") {
                    "{\"models\":[{\"name\":\"fixture:latest\",\"digest\":\"sha256:fixture\",\"size\":1024}]}"
                } else if request.starts_with("POST /api/show ") {
                    "{\"model_info\":{},\"details\":{\"format\":\"gguf\"}}"
                } else {
                    panic!("Unexpected fixture request: {request}");
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("local-models.json");
        let mut settings = LocalSettings {
            endpoint,
            ..Default::default()
        };
        begin_install_attempt(&path, &mut settings, "fixture:latest").unwrap();
        begin_install_attempt(&path, &mut settings, "other:tag").unwrap();
        let runtime = LocalRuntime::new(&settings.endpoint).unwrap();
        let root = managed_root(&path, &settings).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            install_with_attempt(
                &runtime,
                &root,
                &path,
                &mut settings,
                "fixture:latest",
                |_| {},
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
        assert_eq!(result["installed"], "fixture:latest");
        let reopened = storage::load(&path).unwrap();
        assert_eq!(reopened.install_attempts.len(), 1);
        assert_eq!(reopened.install_attempts[0].model, "other:tag");
        assert!(reopened.profiles.is_empty());
    }

    #[test]
    fn unconfirmed_install_history_is_bounded_and_deduplicates_exact_tags() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("local-models.json");
        let mut settings = LocalSettings::default();
        for number in 0..10 {
            begin_install_attempt(&path, &mut settings, &format!("fixture:{number}")).unwrap();
        }
        let reopened = storage::load(&path).unwrap();
        assert_eq!(reopened.install_attempts.len(), 8);
        assert_eq!(reopened.install_attempts[0].model, "fixture:2");
        begin_install_attempt(&path, &mut settings, "fixture:3").unwrap();
        let reopened = storage::load(&path).unwrap();
        assert_eq!(reopened.install_attempts.len(), 8);
        assert_eq!(reopened.install_attempts[7].model, "fixture:3");
        assert_eq!(
            reopened
                .install_attempts
                .iter()
                .filter(|attempt| attempt.model == "fixture:3")
                .count(),
            1
        );
    }

    #[test]
    fn profile_hash_changes_when_same_model_is_recalibrated() {
        let profile = ModelProfile {
            schema: 2,
            model: "fixture:small".into(),
            digest: "sha256:fixture".into(),
            runtime_version: "0.34.2".into(),
            endpoint: MANAGED_MODEL_ENDPOINT.into(),
            context_tokens: 4096,
            output_tokens: 1024,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: None,
            probes: Vec::new(),
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 10,
        };
        let first = profile_sha256(&profile).unwrap();
        let recalibrated = ModelProfile {
            measured_at_unix: 11,
            ..profile
        };
        assert_ne!(first, profile_sha256(&recalibrated).unwrap());
    }

    #[test]
    fn status_keeps_profile_for_equivalent_inventory_alias() {
        let profile = ModelProfile {
            schema: 2,
            model: "fixture:latest".into(),
            digest: "sha256:fixture".into(),
            runtime_version: "0.34.2".into(),
            endpoint: MANAGED_MODEL_ENDPOINT.into(),
            context_tokens: 4096,
            output_tokens: 1024,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: Some(Default::default()),
            probes: vec![ModelProbe {
                name: "SearchReplace edit".into(),
                status: CheckStatus::Passed,
                output: String::new(),
                detail: String::new(),
                input_tokens: None,
                output_tokens: None,
                elapsed_ms: 0,
            }],
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 1,
        };
        let settings = LocalSettings {
            active_model: Some(profile.model.clone()),
            profiles: vec![profile],
            ..Default::default()
        };
        let model = LocalModel {
            name: "library/fixture:latest".into(),
            digest: "sha256:fixture".into(),
            size_bytes: 1024,
            parameter_size: None,
            quantization: None,
        };
        let (found, error) = profile_for_status(&settings, &model, "0.34.2");
        assert!(found.is_some(), "{error:?}");
    }

    #[test]
    fn catalog_exposes_model_specific_pre_setup_storage_only_for_unused_managed_root() {
        let model = |name: &str, bytes: Option<u64>, error: Option<&str>| CatalogModel {
            name: name.into(),
            source: format!("https://registry.ollama.ai/{name}"),
            download_bytes: bytes,
            fit: None,
            error: error.map(str::to_owned),
            first_try_reason: None,
            pre_setup_storage: None,
        };
        let report = json!({
            "root":"C:\\models", "available_bytes":8_000_000_000_u64,
            "runtime_setup_min_free_bytes":5_843_712_056_u64,
            "changeable":true, "runtime_installed":false,
        });
        let mut models = vec![
            model("valid", Some(4_000_000_000), None),
            model("missing", None, Some("manifest unavailable")),
        ];
        attach_pre_setup_storage(&mut models, &report, MANAGED_MODEL_ENDPOINT);
        let plan = models[0].pre_setup_storage.as_ref().unwrap();
        assert_eq!(plan.root, PathBuf::from("C:\\models"));
        assert_eq!(plan.model_download_bytes, 4_000_000_000);
        assert_eq!(plan.pull_reserve_bytes, 1024 * 1024 * 1024);
        assert_eq!(plan.required_bytes, 10_917_453_880);
        assert_eq!(plan.shortfall_bytes, 2_917_453_880);
        assert!(models[1].pre_setup_storage.is_none());
        assert!(serde_json::to_value(&models).unwrap()[0]["pre_setup_storage"].is_object());

        let mut external = vec![model("valid", Some(4_000_000_000), None)];
        attach_pre_setup_storage(&mut external, &report, "http://127.0.0.1:11435");
        assert!(external[0].pre_setup_storage.is_none());
        for unavailable in [
            json!({"root":"C:\\models", "available_bytes":null, "runtime_setup_min_free_bytes":5_843_712_056_u64, "changeable":true, "runtime_installed":false}),
            json!({"root":"C:\\models", "available_bytes":8_000_000_000_u64, "runtime_setup_min_free_bytes":5_843_712_056_u64, "changeable":false, "runtime_installed":false}),
            json!({"root":"C:\\models", "available_bytes":8_000_000_000_u64, "runtime_setup_min_free_bytes":5_843_712_056_u64, "changeable":true, "runtime_installed":true}),
            json!({"root":"C:\\models", "available_bytes":8_000_000_000_u64, "runtime_setup_min_free_bytes":5_843_712_056_u64, "changeable":true, "runtime_installed":false, "reason":"Chosen folder is unavailable"}),
        ] {
            let mut rows = vec![model("valid", Some(4_000_000_000), None)];
            attach_pre_setup_storage(&mut rows, &unavailable, MANAGED_MODEL_ENDPOINT);
            assert!(rows[0].pre_setup_storage.is_none());
        }
        attach_pre_setup_storage(&mut models, &report, "http://127.0.0.1:11435");
        assert!(models.iter().all(|row| row.pre_setup_storage.is_none()));
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn catalog_omits_plans_for_missing_or_replaced_chosen_storage() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let chosen = temp.path().join("chosen");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::create_dir(&chosen).unwrap();
        let path = state_dir.join("local-models.json");
        let settings = LocalSettings {
            schema: 2,
            managed_root: Some(std::fs::canonicalize(&chosen).unwrap()),
            managed_root_identity: Some(phonton_local::disk::directory_identity(&chosen).unwrap()),
            ..LocalSettings::default()
        };
        let mut rows = vec![CatalogModel {
            name: "example".into(),
            source: "https://registry.ollama.ai/example".into(),
            download_bytes: Some(2_000_000_000),
            fit: None,
            error: None,
            first_try_reason: None,
            pre_setup_storage: None,
        }];
        std::fs::rename(&chosen, temp.path().join("moved")).unwrap();
        let missing = storage_status(&path, &settings).unwrap();
        assert_eq!(missing["changeable"], true);
        assert!(missing["reason"].is_string());
        attach_pre_setup_storage(&mut rows, &missing, MANAGED_MODEL_ENDPOINT);
        assert!(rows[0].pre_setup_storage.is_none());

        std::fs::create_dir(&chosen).unwrap();
        let replaced = storage_status(&path, &settings).unwrap();
        assert_eq!(replaced["changeable"], true);
        assert!(replaced["reason"].is_string());
        attach_pre_setup_storage(&mut rows, &replaced, MANAGED_MODEL_ENDPOINT);
        assert!(rows[0].pre_setup_storage.is_none());
    }

    #[test]
    fn model_operations_use_the_same_latest_tag_as_installed_inventory() {
        assert_eq!(operation_model("setup", "").unwrap(), "");
        assert_eq!(operation_model("deselect", "").unwrap(), "");
        for kind in ["install", "calibrate", "select", "remove"] {
            assert_eq!(operation_model(kind, "fixture").unwrap(), "fixture:latest");
            assert_eq!(
                operation_model(kind, "team/fixture").unwrap(),
                "team/fixture:latest"
            );
            assert_eq!(
                operation_model(kind, "library/fixture").unwrap(),
                "fixture:latest"
            );
            assert_eq!(
                operation_model(kind, "LIBRARY/fixture").unwrap(),
                "fixture:latest"
            );
            assert_eq!(
                operation_model(kind, "registry.ollama.ai/LIBRARY/fixture").unwrap(),
                "fixture:latest"
            );
            assert_eq!(
                operation_model(kind, "library/team/fixture").unwrap(),
                "library/team/fixture:latest"
            );
            assert_eq!(
                operation_model(kind, "fixture:small").unwrap(),
                "fixture:small"
            );
        }
    }

    #[test]
    fn active_model_guard_recognizes_default_aliases_without_confusing_hosts() {
        for name in [
            "fixture",
            "LIBRARY/fixture",
            "REGISTRY.OLLAMA.AI/library/FIXTURE:latest",
        ] {
            let requested = operation_model("remove", name).unwrap();
            assert!(active_model_matches(Some("fixture:latest"), &requested).unwrap());
        }
        let distinct_host = operation_model("remove", "library/team/fixture").unwrap();
        assert!(!active_model_matches(Some("team/fixture:latest"), &distinct_host).unwrap());
        assert!(active_model_matches(Some("invalid//name"), "fixture:latest").is_err());
    }

    #[tokio::test]
    async fn deselect_works_offline_and_preserves_calibration_for_a_later_remove() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models.json");
        let profile = ModelProfile {
            schema: 2,
            model: "fixture:latest".into(),
            digest: "sha256:fixture".into(),
            runtime_version: "0.34.2".into(),
            endpoint: LocalSettings::default().endpoint,
            context_tokens: 4096,
            output_tokens: 1024,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: Some(Default::default()),
            probes: Vec::new(),
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 1,
        };
        let missing_root = temp.path().join("missing-runtime");
        let settings = LocalSettings {
            schema: 2,
            managed_root: Some(missing_root.clone()),
            managed_root_used: true,
            active_model: Some("LIBRARY/FIXTURE:latest".into()),
            profiles: vec![profile.clone()],
            ..Default::default()
        };
        storage::save(&path, &settings).unwrap();
        let lease = storage::acquire(&path).unwrap();
        let error = mutate_with_lease(
            "deselect",
            "other:latest",
            None,
            path.clone(),
            lease,
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Selected model changed"));
        assert_eq!(
            storage::load(&path).unwrap().active_model,
            settings.active_model
        );

        let lease = storage::acquire(&path).unwrap();
        let result = mutate_with_lease("deselect", "fixture", None, path.clone(), lease, |_| {})
            .await
            .unwrap();
        assert_eq!(result["changed"], true);
        assert_eq!(result["previous_model"], "LIBRARY/FIXTURE:latest");
        let saved = storage::load(&path).unwrap();
        assert!(saved.active_model.is_none());
        assert_eq!(saved.managed_root, Some(missing_root));
        assert_eq!(
            serde_json::to_value(&saved.profiles).unwrap(),
            serde_json::to_value(&settings.profiles).unwrap()
        );

        let lease = storage::acquire(&path).unwrap();
        assert_eq!(
            mutate_with_lease("deselect", "", None, path.clone(), lease, |_| {})
                .await
                .unwrap()["changed"],
            false
        );
    }

    #[test]
    fn recalibration_replaces_alias_profile_and_clears_failed_active_selection() {
        let old = ModelProfile {
            schema: 2,
            model: "fixture:latest".into(),
            digest: "sha256:fixture".into(),
            runtime_version: "0.34.2".into(),
            endpoint: LocalSettings::default().endpoint,
            context_tokens: 4096,
            output_tokens: 1024,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: Some(Default::default()),
            probes: Vec::new(),
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 1,
        };
        let mut settings = LocalSettings {
            active_model: Some("LIBRARY/FIXTURE:latest".into()),
            profiles: vec![old.clone()],
            ..Default::default()
        };
        let mut failed = old.clone();
        failed.protocol = None;
        failed.context_tokens = 8192;
        failed.measured_at_unix = 2;
        record_calibration(&mut settings, failed).unwrap();
        assert!(settings.active_model.is_none());
        assert_eq!(settings.profiles.len(), 1);
        assert_eq!(settings.profiles[0].measured_at_unix, 2);
        assert!(settings.profiles[0].protocol.is_none());

        settings.active_model = Some("LIBRARY/FIXTURE:latest".into());
        let mut changed = old.clone();
        changed.context_tokens = 8192;
        changed.measured_at_unix = 3;
        record_calibration(&mut settings, changed).unwrap();
        assert_eq!(settings.active_model.as_deref(), Some("fixture:latest"));
        assert_eq!(settings.profiles.len(), 1);
        assert_eq!(settings.profiles[0].context_tokens, 8192);
        assert_eq!(settings.profiles[0].measured_at_unix, 3);

        settings.active_model = Some("fixture:latest".into());
        let mut failed_alias = old;
        failed_alias.model = "library/fixture:latest".into();
        failed_alias.protocol = None;
        failed_alias.measured_at_unix = 4;
        record_calibration(&mut settings, failed_alias).unwrap();
        assert!(settings.active_model.is_none());
        assert_eq!(settings.profiles.len(), 1);
        assert_eq!(settings.profiles[0].model, "library/fixture:latest");
    }

    #[tokio::test]
    async fn remove_refuses_ambiguous_or_active_alias_without_deleting() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let ambiguous = Arc::new(AtomicBool::new(true));
        let deletions = Arc::new(AtomicUsize::new(0));
        let inventory_ambiguous = Arc::clone(&ambiguous);
        let delete_count = Arc::clone(&deletions);
        let app = axum::Router::new()
            .route(
                "/api/tags",
                axum::routing::get(move || {
                    let ambiguous = inventory_ambiguous.load(Ordering::SeqCst);
                    async move {
                        let mut models = vec![json!({
                            "name":"fixture:latest", "digest":"sha256:first", "size":1024
                        })];
                        if ambiguous {
                            models.push(json!({
                                "name":"LIBRARY/FIXTURE:latest", "digest":"sha256:second", "size":1024
                            }));
                        }
                        axum::Json(json!({"models":models}))
                    }
                }),
            )
            .route(
                "/api/delete",
                axum::routing::delete(move || {
                    delete_count.fetch_add(1, Ordering::SeqCst);
                    async { axum::Json(json!({})) }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models.json");
        let mut settings = LocalSettings {
            endpoint,
            ..Default::default()
        };
        storage::save(&path, &settings).unwrap();
        let lease = storage::acquire(&path).unwrap();
        let error = mutate_with_lease("remove", "fixture", None, path.clone(), lease, |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
        assert_eq!(deletions.load(Ordering::SeqCst), 0);

        ambiguous.store(false, Ordering::SeqCst);
        settings.active_model = Some("LIBRARY/FIXTURE:latest".into());
        storage::save(&path, &settings).unwrap();
        let lease = storage::acquire(&path).unwrap();
        let error = mutate_with_lease("remove", "fixture", None, path.clone(), lease, |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("active model"));
        assert_eq!(deletions.load(Ordering::SeqCst), 0);

        settings.active_model = None;
        settings.profiles.push(ModelProfile {
            schema: 2,
            model: "fixture:latest".into(),
            digest: "sha256:first".into(),
            runtime_version: "0.34.2".into(),
            endpoint: settings.endpoint.clone(),
            context_tokens: 4096,
            output_tokens: 1024,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: Some(Default::default()),
            probes: Vec::new(),
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 1,
        });
        storage::save(&path, &settings).unwrap();
        let lease = storage::acquire(&path).unwrap();
        let error = mutate_with_lease("remove", "fixture", None, path.clone(), lease, |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("still installed"));
        assert_eq!(deletions.load(Ordering::SeqCst), 1);
        let saved = storage::load(&path).unwrap();
        assert_eq!(saved.profiles.len(), 1);
        assert_eq!(saved.profiles[0].digest, "sha256:first");
        assert_eq!(saved.profiles[0].measured_at_unix, 1);
        server.abort();
    }

    #[test]
    fn storage_status_exposes_the_runtime_install_reserve() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("models.json");
        let status = storage_status(&path, &LocalSettings::default()).unwrap();
        assert_eq!(
            status["runtime_setup_min_free_bytes"],
            phonton_local::provision::MIN_INSTALL_FREE_BYTES
        );
        assert_eq!(status["runtime_installed"], false);
        let installed = LocalSettings {
            managed_runtime_installed: true,
            ..Default::default()
        };
        assert_eq!(
            storage_status(&path, &installed).unwrap()["runtime_installed"],
            true
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn responding_service_with_stale_receipt_is_a_setup_error() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("managed-process.json"), b"truncated{").unwrap();
        let path = temp.path().join("state").join("local-models.json");
        let mut settings = LocalSettings {
            schema: 2,
            managed_root: Some(root.clone()),
            managed_root_identity: Some(phonton_local::disk::directory_identity(&root).unwrap()),
            ..Default::default()
        };
        storage::save(&path, &settings).unwrap();

        let error = existing_runtime_setup_at(&path, &mut settings, &root, "0.34.2")
            .unwrap_err()
            .to_string();
        assert!(error.contains("saved managed launch cannot be verified"));
        assert!(error.contains("Stop the process using that port"));
        assert!(!storage::load(&path).unwrap().managed_root_used);
        assert!(root.join("managed-process.json").is_file());
        let store = model_store_status(&root, MANAGED_MODEL_ENDPOINT, false);
        assert_eq!(store["status"], "unverified");
        assert_eq!(store["recovery_required"], true);
        assert_eq!(store["goal_run_blocked"], true);
        assert_eq!(store["setup_retryable"], true);
        assert!(require_setup_recovery_allowed(&root, false).is_ok());
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn managed_setup_refuses_an_unsafe_receipt_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("managed-process.json")).unwrap();
        let store = model_store_status(&root, MANAGED_MODEL_ENDPOINT, true);
        assert_eq!(store["recovery_required"], true);
        assert_eq!(store["setup_retryable"], false);
        assert!(require_setup_recovery_allowed(&root, true)
            .unwrap_err()
            .to_string()
            .contains("cannot safely replace"));
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn responding_external_service_without_receipt_remains_unverified() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("runs")).unwrap();
        let path = temp.path().join("local-models.json");
        let mut settings = LocalSettings {
            schema: 2,
            managed_root: Some(root.clone()),
            managed_root_identity: Some(phonton_local::disk::directory_identity(&root).unwrap()),
            managed_root_used: true,
            ..Default::default()
        };
        storage::save(&path, &settings).unwrap();
        let result = existing_runtime_setup_at(&path, &mut settings, &root, "0.34.2").unwrap();
        assert_eq!(result["existing"], true);
        assert_eq!(result["managed_origin"], "unverified");
        assert!(storage::load(&path).unwrap().managed_root_used);
        assert!(!storage::load(&path).unwrap().managed_runtime_installed);
        assert!(model_store_status(&root, MANAGED_MODEL_ENDPOINT, false)
            .get("recovery_required")
            .is_none());
        assert_eq!(
            model_store_status(&root, MANAGED_MODEL_ENDPOINT, false)["goal_run_blocked"],
            false
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn missing_prior_managed_receipt_requires_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        std::fs::create_dir(&root).unwrap();
        let path = temp.path().join("local-models.json");
        let mut settings = LocalSettings {
            schema: 3,
            managed_root: Some(root.clone()),
            managed_root_identity: Some(phonton_local::disk::directory_identity(&root).unwrap()),
            managed_root_used: true,
            managed_runtime_installed: true,
            ..Default::default()
        };
        storage::save(&path, &settings).unwrap();
        let error = existing_runtime_setup_at(&path, &mut settings, &root, "0.34.2")
            .unwrap_err()
            .to_string();
        assert!(error.contains("previous managed launch receipt is missing"));
        assert_eq!(
            model_store_status(&root, MANAGED_MODEL_ENDPOINT, true)["recovery_required"],
            true
        );
        assert_eq!(
            model_store_status(&root, MANAGED_MODEL_ENDPOINT, true)["goal_run_blocked"],
            false
        );
        assert_eq!(
            model_store_status(&root, MANAGED_MODEL_ENDPOINT, true)["setup_retryable"],
            true
        );

        settings.managed_runtime_installed = false;
        let legacy = root.join("ollama-older");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join(".archive-sha256"), "old verified install").unwrap();
        assert!(previous_managed_runtime(&root, false));
        assert_eq!(
            model_store_status(&root, MANAGED_MODEL_ENDPOINT, false)["recovery_required"],
            true
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn missing_default_runtime_keeps_its_install_marker() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state").join("local-models.json");
        let root = path.parent().unwrap().join("runtime");
        let chosen = temp.path().join("new-runtime");
        std::fs::create_dir(&chosen).unwrap();
        let mut settings = LocalSettings {
            schema: 3,
            managed_runtime_installed: true,
            ..Default::default()
        };
        storage::save(&path, &settings).unwrap();
        let error = existing_runtime_setup_at(&path, &mut settings, &root, "0.34.2")
            .unwrap_err()
            .to_string();
        assert!(error.contains("previous managed launch receipt is missing"));
        assert_eq!(
            model_store_status(&root, MANAGED_MODEL_ENDPOINT, true)["recovery_required"],
            true
        );
        assert!(!storage_status(&path, &settings).unwrap()["changeable"]
            .as_bool()
            .unwrap());
        assert!(set_managed_storage_at(&path, &chosen, false).is_err());
        assert!(storage::load(&path).unwrap().managed_runtime_installed);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[tokio::test]
    async fn stale_managed_receipt_stops_install_before_runtime_request() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("managed-process.json"), b"truncated{").unwrap();
        let endpoint = MANAGED_MODEL_ENDPOINT;
        let runtime = LocalRuntime::new(endpoint).unwrap();
        let error = install_model(&runtime, &root, endpoint, "qwen3.5:4b", false, |_| {})
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("previously managed Ollama service can no longer be verified"));
        assert_eq!(
            model_store_status(&root, endpoint, false)["status"],
            "unverified"
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[tokio::test]
    async fn missing_installed_managed_runtime_stops_install_before_runtime_request() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("missing-runtime");
        let endpoint = MANAGED_MODEL_ENDPOINT;
        let runtime = LocalRuntime::new(endpoint).unwrap();
        let error = install_model(&runtime, &root, endpoint, "qwen3.5:4b", true, |_| {})
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("previously used managed model store"));
        assert!(model_store_status(&root, endpoint, true)["reason"]
            .as_str()
            .unwrap()
            .contains("previously used managed folder"));
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[tokio::test]
    async fn alternate_external_endpoint_does_not_inherit_old_managed_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("managed-process.json"), b"truncated{").unwrap();
        let endpoint = "http://127.0.0.1:9";
        let runtime = LocalRuntime::new(endpoint).unwrap();
        let error = install_model(&runtime, &root, endpoint, "qwen3.5:4b", false, |_| {})
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("previously managed"));
        assert!(error.to_string().contains("local runtime request failed"));
    }

    #[test]
    fn status_json_flag_is_accepted_without_changing_context() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).into())
                .collect::<Vec<_>>()
        };
        assert_eq!(parse_status_context(&args(&[])).unwrap(), None);
        assert_eq!(parse_status_context(&args(&["--json"])).unwrap(), None);
        assert_eq!(
            parse_status_context(&args(&["8192", "--json"])).unwrap(),
            Some(8192)
        );
        assert_eq!(
            parse_status_context(&args(&["--json", "8192"])).unwrap(),
            Some(8192)
        );
        assert!(parse_status_context(&args(&["--json", "--json"])).is_err());
        assert!(parse_status_context(&args(&["--unknown"])).is_err());
        assert!(parse_status_context(&args(&["two"]))
            .unwrap_err()
            .to_string()
            .contains("numeric token count"));
        assert!(parse_status_context(&args(&["8192", "4096"])).is_err());
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn idle_setup_lock_does_not_strand_an_unused_storage_choice() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let chosen = temp.path().join("chosen");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::create_dir(&chosen).unwrap();
        let path = state_dir.join("local-models.json");
        let other_state = state_dir.join("other-local-models.json");
        let current = state_dir.join("runtime");
        let lock_path = current.join("runtime-install-state.lock");

        let held = storage::acquire(&current.join("runtime-install-state")).unwrap();
        assert_eq!(
            storage_status(&path, &LocalSettings::default()).unwrap()["changeable"],
            false
        );
        assert!(set_managed_storage_at(&path, &chosen, false).is_err());
        assert!(set_managed_storage_at(&other_state, &chosen, false).is_err());
        drop(held);

        assert!(lock_path.is_file());
        std::fs::write(&lock_path, b"foreign lock data").unwrap();
        assert!(current_root_has_data(&current).unwrap());
        assert!(set_managed_storage_at(&path, &chosen, false).is_err());
        std::fs::write(&lock_path, b"").unwrap();
        assert_eq!(
            storage_status(&path, &LocalSettings::default()).unwrap()["changeable"],
            true
        );
        std::fs::write(current.join("ollama-partial.zip"), b"keep").unwrap();
        assert_eq!(
            storage_status(&path, &LocalSettings::default()).unwrap()["changeable"],
            false
        );
        assert!(set_managed_storage_at(&path, &chosen, false).is_err());
        std::fs::remove_file(current.join("ollama-partial.zip")).unwrap();

        assert_eq!(
            set_managed_storage_at(&path, &chosen, false).unwrap()["changed"],
            true
        );
        assert!(lock_path.is_file());
        assert!(!storage::load(&path).unwrap().managed_root_used);
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn managed_storage_choice_persists_and_isolated_state_ignores_it() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let chosen = temp.path().join("chosen");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir(&chosen).unwrap();
        let path = state_dir.join("local-models.json");
        let original = managed_root_at(&path, &LocalSettings::default(), false).unwrap();
        assert_eq!(original, state_dir.join("runtime"));
        assert!(storage_status(&path, &LocalSettings::default()).unwrap()["changeable"] == true);

        assert_eq!(
            set_managed_storage_at(&path, &chosen, false).unwrap()["changed"],
            true
        );
        let saved = storage::load(&path).unwrap();
        assert_eq!(saved.schema, 2);
        let resolved = managed_root_at(&path, &saved, false).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&chosen).unwrap());
        assert_eq!(
            saved.managed_root_identity,
            Some(phonton_local::disk::directory_identity(&resolved).unwrap())
        );
        assert_eq!(managed_root_at(&path, &saved, true).unwrap(), original);
        assert_eq!(storage_status(&path, &saved).unwrap()["source"], "chosen");
        assert_eq!(
            storage_status(&path, &saved).unwrap()["models_path"],
            json!(resolved.join("models"))
        );
        assert_eq!(
            storage_status(&path, &saved).unwrap()["runs_path"],
            json!(resolved.join("runs"))
        );
        assert_eq!(
            storage_status(&path, &LocalSettings::default()).unwrap()["runs_path"],
            json!(state_dir.join("runs"))
        );
        assert_eq!(
            chosen_run_root_at(&path, &saved, false).unwrap(),
            Some(resolved.join("runs"))
        );
        std::fs::write(resolved.join("runs"), b"occupied name").unwrap();
        assert!(chosen_run_root_at(&path, &saved, false).is_err());
        std::fs::remove_file(resolved.join("runs")).unwrap();
        std::fs::create_dir(resolved.join("runs")).unwrap();
        assert_eq!(
            chosen_run_root_at(&path, &saved, false).unwrap(),
            Some(resolved.join("runs"))
        );
        assert_eq!(
            model_store_status(&resolved, MANAGED_MODEL_ENDPOINT, false)["status"],
            "unverified"
        );
        assert!(set_managed_storage_at(&path, &chosen, true).is_err());
        assert_eq!(
            storage::load(&path).unwrap().managed_root,
            saved.managed_root
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn managed_storage_rejects_occupied_and_unsafe_folders_without_changing_state() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let occupied = temp.path().join("occupied");
        let empty = temp.path().join("empty");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::create_dir(&occupied).unwrap();
        std::fs::create_dir(&empty).unwrap();
        std::fs::write(occupied.join("notes.txt"), "keep").unwrap();
        let path = state_dir.join("local-models.json");
        assert!(set_managed_storage_at(&path, Path::new("relative"), false).is_err());
        assert!(set_managed_storage_at(&path, Path::new(r"\\server\share\models"), false).is_err());
        assert!(
            set_managed_storage_at(&path, temp.path().ancestors().last().unwrap(), false).is_err()
        );
        assert!(set_managed_storage_at(&path, &occupied, false).is_err());
        assert!(storage::load(&path).unwrap().managed_root.is_none());

        let legacy = state_dir.join("runtime");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("managed-process.json"), "stale receipt").unwrap();
        assert!(set_managed_storage_at(&path, &empty, false)
            .unwrap_err()
            .to_string()
            .contains("will not move"));
        assert!(legacy.join("managed-process.json").exists());
        assert!(storage::load(&path).unwrap().managed_root.is_none());
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn managed_storage_change_waits_for_operation_and_model_state_lease() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let chosen = temp.path().join("chosen");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::create_dir(&chosen).unwrap();
        let path = state_dir.join("local-models.json");
        let shared = Arc::new(Mutex::new(Operation {
            running: true,
            ..Default::default()
        }));
        assert!(set_managed_storage_guarded(&path, &chosen, false, &shared).is_err());
        shared.lock().unwrap().running = false;
        let held = storage::acquire(&path).unwrap();
        assert!(set_managed_storage_at(&path, &chosen, false).is_err());
        drop(held);
        assert_eq!(
            set_managed_storage_at(&path, &chosen, false).unwrap()["changed"],
            true
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn accepted_model_operation_holds_state_lease_before_worker_runs() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let chosen = temp.path().join("chosen");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::create_dir(&chosen).unwrap();
        let path = state_dir.join("local-models.json");
        let shared = Arc::new(Mutex::new(Operation::default()));
        let lease = admit_model_operation(
            &path,
            &shared,
            Operation {
                running: true,
                kind: "setup".into(),
                ..Default::default()
            },
            &OperationExpectation {
                endpoint: LocalSettings::default().endpoint,
                root: state_dir.join("runtime"),
            },
        )
        .unwrap();
        // A different process cannot see this process's Operation mutex; the
        // file lease still prevents its storage change before our worker starts.
        assert!(set_managed_storage_at(&path, &chosen, false).is_err());
        drop(lease);
        assert_eq!(
            set_managed_storage_at(&path, &chosen, false).unwrap()["changed"],
            true
        );
    }

    #[test]
    fn model_operation_admission_rejects_stale_endpoint_and_storage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("local-models.json");
        let shared = Arc::new(Mutex::new(Operation::default()));
        let original = LocalSettings::default();
        assert!(OperationExpectation::from_params(&json!({})).is_err());
        let expected = OperationExpectation::from_params(&json!({
            "expected_endpoint": original.endpoint.clone(),
            "expected_storage_root": temp.path().join("runtime"),
        }))
        .unwrap();
        let mut changed = original;
        changed.endpoint = "http://127.0.0.1:11435".into();
        storage::save(&path, &changed).unwrap();
        let accepted = || Operation {
            running: true,
            kind: "install".into(),
            ..Default::default()
        };

        let error = admit_model_operation(&path, &shared, accepted(), &expected)
            .err()
            .unwrap();
        assert!(error.to_string().contains("endpoint changed"));
        assert!(!shared.lock().unwrap().running);

        // A stale displayed root must fail even on platforms where choosing
        // a custom managed storage folder is not supported.
        let current_endpoint = OperationExpectation {
            endpoint: changed.endpoint.clone(),
            root: temp.path().join("stale-runtime"),
        };
        let error = admit_model_operation(&path, &shared, accepted(), &current_endpoint)
            .err()
            .unwrap();
        assert!(error.to_string().contains("storage changed"));
        assert!(!shared.lock().unwrap().running);

        let current = OperationExpectation {
            root: temp.path().join("runtime"),
            ..current_endpoint
        };
        let lease = admit_model_operation(&path, &shared, accepted(), &current).unwrap();
        assert!(shared.lock().unwrap().running);
        drop(lease);
    }

    #[test]
    fn model_operation_admission_validates_selected_storage_for_platform() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("local-models.json");
        let chosen = temp.path().join("chosen");
        std::fs::create_dir(&chosen).unwrap();
        let settings = LocalSettings {
            schema: 2,
            managed_root: Some(chosen.clone()),
            ..Default::default()
        };
        storage::save(&path, &settings).unwrap();
        let saved = std::fs::read(&path).unwrap();
        let shared = Arc::new(Mutex::new(Operation::default()));
        let expected = OperationExpectation {
            endpoint: settings.endpoint.clone(),
            root: temp.path().join("runtime"),
        };
        let accepted = || Operation {
            running: true,
            kind: "install".into(),
            ..Default::default()
        };
        let error = admit_model_operation(&path, &shared, accepted(), &expected)
            .err()
            .unwrap();
        assert!(!shared.lock().unwrap().running);
        assert_eq!(std::fs::read(&path).unwrap(), saved);
        assert!(storage::acquire(&path).is_ok());

        #[cfg(windows)]
        let current = {
            assert!(error.to_string().contains("storage changed"));
            OperationExpectation {
                root: chosen,
                ..expected
            }
        };
        #[cfg(not(windows))]
        let current = {
            assert!(error.to_string().contains("supports Windows only"));
            // Recover by using supported default storage; rejection must not
            // leave the operation running or retain the cross-process lease.
            let mut settings = settings;
            settings.managed_root = None;
            storage::save(&path, &settings).unwrap();
            expected
        };
        let lease = admit_model_operation(&path, &shared, accepted(), &current).unwrap();
        assert!(shared.lock().unwrap().running);
        assert!(storage::acquire(&path).is_err());
        drop(lease);
        assert!(storage::acquire(&path).is_ok());
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn missing_unused_storage_can_change_but_used_storage_cannot_be_abandoned() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let path = state_dir.join("local-models.json");
        set_managed_storage_at(&path, &first, false).unwrap();
        std::fs::remove_dir(&first).unwrap();
        assert_eq!(
            storage_status(&path, &storage::load(&path).unwrap()).unwrap()["changeable"],
            true
        );
        set_managed_storage_at(&path, &second, false).unwrap();
        let mut settings = storage::load(&path).unwrap();
        settings.managed_root_used = true;
        storage::save(&path, &settings).unwrap();
        assert!(storage_status(&path, &settings).unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("claimed this folder"));
        std::fs::remove_dir(&second).unwrap();
        assert_eq!(
            storage_status(&path, &settings).unwrap()["changeable"],
            false
        );
        std::fs::create_dir(&first).unwrap();
        assert!(set_managed_storage_at(&path, &first, false)
            .unwrap_err()
            .to_string()
            .contains("will not move"));
        assert_eq!(
            storage::load(&path).unwrap().managed_root,
            settings.managed_root
        );
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn replaced_used_folder_at_same_path_is_not_the_saved_store() {
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let chosen = temp.path().join("chosen");
        std::fs::create_dir(&state_dir).unwrap();
        std::fs::create_dir(&chosen).unwrap();
        let path = state_dir.join("local-models.json");
        set_managed_storage_at(&path, &chosen, false).unwrap();
        let mut settings = storage::load(&path).unwrap();
        settings.managed_root_used = true;
        storage::save(&path, &settings).unwrap();
        std::fs::rename(&chosen, temp.path().join("old-chosen")).unwrap();
        std::fs::create_dir(&chosen).unwrap();
        let root = managed_root_at(&path, &settings, false).unwrap();
        assert!(verify_managed_root_identity(&root, &settings)
            .unwrap_err()
            .to_string()
            .contains("different directory or volume"));
        assert!(storage_status(&path, &settings).unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("Reconnect the original folder"));
    }

    #[test]
    fn changing_loopback_endpoint_clears_selection_but_preserves_evidence() {
        let mut settings = LocalSettings {
            active_model: Some("fixture:1b".into()),
            ..Default::default()
        };
        let original = settings.endpoint.clone();
        assert!(!change_endpoint(&mut settings, "http://localhost:11434/").unwrap());
        assert_eq!(settings.active_model.as_deref(), Some("fixture:1b"));
        assert!(change_endpoint(&mut settings, "http://[::1]:11435/").unwrap());
        assert_eq!(settings.endpoint, "http://[::1]:11435");
        assert!(settings.active_model.is_none());
        assert!(change_endpoint(&mut settings, "https://ollama.example:11434").is_err());
        assert!(change_endpoint(&mut settings, "http://127.0.0.1:11434/api").is_err());
        assert_eq!(settings.endpoint, "http://[::1]:11435");
        assert!(change_endpoint(&mut settings, &original).unwrap());
        assert!(settings.active_model.is_none());
    }

    #[test]
    fn endpoint_setting_is_atomic_and_refuses_a_held_model_lease() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("local-models.json");
        let mut settings = LocalSettings {
            active_model: Some("fixture:1b".into()),
            ..Default::default()
        };
        settings.profiles.push(ModelProfile {
            schema: 1,
            model: "fixture:1b".into(),
            digest: "sha256:fixture".into(),
            runtime_version: "1.2.3".into(),
            endpoint: settings.endpoint.clone(),
            context_tokens: 4096,
            output_tokens: 512,
            protocol: None,
            thinking: None,
            probes: Vec::new(),
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 0,
        });
        storage::save(&path, &settings).unwrap();
        let original = std::fs::read(&path).unwrap();
        let held = storage::acquire(&path).unwrap();
        assert!(set_endpoint_at(&path, "http://127.0.0.1:11435").is_err());
        drop(held);
        assert!(set_endpoint_at(&path, "http://example.com:11435").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let result = set_endpoint_at(&path, "http://localhost:11435/").unwrap();
        assert_eq!(result["endpoint"], "http://127.0.0.1:11435");
        assert_eq!(result["changed"], true);
        assert_eq!(result["loopback_only"], true);
        assert_eq!(result["local_only"], false);
        let changed = storage::load(&path).unwrap();
        assert!(changed.active_model.is_none());
        assert_eq!(changed.profiles.len(), 1);
        assert_eq!(changed.profiles[0].endpoint, settings.endpoint);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            set_endpoint_at(&path, "http://127.0.0.1:11435").unwrap()["changed"],
            false
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn accepted_model_operation_blocks_endpoint_change_before_worker_starts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("local-models.json");
        storage::save(&path, &LocalSettings::default()).unwrap();
        let shared = Arc::new(Mutex::new(Operation {
            running: true,
            ..Default::default()
        }));
        let original = std::fs::read(&path).unwrap();
        assert!(
            set_endpoint_guarded(&path, "http://127.0.0.1:11435", &shared)
                .unwrap_err()
                .to_string()
                .contains("Another model operation")
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        shared.lock().unwrap().running = false;
        assert_eq!(
            set_endpoint_at(&path, "http://127.0.0.1:11435").unwrap()["changed"],
            true
        );
    }

    #[test]
    fn cancellation_refuses_terminal_model_mutations_after_dispatch() {
        for kind in ["select", "deselect", "remove"] {
            let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
            let shared = Arc::new(Mutex::new(Operation {
                id: "operation-1".into(),
                kind: kind.into(),
                running: true,
                cancel: Some(cancel_tx),
                ..Default::default()
            }));
            let response = request_cancel(&shared, "operation-1").unwrap();
            assert_eq!(response["cancel_requested"], false, "{kind}");
            assert!(!*cancel_rx.borrow(), "{kind}");
            assert!(request_cancel(&shared, "stale-id").is_err());
        }
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let install = Arc::new(Mutex::new(Operation {
            id: "download-1".into(),
            kind: "install".into(),
            running: true,
            cancel: Some(cancel_tx),
            ..Default::default()
        }));
        assert_eq!(
            request_cancel(&install, "download-1").unwrap()["cancel_requested"],
            true
        );
        assert!(*cancel_rx.borrow());
    }

    #[tokio::test]
    async fn terminal_model_mutations_finish_even_if_cancellation_signal_arrives() {
        for kind in ["select", "deselect", "remove"] {
            let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
            let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
            let work = async move {
                finish_rx.await.unwrap();
                Ok(json!({"finished": kind}))
            };
            let task = tokio::spawn(async move {
                await_model_mutation(kind, work, async move {
                    cancel_rx.changed().await.unwrap();
                })
                .await
            });
            cancel_tx.send(true).unwrap();
            tokio::task::yield_now().await;
            assert!(!task.is_finished(), "{kind}");
            finish_tx.send(()).unwrap();
            assert_eq!(task.await.unwrap().unwrap()["finished"], kind);
        }
    }

    #[tokio::test]
    async fn status_metadata_budget_keeps_active_model_ready_when_other_rows_stall() {
        let app = axum::Router::new().route(
            "/api/show",
            axum::routing::post(|axum::Json(request): axum::Json<Value>| async move {
                if request["model"] == "fixture:error" {
                    return axum::Json(json!({}));
                }
                if request["model"] != "fixture:active" {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                axum::Json(json!({"model_info":{"general.architecture":"fixture","fixture.context_length":8192},"details":{"format":"gguf"}}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let runtime = LocalRuntime::new(&endpoint).unwrap();
        let models: Vec<LocalModel> = std::iter::once("fixture:error".to_string())
            .chain((0..5).map(|n| format!("fixture:slow-{n}")))
            .chain(["fixture:active".into()])
            .map(|name| LocalModel {
                name,
                digest: "sha256:fixture".into(),
                size_bytes: 1024,
                parameter_size: None,
                quantization: None,
            })
            .collect();
        let started = Instant::now();
        let metadata = fetch_model_metadata(
            &runtime,
            &models,
            Some("fixture:active"),
            Duration::from_millis(750),
        )
        .await;
        assert!(started.elapsed() < Duration::from_millis(1500));
        assert_eq!(metadata.len(), models.len());
        assert!(matches!(&metadata[0], Err(ModelMetadataError::Rejected(_))));
        assert!(metadata[0]
            .as_ref()
            .unwrap_err()
            .message()
            .contains("local GGUF model metadata"));
        assert!(metadata[1..6].iter().all(|row| row
            .as_ref()
            .unwrap_err()
            .message()
            .contains("status budget")));
        assert!(metadata[1..6]
            .iter()
            .all(|row| matches!(row, Err(ModelMetadataError::BudgetExpired(_)))));
        assert_eq!(
            model_context_ceiling(metadata[6].as_ref().unwrap()),
            Some(8192)
        );
        server.abort();
    }

    #[test]
    fn status_prefers_profile_for_current_endpoint() {
        let model = LocalModel {
            name: "fixture:1b".into(),
            digest: "sha256:fixture".into(),
            size_bytes: 1024,
            parameter_size: None,
            quantization: None,
        };
        let mut settings = LocalSettings::default();
        let profile = ModelProfile {
            schema: 1,
            model: model.name.clone(),
            digest: model.digest.clone(),
            runtime_version: "1.2.3".into(),
            endpoint: settings.endpoint.clone(),
            context_tokens: 4096,
            output_tokens: 512,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: None,
            probes: vec![ModelProbe {
                name: "SearchReplace edit".into(),
                status: CheckStatus::Passed,
                output: String::new(),
                detail: String::new(),
                input_tokens: None,
                output_tokens: None,
                elapsed_ms: 0,
            }],
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 0,
        };
        settings.profiles.push(profile.clone());
        settings.endpoint = "http://127.0.0.1:11435".into();
        assert!(profile_for_status(&settings, &model, "1.2.3")
            .1
            .unwrap()
            .contains("another local endpoint"));
        settings.profiles.push(ModelProfile {
            endpoint: settings.endpoint.clone(),
            ..profile
        });
        let (current, error) = profile_for_status(&settings, &model, "1.2.3");
        assert!(error.is_none());
        assert_eq!(current.unwrap().endpoint, settings.endpoint);
        assert!(unusable_calibration_evidence(&settings, &model, current).is_none());
        let (ready, profile_error, limit, context_error) = profile_and_context_for_status(
            &settings,
            &model,
            "1.2.3",
            &Err(ModelMetadataError::BudgetExpired(
                "Metadata status budget expired".into(),
            )),
        );
        assert!(ready.is_some());
        assert!(profile_error.is_none());
        assert!(limit.is_none());
        assert_eq!(
            context_error.as_deref(),
            Some("Metadata status budget expired")
        );
        let (invalid, profile_error, _, _) = profile_and_context_for_status(
            &settings,
            &model,
            "1.2.4",
            &Err(ModelMetadataError::BudgetExpired(
                "Metadata status budget expired".into(),
            )),
        );
        assert!(invalid.is_none());
        assert!(profile_error.unwrap().contains("Calibrate again"));
        let (rejected, profile_error, _, context_error) = profile_and_context_for_status(
            &settings,
            &model,
            "1.2.3",
            &Err(ModelMetadataError::Rejected(
                "Runtime reports a remote/cloud model".into(),
            )),
        );
        assert!(rejected.is_none());
        assert!(profile_error.unwrap().contains("remote/cloud"));
        assert!(context_error.unwrap().contains("remote/cloud"));
        let (over_limit, profile_error, limit, context_error) = profile_and_context_for_status(
            &settings,
            &model,
            "1.2.3",
            &Ok(
                json!({"model_info":{"general.architecture":"fixture","fixture.context_length":2048}}),
            ),
        );
        assert!(over_limit.is_none());
        assert!(profile_error.unwrap().contains("exceeds"));
        assert_eq!(limit, Some(2048));
        assert!(context_error.is_none());
    }

    #[test]
    fn failed_edit_calibration_keeps_evidence_without_a_selectable_profile() {
        let model = LocalModel {
            name: "fixture:1b".into(),
            digest: "sha256:fixture".into(),
            size_bytes: 1024,
            parameter_size: None,
            quantization: None,
        };
        let mut settings = LocalSettings::default();
        settings.profiles.push(ModelProfile {
            schema: 1,
            model: model.name.clone(),
            digest: model.digest.clone(),
            runtime_version: "1.2.3".into(),
            endpoint: settings.endpoint.clone(),
            context_tokens: 4096,
            output_tokens: 512,
            protocol: None,
            thinking: None,
            probes: vec![
                ModelProbe {
                    name: "SearchReplace edit".into(),
                    status: CheckStatus::Failed,
                    output: "wrong JSON".into(),
                    detail: "format failed".into(),
                    input_tokens: Some(12),
                    output_tokens: Some(5),
                    elapsed_ms: 7,
                },
                ModelProbe {
                    name: "UnifiedDiff edit".into(),
                    status: CheckStatus::Failed,
                    output: "wrong diff".into(),
                    detail: "format failed".into(),
                    input_tokens: Some(12),
                    output_tokens: Some(5),
                    elapsed_ms: 8,
                },
            ],
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 1,
        });
        let (ready, error) = profile_for_status(&settings, &model, "1.2.3");
        assert!(ready.is_none());
        assert!(error.unwrap().contains("No edit protocol passed"));
        let evidence = unusable_calibration_evidence(&settings, &model, ready).unwrap();
        assert_eq!(evidence.probes[0].output, "wrong JSON");
        assert_eq!(evidence.probes[1].output, "wrong diff");
        assert_eq!(
            calibration_exit_code("calibrate", &serde_json::to_value(evidence).unwrap()),
            2
        );
        assert_eq!(
            calibration_exit_code("status", &serde_json::json!({"protocol":null})),
            0
        );
        assert_eq!(
            calibration_exit_code(
                "calibrate",
                &serde_json::json!({"protocol":"search_replace"})
            ),
            0
        );
        let replaced = LocalModel {
            digest: "sha256:changed".into(),
            ..model
        };
        let (ready, error) = profile_for_status(&settings, &replaced, "1.2.3");
        assert!(ready.is_none());
        assert!(error.unwrap().contains("changed since calibration"));
        assert!(unusable_calibration_evidence(&settings, &replaced, ready).is_some());
    }

    #[test]
    fn incomplete_attempt_survives_reopen_but_is_never_a_selectable_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-models.json");
        let model = LocalModel {
            name: "fixture:latest".into(),
            digest: "digest-a".into(),
            size_bytes: 1024,
            parameter_size: None,
            quantization: None,
        };
        let settings = LocalSettings {
            schema: 4,
            calibration_attempt: Some(CalibrationAttempt {
                schema: 1,
                model: model.name.clone(),
                starting_digest: model.digest.clone(),
                runtime_version: "1.2.3".into(),
                endpoint: "http://127.0.0.1:11434".into(),
                context_tokens: 4096,
                hardware: HardwareSnapshot::default(),
                started_at_unix: 1,
                probes: vec![ModelProbe {
                    name: "Think off: SearchReplace edit".into(),
                    status: CheckStatus::Passed,
                    output: "candidate output".into(),
                    detail: "edit format passed".into(),
                    input_tokens: Some(12),
                    output_tokens: Some(5),
                    elapsed_ms: 7,
                }],
            }),
            ..Default::default()
        };
        storage::save(&path, &settings).unwrap();
        let reopened = storage::load(&path).unwrap();
        assert_eq!(
            reopened.calibration_attempt.as_ref().unwrap().probes.len(),
            1
        );
        let (ready, _) = profile_for_status(&reopened, &model, "1.2.3");
        assert!(ready.is_none());
        assert!(unusable_calibration_evidence(&reopened, &model, ready).is_none());
        assert!(reopened.profiles.is_empty());
    }

    #[test]
    fn omitted_context_is_auto_and_explicit_context_is_exact() {
        assert_eq!(context_param(&json!({})).unwrap(), None);
        assert_eq!(context_param(&json!({"context":null})).unwrap(), None);
        assert_eq!(context_param(&json!({"context":8192})).unwrap(), Some(8192));
        assert!(context_param(&json!({"context":"8192"})).is_err());
    }

    #[test]
    fn status_fit_names_the_profile_or_requested_context() {
        let model = LocalModel {
            name: "fixture:1b".into(),
            digest: "sha256:fixture".into(),
            size_bytes: 1 << 30,
            parameter_size: None,
            quantization: None,
        };
        let hardware = HardwareSnapshot {
            ram_available_bytes: Some(3 << 30),
            gpus: vec![GpuSnapshot {
                name: "GPU".into(),
                total_bytes: 3 << 30,
                available_bytes: 3 << 30,
            }],
            ..Default::default()
        };
        let profile = ModelProfile {
            schema: 1,
            model: model.name.clone(),
            digest: model.digest.clone(),
            runtime_version: "1.2.3".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            context_tokens: 8192,
            output_tokens: 512,
            protocol: None,
            thinking: None,
            probes: Vec::new(),
            hardware: hardware.clone(),
            measured_at_unix: 0,
        };
        let fit = fit_for_status(&model, &hardware, Some(&profile), None, Some(32768));
        assert_eq!(fit.context_tokens, 8192);
        assert_eq!(fit.suggested_context, Some(4096));
        assert_eq!(
            fit.status,
            phonton_types::local::FitStatus::InsufficientMemory
        );
        let override_fit =
            fit_for_status(&model, &hardware, Some(&profile), Some(2048), Some(32768));
        assert_eq!(override_fit.context_tokens, 2048);
        assert_eq!(override_fit.suggested_context, Some(4096));
        let unmeasured = fit_for_status(&model, &hardware, None, None, None);
        assert_eq!(unmeasured.context_tokens, 4096);
        assert_eq!(unmeasured.suggested_context, None);
    }

    #[test]
    fn status_hides_stale_profiles_but_keeps_edit_ready_when_creation_fails() {
        let model = LocalModel {
            name: "fixture:1b".into(),
            digest: "sha256:fixture".into(),
            size_bytes: 1024,
            parameter_size: None,
            quantization: None,
        };
        let edit_probe = ModelProbe {
            name: "SearchReplace edit".into(),
            status: CheckStatus::Passed,
            output: String::new(),
            detail: String::new(),
            input_tokens: None,
            output_tokens: None,
            elapsed_ms: 0,
        };
        let profile = ModelProfile {
            schema: 1,
            model: model.name.clone(),
            digest: model.digest.clone(),
            runtime_version: "1.2.3".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            context_tokens: 4096,
            output_tokens: 512,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: None,
            probes: vec![edit_probe],
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 0,
        };
        let mut settings = LocalSettings {
            profiles: vec![profile],
            ..Default::default()
        };
        let (valid, error) = profile_for_status(&settings, &model, "1.2.3");
        assert!(error.is_none());
        assert_eq!(valid.unwrap().creation_status(), CheckStatus::NotRun);
        settings.profiles[0].probes.push(ModelProbe {
            name: CREATION_PROBE_NAME.into(),
            status: CheckStatus::Failed,
            output: "wrong file".into(),
            detail: String::new(),
            input_tokens: None,
            output_tokens: None,
            elapsed_ms: 0,
        });
        let (edit_ready, error) = profile_for_status(&settings, &model, "1.2.3");
        assert!(error.is_none());
        assert_eq!(edit_ready.unwrap().creation_status(), CheckStatus::Failed);
        let (stale, error) = profile_for_status(&settings, &model, "1.2.4");
        assert!(stale.is_none());
        assert!(error.unwrap().contains("Calibrate again"));
        let mut replaced = model.clone();
        replaced.digest = "sha256:updated".into();
        assert!(profile_for_status(&settings, &replaced, "1.2.3")
            .0
            .is_none());
        settings.profiles[0].protocol = Some(EditProtocol::UnifiedDiff);
        settings.profiles[0].probes[0].name = "UnifiedDiff edit".into();
        assert_eq!(
            profile_for_status(&settings, &model, "1.2.3")
                .0
                .map(ModelProfile::creation_status),
            Some(CheckStatus::NotRun)
        );
        settings.profiles[0].probes.push(ModelProbe {
            name: DIFF_CREATION_PROBE_NAME.into(),
            status: CheckStatus::Passed,
            output: String::new(),
            detail: String::new(),
            input_tokens: None,
            output_tokens: None,
            elapsed_ms: 0,
        });
        assert_eq!(
            profile_for_status(&settings, &model, "1.2.3")
                .0
                .map(ModelProfile::creation_status),
            Some(CheckStatus::Passed)
        );
    }
    fn progress(completed: u64) -> DownloadProgress {
        DownloadProgress {
            status: "pulling layer".into(),
            digest: Some("sha256:fixture".into()),
            completed: Some(completed),
            total: Some(1000),
        }
    }
    #[test]
    fn progress_coalesces_small_updates_but_preserves_exact_completed_bytes() {
        let mut printer = ProgressPrinter::default();
        assert!(printer.line(progress(0), Duration::ZERO).is_some());
        for tick in 1..250 {
            assert!(printer
                .line(progress(tick), Duration::from_millis(tick))
                .is_none());
        }
        assert!(printer
            .line(progress(250), Duration::from_millis(250))
            .unwrap()
            .contains("250 / 1000"));
        assert!(printer
            .line(progress(1000), Duration::from_millis(251))
            .unwrap()
            .contains("1000 / 1000"));
        assert!(printer
            .line(progress(1000), Duration::from_secs(1))
            .is_none());
    }
    #[test]
    fn progress_never_hides_backwards_bytes_layer_changes_or_unknown_sizes() {
        let mut printer = ProgressPrinter::default();
        printer.line(progress(500), Duration::ZERO);
        assert!(printer
            .line(progress(300), Duration::from_millis(1))
            .unwrap()
            .contains("300 / 1000"));
        let mut next = progress(0);
        next.digest = Some("sha256:next".into());
        assert!(printer
            .line(next, Duration::from_millis(2))
            .unwrap()
            .contains("sha256:next"));
        let stage = DownloadProgress {
            status: "verifying digest".into(),
            ..Default::default()
        };
        assert_eq!(
            printer.line(stage, Duration::from_millis(3)).unwrap(),
            "verifying digest"
        );
    }
}
