//! Read-only local source discovery and visible execution contracts.
use super::{
    context, invalid, inventory, minimum_complete_check_budget, node_deps, validate_request, Result,
};
use phonton_types::{
    local_run::*, ExpectedArtifact, GoalContract, QualityFloor, RunCommand, TaskClass, TokenPolicy,
    VerifyStepSpec,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

struct RankedSource {
    evidence: ScopeEvidence,
    // One strongest spelling per goal identifier family in this file.
    names: BTreeMap<String, (String, context::NameMatch)>,
    path_hits: usize,
    content_hits: usize,
}

impl RankedSource {
    fn score(&self) -> usize {
        self.names
            .values()
            .take(8)
            .map(|(_, matched)| match matched {
                context::NameMatch::Exact => 100,
                context::NameMatch::Variant => 80,
                context::NameMatch::CaseInsensitive => 60,
            })
            .sum::<usize>()
            + self.path_hits * 20
            + self.content_hits
    }

    fn priority(&self) -> Option<context::NameMatch> {
        self.names.values().map(|(_, matched)| *matched).max()
    }

    fn explain(&mut self, inferred: bool) {
        self.evidence.reason = if !inferred {
            "Selected explicitly".into()
        } else if !self.names.is_empty() {
            format!(
                "Goal names parsed symbol(s): {}",
                self.names
                    .values()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else if self.path_hits > 0 {
            "Path matches goal terms".into()
        } else {
            "Source text matches goal terms; review relevance".into()
        };
    }
}

/// Inspect a bounded Git working tree without inference or project execution.
/// Returned checks are proposals. Host permission is always cleared in a preview.
pub async fn preview(mut request: LocalRunRequest) -> Result<LocalPlan> {
    if request.goal.trim().is_empty() || request.goal.len() > 8192 {
        return Err(invalid("Enter a coding goal of at most 8192 bytes"));
    }
    let root = std::fs::canonicalize(&request.repository)?;
    request.repository = root.clone();
    request.approve_host_execution = false;
    let paths = inventory(&root).await?;
    let inferred = request.files.is_empty() && request.editable_existing.is_empty();
    let mut warnings = vec![
        if request.new_file.is_some() && !request.editable_existing.is_empty() {
            "Creation and existing edit paths are explicit. Other selected source is read-only context. Review the selected check; it does not establish general implementation quality or test coverage.".into()
        } else if request.new_file.is_some() && inferred {
            "Creation target is explicit. Matching existing source is proposed as read-only context; review the selected files and check before running.".into()
        } else if request.new_file.is_some() {
            "Creation path and read-only source context are explicit. Review the selected check; it does not establish general implementation quality or test coverage.".into()
        } else {
            "Scope uses source and symbol matches. Review the files and checks; this does not establish an implementation strategy or test coverage.".into()
        },
    ];
    let query = context::terms(&request.goal.to_lowercase());
    let goal_symbols = context::SymbolQuery::new(&request.goal);
    let mut ranked = Vec::new();
    for path in &paths {
        if !inferred && !request.files.contains(path) {
            continue;
        }
        if inferred && !source_extension(path) {
            continue;
        }
        let mut probe = request.clone();
        probe.files = vec![path.clone()];
        // Validate each candidate source independently, then validate the full
        // reviewed edit subset after discovery and check inference.
        probe.editable_existing.clear();
        if let Err(error) = validate_request(&probe) {
            if inferred {
                continue;
            } else {
                return Err(error);
            }
        }
        if std::fs::metadata(root.join(path))?.len() > 1024 * 1024 {
            if inferred {
                continue;
            } else {
                return Err(invalid("Selected source exceeds 1 MiB"));
            }
        }
        let bytes = std::fs::read(root.join(path))?;
        let Ok(source) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if source.is_empty() || source.contains('\0') {
            continue;
        }
        let symbols = phonton_index::extract_symbol_spans(source, path);
        let mut names: BTreeMap<String, (String, context::NameMatch)> = BTreeMap::new();
        for symbol in &symbols {
            if let Some(matched) = goal_symbols.match_name(&symbol.name) {
                let family = context::symbol_family_key(&symbol.name);
                let entry = names
                    .entry(family)
                    .or_insert_with(|| (symbol.name.clone(), matched));
                if matched > entry.1 {
                    *entry = (symbol.name.clone(), matched);
                }
            }
        }
        let path_terms = context::terms(&path.to_string_lossy().to_lowercase());
        let path_hits = query.intersection(&path_terms).count();
        let content_hits = query
            .intersection(&context::terms(&source.to_lowercase()))
            .count();
        ranked.push(RankedSource {
            names,
            path_hits,
            content_hits,
            evidence: ScopeEvidence {
                path: path.clone(),
                reason: String::new(),
                source_sha256: format!("{:x}", Sha256::digest(&bytes)),
            },
        });
    }
    if inferred {
        let mut strongest = BTreeMap::new();
        for source in &ranked {
            for (family, (_, matched)) in &source.names {
                strongest
                    .entry(family.clone())
                    .and_modify(|best: &mut context::NameMatch| *best = (*best).max(*matched))
                    .or_insert(*matched);
            }
        }
        ranked.retain_mut(|source| {
            let had_name = !source.names.is_empty();
            source
                .names
                .retain(|family, (_, matched)| strongest.get(family) == Some(matched));
            !had_name || !source.names.is_empty()
        });
    }
    for source in &mut ranked {
        source.explain(inferred);
    }
    ranked.sort_by(|a, b| {
        b.priority()
            .cmp(&a.priority())
            .then(b.score().cmp(&a.score()))
            .then(a.evidence.path.cmp(&b.evidence.path))
    });
    if inferred {
        let best = ranked.first().map(RankedSource::score).unwrap_or(0);
        if best == 0 {
            if request.new_file.is_none() {
                return Err(invalid("No source matched the goal. Name a symbol or path, or provide the files explicitly"));
            }
            ranked.clear();
            warnings.push("No existing source matched the goal. The creation prompt will have no repository excerpt; add context files if the new file must follow an existing API or style.".into());
        } else {
            let threshold = if best >= 100 { 100 } else { (best / 2).max(1) };
            ranked.retain(|r| !r.names.is_empty() || r.score() >= threshold);
            if ranked.len() > 8 {
                warnings.push(format!("{} files matched; this bounded preview includes the top eight. Narrow the goal if more context is required.", ranked.len()));
            }
            ranked.truncate(8);
        }
        request.files = ranked.iter().map(|r| r.evidence.path.clone()).collect();
    }
    if ranked.len() != request.files.len() {
        return Err(invalid(
            "Some selected files are missing, linked, empty, or binary source",
        ));
    }
    for path in &request.files {
        if std::fs::read(root.join(path))?
            .last()
            .is_some_and(|byte| *byte != b'\n')
        {
            warnings.push(format!(
                "{} has no final newline; candidate edits preserve that EOF state.",
                path.display()
            ));
        }
    }
    let creation = if let Some(path) = &request.new_file {
        super::validate_creation_target(&root, path).await?;
        warnings.push("The new-file path was selected explicitly. An ordinary edit-format probe does not demonstrate file-creation quality; review the candidate and checks.".into());
        Some(CreationEvidence {
            path: path.clone(),
            reason: "Explicit target absent at read-only planning".into(),
        })
    } else {
        None
    };
    let inferred_checks = request.checks.is_empty();
    if request.checks.is_empty() {
        let editable = if let Some(path) = &request.new_file {
            std::iter::once(path.clone())
                .chain(request.editable_existing.iter().cloned())
                .collect()
        } else {
            request.files.clone()
        };
        let (checks, caution) = suggested_checks(&root, &paths, &editable)?;
        request.checks = checks;
        if let Some(caution) = caution {
            warnings.push(caution.into());
        }
        if !request.checks.is_empty() {
            warnings.push("Verification commands were inferred from repository files. They have not run and require separate host approval.".into());
        }
    }
    let root_npm_check = LocalCheck {
        program: node_deps::command().program,
        args: vec!["test".into()],
    };
    let root_node_runner = super::node_test_invocation(&root_npm_check, &root);
    let root_npm_selected = request.checks.iter().any(node_deps::is_root_npm_test);
    let script_node_selected = root_node_runner.as_ref().is_some_and(|runner| {
        request
            .checks
            .iter()
            .any(|check| check.program == runner.program && check.args == runner.args)
    });
    if root_npm_selected || script_node_selected {
        if root_npm_selected {
            warnings.push("The selected root npm test is diagnostic only: npm may replace its script shell or resolve a different node executable. A successful exit remains Not run; select a direct Node TAP check for verification.".into());
        }
        let preparation = if !paths.contains(&PathBuf::from("package.json")) {
            Err(invalid(
                "Root package.json must be in the captured Git inventory",
            ))
        } else {
            match node_deps::validated_command(&root) {
                Ok(Some(_)) if !paths.contains(&PathBuf::from("package-lock.json")) => {
                    Err(invalid("Root package-lock.json must be in the captured Git inventory for offline npm preparation"))
                }
                result => result,
            }
        };
        match preparation {
            Ok(Some(command)) => {
                if request.preparation.is_none() {
                    request.preparation = Some(command);
                }
                warnings.push("This Node check needs dependencies. The reviewed plan runs offline npm ci with lifecycle scripts disabled in each separate check copy before testing. Preparation uses a check-budget slot and may be unavailable if the npm cache lacks packages.".into());
            }
            Ok(None) => {}
            Err(error) => {
                if request.preparation.is_some() {
                    return Err(error);
                }
                warnings.push(format!(
                    "Root npm dependency preparation is unavailable: {error}"
                ));
                if inferred_checks {
                    request.checks.clear();
                    warnings.push("The inferred Node test was withheld because its dependencies cannot be prepared from the captured lockfile. Choose a supported check explicitly or repair the lockfile.".into());
                }
            }
        }
    }
    if request.checks.is_empty() {
        warnings.push("No conventional check was found. Add a command or receive an explicitly unverified candidate.".into());
    }
    if let Some(warning) = node_inclusion_warning(&request, &root) {
        warnings.push(warning.into());
    }
    if let Some(warning) = python_inclusion_warning(&request, &root) {
        warnings.push(warning.into());
    }
    let minimum_checks = minimum_complete_check_budget(&request);
    if request.budget.check_runs < minimum_checks {
        warnings.push(format!(
            "The selected check budget cannot complete baseline and one candidate: at least {minimum_checks} command reservations are required, including dependency preparation. Increase check_runs or narrow the selected checks before approving host execution."
        ));
    }
    validate_request(&request)?;
    request.expected_source_hashes = ranked
        .iter()
        .map(|source| {
            (
                source.evidence.path.clone(),
                source.evidence.source_sha256.clone(),
            )
        })
        .collect();
    request.expected_baseline_sha256 = Some(super::scoped_hash(
        &root,
        &paths,
        request.new_file.as_ref(),
        false,
    )?);
    Ok(LocalPlan {
        contract: contract(&request),
        request,
        files: ranked.into_iter().map(|r| r.evidence).collect(),
        creation,
        warnings,
    })
}

fn node_inclusion_warning(request: &LocalRunRequest, root: &Path) -> Option<&'static str> {
    if !super::js_writable_scope(request) {
        return None;
    }
    let direct_tap = request.checks.iter().any(|check| {
        super::is_node_command(check) && super::node_test_invocation(check, root).is_some()
    });
    if !direct_tap {
        return Some("JS/TS edits need a reviewed direct node --test --test-reporter=tap check that loads every edited source file. The selected checks cannot prove this inclusion; passing unrelated or opaque commands will leave the candidate unverified.");
    }
    Some("JS/TS edit verification also requires V8 coverage showing each exact edited source loaded by the selected direct Node test. A transpiler that reports only generated files will leave the candidate unverified; loading source alone does not prove its behavior was asserted.")
}

fn python_inclusion_warning(request: &LocalRunRequest, root: &Path) -> Option<&'static str> {
    if !super::python_writable_scope(request) {
        return None;
    }
    let direct_python = request.checks.iter().any(|check| {
        (super::is_python_program(&check.program) || super::is_pytest_command(check))
            && super::check_executes_candidate_verifier(check, root)
    });
    if !direct_python {
        return Some("Python edits need a direct Python unittest, pytest, or candidate-local script check that executes every edited source file. The selected checks cannot establish source inclusion; passing unrelated or opaque commands will leave the candidate unverified.");
    }
    Some("Python edit verification also needs a process-reported execution trace showing every exact edited .py source loaded by the selected direct check. Isolated Python flags, a conflicting sitecustomize, or missing trace output leave inclusion unverified; loading source alone does not prove its behavior was asserted.")
}

fn source_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some(
            "rs" | "js"
                | "mjs"
                | "cjs"
                | "ts"
                | "mts"
                | "cts"
                | "tsx"
                | "jsx"
                | "py"
                | "go"
                | "java"
                | "cs"
                | "cpp"
                | "c"
                | "h"
                | "hpp"
                | "svelte"
                | "vue"
                | "html"
                | "css"
                | "sql"
                | "rb"
                | "php"
                | "swift"
                | "kt"
                | "md"
        )
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckFamily {
    Rust,
    Go,
    Node,
    Python,
}

fn check_family(path: &Path) -> Option<CheckFamily> {
    match path.extension().and_then(|extension| extension.to_str())? {
        "rs" => Some(CheckFamily::Rust),
        "go" => Some(CheckFamily::Go),
        "js" | "cjs" | "mjs" | "ts" | "cts" | "mts" | "jsx" | "tsx" | "vue" | "svelte" | "html"
        | "css" => Some(CheckFamily::Node),
        "py" => Some(CheckFamily::Python),
        _ => None,
    }
}

fn go_skip_trivia(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes[index..].starts_with(b"//") {
            while bytes.get(index).is_some_and(|byte| *byte != b'\n') {
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/*") {
            let Some(end) = bytes[index + 2..].windows(2).position(|pair| pair == b"*/") else {
                return bytes.len();
            };
            index += end + 4;
        } else {
            return index;
        }
    }
}

fn go_skip_literal(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut index = start + 1;
    while index < bytes.len() {
        if quote != b'`' && bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == quote {
            return index + 1;
        } else {
            index += 1;
        }
    }
    bytes.len()
}

fn go_import_literal_might_be_cgo(bytes: &[u8], start: usize) -> (bool, usize) {
    let end = go_skip_literal(bytes, start);
    if end <= start + 1 || bytes[end - 1] != bytes[start] {
        return (false, end);
    }
    let path = &bytes[start + 1..end - 1];
    // Go accepts escaped import paths such as "\\x43" as the cgo pseudo-package.
    let raw_cgo = bytes[start] == b'`'
        && path
            .iter()
            .copied()
            .filter(|byte| *byte != b'\r')
            .eq(b"C".iter().copied());
    (
        path == b"C" || raw_cgo || (bytes[start] == b'"' && path.contains(&b'\\')),
        end,
    )
}

fn go_source_imports_cgo(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        index = go_skip_trivia(bytes, index);
        let Some(&byte) = bytes.get(index) else {
            break;
        };
        if matches!(byte, b'"' | b'`' | b'\'') {
            index = go_skip_literal(bytes, index);
            continue;
        }
        if !byte.is_ascii_alphabetic() && byte != b'_' {
            index += 1;
            continue;
        }
        let start = index;
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            index += 1;
        }
        if &bytes[start..index] != b"import" {
            continue;
        }
        let mut next = go_skip_trivia(bytes, index);
        if bytes.get(next) == Some(&b'(') {
            next += 1;
            while next < bytes.len() {
                next = go_skip_trivia(bytes, next);
                let Some(&byte) = bytes.get(next) else {
                    break;
                };
                if byte == b')' {
                    next += 1;
                    break;
                }
                if matches!(byte, b'"' | b'`') {
                    let (might_be_cgo, end) = go_import_literal_might_be_cgo(bytes, next);
                    if might_be_cgo {
                        return true;
                    }
                    next = end;
                } else {
                    next += 1;
                }
            }
            index = next;
            continue;
        }
        if bytes
            .get(next)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_' || *byte == b'.')
        {
            while bytes
                .get(next)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                next += 1;
            }
            if bytes.get(next) == Some(&b'.') {
                next += 1;
            }
            next = go_skip_trivia(bytes, next);
        }
        if bytes
            .get(next)
            .is_some_and(|byte| matches!(byte, b'"' | b'`'))
        {
            let (might_be_cgo, end) = go_import_literal_might_be_cgo(bytes, next);
            if might_be_cgo {
                return true;
            }
            index = end;
        }
    }
    false
}

pub(super) fn go_source_has_build_exclusion(path: &Path, source: &str) -> bool {
    let ignored_component = path.iter().any(|component| {
        let component = component.to_string_lossy();
        component.starts_with('.')
            || component.starts_with('_')
            || component.eq_ignore_ascii_case("testdata")
    });
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let suffix = stem.rsplit('_').next().unwrap_or("");
    let platform_suffix = matches!(
        suffix,
        "aix"
            | "android"
            | "darwin"
            | "dragonfly"
            | "freebsd"
            | "illumos"
            | "ios"
            | "js"
            | "linux"
            | "netbsd"
            | "openbsd"
            | "plan9"
            | "solaris"
            | "wasip1"
            | "windows"
            | "386"
            | "amd64"
            | "arm"
            | "arm64"
            | "loong64"
            | "mips"
            | "mips64"
            | "mips64le"
            | "mipsle"
            | "ppc64"
            | "ppc64le"
            | "riscv64"
            | "s390x"
            | "wasm"
    );
    ignored_component
        || platform_suffix
        || source.lines().any(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("//go:build") || trimmed.starts_with("// +build")
        })
        || go_source_imports_cgo(source)
}

pub(super) fn has_package_scoped_go_check(checks: &[LocalCheck]) -> bool {
    checks.iter().any(|check| {
        Path::new(&check.program)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("go") || name.eq_ignore_ascii_case("go.exe")
            })
            && matches!(check.args.as_slice(), [test, json, count, package]
                    if test == "test" && json == "-json" && count == "-count=1"
                        && (package == "." || (package.starts_with("./") && package != "./...")))
    })
}

fn python_test_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.ends_with(".py") && (name.starts_with("test") || name.ends_with("_test.py"))
        })
}

fn python_runner_shadowed(paths: &[PathBuf], runner: &str) -> bool {
    let module = format!("{runner}.py");
    let bytecode = format!("{runner}.pyc");
    let package = format!("{runner}/");
    paths.iter().any(|path| {
        let path = path
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        path == module || path == bytecode || path.starts_with(&package)
    })
}

// unittest's root discover only enters package directories. A direct `-s`
// start can collect tests beneath the deepest directory it would skip.
fn python_unittest_start(root: &Path, paths: &[PathBuf], test: &Path) -> Result<PathBuf> {
    let mut directory = PathBuf::new();
    let mut start = PathBuf::from(".");
    if let Some(parent) = test.parent() {
        for component in parent.components() {
            directory.push(component.as_os_str());
            let marker = directory.join("__init__.py");
            let importable = paths.contains(&marker)
                && std::fs::symlink_metadata(root.join(marker))?
                    .file_type()
                    .is_file();
            if !importable {
                start = directory.clone();
            }
        }
    }
    Ok(start)
}

fn python_has_pytest_configuration(root: &Path, paths: &[PathBuf]) -> Result<bool> {
    if paths.iter().any(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "pytest.ini" | ".pytest.ini" | "pytest.toml" | ".pytest.toml" | "conftest.py"
                )
            })
    }) {
        return Ok(true);
    }
    for path in paths.iter().filter(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "pyproject.toml" | "setup.cfg" | "tox.ini"
                )
            })
    }) {
        let file = root.join(path);
        let metadata = std::fs::symlink_metadata(&file)?;
        if !metadata.file_type().is_file() || metadata.len() > 1024 * 1024 {
            return Ok(true);
        }
        let source = std::fs::read_to_string(file)?;
        if source.contains("[tool.pytest]")
            || source.contains("[tool.pytest.ini_options]")
            || source.contains("[tool:pytest]")
            || source.contains("[pytest]")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn python_uses_pytest(root: &Path, paths: &[PathBuf], tests: &[&PathBuf]) -> Result<bool> {
    if python_has_pytest_configuration(root, paths)? {
        return Ok(true);
    }
    for path in tests {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("_test.py"))
        {
            return Ok(true);
        }
        let file = root.join(path);
        let metadata = std::fs::symlink_metadata(&file)?;
        if !metadata.file_type().is_file() || metadata.len() > 1024 * 1024 {
            return Ok(true);
        }
        let source = std::fs::read_to_string(file)?;
        let pytest_function_or_import = source.lines().any(|line| {
            line.starts_with("def test_")
                || line.starts_with("async def test_")
                || line.trim_start().starts_with("import pytest")
                || line.trim_start().starts_with("from pytest import")
        });
        let pytest_class = source.lines().any(|line| {
            line.starts_with("class Test")
                && !line
                    .split_once('(')
                    .and_then(|(_, rest)| rest.split_once(')'))
                    .is_some_and(|(bases, _)| {
                        bases.split(',').any(|base| {
                            base.trim() == "TestCase" || base.trim().ends_with(".TestCase")
                        })
                    })
        }) && source.lines().any(|line| {
            line.trim_start().starts_with("def test_")
                || line.trim_start().starts_with("async def test_")
        });
        if pytest_function_or_import || pytest_class {
            return Ok(true);
        }
    }
    Ok(false)
}

fn nearest_manifest(paths: &[PathBuf], target: &Path, name: &str) -> Option<PathBuf> {
    let mut parent = target.parent();
    while let Some(directory) = parent {
        let candidate = directory.join(name);
        if paths.contains(&candidate) {
            return Some(candidate);
        }
        parent = directory.parent();
    }
    None
}

fn owning_cargo_package(root: &Path, manifest: &Path) -> Option<String> {
    let path = root.join(manifest);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > 1024 * 1024 {
        return None;
    }
    let source = std::fs::read_to_string(path).ok()?;
    let parsed: toml::Value = toml::from_str(&source).ok()?;
    let name = parsed.get("package")?.get("name")?.as_str()?.trim();
    (!name.is_empty() && name.len() <= 256).then(|| name.to_owned())
}

// An inferred command is useful only when it addresses the selected edit
// domain. A passing root suite must not stand in for a nested module/package.
fn suggested_checks(
    root: &Path,
    paths: &[PathBuf],
    editable: &[PathBuf],
) -> Result<(Vec<LocalCheck>, Option<&'static str>)> {
    let Some(family) = editable.first().and_then(|path| check_family(path)) else {
        return Ok((vec![], Some("No check family is known for the selected edit target. Choose a command explicitly.")));
    };
    if editable
        .iter()
        .any(|path| check_family(path) != Some(family))
    {
        return Ok((vec![], Some("Selected edits span multiple check families or include an unknown source type. Choose checks that cover the complete scope explicitly.")));
    }
    match family {
        CheckFamily::Rust => {
            let manifests: BTreeSet<_> = editable
                .iter()
                .filter_map(|path| nearest_manifest(paths, path, "Cargo.toml"))
                .collect();
            if editable
                .iter()
                .any(|path| nearest_manifest(paths, path, "Cargo.toml").is_none())
            {
                return Ok((vec![], Some("Some Rust targets have no owning Cargo manifest. Choose checks explicitly.")));
            }
            if manifests.len() > 4 {
                return Ok((vec![], Some("Selected Rust targets span more than four Cargo manifests. Choose checks explicitly.")));
            }
            let mut checks = Vec::with_capacity(manifests.len());
            for manifest in manifests {
                let Some(package) = owning_cargo_package(root, &manifest) else {
                    return Ok((vec![], Some("A selected Rust target has no readable owning Cargo package. Choose checks explicitly.")));
                };
                // The package flag includes a root package even when a workspace
                // excludes it from default-members, without running unrelated
                // packages under the shared wall-time and check budget.
                let mut args = vec![
                    "test".into(),
                    "--locked".into(),
                    "--package".into(),
                    package,
                ];
                if manifest != Path::new("Cargo.toml") {
                    args.push("--manifest-path".into());
                    args.push(manifest.to_string_lossy().replace('\\', "/"));
                }
                checks.push(LocalCheck {
                    program: "cargo".into(),
                    args,
                });
            }
            Ok((checks, Some("Inferred Cargo checks cover the owning packages only. Add broader workspace checks explicitly when dependents matter.")))
        }
        CheckFamily::Go => {
            if editable.iter().any(|path| {
                nearest_manifest(paths, path, "go.mod")
                    .is_some_and(|manifest| manifest != Path::new("go.mod"))
            }) {
                return Ok((vec![], Some("Selected Go source belongs to a nested Go module; root go test ./... would skip it. Choose a module-specific check explicitly.")));
            }
            if paths.contains(&PathBuf::from("go.mod")) {
                for path in editable {
                    let source = if paths.contains(path) {
                        std::fs::read_to_string(root.join(path))?
                    } else {
                        String::new()
                    };
                    if go_source_has_build_exclusion(path, &source) {
                        return Ok((vec![], Some("Selected Go source is build-constrained, ignored by Go naming rules, or uses cgo, so a default package test may not compile the edited file. Choose checks explicitly for that target.")));
                    }
                }
                let packages: BTreeSet<_> = editable
                    .iter()
                    .map(|path| {
                        path.parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .unwrap_or_else(|| Path::new("."))
                    })
                    .collect();
                if packages.len() > 4 {
                    return Ok((vec![], Some("Selected Go targets span more than four packages. Choose checks for the complete scope explicitly.")));
                }
                let checks = packages
                    .into_iter()
                    .map(|package| LocalCheck {
                        program: "go".into(),
                        args: vec![
                            "test".into(),
                            "-json".into(),
                            "-count=1".into(),
                            if package == Path::new(".") {
                                ".".into()
                            } else {
                                format!("./{}", package.to_string_lossy().replace('\\', "/"))
                            },
                        ],
                    })
                    .collect();
                return Ok((checks, Some("Inferred Go checks run each edited package separately. Add broader module or dependent-package checks explicitly when they matter.")));
            }
            Ok((vec![], None))
        }
        CheckFamily::Node => {
            if editable.iter().any(|path| {
                nearest_manifest(paths, path, "package.json")
                    .is_some_and(|manifest| manifest != Path::new("package.json"))
            }) {
                return Ok((vec![], Some("Selected source belongs to a nested Node package; a root npm test may skip it. Choose a package-specific check explicitly.")));
            }
            if paths.contains(&PathBuf::from("package.json"))
                && std::fs::metadata(root.join("package.json"))?.len() <= 1024 * 1024
            {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&std::fs::read(
                    root.join("package.json"),
                )?) {
                    if value["scripts"]["test"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty())
                    {
                        let check = LocalCheck {
                            program: if cfg!(windows) { "npm.cmd" } else { "npm" }.into(),
                            args: vec!["test".into()],
                        };
                        let Some(runner) = super::node_test_invocation(&check, root) else {
                            return Ok((vec![], Some("The inferred root npm test was withheld: its script has no supported test runner with an explicit TAP reporter, so a successful exit would not prove a completed test. Choose a check that reports named completed cases.")));
                        };
                        return Ok((vec![runner], Some("The root npm script was used to propose a direct Node TAP check. The plan runs Node directly so npm script-shell and node_modules/.bin cannot change the verifier executable; review whether the project needs npm-specific setup.")));
                    }
                }
            }
            let owner = nearest_manifest(paths, &editable[0], "package.json");
            let node_tests: Vec<_> = paths
                .iter()
                .filter(|p| {
                    p.extension()
                        .is_some_and(|e| e == "js" || e == "mjs" || e == "cjs")
                        && nearest_manifest(paths, p, "package.json").as_ref() == owner.as_ref()
                        && (super::conventional_node_test_filename(p)
                            || p.file_name()
                                .and_then(|s| s.to_str())
                                .is_some_and(|s| s.contains(".spec."))
                            || p.components()
                                .any(|component| component.as_os_str() == "test"))
                })
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .collect();
            if node_tests.len() > 32 {
                return Ok((vec![], Some("The selected Node package has more than 32 conventional test files; an inferred subset could omit a failing test. Choose a check explicitly.")));
            }
            if !node_tests.is_empty() {
                let mut args = vec!["--test".into(), "--test-reporter=tap".into()];
                args.extend(node_tests);
                return Ok((
                    vec![LocalCheck {
                        program: "node".into(),
                        args,
                    }],
                    None,
                ));
            }
            Ok((vec![], None))
        }
        CheckFamily::Python => {
            let tests: Vec<_> = paths.iter().filter(|path| python_test_file(path)).collect();
            if !tests.is_empty() {
                if python_uses_pytest(root, paths, &tests)? {
                    if python_runner_shadowed(paths, "pytest") {
                        return Ok((vec![], Some("A repository pytest module could shadow the host test runner. Choose a trusted external check explicitly.")));
                    }
                    let configured = python_has_pytest_configuration(root, paths)?;
                    let default_pattern = |path: &PathBuf| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| {
                                name.starts_with("test_") || name.ends_with("_test.py")
                            })
                    };
                    let explicit: Vec<_> = tests
                        .iter()
                        .filter(|path| configured || !default_pattern(path))
                        .map(|path| path.to_string_lossy().replace('\\', "/"))
                        .collect();
                    if explicit.len() > 32
                        || explicit.iter().map(String::len).sum::<usize>() > 8_000
                    {
                        return Ok((vec![], Some("The inferred pytest check was withheld: explicit test paths exceed the bounded command size. Choose checks for the complete scope explicitly.")));
                    }
                    let mut checks = Vec::new();
                    if configured || tests.iter().any(|path| default_pattern(path)) {
                        checks.push(LocalCheck {
                            program: "python".into(),
                            args: vec!["-m".into(), "pytest".into(), "--color=no".into()],
                        });
                    }
                    if !explicit.is_empty() {
                        // The root check retains project addopts. This second
                        // check names exact files and clears addopts that could
                        // prepend a broad path or deselect the named files.
                        let mut args = vec![
                            "-m".into(),
                            "pytest".into(),
                            "--color=no".into(),
                            "-o".into(),
                            "addopts=".into(),
                        ];
                        args.extend(explicit);
                        checks.push(LocalCheck {
                            program: "python".into(),
                            args,
                        });
                    }
                    return Ok((
                        checks,
                        Some(
                            if configured || tests.iter().any(|path| !default_pattern(path)) {
                                "Inferred pytest checks include explicit test paths that root discovery may skip. The explicit check clears project addopts; pytest must exist in the approved host environment. Review collected tests and broader coverage."
                            } else {
                                "Pytest-style tests were found. The inferred pytest check requires pytest in the approved host environment; review which tests it collects."
                            },
                        ),
                    ));
                }
                if python_runner_shadowed(paths, "unittest") {
                    return Ok((vec![], Some("A repository unittest module could shadow the host test runner. Choose a trusted external check explicitly.")));
                }
                let mut starts = BTreeSet::new();
                for test in tests {
                    starts.insert(python_unittest_start(root, paths, test)?);
                }
                if starts.len() > 4 {
                    return Ok((vec![], Some("Unittest tests span more than four discovery roots. Choose checks for the complete scope explicitly.")));
                }
                let nested = starts.iter().any(|start| start != Path::new("."));
                let checks = starts
                    .into_iter()
                    .map(|start| LocalCheck {
                        program: "python".into(),
                        args: if start == Path::new(".") {
                            vec!["-m".into(), "unittest".into(), "discover".into()]
                        } else {
                            vec![
                                "-m".into(),
                                "unittest".into(),
                                "discover".into(),
                                "-s".into(),
                                start.to_string_lossy().replace('\\', "/"),
                                "-p".into(),
                                "test*.py".into(),
                            ]
                        },
                    })
                    .collect();
                return Ok((
                    checks,
                    nested.then_some("Inferred unittest checks start in nested test directories that root discovery would skip. Review dependent tests and broader coverage explicitly."),
                ));
            }
            Ok((vec![], None))
        }
    }
}

/// Build the existing plan contract from the caller's exact accepted scope.
/// Zero confidence denotes unmeasured semantic interpretation, not a model score.
pub fn contract(request: &LocalRunRequest) -> GoalContract {
    let mut run_plan: Vec<_> = request
        .checks
        .iter()
        .map(|c| RunCommand {
            label: if node_deps::is_root_npm_test(c) {
                "Selected diagnostic-only npm test in a copied check directory; success is Not run"
                    .into()
            } else {
                "Selected verification in a copied check directory".into()
            },
            command: std::iter::once(c.program.clone())
                .chain(c.args.clone())
                .collect(),
            cwd: None,
        })
        .collect();
    if let Some(preparation) = &request.preparation {
        run_plan.insert(
            0,
            RunCommand {
                label:
                    "Offline dependency preparation in a copied check directory, not verification"
                        .into(),
                command: std::iter::once(preparation.program.clone())
                    .chain(preparation.args.clone())
                    .collect(),
                cwd: None,
            },
        );
    }
    let existing_edits = if request.new_file.is_some() {
        &request.editable_existing
    } else {
        &request.files
    };
    let mut expected_artifacts: Vec<_> = existing_edits
        .iter()
        .map(|p| ExpectedArtifact {
            description: "Reviewable candidate change if needed".into(),
            path: Some(p.clone()),
        })
        .collect();
    let mut likely_files = existing_edits.to_vec();
    if let Some(path) = &request.new_file {
        expected_artifacts.push(ExpectedArtifact {
            description: "Explicit new-file candidate, absent at planning".into(),
            path: Some(path.clone()),
        });
        likely_files.push(path.clone());
    }
    GoalContract { goal: request.goal.clone(), task_class: TaskClass::CoreLogic, intent: None, confidence_percent: 0,
        acceptance_criteria: vec![request.goal.clone(), "Return the exact candidate diff with actual selected-check outcomes and known gaps".into()], acceptance_slices: vec![],
        expected_artifacts, likely_files,
        verify_plan: run_plan.iter().map(|c| VerifyStepSpec { name: if c.label.starts_with("Offline dependency") { "Baseline and candidate dependency preparation" } else if c.label.starts_with("Selected diagnostic-only npm test") { "Baseline and candidate diagnostic-only npm test" } else { "Baseline and candidate check" }.into(), layer: None, command: Some(c.clone()) }).collect(), run_plan,
        quality_floor: QualityFloor { criteria: vec!["No model edits to verification definitions".into(), "Verify the exact returned candidate; missing execution is not a pass".into(), "Preserve source and Git index until explicit application".into()] },
        clarification_questions: vec![], assumptions: vec!["Scope and checks are caller-approved; semantic task decomposition is not measured".into(), "Host checks need separate explicit permission; otherwise required isolation fails closed".into()], token_policy: TokenPolicy::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn javascript_plan_warns_when_selected_checks_cannot_prove_source_inclusion() {
        let root = tempfile::tempdir().unwrap();
        let mut request = request_for(root.path(), &["src/app.mjs"]);
        assert!(node_inclusion_warning(&request, root.path())
            .unwrap()
            .contains("selected checks cannot prove"));
        request.checks.push(LocalCheck {
            program: "node".into(),
            args: vec![
                "--test".into(),
                "--test-reporter=tap".into(),
                "app.test.mjs".into(),
            ],
        });
        assert!(node_inclusion_warning(&request, root.path())
            .unwrap()
            .contains("each exact edited source"));
        request.files = vec!["src/app.py".into()];
        assert!(node_inclusion_warning(&request, root.path()).is_none());
        request.files = vec!["src/context.js".into()];
        request.new_file = Some("src/new.py".into());
        assert!(node_inclusion_warning(&request, root.path()).is_none());
        request.editable_existing = vec!["src/context.js".into()];
        assert!(node_inclusion_warning(&request, root.path()).is_some());
    }
    #[test]
    fn python_plan_warns_about_exact_source_inclusion() {
        let root = tempfile::tempdir().unwrap();
        let mut request = request_for(root.path(), &["src/logic.py"]);
        assert!(python_inclusion_warning(&request, root.path())
            .unwrap()
            .contains("selected checks cannot establish"));
        request.checks.push(LocalCheck {
            program: "python".into(),
            args: vec!["-m".into(), "unittest".into(), "discover".into()],
        });
        assert!(python_inclusion_warning(&request, root.path())
            .unwrap()
            .contains("every exact edited .py"));
        request.files = vec!["src/logic.mjs".into()];
        assert!(python_inclusion_warning(&request, root.path()).is_none());
    }
    #[tokio::test]
    async fn inferred_scope_finds_supported_javascript_module_extensions() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        let modules = [
            ("mjs", "parseMjs"),
            ("cjs", "parseCjs"),
            ("mts", "parseMts"),
            ("cts", "parseCts"),
        ];
        for (extension, symbol) in modules {
            std::fs::write(
                root.path().join(format!("module.{extension}")),
                format!("function {symbol}(value) {{ return value; }}\n"),
            )
            .unwrap();
        }
        for (extension, symbol) in modules {
            let plan = preview(LocalRunRequest {
                goal: format!("Fix {symbol}"),
                repository: root.path().into(),
                files: vec![],
                new_file: None,
                editable_existing: vec![],
                checks: vec![],
                preparation: None,
                approve_host_execution: false,
                allow_unverified_runtime: false,
                budget: Default::default(),
                expected_source_hashes: Default::default(),
                expected_baseline_sha256: None,
            })
            .await
            .unwrap();
            assert_eq!(
                plan.request.files,
                vec![PathBuf::from(format!("module.{extension}"))]
            );
            assert!(plan.files[0].reason.contains(symbol));
        }
    }
    #[tokio::test]
    async fn root_npm_plan_reviews_offline_setup_and_withholds_unpreparable_test() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/app.js"),
            "export function add(a, b) { return a - b; }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/test.js"),
            "const { test } = require('node:test'); test('fixture', () => {});\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"test":"node --test --test-reporter=tap src/test.js"},"devDependencies":{"fixture":"1.0.0"}}"#,
        )
        .unwrap();
        let lock = r#"{"lockfileVersion":3,"packages":{"":{},"node_modules/fixture":{"version":"1.0.0","resolved":"https://registry.npmjs.org/fixture/-/fixture-1.0.0.tgz","integrity":"sha512-abc"}}}"#;
        std::fs::write(root.path().join("package-lock.json"), lock).unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        let plan = preview(request_for(root.path(), &["src/app.js"]))
            .await
            .unwrap();
        assert_eq!(
            plan.request.checks[0].args,
            ["--test", "--test-reporter=tap", "src/test.js"]
        );
        assert_eq!(
            plan.request.preparation.as_ref().unwrap().args,
            node_deps::command().args
        );
        assert!(contract(&plan.request).run_plan[0]
            .label
            .contains("preparation"));
        let mut explicit_diagnostic = plan.request.clone();
        explicit_diagnostic.checks = vec![LocalCheck {
            program: node_deps::command().program,
            args: vec!["test".into()],
        }];
        assert!(contract(&explicit_diagnostic)
            .run_plan
            .iter()
            .any(|step| step.label.contains("diagnostic-only npm test")));
        assert!(!plan.request.approve_host_execution);
        assert!(!root.path().join("node_modules").exists());

        std::fs::remove_file(root.path().join("package-lock.json")).unwrap();
        let no_lock = preview(request_for(root.path(), &["src/app.js"]))
            .await
            .unwrap();
        assert!(no_lock.request.checks.is_empty());
        assert!(no_lock
            .warnings
            .iter()
            .any(|warning| warning.contains("withheld")));

        std::fs::write(root.path().join("package-lock.json"), lock).unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["rm", "--cached", "--", "package-lock.json"])
            .status()
            .unwrap()
            .success());
        std::fs::write(root.path().join(".gitignore"), "package-lock.json\n").unwrap();
        let mut explicit = request_for(root.path(), &["src/app.js"]);
        explicit.checks = vec![LocalCheck {
            program: node_deps::command().program,
            args: vec!["test".into()],
        }];
        explicit.preparation = Some(node_deps::command());
        assert!(preview(explicit).await.is_err());
    }

    #[tokio::test]
    async fn inferred_echo_only_npm_test_is_withheld() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/app.js"),
            "export function add(a, b) { return a - b; }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/test.js"),
            "const { test } = require('node:test'); test('app', () => {});\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"test":"echo ready"}}"#,
        )
        .unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        let plan = preview(request_for(root.path(), &["src/app.js"]))
            .await
            .unwrap();
        assert!(plan.request.checks.is_empty());
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("no supported test runner")));
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"test":"node --test src/test.js"}}"#,
        )
        .unwrap();
        let spec_plan = preview(request_for(root.path(), &["src/app.js"]))
            .await
            .unwrap();
        assert!(spec_plan.request.checks.is_empty());
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"test":"node --test --test-reporter=tap src/test.js"}}"#,
        )
        .unwrap();
        let tap_plan = preview(request_for(root.path(), &["src/app.js"]))
            .await
            .unwrap();
        assert_eq!(tap_plan.request.checks.len(), 1);
        assert_eq!(tap_plan.request.checks[0].program, "node");
    }

    #[tokio::test]
    async fn go_module_preview_proposes_a_check_without_running_it() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(
            root.path().join("go.mod"),
            "module example.com/demo\n\ngo 1.22\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("math.go"),
            "package demo\nfunc Add(a, b int) int { return a - b }\n",
        )
        .unwrap();
        let test = root.path().join("math_test.go");
        std::fs::write(
            &test,
            "package demo\nimport \"os\"\nfunc init() { _ = os.WriteFile(\"preview-executed\", []byte(\"bad\"), 0600) }\n",
        )
        .unwrap();
        let request = LocalRunRequest {
            goal: "Fix Add".into(),
            repository: root.path().into(),
            files: vec![],
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: true,
            allow_unverified_runtime: false,
            budget: Default::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        };
        let plan = preview(request.clone()).await.unwrap();
        assert_eq!(plan.request.files, vec![PathBuf::from("math.go")]);
        assert_eq!(plan.request.checks.len(), 1);
        assert_eq!(plan.request.checks[0].program, "go");
        assert_eq!(
            plan.request.checks[0].args,
            ["test", "-json", "-count=1", "."]
        );
        assert!(!plan.request.approve_host_execution);
        assert!(!root.path().join("preview-executed").exists());

        std::fs::write(&test, "package demo\n// changed check definition\n").unwrap();
        let changed = preview(request.clone()).await.unwrap();
        assert_ne!(
            plan.request.expected_baseline_sha256,
            changed.request.expected_baseline_sha256
        );

        std::fs::remove_file(root.path().join("go.mod")).unwrap();
        let without_manifest = preview(request).await.unwrap();
        assert!(without_manifest.request.checks.is_empty());
    }

    #[tokio::test]
    async fn inferred_go_checks_target_edited_packages_not_unrelated_test_packages() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(root.path().join("go.mod"), "module example.com/demo\n").unwrap();
        for package in ["changed", "unrelated"] {
            std::fs::create_dir(root.path().join(package)).unwrap();
        }
        std::fs::write(
            root.path().join("changed/logic.go"),
            "package changed\nfunc Value() int { return 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("unrelated/other_test.go"),
            "package unrelated\nimport \"testing\"\nfunc TestOther(t *testing.T) {}\n",
        )
        .unwrap();

        let plan = preview(request_for(root.path(), &["changed/logic.go"]))
            .await
            .unwrap();
        assert_eq!(plan.request.checks.len(), 1);
        assert_eq!(plan.request.checks[0].program, "go");
        assert_eq!(
            plan.request.checks[0].args,
            ["test", "-json", "-count=1", "./changed"]
        );

        std::fs::write(
            root.path().join("unrelated/other.go"),
            "package unrelated\nfunc Other() int { return 2 }\n",
        )
        .unwrap();
        let multi = preview(request_for(
            root.path(),
            &["changed/logic.go", "unrelated/other.go"],
        ))
        .await
        .unwrap();
        assert_eq!(multi.request.checks.len(), 2);
        assert_eq!(multi.request.checks[0].args[3], "./changed");
        assert_eq!(multi.request.checks[1].args[3], "./unrelated");
    }

    #[tokio::test]
    async fn inferred_go_checks_withhold_build_constrained_sources() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(root.path().join("go.mod"), "module example.com/demo\n").unwrap();
        std::fs::write(
            root.path().join("feature_linux.go"),
            "package demo\nfunc Feature() int { return 1 }\n",
        )
        .unwrap();
        let platform = preview(request_for(root.path(), &["feature_linux.go"]))
            .await
            .unwrap();
        assert!(platform.request.checks.is_empty());
        assert!(platform
            .warnings
            .iter()
            .any(|warning| warning.contains("build-constrained")));

        std::fs::write(
            root.path().join("feature.go"),
            "//go:build customtag\n\npackage demo\nfunc Feature() int { return 1 }\n",
        )
        .unwrap();
        let tagged = preview(request_for(root.path(), &["feature.go"]))
            .await
            .unwrap();
        assert!(tagged.request.checks.is_empty());
        assert!(tagged
            .warnings
            .iter()
            .any(|warning| warning.contains("build-constrained")));

        std::fs::write(
            root.path().join("_feature.go"),
            "package demo\nfunc Feature() int { return 1 }\n",
        )
        .unwrap();
        let ignored = preview(request_for(root.path(), &["_feature.go"]))
            .await
            .unwrap();
        assert!(ignored.request.checks.is_empty());
        assert!(ignored
            .warnings
            .iter()
            .any(|warning| warning.contains("build-constrained")));
    }

    #[tokio::test]
    async fn nested_go_module_does_not_get_unrelated_root_check() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(root.path().join("go.mod"), "module example.com/root\n").unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub/go.mod"), "module example.com/sub\n").unwrap();
        std::fs::write(
            root.path().join("sub/fix.go"),
            "package sub\nfunc Fix() {}\n",
        )
        .unwrap();
        let plan = preview(request_for(root.path(), &["sub/fix.go"]))
            .await
            .unwrap();
        assert!(plan.request.checks.is_empty());
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("nested Go module")));
    }

    #[tokio::test]
    async fn mixed_repository_checks_follow_editable_scope() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("package.json"),
            "{\"scripts\":{\"test\":\"node --test --test-reporter=tap test.js\"}}\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("test.js"),
            "const { test } = require('node:test'); test('app', () => {});\n",
        )
        .unwrap();
        std::fs::write(root.path().join("go.mod"), "module example.com/root\n").unwrap();
        std::fs::write(
            root.path().join("app.mjs"),
            "export function fix() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(root.path().join("fix.go"), "package main\nfunc Fix() {}\n").unwrap();
        let node = preview(request_for(root.path(), &["app.mjs"]))
            .await
            .unwrap();
        assert_eq!(node.request.checks.len(), 1);
        assert_eq!(node.request.checks[0].program, "node");
        let mixed = preview(request_for(root.path(), &["app.mjs", "fix.go"]))
            .await
            .unwrap();
        assert!(mixed.request.checks.is_empty());
        assert!(mixed
            .warnings
            .iter()
            .any(|warning| warning.contains("multiple check families")));
    }

    #[tokio::test]
    async fn nested_node_package_does_not_get_unrelated_root_script() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(
            root.path().join("package.json"),
            "{\"scripts\":{\"test\":\"node test.js\"}}\n",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(
            root.path().join("sub/package.json"),
            "{\"scripts\":{\"test\":\"node test.js\"}}\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("sub/app.mjs"),
            "export function fix() { return 1; }\n",
        )
        .unwrap();
        let plan = preview(request_for(root.path(), &["sub/app.mjs"]))
            .await
            .unwrap();
        assert!(plan.request.checks.is_empty());
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("nested Node package")));
    }

    #[tokio::test]
    async fn rust_member_receives_own_manifest_check() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname = \"root\"\nversion = \"0.1.0\"\n[workspace]\nmembers = [\"member\"]\ndefault-members = [\".\"]\n").unwrap();
        std::fs::create_dir_all(root.path().join("member/src")).unwrap();
        std::fs::write(
            root.path().join("member/Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("member/src/lib.rs"),
            "pub fn fix() -> i32 { 0 }\n",
        )
        .unwrap();
        let plan = preview(request_for(root.path(), &["member/src/lib.rs"]))
            .await
            .unwrap();
        assert_eq!(plan.request.checks.len(), 1);
        assert_eq!(plan.request.checks[0].program, "cargo");
        assert_eq!(
            plan.request.checks[0].args,
            [
                "test",
                "--locked",
                "--package",
                "member",
                "--manifest-path",
                "member/Cargo.toml"
            ]
        );
    }

    #[tokio::test]
    async fn creation_check_follows_new_file_not_read_only_context() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(root.path().join("go.mod"), "module example.com/demo\n").unwrap();
        std::fs::write(
            root.path().join("package.json"),
            "{\"scripts\":{\"test\":\"node test.js\"}}\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("context.js"),
            "export const context = true;\n",
        )
        .unwrap();
        let mut request = request_for(root.path(), &["context.js"]);
        request.new_file = Some("new.go".into());
        let mut mixed_request = request.clone();
        mixed_request.editable_existing = vec!["context.js".into()];
        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.checks.len(), 1);
        assert_eq!(plan.request.checks[0].program, "go");
        assert_eq!(plan.contract.expected_artifacts.len(), 1);
        assert_eq!(
            plan.contract.expected_artifacts[0].path,
            Some(PathBuf::from("new.go"))
        );
        assert_eq!(plan.contract.likely_files, vec![PathBuf::from("new.go")]);
        let mixed = preview(mixed_request).await.unwrap();
        assert!(mixed.request.checks.is_empty());
        assert_eq!(
            mixed.contract.likely_files,
            vec![PathBuf::from("context.js"), PathBuf::from("new.go")]
        );
        assert_eq!(mixed.contract.expected_artifacts.len(), 2);
    }

    #[tokio::test]
    async fn nested_rust_workspace_root_check_targets_the_edited_package() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir_all(root.path().join("rust/src")).unwrap();
        std::fs::create_dir_all(root.path().join("rust/member/src")).unwrap();
        std::fs::write(root.path().join("rust/Cargo.toml"), "[package]\nname = \"rust-root\"\nversion = \"0.1.0\"\n[workspace]\nmembers = [\"member\"]\ndefault-members = [\"member\"]\n").unwrap();
        std::fs::write(
            root.path().join("rust/src/lib.rs"),
            "pub fn fix() -> i32 { 0 }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("rust/member/Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("rust/member/src/lib.rs"),
            "pub fn other() -> i32 { 1 }\n",
        )
        .unwrap();
        let plan = preview(request_for(root.path(), &["rust/src/lib.rs"]))
            .await
            .unwrap();
        assert_eq!(plan.request.checks.len(), 1);
        assert_eq!(
            plan.request.checks[0].args,
            [
                "test",
                "--locked",
                "--package",
                "rust-root",
                "--manifest-path",
                "rust/Cargo.toml"
            ]
        );
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("owning packages only")));
    }

    #[tokio::test]
    async fn virtual_rust_manifest_is_not_inferred_as_a_passing_check() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "pub fn value() -> u32 { 1 }\n",
        )
        .unwrap();
        let plan = preview(request_for(root.path(), &["src/lib.rs"]))
            .await
            .unwrap();
        assert!(plan.request.checks.is_empty());
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("owning Cargo package")));
    }

    #[tokio::test]
    async fn node_fallback_excludes_tests_owned_by_another_package() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(
            root.path().join("app.mjs"),
            "export function fix() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(root.path().join("sub/package.json"), "{}\n").unwrap();
        std::fs::write(
            root.path().join("sub/test.mjs"),
            "import assert from 'node:assert/strict';\nassert.equal(1, 1);\n",
        )
        .unwrap();
        let plan = preview(request_for(root.path(), &["app.mjs"]))
            .await
            .unwrap();
        assert!(plan.request.checks.is_empty());
    }

    #[tokio::test]
    async fn node_fallback_does_not_silently_truncate_test_files() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(
            root.path().join("app.mjs"),
            "export function fix() { return 1; }\n",
        )
        .unwrap();
        for index in 0..33 {
            std::fs::write(
                root.path().join(format!("test_{index:02}.mjs")),
                "import assert from 'node:assert/strict';\nassert.equal(1, 1);\n",
            )
            .unwrap();
        }
        let plan = preview(request_for(root.path(), &["app.mjs"]))
            .await
            .unwrap();
        assert!(plan.request.checks.is_empty());
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("more than 32")));
    }

    fn request_for(root: &Path, files: &[&str]) -> LocalRunRequest {
        LocalRunRequest {
            goal: "Fix selected source".into(),
            repository: root.into(),
            files: files.iter().map(PathBuf::from).collect(),
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: false,
            allow_unverified_runtime: false,
            budget: Default::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        }
    }

    #[tokio::test]
    async fn preview_preserves_large_tracked_asset_in_snapshot_identity() {
        use std::io::{Seek, SeekFrom, Write};

        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::create_dir(root.path().join("assets")).unwrap();
        std::fs::write(
            root.path().join("src/add.py"),
            "def add(a, b):\n    return a + b\n",
        )
        .unwrap();
        let asset = root.path().join("assets/large.bin");
        std::fs::File::create(&asset)
            .unwrap()
            .set_len(65 * 1024 * 1024)
            .unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["add", "src/add.py", "assets/large.bin"])
            .status()
            .unwrap()
            .success());

        let plan = preview(request_for(root.path(), &["src/add.py"]))
            .await
            .unwrap();
        let paths = super::inventory(root.path()).await.unwrap();
        assert!(paths.contains(&PathBuf::from("assets/large.bin")));
        let candidate = tempfile::tempdir().unwrap();
        let candidate_root = candidate.path().join("copy");
        super::super::copy_files(root.path(), &candidate_root, &paths)
            .await
            .unwrap();
        assert_eq!(
            super::super::content_hash(&candidate_root, &paths).unwrap(),
            plan.request.expected_baseline_sha256.clone().unwrap()
        );

        let mut file = std::fs::OpenOptions::new().write(true).open(asset).unwrap();
        file.seek(SeekFrom::Start(32 * 1024 * 1024)).unwrap();
        file.write_all(&[1]).unwrap();
        let changed = preview(request_for(root.path(), &["src/add.py"]))
            .await
            .unwrap();
        assert_ne!(
            changed.request.expected_baseline_sha256,
            plan.request.expected_baseline_sha256
        );
    }
    #[tokio::test]
    async fn preview_warns_when_selected_checks_cannot_complete_one_candidate() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(root.path().join("app.py"), "def value():\n    return 1\n").unwrap();
        std::fs::write(
            root.path().join("test_app.py"),
            "def test_value():\n    assert True\n",
        )
        .unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["add", "."])
            .status()
            .unwrap()
            .success());
        let mut request = request_for(root.path(), &["app.py"]);
        request.checks = vec![LocalCheck {
            program: "python".into(),
            args: vec!["-m".into(), "unittest".into(), "discover".into()],
        }];
        request.budget.check_runs = 1;
        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.budget.check_runs, 1);
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("at least 2")));
    }

    #[tokio::test]
    async fn new_file_with_blank_context_discovers_matching_read_only_source() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        let source = "export function formatTotal(value: number) { return `$${value}`; }\n";
        std::fs::write(root.path().join("src/math.ts"), source).unwrap();
        std::fs::write(
            root.path().join("src/other.ts"),
            "export const unrelated = 1;\n",
        )
        .unwrap();
        let mut request = request_for(root.path(), &[]);
        request.goal = "Add a receipt module using formatTotal".into();
        request.new_file = Some("src/receipt.ts".into());
        request.approve_host_execution = true;
        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.files, vec![PathBuf::from("src/math.ts")]);
        assert!(plan.files[0].reason.contains("formatTotal"));
        assert_eq!(
            plan.request.expected_source_hashes[&PathBuf::from("src/math.ts")],
            format!("{:x}", Sha256::digest(source.as_bytes()))
        );
        assert_eq!(plan.creation.unwrap().path, PathBuf::from("src/receipt.ts"));
        assert_eq!(
            plan.contract.likely_files,
            vec![PathBuf::from("src/receipt.ts")]
        );
        assert_eq!(plan.contract.expected_artifacts.len(), 1);
        assert!(!plan.request.approve_host_execution);
        let assembly = context::assemble(
            &plan.request,
            root.path(),
            "Return only a new-file edit",
            context::ModelContext {
                protocol: phonton_types::local::EditProtocol::UnifiedDiff,
                capacity: 4096,
                output: 512,
            },
            "",
            false,
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(assembly
            .evidence
            .excerpts
            .iter()
            .any(|excerpt| excerpt.path == Path::new("src/math.ts")));
        assert!(assembly.prompt.contains("formatTotal"));
        assert!(assembly.prompt.contains("read-only context"));
    }

    #[tokio::test]
    async fn new_file_context_may_read_source_with_embedded_tests() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "pub fn build_widget() -> u32 { 7 }\n#[cfg(test)] mod tests { #[test] fn widget_works() { assert_eq!(super::build_widget(), 7); } }\n",
        )
        .unwrap();
        let mut request = request_for(root.path(), &[]);
        request.goal = "Add a widget helper using build_widget".into();
        request.new_file = Some("src/widget.rs".into());
        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.files, vec![PathBuf::from("src/lib.rs")]);
        assert_eq!(
            plan.contract.likely_files,
            vec![PathBuf::from("src/widget.rs")]
        );
        assert!(!plan
            .warnings
            .iter()
            .any(|warning| warning.contains("No existing source matched")));

        let mut mixed = request_for(root.path(), &["src/lib.rs"]);
        mixed.goal = "Add a widget helper and update build_widget".into();
        mixed.new_file = Some("src/widget.rs".into());
        mixed.editable_existing = vec![PathBuf::from("src/lib.rs")];
        let mixed_plan = preview(mixed).await.unwrap();
        assert_eq!(
            mixed_plan.request.editable_existing,
            vec![PathBuf::from("src/lib.rs")]
        );
    }

    #[tokio::test]
    async fn new_file_with_no_matching_source_keeps_empty_context_with_warning() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/unrelated.ts"),
            "export function other() { return true; }\n",
        )
        .unwrap();
        let mut request = request_for(root.path(), &[]);
        request.goal = "Add a frobnicator".into();
        request.new_file = Some("src/new.py".into());
        let plan = preview(request).await.unwrap();
        assert!(plan.request.files.is_empty());
        assert!(plan.files.is_empty());
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("No existing source matched")));
        assert_eq!(plan.creation.unwrap().path, PathBuf::from("src/new.py"));
    }

    #[tokio::test]
    async fn explicit_creation_plan_records_absence_without_inventing_source_scope() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        let request = LocalRunRequest {
            goal: "Add a small module".into(),
            repository: root.path().into(),
            files: vec![],
            new_file: Some("src/new.py".into()),
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: false,
            allow_unverified_runtime: false,
            budget: Default::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        };
        let plan = preview(request).await.unwrap();
        assert!(plan.files.is_empty());
        assert_eq!(plan.creation.unwrap().path, PathBuf::from("src/new.py"));
        let before = plan.request.expected_baseline_sha256.unwrap();
        std::fs::write(root.path().join("src/new.py"), "value = 1\n").unwrap();
        let current =
            super::super::scoped_hash(root.path(), &[], Some(&PathBuf::from("src/new.py")), false)
                .unwrap();
        assert_ne!(before, current);
    }
    #[tokio::test]
    async fn proposed_package_check_reports_real_test_failure() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"test":"node --test --test-reporter=tap test_add.js"}}"#,
        )
        .unwrap();
        std::fs::write(
            root.path().join("add.js"),
            "exports.add = (a, b) => a - b;\n",
        )
        .unwrap();
        std::fs::write(root.path().join("test_add.js"), "const { test } = require('node:test'); test('addition boundary', () => require('node:assert/strict').equal(require('./add').add(2, 3), 5));\n").unwrap();
        let (proposals, _) = suggested_checks(
            root.path(),
            &[PathBuf::from("package.json")],
            &[PathBuf::from("add.js")],
        )
        .unwrap();
        let check = &proposals[0];
        let output = phonton_sandbox::Sandbox::new(root.path().into(), "plan-test".into())
            .with_host_execution_approval()
            .run_approved_check(
                check.program.clone(),
                check.args.clone(),
                std::time::Duration::from_secs(30),
            )
            .await
            .expect("proposed package check must launch");
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("addition boundary"));
    }
    #[tokio::test]
    async fn discovers_named_source_and_suggests_checks_without_running_them() {
        let root = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(root.path())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(
            root.path().join("config.js"),
            "function parsePort(v) { return parseInt(v); }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("other.js"),
            "function unrelated() { return 42; }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("test_config.js"),
            "throw new Error('must not run while planning');\n",
        )
        .unwrap();
        std::fs::write(root.path().join(".env"), "private=value\n").unwrap();
        std::fs::write(root.path().join("deleted.js"), "unused\n").unwrap();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["add", "deleted.js"])
            .status()
            .unwrap()
            .success());
        std::fs::remove_file(root.path().join("deleted.js")).unwrap();
        let request = LocalRunRequest {
            goal: "Fix parsePort validation".into(),
            repository: root.path().into(),
            files: vec![],
            new_file: None,
            editable_existing: vec![],
            checks: vec![],
            preparation: None,
            approve_host_execution: true,
            allow_unverified_runtime: false,
            budget: Default::default(),
            expected_source_hashes: Default::default(),
            expected_baseline_sha256: None,
        };
        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.files, vec![PathBuf::from("config.js")]);
        assert_eq!(
            plan.request.checks[0].args,
            vec!["--test", "--test-reporter=tap", "test_config.js"]
        );
        assert!(!plan.request.approve_host_execution);
        assert!(plan.request.expected_baseline_sha256.is_some());
        assert!(plan.files[0].reason.contains("parsePort"));
        assert_eq!(plan.contract.goal, "Fix parsePort validation");
        assert_eq!(
            std::fs::read_to_string(root.path().join("config.js")).unwrap(),
            "function parsePort(v) { return parseInt(v); }\n"
        );
    }

    #[tokio::test]
    async fn camel_case_goal_discovers_snake_case_symbol_without_running_source() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        let source = b"export function parse_port(value) { return Number.parseInt(value, 10); }\n";
        std::fs::write(root.path().join("src/port.js"), source).unwrap();
        let mut request = request_for(root.path(), &[]);
        request.goal = "Fix parsePort".into();
        request.approve_host_execution = true;

        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.files, vec![PathBuf::from("src/port.js")]);
        assert!(plan.files[0].reason.contains("parse_port"));
        assert!(!plan.request.approve_host_execution);
        assert_eq!(
            std::fs::read(root.path().join("src/port.js")).unwrap(),
            source
        );
    }

    #[tokio::test]
    async fn exact_symbol_name_stays_ahead_of_snake_case_variant() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/exact.js"),
            "function parsePort() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("src/variant.js"),
            "function parse_port() { return 2; }\n",
        )
        .unwrap();
        let mut request = request_for(root.path(), &[]);
        request.goal = "Fix parsePort".into();

        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.files, vec![PathBuf::from("src/exact.js")]);
    }

    #[tokio::test]
    async fn one_exact_name_beats_multiple_variant_symbols_and_path_hits() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::create_dir_all(root.path().join("src/port")).unwrap();
        std::fs::write(
            root.path().join("src/exact.js"),
            "function parsePort() { return 1; }\n",
        )
        .unwrap();
        let variants = "class First { parse_port() { return 1; } }\nclass Second { parse_port() { return 2; } }\n";
        std::fs::write(root.path().join("src/port/variants.js"), variants).unwrap();
        assert_eq!(
            phonton_index::extract_symbol_spans(variants, Path::new("src/port/variants.js"))
                .iter()
                .filter(|symbol| symbol.name == "parse_port")
                .count(),
            2
        );
        let mut request = request_for(root.path(), &[]);
        request.goal = "Fix parsePort in port handling".into();

        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.files, vec![PathBuf::from("src/exact.js")]);
    }

    #[tokio::test]
    async fn same_case_name_beats_case_insensitive_fallback() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(
            root.path().join("exact.js"),
            "function parsePort() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("folded.js"),
            "function parseport() { return 2; }\n",
        )
        .unwrap();
        let mut request = request_for(root.path(), &[]);
        request.goal = "Fix parsePort".into();

        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.files, vec![PathBuf::from("exact.js")]);
    }

    #[tokio::test]
    async fn distinct_goal_symbols_keep_their_best_spelling_matches() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(
            root.path().join("port.js"),
            "function parsePort() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("host.js"),
            "function validate_host() { return true; }\n",
        )
        .unwrap();
        let mut request = request_for(root.path(), &[]);
        request.goal = "Fix parsePort and validateHost".into();

        let plan = preview(request).await.unwrap();
        assert_eq!(
            plan.request.files,
            vec![PathBuf::from("port.js"), PathBuf::from("host.js")]
        );
    }

    #[tokio::test]
    async fn discovers_named_source_without_a_final_newline() {
        let root = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success());
        let path = PathBuf::from("src/parsePort.js");
        std::fs::create_dir(root.path().join("src")).unwrap();
        let source = b"export function parsePort(value) { return Number(value); }";
        std::fs::write(root.path().join(&path), source).unwrap();
        let mut request = request_for(root.path(), &[]);
        request.goal = "Fix parsePort".into();

        let plan = preview(request).await.unwrap();
        assert_eq!(plan.request.files, vec![path.clone()]);
        assert_eq!(
            plan.files[0].source_sha256,
            format!("{:x}", Sha256::digest(source))
        );
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("no final newline")));
        assert_eq!(std::fs::read(root.path().join(path)).unwrap(), source);
    }

    #[test]
    fn python_check_matches_the_discovered_test_framework() {
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from("app.py");
        let test = PathBuf::from("test_app.py");
        std::fs::write(root.path().join(&source), "def value():\n    return 1\n").unwrap();
        std::fs::write(
            root.path().join(&test),
            "def test_value():\n    assert True\n",
        )
        .unwrap();
        let (checks, caution) = suggested_checks(
            root.path(),
            &[source.clone(), test.clone()],
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert_eq!(checks[0].args, ["-m", "pytest", "--color=no"]);
        assert!(caution.unwrap().contains("pytest"));

        std::fs::write(
            root.path().join(&test),
            "class TestValue:\n    def test_value(self):\n        assert True\n",
        )
        .unwrap();
        let (checks, _) = suggested_checks(
            root.path(),
            &[source.clone(), test.clone()],
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert_eq!(checks[0].args, ["-m", "pytest", "--color=no"]);

        std::fs::write(
            root.path().join(&test),
            "import unittest\nclass TestValue(unittest.TestCase):\n    def test_value(self):\n        self.assertTrue(True)\n",
        )
        .unwrap();
        let (checks, _) = suggested_checks(
            root.path(),
            &[source.clone(), test.clone()],
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert_eq!(checks[0].args, ["-m", "unittest", "discover"]);

        std::fs::write(
            root.path().join(&test),
            "import unittest as ut\nclass TestValue(ut.TestCase):\n    def test_value(self):\n        self.assertTrue(True)\n",
        )
        .unwrap();
        let (checks, _) = suggested_checks(
            root.path(),
            &[source.clone(), test.clone()],
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert_eq!(checks[0].args, ["-m", "unittest", "discover"]);

        std::fs::write(
            root.path().join(&test),
            "from unittest import TestCase\nclass TestValue(TestCase):\n    def test_value(self):\n        self.assertTrue(True)\n",
        )
        .unwrap();
        let (checks, _) = suggested_checks(
            root.path(),
            &[source.clone(), test.clone()],
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert_eq!(checks[0].args, ["-m", "unittest", "discover"]);

        std::fs::write(
            root.path().join(&test),
            "from unittest import TestCase\nclass TestPytest:\n    def test_fail(self):\n        assert False\nclass TestUnit(TestCase):\n    def test_pass(self):\n        self.assertTrue(True)\n",
        )
        .unwrap();
        let (checks, _) = suggested_checks(
            root.path(),
            &[source.clone(), test.clone()],
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert_eq!(checks[0].args, ["-m", "pytest", "--color=no"]);

        std::fs::write(
            root.path().join(&test),
            "from unittest import TestCase\nclass TestCaseBehavior:\n    def test_fail(self):\n        assert False\nclass TestUnit(TestCase):\n    def test_pass(self):\n        self.assertTrue(True)\n",
        )
        .unwrap();
        let (checks, _) = suggested_checks(
            root.path(),
            &[source.clone(), test.clone()],
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert_eq!(checks[0].args, ["-m", "pytest", "--color=no"]);

        let config = PathBuf::from("pytest.ini");
        std::fs::write(root.path().join(&config), "[pytest]\n").unwrap();
        let (checks, _) = suggested_checks(
            root.path(),
            &[source, test, config],
            &[PathBuf::from("app.py")],
        )
        .unwrap();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].args, ["-m", "pytest", "--color=no"]);
        assert_eq!(
            checks[1].args,
            [
                "-m",
                "pytest",
                "--color=no",
                "-o",
                "addopts=",
                "test_app.py"
            ]
        );
    }

    #[test]
    fn inferred_unittest_checks_cover_nested_tests_without_package_markers() {
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from("app.py");
        let root_test = PathBuf::from("test_ok.py");
        let nested_test = PathBuf::from("nested/test_app.py");
        std::fs::create_dir(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join(&source), "def value():\n    return 1\n").unwrap();
        std::fs::write(root.path().join(&root_test), "import unittest\nclass TestOk(unittest.TestCase):\n    def test_ok(self):\n        self.assertTrue(True)\n").unwrap();
        std::fs::write(root.path().join(&nested_test), "import unittest\nclass TestApp(unittest.TestCase):\n    def test_app(self):\n        self.fail('must run')\n").unwrap();
        let paths = vec![source.clone(), root_test, nested_test];

        let (checks, warning) = suggested_checks(root.path(), &paths, &[source]).unwrap();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].args, ["-m", "unittest", "discover"]);
        assert_eq!(
            checks[1].args,
            ["-m", "unittest", "discover", "-s", "nested", "-p", "test*.py"]
        );
        assert!(warning.unwrap().contains("root discovery would skip"));

        let nested_only = vec![paths[0].clone(), paths[2].clone()];
        let (checks, _) =
            suggested_checks(root.path(), &nested_only, std::slice::from_ref(&paths[0])).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].args[3..], ["-s", "nested", "-p", "test*.py"]);

        let marker = PathBuf::from("nested/__init__.py");
        std::fs::write(root.path().join(&marker), "").unwrap();
        let with_package = vec![paths[0].clone(), paths[2].clone(), marker];
        let (checks, warning) =
            suggested_checks(root.path(), &with_package, std::slice::from_ref(&paths[0])).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].args, ["-m", "unittest", "discover"]);
        assert!(warning.is_none());
    }

    #[test]
    fn inferred_pytest_checks_include_nondefault_test_filenames() {
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from("app.py");
        let default_test = PathBuf::from("test_app.py");
        let nondefault_test = PathBuf::from("testlegacy.py");
        std::fs::write(root.path().join(&source), "def value():\n    return 1\n").unwrap();
        std::fs::write(
            root.path().join(&default_test),
            "def test_value():\n    assert True\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join(&nondefault_test),
            "def test_legacy():\n    assert False\n",
        )
        .unwrap();
        let paths = vec![source.clone(), default_test, nondefault_test];

        let (checks, warning) =
            suggested_checks(root.path(), &paths, std::slice::from_ref(&source)).unwrap();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].args, ["-m", "pytest", "--color=no"]);
        assert_eq!(
            checks[1].args,
            [
                "-m",
                "pytest",
                "--color=no",
                "-o",
                "addopts=",
                "testlegacy.py"
            ]
        );
        assert!(warning.unwrap().contains("explicit"));

        let only_nondefault = vec![source.clone(), paths[2].clone()];
        let (checks, _) =
            suggested_checks(root.path(), &only_nondefault, std::slice::from_ref(&source)).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(
            checks[0].args,
            [
                "-m",
                "pytest",
                "--color=no",
                "-o",
                "addopts=",
                "testlegacy.py"
            ]
        );

        let config = PathBuf::from("pytest.ini");
        std::fs::write(root.path().join(&config), "[pytest]\n").unwrap();
        let configured = vec![source.clone(), paths[1].clone(), config];
        let (checks, _) = suggested_checks(root.path(), &configured, &[source]).unwrap();
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].args, ["-m", "pytest", "--color=no"]);
        assert_eq!(
            checks[1].args,
            [
                "-m",
                "pytest",
                "--color=no",
                "-o",
                "addopts=",
                "test_app.py"
            ]
        );
    }

    #[test]
    fn inferred_pytest_recognizes_current_configuration_names() {
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from("app.py");
        let test = PathBuf::from("test_app.py");
        std::fs::write(root.path().join(&source), "def value():\n    return 1\n").unwrap();
        std::fs::write(
            root.path().join(&test),
            "def test_value():\n    assert True\n",
        )
        .unwrap();
        for (name, content) in [
            (".pytest.ini", "[pytest]\n"),
            ("pytest.toml", "[pytest]\n"),
            (".pytest.toml", "[pytest]\n"),
            ("PyTest.ini", "[pytest]\n"),
            ("pyproject.toml", "[tool.pytest]\n"),
        ] {
            let config = PathBuf::from(name);
            std::fs::write(root.path().join(&config), content).unwrap();
            let paths = vec![source.clone(), test.clone(), config];
            let (checks, _) =
                suggested_checks(root.path(), &paths, std::slice::from_ref(&source)).unwrap();
            assert_eq!(checks.len(), 2, "{name}");
            std::fs::remove_file(root.path().join(name)).unwrap();
        }
    }

    #[test]
    fn inferred_python_checks_withhold_shadowed_runner_modules() {
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from("app.py");
        let test = PathBuf::from("test_app.py");
        std::fs::write(root.path().join(&source), "def value():\n    return 1\n").unwrap();
        std::fs::write(
            root.path().join(&test),
            "def test_value():\n    assert True\n",
        )
        .unwrap();
        let (checks, warning) = suggested_checks(
            root.path(),
            &[source.clone(), test.clone(), PathBuf::from("pytest.py")],
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert!(checks.is_empty());
        assert!(warning.unwrap().contains("shadow"));

        let (checks, warning) = suggested_checks(
            root.path(),
            &[source.clone(), test.clone(), PathBuf::from("pytest.pyc")],
            std::slice::from_ref(&source),
        )
        .unwrap();
        assert!(checks.is_empty());
        assert!(warning.unwrap().contains("shadow"));

        std::fs::write(
            root.path().join(&test),
            "import unittest\nclass TestValue(unittest.TestCase):\n    def test_value(self):\n        self.assertTrue(True)\n",
        )
        .unwrap();
        let (checks, warning) = suggested_checks(
            root.path(),
            &[source.clone(), test, PathBuf::from("unittest/__main__.py")],
            &[source],
        )
        .unwrap();
        assert!(checks.is_empty());
        assert!(warning.unwrap().contains("shadow"));
    }

    #[test]
    fn inferred_pytest_withholds_unbounded_explicit_paths() {
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from("app.py");
        std::fs::write(root.path().join(&source), "def value():\n    return 1\n").unwrap();
        let mut paths = vec![source.clone()];
        for number in 0..33 {
            let test = PathBuf::from(format!("testcase{number}.py"));
            std::fs::write(
                root.path().join(&test),
                "def test_value():\n    assert True\n",
            )
            .unwrap();
            paths.push(test);
        }
        let (checks, warning) = suggested_checks(root.path(), &paths, &[source]).unwrap();
        assert!(checks.is_empty());
        assert!(warning.unwrap().contains("bounded command size"));
    }

    #[test]
    fn inferred_unittest_withholds_more_than_four_discovery_roots() {
        let root = tempfile::tempdir().unwrap();
        let source = PathBuf::from("app.py");
        std::fs::write(root.path().join(&source), "def value():\n    return 1\n").unwrap();
        let mut paths = vec![source.clone()];
        for number in 0..5 {
            let directory = format!("tests{number}");
            std::fs::create_dir(root.path().join(&directory)).unwrap();
            let path = PathBuf::from(format!("{directory}/test_app.py"));
            std::fs::write(root.path().join(&path), "import unittest\nclass TestApp(unittest.TestCase):\n    def test_app(self):\n        self.assertTrue(True)\n").unwrap();
            paths.push(path);
        }
        let (checks, warning) = suggested_checks(root.path(), &paths, &[source]).unwrap();
        assert!(checks.is_empty());
        assert!(warning.unwrap().contains("more than four discovery roots"));
    }
}
