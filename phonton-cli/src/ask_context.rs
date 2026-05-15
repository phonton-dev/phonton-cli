//! Workspace-aware Ask context builder.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use phonton_types::{render_prompt_attachments, PromptAttachment};

pub(crate) const ASK_CONTEXT_TARGET_TOKENS: usize = 1_200;

pub(crate) const ASK_SYSTEM_PROMPT: &str = "\
You are Phonton Ask, a workspace-aware assistant inside a local-first ADE. \
Answer from the provided workspace context when it is relevant. Cite workspace \
paths you inspected. If the provided context is insufficient, say exactly what \
is missing instead of guessing. Do not claim tests, builds, or verification ran \
unless the context says they did. When a failed Phonton goal is relevant, point \
to /problems, /why-tokens, /retry, or phonton diff only if that directly helps.";

#[derive(Debug, Clone, Copy)]
pub(crate) struct AskContextRequest<'a> {
    pub question: &'a str,
    pub workspace_root: &'a Path,
    pub attachments: &'a [PromptAttachment],
    pub current_goal: Option<&'a str>,
    pub diagnostics: &'a [String],
    pub max_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AskContextReport {
    pub prompt: String,
    pub context_tokens: usize,
    pub selected_paths: Vec<String>,
    pub summary: String,
}

pub(crate) fn build_stateless_ask_prompt(question: &str) -> String {
    question.to_string()
}

pub(crate) fn build_ask_context(request: AskContextRequest<'_>) -> AskContextReport {
    let max_tokens = request.max_tokens.max(128);
    let workspace_root = request
        .workspace_root
        .canonicalize()
        .unwrap_or_else(|_| request.workspace_root.to_path_buf());
    let mut selected_paths = Vec::new();
    let mut sections = Vec::new();

    let attachments = render_prompt_attachments(request.attachments);
    if !attachments.is_empty() {
        for attachment in request.attachments {
            selected_paths.push(display_path(&attachment.path));
        }
        sections.push(attachments);
    }

    let mut task = String::new();
    if let Some(goal) = request.current_goal.filter(|goal| !goal.trim().is_empty()) {
        task.push_str("# Current Phonton goal\n");
        task.push_str("Current goal: ");
        task.push_str(goal.trim());
        task.push('\n');
    }
    if !request.diagnostics.is_empty() {
        task.push_str("# Recent diagnostics\n");
        for diagnostic in request.diagnostics.iter().take(6) {
            task.push_str("- ");
            task.push_str(diagnostic);
            task.push('\n');
        }
    }
    if !task.is_empty() {
        sections.push(task);
    }

    let files = collect_workspace_files(&workspace_root, 48);
    sections.push(workspace_facts(&workspace_root, &files));
    for path in files.iter().take(24) {
        selected_paths.push(display_relative(&workspace_root, path));
    }
    sections.push(file_map_section(&workspace_root, &files, 24));
    for path in top_lexical_snippets(&workspace_root, &files, request.question, 3) {
        if let Some(section) = snippet_section(&workspace_root, &path) {
            selected_paths.push(display_relative(&workspace_root, &path));
            sections.push(section);
        }
    }

    let question_section = format!("\n# Question\n{}\n", request.question.trim());
    let context_budget = max_tokens
        .saturating_sub(estimate_tokens(&question_section))
        .max(96)
        .min(max_tokens);
    let mut prompt = String::from("# Workspace context\n");
    prompt.push_str("Use this context to answer the Ask question. Cite workspace paths when you rely on them. If the answer is not supported by this context, say what is missing.\n\n");
    for section in sections {
        push_budgeted_section(&mut prompt, &section, context_budget);
        if estimate_tokens(&prompt) >= context_budget {
            break;
        }
    }
    prompt.push_str(&question_section);
    prompt = trim_to_token_budget(&prompt, max_tokens);

    selected_paths.sort();
    selected_paths.dedup();
    let context_tokens = estimate_tokens(&prompt).min(max_tokens);
    let summary = format!(
        "ctx: workspace {} {}, ~{} tok",
        selected_paths.len(),
        if selected_paths.len() == 1 {
            "file"
        } else {
            "files"
        },
        context_tokens
    );

    AskContextReport {
        prompt,
        context_tokens,
        selected_paths,
        summary,
    }
}

fn workspace_facts(root: &Path, files: &[PathBuf]) -> String {
    let mut out = String::from("# Workspace facts\n");
    out.push_str(&format!("root: {}\n", root.display()));
    for name in [
        "AGENTS.md",
        "README.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "vite.config.ts",
    ] {
        if root.join(name).exists() {
            out.push_str("- found ");
            out.push_str(name);
            out.push('\n');
        }
    }
    out.push_str(&format!("- indexed file candidates: {}\n", files.len()));
    out
}

fn file_map_section(root: &Path, files: &[PathBuf], limit: usize) -> String {
    let mut out = String::from("# Workspace file map\n");
    for path in files.iter().take(limit) {
        out.push_str("- ");
        out.push_str(&display_relative(root, path));
        out.push('\n');
    }
    out
}

fn collect_workspace_files(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        if out.len() >= limit {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if should_skip_name(name) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                queue.push_back(path);
            } else if kind.is_file() && is_context_file(&path) {
                out.push(path);
                if out.len() >= limit {
                    break;
                }
            }
        }
    }
    out.sort();
    out
}

fn top_lexical_snippets(
    root: &Path,
    files: &[PathBuf],
    question: &str,
    limit: usize,
) -> Vec<PathBuf> {
    let query = query_terms(question);
    if query.is_empty() {
        return files.iter().take(limit).cloned().collect();
    }
    let mut scored = Vec::new();
    for path in files.iter().take(48) {
        let rel = display_relative(root, path).to_ascii_lowercase();
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let haystack = format!("{}\n{}", rel, text.to_ascii_lowercase());
        let score = query
            .iter()
            .filter(|term| haystack.contains(term.as_str()))
            .count();
        if score > 0 {
            scored.push((score, path.clone()));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, path)| path)
        .collect()
}

fn snippet_section(root: &Path, path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let snippet: String = text.chars().take(1_200).collect();
    let mut out = format!("# Relevant file: {}\n", display_relative(root, path));
    out.push_str("<file-excerpt>\n");
    out.push_str(&snippet);
    if !snippet.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("</file-excerpt>\n");
    Some(out)
}

fn push_budgeted_section(out: &mut String, section: &str, max_tokens: usize) {
    let used = estimate_tokens(out);
    if used >= max_tokens {
        return;
    }
    let remaining = max_tokens - used;
    let trimmed = trim_to_token_budget(section, remaining);
    if !trimmed.is_empty() {
        out.push_str(&trimmed);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
}

fn trim_to_token_budget(text: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(24)).collect();
    out.push_str("\n[context truncated]\n");
    out
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count().saturating_add(3) / 4
}

fn query_terms(question: &str) -> HashSet<String> {
    question
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .map(|term| term.trim().to_ascii_lowercase())
        .filter(|term| term.len() >= 3)
        .collect()
}

fn should_skip_name(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "target"
                | "node_modules"
                | "dist"
                | "build"
                | "coverage"
                | "tmp"
                | "temp"
                | "out"
                | "__pycache__"
        )
}

fn is_context_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if matches!(
        name,
        "README.md" | "AGENTS.md" | "Cargo.toml" | "package.json"
    ) {
        return true;
    }
    matches!(
        path.extension().and_then(|s| s.to_str()).unwrap_or(""),
        "rs" | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "md"
            | "html"
            | "css"
    )
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(display_path)
        .unwrap_or_else(|_| display_path(path))
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use phonton_types::{PromptAttachment, PromptAttachmentKind};

    #[test]
    fn ask_prompt_includes_workspace_map_and_explicit_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("README.md"),
            "# Demo\nThis explains Ask context.\n",
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "pub fn ask_context() {}\n").unwrap();

        let attachment = PromptAttachment {
            path: "README.md".into(),
            kind: PromptAttachmentKind::Text,
            mime_type: Some("text/markdown".into()),
            size_bytes: 34,
            text: Some("# Demo\nThis explains Ask context.\n".into()),
            data_base64: None,
            truncated: false,
            note: None,
        };

        let report = build_ask_context(AskContextRequest {
            question: "what does ask context do?",
            workspace_root: temp.path(),
            attachments: &[attachment],
            current_goal: None,
            diagnostics: &[],
            max_tokens: 1_200,
        });

        assert!(report.prompt.contains("# Workspace context"));
        assert!(report.prompt.contains("README.md"));
        assert!(report.prompt.contains("src/lib.rs"));
        assert!(report.prompt.contains("This explains Ask context."));
        assert!(report.prompt.contains("Cite workspace paths"));
        assert!(report.context_tokens <= 1_200);
        assert!(report.summary.starts_with("ctx: workspace"));
    }

    #[test]
    fn ask_context_includes_goal_diagnostics_and_stays_bounded() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("package.json"),
            "{\"scripts\":{\"test\":\"vitest\"}}\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("huge.ts"),
            "export const value = 1;\n".repeat(2_000),
        )
        .unwrap();

        let diagnostics = vec!["[typescript syntax] src/app.ts: missing semicolon".to_string()];
        let report = build_ask_context(AskContextRequest {
            question: "why did it fail?",
            workspace_root: temp.path(),
            attachments: &[],
            current_goal: Some("build playable chess"),
            diagnostics: &diagnostics,
            max_tokens: 350,
        });

        assert!(report.prompt.contains("Current goal: build playable chess"));
        assert!(report.prompt.contains("missing semicolon"));
        assert!(report.context_tokens <= 350);
    }

    #[test]
    fn stateless_prompt_preserves_old_no_workspace_shape() {
        let prompt = build_stateless_ask_prompt("what is this?");

        assert_eq!(prompt, "what is this?");
    }
}
