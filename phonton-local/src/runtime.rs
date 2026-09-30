//! Ollama transport with loopback-only inference and bounded response handling.

use crate::{estimate_fit, LocalError, Result};
use phonton_types::local::*;
use reqwest::{redirect::Policy, Client, Url};
use serde_json::{json, Value};
use std::future::Future;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const MAX_RESPONSE: usize = 4 * 1024 * 1024;
const MIN_RESIDENT_LIFETIME_SECONDS: i64 = 30;
const CALIBRATION_HOST_RESERVE_BYTES: u64 = 1536 * 1024 * 1024;
const MAX_INVENTORY_WARNINGS: usize = 8;
const MAX_SAVED_ATTEMPT_OUTPUT_CHARS: usize = 16 * 1024;

struct CalibrationAdmission {
    context_tokens: u32,
    required_resident: Option<ResidentModel>,
}

#[cfg(all(windows, target_arch = "x86_64"))]
struct CancelBlobScan(std::sync::Arc<std::sync::atomic::AtomicBool>);

#[cfg(all(windows, target_arch = "x86_64"))]
impl Drop for CancelBlobScan {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn parse_installed_model(model: &Value) -> Result<LocalModel> {
    let name = model["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| LocalError::Invalid("Model name missing".into()))?;
    canonical_model_name(name)?;
    let digest = model["digest"]
        .as_str()
        .filter(|digest| !digest.is_empty())
        .ok_or_else(|| LocalError::Invalid("Model digest missing".into()))?;
    let size_bytes = model["size"]
        .as_u64()
        .ok_or_else(|| LocalError::Invalid("Model size missing".into()))?;
    if size_bytes == 0 {
        return Err(LocalError::Invalid("Model size is zero".into()));
    }
    Ok(LocalModel {
        name: name.into(),
        digest: digest.into(),
        size_bytes,
        parameter_size: model["details"]["parameter_size"]
            .as_str()
            .map(str::to_owned),
        quantization: model["details"]["quantization_level"]
            .as_str()
            .map(str::to_owned),
    })
}

fn parse_installed_inventory(value: &Value) -> Result<InstalledInventory> {
    let entries = value["models"]
        .as_array()
        .ok_or_else(|| LocalError::Invalid("Runtime did not return a model list.".into()))?;
    let mut inventory = InstalledInventory {
        models: Vec::with_capacity(entries.len()),
        warnings: Vec::new(),
    };
    let mut omitted = 0;
    for (index, entry) in entries.iter().enumerate() {
        match parse_installed_model(entry) {
            Ok(model) => inventory.models.push(model),
            Err(error) => {
                omitted += 1;
                if inventory.warnings.len() < MAX_INVENTORY_WARNINGS {
                    inventory.warnings.push(format!(
                        "Runtime model entry {} omitted: {error}.",
                        index + 1
                    ));
                }
            }
        }
    }
    if omitted > inventory.warnings.len() {
        inventory.warnings.push(format!(
            "{} more unusable runtime model entries omitted.",
            omitted - inventory.warnings.len()
        ));
    }
    Ok(inventory)
}

fn mark_first_try(models: &mut [CatalogModel]) {
    // The catalog has only manifest sizes and a 4K cold-load estimate. This
    // chooses a resource starting point, never a quality or speed winner.
    let eligible = |model: &CatalogModel, status: FitStatus| -> Option<u64> {
        if model.error.is_some() {
            return None;
        }
        let bytes = model.download_bytes.filter(|bytes| *bytes > 0)?;
        let fit = model.fit.as_ref()?;
        (fit.status == status && fit.context_tokens == 4096).then_some(bytes)
    };
    for model in models.iter_mut() {
        model.first_try_reason = None;
    }
    let gpu = models
        .iter()
        .enumerate()
        .filter_map(|(index, model)| {
            eligible(model, FitStatus::LikelyFitsGpu)
                .map(|bytes| (index, bytes, model.name.as_str()))
        })
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.2.cmp(a.2)))
        .map(|(index, _, _)| index);
    if let Some(index) = gpu {
        models[index].first_try_reason = Some("Largest manifest-backed catalog entry with a likely 4K cold-load fit on one observed GPU at browse time. This is a memory-based starting point, not a coding-quality or speed result. Calibrate after download before selecting.".into());
        return;
    }
    let cpu = models
        .iter()
        .enumerate()
        .filter_map(|(index, model)| {
            eligible(model, FitStatus::CpuOrOffload)
                .map(|bytes| (index, bytes, model.name.as_str()))
        })
        .min_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(b.2)))
        .map(|(index, _, _)| index);
    if let Some(index) = cpu {
        models[index].first_try_reason = Some("Smallest manifest-backed catalog entry with a 4K CPU/offload cold-load fit. CPU/offload speed and coding quality are unmeasured. Calibrate after download before selecting.".into());
    }
}

async fn catalog_entry(
    http: &Client,
    hardware: &HardwareSnapshot,
    name: String,
    source: String,
) -> CatalogModel {
    let result = async {
        let value = response_json(http.get(&source).send().await?).await?;
        manifest_descriptor_bytes(&value)
    }
    .await;
    let (download_bytes, fit, error) = match result {
        Ok(size) => (Some(size), Some(estimate_fit(size, hardware)), None),
        Err(error) => (None, None, Some(error.to_string())),
    };
    CatalogModel {
        name,
        source,
        download_bytes,
        fit,
        error,
        first_try_reason: None,
        pre_setup_storage: None,
    }
}

fn manifest_descriptors(manifest: &Value) -> Result<Vec<crate::disk::ManifestBlob>> {
    let layers = manifest["layers"]
        .as_array()
        .filter(|layers| !layers.is_empty())
        .ok_or_else(|| LocalError::Invalid("Registry manifest has no layers".into()))?;
    let config = manifest
        .get("config")
        .ok_or_else(|| LocalError::Invalid("Registry manifest has no config blob".into()))?;
    let mut seen = std::collections::HashMap::<String, u64>::new();
    let mut blobs = Vec::new();
    let mut total = 0u64;
    for descriptor in std::iter::once(config).chain(layers) {
        let digest = descriptor["digest"]
            .as_str()
            .ok_or_else(|| LocalError::Invalid("Registry manifest blob has no digest".into()))?;
        let hash = digest.strip_prefix("sha256:").ok_or_else(|| {
            LocalError::Invalid("Registry manifest blob has an unsupported digest".into())
        })?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(LocalError::Invalid(
                "Registry manifest blob has an invalid SHA-256 digest".into(),
            ));
        }
        let bytes = descriptor["size"]
            .as_u64()
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| {
                LocalError::Invalid("Registry manifest blob has no positive size".into())
            })?;
        let hash = hash.to_ascii_lowercase();
        match seen.insert(hash.clone(), bytes) {
            Some(previous) if previous != bytes => {
                return Err(LocalError::Invalid(
                    "Registry manifest repeats a digest with conflicting sizes".into(),
                ));
            }
            Some(_) => {}
            None => {
                total = total.checked_add(bytes).ok_or_else(|| {
                    LocalError::Invalid("Registry manifest size overflowed".into())
                })?;
                blobs.push(crate::disk::ManifestBlob {
                    sha256: hash,
                    size_bytes: bytes,
                });
            }
        }
    }
    Ok(blobs)
}

fn manifest_descriptor_bytes(manifest: &Value) -> Result<u64> {
    Ok(manifest_descriptors(manifest)?
        .iter()
        .map(|blob| blob.size_bytes)
        .sum())
}

fn official_manifest_url(name: &str) -> Result<String> {
    validate_model(name)?;
    let (repository, tag) = name.rsplit_once(':').unwrap_or((name, "latest"));
    let segments: Vec<_> = repository.split('/').collect();
    let (namespace, family) = match segments.as_slice() {
        [family] => ("library", *family),
        [namespace, family] => (*namespace, *family),
        _ => return Err(LocalError::Invalid(
            "Disk preflight supports registry names of the form model:tag or namespace/model:tag"
                .into(),
        )),
    };
    let valid = |segment: &str| {
        !segment.is_empty()
            && !segment.starts_with('.')
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
    };
    if !valid(namespace)
        || !valid(family)
        || tag.is_empty()
        || tag.contains(':')
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
    {
        return Err(LocalError::Invalid(
            "Model name cannot be mapped to an official registry manifest".into(),
        ));
    }
    Ok(format!(
        "https://registry.ollama.ai/v2/{namespace}/{family}/manifests/{tag}"
    ))
}

async fn catalog_entries(
    http: &Client,
    hardware: &HardwareSnapshot,
    entries: [(String, String); 5],
) -> Vec<CatalogModel> {
    let [first, second, third, fourth, fifth] =
        entries.map(|(name, source)| catalog_entry(http, hardware, name, source));
    // The curated set is fixed at five. Poll every independent registry read
    // together, then keep display order regardless of response order.
    let (first, second, third, fourth, fifth) = tokio::join!(first, second, third, fourth, fifth);
    let mut models = vec![first, second, third, fourth, fifth];
    mark_first_try(&mut models);
    models
}

/// Bounded edit inputs and exact source spans for constrained JSON decoding.
pub struct EditRequest<'a> {
    /// Installed model identity.
    pub model: &'a str,
    /// Measured editing protocol.
    pub protocol: EditProtocol,
    /// Harness-owned instructions.
    pub system: &'a str,
    /// Goal and captured source data.
    pub user: &'a str,
    /// Calibrated context ceiling.
    pub context: u32,
    /// Reserved output tokens.
    pub output: u32,
    /// Thinking request measured with this model profile.
    pub thinking: LocalThinkingMode,
    /// Allowed repository-relative files.
    pub paths: &'a [String],
    /// Exact search spans from captured files.
    pub searches: &'a [String],
    /// Exact approved creation path for a constrained whole-file JSON edit.
    pub create_path: Option<&'a str>,
    /// Require the selected loaded model to remain observable just before chat.
    pub required_resident: Option<&'a ResidentModel>,
}

/// Bounded structured strategy request, separate from a candidate edit.
pub struct HypothesisRequest<'a> {
    /// Selected installed model.
    pub model: &'a str,
    /// Harness-owned instruction text.
    pub system: &'a str,
    /// Captured source excerpts and rejected-candidate evidence.
    pub user: &'a str,
    /// Exact response shape and reviewed path/anchor choices.
    pub schema: &'a Value,
    /// Calibrated context ceiling.
    pub context: u32,
    /// Reserved maximum output tokens.
    pub output: u32,
    /// Thinking request measured with this model profile.
    pub thinking: LocalThinkingMode,
    /// Exact resident identity required for low-memory reuse.
    pub required_resident: Option<&'a ResidentModel>,
}

/// Exact structured-edit constraint schema, shared by context admission and the
/// runtime request. Its serialized size is a byte bound, not a token measurement.
pub fn edit_schema(paths: &[String], searches: &[String]) -> Value {
    json!({"type":"object", "properties": {"path":{"type":"string","enum":paths}, "search":{"type":"string","enum":searches}, "text":{"type":"string"}}, "required":["path","search","text"], "additionalProperties":false})
}

/// Constrain a proposed new file to the one explicit path. Candidate checks,
/// not this schema, determine whether its contents solve the goal.
pub fn create_schema(path: &str) -> Value {
    json!({"type":"object", "properties": {"path":{"type":"string","enum":[path]}, "text":{"type":"string"}}, "required":["path","text"], "additionalProperties":false})
}

/// A loopback Ollama client. Clones share a connection pool, not model state.
#[derive(Clone)]
pub struct LocalRuntime {
    endpoint: String,
    http: Client,
}

fn with_thinking_mode(mut request: Value, thinking: LocalThinkingMode) -> Value {
    if thinking == LocalThinkingMode::Off {
        request["think"] = json!(false);
    }
    request
}

struct ModeCalibration {
    probes: Vec<ModelProbe>,
    edit_protocol: Option<EditProtocol>,
    creation_protocol: Option<EditProtocol>,
}

impl ModeCalibration {
    fn selected_protocol(&self) -> Option<EditProtocol> {
        self.creation_protocol.or(self.edit_protocol)
    }

    fn prefix_probes(&mut self, mode: &str) {
        for probe in &mut self.probes {
            probe.name = format!("{mode}: {}", probe.name);
        }
    }
}

/// Normalize a loopback endpoint without DNS, credentials, query or redirects.
pub fn local_endpoint(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw).map_err(|e| LocalError::Invalid(e.to_string()))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(LocalError::Invalid(
            "Local endpoint must be a loopback HTTP(S) origin without credentials, path or query."
                .into(),
        ));
    }
    if url.host_str() == Some("localhost") {
        url.set_host(Some("127.0.0.1"))
            .map_err(|_| LocalError::Invalid("Invalid loopback address".into()))?;
    }
    let loopback = url
        .host_str()
        .and_then(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|ip| ip.is_loopback());
    if !loopback {
        return Err(LocalError::Invalid("Local-only mode requires a literal loopback address (127.0.0.1 or [::1]). Remote inference is disabled.".into()));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// Reject remote model selectors and malformed runtime names before a request.
pub fn validate_model(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 200
        || name.starts_with('/')
        || name.contains("..")
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.:/".contains(c))
        || name.to_ascii_lowercase().contains("cloud")
    {
        return Err(LocalError::Invalid(
            "Use a local model tag such as qwen2.5-coder:1.5b; cloud models are disabled.".into(),
        ));
    }
    Ok(())
}

/// Resolve Ollama's default host, library namespace, and `latest` tag before
/// matching local inventory. A three-part name starts with a host, not a namespace.
pub fn canonical_model_name(name: &str) -> Result<String> {
    validate_model(name)?;
    let parts = name.split('/').collect::<Vec<_>>();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(LocalError::Invalid(
            "Model name contains an empty part.".into(),
        ));
    }
    let name = match parts.as_slice() {
        [_] => name,
        [namespace, model] if namespace.eq_ignore_ascii_case("library") => *model,
        [_, _] => name,
        [host, namespace, model] if host.eq_ignore_ascii_case("registry.ollama.ai") => {
            if namespace.eq_ignore_ascii_case("library") {
                *model
            } else {
                name.split_once('/').map(|(_, rest)| rest).unwrap_or(name)
            }
        }
        [_, _, _] => name,
        _ => {
            return Err(LocalError::Invalid(
                "Use a model name with at most host/namespace/model parts.".into(),
            ));
        }
    };
    let leaf = name.rsplit('/').next().unwrap_or(name);
    if let Some((model, tag)) = leaf.split_once(':') {
        if model.is_empty() || tag.is_empty() || tag.contains(':') {
            return Err(LocalError::Invalid("Model name has an invalid tag.".into()));
        }
        return Ok(name.to_owned());
    }
    Ok(format!("{name}:latest"))
}

/// Resolve one runtime inventory row by Ollama identity. Ambiguous aliases
/// cannot safely identify a model for calibration, selection, or removal.
pub fn find_installed_model<'a>(
    models: &'a [LocalModel],
    name: &str,
) -> Result<Option<&'a LocalModel>> {
    let requested = canonical_model_name(name)?;
    let mut matches = models.iter().filter(|model| {
        canonical_model_name(&model.name)
            .is_ok_and(|candidate| candidate.eq_ignore_ascii_case(&requested))
    });
    let found = matches.next();
    if matches.next().is_some() {
        return Err(LocalError::Invalid(
            "Runtime reported ambiguous installed model aliases; refusing to target a model."
                .into(),
        ));
    }
    Ok(found)
}

impl LocalRuntime {
    /// Create a transport that cannot follow a redirect or inherit a proxy.
    pub fn new(endpoint: &str) -> Result<Self> {
        Ok(Self {
            endpoint: local_endpoint(endpoint)?,
            http: Client::builder()
                .no_proxy()
                .redirect(Policy::none())
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(600))
                .build()?,
        })
    }

    /// Normalized endpoint used by profile identity.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}{path}", self.endpoint))
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        response_json(response).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}{path}", self.endpoint))
            .json(body)
            .send()
            .await?;
        response_json(response).await
    }

    /// Runtime-reported version; absence means unavailable, never a fake version.
    pub async fn version(&self) -> Result<String> {
        let value = self.get("/api/version").await?;
        value["version"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| LocalError::Invalid("Runtime did not report its version.".into()))
    }

    /// Enumerate usable installed models and report incomplete runtime entries.
    pub async fn inventory(&self) -> Result<InstalledInventory> {
        let value = self.get("/api/tags").await?;
        parse_installed_inventory(&value)
    }

    /// Enumerate only models with the identity and size required for safe use.
    pub async fn installed(&self) -> Result<Vec<LocalModel>> {
        Ok(self.inventory().await?.models)
    }

    /// Observe an exact model already loaded with at least the requested
    /// context. A missing or mismatched observation never authorizes reuse.
    pub async fn resident(
        &self,
        name: &str,
        digest: &str,
        required_context: u32,
    ) -> Result<Option<ResidentModel>> {
        let name = canonical_model_name(name)?;
        let value = self.get("/api/ps").await?;
        resident_from_ps(&value, &name, digest, required_context)
    }

    /// Fetch metadata and reject daemon-side cloud aliases before sending code.
    pub async fn show_local(&self, name: &str) -> Result<Value> {
        let name = canonical_model_name(name)?;
        let value = self.post("/api/show", &json!({ "model": name })).await?;
        ensure_local_metadata(&value)?;
        Ok(value)
    }

    /// Load a real manifest for each catalog entry. This explicit network action
    /// sends no repository data. Failed lookups stay unavailable, never guessed.
    pub async fn catalog(hardware: &HardwareSnapshot) -> Result<Vec<CatalogModel>> {
        let http = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;
        let entries = [
            ("qwen2.5-coder", "0.5b"),
            ("qwen2.5-coder", "1.5b"),
            ("qwen2.5-coder", "3b"),
            ("qwen2.5-coder", "7b"),
            ("qwen3.5", "4b"),
        ]
        .map(|(family, tag)| {
            (
                format!("{family}:{tag}"),
                format!("https://registry.ollama.ai/v2/library/{family}/manifests/{tag}"),
            )
        });
        Ok(catalog_entries(&http, hardware, entries).await)
    }

    /// Fetch a fresh official registry manifest for the requested tag and sum
    /// unique config/layer blobs. This is an estimate, not a pinned pull: Ollama
    /// may resolve a mutable tag again after this request.
    pub async fn manifest_download_bytes(name: &str) -> Result<u64> {
        manifest_descriptor_bytes(&Self::official_manifest(name).await?)
    }

    async fn official_manifest(name: &str) -> Result<Value> {
        let url = official_manifest_url(name)?;
        let http = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;
        let response = http
            .get(url)
            .header(
                "Accept",
                "application/vnd.docker.distribution.manifest.v2+json",
            )
            .send()
            .await?;
        response_json(response).await
    }

    /// Admit a pull against the verified managed store. Only when a full-size
    /// estimate does not fit, hash completed blobs in a blocking worker and
    /// remeasure the bound store before crediting those exact bytes.
    #[cfg(all(windows, target_arch = "x86_64"))]
    pub async fn managed_download_admission(
        name: &str,
        binding: &crate::managed_store::VerifiedManagedStore,
    ) -> Result<ModelDownloadAdmission> {
        let descriptors = manifest_descriptors(&Self::official_manifest(name).await?)?;
        let manifest_bytes = descriptors.iter().map(|blob| blob.size_bytes).sum();
        let available = binding.available_bytes()?;
        let reserve = crate::disk::model_download_reserve(manifest_bytes)?;
        let full_admission = crate::disk::admit_model_download(manifest_bytes, 0, available);
        if available < reserve || full_admission.is_ok() {
            return full_admission;
        }
        let blobs = binding.verified_blobs_directory()?;
        let cancelled = CancelBlobScan(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )));
        let worker_cancelled = cancelled.0.clone();
        let credited = tokio::task::spawn_blocking(move || {
            crate::disk::verified_complete_blob_bytes(&blobs, &descriptors, &worker_cancelled)
        })
        .await
        .map_err(|error| {
            LocalError::Invalid(format!("Managed model blob scan failed: {error}"))
        })??;
        crate::disk::admit_model_download(manifest_bytes, credited, binding.available_bytes()?)
    }

    /// Stream actual layer progress and return the installed model identity.
    /// Dropping this future closes the pull stream; the runtime may retain
    /// partial layers for a subsequent resume.
    pub async fn pull(
        &self,
        name: &str,
        progress: impl FnMut(DownloadProgress),
    ) -> Result<LocalModel> {
        self.pull_checked(name, progress, || Ok(())).await
    }

    /// Stream a pull while rechecking an optional caller-owned local admission
    /// invariant before the request and at each received progress event.
    /// Success requires the terminal event and a matching installed inventory
    /// entry with a usable digest and size; progress alone is not installation.
    pub async fn pull_checked(
        &self,
        name: &str,
        mut progress: impl FnMut(DownloadProgress),
        mut check: impl FnMut() -> Result<()>,
    ) -> Result<LocalModel> {
        let name = canonical_model_name(name)?;
        check()?;
        let mut response = self
            .http
            .post(format!("{}/api/pull", self.endpoint))
            .timeout(Duration::from_secs(7200))
            .json(&json!({ "model": &name, "stream": true }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(LocalError::Invalid(format!(
                "Download refused: HTTP {}",
                response.status()
            )));
        }
        let mut buffer = Vec::new();
        let mut terminal_success = None;
        while let Some(chunk) = response.chunk().await? {
            buffer.extend_from_slice(&chunk);
            if buffer.len() > MAX_RESPONSE {
                return Err(LocalError::Invalid(
                    "Runtime download event exceeds size limit".into(),
                ));
            }
            while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = buffer.drain(..=end).collect();
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let event = parse_progress(&line)?;
                check()?;
                if event.status == "success" {
                    terminal_success = Some(event);
                } else {
                    terminal_success = None;
                    progress(event);
                }
            }
        }
        if !buffer.iter().all(u8::is_ascii_whitespace) {
            let event = parse_progress(&buffer)?;
            check()?;
            if event.status == "success" {
                terminal_success = Some(event);
            } else {
                terminal_success = None;
                progress(event);
            }
        }
        let Some(success_event) = terminal_success else {
            return Err(LocalError::Invalid(
                "Download ended before the runtime confirmed terminal success. Refresh installed models before retrying. Ollama may retain partial layers, but a managed retry can still require more free space."
                    .into(),
            ));
        };
        check()?;
        let models = self.installed().await?;
        check()?;
        let installed = find_installed_model(&models, &name)?.cloned().ok_or_else(|| {
            LocalError::Invalid(
                "Runtime reported download success, but the requested model is missing from the installed inventory with a usable digest and size. Refresh installed models before retrying."
                    .into(),
            )
        })?;
        self.show_local(&name).await?;
        check()?;
        progress(success_event);
        Ok(installed)
    }

    /// Remove one unambiguous installed model and confirm that its identity
    /// disappeared. User surfaces must request this explicitly. Ollama does not
    /// offer conditional deletion, so external changes can still race this check.
    pub async fn remove(&self, name: &str) -> Result<()> {
        let name = canonical_model_name(name)?;
        let before = self.inventory().await?;
        if !before.warnings.is_empty() {
            return Err(LocalError::Invalid(
                "Installed inventory has unusable entries; refusing removal until identity can be confirmed. Refresh model status before retrying."
                    .into(),
            ));
        }
        find_installed_model(&before.models, &name)?.ok_or_else(|| {
            LocalError::Invalid(
                "Model is not installed; refresh model status before retrying.".into(),
            )
        })?;
        let response = self
            .http
            .delete(format!("{}/api/delete", self.endpoint))
            .json(&json!({ "model": name }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(LocalError::Invalid(format!(
                "Removal refused: HTTP {}",
                response.status()
            )));
        }
        let inventory = self.inventory().await.map_err(|error| {
            LocalError::Invalid(format!(
                "Runtime acknowledged removal, but installed inventory could not be confirmed: {error}. Refresh model status before retrying."
            ))
        })?;
        if !inventory.warnings.is_empty() {
            return Err(LocalError::Invalid(
                "Runtime acknowledged removal, but its installed inventory contains unusable entries; absence cannot be confirmed. Refresh model status before retrying."
                    .into(),
            ));
        }
        if find_installed_model(&inventory.models, &name)?.is_some() {
            return Err(LocalError::Invalid(format!(
                "Runtime acknowledged removal, but {name} is still installed. Refresh model status before retrying."
            )));
        }
        Ok(())
    }

    /// Generate using an explicitly bounded context/output budget. No fallback.
    pub async fn chat(
        &self,
        name: &str,
        system: &str,
        user: &str,
        context: u32,
        output: u32,
        thinking: LocalThinkingMode,
    ) -> Result<Value> {
        self.show_local(name).await?;
        self.post(
            "/api/chat",
            &with_thinking_mode(json!({
                "model": name, "stream": false, "keep_alive": "2m",
                "messages": [{"role":"system", "content":system}, {"role":"user", "content":user}],
                "options": { "num_ctx": context, "num_predict": output, "temperature": 0 }
            }), thinking),
        )
        .await
    }

    /// Request a structured, bounded restart strategy. This returns data only;
    /// it cannot choose a file outside the caller's schema or edit the snapshot.
    pub async fn chat_hypothesis(&self, request: HypothesisRequest<'_>) -> Result<Value> {
        self.show_local(request.model).await?;
        self.require_resident(request.model, request.context, request.required_resident)
            .await?;
        self.post(
            "/api/chat",
            &with_thinking_mode(json!({
                "model": request.model, "stream": false, "keep_alive": "2m",
                "format": request.schema,
                "messages": [{"role":"system", "content":request.system}, {"role":"user", "content":request.user}],
                "options": { "num_ctx": request.context, "num_predict": request.output, "temperature": 0 }
            }), request.thinking),
        )
        .await
    }

    async fn require_resident(
        &self,
        name: &str,
        context: u32,
        required: Option<&ResidentModel>,
    ) -> Result<()> {
        if let Some(required) = required {
            let current = self
                .resident(name, &required.digest, context)
                .await
                .map_err(|error| LocalError::ResidentUnavailable(error.to_string()))?;
            if !current.is_some_and(|observed| {
                observed.name == required.name
                    && observed.size_bytes == required.size_bytes
                    && observed.size_vram_bytes == required.size_vram_bytes
            }) {
                return Err(LocalError::ResidentUnavailable(
                    "identity, allocation, context, or remaining load time changed".into(),
                ));
            }
        }
        Ok(())
    }

    /// Request an edit using the runtime's constrained JSON format when the
    /// profile selects search/replace. The exact same transport is calibrated
    /// and used for repository work; correctness still requires strict parsing.
    pub async fn chat_edit(&self, request: EditRequest<'_>) -> Result<Value> {
        let EditRequest {
            model: name,
            protocol,
            system,
            user,
            context,
            output,
            thinking,
            paths,
            searches,
            create_path,
            required_resident,
        } = request;
        if let Some(path) = create_path {
            if paths != [path] || protocol != EditProtocol::SearchReplace {
                return Err(LocalError::Invalid(
                    "Creation schema does not match the selected edit transport or exact path"
                        .into(),
                ));
            }
        }
        self.show_local(name).await?;
        self.require_resident(name, context, required_resident)
            .await?;
        if protocol == EditProtocol::UnifiedDiff {
            return self.post(
                "/api/chat",
                &with_thinking_mode(json!({
                    "model": name, "stream": false, "keep_alive": "2m",
                    "messages": [{"role":"system", "content":system}, {"role":"user", "content":user}],
                    "options": { "num_ctx": context, "num_predict": output, "temperature": 0 }
                }), thinking),
            ).await;
        }
        self.post(
            "/api/chat",
            &with_thinking_mode(json!({
                "model": name, "stream": false, "keep_alive": "2m",
                "format": create_path.map(create_schema).unwrap_or_else(|| edit_schema(paths, searches)),
                "messages":[{"role":"system","content":system},{"role":"user","content":user}],
                "options":{"num_ctx":context,"num_predict":output,"temperature":0}
            }), thinking),
        )
        .await
    }

    async fn calibration_admission(
        &self,
        model: &LocalModel,
        hardware: &HardwareSnapshot,
        model_ceiling: Option<u32>,
        requested_context: Option<u32>,
    ) -> Result<CalibrationAdmission> {
        validate_calibration_context_request(requested_context, model_ceiling)?;
        match calibration_context(model.size_bytes, hardware, model_ceiling, requested_context) {
            Ok(context_tokens) => Ok(CalibrationAdmission {
                context_tokens,
                required_resident: None,
            }),
            Err(cold_error) => {
                if hardware
                    .ram_available_bytes
                    .is_none_or(|bytes| bytes < CALIBRATION_HOST_RESERVE_BYTES)
                    || (requested_context.is_none() && model_ceiling.is_none())
                {
                    return Err(cold_error);
                }
                let resident = self
                    .resident(
                        &model.name,
                        &model.digest,
                        requested_context.unwrap_or(2048),
                    )
                    .await
                    .map_err(|error| {
                        LocalError::ResidentUnavailable(format!(
                            "Cold loading does not fit and exact resident evidence is unavailable: {error}"
                        ))
                    })?
                    .ok_or_else(|| {
                        LocalError::ResidentUnavailable(format!(
                            "Cold loading does not fit ({cold_error}); no matching resident model with sufficient context and remaining load time is available"
                        ))
                    })?;
                let context_tokens = requested_context.unwrap_or_else(|| {
                    resident
                        .context_length
                        .min(model_ceiling.unwrap_or(0))
                        .min(32768)
                });
                if context_tokens < 2048 || context_tokens > resident.context_length {
                    return Err(LocalError::ResidentUnavailable(
                        "Resident model context is too small for calibration.".into(),
                    ));
                }
                Ok(CalibrationAdmission {
                    context_tokens,
                    required_resident: Some(resident),
                })
            }
        }
    }

    async fn calibration_probe_resident<F, Fut>(
        &self,
        model: &LocalModel,
        admission: &CalibrationAdmission,
        observe_hardware: &F,
    ) -> Result<Option<ResidentModel>>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = HardwareSnapshot>,
    {
        let hardware = observe_hardware().await;
        if hardware
            .ram_available_bytes
            .is_none_or(|bytes| bytes < CALIBRATION_HOST_RESERVE_BYTES)
        {
            return Err(LocalError::Invalid(
                "Available host RAM fell below the 1.5 GiB calibration reserve; stop probes and free memory before retrying."
                    .into(),
            ));
        }
        let fit =
            crate::estimate_fit_for_context(model.size_bytes, &hardware, admission.context_tokens);
        if fit.status == FitStatus::Unknown {
            return Err(LocalError::Invalid(fit.explanation));
        }
        if fit.status != FitStatus::InsufficientMemory && admission.required_resident.is_none() {
            return Ok(None);
        }
        let observed = self
            .resident(&model.name, &model.digest, admission.context_tokens)
            .await
            .map_err(|error| LocalError::ResidentUnavailable(error.to_string()))?
            .ok_or_else(|| {
                LocalError::ResidentUnavailable(
                    "Exact resident model is no longer loaded with the calibration context.".into(),
                )
            })?;
        if admission.required_resident.as_ref().is_some_and(|prior| {
            observed.name != prior.name
                || observed.digest != prior.digest
                || observed.size_bytes != prior.size_bytes
                || observed.size_vram_bytes != prior.size_vram_bytes
        }) {
            return Err(LocalError::ResidentUnavailable(
                "Resident model identity or allocation changed during calibration.".into(),
            ));
        }
        Ok(Some(observed))
    }

    async fn calibrate_edit_probes<F, Fut, P>(
        &self,
        model: &LocalModel,
        admission: &CalibrationAdmission,
        thinking: LocalThinkingMode,
        observe_hardware: &F,
        on_probe: &mut P,
    ) -> Result<(Vec<ModelProbe>, Vec<EditProtocol>)>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = HardwareSnapshot>,
        P: FnMut(&ModelProbe, LocalThinkingMode) -> Result<()>,
    {
        let name = model.name.as_str();
        let context = admission.context_tokens;
        let mut probes = Vec::new();
        let mut passing_edit_protocols = Vec::new();
        for (kind, instruction) in [
            (EditProtocol::SearchReplace, "Return only a JSON object with keys path, search, text. path must be add.py. search must be the exact original line, text the corrected replacement line. No markdown."),
            (EditProtocol::UnifiedDiff, "Return only a unified diff for add.py with --- a/add.py, +++ b/add.py, and a @@ hunk header. No markdown or prose."),
        ] {
            let required_resident = self
                .calibration_probe_resident(model, admission, observe_hardware)
                .await?;
            let start = Instant::now();
            let result = self
                .chat_edit(EditRequest {
                    model: name,
                    protocol: kind,
                    system: instruction,
                    user: "File add.py contains exactly:\ndef add(a, b): return a - b\n\nFix add so it adds its two arguments.",
                    context,
                    output: 512,
                    thinking,
                    paths: &["add.py".into()],
                    searches: &["def add(a, b): return a - b".into()],
                    create_path: None,
                    required_resident: required_resident.as_ref(),
                })
                .await;
            let probe = match result {
                Ok(response) => {
                    let output = response["message"]["content"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    let passed = edit_probe_passed(kind, &output)
                        && response["done"].as_bool() == Some(true)
                        && response["done_reason"].as_str() != Some("length");
                    if passed {
                        passing_edit_protocols.push(kind);
                    }
                    let has_thinking = response["message"]["thinking"]
                        .as_str()
                        .is_some_and(|value| !value.is_empty());
                    let thinking_note = if has_thinking {
                        " Runtime returned separate thinking output."
                    } else {
                        ""
                    };
                    ModelProbe {
                        name: format!("{kind:?} edit"),
                        status: if passed { CheckStatus::Passed } else { CheckStatus::Failed },
                        output,
                        detail: format!("Exact one-line arithmetic fixture with {thinking:?} thinking request; no generated code executed. Does not certify general coding ability.{thinking_note}"),
                        input_tokens: response["prompt_eval_count"].as_u64(),
                        output_tokens: response["eval_count"].as_u64(),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    }
                },
                Err(error) => ModelProbe {
                    name: format!("{kind:?} edit"),
                    status: CheckStatus::Unavailable,
                    output: String::new(),
                    detail: error.to_string(),
                    input_tokens: None,
                    output_tokens: None,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                },
            };
            on_probe(&probe, thinking)?;
            probes.push(probe);
        }
        Ok((probes, passing_edit_protocols))
    }

    async fn calibrate_mode<F, Fut, P>(
        &self,
        model: &LocalModel,
        admission: &CalibrationAdmission,
        thinking: LocalThinkingMode,
        observe_hardware: &F,
        on_probe: &mut P,
    ) -> Result<ModeCalibration>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = HardwareSnapshot>,
        P: FnMut(&ModelProbe, LocalThinkingMode) -> Result<()>,
    {
        let name = model.name.as_str();
        let context = admission.context_tokens;
        let (mut probes, passing_edit_protocols) = self
            .calibrate_edit_probes(model, admission, thinking, observe_hardware, on_probe)
            .await?;
        let mut creation_protocol = None;
        for kind in passing_edit_protocols.iter().copied() {
            let (probe_name, instruction, create_path) = match kind {
                EditProtocol::SearchReplace => (
                    CREATION_PROBE_NAME,
                    "Return only a JSON object with exactly path and text. path must be probe_add.py. text must be the complete new file; no markdown or prose.",
                    Some("probe_add.py"),
                ),
                EditProtocol::UnifiedDiff => (
                    DIFF_CREATION_PROBE_NAME,
                    "Return only a unified diff for the absent probe_add.py with --- /dev/null, +++ b/probe_add.py, and exact @@ hunk counts. No markdown or prose.",
                    None,
                ),
            };
            let required_resident = self
                .calibration_probe_resident(model, admission, observe_hardware)
                .await?;
            let start = Instant::now();
            let creation_result = self.chat_edit(EditRequest {
                model: name,
                protocol: kind,
                system: instruction,
                user: "Create the absent file probe_add.py with exactly this content:\ndef add(a, b):\n    return a + b\n",
                context,
                output: 512,
                thinking,
                paths: &["probe_add.py".into()],
                searches: &[],
                create_path,
                required_resident: required_resident.as_ref(),
            }).await;
            let creation_probe = match creation_result {
                Ok(response) => {
                    let output = response["message"]["content"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned();
                    let passed = create_probe_passed(kind, &output)
                        && response["done"].as_bool() == Some(true)
                        && response["done_reason"].as_str() != Some("length");
                    ModelProbe {
                        name: probe_name.into(),
                        status: if passed { CheckStatus::Passed } else { CheckStatus::Failed },
                        output,
                        detail: "Exact new-file transport and text fixture; no generated code executed. Measures creation format, not general coding quality.".into(),
                        input_tokens: response["prompt_eval_count"].as_u64(),
                        output_tokens: response["eval_count"].as_u64(),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    }
                }
                Err(error) => ModelProbe {
                    name: probe_name.into(),
                    status: CheckStatus::Unavailable,
                    output: String::new(),
                    detail: error.to_string(),
                    input_tokens: None,
                    output_tokens: None,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                },
            };
            if creation_probe.status == CheckStatus::Passed && creation_protocol.is_none() {
                creation_protocol = Some(kind);
            }
            on_probe(&creation_probe, thinking)?;
            probes.push(creation_probe);
        }
        // A new-file goal should use a transport demonstrated for both edits
        // and creation. Existing-file goals can still use the first edit pass
        // when no creation format passed.
        Ok(ModeCalibration {
            probes,
            edit_protocol: passing_edit_protocols.first().copied(),
            creation_protocol,
        })
    }

    /// Measure edit, creation, and tool-call formats against fixed tiny fixtures;
    /// never run model output as a command. Probes are not coding benchmarks.
    pub async fn calibrate(
        &self,
        name: &str,
        requested_context: Option<u32>,
        hardware: HardwareSnapshot,
    ) -> Result<ModelProfile> {
        self.calibrate_with_progress(name, requested_context, hardware, |_| Ok(()))
            .await
    }

    /// Persist each finished probe through the caller before starting another.
    /// The callback receives only incomplete, nonselectable evidence; callers
    /// must save the final returned profile separately after calibration ends.
    pub async fn calibrate_with_progress<P>(
        &self,
        name: &str,
        requested_context: Option<u32>,
        hardware: HardwareSnapshot,
        mut on_progress: P,
    ) -> Result<ModelProfile>
    where
        P: FnMut(&CalibrationAttempt) -> Result<()>,
    {
        self.calibrate_with_observer_and_progress(
            name,
            requested_context,
            hardware,
            &|| crate::hardware::detect(),
            &mut on_progress,
        )
        .await
    }

    #[cfg(test)]
    async fn calibrate_with_observer<F, Fut>(
        &self,
        name: &str,
        requested_context: Option<u32>,
        hardware: HardwareSnapshot,
        observe_hardware: &F,
    ) -> Result<ModelProfile>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = HardwareSnapshot>,
    {
        self.calibrate_with_observer_and_progress(
            name,
            requested_context,
            hardware,
            observe_hardware,
            &mut |_| Ok(()),
        )
        .await
    }

    async fn calibrate_with_observer_and_progress<F, Fut, P>(
        &self,
        name: &str,
        requested_context: Option<u32>,
        hardware: HardwareSnapshot,
        observe_hardware: &F,
        on_progress: &mut P,
    ) -> Result<ModelProfile>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = HardwareSnapshot>,
        P: FnMut(&CalibrationAttempt) -> Result<()>,
    {
        let installed = self.installed().await?;
        let model = find_installed_model(&installed, name)?.ok_or_else(|| {
            LocalError::Invalid("Install this exact model tag before calibration.".into())
        })?;
        let name = model.name.as_str();
        let metadata = self.show_local(name).await?;
        let admission = self
            .calibration_admission(
                model,
                &hardware,
                model_context_ceiling(&metadata),
                requested_context,
            )
            .await?;
        let context = admission.context_tokens;
        let version = self.version().await?;
        let started_at_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut attempt = CalibrationAttempt {
            schema: 1,
            model: name.into(),
            starting_digest: model.digest.clone(),
            runtime_version: version.clone(),
            endpoint: self.endpoint.clone(),
            context_tokens: context,
            hardware: hardware.clone(),
            started_at_unix,
            probes: Vec::new(),
        };
        on_progress(&attempt)?;
        let mut record_probe = |probe: &ModelProbe, thinking: LocalThinkingMode| {
            let mut saved = probe.clone();
            saved.name = format!(
                "{}: {}",
                match thinking {
                    LocalThinkingMode::Off => "Think off",
                    LocalThinkingMode::RuntimeDefault => "Runtime default",
                },
                saved.name
            );
            if let Some((cut, _)) = saved
                .output
                .char_indices()
                .nth(MAX_SAVED_ATTEMPT_OUTPUT_CHARS)
            {
                saved.output.truncate(cut);
                saved
                    .output
                    .push_str("\n[output truncated in incomplete calibration evidence]");
            }
            attempt.probes.push(saved);
            on_progress(&attempt)
        };
        let mut off = self
            .calibrate_mode(
                model,
                &admission,
                LocalThinkingMode::Off,
                observe_hardware,
                &mut record_probe,
            )
            .await?;
        let (thinking, mut selected) = if off.creation_protocol.is_some() {
            (LocalThinkingMode::Off, off)
        } else {
            let mut default = self
                .calibrate_mode(
                    model,
                    &admission,
                    LocalThinkingMode::RuntimeDefault,
                    observe_hardware,
                    &mut record_probe,
                )
                .await?;
            if default.creation_protocol.is_some() || off.edit_protocol.is_none() {
                off.prefix_probes("Think off");
                off.probes.extend(default.probes);
                default.probes = off.probes;
                (LocalThinkingMode::RuntimeDefault, default)
            } else {
                default.prefix_probes("Runtime default");
                off.probes.extend(default.probes);
                (LocalThinkingMode::Off, off)
            }
        };
        // The selected mode must own the unprefixed edit/creation probes.
        let protocol = selected.selected_protocol();
        let mut probes = std::mem::take(&mut selected.probes);
        if admission.required_resident.is_some() {
            if let Some(first) = probes.first_mut() {
                first.detail.push_str(" Calibration started from an exact resident-model observation because cold loading did not fit. Residency was rechecked before each probe and was not reserved.");
            }
        }
        let required_resident = self
            .calibration_probe_resident(model, &admission, observe_hardware)
            .await?;
        self.require_resident(name, context, required_resident.as_ref())
            .await?;
        let start = Instant::now();
        let tool_result = self.post("/api/chat", &with_thinking_mode(json!({
            "model": name, "stream": false, "keep_alive": "2m",
            "messages": [{"role":"user", "content":"Call read_file with path add.py. Do not answer in prose."}],
            "tools": [{"type":"function", "function": {"name":"read_file", "description":"Read a file", "parameters":{"type":"object", "properties":{"path":{"type":"string", "enum":["add.py"]}}, "required":["path"]}}}],
            "options": {"num_ctx": context, "num_predict": 256, "temperature": 0}
        }), thinking)).await;
        let tool_probe = match tool_result {
            Ok(response) => {
                let calls = response["message"]["tool_calls"].as_array();
                let passed = calls.is_some_and(|calls| {
                    calls.len() == 1
                        && calls[0]["function"]["name"] == "read_file"
                        && calls[0]["function"]["arguments"]["path"] == "add.py"
                });
                ModelProbe {
                    name: "Tool call format".into(),
                    status: if passed {
                        CheckStatus::Passed
                    } else {
                        CheckStatus::Failed
                    },
                    output: response["message"].to_string(),
                    detail:
                        "Read-file call format only. No tool or generated command was executed."
                            .into(),
                    input_tokens: response["prompt_eval_count"].as_u64(),
                    output_tokens: response["eval_count"].as_u64(),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                }
            }
            Err(error) => ModelProbe {
                name: "Tool call format".into(),
                status: CheckStatus::Unavailable,
                output: String::new(),
                detail: error.to_string(),
                input_tokens: None,
                output_tokens: None,
                elapsed_ms: start.elapsed().as_millis() as u64,
            },
        };
        record_probe(&tool_probe, thinking)?;
        probes.push(tool_probe);
        // The installed digest must still identify the weights measured above.
        let after = self.installed().await?;
        let current = find_installed_model(&after, name)?;
        if !current.is_some_and(|current| {
            current.digest == model.digest && current.size_bytes == model.size_bytes
        }) {
            return Err(LocalError::Invalid(
                "Model changed during calibration; repeat the probes.".into(),
            ));
        }
        Ok(ModelProfile {
            schema: 2,
            model: name.into(),
            digest: model.digest.clone(),
            runtime_version: version,
            endpoint: self.endpoint.clone(),
            context_tokens: context,
            // Probes use small fixed limits; goal edits need room for a complete file.
            output_tokens: context / 4,
            protocol,
            thinking: Some(thinking),
            probes,
            hardware,
            measured_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        })
    }
}

/// Read the installed GGUF model's reported context ceiling from Ollama's
/// model metadata. Missing or malformed metadata cannot justify an auto choice.
pub fn model_context_ceiling(metadata: &Value) -> Option<u32> {
    let info = metadata.get("model_info")?.as_object()?;
    let architecture = info.get("general.architecture")?.as_str()?;
    let architecture_key = format!("{architecture}.context_length");
    let limit = info
        .get(&architecture_key)
        .or_else(|| info.get("general.context_length"))?
        .as_u64()?;
    u32::try_from(limit).ok().filter(|limit| *limit >= 2048)
}

/// Refuse a calibrated context above the installed model's current reported
/// ceiling. Missing ceiling metadata leaves an explicit older profile usable;
/// it does not authorize a new automatic calibration choice.
pub fn validate_profile_context(profile: &ModelProfile, metadata: &Value) -> Result<()> {
    if model_context_ceiling(metadata).is_some_and(|limit| profile.context_tokens > limit) {
        return Err(LocalError::Invalid(
            "Calibrated context exceeds the installed model's reported limit. Calibrate again."
                .into(),
        ));
    }
    Ok(())
}

fn validate_calibration_context_request(
    requested_context: Option<u32>,
    model_ceiling: Option<u32>,
) -> Result<()> {
    if let Some(context) = requested_context {
        if !(2048..=32768).contains(&context) {
            return Err(LocalError::Invalid(
                "Calibration context must be 2048..32768 tokens.".into(),
            ));
        }
        if model_ceiling.is_some_and(|ceiling| context > ceiling) {
            return Err(LocalError::Invalid(
                "Requested context exceeds the installed model's reported limit.".into(),
            ));
        }
    }
    Ok(())
}

fn calibration_context(
    weights_bytes: u64,
    hardware: &HardwareSnapshot,
    model_ceiling: Option<u32>,
    requested_context: Option<u32>,
) -> Result<u32> {
    validate_calibration_context_request(requested_context, model_ceiling)?;
    let context = match requested_context {
        Some(context) => context,
        None => crate::recommend_context(weights_bytes, hardware, model_ceiling).ok_or_else(
            || LocalError::Invalid("No safe automatic context is available from current memory and model metadata. Free resources or choose an explicit supported context.".into()),
        )?,
    };
    let fit = crate::estimate_fit_for_context(weights_bytes, hardware, context);
    if matches!(
        fit.status,
        FitStatus::InsufficientMemory | FitStatus::Unknown
    ) {
        return Err(LocalError::Invalid(fit.explanation));
    }
    Ok(context)
}

fn resident_from_ps(
    value: &Value,
    name: &str,
    digest: &str,
    required_context: u32,
) -> Result<Option<ResidentModel>> {
    let models = value["models"].as_array().ok_or_else(|| {
        LocalError::Invalid("Runtime did not return a running-model list.".into())
    })?;
    let requested = canonical_model_name(name)?;
    let mut matching = models.iter().filter(|model| {
        model["name"].as_str().is_some_and(|reported| {
            canonical_model_name(reported)
                .is_ok_and(|candidate| candidate.eq_ignore_ascii_case(&requested))
        })
    });
    let Some(model) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err(LocalError::Invalid(
            "Runtime reported ambiguous resident model aliases; refusing reuse.".into(),
        ));
    }
    if model["digest"].as_str() != Some(digest) {
        return Err(LocalError::Invalid(
            "Resident model digest differs from the calibrated selection.".into(),
        ));
    }
    let context_length = model["context_length"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| LocalError::Invalid("Resident context length is unavailable.".into()))?;
    if context_length < required_context {
        return Ok(None);
    }
    let size_bytes = model["size"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| LocalError::Invalid("Resident model size is unavailable.".into()))?;
    let size_vram_bytes = model["size_vram"]
        .as_u64()
        .filter(|n| *n <= size_bytes)
        .ok_or_else(|| LocalError::Invalid("Resident GPU allocation is unavailable.".into()))?;
    let expires_at = model["expires_at"]
        .as_str()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .ok_or_else(|| LocalError::Invalid("Resident unload time is unavailable.".into()))?;
    if expires_at
        <= OffsetDateTime::now_utc() + time::Duration::seconds(MIN_RESIDENT_LIFETIME_SECONDS)
    {
        return Ok(None);
    }
    Ok(Some(ResidentModel {
        name: requested,
        digest: digest.into(),
        size_bytes,
        size_vram_bytes,
        context_length,
        expires_at_unix: expires_at.unix_timestamp(),
    }))
}

/// Confirm a measured profile still identifies the installed model and runtime.
pub fn validate_profile(
    profile: &ModelProfile,
    model: &LocalModel,
    endpoint: &str,
    version: &str,
) -> Result<()> {
    if !matches!((profile.schema, profile.thinking), (1, None) | (2, Some(_)))
        || profile.digest != model.digest
        || !canonical_model_name(&profile.model)?
            .eq_ignore_ascii_case(&canonical_model_name(&model.name)?)
        || profile.endpoint != local_endpoint(endpoint)?
        || profile.runtime_version != version
    {
        return Err(LocalError::Invalid(
            "Model or runtime changed since calibration. Calibrate again before selecting it."
                .into(),
        ));
    }
    let measured_protocol = profile.protocol.is_some_and(|protocol| {
        profile.probes.iter().any(|probe| {
            probe.name == format!("{protocol:?} edit") && probe.status == CheckStatus::Passed
        })
    });
    if !measured_protocol
        || !(2048..=32768).contains(&profile.context_tokens)
        || profile.output_tokens == 0
        || profile.output_tokens > profile.context_tokens / 4
    {
        return Err(LocalError::Invalid("No edit protocol passed calibration. Inspect the probe outputs or choose another model.".into()));
    }
    Ok(())
}

fn ensure_local_metadata(value: &Value) -> Result<()> {
    if ["remote_model", "remote_host"].iter().any(|key| {
        value
            .get(key)
            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
    }) {
        return Err(LocalError::Invalid("Runtime reports a remote/cloud model. Local-only mode refuses to send repository context.".into()));
    }
    // Local GGUF models must expose model metadata; fail closed for unknown kinds.
    if !value["model_info"].is_object() || value["details"]["format"].as_str() != Some("gguf") {
        return Err(LocalError::Invalid(
            "Runtime did not establish local GGUF model metadata.".into(),
        ));
    }
    Ok(())
}

async fn response_json(mut response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        return Err(LocalError::Invalid(format!(
            "Runtime/registry returned HTTP {}",
            response.status()
        )));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
            return Err(LocalError::Invalid(
                "Runtime/registry response exceeds size limit".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    if let Some(error) = value.get("error") {
        return Err(LocalError::Invalid(format!("Runtime: {error}")));
    }
    Ok(value)
}

fn parse_progress(bytes: &[u8]) -> Result<DownloadProgress> {
    let value: Value = serde_json::from_slice(bytes)?;
    if let Some(error) = value.get("error") {
        return Err(LocalError::Invalid(format!("Download: {error}")));
    }
    let status = value["status"]
        .as_str()
        .ok_or_else(|| LocalError::Invalid("Download event has no status".into()))?;
    Ok(DownloadProgress {
        status: status.into(),
        digest: value["digest"].as_str().map(str::to_owned),
        completed: value["completed"].as_u64(),
        total: value["total"].as_u64(),
    })
}

fn edit_probe_passed(protocol: EditProtocol, output: &str) -> bool {
    match protocol {
        EditProtocol::SearchReplace => serde_json::from_str::<crate::edit::Edit>(output.trim()).ok().is_some_and(|edit|
            edit.path == "add.py" && edit.search.trim_end() == "def add(a, b): return a - b"
                && edit.replace.trim_end() == "def add(a, b): return a + b"),
        EditProtocol::UnifiedDiff => output.trim().replace("\r\n", "\n") == "--- a/add.py\n+++ b/add.py\n@@ -1 +1 @@\n-def add(a, b): return a - b\n+def add(a, b): return a + b"
            || output.trim().replace("\r\n", "\n") == "--- a/add.py\n+++ b/add.py\n@@ -1,1 +1,1 @@\n-def add(a, b): return a - b\n+def add(a, b): return a + b",
    }
}

fn create_probe_passed(protocol: EditProtocol, output: &str) -> bool {
    match protocol {
        EditProtocol::SearchReplace => {
            crate::edit::parse_creation_text(std::path::Path::new("probe_add.py"), output)
                .is_ok_and(|text| text == "def add(a, b):\n    return a + b\n")
        }
        EditProtocol::UnifiedDiff => output.trim().replace("\r\n", "\n")
            == "--- /dev/null\n+++ b/probe_add.py\n@@ -0,0 +1,2 @@\n+def add(a, b):\n+    return a + b",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_hardware(
        hardware: HardwareSnapshot,
    ) -> impl Fn() -> std::future::Ready<HardwareSnapshot> {
        move || std::future::ready(hardware.clone())
    }

    async fn resident_calibration_fixture() -> (
        LocalRuntime,
        std::sync::Arc<std::sync::atomic::AtomicU8>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::{
            atomic::{AtomicU8, AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let runtime =
            LocalRuntime::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let mode = Arc::new(AtomicU8::new(0));
        let chats = Arc::new(AtomicUsize::new(0));
        let server_mode = Arc::clone(&mode);
        let server_chats = Arc::clone(&chats);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                let headers_end = loop {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0 && request.len() < 64 * 1024);
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let path = headers.split_whitespace().nth(1).unwrap_or("").to_owned();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while request.len() < headers_end + length {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                }
                let current_mode = server_mode.load(Ordering::SeqCst);
                let body = match path.as_str() {
                    "/api/tags" => {
                        let after_probe = server_chats.load(Ordering::SeqCst) > 0;
                        match (current_mode, after_probe) {
                            (7, true) => {
                                json!({"models":[{"name":"library/fixture:latest","digest":"digest-a","size":1073741824}]})
                            }
                            (8, true) => {
                                json!({"models":[{"name":"library/fixture:latest","digest":"digest-b","size":1073741824}]})
                            }
                            (9, true) => json!({"models":[
                                {"name":"fixture:latest","digest":"digest-a","size":1073741824},
                                {"name":"library/fixture:latest","digest":"digest-a","size":1073741824}
                            ]}),
                            _ => {
                                json!({"models":[{"name":"fixture:latest","digest":"digest-a","size":1073741824}]})
                            }
                        }
                    }
                    "/api/show" if current_mode == 6 => json!({"details":{"format":"gguf"}}),
                    "/api/show" => {
                        json!({"model_info":{"general.architecture":"qwen2","qwen2.context_length":8192},"details":{"format":"gguf"}})
                    }
                    "/api/version" => json!({"version":"0.11.0"}),
                    "/api/ps" => {
                        let expires = (OffsetDateTime::now_utc()
                            + time::Duration::seconds(if current_mode == 3 { 5 } else { 120 }))
                        .format(&Rfc3339)
                        .unwrap();
                        if current_mode == 4 && server_chats.load(Ordering::SeqCst) > 0 {
                            json!({"models":[]})
                        } else {
                            json!({"models":[{"name":"fixture:latest","digest":if current_mode == 1 {"digest-b"} else {"digest-a"},"size":1073741824,"size_vram":if current_mode == 5 && server_chats.load(Ordering::SeqCst) > 0 {1024} else {0},"context_length":if current_mode == 2 {2048} else {4096},"expires_at":expires}]})
                        }
                    }
                    "/api/chat" => {
                        server_chats.fetch_add(1, Ordering::SeqCst);
                        json!({"message":{"content":""},"done":true,"prompt_eval_count":1,"eval_count":1})
                    }
                    _ => json!({"error":"unexpected fixture path"}),
                };
                let body = body.to_string();
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (runtime, mode, chats, server)
    }

    #[test]
    fn canonical_names_preserve_host_identity_and_resolve_default_aliases() {
        for (input, expected) in [
            ("fixture", "fixture:latest"),
            ("LIBRARY/fixture", "fixture:latest"),
            ("registry.ollama.ai/LIBRARY/fixture", "fixture:latest"),
            ("REGISTRY.OLLAMA.AI/team/fixture", "team/fixture:latest"),
            ("library/team/fixture", "library/team/fixture:latest"),
            ("hf.co/team/fixture:Q4_K_M", "hf.co/team/fixture:Q4_K_M"),
        ] {
            assert_eq!(canonical_model_name(input).unwrap(), expected);
        }
        assert!(canonical_model_name("a/b/c/d").is_err());
    }

    #[test]
    fn ambiguous_installed_aliases_are_not_selected_by_row_order() {
        let model = |name: &str, digest: &str| LocalModel {
            name: name.into(),
            digest: digest.into(),
            size_bytes: 1024,
            parameter_size: None,
            quantization: None,
        };
        let models = vec![
            model("fixture:latest", "sha256:first"),
            model("LIBRARY/FIXTURE:latest", "sha256:second"),
        ];
        assert!(find_installed_model(&models, "fixture")
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        assert_eq!(
            find_installed_model(&models[..1], "registry.ollama.ai/library/fixture")
                .unwrap()
                .unwrap()
                .digest,
            "sha256:first"
        );
        assert!(find_installed_model(&models, "hf.co/team/fixture")
            .unwrap()
            .is_none());
    }

    #[test]
    fn manifest_estimate_includes_config_and_unique_layers() {
        let first = format!("sha256:{}", "a".repeat(64));
        let second = format!("sha256:{}", "b".repeat(64));
        let manifest = json!({"config":{"digest":first,"size":10},"layers":[
            {"digest":first,"size":10}, {"digest":second,"size":25}
        ]});
        assert_eq!(manifest_descriptor_bytes(&manifest).unwrap(), 35);
        let mut conflict = manifest.clone();
        conflict["layers"][0]["size"] = json!(11);
        assert!(manifest_descriptor_bytes(&conflict).is_err());
        let mut overflow = manifest.clone();
        overflow["layers"][1]["size"] = json!(u64::MAX);
        assert!(manifest_descriptor_bytes(&overflow).is_err());
        let mut missing = manifest.clone();
        missing["config"]["digest"] = json!("sha256:short");
        assert!(manifest_descriptor_bytes(&missing).is_err());
    }

    #[test]
    fn disk_preflight_uses_only_the_official_registry() {
        assert_eq!(
            official_manifest_url("qwen3.5:4b").unwrap(),
            "https://registry.ollama.ai/v2/library/qwen3.5/manifests/4b"
        );
        assert_eq!(
            official_manifest_url("team/coder:latest").unwrap(),
            "https://registry.ollama.ai/v2/team/coder/manifests/latest"
        );
        for name in [
            "team/../coder:tag",
            "team/sub/coder:tag",
            "repo:tag:other",
            "/repo:tag",
        ] {
            assert!(official_manifest_url(name).is_err(), "accepted {name}");
        }
    }

    #[tokio::test]
    async fn checked_pull_refuses_before_contacting_runtime() {
        let runtime = LocalRuntime::new("http://127.0.0.1:9").unwrap();
        let error = runtime
            .pull_checked(
                "qwen3.5:4b",
                |_| {},
                || Err(LocalError::Invalid("store identity changed".into())),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("store identity changed"));
    }

    #[tokio::test]
    async fn calibration_selects_measured_thinking_mode_and_creation_transport() {
        use std::sync::{
            atomic::{AtomicU8, Ordering},
            Arc, Mutex,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_seen = Arc::clone(&seen);
        let thinking_requests = Arc::new(Mutex::new(Vec::<bool>::new()));
        let server_thinking_requests = Arc::clone(&thinking_requests);
        let thinking_case = Arc::new(AtomicU8::new(0));
        let server_thinking_case = Arc::clone(&thinking_case);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 8192];
                let headers_end = loop {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "Fixture request ended before headers");
                    request.extend_from_slice(&chunk[..read]);
                    assert!(request.len() < 64 * 1024);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let path = headers.split_whitespace().nth(1).unwrap_or("").to_owned();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while request.len() < headers_end + length {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "Fixture request ended before body");
                    request.extend_from_slice(&chunk[..read]);
                }
                let input: Value = if length == 0 {
                    json!({})
                } else {
                    serde_json::from_slice(&request[headers_end..headers_end + length]).unwrap()
                };
                let body = match path.as_str() {
                    "/api/tags" => {
                        json!({"models":[
                            {"name":"fixture:small","digest":"sha256:fixture","size":1048576},
                            {"name":"fixture:latest","digest":"sha256:latest","size":1048576}
                        ]})
                    }
                    "/api/show" => {
                        json!({"model_info":{"general.architecture":"qwen2","qwen2.context_length":8192},"details":{"format":"gguf"}})
                    }
                    "/api/version" => json!({"version":"0.11.0"}),
                    "/api/chat" => {
                        let thinking_disabled = input["think"].as_bool() == Some(false);
                        server_thinking_requests
                            .lock()
                            .unwrap()
                            .push(thinking_disabled);
                        let instruction = input["messages"][0]["content"].as_str().unwrap_or("");
                        let (kind, output) = if instruction.contains("exactly path and text") {
                            (
                                "structured creation",
                                r#"{"path":"probe_add.py","text":"not the fixture\n"}"#,
                            )
                        } else if instruction.contains("absent probe_add.py") {
                            ("diff creation", "--- /dev/null\n+++ b/probe_add.py\n@@ -0,0 +1,2 @@\n+def add(a, b):\n+    return a + b\n")
                        } else if instruction.contains("JSON object with keys path") {
                            (
                                "structured edit",
                                r#"{"path":"add.py","search":"def add(a, b): return a - b","text":"def add(a, b): return a + b"}"#,
                            )
                        } else if instruction.contains("unified diff for add.py") {
                            ("diff edit", "--- a/add.py\n+++ b/add.py\n@@ -1 +1 @@\n-def add(a, b): return a - b\n+def add(a, b): return a + b\n")
                        } else {
                            ("tool call", "")
                        };
                        server_seen.lock().unwrap().push(kind.into());
                        let case = server_thinking_case.load(Ordering::Relaxed);
                        if thinking_disabled && case == 1 {
                            json!({"error":"think=false unsupported"})
                        } else if !thinking_disabled && case == 0 {
                            json!({"message":{"thinking":"reasoning used the output limit","content":""},"done":true,"done_reason":"length","eval_count":512})
                        } else if thinking_disabled && case == 2 && kind.contains("creation") {
                            json!({"message":{"content":"creation did not match"},"done":true})
                        } else if kind == "tool call" {
                            json!({"message":{"content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"add.py"}}}]},"done":true})
                        } else {
                            json!({"message":{"content":output},"done":true,"prompt_eval_count":12,"eval_count":20})
                        }
                    }
                    _ => json!({"error":"Unexpected fixture endpoint"}),
                };
                let status = if body.get("error").is_some() {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                };
                let body = body.to_string();
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let hardware = HardwareSnapshot {
            ram_total_bytes: Some(16u64 << 30),
            ram_available_bytes: Some(8u64 << 30),
            ..Default::default()
        };
        let observe_hardware = fixed_hardware(hardware.clone());
        let profile = tokio::time::timeout(
            Duration::from_secs(10),
            LocalRuntime::new(&endpoint)
                .unwrap()
                .calibrate_with_observer("fixture", Some(4096), hardware, &observe_hardware),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(profile.model, "fixture:latest");
        let schema = json!({"type":"object"});
        LocalRuntime::new(&endpoint)
            .unwrap()
            .chat_hypothesis(HypothesisRequest {
                model: "fixture:small",
                system: "Choose an approach",
                user: "Fix add.py",
                schema: &schema,
                context: profile.context_tokens,
                output: 128,
                thinking: profile.thinking.unwrap_or_default(),
                required_resident: None,
            })
            .await
            .unwrap();
        assert_eq!(profile.protocol, Some(EditProtocol::UnifiedDiff));
        assert_eq!(profile.schema, 2);
        assert_eq!(profile.thinking, Some(LocalThinkingMode::Off));
        assert_eq!(profile.output_tokens, 1024);
        assert_eq!(profile.creation_status(), CheckStatus::Passed);
        assert_eq!(
            profile
                .probes
                .iter()
                .find(|probe| probe.name == CREATION_PROBE_NAME)
                .unwrap()
                .status,
            CheckStatus::Failed
        );
        assert_eq!(
            profile
                .probes
                .iter()
                .find(|probe| probe.name == DIFF_CREATION_PROBE_NAME)
                .unwrap()
                .status,
            CheckStatus::Passed
        );
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [
                "structured edit",
                "diff edit",
                "structured creation",
                "diff creation",
                "tool call",
                "tool call"
            ]
        );
        assert!(thinking_requests
            .lock()
            .unwrap()
            .iter()
            .all(|disabled| *disabled));

        thinking_case.store(1, Ordering::Relaxed);
        let default_profile = tokio::time::timeout(
            Duration::from_secs(10),
            LocalRuntime::new(&endpoint)
                .unwrap()
                .calibrate_with_observer(
                    "fixture:small",
                    Some(4096),
                    HardwareSnapshot {
                        ram_total_bytes: Some(16u64 << 30),
                        ram_available_bytes: Some(8u64 << 30),
                        ..Default::default()
                    },
                    &observe_hardware,
                ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(default_profile.schema, 2);
        assert_eq!(
            default_profile.thinking,
            Some(LocalThinkingMode::RuntimeDefault)
        );
        assert_eq!(default_profile.protocol, Some(EditProtocol::UnifiedDiff));
        assert_eq!(default_profile.creation_status(), CheckStatus::Passed);
        assert_eq!(
            default_profile.probes[0].name,
            "Think off: SearchReplace edit"
        );
        assert_eq!(default_profile.probes[0].status, CheckStatus::Unavailable);
        assert_eq!(
            thinking_requests.lock().unwrap().as_slice(),
            [true, true, true, true, true, true, true, true, false, false, false, false, false],
        );

        thinking_case.store(2, Ordering::Relaxed);
        let new_file_profile = tokio::time::timeout(
            Duration::from_secs(10),
            LocalRuntime::new(&endpoint)
                .unwrap()
                .calibrate_with_observer(
                    "fixture:small",
                    Some(4096),
                    HardwareSnapshot {
                        ram_total_bytes: Some(16u64 << 30),
                        ram_available_bytes: Some(8u64 << 30),
                        ..Default::default()
                    },
                    &observe_hardware,
                ),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        assert_eq!(
            new_file_profile.thinking,
            Some(LocalThinkingMode::RuntimeDefault)
        );
        assert_eq!(new_file_profile.protocol, Some(EditProtocol::UnifiedDiff));
        assert_eq!(new_file_profile.creation_status(), CheckStatus::Passed);
        assert!(new_file_profile
            .probes
            .iter()
            .any(|probe| probe.name == "Think off: UnifiedDiff create"
                && probe.status == CheckStatus::Failed));
        assert_eq!(
            &thinking_requests.lock().unwrap()[13..],
            [true, true, true, true, false, false, false, false, false],
        );
    }

    #[tokio::test]
    async fn calibration_rechecks_inventory_alias_at_probe_completion() {
        use std::sync::atomic::Ordering;

        let (runtime, mode, chats, server) = resident_calibration_fixture().await;
        let hardware = HardwareSnapshot {
            ram_total_bytes: Some(16u64 << 30),
            ram_available_bytes: Some(1800 * 1024 * 1024),
            ..Default::default()
        };
        let observe = fixed_hardware(hardware.clone());
        mode.store(7, Ordering::SeqCst);
        let profile = runtime
            .calibrate_with_observer("fixture", Some(4096), hardware.clone(), &observe)
            .await
            .unwrap();
        assert_eq!(profile.model, "fixture:latest");
        assert_eq!(profile.digest, "digest-a");
        assert!(chats.load(Ordering::SeqCst) > 0);

        for case in [8, 9] {
            chats.store(0, Ordering::SeqCst);
            mode.store(case, Ordering::SeqCst);
            let error = runtime
                .calibrate_with_observer("fixture", Some(4096), hardware.clone(), &observe)
                .await
                .unwrap_err();
            assert!(
                chats.load(Ordering::SeqCst) > 0,
                "case {case} failed before probing"
            );
            assert!(
                error.to_string().contains(if case == 8 {
                    "Model changed"
                } else {
                    "ambiguous"
                }),
                "case {case}: {error}"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn resident_calibration_retry_rechecks_identity_context_and_memory() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let (runtime, mode, chats, server) = resident_calibration_fixture().await;
        let low_memory = HardwareSnapshot {
            ram_total_bytes: Some(16u64 << 30),
            ram_available_bytes: Some(1800 * 1024 * 1024),
            ..Default::default()
        };
        assert!(calibration_context(1 << 30, &low_memory, Some(8192), Some(4096)).is_err());
        let observe = fixed_hardware(low_memory.clone());
        let profile = tokio::time::timeout(
            Duration::from_secs(10),
            runtime.calibrate_with_observer("fixture", Some(4096), low_memory.clone(), &observe),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(profile.context_tokens, 4096);
        assert!(profile.probes[0].detail.contains("exact resident-model"));
        assert_eq!(chats.load(Ordering::SeqCst), 5);

        chats.store(0, Ordering::SeqCst);
        let automatic = runtime
            .calibrate_with_observer("fixture", None, low_memory.clone(), &observe)
            .await
            .unwrap();
        assert_eq!(automatic.context_tokens, 4096);
        assert_eq!(chats.load(Ordering::SeqCst), 5);

        for case in [1, 2, 3, 6] {
            mode.store(case, Ordering::SeqCst);
            chats.store(0, Ordering::SeqCst);
            let error = runtime
                .calibrate_with_observer(
                    "fixture",
                    if case == 6 { None } else { Some(4096) },
                    low_memory.clone(),
                    &observe,
                )
                .await
                .unwrap_err();
            assert_eq!(chats.load(Ordering::SeqCst), 0, "case {case}: {error}");
        }

        mode.store(0, Ordering::SeqCst);
        chats.store(0, Ordering::SeqCst);
        let too_little_ram = HardwareSnapshot {
            ram_available_bytes: Some(1400 * 1024 * 1024),
            ..low_memory.clone()
        };
        assert!(runtime
            .calibrate_with_observer("fixture", Some(4096), too_little_ram, &observe,)
            .await
            .is_err());
        assert_eq!(chats.load(Ordering::SeqCst), 0);

        for case in [4, 5] {
            mode.store(case, Ordering::SeqCst);
            chats.store(0, Ordering::SeqCst);
            let error = runtime
                .calibrate_with_observer("fixture", Some(4096), low_memory.clone(), &observe)
                .await
                .unwrap_err();
            assert_eq!(chats.load(Ordering::SeqCst), 1, "case {case}: {error}");
        }

        mode.store(0, Ordering::SeqCst);
        chats.store(0, Ordering::SeqCst);
        let readings = Arc::new(AtomicUsize::new(0));
        let observe_drop = {
            let readings = Arc::clone(&readings);
            let baseline = low_memory.clone();
            move || {
                let mut hardware = baseline.clone();
                if readings.fetch_add(1, Ordering::SeqCst) > 0 {
                    hardware.ram_available_bytes = Some(1400 * 1024 * 1024);
                }
                std::future::ready(hardware)
            }
        };
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("local-models.json");
        let mut settings = LocalSettings::default();
        let mut saved_probe_counts = Vec::new();
        let error = runtime
            .calibrate_with_observer_and_progress(
                "fixture",
                Some(4096),
                low_memory,
                &observe_drop,
                &mut |attempt| {
                    settings.schema = 4;
                    settings.calibration_attempt = Some(attempt.clone());
                    crate::storage::save(&state_path, &settings)?;
                    saved_probe_counts.push(attempt.probes.len());
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("1.5 GiB"));
        assert_eq!(chats.load(Ordering::SeqCst), 1);
        assert_eq!(saved_probe_counts, [0, 1]);
        server.abort();
        drop(runtime);
        let reopened = crate::storage::load(&state_path).unwrap();
        let attempt = reopened.calibration_attempt.unwrap();
        assert_eq!(attempt.probes.len(), 1);
        assert_eq!(attempt.probes[0].name, "Think off: SearchReplace edit");
        assert_eq!(attempt.probes[0].status, CheckStatus::Failed);
        assert_eq!(attempt.probes[0].input_tokens, Some(1));
        assert_eq!(attempt.starting_digest, "digest-a");
        assert!(reopened.profiles.is_empty());
    }

    fn catalog_fixture(name: &str, bytes: Option<u64>, status: FitStatus) -> CatalogModel {
        CatalogModel {
            name: name.into(),
            source: format!("https://registry.ollama.ai/{name}"),
            download_bytes: bytes,
            fit: bytes.map(|n| ModelFit {
                status,
                estimated_required_bytes: n,
                context_tokens: 4096,
                suggested_context: None,
                explanation: String::new(),
            }),
            error: None,
            first_try_reason: None,
            pre_setup_storage: None,
        }
    }

    #[test]
    fn first_try_prefers_largest_manifest_backed_gpu_fit() {
        let mut models = vec![
            catalog_fixture("cpu:large", Some(8), FitStatus::CpuOrOffload),
            catalog_fixture("gpu:small", Some(2), FitStatus::LikelyFitsGpu),
            catalog_fixture("gpu:large", Some(4), FitStatus::LikelyFitsGpu),
            catalog_fixture("memory:large", Some(12), FitStatus::InsufficientMemory),
            catalog_fixture("unknown:large", Some(16), FitStatus::Unknown),
        ];
        mark_first_try(&mut models);
        assert_eq!(
            models
                .iter()
                .filter(|m| m.first_try_reason.is_some())
                .count(),
            1
        );
        assert!(models[2]
            .first_try_reason
            .as_deref()
            .unwrap()
            .contains("4K"));
        assert!(models[2]
            .first_try_reason
            .as_deref()
            .unwrap()
            .contains("memory-based"));
        assert!(models[0].first_try_reason.is_none());
    }

    #[test]
    fn first_try_cpu_fallback_chooses_smallest_and_ties_by_name() {
        let mut models = vec![
            catalog_fixture("z:small", Some(2), FitStatus::CpuOrOffload),
            catalog_fixture("large", Some(8), FitStatus::CpuOrOffload),
            catalog_fixture("a:small", Some(2), FitStatus::CpuOrOffload),
        ];
        mark_first_try(&mut models);
        assert!(models[2]
            .first_try_reason
            .as_deref()
            .unwrap()
            .contains("CPU/offload"));
        assert!(models[0].first_try_reason.is_none());
        assert!(models[1].first_try_reason.is_none());
    }

    #[test]
    fn first_try_withholds_on_unknown_memory_and_failed_manifest() {
        let mut models = vec![
            catalog_fixture("memory", Some(2), FitStatus::InsufficientMemory),
            catalog_fixture("unknown", Some(3), FitStatus::Unknown),
            CatalogModel {
                error: Some("manifest unavailable".into()),
                ..catalog_fixture("failed", Some(4), FitStatus::LikelyFitsGpu)
            },
        ];
        mark_first_try(&mut models);
        assert!(models.iter().all(|m| m.first_try_reason.is_none()));
        assert!(serde_json::to_value(&models)
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m.get("first_try_reason").is_none()));
        let old_entry = serde_json::json!({"name":"old","source":"registry","download_bytes":null,"fit":null,"error":null});
        let decoded: CatalogModel = serde_json::from_value(old_entry).unwrap();
        assert!(decoded.first_try_reason.is_none());
    }

    #[tokio::test]
    async fn catalog_fetches_manifests_together_and_keeps_order_and_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            // Hold every response until all five requests arrive. A serial
            // catalog cannot finish this fixture before the test timeout.
            let mut requests = Vec::new();
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 2048];
                let mut request = Vec::new();
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).await.unwrap();
                    assert!(read > 0, "Registry request ended before its headers");
                    request.extend_from_slice(&buffer[..read]);
                    assert!(
                        request.len() <= 8192,
                        "Registry request headers grew too large"
                    );
                }
                let path = String::from_utf8_lossy(&request)
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_owned();
                requests.push((stream, path));
            }
            requests.sort_by_key(|(_, path)| {
                std::cmp::Reverse(path.rsplit('/').next().unwrap().parse::<u64>().unwrap())
            });
            let mut response_order = Vec::new();
            for (mut stream, path) in requests {
                let index = path.rsplit('/').next().unwrap().parse::<u64>().unwrap();
                response_order.push(index);
                let (status, body) = if index == 0 {
                    ("503 Service Unavailable", "{}".to_owned())
                } else {
                    (
                        "200 OK",
                        json!({"config":{"digest":format!("sha256:{}", "a".repeat(64)),"size":10},"layers":[{"digest":format!("sha256:{}", "b".repeat(64)),"size":index * 100_000_000}]}).to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            response_order
        });
        let entries = std::array::from_fn(|index| {
            (
                format!("model-{index}"),
                format!("{endpoint}/manifests/{index}"),
            )
        });
        let hardware = HardwareSnapshot {
            ram_total_bytes: Some(16u64 << 30),
            ram_available_bytes: Some(8u64 << 30),
            gpus: vec![GpuSnapshot {
                name: "fixture GPU".into(),
                total_bytes: 8u64 << 30,
                available_bytes: 6u64 << 30,
            }],
            ..Default::default()
        };
        let http = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let models = tokio::time::timeout(
            Duration::from_secs(5),
            catalog_entries(&http, &hardware, entries),
        )
        .await
        .expect("Catalog waited for one manifest before requesting the others");
        assert_eq!(server.await.unwrap(), [4, 3, 2, 1, 0]);
        assert_eq!(
            models
                .iter()
                .map(|model| model.name.as_str())
                .collect::<Vec<_>>(),
            ["model-0", "model-1", "model-2", "model-3", "model-4"]
        );
        assert!(models[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("503")));
        assert!(models[0].download_bytes.is_none());
        assert!(models[0].first_try_reason.is_none());
        for (index, model) in models.iter().enumerate().skip(1) {
            assert_eq!(model.download_bytes, Some(index as u64 * 100_000_000 + 10));
            assert!(model.error.is_none());
        }
        assert!(models[4].first_try_reason.is_some());
    }

    #[test]
    fn model_context_limit_requires_reported_architecture_metadata() {
        assert_eq!(
            model_context_ceiling(
                &json!({"model_info":{"general.architecture":"qwen2","qwen2.context_length":32768}})
            ),
            Some(32768)
        );
        assert_eq!(
            model_context_ceiling(
                &json!({"model_info":{"general.architecture":"qwen2","general.context_length":4096}})
            ),
            Some(4096)
        );
        assert_eq!(model_context_ceiling(&json!({"model_info":{}})), None);
        assert_eq!(
            model_context_ceiling(
                &json!({"model_info":{"general.architecture":"qwen2","qwen2.context_length":"32768"}})
            ),
            None
        );
    }

    #[test]
    fn automatic_and_explicit_calibration_contexts_keep_admission_separate() {
        let hardware = HardwareSnapshot {
            ram_available_bytes: Some(3 << 30),
            gpus: vec![GpuSnapshot {
                name: "GPU".into(),
                total_bytes: 3 << 30,
                available_bytes: 3 << 30,
            }],
            ..Default::default()
        };
        assert_eq!(
            calibration_context(1 << 30, &hardware, Some(32768), None).unwrap(),
            4096
        );
        assert_eq!(
            calibration_context(1 << 30, &hardware, Some(32768), Some(2048)).unwrap(),
            2048
        );
        assert!(calibration_context(1 << 30, &hardware, None, None).is_err());
        assert!(
            calibration_context(1 << 30, &hardware, Some(4096), Some(8192))
                .unwrap_err()
                .to_string()
                .contains("reported limit")
        );
        assert!(
            calibration_context(1 << 30, &hardware, Some(32768), Some(8192))
                .unwrap_err()
                .to_string()
                .contains("exceeds available memory")
        );
        assert!(calibration_context(
            1 << 30,
            &HardwareSnapshot::default(),
            Some(32768),
            Some(4096)
        )
        .is_err());
    }

    async fn serve_once(status: &str, body: &str, extra: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            let _ = stream.read(&mut request).await.unwrap();
            // Deliberately split JSON records across transport writes.
            for chunk in response.as_bytes().chunks(7) {
                stream.write_all(chunk).await.unwrap();
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn transport_refuses_redirects() {
        let endpoint = serve_once("302 Found", "", "Location: https://example.com\r\n").await;
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .version()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("302"));
    }

    #[tokio::test]
    async fn incomplete_installed_tag_does_not_hide_valid_model() {
        let body = json!({"models": [
            {"name": "qwen2.5-coder:1.5b", "digest": "sha256:valid", "size": 1_000_000},
            {"name": "unrelated:cloud", "size": 2_000_000}
        ]})
        .to_string();
        let endpoint = serve_once("200 OK", &body, "Content-Type: application/json\r\n").await;
        let models = LocalRuntime::new(&endpoint)
            .unwrap()
            .installed()
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "qwen2.5-coder:1.5b");
        assert_eq!(models[0].digest, "sha256:valid");
    }

    #[test]
    fn incomplete_inventory_entries_remain_diagnostic() {
        let inventory = parse_installed_inventory(&json!({"models": [
            {"name": "valid:1b", "digest": "sha256:valid", "size": 1_000_000},
            {"name": "missing-digest:1b", "size": 2_000_000},
            {"name": "missing-size:1b", "digest": "sha256:other"},
            null,
            {"name": "vendor/model:cloud", "digest": "sha256:cloud", "size": 3_000_000},
            {"name": "zero:1b", "digest": "sha256:zero", "size": 0}
        ]}))
        .unwrap();
        assert_eq!(inventory.models.len(), 1);
        assert_eq!(inventory.warnings.len(), 5);
        assert!(inventory.warnings[0].contains("entry 2"));
        assert!(inventory.warnings[0].contains("digest missing"));
        assert!(inventory.warnings[1].contains("entry 3"));
        assert!(inventory.warnings[1].contains("size missing"));
        assert!(inventory.warnings[2].contains("entry 4"));
        assert!(inventory.warnings[3].contains("entry 5"));
        assert!(inventory.warnings[3].contains("cloud models are disabled"));
        assert!(inventory.warnings[4].contains("entry 6"));
        assert!(inventory.warnings[4].contains("size is zero"));
        assert!(parse_installed_inventory(&json!({"models": {}})).is_err());

        let many = parse_installed_inventory(&json!({"models": vec![json!({}); 10]})).unwrap();
        assert!(many.models.is_empty());
        assert_eq!(many.warnings.len(), MAX_INVENTORY_WARNINGS + 1);
        assert!(many.warnings.last().unwrap().contains("2 more"));
    }

    #[tokio::test]
    async fn expiring_resident_stops_before_chat_request() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let server_seen = Arc::clone(&seen);
        let expires_at = (OffsetDateTime::now_utc() + time::Duration::seconds(5))
            .format(&Rfc3339)
            .unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let Ok(Ok((mut stream, _))) =
                    tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
                else {
                    break;
                };
                let mut request = [0; 8192];
                let size = stream.read(&mut request).await.unwrap();
                let line = String::from_utf8_lossy(&request[..size]);
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                server_seen.lock().unwrap().push(path.clone());
                let body = match path.as_str() {
                    "/api/show" => json!({"model_info":{},"details":{"format":"gguf"}}),
                    "/api/ps" => {
                        json!({"models":[{"name":"coder:small","digest":"digest-a","size":1200,"size_vram":1000,"context_length":4096,"expires_at":expires_at}]})
                    }
                    _ => json!({"done":true}),
                };
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let required = ResidentModel {
            name: "coder:small".into(),
            digest: "digest-a".into(),
            size_bytes: 1200,
            size_vram_bytes: 1000,
            context_length: 4096,
            expires_at_unix: 4_102_444_800,
        };
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .chat_edit(EditRequest {
                model: "coder:small",
                protocol: EditProtocol::UnifiedDiff,
                system: "Edit the file",
                user: "Fix add.py",
                context: 4096,
                output: 128,
                thinking: LocalThinkingMode::RuntimeDefault,
                paths: &[],
                searches: &[],
                create_path: None,
                required_resident: Some(&required),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, LocalError::ResidentUnavailable(_)));
        server.await.unwrap();
        assert_eq!(seen.lock().unwrap().as_slice(), ["/api/show", "/api/ps"]);
    }

    #[tokio::test]
    async fn incomplete_pull_preserves_progress_but_never_succeeds() {
        let endpoint = serve_once(
            "200 OK",
            "{\"status\":\"pulling\",\"total\":100,\"completed\":23}\n",
            "",
        )
        .await;
        let mut events = Vec::new();
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .pull("qwen2.5-coder:0.5b", |event| events.push(event))
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("before the runtime confirmed terminal success"));
        assert!(error.to_string().contains("Refresh installed models"));
        assert!(error
            .to_string()
            .contains("managed retry can still require more free space"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].completed, Some(23));
    }

    async fn serve_pull_inventory_fixture(
        pull_body: &'static str,
        tags_body: &'static str,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 8192];
                let read = stream.read(&mut request).await.unwrap();
                let path = String::from_utf8_lossy(&request[..read]);
                let body = if path.starts_with("POST /api/pull ") {
                    pull_body
                } else if path.starts_with("GET /api/tags ") {
                    tags_body
                } else if path.starts_with("POST /api/show ") {
                    r#"{"model_info":{"general.architecture":"qwen2"},"details":{"format":"gguf"}}"#
                } else {
                    panic!("unexpected pull fixture request: {path}");
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        endpoint
    }

    #[tokio::test]
    async fn pull_requires_terminal_success_and_installed_inventory_identity() {
        let endpoint = serve_pull_inventory_fixture(
            "{\"status\":\"success\"}\n{\"status\":\"pulling\"}\n",
            r#"{"models":[{"name":"qwen2.5-coder:0.5b","digest":"sha256:old","size":1048576}]}"#,
        )
        .await;
        let mut events = Vec::new();
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .pull("qwen2.5-coder:0.5b", |event| events.push(event.status))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("terminal success"), "{error}");
        assert_eq!(events, ["pulling"]);

        let endpoint =
            serve_pull_inventory_fixture("{\"status\":\"success\"}\n", r#"{"models":[]}"#).await;
        let mut events = Vec::new();
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .pull("qwen2.5-coder:0.5b", |event| events.push(event.status))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("installed inventory"), "{error}");
        assert!(events.is_empty());

        let endpoint = serve_pull_inventory_fixture(
            "{\"status\":\"pulling\"}\n{\"status\":\"success\"}\n",
            r#"{"models":[{"name":"qwen2.5-coder:0.5b","digest":"sha256:observed","size":1048576}]}"#,
        )
        .await;
        let mut events = Vec::new();
        let installed = LocalRuntime::new(&endpoint)
            .unwrap()
            .pull("qwen2.5-coder:0.5b", |event| events.push(event.status))
            .await
            .unwrap();
        assert_eq!(installed.name, "qwen2.5-coder:0.5b");
        assert_eq!(installed.digest, "sha256:observed");
        assert_eq!(events, ["pulling", "success"]);
    }

    async fn serve_remove_inventory_fixture(
        before_tags: &'static str,
        after_tags: Option<&'static str>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut paths = Vec::new();
            let request_count = if after_tags.is_some() { 3 } else { 1 };
            let mut tag_reads = 0;
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 8192];
                let read = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                let body = match path.as_str() {
                    "/api/delete" => r#"{"deleted":true}"#,
                    "/api/tags" => {
                        tag_reads += 1;
                        if tag_reads == 1 {
                            before_tags
                        } else {
                            after_tags.unwrap()
                        }
                    }
                    _ => panic!("unexpected model removal request: {path}"),
                };
                paths.push(path);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            paths
        });
        (endpoint, server)
    }

    #[tokio::test]
    async fn remove_requires_absence_from_installed_inventory() {
        let (endpoint, server) = serve_remove_inventory_fixture(
            r#"{"models":[{"name":"coder:small","digest":"sha256:still-installed","size":1048576}]}"#,
            Some(r#"{"models":[{"name":"coder:small","digest":"sha256:still-installed","size":1048576}]}"#),
        )
        .await;
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .remove("coder:small")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("still installed"), "{error}");
        assert_eq!(
            server.await.unwrap(),
            ["/api/tags", "/api/delete", "/api/tags"]
        );

        let (endpoint, server) = serve_remove_inventory_fixture(
            r#"{"models":[{"name":"coder:small","digest":"sha256:installed","size":1048576}]}"#,
            Some(r#"{"models":[{"name":"coder:small","size":1048576}]}"#),
        )
        .await;
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .remove("coder:small")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot be confirmed"), "{error}");
        assert_eq!(
            server.await.unwrap(),
            ["/api/tags", "/api/delete", "/api/tags"]
        );

        let (endpoint, server) = serve_remove_inventory_fixture(
            r#"{"models":[{"name":"coder:small","digest":"sha256:installed","size":1048576}]}"#,
            Some(r#"{"models":[]}"#),
        )
        .await;
        LocalRuntime::new(&endpoint)
            .unwrap()
            .remove("coder:small")
            .await
            .unwrap();
        assert_eq!(
            server.await.unwrap(),
            ["/api/tags", "/api/delete", "/api/tags"]
        );
    }

    #[tokio::test]
    async fn remove_refuses_ambiguous_or_incomplete_inventory_before_delete() {
        let (endpoint, server) = serve_remove_inventory_fixture(
            r#"{"models":[
                {"name":"coder:small","digest":"sha256:first","size":1048576},
                {"name":"library/coder:small","digest":"sha256:second","size":1048576}
            ]}"#,
            None,
        )
        .await;
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .remove("coder:small")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ambiguous"), "{error}");
        assert_eq!(server.await.unwrap(), ["/api/tags"]);

        let (endpoint, server) = serve_remove_inventory_fixture(
            r#"{"models":[{"name":"coder:small","size":1048576}]}"#,
            None,
        )
        .await;
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .remove("coder:small")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unusable"), "{error}");
        assert_eq!(server.await.unwrap(), ["/api/tags"]);
    }

    #[tokio::test]
    async fn remove_does_not_accept_malformed_inventory_names_as_confirmed_absence() {
        let valid =
            r#"{"models":[{"name":"coder:small","digest":"sha256:installed","size":1048576}]}"#;
        let malformed = r#"{"models":[{"name":"library//coder:small","digest":"sha256:installed","size":1048576}]}"#;

        let (endpoint, server) = serve_remove_inventory_fixture(malformed, None).await;
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .remove("coder:small")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unusable"), "{error}");
        assert_eq!(server.await.unwrap(), ["/api/tags"]);

        let (endpoint, server) = serve_remove_inventory_fixture(valid, Some(malformed)).await;
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .remove("coder:small")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot be confirmed"), "{error}");
        assert_eq!(
            server.await.unwrap(),
            ["/api/tags", "/api/delete", "/api/tags"]
        );
    }

    #[tokio::test]
    async fn daemon_error_after_progress_is_not_a_download_success() {
        let endpoint = serve_once(
            "200 OK",
            "{\"status\":\"pulling\"}\n{\"error\":\"disk full\"}\n",
            "",
        )
        .await;
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .pull("qwen2.5-coder:0.5b", |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disk full"));
    }

    #[tokio::test]
    async fn cloud_alias_cannot_receive_prompt_context() {
        let endpoint = serve_once(
            "200 OK",
            r#"{"remote_model":"cloud-alias","model_info":{},"details":{"format":"gguf"}}"#,
            "",
        )
        .await;
        let error = LocalRuntime::new(&endpoint)
            .unwrap()
            .chat(
                "innocent-name:latest",
                "private code",
                "private goal",
                4096,
                512,
                LocalThinkingMode::RuntimeDefault,
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("refuses to send repository context"));
    }

    #[test]
    fn changed_digest_or_runtime_invalidates_calibration() {
        let model = LocalModel {
            name: "m:1".into(),
            digest: "digest-a".into(),
            size_bytes: 1,
            parameter_size: None,
            quantization: None,
        };
        let mut profile = ModelProfile {
            schema: 1,
            model: model.name.clone(),
            digest: model.digest.clone(),
            runtime_version: "1".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            context_tokens: 4096,
            output_tokens: 512,
            protocol: Some(EditProtocol::SearchReplace),
            thinking: None,
            probes: vec![ModelProbe {
                name: "SearchReplace edit".into(),
                status: CheckStatus::Passed,
                output: String::new(),
                detail: String::new(),
                input_tokens: Some(10),
                output_tokens: Some(10),
                elapsed_ms: 5,
            }],
            hardware: HardwareSnapshot::default(),
            measured_at_unix: 1,
        };
        let mut legacy_profile = serde_json::to_value(&profile).unwrap();
        legacy_profile.as_object_mut().unwrap().remove("thinking");
        let legacy_profile: ModelProfile = serde_json::from_value(legacy_profile).unwrap();
        assert_eq!(legacy_profile.thinking, None);
        assert!(validate_profile(&profile, &model, &profile.endpoint, "1").is_ok());
        let equivalent_alias = LocalModel {
            name: "library/m:1".into(),
            ..model.clone()
        };
        assert!(validate_profile(&profile, &equivalent_alias, &profile.endpoint, "1").is_ok());
        let distinct_namespace = LocalModel {
            name: "team/m:1".into(),
            ..model.clone()
        };
        assert!(validate_profile(&profile, &distinct_namespace, &profile.endpoint, "1").is_err());
        let mut measured_profile = profile.clone();
        measured_profile.schema = 2;
        measured_profile.thinking = Some(LocalThinkingMode::Off);
        assert!(validate_profile(&measured_profile, &model, &profile.endpoint, "1").is_ok());
        let mut stripped = serde_json::to_value(&measured_profile).unwrap();
        stripped.as_object_mut().unwrap().remove("thinking");
        let stripped: ModelProfile = serde_json::from_value(stripped).unwrap();
        assert!(validate_profile(&stripped, &model, &profile.endpoint, "1").is_err());
        profile.thinking = Some(LocalThinkingMode::Off);
        assert!(validate_profile(&profile, &model, &profile.endpoint, "1").is_err());
        profile.thinking = None;
        assert!(validate_profile_context(
            &profile,
            &json!({"model_info":{"general.architecture":"qwen2","qwen2.context_length":4096}})
        )
        .is_ok());
        assert!(validate_profile_context(
            &profile,
            &json!({"model_info":{"general.architecture":"qwen2","qwen2.context_length":2048}})
        )
        .is_err());
        assert!(validate_profile(&profile, &model, &profile.endpoint, "2").is_err());
        profile.digest = "digest-b".into();
        assert!(validate_profile(&profile, &model, &profile.endpoint, "1").is_err());
    }

    #[test]
    fn resident_reuse_requires_exact_digest_context_and_reported_allocation() {
        let expires_at = (OffsetDateTime::now_utc() + time::Duration::minutes(2))
            .format(&Rfc3339)
            .unwrap();
        let observed = json!({"models":[{"name":"coder:small","digest":"digest-a","size":1200,"size_vram":1000,"context_length":4096,"expires_at":expires_at}]});
        let matching = resident_from_ps(&observed, "coder:small", "digest-a", 4096)
            .unwrap()
            .unwrap();
        assert_eq!(matching.context_length, 4096);
        assert_eq!(matching.size_vram_bytes, 1000);
        assert!(matching.expires_at_unix > OffsetDateTime::now_utc().unix_timestamp());
        let alias = json!({"models":[{"name":"library/coder:small","digest":"digest-a","size":1200,"size_vram":1000,"context_length":4096,"expires_at":expires_at}]});
        assert!(resident_from_ps(&alias, "coder:small", "digest-a", 4096)
            .unwrap()
            .is_some());
        let distinct = json!({"models":[{"name":"team/coder:small","digest":"digest-a","size":1200,"size_vram":1000,"context_length":4096,"expires_at":expires_at}]});
        assert!(resident_from_ps(&distinct, "coder:small", "digest-a", 4096)
            .unwrap()
            .is_none());
        let ambiguous = json!({"models":[{"name":"coder:small","digest":"digest-a"},{"name":"library/coder:small","digest":"digest-a"}]});
        assert!(resident_from_ps(&ambiguous, "coder:small", "digest-a", 4096).is_err());
        assert!(resident_from_ps(&observed, "coder:small", "digest-a", 8192)
            .unwrap()
            .is_none());
        assert!(resident_from_ps(&observed, "coder:small", "digest-b", 4096).is_err());
        assert!(
            resident_from_ps(&json!({"models":[]}), "coder:small", "digest-a", 4096)
                .unwrap()
                .is_none()
        );
        assert!(resident_from_ps(&json!({"models":[{"name":"coder:small","digest":"digest-a","size":1200,"context_length":4096,"expires_at":expires_at}]}), "coder:small", "digest-a", 4096).is_err());
        assert!(resident_from_ps(&json!({"models":[{"name":"coder:small","digest":"digest-a","size":1200,"size_vram":1000,"context_length":4096}]}), "coder:small", "digest-a", 4096).is_err());
        let near_expiry = (OffsetDateTime::now_utc() + time::Duration::seconds(5))
            .format(&Rfc3339)
            .unwrap();
        assert!(resident_from_ps(&json!({"models":[{"name":"coder:small","digest":"digest-a","size":1200,"size_vram":1000,"context_length":4096,"expires_at":near_expiry}]}), "coder:small", "digest-a", 4096).unwrap().is_none());
        assert!(resident_from_ps(&json!({"models":[{"name":"coder:small","digest":"digest-a","size":1200,"size_vram":1000,"context_length":4096,"expires_at":"soon"}]}), "coder:small", "digest-a", 4096).is_err());
    }

    #[test]
    fn local_transport_rejects_external_origins_and_ambiguous_urls() {
        for value in [
            "https://ollama.com",
            "http://127.0.0.1.evil.test",
            "http://192.168.1.2:11434",
            "http://user:secret@127.0.0.1",
            "http://localhost/api",
            "http://localhost?proxy=x",
            "file:///tmp/model",
        ] {
            assert!(local_endpoint(value).is_err(), "accepted {value}");
        }
        assert_eq!(
            local_endpoint("http://localhost:11434/").unwrap(),
            "http://127.0.0.1:11434"
        );
        assert!(local_endpoint("http://[::1]:11434").is_ok());
    }

    #[test]
    fn cloud_alias_metadata_is_rejected_before_inference() {
        assert!(ensure_local_metadata(
            &json!({"remote_model":"large", "details":{"format":"gguf"}, "model_info":{}})
        )
        .is_err());
        assert!(
            ensure_local_metadata(&json!({"details":{"format":"gguf"}, "model_info":{}})).is_ok()
        );
        assert!(ensure_local_metadata(&json!({})).is_err());
    }

    #[test]
    fn pull_errors_cannot_be_misreported_as_success() {
        assert!(parse_progress(br#"{"error":"disk full","status":"success"}"#).is_err());
        assert!(parse_progress(br#"{"completed":100}"#).is_err());
        let event = parse_progress(br#"{"status":"pulling","total":100,"completed":21}"#).unwrap();
        assert_eq!(event.completed, Some(21));
    }

    #[test]
    fn probe_rejects_plausible_but_wrong_or_out_of_scope_edits() {
        assert!(!edit_probe_passed(
            EditProtocol::SearchReplace,
            r#"{"path":"other.py","search":"def add(a, b): return a - b","replace":"def add(a, b): return a + b"}"#
        ));
        assert!(!edit_probe_passed(
            EditProtocol::SearchReplace,
            r#"{"path":"add.py","search":"def add(a, b): return a - b","replace":"def add(a, b): return a * b"}"#
        ));
        assert!(edit_probe_passed(
            EditProtocol::SearchReplace,
            r#"{"path":"add.py","search":"def add(a, b): return a - b","replace":"def add(a, b): return a + b"}"#
        ));
    }

    #[test]
    fn creation_probe_uses_the_exact_new_file_parser() {
        assert!(create_probe_passed(
            EditProtocol::SearchReplace,
            r#"{"path":"probe_add.py","text":"def add(a, b):\n    return a + b"}"#
        ));
        for output in [
            r#"{"path":"other.py","text":"def add(a, b):\n    return a + b\n"}"#,
            r#"{"path":"probe_add.py","text":"def add(a, b):\n    return a - b\n"}"#,
            r#"{"path":"probe_add.py","text":"def add(a, b):\n    return a + b\n","extra":true}"#,
            "```json\n{\"path\":\"probe_add.py\",\"text\":\"wrong\"}\n```",
        ] {
            assert!(!create_probe_passed(EditProtocol::SearchReplace, output));
        }
        let canonical = "--- /dev/null\n+++ b/probe_add.py\n@@ -0,0 +1,2 @@\n+def add(a, b):\n+    return a + b\n";
        assert!(create_probe_passed(EditProtocol::UnifiedDiff, canonical));
        assert!(!create_probe_passed(
            EditProtocol::UnifiedDiff,
            &canonical.replace("+    return a + b", "+    return a - b")
        ));
        assert!(!create_probe_passed(
            EditProtocol::UnifiedDiff,
            &canonical.replace("+++ b/probe_add.py", "+++ b/other.py")
        ));
    }
}
