//! Point-in-time binding between a Phonton-started Ollama process and the
//! model store supplied to that process at launch. A loopback HTTP answer
//! alone cannot establish an external service's model-store directory.

use crate::{disk, provision, LocalError, Result};
use phonton_types::local::LocalDirectoryIdentity;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{FILETIME, HANDLE, WAIT_TIMEOUT};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

const RECEIPT_NAME: &str = "managed-process.json";
const MAX_RECEIPT_BYTES: u64 = 64 * 1024;
#[cfg(windows)]
const SYNCHRONIZE_PROCESS: u32 = 0x0010_0000;

/// A path in the platform's native units: UTF-16 on Windows, bytes on Unix.
#[cfg(windows)]
type PathUnits = Vec<u16>;
#[cfg(unix)]
type PathUnits = Vec<u8>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LaunchReceipt {
    schema: u32,
    pid: u32,
    created_at: u64,
    image: PathUnits,
    root: PathUnits,
    store: PathUnits,
    blobs: PathUnits,
    root_identity: LocalDirectoryIdentity,
    store_identity: LocalDirectoryIdentity,
    blobs_identity: LocalDirectoryIdentity,
    endpoint: String,
    version: String,
}

#[cfg(windows)]
struct ProcessHandle(OwnedHandle);

#[cfg(windows)]
impl ProcessHandle {
    fn open(pid: u32) -> Result<Self> {
        if pid == 0 {
            return Err(LocalError::Invalid(
                "Managed runtime receipt has no process ID".into(),
            ));
        }
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE_PROCESS,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Err(LocalError::Invalid(format!(
                "Managed runtime process {pid} is no longer inspectable: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(handle) }))
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    fn identity(&self) -> Result<(u64, PathUnits)> {
        if unsafe { WaitForSingleObject(self.raw(), 0) } != WAIT_TIMEOUT {
            return Err(LocalError::Invalid(
                "Managed runtime process has exited".into(),
            ));
        }
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if unsafe {
            GetProcessTimes(
                self.raw(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let created_at =
            (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        let mut image = vec![0u16; 32_768];
        let mut size = u32::try_from(image.len())
            .map_err(|_| LocalError::Invalid("Managed runtime image path is too long".into()))?;
        if unsafe { QueryFullProcessImageNameW(self.raw(), 0, image.as_mut_ptr(), &mut size) } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        image.truncate(size as usize);
        let image = PathBuf::from(std::ffi::OsString::from_wide(&image));
        Ok((created_at, path_units(&std::fs::canonicalize(image)?)))
    }
}

#[cfg(windows)]
fn path_units(path: &Path) -> PathUnits {
    path.as_os_str().encode_wide().collect()
}

#[cfg(unix)]
fn path_units(path: &Path) -> PathUnits {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

/// A process ID plus the start time read when it was opened. Unix has no
/// process handle to hold, so every identity check compares the start time:
/// a reused PID belongs to a process that started later.
#[cfg(unix)]
struct ProcessHandle {
    pid: u32,
}

#[cfg(unix)]
impl ProcessHandle {
    fn open(pid: u32) -> Result<Self> {
        if pid == 0 || i32::try_from(pid).is_err() {
            return Err(LocalError::Invalid(
                "Managed runtime receipt has no process ID".into(),
            ));
        }
        process_start_and_image(pid)?;
        Ok(Self { pid })
    }

    fn identity(&self) -> Result<(u64, PathUnits)> {
        let (created_at, image) = process_start_and_image(self.pid)?;
        Ok((created_at, path_units(&std::fs::canonicalize(image)?)))
    }
}

/// Start time (clock ticks since boot) and executable from `/proc`.
#[cfg(target_os = "linux")]
fn process_start_and_image(pid: u32) -> Result<(u64, PathBuf)> {
    let gone = || LocalError::Invalid(format!("Managed runtime process {pid} has exited"));
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|_| gone())?;
    // The command name may contain spaces or ')'; fields follow the last ')'.
    let fields: Vec<&str> = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().collect())
        .unwrap_or_default();
    if matches!(fields.first(), None | Some(&"Z") | Some(&"X")) {
        return Err(gone());
    }
    let started = fields
        .get(19)
        .and_then(|field| field.parse::<u64>().ok())
        .ok_or_else(|| {
            LocalError::Invalid("Managed runtime process status is unreadable".into())
        })?;
    let image = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|error| {
        LocalError::Invalid(format!(
            "Managed runtime process {pid} is no longer inspectable: {error}"
        ))
    })?;
    Ok((started, image))
}

/// Start time (microseconds since the epoch) and executable from libproc.
#[cfg(target_os = "macos")]
fn process_start_and_image(pid: u32) -> Result<(u64, PathBuf)> {
    use std::os::unix::ffi::OsStringExt;
    const SZOMB: u32 = 5;
    let gone = || LocalError::Invalid(format!("Managed runtime process {pid} has exited"));
    let pid = i32::try_from(pid).map_err(|_| gone())?;
    // SAFETY: proc_bsdinfo is plain data; the kernel fills at most `size` bytes.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if read != size || info.pbi_status == SZOMB {
        return Err(gone());
    }
    let started = info
        .pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec);
    let mut image = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is writable for its full length.
    let length = unsafe { libc::proc_pidpath(pid, image.as_mut_ptr().cast(), image.len() as u32) };
    if length <= 0 {
        return Err(LocalError::Invalid(format!(
            "Managed runtime process {pid} is no longer inspectable: {}",
            std::io::Error::last_os_error()
        )));
    }
    image.truncate(length as usize);
    Ok((started, PathBuf::from(std::ffi::OsString::from_vec(image))))
}

fn receipt_path(root: &Path) -> PathBuf {
    root.join(RECEIPT_NAME)
}

fn checked_store_paths(root: &Path, create: bool) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let root = std::fs::canonicalize(root)?;
    let store = root.join("models");
    if create && !store.exists() {
        std::fs::create_dir(&store)?;
    }
    provision::checked_direct_child(&root, &store)?;
    if !std::fs::symlink_metadata(&store)?.is_dir() {
        return Err(LocalError::Invalid(
            "Managed model store is not a directory".into(),
        ));
    }
    let blobs = store.join("blobs");
    if create && !blobs.exists() {
        std::fs::create_dir(&blobs)?;
    }
    provision::checked_direct_child(&store, &blobs)?;
    if !std::fs::symlink_metadata(&blobs)?.is_dir() {
        return Err(LocalError::Invalid(
            "Managed model blob store is not a directory".into(),
        ));
    }
    Ok((root, store, blobs))
}

fn read_receipt(root: &Path) -> Result<Option<LaunchReceipt>> {
    let path = receipt_path(root);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.len() > MAX_RECEIPT_BYTES {
        return Err(LocalError::Invalid(
            "Managed runtime receipt is not a small regular file".into(),
        ));
    }
    provision::checked_direct_child(root, &path)?;
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    if value["schema"] != 2 {
        return Err(LocalError::Invalid(
            "Managed runtime receipt predates directory identity verification; stop it and rerun managed setup".into(),
        ));
    }
    let receipt: LaunchReceipt = serde_json::from_value(value)?;
    if receipt.pid == 0 || receipt.created_at == 0 || receipt.image.is_empty() {
        return Err(LocalError::Invalid(
            "Managed runtime receipt is invalid".into(),
        ));
    }
    Ok(Some(receipt))
}

fn safe_existing_child(parent: &Path, child: &Path, directory: bool) -> Result<()> {
    match std::fs::symlink_metadata(child) {
        Ok(metadata) => {
            if metadata.is_dir() != directory || !(metadata.is_dir() || metadata.is_file()) {
                return Err(LocalError::Invalid(format!(
                    "Managed runtime path has an unexpected file type: {}",
                    child.display()
                )));
            }
            provision::checked_direct_child(parent, child)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Whether an offline managed setup may try to replace a missing or stale
/// launch receipt. This is a read-only hint; setup must recheck every path.
/// A valid receipt for different storage remains blocked instead of being
/// silently rebound to a replacement model store.
pub fn setup_retryable(root: &Path, endpoint: &str) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    if disk::require_local_drive_directory(&root).is_err() {
        return false;
    }
    let store = root.join("models");
    let blobs = store.join("blobs");
    if safe_existing_child(&root, &store, true).is_err()
        || safe_existing_child(&store, &blobs, true).is_err()
        || safe_existing_child(&root, &root.join("runtime.log"), false).is_err()
        || safe_existing_child(&root, &receipt_path(&root), false).is_err()
    {
        return false;
    }
    let receipt = match read_receipt(&root) {
        Ok(Some(receipt)) => receipt,
        Ok(None) | Err(LocalError::Json(_) | LocalError::Invalid(_)) => return true,
        Err(_) => return false,
    };
    if receipt.endpoint != endpoint {
        return false;
    }
    let Ok((root, store, blobs)) = checked_store_paths(&root, false) else {
        return false;
    };
    path_units(&root) == receipt.root
        && path_units(&store) == receipt.store
        && path_units(&blobs) == receipt.blobs
        && disk::directory_identity(&root).is_ok_and(|identity| identity == receipt.root_identity)
        && disk::directory_identity(&store).is_ok_and(|identity| identity == receipt.store_identity)
        && disk::directory_identity(&blobs).is_ok_and(|identity| identity == receipt.blobs_identity)
}

/// Record a successfully started child while the caller holds the runtime-root
/// lease. The caller must first verify the child owns the exact loopback port.
pub(crate) fn record_launch(root: &Path, port: u16, pid: u32, version: &str) -> Result<()> {
    if provision::loopback_listener_owner(port)? != Some(pid) {
        return Err(LocalError::Invalid(
            "Managed runtime listener changed before its receipt was recorded".into(),
        ));
    }
    let (root, store, blobs) = checked_store_paths(root, true)?;
    let (created_at, image) = ProcessHandle::open(pid)?.identity()?;
    let receipt = LaunchReceipt {
        schema: 2,
        pid,
        created_at,
        image,
        root: path_units(&root),
        store: path_units(&store),
        blobs: path_units(&blobs),
        root_identity: disk::directory_identity(&root)?,
        store_identity: disk::directory_identity(&store)?,
        blobs_identity: disk::directory_identity(&blobs)?,
        endpoint: format!("http://127.0.0.1:{port}"),
        version: version.to_owned(),
    };
    let final_path = receipt_path(&root);
    match std::fs::symlink_metadata(&final_path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(LocalError::Invalid(
                    "Managed runtime receipt path is not a regular file".into(),
                ));
            }
            provision::checked_direct_child(&root, &final_path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| LocalError::Invalid("System clock is before the Unix epoch".into()))?
        .as_nanos();
    let stage = root.join(format!("{RECEIPT_NAME}.{}-{nonce}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    if bytes.len() as u64 > MAX_RECEIPT_BYTES {
        return Err(LocalError::Invalid(
            "Managed runtime receipt exceeds its size limit".into(),
        ));
    }
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&stage, &final_path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&stage);
    }
    result?;
    Ok(())
}

/// A verified point-in-time managed model-store binding. Recheck immediately
/// before a pull and as progress arrives; other processes can still race it.
pub struct VerifiedManagedStore {
    root: PathBuf,
    receipt: LaunchReceipt,
    process: ProcessHandle,
}

impl VerifiedManagedStore {
    /// The PID recorded when Phonton started this managed runtime.
    pub fn pid(&self) -> u32 {
        self.receipt.pid
    }

    /// Recheck process identity, exact listener ownership, store paths and free
    /// bytes for the caller's quota on the model-blob volume.
    pub fn available_bytes(&self) -> Result<u64> {
        let (created_at, image) = self.process.identity()?;
        if created_at != self.receipt.created_at || image != self.receipt.image {
            return Err(LocalError::Invalid(
                "Managed runtime process identity changed".into(),
            ));
        }
        let port = self
            .receipt
            .endpoint
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .ok_or_else(|| {
                LocalError::Invalid("Managed runtime receipt has an invalid endpoint".into())
            })?;
        if provision::loopback_listener_owner(port)? != Some(self.receipt.pid) {
            return Err(LocalError::Invalid(
                "Managed runtime no longer owns its exact loopback listener".into(),
            ));
        }
        let (root, store, blobs) = checked_store_paths(&self.root, false)?;
        if path_units(&root) != self.receipt.root
            || path_units(&store) != self.receipt.store
            || path_units(&blobs) != self.receipt.blobs
        {
            return Err(LocalError::Invalid(
                "Managed model-store path changed since startup".into(),
            ));
        }
        if disk::directory_identity(&root)? != self.receipt.root_identity
            || disk::directory_identity(&store)? != self.receipt.store_identity
            || disk::directory_identity(&blobs)? != self.receipt.blobs_identity
        {
            return Err(LocalError::Invalid(
                "Managed model-store directory or volume changed since startup".into(),
            ));
        }
        disk::available_directory_bytes(&blobs)
    }

    /// Expose the blob directory only after revalidating the exact managed
    /// process, listener, store path and directory identities.
    pub(crate) fn verified_blobs_directory(&self) -> Result<PathBuf> {
        self.available_bytes()?;
        let (_, _, blobs) = checked_store_paths(&self.root, false)?;
        Ok(blobs)
    }
}

/// Bind a later operation to the exact live process Phonton started. A missing
/// receipt means an external/unverified service; a stale receipt is an error.
pub fn bind(root: &Path, endpoint: &str) -> Result<Option<VerifiedManagedStore>> {
    let root = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(receipt) = read_receipt(&root)? else {
        return Ok(None);
    };
    // A deliberately nondefault configured loopback origin is an external
    // service. A mismatch at the managed default is a damaged/stale receipt
    // and must not silently downgrade to an unchecked pull.
    if endpoint != receipt.endpoint {
        if endpoint == "http://127.0.0.1:11434" {
            return Err(LocalError::Invalid(
                "Managed default endpoint differs from the launch receipt".into(),
            ));
        }
        return Ok(None);
    }
    if path_units(&root) != receipt.root {
        return Err(LocalError::Invalid(
            "Managed root differs from the launch receipt".into(),
        ));
    }
    let process = ProcessHandle::open(receipt.pid)?;
    let binding = VerifiedManagedStore {
        root,
        receipt,
        process,
    };
    binding.available_bytes()?;
    Ok(Some(binding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn matching_live_process_and_store_bind_but_stale_receipts_do_not() {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{port}");
        record_launch(root.path(), port, std::process::id(), "test").unwrap();
        let binding = bind(root.path(), &endpoint).unwrap().unwrap();
        assert_eq!(binding.pid(), std::process::id());
        assert!(binding.available_bytes().unwrap() > 0);

        let path = receipt_path(root.path());
        let original = std::fs::read(&path).unwrap();
        let mut receipt: LaunchReceipt = serde_json::from_slice(&original).unwrap();
        receipt.created_at += 1;
        std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(bind(root.path(), &endpoint)
            .err()
            .unwrap()
            .to_string()
            .contains("identity changed"));
        receipt.created_at -= 1;
        receipt.image.push(1);
        std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(bind(root.path(), &endpoint)
            .err()
            .unwrap()
            .to_string()
            .contains("identity changed"));
        std::fs::write(&path, original).unwrap();
        drop(listener);
        assert!(binding.available_bytes().is_err());
    }

    #[test]
    fn new_managed_launch_replaces_a_corrupt_prior_receipt() {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::write(receipt_path(root.path()), b"truncated{").unwrap();
        record_launch(root.path(), port, std::process::id(), "test").unwrap();
        assert!(bind(root.path(), &format!("http://127.0.0.1:{port}"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn setup_retry_accepts_a_regular_stale_receipt_but_not_an_unsafe_receipt_path() {
        let root = tempfile::tempdir().unwrap();
        let endpoint = "http://127.0.0.1:11434";
        assert!(setup_retryable(root.path(), endpoint));
        std::fs::write(receipt_path(root.path()), b"truncated{").unwrap();
        assert!(setup_retryable(root.path(), endpoint));
        std::fs::remove_file(receipt_path(root.path())).unwrap();
        std::fs::create_dir(receipt_path(root.path())).unwrap();
        assert!(!setup_retryable(root.path(), endpoint));
    }

    #[test]
    fn setup_retry_preserves_a_valid_receipts_store_identity() {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{port}");
        record_launch(root.path(), port, std::process::id(), "test").unwrap();
        assert!(setup_retryable(root.path(), &endpoint));
        assert!(!setup_retryable(root.path(), "http://127.0.0.1:11434"));
        let store = root.path().join("models");
        std::fs::rename(&store, root.path().join("old-models")).unwrap();
        std::fs::create_dir(&store).unwrap();
        std::fs::create_dir(store.join("blobs")).unwrap();
        assert!(!setup_retryable(root.path(), &endpoint));
    }

    #[test]
    fn replacement_store_at_same_path_invalidates_binding() {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{port}");
        record_launch(root.path(), port, std::process::id(), "test").unwrap();
        let store = root.path().join("models");
        std::fs::rename(&store, root.path().join("old-models")).unwrap();
        std::fs::create_dir(&store).unwrap();
        std::fs::create_dir(store.join("blobs")).unwrap();
        assert!(bind(root.path(), &endpoint)
            .err()
            .unwrap()
            .to_string()
            .contains("directory or volume changed"));
    }

    #[test]
    fn legacy_receipt_without_directory_identity_is_not_verified() {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{port}");
        record_launch(root.path(), port, std::process::id(), "test").unwrap();
        let path = receipt_path(root.path());
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["schema"] = 1.into();
        value.as_object_mut().unwrap().remove("root_identity");
        value.as_object_mut().unwrap().remove("store_identity");
        value.as_object_mut().unwrap().remove("blobs_identity");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(bind(root.path(), &endpoint)
            .err()
            .unwrap()
            .to_string()
            .contains("predates directory identity verification"));
    }

    #[test]
    fn missing_receipt_is_explicitly_unverified() {
        let root = tempfile::tempdir().unwrap();
        assert!(bind(root.path(), "http://127.0.0.1:11434")
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_different_configured_origin_is_external_despite_an_old_receipt() {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        record_launch(root.path(), port, std::process::id(), "test").unwrap();
        let alternate = if port == 11435 { 11436 } else { 11435 };
        assert!(bind(root.path(), &format!("http://127.0.0.1:{alternate}"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn managed_default_refuses_a_receipt_with_a_different_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        record_launch(root.path(), port, std::process::id(), "test").unwrap();
        let path = receipt_path(root.path());
        let mut receipt: LaunchReceipt =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        receipt.endpoint = "http://127.0.0.1:9".into();
        std::fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(bind(root.path(), "http://127.0.0.1:11434")
            .err()
            .unwrap()
            .to_string()
            .contains("default endpoint differs"));
    }

    #[test]
    fn redirected_blob_directory_invalidates_a_managed_binding() {
        #[cfg(unix)]
        use std::os::unix::fs::symlink as symlink_dir;
        #[cfg(windows)]
        use std::os::windows::fs::symlink_dir;
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{port}");
        record_launch(root.path(), port, std::process::id(), "test").unwrap();
        let blobs = root.path().join("models").join("blobs");
        std::fs::remove_dir(&blobs).unwrap();
        match symlink_dir(elsewhere.path(), &blobs) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("could not create fixture link: {error}"),
        }
        assert!(bind(root.path(), &endpoint)
            .err()
            .unwrap()
            .to_string()
            .contains("reparse point"));
    }
}
