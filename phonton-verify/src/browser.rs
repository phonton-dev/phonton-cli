//! Supervised static rendering checks; functional assertions remain explicit.
use anyhow::Result;
use phonton_types::verification::VerificationExecution;
use phonton_types::{VerifyLayer, VerifyResult};
use std::{path::Path, time::Duration};

/// Detect a web surface, requiring isolation before any browser execution.
pub async fn verify_browser_check(working_dir: &Path) -> Result<Option<VerifyResult>> {
    verify_browser_check_with_execution(working_dir, VerificationExecution::RequireIsolation).await
}

/// Run a static render smoke through the shared executor with explicit authority.
/// This proves neither application functionality nor filesystem/network isolation.
pub async fn verify_browser_check_with_execution(
    working_dir: &Path,
    policy: VerificationExecution,
) -> Result<Option<VerifyResult>> {
    if !working_dir.join("index.html").is_file()
        && !package_json_looks_browser_runnable(&working_dir.join("package.json"))
    {
        return Ok(None);
    }
    if policy == VerificationExecution::RequireIsolation {
        return Ok(Some(VerifyResult::Unavailable { reason: "Browser verification unavailable: no isolation backend is configured and host execution was not approved.".into() }));
    }
    if !working_dir.join("index.html").is_file() {
        return Ok(Some(VerifyResult::Unavailable { reason: "Browser verification unavailable: this project needs an explicit build/start/test command; static rendering cannot verify it.".into() }));
    }
    let evidence = tempfile::tempdir()?;
    let script = evidence.path().join("render-check.cjs");
    std::fs::write(&script, include_str!("browser_check.cjs"))?;
    let mut module_roots = Vec::new();
    for root in working_dir.ancestors().take(16) {
        module_roots.push(root.to_path_buf());
    }
    if let Some(root) = Path::new(env!("CARGO_MANIFEST_DIR")).parent() {
        module_roots.push(root.to_path_buf());
    }
    let output = match crate::executor::run(
        working_dir,
        "node",
        vec![
            script.to_string_lossy().into(),
            serde_json::to_string(&module_roots)?,
        ],
        Duration::from_secs(45),
        policy,
    )
    .await
    {
        Ok(output) => output,
        Err(unavailable) => return Ok(Some(unavailable)),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("PHONTON_JSON:"))
        .next_back()
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok());
    let Some(result) = result else {
        return Ok(Some(VerifyResult::Unavailable {
            reason: format!(
                "Browser verification unavailable: missing result; {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        }));
    };
    let errors: Vec<String> = result["errors"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if result["unavailable"] == true || !output.status.success() {
        Ok(Some(VerifyResult::Unavailable {
            reason: format!("Browser verification unavailable: {}", errors.join("; ")),
        }))
    } else if result["success"] == true && errors.is_empty() {
        Ok(Some(VerifyResult::Pass {
            layer: VerifyLayer::BrowserCheck,
        }))
    } else {
        Ok(Some(VerifyResult::Fail {
            layer: VerifyLayer::BrowserCheck,
            errors,
            attempt: 1,
        }))
    }
}

fn package_json_looks_browser_runnable(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };

    let frontend_names = [
        "vite",
        "next",
        "react-scripts",
        "webpack",
        "parcel",
        "@sveltejs/kit",
        "astro",
    ];
    for section in ["dependencies", "devDependencies"] {
        if let Some(deps) = value.get(section).and_then(|v| v.as_object()) {
            if frontend_names.iter().any(|name| deps.contains_key(*name)) {
                return true;
            }
        }
    }

    value
        .get("scripts")
        .and_then(|v| v.as_object())
        .map(|scripts| {
            scripts.iter().any(|(name, command)| {
                matches!(name.as_str(), "dev" | "start" | "serve" | "preview")
                    && command
                        .as_str()
                        .map(|cmd| {
                            frontend_names
                                .iter()
                                .any(|needle| cmd.to_ascii_lowercase().contains(needle))
                        })
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}
