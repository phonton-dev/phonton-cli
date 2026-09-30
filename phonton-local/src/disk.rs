//! Free-space measurement and conservative model-download admission.

use crate::{LocalError, Result};
use phonton_types::local::{ModelDownloadAdmission, PreSetupStoragePlan};
use std::path::{Path, PathBuf};

/// Read the Windows volume serial and 128-bit file ID of an existing directory.
/// The pair changes when a drive letter or folder is replaced, even if the
/// canonical path text stays the same.
#[cfg(windows)]
pub fn directory_identity(
    directory: &Path,
) -> Result<phonton_types::local::LocalDirectoryIdentity> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO,
    };

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(directory)?;
    if !file.metadata()?.is_dir() {
        return Err(LocalError::Invalid(
            "Managed runtime identity requires a directory".into(),
        ));
    }
    let mut info = FILE_ID_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if info.FileId.Identifier == [0; 16] {
        return Err(LocalError::Invalid(
            "This local drive did not provide a stable directory identity".into(),
        ));
    }
    Ok(phonton_types::local::LocalDirectoryIdentity {
        volume_serial: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

/// Report that managed directory identity is unavailable on this platform.
#[cfg(not(windows))]
pub fn directory_identity(
    _directory: &Path,
) -> Result<phonton_types::local::LocalDirectoryIdentity> {
    Err(LocalError::Invalid(
        "Managed runtime directory identity is supported on Windows only".into(),
    ))
}

/// Require a fixed or removable local Windows volume for managed runtime
/// files. A mapped network share can still have a drive-letter path.
#[cfg(windows)]
pub fn require_local_drive_directory(directory: &Path) -> Result<()> {
    use std::path::{Component, Prefix};
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

    // GetDriveTypeW returns these Win32 drive-type values.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;

    let canonical = std::fs::canonicalize(directory)?;
    let letter = match canonical.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => letter,
            _ => {
                return Err(LocalError::Invalid(
                    "Managed runtime requires a local Windows drive".into(),
                ))
            }
        },
        _ => {
            return Err(LocalError::Invalid(
                "Managed runtime requires a local Windows drive".into(),
            ))
        }
    };
    let root = [letter as u16, b':' as u16, b'\\' as u16, 0];
    let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
    if !matches!(kind, DRIVE_FIXED | DRIVE_REMOVABLE) {
        return Err(LocalError::Invalid(
            "Managed runtime storage must be on a local fixed or removable drive, not a mapped network drive".into(),
        ));
    }
    Ok(())
}

/// Measure free bytes available to this process on the volume containing an
/// existing directory. The caller must independently establish that a runtime
/// actually writes to that directory.
pub fn available_directory_bytes(directory: &Path) -> Result<u64> {
    let canonical = std::fs::canonicalize(directory)?;
    if !canonical.is_dir() {
        return Err(LocalError::Invalid(
            "Runtime installation root is not a directory; disk admission is unavailable.".into(),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

        let wide: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(free)
    }
    #[cfg(not(windows))]
    {
        let _ = canonical;
        Err(LocalError::Invalid(
            "Runtime-installation disk measurement is not implemented on this platform.".into(),
        ))
    }
}

/// A unique SHA-256 manifest descriptor. The digest is normalized to lowercase.
#[derive(Debug, Clone)]
pub(crate) struct ManifestBlob {
    pub(crate) sha256: String,
    pub(crate) size_bytes: u64,
}

/// Count only exact, regular, hash-verified Ollama blobs in a verified store.
/// Sparse `-partial` files and progress checkpoints are deliberately excluded.
#[cfg(all(windows, target_arch = "x86_64"))]
pub(crate) fn verified_complete_blob_bytes(
    blobs: &Path,
    manifest: &[ManifestBlob],
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<u64> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    use std::sync::atomic::Ordering;

    let mut credited = 0u64;
    let mut seen = std::collections::HashSet::new();
    if cancelled.load(Ordering::Acquire) {
        return Err(LocalError::Cancelled);
    }
    for descriptor in manifest {
        if cancelled.load(Ordering::Acquire) {
            return Err(LocalError::Cancelled);
        }
        if !seen.insert(&descriptor.sha256) {
            continue;
        }
        let path = blobs.join(format!("sha256-{}", descriptor.sha256));
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        crate::provision::checked_direct_child(blobs, &path)?;
        if !metadata.is_file() || metadata.len() != descriptor.size_bytes {
            continue;
        }
        let mut file = std::fs::File::open(&path)?;
        if file.metadata()?.len() != descriptor.size_bytes {
            continue;
        }
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 256 * 1024];
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Err(LocalError::Cancelled);
            }
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        if file.metadata()?.len() != descriptor.size_bytes {
            continue;
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(LocalError::Cancelled);
        }
        crate::provision::checked_direct_child(blobs, &path)?;
        if format!("{:x}", hasher.finalize()) == descriptor.sha256 {
            credited = credited.checked_add(descriptor.size_bytes).ok_or_else(|| {
                LocalError::Invalid("Verified model blob sizes overflowed".into())
            })?;
        }
    }
    Ok(credited)
}

/// Reserve at least 1 GiB or 10% of the original manifest size.
pub(crate) fn model_download_reserve(manifest_bytes: u64) -> Result<u64> {
    const GIB: u64 = 1024 * 1024 * 1024;
    if manifest_bytes == 0 {
        return Err(LocalError::Invalid(
            "Model manifest has no download size".into(),
        ));
    }
    Ok(GIB.max(manifest_bytes / 10))
}

/// Conservative pre-setup allowance for one model on an unused managed drive.
/// The caller decides whether the storage choice is still changeable; this
/// estimate is never a substitute for live setup and pull admission.
pub fn pre_setup_storage_plan(
    root: PathBuf,
    available_bytes: u64,
    runtime_setup_min_free_bytes: u64,
    model_download_bytes: u64,
) -> Result<PreSetupStoragePlan> {
    let pull_reserve_bytes = model_download_reserve(model_download_bytes)?;
    let required_bytes = runtime_setup_min_free_bytes
        .checked_add(model_download_bytes)
        .and_then(|bytes| bytes.checked_add(pull_reserve_bytes))
        .ok_or_else(|| {
            LocalError::Invalid("Pre-setup model allowance exceeds disk limits".into())
        })?;
    Ok(PreSetupStoragePlan {
        root,
        available_bytes,
        runtime_setup_min_free_bytes,
        model_download_bytes,
        pull_reserve_bytes,
        required_bytes,
        shortfall_bytes: required_bytes.saturating_sub(available_bytes),
    })
}

/// Require uncredited manifest payload plus a reserve before a managed pull.
/// The returned estimate records observed free space at the admission check.
/// A mutable tag or concurrent writer can change actual usage; the pull must
/// independently recheck the reserve while progress arrives.
pub fn admit_model_download(
    manifest_bytes: u64,
    credited_existing_bytes: u64,
    available_bytes: u64,
) -> Result<ModelDownloadAdmission> {
    let reserve = model_download_reserve(manifest_bytes)?;
    let remaining_bytes = manifest_bytes
        .checked_sub(credited_existing_bytes)
        .ok_or_else(|| {
            LocalError::Invalid("Verified model blobs exceed the manifest size".into())
        })?;
    let required = remaining_bytes.checked_add(reserve).ok_or_else(|| {
        LocalError::Invalid("Model manifest size exceeds disk admission limits".into())
    })?;
    if available_bytes < required {
        return Err(LocalError::Invalid(format!(
            "Managed model store has {} bytes free; this manifest has {} bytes remaining after {} bytes of verified complete blobs, plus {} bytes of reserve. Free at least {} more bytes on the model-store volume before retrying, or use another local runtime. Sparse partial downloads cannot safely be credited.",
            available_bytes, remaining_bytes, credited_existing_bytes, reserve, required - available_bytes
        )));
    }
    Ok(ModelDownloadAdmission {
        manifest_bytes,
        credited_existing_bytes,
        remaining_bytes,
        reserve_bytes: reserve,
        available_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn measures_the_existing_store_volume() {
        let store = tempfile::tempdir().unwrap();
        require_local_drive_directory(store.path()).unwrap();
        assert!(available_directory_bytes(store.path()).unwrap() > 0);
        assert!(available_directory_bytes(&store.path().join("missing")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn directory_identity_changes_when_folder_is_replaced_at_same_path() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("runtime");
        std::fs::create_dir(&root).unwrap();
        let before = directory_identity(&root).unwrap();
        std::fs::rename(&root, parent.path().join("old-runtime")).unwrap();
        std::fs::create_dir(&root).unwrap();
        assert_ne!(directory_identity(&root).unwrap(), before);
    }

    #[test]
    fn model_admission_checks_full_estimate_and_reserve() {
        let gib = 1024 * 1024 * 1024;
        assert!(admit_model_download(gib, 0, 2 * gib - 1).is_err());
        let admitted = admit_model_download(gib, 0, 2 * gib).unwrap();
        assert_eq!(admitted.reserve_bytes, gib);
        assert_eq!(admitted.remaining_bytes, gib);
        assert!(admit_model_download(u64::MAX, 0, u64::MAX).is_err());
        assert!(admit_model_download(0, 0, 2 * gib).is_err());
    }

    #[test]
    fn pre_setup_allowance_matches_pull_reserve_and_reports_exact_shortfall() {
        let gib = 1024 * 1024 * 1024;
        let root = PathBuf::from("managed-runtime");
        let plan = pre_setup_storage_plan(root.clone(), 8 * gib, 6 * gib, 4 * gib).unwrap();
        assert_eq!(plan.root, root);
        assert_eq!(plan.pull_reserve_bytes, gib);
        assert_eq!(plan.required_bytes, 11 * gib);
        assert_eq!(plan.shortfall_bytes, 3 * gib);
        assert_eq!(
            pre_setup_storage_plan(plan.root, 11 * gib, 6 * gib, 4 * gib)
                .unwrap()
                .shortfall_bytes,
            0
        );
        assert_eq!(
            pre_setup_storage_plan(PathBuf::new(), 0, gib, 20 * gib)
                .unwrap()
                .pull_reserve_bytes,
            2 * gib
        );
        assert!(pre_setup_storage_plan(PathBuf::new(), 0, gib, 0).is_err());
        assert!(pre_setup_storage_plan(PathBuf::new(), 0, u64::MAX, gib).is_err());
    }

    #[test]
    fn model_admission_credits_only_verified_complete_bytes() {
        let gib = 1024 * 1024 * 1024;
        let admitted = admit_model_download(3 * gib, 2 * gib, 2 * gib).unwrap();
        assert_eq!(admitted.manifest_bytes, 3 * gib);
        assert_eq!(admitted.credited_existing_bytes, 2 * gib);
        assert_eq!(admitted.remaining_bytes, gib);
        assert_eq!(admitted.reserve_bytes, gib);
        assert!(admit_model_download(3 * gib, 2 * gib, 2 * gib - 1).is_err());
        assert!(admit_model_download(3 * gib, 3 * gib + 1, 2 * gib).is_err());
        let error = admit_model_download(3 * gib, 0, 2 * gib).unwrap_err();
        assert!(error
            .to_string()
            .contains(&format!("{} more bytes", 2 * gib)));
        assert!(error
            .to_string()
            .contains("Sparse partial downloads cannot safely be credited"));
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn blob_credit_requires_exact_name_size_and_sha256() {
        use sha2::{Digest, Sha256};
        use std::sync::atomic::AtomicBool;

        let store = tempfile::tempdir().unwrap();
        let cancelled = AtomicBool::new(false);
        let digest = format!("{:x}", Sha256::digest(b"valid"));
        let blob = ManifestBlob {
            sha256: digest.clone(),
            size_bytes: 5,
        };
        std::fs::write(
            store.path().join(format!("sha256-{digest}-partial")),
            b"valid",
        )
        .unwrap();
        assert_eq!(
            verified_complete_blob_bytes(store.path(), std::slice::from_ref(&blob), &cancelled)
                .unwrap(),
            0
        );
        let path = store.path().join(format!("sha256-{digest}"));
        std::fs::write(&path, b"wrong").unwrap();
        assert_eq!(
            verified_complete_blob_bytes(store.path(), std::slice::from_ref(&blob), &cancelled)
                .unwrap(),
            0
        );
        std::fs::write(&path, b"valid!").unwrap();
        assert_eq!(
            verified_complete_blob_bytes(store.path(), std::slice::from_ref(&blob), &cancelled)
                .unwrap(),
            0
        );
        std::fs::write(&path, b"valid").unwrap();
        assert_eq!(
            verified_complete_blob_bytes(store.path(), &[blob.clone(), blob], &cancelled).unwrap(),
            5
        );
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        assert!(matches!(
            verified_complete_blob_bytes(
                store.path(),
                &[ManifestBlob {
                    sha256: digest,
                    size_bytes: 5
                }],
                &cancelled
            ),
            Err(LocalError::Cancelled)
        ));
    }

    #[cfg(all(windows, target_arch = "x86_64"))]
    #[test]
    fn linked_blob_is_not_credited() {
        use sha2::{Digest, Sha256};
        use std::os::windows::fs::symlink_file;
        use std::sync::atomic::AtomicBool;

        let store = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let digest = format!("{:x}", Sha256::digest(b"valid"));
        let target = outside.path().join("blob");
        std::fs::write(&target, b"valid").unwrap();
        let path = store.path().join(format!("sha256-{digest}"));
        match symlink_file(&target, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("could not create fixture link: {error}"),
        }
        let blob = ManifestBlob {
            sha256: digest,
            size_bytes: 5,
        };
        assert!(
            verified_complete_blob_bytes(store.path(), &[blob], &AtomicBool::new(false))
                .unwrap_err()
                .to_string()
                .contains("reparse point")
        );
    }
}
