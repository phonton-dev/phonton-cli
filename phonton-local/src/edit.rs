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

/// Parse and normalize the exact creation transport without touching a file.
/// Calibration uses this same contract before any repository goal relies on it.
pub(crate) fn parse_creation_text(target: &Path, response: &str) -> Result<String> {
    if response.len() > 1024 * 1024 {
        return Err(LocalError::Invalid("Creation output exceeds 1 MiB".into()));
    }
    let edit: CreateEdit = serde_json::from_str(response)?;
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
    let edit: Edit = serde_json::from_str(response)?;
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
    let updated = original.replacen(&edit.search, &edit.replace, 1);
    if updated.len() > 1024 * 1024 || original.contains('\0') || updated.contains('\0') {
        return Err(LocalError::Invalid(
            "Edit is binary or exceeds the file size budget".into(),
        ));
    }
    canonical_change(&relative, &original, &updated)
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
