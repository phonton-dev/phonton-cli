//! Conservative, reviewed offline preparation for a root npm test script.
//! This is dependency setup, never evidence that the task passed verification.

use super::{invalid, Result};
use phonton_types::local_run::LocalCheck;
use serde_json::Value;
use std::path::Path;

const MAX_PACKAGE_JSON: u64 = 1024 * 1024;
const MAX_LOCKFILE: u64 = 2 * 1024 * 1024;
const MAX_LOCKED_PACKAGES: usize = 512;

pub(super) fn command() -> LocalCheck {
    LocalCheck {
        program: if cfg!(windows) { "npm.cmd" } else { "npm" }.into(),
        args: [
            "ci",
            "--offline",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
            "--no-update-notifier",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    }
}

pub(super) fn is_root_npm_test(check: &LocalCheck) -> bool {
    check
        .program
        .eq_ignore_ascii_case(if cfg!(windows) { "npm.cmd" } else { "npm" })
        && (matches!(check.args.as_slice(), [test] if test == "test")
            || matches!(check.args.as_slice(), [run, test] if run == "run" && test == "test"))
}

fn bounded_json(root: &Path, name: &str, limit: u64) -> Result<Value> {
    let path = root.join(name);
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| invalid(format!("Offline npm preparation needs {name}: {error}")))?;
    if !metadata.file_type().is_file() || metadata.len() > limit {
        return Err(invalid(format!(
            "Offline npm preparation needs a regular {name} of at most {limit} bytes"
        )));
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

/// Return the exact setup command only when a root package has dependencies
/// and a bounded registry lockfile that npm can consume without network fetches.
pub(super) fn validated_command(root: &Path) -> Result<Option<LocalCheck>> {
    let package = bounded_json(root, "package.json", MAX_PACKAGE_JSON)?;
    if package.get("workspaces").is_some()
        || package.get("packageManager").is_some_and(|manager| {
            !manager
                .as_str()
                .is_some_and(|text| text.starts_with("npm@"))
        })
    {
        return Err(invalid(
            "Automatic offline npm preparation does not support workspaces or another package manager",
        ));
    }
    let mut dependencies = false;
    for group in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        if let Some(value) = package.get(group) {
            let entries = value
                .as_object()
                .ok_or_else(|| invalid(format!("package.json {group} is not an object")))?;
            dependencies |= !entries.is_empty();
            if entries.values().any(|spec| {
                spec.as_str().is_none_or(|text| {
                    [
                        "file:",
                        "link:",
                        "workspace:",
                        "git:",
                        "git+",
                        "http:",
                        "https:",
                    ]
                    .iter()
                    .any(|prefix| text.starts_with(prefix))
                })
            }) {
                return Err(invalid(
                    "Automatic offline npm preparation needs registry dependencies, not local, Git or URL dependencies",
                ));
            }
        }
    }
    if !dependencies {
        return Ok(None);
    }
    let lock = bounded_json(root, "package-lock.json", MAX_LOCKFILE)?;
    if !matches!(lock["lockfileVersion"].as_u64(), Some(2 | 3)) {
        return Err(invalid(
            "Automatic offline npm preparation needs a v2/v3 package-lock.json",
        ));
    }
    let packages = lock["packages"]
        .as_object()
        .ok_or_else(|| invalid("package-lock.json has no packages object"))?;
    if packages.is_empty() || packages.len() > MAX_LOCKED_PACKAGES || !packages.contains_key("") {
        return Err(invalid(
            "Offline npm lockfile has no root package or exceeds 512 package entries",
        ));
    }
    for (path, entry) in packages {
        if path.is_empty() {
            continue;
        }
        if !path.starts_with("node_modules/")
            || path.contains('\\')
            || path.split('/').any(|part| part == ".." || part.is_empty())
            || entry["link"].as_bool() == Some(true)
            || !entry["resolved"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://registry.npmjs.org/"))
            || !entry["integrity"].as_str().is_some_and(|hash| {
                hash.len() <= 1024 && (hash.starts_with("sha512-") || hash.starts_with("sha256-"))
            })
        {
            return Err(invalid(format!(
                "Offline npm lockfile has an unsupported package source: {path}"
            )));
        }
    }
    Ok(Some(command()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_command_needs_registry_lock_and_never_uses_lifecycle_scripts() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"test":"fixture-check"},"devDependencies":{"fixture":"1.0.0"}}"#,
        )
        .unwrap();
        let lock = r#"{"lockfileVersion":3,"packages":{"":{},"node_modules/fixture":{"version":"1.0.0","resolved":"https://registry.npmjs.org/fixture/-/fixture-1.0.0.tgz","integrity":"sha512-abc"}}}"#;
        std::fs::write(root.path().join("package-lock.json"), lock).unwrap();
        let check = validated_command(root.path()).unwrap().unwrap();
        assert!(check.args.contains(&"--offline".into()));
        assert!(check.args.contains(&"--ignore-scripts".into()));
        assert!(is_root_npm_test(&LocalCheck {
            program: check.program.clone(),
            args: vec!["test".into()],
        }));
        assert!(!is_root_npm_test(&LocalCheck {
            program: check.program.clone(),
            args: vec!["test".into(), "--prefix".into(), "nested".into()],
        }));
        std::fs::write(
            root.path().join("package-lock.json"),
            lock.replace("https://registry.npmjs.org/", "file:../"),
        )
        .unwrap();
        assert!(validated_command(root.path()).is_err());
        std::fs::write(
            root.path().join("package-lock.json"),
            lock.replace("node_modules/fixture", r"node_modules/..\\fixture"),
        )
        .unwrap();
        assert!(validated_command(root.path()).is_err());
        std::fs::write(root.path().join("package-lock.json"), lock).unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"test":"node test.js"}}"#,
        )
        .unwrap();
        assert!(validated_command(root.path()).unwrap().is_none());
    }
}
