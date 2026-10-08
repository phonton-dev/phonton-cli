//! Explicit portable runtime installation. No PATH edits, login, startup entries,
//! package manager scripts, or system-wide installation.

use crate::{LocalError, Result};
use phonton_types::local::DownloadProgress;
use std::path::Path;

/// Pinned official release. Updating this requires replacing both URL and hash.
pub const RUNTIME_VERSION: &str = "0.34.2";
/// Published GitHub asset size, observed on 2026-09-22.
pub const ARCHIVE_BYTES: u64 = 1_460_928_014;
/// Minimum free space for a fresh managed runtime install, including extraction reserve.
pub const MIN_INSTALL_FREE_BYTES: u64 = ARCHIVE_BYTES * 4;
#[cfg(all(windows, target_arch = "x86_64"))]
const ARCHIVE_SHA256: &str = "8f3fd071a2a2f9497b562f43502c77c2b701a99d1ee5dfda28da8c786373063b";
// Independently extracted ollama.exe from the archive with the pinned SHA-256.
#[cfg(all(windows, target_arch = "x86_64"))]
const EXECUTABLE_SHA256: &str = "ad41dcf55c5de96d4a0bff7c559a17285c3aa064a6f12d23db3ebf59ad8e4125";
// Path-and-content digest of the 82 files independently extracted from the
// pinned ZIP. This covers the executable, CPU/CUDA/Vulkan DLLs and load tree.
#[cfg(all(windows, target_arch = "x86_64"))]
const TREE_SHA256: &str = "3367e5c4874bcdced42c85cfac1aa0f8aa68e7b538e7c7d6ea17495323c74397";
#[cfg(all(windows, target_arch = "x86_64"))]
const ARCHIVE_URL: &str =
    "https://github.com/ollama/ollama/releases/download/v0.34.2/ollama-windows-amd64.zip";

#[cfg(all(windows, target_arch = "x86_64"))]
static ARCHIVE_HASHES_IN_FLIGHT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(windows, target_arch = "x86_64"))]
#[derive(Debug)]
struct ArchiveHashPermit;

#[cfg(all(windows, target_arch = "x86_64"))]
impl ArchiveHashPermit {
    fn new() -> Self {
        ARCHIVE_HASHES_IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
impl Drop for ArchiveHashPermit {
    fn drop(&mut self) {
        ARCHIVE_HASHES_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
#[derive(Debug)]
struct LockedArchive {
    // Drop the Windows file handle before the permit allows stage cleanup.
    file: std::fs::File,
    _permit: ArchiveHashPermit,
}

#[cfg(all(windows, target_arch = "x86_64"))]
async fn wait_for_archive_hashes() -> Result<()> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    tokio::time::timeout(Duration::from_secs(60), async {
        while ARCHIVE_HASHES_IN_FLIGHT.load(Ordering::Acquire) > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| {
        LocalError::Invalid(
            "An earlier runtime archive verification is still finishing; retry setup shortly."
                .into(),
        )
    })
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn is_reparse_point(path: &Path) -> Result<bool> {
    use std::os::windows::fs::MetadataExt;
    const REPARSE_POINT: u32 = 0x400;
    Ok(std::fs::symlink_metadata(path)?.file_attributes() & REPARSE_POINT != 0)
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn fresh_runtime_root(root: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
        Ok(metadata) if metadata.is_dir() && !is_reparse_point(root)? => {
            Ok(std::fs::read_dir(root)?.next().transpose()?.is_none())
        }
        Ok(_) => Ok(false),
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn require_install_reserve(free: u64, credited_download_bytes: u64) -> Result<()> {
    if free.saturating_add(credited_download_bytes) < MIN_INSTALL_FREE_BYTES {
        return Err(LocalError::Invalid("Managed runtime setup requires about 6 GB of free disk space, less bytes already allocated to its resumable archive, including extraction reserve.".into()));
    }
    Ok(())
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn reject_nonlocal_runtime_root(path: &Path) -> Result<()> {
    use std::path::{Component, Prefix};
    if let Some(Component::Prefix(prefix)) = path.components().next() {
        if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
            return Err(LocalError::Invalid(
                "Managed runtime setup requires a local drive path; network and device roots are unsupported"
                    .into(),
            ));
        }
    }
    Ok(())
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn local_runtime_root(canonical: &Path) -> Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Prefix};
    reject_nonlocal_runtime_root(canonical)?;
    if !canonical.is_absolute() {
        return Err(LocalError::Invalid(
            "Managed runtime canonical root is not an absolute local drive path".into(),
        ));
    }
    match canonical.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(_) => Ok(canonical.to_path_buf()),
            Prefix::VerbatimDisk(_) => {
                let wide: Vec<u16> = canonical.as_os_str().encode_wide().collect();
                if !wide.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]) {
                    return Err(LocalError::Invalid(
                        "Managed runtime canonical drive path is malformed".into(),
                    ));
                }
                Ok(std::path::PathBuf::from(OsString::from_wide(&wide[4..])))
            }
            _ => Err(LocalError::Invalid(
                "Managed runtime canonical root is not a local drive path".into(),
            )),
        },
        _ => Err(LocalError::Invalid(
            "Managed runtime canonical root is not an absolute local drive path".into(),
        )),
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
pub(crate) fn checked_direct_child(root: &Path, child: &Path) -> Result<()> {
    if is_reparse_point(child)? {
        return Err(LocalError::Invalid(format!(
            "Managed runtime path is a reparse point; inspect it before setup: {}",
            child.display()
        )));
    }
    let root = std::fs::canonicalize(root)?;
    let child = std::fs::canonicalize(child)?;
    if child.parent() != Some(root.as_path()) {
        return Err(LocalError::Invalid(format!(
            "Managed runtime path resolved outside its root: {}",
            child.display()
        )));
    }
    Ok(())
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn reject_tree_reparse_points(path: &Path) -> Result<()> {
    if is_reparse_point(path)? {
        return Err(LocalError::Invalid(format!(
            "Managed runtime staging contains a reparse point; inspect it before retrying: {}",
            path.display()
        )));
    }
    if std::fs::symlink_metadata(path)?.is_dir() {
        for entry in std::fs::read_dir(path)? {
            reject_tree_reparse_points(&entry?.path())?;
        }
    }
    Ok(())
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn stage_marker(archive_hash: &str) -> String {
    format!("phonton-managed-ollama-stage-v1:{archive_hash}\n")
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn cleanup_owned_stages(root: &Path, final_name: &str, archive_hash: &str) -> Result<()> {
    let prefix = format!("{final_name}.staging-");
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let stage = entry.path();
        let metadata = std::fs::symlink_metadata(&stage)?;
        if !metadata.is_dir() || is_reparse_point(&stage)? {
            continue; // Unknown content is never retired automatically.
        }
        let marker = stage.join(".phonton-stage");
        let Ok(marker_meta) = std::fs::symlink_metadata(&marker) else {
            continue;
        };
        if !marker_meta.is_file() || is_reparse_point(&marker)? || marker_meta.len() > 128 {
            continue;
        }
        if std::fs::read_to_string(&marker)? != stage_marker(archive_hash) {
            continue;
        }
        // Verify the resolved absolute child and every descendant before any
        // recursive delete. A foreign or linked tree is preserved for review.
        checked_direct_child(root, &stage)?;
        reject_tree_reparse_points(&stage)?;
        std::fs::remove_dir_all(&stage)?;
    }
    Ok(())
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn create_owned_stage(
    root: &Path,
    final_name: &str,
    archive_hash: &str,
) -> Result<std::path::PathBuf> {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};
    for attempt in 0..4u8 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| LocalError::Invalid("System clock is before the Unix epoch".into()))?
            .as_nanos();
        let stage = root.join(format!(
            "{final_name}.staging-{}-{nanos}-{attempt}",
            std::process::id()
        ));
        match std::fs::create_dir(&stage) {
            Ok(()) => {
                let mut marker = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(stage.join(".phonton-stage"))?;
                marker.write_all(stage_marker(archive_hash).as_bytes())?;
                marker.sync_all()?;
                return Ok(stage);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(LocalError::Invalid(
        "Could not reserve a unique managed runtime staging directory".into(),
    ))
}

#[cfg(all(windows, target_arch = "x86_64"))]
#[derive(Debug)]
struct OwnedDownloadStage {
    path: std::path::PathBuf,
    partial: std::path::PathBuf,
    file: std::fs::File,
    received: u64,
    // Logical length alone can credit a sparse file that uses almost no disk.
    allocated: u64,
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn allocated_file_bytes(file: &std::fs::File) -> Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandleEx, FILE_STANDARD_INFO,
    };
    let mut info = FILE_STANDARD_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&mut info as *mut FILE_STANDARD_INFO).cast(),
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if info.AllocationSize < 0
        || info.EndOfFile < 0
        || info.NumberOfLinks != 1
        || info.DeletePending
        || info.Directory
        || info.EndOfFile as u64 != file.metadata()?.len()
    {
        return Err(LocalError::Invalid(
            "Managed runtime partial archive is not a unique regular file; Phonton preserved it for inspection".into(),
        ));
    }
    Ok((info.AllocationSize as u64).min(info.EndOfFile as u64))
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn open_owned_partial(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const REPARSE_POINT: u32 = 0x400;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & REPARSE_POINT != 0 {
        return Err(LocalError::Invalid(format!(
            "Managed runtime partial archive is not a regular file: {}",
            path.display()
        )));
    }
    Ok(file)
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn find_owned_download_stage(
    root: &Path,
    archive_name: &str,
    archive_hash: &str,
    archive_bytes: u64,
) -> Result<Option<OwnedDownloadStage>> {
    let prefix = format!("{archive_name}.staging-");
    let mut selected = None;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let stage = entry.path();
        let metadata = std::fs::symlink_metadata(&stage)?;
        if !metadata.is_dir() || is_reparse_point(&stage)? {
            continue;
        }
        let marker = stage.join(".phonton-stage");
        let partial = stage.join("download.partial");
        let (Ok(marker_meta), Ok(partial_meta)) = (
            std::fs::symlink_metadata(&marker),
            std::fs::symlink_metadata(&partial),
        ) else {
            continue;
        };
        if !marker_meta.is_file()
            || marker_meta.len() > 128
            || is_reparse_point(&marker)?
            || !partial_meta.is_file()
            || is_reparse_point(&partial)?
            || std::fs::read_to_string(&marker)? != stage_marker(archive_hash)
        {
            continue;
        }
        // A marked directory containing anything else is not safely owned as
        // a resumable download; leave it untouched for inspection.
        let mut names = std::fs::read_dir(&stage)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        let expected = [
            std::ffi::OsString::from(".phonton-stage"),
            std::ffi::OsString::from("download.partial"),
        ];
        if names.as_slice() != expected.as_slice() {
            continue;
        }
        checked_direct_child(root, &stage)?;
        checked_direct_child(&stage, &partial)?;
        let file = open_owned_partial(&partial)?;
        let received = file.metadata()?.len();
        if received > archive_bytes {
            return Err(LocalError::Invalid(format!(
                "Managed runtime partial archive exceeds the pinned size; Phonton preserved it for inspection: {}",
                partial.display()
            )));
        }
        let allocated = allocated_file_bytes(&file)?.min(received);
        if selected.is_some() {
            return Err(LocalError::Invalid(
                "Multiple owned runtime downloads could be resumed; Phonton preserved them for inspection."
                    .into(),
            ));
        }
        selected = Some(OwnedDownloadStage {
            path: stage,
            partial,
            file,
            received,
            allocated,
        });
    }
    Ok(selected)
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn create_download_stage(
    root: &Path,
    archive_name: &str,
    archive_hash: &str,
) -> Result<OwnedDownloadStage> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const REPARSE_POINT: u32 = 0x400;
    let path = create_owned_stage(root, archive_name, archive_hash)?;
    checked_direct_child(root, &path)?;
    let partial = path.join("download.partial");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .custom_flags(OPEN_REPARSE_POINT)
        .create_new(true)
        .open(&partial)?;
    if !file.metadata()?.is_file() || file.metadata()?.file_attributes() & REPARSE_POINT != 0 {
        return Err(LocalError::Invalid(
            "Managed runtime partial archive creation did not yield a regular file".into(),
        ));
    }
    Ok(OwnedDownloadStage {
        path,
        partial,
        file,
        received: 0,
        allocated: 0,
    })
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn move_path_no_replace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;
    let from: Vec<u16> = from.as_os_str().encode_wide().chain([0]).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain([0]).collect();
    // MoveFileW has no replace flag. Sibling paths stay on the same volume;
    // an unexpectedly occupied destination is never replaced.
    if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(all(windows, target_arch = "x86_64"))]
async fn hash_locked_archive(path: &Path) -> Result<(LockedArchive, String)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const REPARSE_POINT: u32 = 0x400;
    // Keep this handle alive through Expand-Archive. Other readers may open it,
    // but writers and renamers cannot replace the bytes after verification.
    // Opening the leaf itself makes a link visible instead of following it to
    // a target whose pathname could change while Expand-Archive reopens ours.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & REPARSE_POINT != 0 {
        return Err(LocalError::Invalid(format!(
            "Managed runtime archive is not a regular file; inspect it before setup: {}",
            path.display()
        )));
    }
    let permit = ArchiveHashPermit::new();
    tokio::task::spawn_blocking(move || -> Result<(LockedArchive, String)> {
        let mut locked = LockedArchive {
            file,
            _permit: permit,
        };
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 128 * 1024];
        loop {
            let count = locked.file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        Ok((locked, format!("{:x}", hasher.finalize())))
    })
    .await
    .map_err(|error| LocalError::Invalid(format!("Runtime archive hash worker failed: {error}")))?
}

#[cfg(all(windows, target_arch = "x86_64"))]
async fn verified_archive_for_credit(
    path: &Path,
    expected_bytes: u64,
    expected_hash: &str,
) -> Result<(LockedArchive, u64)> {
    let (locked, hash) = hash_locked_archive(path).await?;
    if locked.file.metadata()?.len() != expected_bytes || hash != expected_hash {
        return Err(LocalError::Invalid(format!(
            "Runtime archive size or checksum mismatch: {}. It was not executed or extracted.",
            path.display()
        )));
    }
    let allocated = allocated_file_bytes(&locked.file)?;
    Ok((locked, allocated.min(expected_bytes)))
}

#[cfg(all(windows, target_arch = "x86_64"))]
async fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 128 * 1024];
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(all(windows, target_arch = "x86_64"))]
async fn tree_sha256(root: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    fn visit(
        root: &Path,
        directory: &Path,
        paths: &mut Vec<(String, std::path::PathBuf)>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            if is_reparse_point(&path)? {
                return Err(LocalError::Invalid(format!(
                    "Managed runtime tree contains a reparse point: {}",
                    path.display()
                )));
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                visit(root, &path, paths)?;
            } else if metadata.is_file() {
                if directory == root
                    && matches!(
                        path.file_name().and_then(|n| n.to_str()),
                        Some(".phonton-stage" | ".archive-sha256")
                    )
                {
                    continue;
                }
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| {
                        LocalError::Invalid("Runtime file resolved outside staging root".into())
                    })?
                    .to_str()
                    .ok_or_else(|| {
                        LocalError::Invalid("Runtime archive contains a non-UTF-8 path".into())
                    })?
                    .replace('\\', "/");
                paths.push((relative, path));
            } else {
                return Err(LocalError::Invalid(format!(
                    "Unexpected runtime tree entry: {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    visit(root, root, &mut paths)?;
    paths.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = Sha256::new();
    for (relative, path) in paths {
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update(sha256_file(&path).await?.as_bytes());
        hasher.update([10]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(all(windows, target_arch = "x86_64"))]
async fn installed_executable(
    root: &Path,
    directory: &Path,
    archive_hash: &str,
    executable_hash: &str,
    tree_hash: &str,
) -> Result<Option<std::path::PathBuf>> {
    match std::fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    checked_direct_child(root, directory)?;
    let executable = directory.join("ollama.exe");
    let receipt = directory.join(".archive-sha256");
    if !executable.is_file() || !receipt.is_file() {
        return Err(LocalError::Invalid(format!(
            "Incomplete legacy runtime installation at {}. Phonton left it untouched; inspect and move it out of the managed runtime root before retrying setup.",
            directory.display()
        )));
    }
    if is_reparse_point(&executable)? || is_reparse_point(&receipt)? {
        return Err(LocalError::Invalid(
            "Managed runtime executable or receipt is a reparse point; inspect it before setup."
                .into(),
        ));
    }
    if tokio::fs::read_to_string(&receipt).await?.trim() != archive_hash
        || sha256_file(&executable).await? != executable_hash
        || tree_sha256(directory).await? != tree_hash
    {
        return Err(LocalError::Invalid(format!(
            "Managed runtime receipt or file hashes differ at {}. Phonton left it untouched; inspect and move it out of the managed runtime root before retrying setup.",
            directory.display()
        )));
    }
    Ok(Some(executable))
}

#[cfg(all(windows, target_arch = "x86_64"))]
async fn publish_verified_archive(
    root: &Path,
    archive: &Path,
    directory: &Path,
    archive_hash: &str,
    executable_hash: &str,
    tree_hash: &str,
) -> Result<std::path::PathBuf> {
    use std::io::Write;
    use std::time::Duration;
    let final_name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| LocalError::Invalid("Managed runtime directory name is invalid".into()))?;
    cleanup_owned_stages(root, final_name, archive_hash)?;
    let stage = create_owned_stage(root, final_name, archive_hash)?;
    checked_direct_child(root, &stage)?;
    let mut command = tokio::process::Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", "Expand-Archive -LiteralPath $env:PHONTON_RUNTIME_ARCHIVE -DestinationPath $env:PHONTON_RUNTIME_DEST -ErrorAction Stop"])
        .env("PHONTON_RUNTIME_ARCHIVE", archive).env("PHONTON_RUNTIME_DEST", &stage)
        .creation_flags(0x08000000).kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(600), command.output())
        .await
        .map_err(|_| {
            LocalError::Invalid(
                "Runtime extraction timed out; retry setup to recover the owned staging directory"
                    .into(),
            )
        })??;
    let staged_executable = stage.join("ollama.exe");
    if !result.status.success()
        || !staged_executable.is_file()
        || is_reparse_point(&staged_executable)?
        || sha256_file(&staged_executable).await? != executable_hash
    {
        return Err(LocalError::Invalid(format!(
            "Runtime extraction or executable hash verification failed: {}. Retry setup to recover the owned staging directory.",
            String::from_utf8_lossy(&result.stderr)
        )));
    }
    if std::fs::read_to_string(stage.join(".phonton-stage"))? != stage_marker(archive_hash) {
        return Err(LocalError::Invalid(
            "Runtime extraction changed its staging ownership marker; inspect it before retrying."
                .into(),
        ));
    }
    reject_tree_reparse_points(&stage)?;
    if tree_sha256(&stage).await? != tree_hash {
        return Err(LocalError::Invalid("Runtime extraction did not match the pinned file tree; retry setup after inspecting the owned stage.".into()));
    }
    let mut receipt = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stage.join(".archive-sha256"))?;
    receipt.write_all(archive_hash.as_bytes())?;
    receipt.sync_all()?;
    drop(receipt);
    if std::fs::symlink_metadata(directory).is_ok() {
        return Err(LocalError::Invalid(format!("Managed runtime final directory appeared during extraction: {}. Phonton preserved both paths for inspection.", directory.display())));
    }
    checked_direct_child(root, &stage)?;
    move_path_no_replace(&stage, directory)?;
    Ok(directory.join("ollama.exe"))
}

#[cfg(all(windows, target_arch = "x86_64"))]
async fn download_runtime_archive(
    client: &reqwest::Client,
    url: &str,
    archive: &Path,
    archive_hash: &str,
    archive_bytes: u64,
    selected: Option<OwnedDownloadStage>,
    progress: &mut impl FnMut(DownloadProgress),
) -> Result<()> {
    use reqwest::header::{
        ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, RANGE,
    };
    use reqwest::StatusCode;
    use std::io::{Seek, SeekFrom, Write};

    if archive_bytes == 0 {
        return Err(LocalError::Invalid(
            "Pinned runtime archive size must be positive".into(),
        ));
    }
    let root = archive.parent().ok_or_else(|| {
        LocalError::Invalid("Managed runtime archive has no parent directory".into())
    })?;
    let archive_name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| LocalError::Invalid("Managed runtime archive name is invalid".into()))?;
    let start = selected.as_ref().map_or(0, |stage| stage.received);
    let mut response = if start == archive_bytes {
        None
    } else {
        let mut request = client.get(url).header(ACCEPT_ENCODING, "identity");
        if start > 0 {
            request = request.header(RANGE, format!("bytes={start}-"));
        }
        let response = request.send().await?;
        if let Some(encoding) = response.headers().get(CONTENT_ENCODING) {
            if !encoding
                .to_str()
                .is_ok_and(|value| value.eq_ignore_ascii_case("identity"))
            {
                return Err(LocalError::Invalid(
                    "Runtime archive response used an unexpected content encoding".into(),
                ));
            }
        }
        let content_length = response
            .headers()
            .get(CONTENT_LENGTH)
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| {
                        LocalError::Invalid("Runtime archive response has an invalid length".into())
                    })
            })
            .transpose()?;
        if response.status() == StatusCode::PARTIAL_CONTENT && start > 0 {
            let expected_range = format!("bytes {start}-{}/{}", archive_bytes - 1, archive_bytes);
            if response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                != Some(expected_range.as_str())
                || content_length.is_some_and(|length| length != archive_bytes - start)
            {
                return Err(LocalError::Invalid(
                    "Runtime archive range response did not match the pinned remaining bytes; the partial download was preserved"
                        .into(),
                ));
            }
        } else if response.status() == StatusCode::OK {
            if start > 0 {
                return Err(LocalError::Invalid(
                    "Runtime origin ignored the resume Range request (HTTP 200); Phonton preserved the owned partial download."
                        .into(),
                ));
            }
            if content_length.is_some_and(|length| length != archive_bytes) {
                return Err(LocalError::Invalid(
                    "Runtime archive full response did not match the pinned size; the partial download was preserved"
                        .into(),
                ));
            }
        } else {
            return Err(LocalError::Invalid(format!(
                "Runtime download: HTTP {}; the partial download was preserved",
                response.status()
            )));
        }
        Some(response)
    };

    let mut stage = match selected {
        Some(stage) => stage,
        None => create_download_stage(root, archive_name, archive_hash)?,
    };
    stage.file.seek(SeekFrom::Start(start))?;
    let mut received = start;
    let mut reported = received;
    let mut synced = received;
    if start > 0 && response.is_some() {
        progress(DownloadProgress {
            status: "Resuming Ollama runtime download".into(),
            completed: Some(received),
            total: Some(archive_bytes),
            digest: Some(format!("sha256:{archive_hash}")),
        });
    }
    while let Some(response) = response.as_mut() {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| LocalError::Invalid("Runtime archive size overflowed".into()))?;
        if received > archive_bytes {
            return Err(LocalError::Invalid(
                "Runtime archive exceeds pinned size; the partial download was preserved".into(),
            ));
        }
        // Synchronous writes leave no detached Tokio file operation behind if
        // the caller cancels at the next response await and releases its lease.
        stage.file.write_all(&chunk)?;
        if received.saturating_sub(synced) >= 32 * 1024 * 1024 {
            stage.file.sync_data()?;
            synced = received;
        }
        if received.saturating_sub(reported) >= 1024 * 1024 || received == archive_bytes {
            progress(DownloadProgress {
                status: "Downloading Ollama runtime".into(),
                completed: Some(received),
                total: Some(archive_bytes),
                digest: Some(format!("sha256:{archive_hash}")),
            });
            reported = received;
        }
    }
    drop(response);
    stage.file.sync_all()?;
    if received != archive_bytes || stage.file.metadata()?.len() != archive_bytes {
        return Err(LocalError::Invalid(
            "Runtime archive is incomplete; retry setup to resume its owned download stage.".into(),
        ));
    }
    let partial = stage.partial.clone();
    let stage_path = stage.path.clone();
    drop(stage.file);
    let (locked_partial, partial_hash) = hash_locked_archive(&partial).await?;
    if partial_hash != archive_hash {
        return Err(LocalError::Invalid(format!(
            "Downloaded runtime archive checksum mismatch in {}. The owned download stage was preserved for inspection.",
            stage_path.display()
        )));
    }
    drop(locked_partial);
    checked_direct_child(root, &stage_path)?;
    reject_tree_reparse_points(&stage_path)?;
    move_path_no_replace(&partial, archive)?;
    let marker = stage_path.join(".phonton-stage");
    let remaining = std::fs::read_dir(&stage_path)?.collect::<std::io::Result<Vec<_>>>()?;
    if remaining.len() == 1
        && !is_reparse_point(&marker)?
        && std::fs::read_to_string(&marker)? == stage_marker(archive_hash)
    {
        std::fs::remove_file(marker)?;
        std::fs::remove_dir(stage_path)?;
    }
    Ok(())
}

/// Install a hash-checked official portable Windows x64 runtime into a managed
/// directory. A runtime-root lease serializes installations; the caller owns
/// cancellation.
pub async fn install(
    root: &Path,
    progress: impl FnMut(DownloadProgress),
) -> Result<std::path::PathBuf> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        install_windows(root, progress).await
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        let _ = (root, progress);
        Err(LocalError::Invalid("Managed runtime installation supports Windows x64. Install Ollama from https://ollama.com/download on this platform, then connect its loopback endpoint.".into()))
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
async fn install_windows(
    root: &Path,
    mut progress: impl FnMut(DownloadProgress),
) -> Result<std::path::PathBuf> {
    use std::time::Duration;
    reject_nonlocal_runtime_root(root)?;
    let existing = root
        .ancestors()
        .find(|candidate| candidate.is_dir())
        .ok_or_else(|| LocalError::Invalid("Managed runtime drive is unavailable".into()))?;
    crate::disk::require_local_drive_directory(existing)?;
    // Refuse a fresh install before creating its root or persistent lease file.
    // An already installed runtime may still be restarted on a low-space drive.
    if fresh_runtime_root(root)? {
        let free = crate::disk::available_directory_bytes(existing).map_err(|error| {
            LocalError::Invalid(format!(
                "Could not measure runtime installation disk space: {error}"
            ))
        })?;
        require_install_reserve(free, 0)?;
    }
    tokio::fs::create_dir_all(root).await?;
    let root = local_runtime_root(&tokio::fs::canonicalize(root).await?)?;
    crate::disk::require_local_drive_directory(&root)?;
    let _install_lease = crate::storage::acquire(&root.join("runtime-install-state"))?;
    // A cancelled await does not stop an already-running blocking hash worker.
    // Let its locked archive handle close before an owned stage is retired.
    wait_for_archive_hashes().await?;
    let directory = root.join(format!("ollama-{RUNTIME_VERSION}"));
    progress(DownloadProgress {
        status: "Checking managed runtime files".into(),
        ..Default::default()
    });
    if let Some(executable) = installed_executable(
        &root,
        &directory,
        ARCHIVE_SHA256,
        EXECUTABLE_SHA256,
        TREE_SHA256,
    )
    .await?
    {
        return Ok(executable);
    }
    cleanup_owned_stages(&root, &format!("ollama-{RUNTIME_VERSION}"), ARCHIVE_SHA256)?;
    let archive = root.join(format!("ollama-{RUNTIME_VERSION}.zip"));
    let archive_present = match std::fs::symlink_metadata(&archive) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.is_file() && !is_reparse_point(&archive)? => true,
        Ok(_) => {
            return Err(LocalError::Invalid(format!(
                "Managed runtime archive is not a regular file; Phonton left it untouched: {}",
                archive.display()
            )))
        }
    };
    let verified_archive = if archive_present {
        progress(DownloadProgress {
            status: "Verifying official runtime archive".into(),
            ..Default::default()
        });
        Some(verified_archive_for_credit(&archive, ARCHIVE_BYTES, ARCHIVE_SHA256).await?)
    } else {
        None
    };
    let download_stage = if archive_present {
        None
    } else {
        find_owned_download_stage(
            &root,
            &format!("ollama-{RUNTIME_VERSION}.zip"),
            ARCHIVE_SHA256,
            ARCHIVE_BYTES,
        )?
    };
    // An existing verified runtime can restart with low disk. The reserve is
    // needed only when downloading or extracting a fresh installation.
    // Measure the canonical runtime directory's volume, not the working drive.
    let free = crate::disk::available_directory_bytes(&root).map_err(|error| {
        LocalError::Invalid(format!(
            "Could not measure runtime installation disk space: {error}"
        ))
    })?;
    require_install_reserve(
        free,
        download_stage
            .as_ref()
            .map_or(0, |stage| stage.allocated)
            .max(
                verified_archive
                    .as_ref()
                    .map_or(0, |(_, allocated)| *allocated),
            ),
    )?;
    if !archive_present {
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(45))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let trusted = attempt.url().scheme() == "https"
                    && matches!(
                        attempt.url().host_str(),
                        Some(
                            "github.com"
                                | "release-assets.githubusercontent.com"
                                | "objects.githubusercontent.com"
                        )
                    );
                if trusted && attempt.previous().len() < 5 {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .timeout(Duration::from_secs(3600))
            .build()?;
        download_runtime_archive(
            &client,
            ARCHIVE_URL,
            &archive,
            ARCHIVE_SHA256,
            ARCHIVE_BYTES,
            download_stage,
            &mut progress,
        )
        .await?;
    }
    let _archive_handle = if let Some((locked, _)) = verified_archive {
        locked
    } else {
        progress(DownloadProgress {
            status: "Verifying official runtime archive".into(),
            ..Default::default()
        });
        verified_archive_for_credit(&archive, ARCHIVE_BYTES, ARCHIVE_SHA256)
            .await?
            .0
    };
    progress(DownloadProgress {
        status: "Extracting and verifying pinned runtime files".into(),
        ..Default::default()
    });
    publish_verified_archive(
        &root,
        &archive,
        &directory,
        ARCHIVE_SHA256,
        EXECUTABLE_SHA256,
        TREE_SHA256,
    )
    .await
}

#[cfg(windows)]
pub(crate) fn loopback_listener_owner(port: u16) -> Result<Option<u32>> {
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows_sys::Win32::Networking::WinSock::AF_INET;

    let mut size = 0u32;
    let mut status = unsafe {
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            u32::from(AF_INET),
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    if status != ERROR_INSUFFICIENT_BUFFER {
        return Err(LocalError::Invalid(format!(
            "Could not inspect the managed runtime listener (Windows error {status})"
        )));
    }
    for _ in 0..4 {
        if size == 0 || size > 16 * 1024 * 1024 {
            return Err(LocalError::Invalid(
                "Managed runtime listener table has an invalid size".into(),
            ));
        }
        // The IP Helper API needs an aligned, writable buffer. u32 storage
        // provides the alignment of its table and row structures.
        let mut buffer = vec![0u32; (size as usize).div_ceil(4)];
        let mut returned = u32::try_from(buffer.len() * 4)
            .map_err(|_| LocalError::Invalid("Runtime listener table is too large".into()))?;
        status = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr().cast(),
                &mut returned,
                0,
                u32::from(AF_INET),
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if status == ERROR_INSUFFICIENT_BUFFER {
            size = returned;
            continue;
        }
        if status != 0 {
            return Err(LocalError::Invalid(format!(
                "Could not inspect the managed runtime listener (Windows error {status})"
            )));
        }
        let header = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table);
        let count = usize::try_from(buffer[0])
            .map_err(|_| LocalError::Invalid("Invalid runtime listener count".into()))?;
        let rows_size = count
            .checked_mul(std::mem::size_of::<MIB_TCPROW_OWNER_PID>())
            .and_then(|n| header.checked_add(n))
            .ok_or_else(|| LocalError::Invalid("Invalid runtime listener table".into()))?;
        if rows_size > returned as usize {
            return Err(LocalError::Invalid(
                "Runtime listener table was truncated".into(),
            ));
        }
        // The returned length above covers every row, and u32 storage aligns
        // the table. IP Helper supplies the count and row layout.
        let rows = unsafe {
            std::slice::from_raw_parts(
                buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(header)
                    .cast::<MIB_TCPROW_OWNER_PID>(),
                count,
            )
        };
        let mut owner = None;
        for row in rows {
            if row.dwLocalAddr == u32::from_ne_bytes([127, 0, 0, 1])
                && u16::from_be(row.dwLocalPort as u16) == port
            {
                if owner.is_some_and(|previous| previous != row.dwOwningPid) {
                    return Err(LocalError::Invalid(
                        "Multiple processes own the managed runtime listener".into(),
                    ));
                }
                owner = Some(row.dwOwningPid);
            }
        }
        return Ok(owner);
    }
    Err(LocalError::Invalid(
        "Managed runtime listener table kept changing".into(),
    ))
}

#[cfg(not(windows))]
pub(crate) fn loopback_listener_owner(_port: u16) -> Result<Option<u32>> {
    Err(LocalError::Invalid(
        "Managed runtime listener verification currently supports Windows".into(),
    ))
}

/// Whether the default managed loopback endpoint is currently unavailable.
/// This is a pre-download observation; startup checks ownership again.
pub fn default_listener_unavailable() -> Result<bool> {
    listener_unavailable(11434)
}

fn listener_unavailable(port: u16) -> Result<bool> {
    if loopback_listener_owner(port)?.is_some() {
        return Ok(true);
    }
    // A wildcard listener also blocks 127.0.0.1 but has no exact loopback row
    // in the TCP owner table. Try a short reservation to catch that case.
    Ok(std::net::TcpListener::bind(("127.0.0.1", port)).is_err())
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn managed_start_paths(root: &Path) -> Result<std::fs::File> {
    let models = root.join("models");
    std::fs::create_dir_all(&models)?;
    checked_direct_child(root, &models)?;
    if !std::fs::symlink_metadata(&models)?.is_dir() {
        return Err(LocalError::Invalid(
            "Managed runtime model store is not a directory".into(),
        ));
    }

    let log_path = root.join("runtime.log");
    match std::fs::symlink_metadata(&log_path) {
        Ok(metadata) => {
            if !metadata.is_file() || is_reparse_point(&log_path)? {
                return Err(LocalError::Invalid(format!(
                    "Managed runtime log is not a regular local file: {}",
                    log_path.display()
                )));
            }
            checked_direct_child(root, &log_path)?;
            Ok(std::fs::OpenOptions::new().append(true).open(log_path)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(log_path)?)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
fn managed_start_paths(_root: &Path) -> Result<std::fs::File> {
    Err(LocalError::Invalid(
        "Managed runtime startup currently supports Windows x64".into(),
    ))
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn canonical_start_root(root: &Path) -> Result<std::path::PathBuf> {
    reject_nonlocal_runtime_root(root)?;
    let root = local_runtime_root(&std::fs::canonicalize(root)?)?;
    crate::disk::require_local_drive_directory(&root)?;
    Ok(root)
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
fn canonical_start_root(_root: &Path) -> Result<std::path::PathBuf> {
    Err(LocalError::Invalid(
        "Managed runtime startup currently supports Windows x64".into(),
    ))
}

/// Start only the managed executable, on loopback with one loaded model and cloud
/// features disabled. Existing daemons are checked by the caller before startup.
/// The returned version was read while this child owned the loopback listener.
pub async fn start(executable: &Path, root: &Path) -> Result<String> {
    use std::process::Stdio;
    let root = canonical_start_root(root)?;
    let log = managed_start_paths(&root)?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("serve")
        .env("OLLAMA_HOST", "127.0.0.1:11434")
        .env("OLLAMA_MODELS", root.join("models"))
        .env("OLLAMA_NO_CLOUD", "1")
        .env("OLLAMA_NUM_PARALLEL", "1")
        .env("OLLAMA_MAX_LOADED_MODELS", "1")
        .env("OLLAMA_CONTEXT_LENGTH", "4096")
        .env("OLLAMA_KEEP_ALIVE", "2m")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    start_command(command, &root, 11434).await
}

/// The managed runtime outlives this process. Windows children inherit every
/// inheritable handle, so without this a piped `phonton models setup` never
/// reaches EOF while the runtime holds the caller's stdout/stderr pipe. Child
/// stdio set through `Stdio::inherit` is duplicated by std and unaffected.
#[cfg(windows)]
fn stop_std_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    for handle in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        if !handle.is_null() {
            // SAFETY: the handle belongs to this process; a failure only
            // leaves inheritance as it was.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

async fn start_command(
    mut command: tokio::process::Command,
    root: &Path,
    port: u16,
) -> Result<String> {
    use std::time::Duration;
    struct StartupChild {
        child: tokio::process::Child,
        ready: bool,
    }
    impl Drop for StartupChild {
        fn drop(&mut self) {
            if !self.ready {
                let _ = self.child.start_kill();
            }
        }
    }
    // Install releases this same canonical runtime-root lease before calling
    // start. Holding it through readiness keeps competing Phonton setups from
    // spawning two managed children after installation completes.
    let root = canonical_start_root(root)?;
    let _start_lease = crate::storage::acquire(&root.join("runtime-install-state"))?;
    #[cfg(windows)]
    stop_std_handle_inheritance();
    let mut startup = StartupChild {
        child: command.spawn()?,
        ready: false,
    };
    let child_pid = startup
        .child
        .id()
        .ok_or_else(|| LocalError::Invalid("Managed runtime child has no process ID".into()))?;
    let runtime = crate::runtime::LocalRuntime::new(&format!("http://127.0.0.1:{port}"))?;
    for _ in 0..30 {
        if let Some(status) = startup.child.try_wait()? {
            return Err(LocalError::Invalid(format!(
                "Runtime exited ({status}); inspect {}",
                root.join("runtime.log").display()
            )));
        }
        match loopback_listener_owner(port)? {
            Some(owner) if owner != child_pid => {
                return Err(LocalError::Invalid(format!(
                    "Another process owns the managed runtime listener at 127.0.0.1:{port}; managed startup was refused"
                )));
            }
            Some(_) => {
                if let Ok(version) = runtime.version().await {
                    if let Some(status) = startup.child.try_wait()? {
                        return Err(LocalError::Invalid(format!(
                            "Runtime exited ({status}); inspect {}",
                            root.join("runtime.log").display()
                        )));
                    }
                    if loopback_listener_owner(port)? != Some(child_pid) {
                        return Err(LocalError::Invalid(
                            "Managed runtime listener changed during readiness; managed startup was refused".into(),
                        ));
                    }
                    #[cfg(all(windows, target_arch = "x86_64"))]
                    crate::managed_store::record_launch(&root, port, child_pid, &version)?;
                    startup.ready = true;
                    return Ok(version);
                }
            }
            None => {}
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // This is our own child, not a process found by name or port.
    let _ = startup.child.kill().await;
    Err(LocalError::Invalid(
        "Managed runtime did not become ready; inspect runtime.log".into(),
    ))
}

#[cfg(all(test, windows, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    fn fake_start_command(port: u16, mode: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "provision::tests::managed_runtime_test_child",
                "--ignored",
            ])
            .env("PHONTON_START_TEST_PORT", port.to_string())
            .env("PHONTON_START_TEST_MODE", mode)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(0x08000000);
        command
    }

    #[test]
    #[ignore = "test-process helper; selected only by the managed runtime startup tests"]
    fn managed_runtime_test_child() {
        let Ok(port) = std::env::var("PHONTON_START_TEST_PORT") else {
            return;
        };
        if std::env::var("PHONTON_START_TEST_MODE").as_deref() == Ok("sleep") {
            std::thread::sleep(std::time::Duration::from_secs(5));
            return;
        }
        let listener = TcpListener::bind(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request).unwrap();
        let body = r#"{"version":"mock-1"}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        stream.flush().unwrap();
        // Keep the listener alive until the parent has checked its owner again.
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    #[test]
    fn listener_owner_identifies_the_exact_loopback_process() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(
            loopback_listener_owner(port).unwrap(),
            Some(std::process::id())
        );
        assert!(listener_unavailable(port).unwrap());
    }

    #[test]
    fn managed_start_refuses_redirected_or_non_file_paths() {
        let root = tempfile::tempdir().unwrap();
        let models = root.path().join("models");
        std::fs::write(&models, b"not a directory").unwrap();
        assert!(managed_start_paths(root.path()).is_err());
        std::fs::remove_file(&models).unwrap();

        let log = root.path().join("runtime.log");
        std::fs::create_dir(&log).unwrap();
        assert!(managed_start_paths(root.path()).is_err());
        std::fs::remove_dir(&log).unwrap();

        let elsewhere = tempfile::tempdir().unwrap();
        if std::os::windows::fs::symlink_dir(elsewhere.path(), &models).is_ok() {
            assert!(managed_start_paths(root.path()).is_err());
            std::fs::remove_dir(&models).unwrap();
        }
        if std::os::windows::fs::symlink_file(elsewhere.path().join("log"), &log).is_ok() {
            assert!(managed_start_paths(root.path()).is_err());
            std::fs::remove_file(&log).unwrap();
        }
    }

    #[tokio::test]
    async fn managed_start_rejects_a_different_loopback_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = tempfile::tempdir().unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            start_command(fake_start_command(port, "sleep"), root.path(), port),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("Another process owns"));
        drop(listener);
    }

    #[tokio::test]
    async fn managed_start_accepts_only_its_child_owned_listener() {
        let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let root = tempfile::tempdir().unwrap();
        let version = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            start_command(fake_start_command(port, "serve"), root.path(), port),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(version, "mock-1");
    }

    fn fixture(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let source = root.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("ollama.exe"), b"tiny fixture executable").unwrap();
        std::fs::write(source.join("cpu.dll"), b"tiny fixture library").unwrap();
        let archive = root.join("fixture.zip");
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Compress-Archive -Path (Join-Path $env:PHONTON_FIXTURE_DIR '*') -DestinationPath $env:PHONTON_FIXTURE_ZIP -ErrorAction Stop"])
            .env("PHONTON_FIXTURE_DIR", &source)
            .env("PHONTON_FIXTURE_ZIP", &archive)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (source, archive)
    }

    async fn fixture_hashes(source: &Path, archive: &Path) -> (String, String, String) {
        (
            sha256_file(archive).await.unwrap(),
            sha256_file(&source.join("ollama.exe")).await.unwrap(),
            tree_sha256(source).await.unwrap(),
        )
    }

    fn archive_digest(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(bytes))
    }

    fn archive_test_server(response: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/runtime.zip", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.ends_with(b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.len() > 32 * 1024 || request.windows(4).any(|bytes| bytes == b"\r\n\r\n")
                {
                    break;
                }
            }
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            String::from_utf8(request).unwrap()
        });
        (url, handle)
    }

    fn archive_http_response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response =
            format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n").into_bytes();
        response.extend_from_slice(body);
        response
    }

    async fn test_archive_download(root: &Path, url: &str, bytes: &[u8]) -> Result<()> {
        let name = "ollama-fixture.zip";
        let archive = root.join(name);
        let digest = archive_digest(bytes);
        let selected = find_owned_download_stage(root, name, &digest, bytes.len() as u64)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        download_runtime_archive(
            &client,
            url,
            &archive,
            &digest,
            bytes.len() as u64,
            selected,
            &mut |_| {},
        )
        .await
    }

    #[test]
    fn runtime_root_rejects_unc_and_preserves_local_utf16() {
        assert!(reject_nonlocal_runtime_root(Path::new(r"\\server\share\runtime")).is_err());
        assert!(local_runtime_root(Path::new(r"\\?\UNC\server\share\runtime")).is_err());
        let temp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();
        let normalized = local_runtime_root(&canonical).unwrap();
        assert_eq!(std::fs::canonicalize(normalized).unwrap(), canonical);

        let unusual = std::path::PathBuf::from(std::ffi::OsString::from_wide(&[
            b'\\' as u16,
            b'\\' as u16,
            b'?' as u16,
            b'\\' as u16,
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
        ]));
        let normalized = local_runtime_root(&unusual).unwrap();
        assert_eq!(
            normalized.as_os_str().encode_wide().collect::<Vec<_>>(),
            [b'C' as u16, b':' as u16, b'\\' as u16, 0xd800]
        );
    }

    #[test]
    fn explicit_stage_cleanup_preserves_a_legacy_partial() {
        let temp = tempfile::tempdir().unwrap();
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", "archive").unwrap();
        std::fs::write(stage.join("download.partial"), b"interrupted bytes").unwrap();
        let legacy = temp.path().join("ollama-fixture.zip.partial");
        std::fs::write(&legacy, b"unknown bytes").unwrap();
        cleanup_owned_stages(temp.path(), "ollama-fixture.zip", "archive").unwrap();
        assert!(!stage.exists());
        assert_eq!(std::fs::read(legacy).unwrap(), b"unknown bytes");
    }

    #[tokio::test]
    async fn fresh_archive_download_publishes_only_verified_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"0123456789abcdef";
        let response = archive_http_response("200 OK", "Content-Length: 16\r\n", bytes);
        let (url, server) = archive_test_server(response);
        test_archive_download(temp.path(), &url, bytes)
            .await
            .unwrap();
        assert!(!server
            .join()
            .unwrap()
            .to_ascii_lowercase()
            .contains("range:"));
        assert_eq!(
            std::fs::read(temp.path().join("ollama-fixture.zip")).unwrap(),
            bytes
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn interrupted_owned_archive_resumes_exact_remaining_range() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"0123456789abcdef";
        let digest = archive_digest(bytes);
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", &digest).unwrap();
        std::fs::write(stage.join("download.partial"), &bytes[..6]).unwrap();
        let foreign = temp.path().join("ollama-fixture.zip.staging-foreign");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("keep.txt"), b"foreign data").unwrap();
        let response = archive_http_response(
            "206 Partial Content",
            "Content-Range: bytes 6-15/16\r\nContent-Length: 10\r\n",
            &bytes[6..],
        );
        let (url, server) = archive_test_server(response);
        test_archive_download(temp.path(), &url, bytes)
            .await
            .unwrap();
        assert!(server
            .join()
            .unwrap()
            .to_ascii_lowercase()
            .contains("range: bytes=6-"));
        assert_eq!(
            std::fs::read(temp.path().join("ollama-fixture.zip")).unwrap(),
            bytes
        );
        assert!(!stage.exists());
        assert_eq!(
            std::fs::read(foreign.join("keep.txt")).unwrap(),
            b"foreign data"
        );
    }

    #[tokio::test]
    async fn ignored_range_preserves_the_owned_partial() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"0123456789abcdef";
        let digest = archive_digest(bytes);
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", &digest).unwrap();
        let partial = stage.join("download.partial");
        std::fs::write(&partial, &bytes[..6]).unwrap();
        let response = archive_http_response("200 OK", "Content-Length: 16\r\n", bytes);
        let (url, server) = archive_test_server(response);
        let error = test_archive_download(temp.path(), &url, bytes)
            .await
            .unwrap_err();
        assert!(server
            .join()
            .unwrap()
            .to_ascii_lowercase()
            .contains("range: bytes=6-"));
        assert!(error.to_string().contains("ignored the resume Range"));
        assert_eq!(std::fs::read(partial).unwrap(), &bytes[..6]);
        assert!(!temp.path().join("ollama-fixture.zip").exists());
    }

    #[tokio::test]
    async fn invalid_fresh_full_response_is_rejected_before_stage_creation() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"0123456789abcdef";
        let response = archive_http_response("200 OK", "Content-Length: 15\r\n", bytes);
        let (url, server) = archive_test_server(response);
        let error = test_archive_download(temp.path(), &url, bytes)
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("full response"));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn lengthless_full_response_does_not_discard_a_resumable_partial() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"0123456789abcdef";
        let digest = archive_digest(bytes);
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", &digest).unwrap();
        let partial = stage.join("download.partial");
        std::fs::write(&partial, &bytes[..6]).unwrap();
        let response = archive_http_response("200 OK", "", bytes);
        let (url, server) = archive_test_server(response);
        let error = test_archive_download(temp.path(), &url, bytes)
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("ignored the resume Range"));
        assert_eq!(std::fs::read(partial).unwrap(), &bytes[..6]);
        assert!(!temp.path().join("ollama-fixture.zip").exists());
    }

    #[tokio::test]
    async fn malformed_range_response_preserves_owned_partial() {
        let bytes = b"0123456789abcdef";
        for headers in [
            "Content-Range: bytes 5-15/16\r\nContent-Length: 10\r\n",
            "Content-Range: bytes 6-15/16\r\nContent-Length: 9\r\n",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let digest = archive_digest(bytes);
            let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", &digest).unwrap();
            let partial = stage.join("download.partial");
            std::fs::write(&partial, &bytes[..6]).unwrap();
            let response = archive_http_response("206 Partial Content", headers, &bytes[6..]);
            let (url, server) = archive_test_server(response);
            let error = test_archive_download(temp.path(), &url, bytes)
                .await
                .unwrap_err();
            server.join().unwrap();
            assert!(error.to_string().contains("range response"));
            assert_eq!(std::fs::read(partial).unwrap(), &bytes[..6]);
            assert!(!temp.path().join("ollama-fixture.zip").exists());
        }
    }

    #[tokio::test]
    async fn short_range_body_preserves_progress_for_retry() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"0123456789abcdef";
        let digest = archive_digest(bytes);
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", &digest).unwrap();
        let partial = stage.join("download.partial");
        std::fs::write(&partial, &bytes[..6]).unwrap();
        let response = archive_http_response(
            "206 Partial Content",
            "Content-Range: bytes 6-15/16\r\nContent-Length: 10\r\n",
            &bytes[6..8],
        );
        let (url, server) = archive_test_server(response);
        assert!(test_archive_download(temp.path(), &url, bytes)
            .await
            .is_err());
        server.join().unwrap();
        let saved = std::fs::read(partial).unwrap();
        assert!(saved.starts_with(&bytes[..6]));
        assert!(saved.len() < bytes.len());
        assert!(!temp.path().join("ollama-fixture.zip").exists());
    }

    #[tokio::test]
    async fn complete_owned_partial_is_hashed_without_network() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"0123456789abcdef";
        let digest = archive_digest(bytes);
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", &digest).unwrap();
        std::fs::write(stage.join("download.partial"), bytes).unwrap();
        test_archive_download(temp.path(), "http://127.0.0.1:1/not-listening", bytes)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("ollama-fixture.zip")).unwrap(),
            bytes
        );
        assert!(!stage.exists());
    }

    #[tokio::test]
    async fn wrong_completed_partial_is_preserved_without_publishing() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"0123456789abcdef";
        let digest = archive_digest(bytes);
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", &digest).unwrap();
        let partial = stage.join("download.partial");
        std::fs::write(&partial, b"WRONG!6789abcdef").unwrap();
        let error = test_archive_download(temp.path(), "http://127.0.0.1:1/not-listening", bytes)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(std::fs::read(partial).unwrap(), b"WRONG!6789abcdef");
        assert!(!temp.path().join("ollama-fixture.zip").exists());
    }

    #[test]
    fn ambiguous_owned_download_stages_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        for _ in 0..2 {
            let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", "archive").unwrap();
            std::fs::write(stage.join("download.partial"), b"some bytes").unwrap();
        }
        assert!(
            find_owned_download_stage(temp.path(), "ollama-fixture.zip", "archive", 100)
                .unwrap_err()
                .to_string()
                .contains("Multiple owned")
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
    }

    #[test]
    fn marked_stage_with_foreign_content_is_not_selected() {
        let temp = tempfile::tempdir().unwrap();
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", "archive").unwrap();
        std::fs::write(stage.join("download.partial"), b"some bytes").unwrap();
        std::fs::write(stage.join("keep.txt"), b"foreign data").unwrap();
        assert!(
            find_owned_download_stage(temp.path(), "ollama-fixture.zip", "archive", 100)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            std::fs::read(stage.join("keep.txt")).unwrap(),
            b"foreign data"
        );
    }

    #[test]
    fn linked_partial_is_never_selected_or_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", "archive").unwrap();
        let target = temp.path().join("foreign.zip");
        std::fs::write(&target, b"foreign bytes").unwrap();
        let partial = stage.join("download.partial");
        match std::os::windows::fs::symlink_file(&target, &partial) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("could not create fixture link: {error}"),
        }
        assert!(
            find_owned_download_stage(temp.path(), "ollama-fixture.zip", "archive", 100)
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"foreign bytes");
        assert!(partial.exists());
    }

    #[test]
    fn hardlinked_partial_is_preserved_without_resuming() {
        let temp = tempfile::tempdir().unwrap();
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", "archive").unwrap();
        let target = temp.path().join("foreign.zip");
        std::fs::write(&target, b"foreign bytes").unwrap();
        let partial = stage.join("download.partial");
        std::fs::hard_link(&target, &partial).unwrap();
        assert!(
            find_owned_download_stage(temp.path(), "ollama-fixture.zip", "archive", 100)
                .unwrap_err()
                .to_string()
                .contains("not a unique regular file")
        );
        assert_eq!(std::fs::read(target).unwrap(), b"foreign bytes");
        assert_eq!(std::fs::read(partial).unwrap(), b"foreign bytes");
    }

    #[test]
    fn reserve_credits_only_allocated_partial_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", "archive").unwrap();
        let partial = stage.join("download.partial");
        let file = std::fs::File::create(&partial).unwrap();
        file.set_len(1024 * 1024).unwrap();
        drop(file);
        let selected =
            find_owned_download_stage(temp.path(), "ollama-fixture.zip", "archive", 1024 * 1024)
                .unwrap()
                .unwrap();
        assert!(selected.allocated <= selected.received);
        assert!(require_install_reserve(
            MIN_INSTALL_FREE_BYTES - selected.allocated,
            selected.allocated
        )
        .is_ok());
        assert!(require_install_reserve(
            MIN_INSTALL_FREE_BYTES - selected.allocated - 1,
            selected.allocated
        )
        .is_err());
    }

    #[tokio::test]
    async fn completed_verified_archive_credits_its_allocation_before_extraction() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = b"verified archive bytes";
        let archive = temp.path().join("ollama-fixture.zip");
        std::fs::write(&archive, bytes).unwrap();
        let (locked, allocated) =
            verified_archive_for_credit(&archive, bytes.len() as u64, &archive_digest(bytes))
                .await
                .unwrap();
        assert!(allocated > 0);
        assert!(allocated <= bytes.len() as u64);
        assert!(require_install_reserve(MIN_INSTALL_FREE_BYTES - allocated, allocated).is_ok());
        assert!(
            require_install_reserve(MIN_INSTALL_FREE_BYTES - allocated - 1, allocated).is_err()
        );
        assert!(std::fs::OpenOptions::new()
            .write(true)
            .open(&archive)
            .is_err());
        drop(locked);
    }

    #[tokio::test]
    async fn changed_completed_archive_cannot_gain_reserve_credit() {
        let temp = tempfile::tempdir().unwrap();
        let expected = b"verified archive bytes";
        let changed = b"altered  archive bytes";
        assert_eq!(expected.len(), changed.len());
        let archive = temp.path().join("ollama-fixture.zip");
        std::fs::write(&archive, changed).unwrap();
        assert!(verified_archive_for_credit(
            &archive,
            expected.len() as u64,
            &archive_digest(expected)
        )
        .await
        .is_err());
        assert_eq!(std::fs::read(&archive).unwrap(), changed);
    }

    #[tokio::test]
    async fn publishes_only_a_complete_verified_stage_and_reuses_it() {
        let temp = tempfile::tempdir().unwrap();
        let (source, archive) = fixture(temp.path());
        let (archive_hash, executable_hash, tree_hash) = fixture_hashes(&source, &archive).await;
        let final_dir = temp.path().join("ollama-fixture");
        let locked = hash_locked_archive(&archive).await.unwrap();
        assert_eq!(locked.1, archive_hash);
        assert!(std::fs::OpenOptions::new()
            .write(true)
            .open(&archive)
            .is_err());
        let executable = publish_verified_archive(
            temp.path(),
            &archive,
            &final_dir,
            &archive_hash,
            &executable_hash,
            &tree_hash,
        )
        .await
        .unwrap();
        drop(locked);
        assert_eq!(
            std::fs::read(&executable).unwrap(),
            b"tiny fixture executable"
        );
        assert_eq!(
            std::fs::read_to_string(final_dir.join(".archive-sha256")).unwrap(),
            archive_hash
        );
        assert_eq!(tree_sha256(&final_dir).await.unwrap(), tree_hash);
        assert_eq!(
            installed_executable(
                temp.path(),
                &final_dir,
                &archive_hash,
                &executable_hash,
                &tree_hash
            )
            .await
            .unwrap(),
            Some(executable)
        );
    }

    #[tokio::test]
    async fn interrupted_owned_stage_is_retired_but_unknown_stage_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let (source, archive) = fixture(temp.path());
        let (archive_hash, executable_hash, tree_hash) = fixture_hashes(&source, &archive).await;
        let final_dir = temp.path().join("ollama-fixture");
        let owned = create_owned_stage(temp.path(), "ollama-fixture", &archive_hash).unwrap();
        std::fs::write(owned.join("partial.dll"), b"incomplete").unwrap();
        let unknown = temp.path().join("ollama-fixture.staging-unknown");
        std::fs::create_dir(&unknown).unwrap();
        std::fs::write(unknown.join("keep.txt"), b"user data").unwrap();
        let corrupt = create_owned_stage(temp.path(), "ollama-fixture", &archive_hash).unwrap();
        std::fs::write(corrupt.join(".phonton-stage"), b"unknown marker\n").unwrap();
        std::fs::write(corrupt.join("keep.txt"), b"other user data").unwrap();
        publish_verified_archive(
            temp.path(),
            &archive,
            &final_dir,
            &archive_hash,
            &executable_hash,
            &tree_hash,
        )
        .await
        .unwrap();
        assert!(!owned.exists());
        assert_eq!(
            std::fs::read(unknown.join("keep.txt")).unwrap(),
            b"user data"
        );
        assert_eq!(
            std::fs::read(corrupt.join("keep.txt")).unwrap(),
            b"other user data"
        );
        assert!(final_dir.join("ollama.exe").is_file());
    }

    #[tokio::test]
    async fn failed_verification_leaves_owned_stage_for_safe_retry() {
        let temp = tempfile::tempdir().unwrap();
        let (source, archive) = fixture(temp.path());
        let (archive_hash, executable_hash, tree_hash) = fixture_hashes(&source, &archive).await;
        let final_dir = temp.path().join("ollama-fixture");
        assert!(publish_verified_archive(
            temp.path(),
            &archive,
            &final_dir,
            &archive_hash,
            &"0".repeat(64),
            &tree_hash
        )
        .await
        .is_err());
        assert!(!final_dir.exists());
        let staged = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("ollama-fixture.staging-")
            })
            .unwrap()
            .path();
        assert!(staged.join(".phonton-stage").is_file());
        publish_verified_archive(
            temp.path(),
            &archive,
            &final_dir,
            &archive_hash,
            &executable_hash,
            &tree_hash,
        )
        .await
        .unwrap();
        assert!(!staged.exists());
        assert!(final_dir.join("ollama.exe").is_file());
    }

    #[tokio::test]
    async fn incomplete_or_changed_final_directory_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let final_dir = temp.path().join("ollama-fixture");
        std::fs::create_dir(&final_dir).unwrap();
        std::fs::write(final_dir.join("ollama.exe"), b"user data").unwrap();
        let bytes = std::fs::read(final_dir.join("ollama.exe")).unwrap();
        assert!(
            installed_executable(temp.path(), &final_dir, "archive", "exe", "tree")
                .await
                .unwrap_err()
                .to_string()
                .contains("Incomplete legacy")
        );
        std::fs::write(final_dir.join(".archive-sha256"), b"wrong").unwrap();
        assert!(
            installed_executable(temp.path(), &final_dir, "archive", "exe", "tree")
                .await
                .unwrap_err()
                .to_string()
                .contains("hashes differ")
        );
        assert_eq!(std::fs::read(final_dir.join("ollama.exe")).unwrap(), bytes);
        assert!(std::fs::read_to_string(final_dir.join(".archive-sha256"))
            .unwrap()
            .contains("wrong"));
    }

    #[tokio::test]
    async fn valid_receipt_does_not_hide_executable_or_library_changes() {
        let temp = tempfile::tempdir().unwrap();
        let (source, archive) = fixture(temp.path());
        let (archive_hash, executable_hash, tree_hash) = fixture_hashes(&source, &archive).await;
        let final_dir = temp.path().join("ollama-fixture");
        publish_verified_archive(
            temp.path(),
            &archive,
            &final_dir,
            &archive_hash,
            &executable_hash,
            &tree_hash,
        )
        .await
        .unwrap();
        let executable = final_dir.join("ollama.exe");
        let library = final_dir.join("cpu.dll");
        std::fs::write(&executable, b"changed executable").unwrap();
        assert!(installed_executable(
            temp.path(),
            &final_dir,
            &archive_hash,
            &executable_hash,
            &tree_hash
        )
        .await
        .is_err());
        assert_eq!(std::fs::read(&executable).unwrap(), b"changed executable");
        assert_eq!(
            std::fs::read(final_dir.join(".archive-sha256")).unwrap(),
            archive_hash.as_bytes()
        );
        std::fs::write(&executable, b"tiny fixture executable").unwrap();
        std::fs::write(&library, b"changed library").unwrap();
        assert!(installed_executable(
            temp.path(),
            &final_dir,
            &archive_hash,
            &executable_hash,
            &tree_hash
        )
        .await
        .is_err());
        assert_eq!(std::fs::read(&library).unwrap(), b"changed library");
    }

    #[test]
    fn publish_move_refuses_an_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("owned-stage");
        let to = temp.path().join("final");
        std::fs::create_dir(&from).unwrap();
        std::fs::create_dir(&to).unwrap();
        assert!(move_path_no_replace(&from, &to).is_err());
        assert!(from.is_dir());
        assert!(to.is_dir());
        let source_file = temp.path().join("download.partial");
        let archive_file = temp.path().join("runtime.zip");
        std::fs::write(&source_file, b"new archive").unwrap();
        std::fs::write(&archive_file, b"unknown archive").unwrap();
        assert!(move_path_no_replace(&source_file, &archive_file).is_err());
        assert_eq!(std::fs::read(&source_file).unwrap(), b"new archive");
        assert_eq!(std::fs::read(&archive_file).unwrap(), b"unknown archive");
    }

    #[tokio::test]
    async fn locked_archive_refuses_a_reparse_point() {
        use std::os::windows::fs::symlink_file;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.zip");
        let link = temp.path().join("archive.zip");
        std::fs::write(&target, b"source bytes").unwrap();
        match symlink_file(&target, &link) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("could not create fixture link: {error}"),
        }
        assert!(hash_locked_archive(&link)
            .await
            .unwrap_err()
            .to_string()
            .contains("not a regular file"));
        assert_eq!(std::fs::read(&target).unwrap(), b"source bytes");
    }

    #[tokio::test]
    async fn cancelled_hash_awaiter_waits_for_its_file_handle_before_stage_cleanup() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::Duration;
        let temp = tempfile::tempdir().unwrap();
        let stage = create_owned_stage(temp.path(), "ollama-fixture.zip", "archive").unwrap();
        let partial = stage.join("download.partial");
        std::fs::write(&partial, b"archive bytes").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&partial)
            .unwrap();
        let permit = ArchiveHashPermit::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let awaiter = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _locked = LockedArchive {
                    file,
                    _permit: permit,
                };
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            })
            .await
        });
        started_rx.await.unwrap();
        awaiter.abort();
        assert!(std::fs::OpenOptions::new()
            .write(true)
            .open(&partial)
            .is_err());
        let wait = tokio::spawn(wait_for_archive_hashes());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!wait.is_finished());
        release_tx.send(()).unwrap();
        wait.await.unwrap().unwrap();
        cleanup_owned_stages(temp.path(), "ollama-fixture.zip", "archive").unwrap();
        assert!(!stage.exists());
    }

    #[tokio::test]
    async fn fresh_install_requires_space_before_creating_runtime_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        assert!(fresh_runtime_root(&root).unwrap());
        assert!(require_install_reserve(0, 0).is_err());
        assert!(!root.exists());

        std::fs::create_dir(&root).unwrap();
        assert!(fresh_runtime_root(&root).unwrap());
        assert!(require_install_reserve(MIN_INSTALL_FREE_BYTES - 1, 0).is_err());
        assert!(require_install_reserve(MIN_INSTALL_FREE_BYTES, 0).is_ok());

        std::fs::write(root.join("partial.zip"), b"preserve").unwrap();
        assert!(!fresh_runtime_root(&root).unwrap());
    }

    #[tokio::test]
    async fn runtime_root_lease_blocks_a_second_setup_even_with_a_different_model_state() {
        let temp = tempfile::tempdir().unwrap();
        let held = crate::storage::acquire(&temp.path().join("runtime-install-state")).unwrap();
        let error = install_windows(temp.path(), |_| {}).await.unwrap_err();
        assert!(error.to_string().contains("Another Phonton process"));
        drop(held);
    }

    #[tokio::test]
    #[ignore = "requires PHONTON_RUNTIME_SMOKE_ROOT and PHONTON_RUNTIME_SMOKE_ARCHIVE; extracts the pinned 1.46 GB archive"]
    async fn isolated_official_archive_recovers_an_interrupted_stage() {
        let root = std::path::PathBuf::from(
            std::env::var_os("PHONTON_RUNTIME_SMOKE_ROOT").expect("isolated root required"),
        );
        let source = std::path::PathBuf::from(
            std::env::var_os("PHONTON_RUNTIME_SMOKE_ARCHIVE")
                .expect("pinned source archive required"),
        );
        assert!(root.is_absolute() && !root.exists());
        assert_eq!(sha256_file(&source).await.unwrap(), ARCHIVE_SHA256);
        std::fs::create_dir(&root).unwrap();
        let archive = root.join(format!("ollama-{RUNTIME_VERSION}.zip"));
        std::fs::hard_link(&source, &archive).unwrap();
        let staged =
            create_owned_stage(&root, &format!("ollama-{RUNTIME_VERSION}"), ARCHIVE_SHA256)
                .unwrap();
        std::fs::write(staged.join("partial.dll"), b"interrupted extraction").unwrap();
        let executable = install_windows(&root, |_| {}).await.unwrap();
        assert!(!staged.exists());
        assert_eq!(sha256_file(&executable).await.unwrap(), EXECUTABLE_SHA256);
        assert_eq!(
            tree_sha256(executable.parent().unwrap()).await.unwrap(),
            TREE_SHA256
        );
        assert_eq!(install_windows(&root, |_| {}).await.unwrap(), executable);
        assert_eq!(sha256_file(&source).await.unwrap(), ARCHIVE_SHA256);
    }
}
