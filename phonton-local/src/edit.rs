//! Strict conversion from small-model search/replace output to canonical hunks.
//! Models cannot silently summarize the old side or choose a path outside scope.

use crate::{LocalError, Result};
use phonton_types::{DiffHunk, DiffLine};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Edit {
    pub(crate) path: String,
    pub(crate) search: String,
    #[serde(alias = "text")]
    pub(crate) replace: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateEdit {
    path: String,
    text: String,
}

/// Small models often write a regex `\b` (word boundary) or `\f` inside a JSON
/// string with one backslash, which JSON decodes as a backspace or form-feed
/// control character. Source code essentially never contains those raw
/// characters, so keep them as a literal backslash + letter instead. Already
/// escaped pairs (`\\b`) are left untouched.
pub(crate) fn preserve_code_escapes(response: &str) -> std::borrow::Cow<'_, str> {
    if !response.contains("\\b") && !response.contains("\\f") {
        return std::borrow::Cow::Borrowed(response);
    }
    let mut out = String::with_capacity(response.len() + 8);
    let mut chars = response.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(next @ ('b' | 'f')) => {
                out.push_str("\\\\");
                out.push(next);
            }
            Some(next) => {
                out.push('\\');
                out.push(next);
            }
            None => out.push('\\'),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Parse and normalize the exact creation transport without touching a file.
/// Calibration uses this same contract before any repository goal relies on it.
pub(crate) fn parse_creation_text(target: &Path, response: &str) -> Result<String> {
    if response.len() > 1024 * 1024 {
        return Err(LocalError::Invalid("Creation output exceeds 1 MiB".into()));
    }
    let edit: CreateEdit = serde_json::from_str(&preserve_code_escapes(response))?;
    let path = safe_relative_path(&edit.path)?;
    if path != target {
        return Err(LocalError::Invalid(
            "Creation path differs from explicit scope".into(),
        ));
    }
    let mut content = edit.text;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    Ok(content)
}

/// Turn a constrained whole-file proposal into exact hunks for the one
/// explicitly requested target. A repair may replace a prior candidate's
/// created file; it cannot choose another path or write directly to disk.
/// A missing final line separator is added deterministically; the candidate
/// hash and canonical diff identify those normalized bytes, while raw output
/// remains available in the receipt.
pub fn create_json_hunks(root: &Path, target: &Path, response: &str) -> Result<Vec<DiffHunk>> {
    let content = parse_creation_text(target, response)?;
    if content.is_empty() {
        return Err(LocalError::Invalid(
            "Creation output must be nonempty".into(),
        ));
    }
    let path = target;
    let root = std::fs::canonicalize(root)?;
    let mut cursor = root.clone();
    let parts: Vec<_> = path.components().collect();
    let mut present = false;
    for (index, part) in parts.iter().enumerate() {
        cursor.push(part.as_os_str());
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(LocalError::Invalid(
                        "Linked creation targets are unsupported".into(),
                    ));
                }
                if index + 1 == parts.len() {
                    if !metadata.is_file() {
                        return Err(LocalError::Invalid(
                            "Creation target is not a regular file".into(),
                        ));
                    }
                    present = true;
                } else if !metadata.is_dir() {
                    return Err(LocalError::Invalid(
                        "Creation parent is not a directory".into(),
                    ));
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && index + 1 == parts.len() => {}
            Err(error) => return Err(error.into()),
        }
    }
    if present {
        let original = std::fs::read_to_string(cursor)?;
        canonical_change(path, &original, &content)
    } else {
        canonical_new_file(path, &content)
    }
}

/// Convert one measured search/replace response. The scope is supplied by the
/// harness, not by the model. This function never writes files or runs commands.
pub fn search_replace_hunks(
    root: &Path,
    allowed: &[PathBuf],
    response: &str,
) -> Result<Vec<DiffHunk>> {
    if response.len() > 1024 * 1024 {
        return Err(LocalError::Invalid("Edit output exceeds 1 MiB".into()));
    }
    let edit: Edit = serde_json::from_str(&preserve_code_escapes(response))?;
    let relative = safe_relative_path(&edit.path)?;
    if !allowed.iter().any(|path| path == &relative) {
        return Err(LocalError::Invalid(format!(
            "{} is outside the approved file scope",
            edit.path
        )));
    }
    let root = std::fs::canonicalize(root)?;
    let path = std::fs::canonicalize(root.join(&relative))?;
    if !path.starts_with(&root) {
        return Err(LocalError::Invalid(
            "Edit target resolves outside the workspace".into(),
        ));
    }
    // Reject links even within scope: candidate copies must not change identity
    // or write through a link into another candidate or the source repository.
    let mut cursor = root.clone();
    for part in relative.components() {
        cursor.push(part);
        if std::fs::symlink_metadata(&cursor)?.file_type().is_symlink() {
            return Err(LocalError::Invalid(
                "Symbolic-link edit targets are unsupported".into(),
            ));
        }
    }
    if std::fs::metadata(&path)?.len() > 1024 * 1024 {
        return Err(LocalError::Invalid(
            "Edit target exceeds 1 MiB; narrow the task".into(),
        ));
    }
    let original = std::fs::read_to_string(&path)?;
    if edit.search.is_empty() || original.matches(&edit.search).count() != 1 {
        return Err(LocalError::Invalid(
            "Search text must occur exactly once in the original file. Include more exact context."
                .into(),
        ));
    }
    if edit.search == edit.replace {
        return Err(LocalError::Invalid("Edit makes no change".into()));
    }
    let search = block_anchor_search(&original, &edit.search, &edit.replace, &relative)
        .unwrap_or(&edit.search);
    if let Some(problem) = unbalanced_replacement(search, &edit.replace, &relative) {
        return Err(LocalError::Invalid(problem));
    }
    let updated = original.replacen(search, &edit.replace, 1);
    if updated.len() > 1024 * 1024 || original.contains('\0') || updated.contains('\0') {
        return Err(LocalError::Invalid(
            "Edit is binary or exceeds the file size budget".into(),
        ));
    }
    canonical_change(&relative, &original, &updated)
}

/// Small models often answer with only a block's opening line as `search`
/// (`class OrderBook {`, `def parse(line):`) and the whole rewritten block as
/// the replacement. Replacing just that line duplicates the old body. When
/// the replacement starts with the anchor line and is itself one complete
/// block, return the original text of the whole block the anchor opens
/// instead. An insertion after an opening line (an unclosed replacement)
/// keeps the exact-search meaning.
fn block_anchor_search<'a>(
    original: &'a str,
    search: &str,
    replace: &str,
    path: &Path,
) -> Option<&'a str> {
    let anchor = search.trim_end_matches(['\r', '\n']);
    if anchor.contains('\n')
        || anchor.trim().is_empty()
        || !replace.starts_with(anchor)
        || replace.lines().count() < 2
    {
        return None;
    }
    let start = original.find(search)?;
    let is_python = path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("py"));
    let end = match brace_language(path) {
        None if is_python => python_block_end(original, start, replace)?,
        None => return None,
        Some(rust) => {
            // The anchor must open a block and the replacement must open and
            // close exactly one.
            if brace_balance(anchor, rust).is_none_or(|(depth, _)| depth == 0)
                || brace_balance(replace, rust) != Some((0, true))
            {
                return None;
            }
            let close = start + block_close(&original[start..], rust)?;
            // Keep `);` after the closing brace unless the replacement wrote it.
            let line_end = original[close..]
                .find('\n')
                .map_or(original.len(), |offset| close + offset);
            let tail = original[close..line_end].trim_end();
            if !tail.trim().is_empty() && replace.trim_end().ends_with(tail) {
                line_end
            } else {
                close
            }
        }
    };
    (end > start + search.len()).then(|| &original[start..end])
}

/// Brace-delimited source by extension; `Some(true)` for Rust, whose `'` is
/// a lifetime as often as a quote.
fn brace_language(path: &Path) -> Option<bool> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "js" | "mjs"
            | "cjs"
            | "jsx"
            | "ts"
            | "mts"
            | "cts"
            | "tsx"
            | "rs"
            | "go"
            | "java"
            | "kt"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "hpp"
            | "cs"
            | "swift"
            | "php"
            | "scala"
    )
    .then_some(extension == "rs")
}

/// Replacing text with text that opens or closes a different number of
/// braces always leaves the file unbalanced, usually because the model cut
/// its replacement off. Name that instead of waiting for a parser error.
fn unbalanced_replacement(search: &str, replace: &str, path: &Path) -> Option<String> {
    let rust = brace_language(path)?;
    // A regex like /\{/ is outside what this scanner understands.
    if [search, replace]
        .iter()
        .any(|text| text.contains("\\{") || text.contains("\\}"))
    {
        return None;
    }
    let net = |text: &str| {
        let mut depth = 0;
        walk_braces(text, rust, |_, after| {
            depth = after;
            true
        });
        depth
    };
    let (before, after) = (net(search), net(replace));
    (before != after).then(|| {
        if after > before {
            format!(
                "The replacement text leaves {} more brace(s) open than the text it replaces; it looks cut off. Return the complete function or block as text.",
                after - before
            )
        } else {
            format!(
                "The replacement text closes {} more brace(s) than the text it replaces. Replace a complete function or block.",
                before - after
            )
        }
    })
}

/// Visit each brace outside strings and comments with its byte offset and the
/// depth after it, until `visit` returns false.
fn walk_braces(text: &str, rust: bool, mut visit: impl FnMut(usize, i64) -> bool) {
    let mut depth = 0i64;
    let mut chars = text.char_indices().peekable();
    while let Some((offset, c)) = chars.next() {
        let quote = match c {
            '{' | '}' => {
                depth += if c == '{' { 1 } else { -1 };
                if !visit(offset, depth) {
                    return;
                }
                continue;
            }
            '/' if chars.peek().is_some_and(|(_, n)| *n == '/') => {
                for (_, n) in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                continue;
            }
            '/' if chars.peek().is_some_and(|(_, n)| *n == '*') => {
                chars.next();
                let mut star = false;
                for (_, n) in chars.by_ref() {
                    if star && n == '/' {
                        break;
                    }
                    star = n == '*';
                }
                continue;
            }
            // Rust uses ' for lifetimes, so only " strings are skipped there.
            '"' | '`' => c,
            '\'' if !rust => c,
            _ => continue,
        };
        let mut escaped = false;
        for (_, n) in chars.by_ref() {
            if escaped {
                escaped = false;
            } else if n == '\\' {
                escaped = true;
            } else if n == quote || (n == '\n' && quote != '`') {
                break;
            }
        }
    }
}

/// Net brace depth of `text` and whether it opened one; `None` if a brace
/// closes one it never opened.
fn brace_balance(text: &str, rust: bool) -> Option<(i64, bool)> {
    let (mut depth, mut opened, mut valid) = (0, false, true);
    walk_braces(text, rust, |_, after| {
        opened |= after > depth;
        depth = after;
        valid = after >= 0;
        valid
    });
    valid.then_some((depth, opened))
}

/// Byte offset just past the brace that closes the first one `text` opens.
fn block_close(text: &str, rust: bool) -> Option<usize> {
    let mut close = None;
    walk_braces(text, rust, |offset, after| {
        if after == 0 {
            close = Some(offset + 1);
        }
        after > 0
    });
    close
}

/// End of the indented Python block opened by the anchor line: just past the
/// last nonblank line indented deeper than it. The replacement must be one
/// such block.
fn python_block_end(original: &str, start: usize, replace: &str) -> Option<usize> {
    let indent = |line: &str| line.len() - line.trim_start().len();
    let line_start = original[..start].rfind('\n').map_or(0, |i| i + 1);
    let anchor = original[line_start..].split_inclusive('\n').next()?;
    if !anchor.trim_end().ends_with(':') {
        return None;
    }
    let base = indent(anchor);
    if replace
        .lines()
        .skip(1)
        .any(|line| !line.trim().is_empty() && indent(line) <= base)
    {
        return None;
    }
    let mut position = line_start + anchor.len();
    let mut end = None;
    for line in original[position..].split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        if !text.trim().is_empty() {
            if indent(text) <= base {
                break;
            }
            end = Some(position + text.len());
        }
        position += line.len();
    }
    end
}

/// Render a complete existing-file change against captured baseline bytes.
/// Identical inputs produce no hunks. Nonempty edits preserve the original
/// final-newline state; this function performs no filesystem I/O.
pub fn canonical_change(relative: &Path, original: &str, updated: &str) -> Result<Vec<DiffHunk>> {
    let relative = safe_relative_path(&relative.to_string_lossy().replace('\\', "/"))?;
    if original.len() > 1024 * 1024
        || updated.len() > 1024 * 1024
        || original.contains('\0')
        || updated.contains('\0')
    {
        return Err(LocalError::Invalid(
            "Canonical change requires bounded source without NUL bytes".into(),
        ));
    }
    if original.is_empty() && !updated.ends_with('\n') {
        return Err(LocalError::Invalid(
            "An empty existing file requires newline-terminated replacement text".into(),
        ));
    }
    if !original.is_empty()
        && !updated.is_empty()
        && original.ends_with('\n') != updated.ends_with('\n')
    {
        return Err(LocalError::Invalid(
            "Existing-file edits must preserve the final newline state".into(),
        ));
    }
    if original == updated {
        return Ok(Vec::new());
    }
    // Keep carriage returns in canonical hunk lines so CRLF files are not
    // silently normalized by the alternate editing protocol.
    let before: Vec<&str> = original.split_terminator('\n').collect();
    let after: Vec<&str> = updated.split_terminator('\n').collect();
    // A final unterminated line differs from the same text followed by LF.
    // This matters when a new line is appended after the old EOF line.
    let same_line = |old: usize, new: usize| {
        before[old] == after[new]
            && (!original.ends_with('\n') && old + 1 == before.len())
                == (!updated.ends_with('\n') && new + 1 == after.len())
    };
    let prefix = (0..before.len().min(after.len()))
        .take_while(|&index| same_line(index, index))
        .count();
    let suffix = (0..(before.len() - prefix).min(after.len() - prefix))
        .take_while(|&offset| same_line(before.len() - 1 - offset, after.len() - 1 - offset))
        .count();
    if prefix == before.len() && prefix == after.len() {
        return Err(LocalError::Invalid(
            "Edit changes only line endings; no canonical text change".into(),
        ));
    }
    let start = prefix.saturating_sub(3);
    let old_end = (before.len() - suffix + 3).min(before.len());
    let new_end = (after.len() - suffix + 3).min(after.len());
    let mut lines: Vec<DiffLine> = before[start..prefix]
        .iter()
        .map(|s| DiffLine::Context((*s).into()))
        .collect();
    lines.extend(
        before[prefix..before.len() - suffix]
            .iter()
            .map(|s| DiffLine::Removed((*s).into())),
    );
    lines.extend(
        after[prefix..after.len() - suffix]
            .iter()
            .map(|s| DiffLine::Added((*s).into())),
    );
    lines.extend(
        before[before.len() - suffix..old_end]
            .iter()
            .map(|s| DiffLine::Context((*s).into())),
    );
    Ok(vec![DiffHunk {
        file_path: relative,
        old_start: if old_end == start {
            start as u32
        } else {
            start as u32 + 1
        },
        old_count: (old_end - start) as u32,
        new_start: if new_end == start {
            start as u32
        } else {
            start as u32 + 1
        },
        new_count: (new_end - start) as u32,
        lines,
    }])
}

/// Render one explicitly scoped new text file against absence, not an empty
/// existing file. The caller separately proves the target was absent.
pub fn canonical_new_file(relative: &Path, content: &str) -> Result<Vec<DiffHunk>> {
    let path = safe_relative_path(&relative.to_string_lossy().replace('\\', "/"))?;
    if content.is_empty()
        || content.len() > 1024 * 1024
        || content.contains('\0')
        || !content.ends_with('\n')
    {
        return Err(LocalError::Invalid(
            "New source requires bounded nonempty newline-terminated text".into(),
        ));
    }
    let lines: Vec<_> = content
        .split_terminator('\n')
        .map(|line| DiffLine::Added(line.into()))
        .collect();
    Ok(vec![DiffHunk {
        file_path: path,
        old_start: 0,
        old_count: 0,
        new_start: 1,
        new_count: lines.len() as u32,
        lines,
    }])
}

/// Validate portable repository-relative paths, including Windows ADS/device
/// names and trailing-dot aliases that a Unix-only traversal check would miss.
pub fn safe_relative_path(raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    if raw.is_empty()
        || raw.contains(['\\', ':', '\0'])
        || path.is_absolute()
        || path
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
        || raw.split('/').any(|part| {
            let lower = part.to_ascii_lowercase();
            let stem = lower.split('.').next().unwrap_or("");
            part.is_empty()
                || part.ends_with(['.', ' '])
                || matches!(lower.as_str(), ".git" | ".ssh" | ".aws")
                || lower.starts_with(".env")
                || matches!(stem, "con" | "prn" | "aux" | "nul")
                || (stem.len() == 4
                    && (stem.starts_with("com") || stem.starts_with("lpt"))
                    && stem.as_bytes()[3].is_ascii_digit())
        })
    {
        return Err(LocalError::Invalid(format!("Unsafe edit path: {raw}")));
    }
    Ok(path.to_path_buf())
}

/// Apply canonical hunks in memory at exact original offsets. No fuzzy match,
/// summarized old side, path alias, overlapping hunk, or implicit file creation.
/// The caller writes these bytes only into an isolated candidate directory.
pub fn materialize_hunks(
    root: &Path,
    allowed: &[PathBuf],
    hunks: &[DiffHunk],
) -> Result<std::collections::BTreeMap<PathBuf, String>> {
    materialize(root, allowed, hunks, false)
}

/// Materialize exact hunks with explicitly scoped new-file creation permitted.
/// Existing files still require an exact old side; an addition never replaces
/// an existing file implicitly. No filesystem mutation happens here.
pub fn materialize_hunks_with_new_files(
    root: &Path,
    allowed: &[PathBuf],
    hunks: &[DiffHunk],
) -> Result<std::collections::BTreeMap<PathBuf, String>> {
    materialize(root, allowed, hunks, true)
}

fn materialize(
    root: &Path,
    allowed: &[PathBuf],
    hunks: &[DiffHunk],
    allow_new: bool,
) -> Result<std::collections::BTreeMap<PathBuf, String>> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<PathBuf, Vec<&DiffHunk>> = BTreeMap::new();
    if hunks.is_empty() {
        return Err(LocalError::Invalid("No edits were produced".into()));
    }
    for hunk in hunks {
        if hunk.lines.iter().any(|line| {
            let text = match line {
                DiffLine::Added(text) | DiffLine::Removed(text) | DiffLine::Context(text) => text,
            };
            text.contains(['\n', '\0'])
        }) {
            return Err(LocalError::Invalid(
                "Canonical hunk lines cannot contain embedded newlines or NUL bytes".into(),
            ));
        }
        let raw = hunk.file_path.to_string_lossy().replace('\\', "/");
        let path = safe_relative_path(&raw)?;
        if !allowed.contains(&path) {
            return Err(LocalError::Invalid(format!("Out of scope: {raw}")));
        }
        groups.entry(path).or_default().push(hunk);
    }
    let root = std::fs::canonicalize(root)?;
    let mut result = BTreeMap::new();
    for (path, mut group) in groups {
        let full = root.join(&path);
        let mut cursor = root.clone();
        for part in path.components() {
            cursor.push(part);
            let metadata = match std::fs::symlink_metadata(&cursor) {
                Ok(metadata) => metadata,
                Err(error) if allow_new && error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            };
            if metadata.file_type().is_symlink()
                || !std::fs::canonicalize(&cursor)?.starts_with(&root)
            {
                return Err(LocalError::Invalid(
                    "Linked edit targets are unsupported".into(),
                ));
            }
        }
        let is_new = !full.exists();
        let original = if is_new && allow_new {
            String::new()
        } else {
            if std::fs::metadata(&full)?.len() > 1024 * 1024 {
                return Err(LocalError::Invalid("Edit file exceeds 1 MiB".into()));
            }
            std::fs::read_to_string(&full)?
        };
        if original.contains('\0') {
            return Err(LocalError::Invalid(
                "Edits require text without NUL bytes".into(),
            ));
        }
        let raw_lines: Vec<&str> = original.split_terminator('\n').collect();
        // Unified diffs describe line text, while CRLF is a file encoding
        // detail. Accept both parser-produced lines (without CR) and
        // canonical_change lines (with CR), then retain the original style.
        // Mixed endings still require exact byte-for-byte hunk lines.
        let terminated =
            raw_lines.len() - usize::from(!original.is_empty() && !original.ends_with('\n'));
        let crlf = terminated > 0
            && raw_lines[..terminated]
                .iter()
                .all(|line| line.ends_with('\r'));
        let lines: Vec<&str> = raw_lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                if crlf && index < terminated {
                    line.strip_suffix('\r').unwrap_or(line)
                } else {
                    line
                }
            })
            .collect();
        group.sort_by_key(|h| h.old_start);
        let new_total = group
            .iter()
            .try_fold(lines.len(), |count, hunk| {
                count
                    .checked_add(hunk.new_count as usize)
                    .and_then(|next| next.checked_sub(hunk.old_count as usize))
            })
            .ok_or_else(|| LocalError::Invalid("Invalid candidate line count".into()))?;
        let new_terminated =
            new_total - usize::from(!is_new && !original.ends_with('\n') && new_total > 0);
        let mut consumed = 0usize;
        let mut updated = Vec::new();
        for hunk in group {
            let start = if is_new && hunk.old_count == 0 && hunk.old_start <= 1 {
                0
            } else if hunk.old_count == 0 {
                hunk.old_start as usize
            } else {
                hunk.old_start
                    .checked_sub(1)
                    .ok_or_else(|| LocalError::Invalid("Invalid hunk offset".into()))?
                    as usize
            };
            let mut old_cursor = start;
            let old: Vec<&str> = hunk
                .lines
                .iter()
                .filter_map(|l| match l {
                    DiffLine::Context(s) | DiffLine::Removed(s) => {
                        let index = old_cursor;
                        old_cursor += 1;
                        Some(if crlf && index < terminated {
                            s.strip_suffix('\r').unwrap_or(s)
                        } else {
                            s.as_str()
                        })
                    }
                    _ => None,
                })
                .collect();
            let mut new_cursor = hunk.new_start.saturating_sub(1) as usize;
            let new: Vec<&str> = hunk
                .lines
                .iter()
                .filter_map(|l| match l {
                    DiffLine::Context(s) | DiffLine::Added(s) => {
                        let index = new_cursor;
                        new_cursor += 1;
                        Some(if crlf && index < new_terminated {
                            s.strip_suffix('\r').unwrap_or(s)
                        } else {
                            s.as_str()
                        })
                    }
                    _ => None,
                })
                .collect();
            if old.len() != hunk.old_count as usize
                || new.len() != hunk.new_count as usize
                || start < consumed
                || start + old.len() > lines.len()
                || lines[start..start + old.len()] != old
            {
                return Err(LocalError::Invalid(format!(
                    "Exact hunk verification failed for {}",
                    path.display()
                )));
            }
            updated.extend_from_slice(&lines[consumed..start]);
            let expected_new_start = if new.is_empty() {
                updated.len()
            } else {
                updated.len() + 1
            };
            if hunk.new_start as usize != expected_new_start {
                return Err(LocalError::Invalid(
                    "New-side hunk offset does not identify the resulting candidate".into(),
                ));
            }
            updated.extend(new);
            consumed = start + old.len();
        }
        updated.extend_from_slice(&lines[consumed..]);
        let ending = if crlf { "\r\n" } else { "\n" };
        let content = if updated.is_empty() {
            String::new()
        } else if is_new || original.is_empty() || original.ends_with('\n') {
            format!("{}{}", updated.join(ending), ending)
        } else {
            updated.join(ending)
        };
        if content == original || content.len() > 1024 * 1024 {
            return Err(LocalError::Invalid(
                "No change or file budget exceeded".into(),
            ));
        }
        result.insert(path, content);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply one search/replace response to `original` saved as `name`.
    fn apply(name: &str, original: &str, search: &str, text: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from(name);
        std::fs::write(dir.path().join(&path), original).unwrap();
        let raw = serde_json::json!({"path": name, "search": search, "text": text}).to_string();
        let scope = [path.clone()];
        let hunks = search_replace_hunks(dir.path(), &scope, &raw).unwrap();
        materialize_hunks(dir.path(), &scope, &hunks).unwrap()[&path].clone()
    }

    #[test]
    fn an_opening_line_anchor_rewrites_the_whole_block() {
        let original =
            "function f(a) {\n  if (a) { return '}'; }\n  return a;\n}\nfunction g() {}\n";
        let updated = apply(
            "f.js",
            original,
            "function f(a) {",
            "function f(a) {\n  return a + 1;\n}",
        );
        assert_eq!(
            updated,
            "function f(a) {\n  return a + 1;\n}\nfunction g() {}\n"
        );
    }

    #[test]
    fn an_unclosed_replacement_after_an_anchor_is_an_insertion() {
        let original = "class A {\n  b() {}\n}\n";
        let updated = apply("a.ts", original, "class A {", "class A {\n  a() {\n  }\n");
        assert_eq!(updated, "class A {\n  a() {\n  }\n\n  b() {}\n}\n");
    }

    #[test]
    fn a_block_closing_mid_line_keeps_its_tail_unless_rewritten() {
        let original = "describe('x', () => {\n  it('a');\n});\nrun();\n";
        let kept = apply(
            "t.js",
            original,
            "describe('x', () => {",
            "describe('x', () => {\n  it('b');\n}",
        );
        assert_eq!(kept, "describe('x', () => {\n  it('b');\n});\nrun();\n");
        let rewritten = apply(
            "t.js",
            original,
            "describe('x', () => {",
            "describe('x', () => {\n  it('b');\n});",
        );
        assert_eq!(rewritten, kept);
    }

    #[test]
    fn rust_lifetimes_do_not_hide_braces() {
        let original = "fn f<'a>(x: &'a str) -> &'a str {\n    x\n}\nfn g() {}\n";
        let updated = apply(
            "lib.rs",
            original,
            "fn f<'a>(x: &'a str) -> &'a str {",
            "fn f<'a>(x: &'a str) -> &'a str {\n    x.trim()\n}",
        );
        assert_eq!(
            updated,
            "fn f<'a>(x: &'a str) -> &'a str {\n    x.trim()\n}\nfn g() {}\n"
        );
    }

    #[test]
    fn a_python_def_anchor_rewrites_its_indented_body() {
        let original =
            "def f(a):\n    if a:\n        return 1\n\n    return 2\n\n\ndef g():\n    pass\n";
        let updated = apply("m.py", original, "def f(a):", "def f(a):\n    return 3");
        assert_eq!(updated, "def f(a):\n    return 3\n\n\ndef g():\n    pass\n");
        // A replacement that also starts a second top-level block is ambiguous.
        let both = apply(
            "m.py",
            original,
            "def f(a):",
            "def f(a):\n    return 3\n\ndef h():\n    pass",
        );
        assert!(both.matches("return 2").count() == 1, "{both}");
    }

    #[test]
    fn a_cut_off_replacement_is_named_before_any_check_runs() {
        // qwen2.5-coder:3b on shop-top-ties stopped mid-function; the parser
        // error ("Unexpected end of input") pointed nowhere useful.
        let original = include_str!("../../fixtures/shop/src/shop.js");
        let start = original.find("function topProducts").unwrap();
        let end = start + original[start..].find("\n}\n").unwrap() + 2;
        let search = &original[start..end];
        let cut = &search[..search.find("    .map(").unwrap()];
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("shop.js");
        std::fs::write(dir.path().join(&path), original).unwrap();
        let raw = serde_json::json!({"path": "shop.js", "search": search, "text": cut}).to_string();
        let error = search_replace_hunks(dir.path(), &[path], &raw)
            .unwrap_err()
            .to_string();
        assert!(error.contains("leaves 1 more brace(s) open"), "{error}");
    }

    #[test]
    fn real_3b_class_rewrite_no_longer_duplicates_the_class() {
        // qwen2.5-coder:3b on shop-cancel-restock: `search` is the class's
        // opening line and `text` the whole rewritten class. Exact replacement
        // duplicated the body and every candidate failed with a SyntaxError.
        let original = include_str!("../../fixtures/shop/src/shop.js");
        let raw = include_str!("../testdata/shop-anchor-rewrite.json");
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("src/shop.js");
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join(&path), original).unwrap();
        let scope = [path.clone()];
        let hunks = search_replace_hunks(dir.path(), &scope, raw).unwrap();
        let updated = &materialize_hunks(dir.path(), &scope, &hunks).unwrap()[&path];
        assert_eq!(updated.matches("class OrderBook {").count(), 1);
        assert_eq!(updated.matches("  constructor(stock) {").count(), 1);
        assert_eq!(brace_balance(updated, false), Some((0, true)));
        // Everything around the class is untouched.
        assert!(updated.starts_with(&original[..original.find("class OrderBook {").unwrap()]));
        assert!(updated.contains("function dailySales(orders) {"));
        assert!(updated.ends_with(&original[original.find("function dailySales").unwrap()..]));
    }

    #[test]
    fn under_escaped_regex_boundaries_stay_regex_boundaries() {
        // Verbatim qwen2.5-coder:3b output: `\b` meant a regex word boundary,
        // but strict JSON decodes it as a backspace (0x08).
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("port.js");
        std::fs::write(
            dir.path().join(&path),
            "  const port = Number.parseInt(input, 10);\n",
        )
        .unwrap();
        let raw = r#"{"path":"port.js","search":"  const port = Number.parseInt(input, 10);","text":"  if (!/^\b[0-9]{1,5}\b$/.test(input)) throw 1;\n  const port = Number.parseInt(input, 10);"}"#;
        let scope = [path.clone()];
        let hunks = search_replace_hunks(dir.path(), &scope, raw).unwrap();
        let out = materialize_hunks(dir.path(), &scope, &hunks).unwrap();
        assert!(out[&path].contains(r"/^\b[0-9]{1,5}\b$/"));
        assert!(!out[&path].contains('\u{8}'));
        // A properly escaped backslash pair and real newlines are unchanged.
        assert_eq!(preserve_code_escapes(r#"a\\b\nc"#), r#"a\\b\nc"#);
    }

    #[test]
    fn canonical_creation_is_distinct_from_an_empty_existing_file() {
        let root = tempfile::tempdir().unwrap();
        let path = PathBuf::from("new.py");
        let hunks = canonical_new_file(&path, "value = 1\n").unwrap();
        assert_eq!(hunks[0].old_start, 0);
        assert_eq!(hunks[0].old_count, 0);
        assert_eq!(hunks[0].new_count, 1);
        let made =
            materialize_hunks_with_new_files(root.path(), std::slice::from_ref(&path), &hunks)
                .unwrap();
        assert_eq!(made[&path], "value = 1\n");
        assert!(materialize_hunks(root.path(), std::slice::from_ref(&path), &hunks).is_err());
        assert!(
            materialize_hunks_with_new_files(root.path(), &["other.py".into()], &hunks).is_err()
        );
        assert!(canonical_new_file(&path, "").is_err());
        assert!(canonical_new_file(&path, "no final newline").is_err());
    }
    #[test]
    fn constrained_json_creation_only_targets_the_requested_path() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        let target = PathBuf::from("src/new.py");
        let first = create_json_hunks(
            root.path(),
            &target,
            r#"{"path":"src/new.py","text":"value = 1"}"#,
        )
        .unwrap();
        let content =
            materialize_hunks_with_new_files(root.path(), std::slice::from_ref(&target), &first)
                .unwrap();
        assert_eq!(content[&target], "value = 1\n");
        std::fs::write(root.path().join(&target), "value = 1\n").unwrap();
        let repair = create_json_hunks(
            root.path(),
            &target,
            r#"{"path":"src/new.py","text":"value = 2"}"#,
        )
        .unwrap();
        assert_eq!(
            materialize_hunks_with_new_files(root.path(), std::slice::from_ref(&target), &repair)
                .unwrap()[&target],
            "value = 2\n"
        );
        for bad in [
            r#"{"path":"src/other.py","text":"value = 1\n"}"#,
            r#"{"path":"../new.py","text":"value = 1\n"}"#,
            r#"{"path":"src/new.py","text":"value = 1\n","command":"skip tests"}"#,
            r#"{"path":"src/new.py","text":""}"#,
        ] {
            assert!(
                create_json_hunks(root.path(), &target, bad).is_err(),
                "accepted {bad}"
            );
        }
    }
    #[test]
    fn rejects_cross_platform_escape_and_device_paths() {
        for path in [
            "../outside",
            "/outside",
            "C:/outside",
            "src/file:stream",
            "src/../x",
            "src\\..\\x",
            ".git/config",
            ".env.local",
            "aux.txt",
            "src/NUL",
            "src/file.",
        ] {
            assert!(safe_relative_path(path).is_err(), "accepted {path}");
        }
        assert_eq!(
            safe_relative_path("src/lib.rs").unwrap(),
            PathBuf::from("src/lib.rs")
        );
    }
    #[test]
    fn exact_edit_preserves_other_lines_and_never_writes_source() {
        let dir = tempfile::tempdir().unwrap();
        let before = "# calculate\ndef add(a, b): return a - b\n# keep this\n";
        std::fs::write(dir.path().join("add.py"), before).unwrap();
        let result = search_replace_hunks(
            dir.path(),
            &["add.py".into()],
            r#"{"path":"add.py","search":"return a - b","replace":"return a + b"}"#,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("add.py")).unwrap(),
            before
        );
        assert_eq!(result[0].old_count, 3);
        assert_eq!(result[0].new_count, 3);
        assert!(matches!(&result[0].lines[0], DiffLine::Context(s) if s == "# calculate"));
    }

    #[test]
    fn existing_source_without_final_newline_keeps_exact_eof_in_both_edit_protocols() {
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("math.js");
        let before = "const keep = 1;\nexport const add = (a, b) => a - b";
        let after = "const keep = 1;\nexport const add = (a, b) => a + b";
        std::fs::write(dir.path().join(&path), before).unwrap();
        let scope = [path.clone()];

        let search_hunks = search_replace_hunks(
            dir.path(),
            &scope,
            r#"{"path":"math.js","search":"a - b","text":"a + b"}"#,
        )
        .unwrap();
        let materialized = materialize_hunks(dir.path(), &scope, &search_hunks).unwrap();
        assert_eq!(materialized[&path], after);
        assert_eq!(
            canonical_change(&path, before, after).unwrap(),
            search_hunks
        );

        let diff_hunks = [DiffHunk {
            file_path: path.clone(),
            old_start: 2,
            old_count: 1,
            new_start: 2,
            new_count: 1,
            lines: vec![
                DiffLine::Removed("export const add = (a, b) => a - b".into()),
                DiffLine::Added("export const add = (a, b) => a + b".into()),
            ],
        }];
        assert_eq!(
            materialize_hunks(dir.path(), &scope, &diff_hunks).unwrap()[&path],
            after
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&path)).unwrap(),
            before
        );
    }
    #[test]
    fn appending_after_an_unterminated_line_records_its_changed_eof_role() {
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("code.txt");
        std::fs::write(dir.path().join(&path), "first").unwrap();
        let hunks = canonical_change(&path, "first", "first\nsecond").unwrap();
        assert!(matches!(&hunks[0].lines[0], DiffLine::Removed(text) if text == "first"));
        assert!(matches!(&hunks[0].lines[1], DiffLine::Added(text) if text == "first"));
        assert_eq!(
            materialize_hunks(dir.path(), std::slice::from_ref(&path), &hunks).unwrap()[&path],
            "first\nsecond"
        );
        assert!(canonical_change(&path, "first", "first\n").is_err());
        let empty_path = PathBuf::from("empty.txt");
        std::fs::write(dir.path().join(&empty_path), "").unwrap();
        let empty_hunks = canonical_change(&empty_path, "", "first\n").unwrap();
        assert_eq!(
            materialize_hunks(dir.path(), std::slice::from_ref(&empty_path), &empty_hunks).unwrap()
                [&empty_path],
            "first\n"
        );
        assert!(canonical_change(&path, "", "first").is_err());
        assert!(search_replace_hunks(
            dir.path(),
            &[path],
            r#"{"path":"code.txt","search":"first","replace":"first\n"}"#,
        )
        .is_err());
    }
    #[test]
    fn unterminated_terminal_carriage_return_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("code.txt");
        let scope = [path.clone()];
        for (before, after) in [
            ("first\r\nlast\r", "changed\r\nlast\r"),
            ("first\r\nlast\r", "first\r\ntail\r"),
            ("last\r", "tail\r"),
        ] {
            std::fs::write(dir.path().join(&path), before).unwrap();
            let hunks = canonical_change(&path, before, after).unwrap();
            assert_eq!(
                materialize_hunks(dir.path(), &scope, &hunks).unwrap()[&path],
                after
            );
        }
    }
    #[test]
    fn repeated_search_and_out_of_scope_edits_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("add.py"), "same\nsame\n").unwrap();
        let edit = r#"{"path":"add.py","search":"same","replace":"different"}"#;
        assert!(search_replace_hunks(dir.path(), &["add.py".into()], edit).is_err());
        assert!(search_replace_hunks(dir.path(), &["other.py".into()], edit).is_err());
    }

    #[test]
    fn canonical_edits_preserve_crlf_and_reject_wrong_old_side() {
        let dir = tempfile::tempdir().unwrap();
        let before = "keep\r\nwrong\r\nend\r\n";
        std::fs::write(dir.path().join("code.txt"), before).unwrap();
        let scope = [PathBuf::from("code.txt")];
        let mut hunks = search_replace_hunks(
            dir.path(),
            &scope,
            r#"{"path":"code.txt","search":"wrong","replace":"right"}"#,
        )
        .unwrap();
        let files = materialize_hunks(dir.path(), &scope, &hunks).unwrap();
        assert_eq!(files[&scope[0]], "keep\r\nright\r\nend\r\n");
        hunks[0].old_count += 1;
        assert!(materialize_hunks(dir.path(), &scope, &hunks).is_err());
        hunks[0].old_count -= 1;
        hunks[0].lines[0] = DiffLine::Context("invented old side".into());
        assert!(materialize_hunks(dir.path(), &scope, &hunks).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("code.txt")).unwrap(),
            before
        );
    }

    #[test]
    fn model_diff_without_carriage_returns_edits_crlf_source_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("code.txt");
        std::fs::write(dir.path().join(&path), "keep\r\nwrong\r\nend\r\n").unwrap();
        let hunks = vec![DiffHunk {
            file_path: path.clone(),
            old_start: 2,
            old_count: 1,
            new_start: 2,
            new_count: 1,
            lines: vec![
                DiffLine::Removed("wrong".into()),
                DiffLine::Added("right".into()),
            ],
        }];
        let result = materialize_hunks(dir.path(), std::slice::from_ref(&path), &hunks).unwrap();
        assert_eq!(result[&path], "keep\r\nright\r\nend\r\n");

        let mut invented = hunks;
        invented[0].lines[0] = DiffLine::Removed("other".into());
        assert!(materialize_hunks(dir.path(), &[path], &invented).is_err());
    }

    #[test]
    fn new_files_require_permission_and_insertions_preserve_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let scope = [PathBuf::from("nested/code.txt")];
        let mut hunks = vec![DiffHunk {
            file_path: scope[0].clone(),
            old_start: 0,
            old_count: 0,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine::Added("inserted".into())],
        }];
        assert!(materialize_hunks(dir.path(), &scope, &hunks).is_err());
        assert_eq!(
            materialize_hunks_with_new_files(dir.path(), &scope, &hunks).unwrap()[&scope[0]],
            "inserted\n"
        );
        assert!(!dir.path().join(&scope[0]).exists());
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join(&scope[0]), "keep\ntail\n").unwrap();
        assert_eq!(
            materialize_hunks_with_new_files(dir.path(), &scope, &hunks).unwrap()[&scope[0]],
            "inserted\nkeep\ntail\n"
        );
        hunks[0].old_start = 1;
        hunks[0].new_start = 2;
        assert_eq!(
            materialize_hunks(dir.path(), &scope, &hunks).unwrap()[&scope[0]],
            "keep\ninserted\ntail\n"
        );
        hunks[0].new_start = 1;
        assert!(materialize_hunks(dir.path(), &scope, &hunks).is_err());
    }

    #[test]
    fn all_files_are_validated_before_any_candidate_is_returned() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.txt"), "actual\n").unwrap();
        let hunks = vec![
            DiffHunk {
                file_path: "new.txt".into(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine::Added("new".into())],
            },
            DiffHunk {
                file_path: "old.txt".into(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![
                    DiffLine::Removed("summary of original".into()),
                    DiffLine::Added("new".into()),
                ],
            },
        ];
        assert!(materialize_hunks_with_new_files(
            dir.path(),
            &["new.txt".into(), "old.txt".into()],
            &hunks
        )
        .is_err());
        assert!(!dir.path().join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("old.txt")).unwrap(),
            "actual\n"
        );
    }

    #[test]
    fn canonical_hunks_reject_hidden_line_breaks_and_binary_output() {
        let dir = tempfile::tempdir().unwrap();
        for content in ["visible\nhidden", "text\0binary"] {
            let hunk = DiffHunk {
                file_path: "new.txt".into(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine::Added(content.into())],
            };
            assert!(
                materialize_hunks_with_new_files(dir.path(), &["new.txt".into()], &[hunk]).is_err()
            );
        }
        assert!(!dir.path().join("new.txt").exists());
    }
}
