//! Small inspectable state file with bounded reads and atomic replacement.

use crate::{LocalError, Result};
use phonton_types::local::LocalSettings;
use std::path::Path;

/// Cross-process operation lease. The operating system releases it on process
/// exit, so an interruption cannot leave a stale owner blocking future setup.
pub struct StateLease {
    _file: std::fs::File,
}

/// Acquire exclusive model-management ownership without waiting indefinitely.
pub fn acquire(path: &Path) -> Result<StateLease> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    file.try_lock().map_err(|error| LocalError::Invalid(format!("Another Phonton process owns local model management, or the lock is unavailable: {error}")))?;
    Ok(StateLease { _file: file })
}

/// Read settings without treating corrupt data as a fresh installation.
pub fn load(path: &Path) -> Result<LocalSettings> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(LocalSettings::default()),
        Err(e) => return Err(e.into()),
        Ok(metadata) if metadata.len() > 4 * 1024 * 1024 => {
            return Err(LocalError::Invalid(
                "Local model state exceeds 4 MiB".into(),
            ))
        }
        Ok(_) => {}
    }
    let settings: LocalSettings = serde_json::from_slice(&std::fs::read(path)?)?;
    if !matches!(settings.schema, 1..=5)
        || (settings.schema == 1
            && (settings.managed_root.is_some()
                || settings.managed_root_identity.is_some()
                || settings.managed_root_used
                || settings.managed_runtime_installed))
        || (settings.managed_root_used && settings.managed_root.is_none())
        || (settings.managed_runtime_installed && !matches!(settings.schema, 3..=5))
        || (settings.calibration_attempt.is_some() && !matches!(settings.schema, 4..=5))
        || (!settings.install_attempts.is_empty() && settings.schema != 5)
        || settings.install_attempts.len() > 8
        || settings
            .install_attempts
            .iter()
            .any(|attempt| attempt.schema != 1)
        || (settings.managed_root_identity.is_some() && settings.managed_root.is_none())
    {
        return Err(LocalError::Invalid(
            "Unsupported local model state version".into(),
        ));
    }
    crate::runtime::local_endpoint(&settings.endpoint)?;
    Ok(settings)
}

/// Write through a sibling file. A failed write leaves the prior file intact.
/// The caller serializes state mutations within its CLI/sidecar process.
pub fn save(path: &Path, settings: &LocalSettings) -> Result<()> {
    use std::io::Write;
    if !matches!(settings.schema, 1..=5) {
        return Err(LocalError::Invalid(
            "Unsupported local model state version".into(),
        ));
    }
    if (settings.managed_root.is_some()
        || settings.managed_root_identity.is_some()
        || settings.managed_root_used
        || settings.managed_runtime_installed)
        && !matches!(settings.schema, 2..=5)
    {
        return Err(LocalError::Invalid(
            "Managed storage requires local model state schema 2 or later".into(),
        ));
    }
    if settings.managed_root_used && settings.managed_root.is_none() {
        return Err(LocalError::Invalid(
            "Used managed storage is missing its saved folder".into(),
        ));
    }
    if settings.managed_runtime_installed && !matches!(settings.schema, 3..=5) {
        return Err(LocalError::Invalid(
            "Managed runtime installation requires local model state schema 3 or later".into(),
        ));
    }
    if settings.calibration_attempt.is_some() && !matches!(settings.schema, 4..=5) {
        return Err(LocalError::Invalid(
            "Incomplete calibration evidence requires local model state schema 4".into(),
        ));
    }
    if !settings.install_attempts.is_empty() && settings.schema != 5 {
        return Err(LocalError::Invalid(
            "Unconfirmed model install identities require local model state schema 5".into(),
        ));
    }
    if settings.install_attempts.len() > 8
        || settings
            .install_attempts
            .iter()
            .any(|attempt| attempt.schema != 1)
    {
        return Err(LocalError::Invalid(
            "Unconfirmed model install history is invalid or exceeds eight entries".into(),
        ));
    }
    if settings.managed_root_identity.is_some() && settings.managed_root.is_none() {
        return Err(LocalError::Invalid(
            "Managed root identity requires a managed root".into(),
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(settings)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(LocalError::Invalid(
            "Local model state exceeds 4 MiB".into(),
        ));
    }
    // A process can exit before renaming its sibling file. Include a fresh
    // timestamp and bounded collision retry so PID reuse cannot strand saves.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let mut collision = 0;
    let (temp, mut file) = loop {
        let temp = path.with_extension(format!(
            "{}.{}.{}.tmp",
            std::process::id(),
            stamp,
            collision
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && collision < 7 => {
                collision += 1;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::local::{CalibrationAttempt, HardwareSnapshot, InstallAttempt};
    #[test]
    fn operation_lease_is_exclusive_and_recovers_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let first = acquire(&path).unwrap();
        assert!(acquire(&path).is_err());
        drop(first);
        assert!(acquire(&path).is_ok());
    }
    #[test]
    fn corrupt_state_is_not_silently_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        assert!(load(&path).unwrap().active_model.is_none());
        std::fs::write(&path, "truncated{").unwrap();
        assert!(load(&path).is_err());
    }
    #[test]
    fn state_can_be_replaced_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        save(&path, &LocalSettings::default()).unwrap();
        let mut settings = load(&path).unwrap();
        settings.active_model = Some("qwen2.5-coder:1.5b".into());
        save(&path, &settings).unwrap();
        assert_eq!(load(&path).unwrap().active_model, settings.active_model);
    }

    #[test]
    fn stale_temp_from_reused_pid_does_not_block_the_next_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let stale = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&stale, b"interrupted write").unwrap();
        save(&path, &LocalSettings::default()).unwrap();
        assert_eq!(load(&path).unwrap().schema, 1);
        assert_eq!(std::fs::read(&stale).unwrap(), b"interrupted write");
    }

    #[test]
    fn legacy_state_without_managed_root_remains_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-models.json");
        std::fs::write(
            &path,
            r#"{"schema":1,"endpoint":"http://127.0.0.1:11434","active_model":null,"profiles":[]}"#,
        )
        .unwrap();
        assert!(load(&path).unwrap().managed_root.is_none());
    }

    #[test]
    fn managed_root_needs_schema_two_to_protect_it_from_older_engines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-models.json");
        let settings = LocalSettings {
            managed_root: Some(dir.path().join("models")),
            ..Default::default()
        };
        assert!(save(&path, &settings).is_err());
        let mut upgraded = settings;
        upgraded.schema = 2;
        save(&path, &upgraded).unwrap();
        assert_eq!(load(&path).unwrap().managed_root, upgraded.managed_root);
    }

    #[test]
    fn managed_install_marker_needs_schema_three_even_at_default_root() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-models.json");
        let mut settings = LocalSettings {
            schema: 2,
            managed_runtime_installed: true,
            ..Default::default()
        };
        assert!(save(&path, &settings).is_err());
        settings.schema = 3;
        save(&path, &settings).unwrap();
        assert!(load(&path).unwrap().managed_runtime_installed);
        assert_eq!(load(&path).unwrap().schema, 3);
    }

    #[test]
    fn incomplete_calibration_requires_schema_four_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-models.json");
        let mut settings = LocalSettings {
            schema: 3,
            managed_runtime_installed: true,
            calibration_attempt: Some(CalibrationAttempt {
                schema: 1,
                model: "fixture:latest".into(),
                starting_digest: "digest-a".into(),
                runtime_version: "0.13.0".into(),
                endpoint: "http://127.0.0.1:11434".into(),
                context_tokens: 4096,
                hardware: HardwareSnapshot::default(),
                started_at_unix: 1,
                probes: Vec::new(),
            }),
            ..Default::default()
        };
        assert!(save(&path, &settings).is_err());
        settings.schema = 4;
        save(&path, &settings).unwrap();
        let reopened = load(&path).unwrap();
        assert!(reopened.managed_runtime_installed);
        assert_eq!(
            reopened.calibration_attempt.unwrap().starting_digest,
            "digest-a"
        );
    }

    #[test]
    fn install_attempt_requires_schema_five_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-models.json");
        let mut settings = LocalSettings {
            schema: 4,
            install_attempts: vec![InstallAttempt {
                schema: 1,
                model: "qwen3.5:4b".into(),
                endpoint: "http://127.0.0.1:11434".into(),
                started_at_unix: 5,
            }],
            ..Default::default()
        };
        assert!(save(&path, &settings).is_err());
        std::fs::write(&path, serde_json::to_vec(&settings).unwrap()).unwrap();
        assert!(load(&path).is_err());
        settings.schema = 5;
        save(&path, &settings).unwrap();
        let reopened = load(&path).unwrap();
        assert_eq!(reopened.install_attempts[0].model, "qwen3.5:4b");
        assert!(reopened.profiles.is_empty());
        settings.calibration_attempt = Some(CalibrationAttempt {
            schema: 1,
            model: "fixture:latest".into(),
            starting_digest: "digest-a".into(),
            runtime_version: "0.13.0".into(),
            endpoint: settings.endpoint.clone(),
            context_tokens: 4096,
            hardware: HardwareSnapshot::default(),
            started_at_unix: 6,
            probes: Vec::new(),
        });
        save(&path, &settings).unwrap();
        let reopened = load(&path).unwrap();
        assert_eq!(reopened.install_attempts.len(), 1);
        assert!(reopened.calibration_attempt.is_some());
    }
}
