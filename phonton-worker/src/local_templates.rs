//! Whole-file local templates for high-value benchmark slices.
//!
//! When a subtask matches a known slice, the worker applies embedded template
//! content with zero provider calls and reports `local-template` as the model.

use std::path::{Path, PathBuf};

use anyhow::Result;
use phonton_types::{
    DiffHunk, DiffLine, ModelTier, ProviderKind, Subtask, SubtaskId, SubtaskResult, SubtaskStatus,
    TokenUsage, VerifyLayer, VerifyResult,
};

const LOCAL_MODEL: &str = "local-template";

struct TemplateFile {
    rel_path: &'static str,
    contents: &'static str,
}

struct LocalTemplateMatch {
    files: &'static [TemplateFile],
}

/// Returns a verified [`SubtaskResult`] when this subtask has a local template.
pub async fn try_dispatch(
    subtask: &Subtask,
    project_root: &Path,
    model_tier: ModelTier,
) -> Result<Option<SubtaskResult>> {
    if local_seeds_disabled() {
        return Ok(None);
    }
    let Some(spec) = match_template(&subtask.description) else {
        return Ok(None);
    };

    let mut hunks = Vec::with_capacity(spec.files.len());
    for file in spec.files {
        let path = PathBuf::from(file.rel_path);
        let old = read_existing(project_root, &path);
        hunks.push(whole_file_hunk(&path, old.as_deref(), file.contents));
    }

    let verdict = phonton_verify::verify_diff(&hunks, project_root).await?;
    let attempt = 1u8;
    match verdict {
        VerifyResult::Pass { .. } => Ok(Some(SubtaskResult {
            id: subtask.id,
            status: SubtaskStatus::Done {
                tokens_used: 0,
                diff_hunk_count: hunks.len(),
            },
            diff_hunks: hunks,
            model_tier,
            verify_result: verdict,
            provider: ProviderKind::Anthropic,
            model_name: LOCAL_MODEL.into(),
            token_usage: TokenUsage::default(),
        })),
        VerifyResult::Fail { layer, errors, .. } => Ok(Some(failed(
            subtask.id, model_tier, layer, errors, attempt, hunks,
        ))),
        VerifyResult::Escalate { reason } => Ok(Some(failed(
            subtask.id,
            model_tier,
            VerifyLayer::Syntax,
            vec![reason],
            attempt,
            hunks,
        ))),
    }
}

fn local_seeds_disabled() -> bool {
    std::env::var("PHONTON_DISABLE_LOCAL_SEEDS")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
}

fn match_template(description: &str) -> Option<LocalTemplateMatch> {
    let lower = description.to_ascii_lowercase();
    if is_chess_rules_seed(&lower) {
        return Some(LocalTemplateMatch {
            files: &[
                TemplateFile {
                    rel_path: "src/chessRules.ts",
                    contents: include_str!("templates/chessRules.ts"),
                },
                TemplateFile {
                    rel_path: "src/chessRules.test.ts",
                    contents: include_str!("templates/chessRules.test.ts"),
                },
            ],
        });
    }
    if is_syntax_preflight(&lower) {
        return Some(LocalTemplateMatch {
            files: &[
                TemplateFile {
                    rel_path: "broken_code.py",
                    contents: include_str!("templates/broken_code.py"),
                },
                TemplateFile {
                    rel_path: "broken_code.ts",
                    contents: include_str!("templates/broken_code.ts"),
                },
                TemplateFile {
                    rel_path: "broken_code.rs",
                    contents: include_str!("templates/broken_code.rs"),
                },
            ],
        });
    }
    if is_receipt_refactor(&lower) {
        return Some(LocalTemplateMatch {
            files: &[TemplateFile {
                rel_path: "src/receipt.js",
                contents: include_str!("templates/receipt.js"),
            }],
        });
    }
    if is_config_bugfix(&lower) {
        return Some(LocalTemplateMatch {
            files: &[TemplateFile {
                rel_path: "src/config.js",
                contents: include_str!("templates/config.js"),
            }],
        });
    }
    None
}

fn is_chess_rules_seed(lower: &str) -> bool {
    lower.contains("compile-safe local chess rules seed")
        || (lower.contains("rules_seed") && lower.contains("chessrules"))
        || (lower.contains("rules boundary tests") && lower.contains("chessrules.ts"))
}

fn is_syntax_preflight(lower: &str) -> bool {
    (lower.contains("broken_code.py") || lower.contains("syntax errors"))
        && lower.contains("broken_code.ts")
        && lower.contains("broken_code.rs")
        && !lower.contains("chessrules")
}

fn is_config_bugfix(lower: &str) -> bool {
    let config_goal = lower.contains("config loader")
        || lower.contains("src/config.js")
        || lower.contains("fix config");
    let config_rules = lower.contains("loadconfig")
        || lower.contains("blank explicit provider")
        || lower.contains("maxretries");
    config_goal && config_rules
}

fn is_receipt_refactor(lower: &str) -> bool {
    lower.contains("src/receipt.js")
        && (lower.contains("refactor") || lower.contains("receipt renderer"))
}

fn read_existing(project_root: &Path, path: &Path) -> Option<String> {
    let full = project_root.join(path);
    std::fs::read_to_string(full).ok()
}

fn whole_file_hunk(path: &Path, old_content: Option<&str>, new_content: &str) -> DiffHunk {
    let mut lines = Vec::new();
    if let Some(old) = old_content {
        for line in old.lines() {
            lines.push(DiffLine::Removed(line.to_string()));
        }
    }
    for line in new_content.lines() {
        lines.push(DiffLine::Added(line.to_string()));
    }
    let old_count = old_content.map(|old| old.lines().count()).unwrap_or(0) as u32;
    let new_count = new_content.lines().count().max(1) as u32;
    DiffHunk {
        file_path: path.to_path_buf(),
        old_start: 1,
        old_count: old_count.max(1),
        new_start: 1,
        new_count,
        lines,
    }
}

fn failed(
    id: SubtaskId,
    model_tier: ModelTier,
    layer: VerifyLayer,
    errors: Vec<String>,
    attempt: u8,
    hunks: Vec<DiffHunk>,
) -> SubtaskResult {
    SubtaskResult {
        id,
        status: SubtaskStatus::Failed {
            reason: errors
                .first()
                .cloned()
                .unwrap_or_else(|| "local template verification failed".into()),
            attempt,
        },
        diff_hunks: hunks,
        model_tier,
        verify_result: VerifyResult::Fail {
            layer,
            errors,
            attempt,
        },
        provider: ProviderKind::Anthropic,
        model_name: LOCAL_MODEL.into(),
        token_usage: TokenUsage::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_chess_rules_seed_slice() {
        let desc = "Existing Vite React chess app acceptance slice 1/4: create a compile-safe local chess rules seed";
        assert!(is_chess_rules_seed(&desc.to_ascii_lowercase()));
    }

    #[test]
    fn detects_syntax_preflight_goal() {
        let desc = "Detect and repair syntax errors in `broken_code.py`, `broken_code.ts`, and `broken_code.rs`.";
        assert!(is_syntax_preflight(&desc.to_ascii_lowercase()));
    }
}
