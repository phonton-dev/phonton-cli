//! Conservative Python source inclusion from process-reported execution traces.
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

const MAX_TRACE_FILES: usize = 64;
const MAX_TRACE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TOTAL_TRACE_BYTES: u64 = 8 * 1024 * 1024;

// The hook is harness-owned and lives outside the candidate. An audit `exec`
// event means Python executed a code object bearing this filename; it does not
// prove an assertion covered the edit, or protect against a hostile process.
const SITE_HOOK: &str = r#"import atexit as _atexit
import importlib.machinery as _machinery
import importlib.util as _import_util
import json as _json
import os as _os
import sys as _sys

_seen = set()
_truncated = False
_destination = _os.environ.get("PHONTON_PYTHON_TRACE_DIR")

def _identity(path):
    value = _os.path.normcase(_os.path.realpath(_os.path.abspath(path)))
    if _os.name == "nt":
        if value.startswith("\\\\?\\unc\\"):
            return "\\\\" + value[8:]
        if value.startswith("\\\\?\\"):
            return value[4:]
    return value

try:
    with open(_os.path.join(_os.path.dirname(__file__), "expected.json"), encoding="utf-8") as _input:
        _expected = {
            _identity(name)
            for name in _json.load(_input)
        }
except (OSError, ValueError, TypeError):
    _expected = set()
    _truncated = True

def _record(event, args):
    global _truncated
    if event != "exec" or not args:
        return
    name = getattr(args[0], "co_filename", None)
    if not isinstance(name, str) or name.startswith("<"):
        return
    try:
        resolved = _identity(name)
    except (OSError, ValueError, TypeError):
        return
    if resolved not in _expected:
        return
    if len(resolved) > 2048 or len(_seen) >= 512:
        _truncated = True
        return
    _seen.add(resolved)

def _save():
    if not _destination:
        return
    try:
        with open(_os.path.join(_destination, str(_os.getpid()) + ".json"), "x", encoding="utf-8") as output:
            _json.dump({"schema": 1, "paths": sorted(_seen), "truncated": _truncated}, output)
    except OSError:
        pass

_sys.addaudithook(_record)
_atexit.register(_save)

# A normal interpreter may already have sitecustomize in site-packages. Keep
# its startup effects: the private hook must not silently replace that module.
def _chain_sitecustomize():
    global _truncated
    _hook_file = _identity(__file__)
    for _entry in _sys.path:
        _spec = _machinery.PathFinder.find_spec("sitecustomize", [_entry])
        if _spec is None:
            continue
        if _spec.origin and _identity(_spec.origin) == _hook_file:
            continue
        if _spec.loader is None:
            _truncated = True
            return
        _current = _sys.modules.get("sitecustomize")
        try:
            _module = _import_util.module_from_spec(_spec)
            _sys.modules["sitecustomize"] = _module
            _spec.loader.exec_module(_module)
        except BaseException:
            _truncated = True
            if _current is not None:
                _sys.modules["sitecustomize"] = _current
            raise
        return

_chain_sitecustomize()
"#;

pub(super) fn changed_sources(changes: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    changes
        .iter()
        .filter(|path| is_source(path))
        .cloned()
        .collect()
}

pub(super) fn is_source(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("py"))
}

pub(super) struct PythonTrace {
    _root: tempfile::TempDir,
    hook: PathBuf,
    output: PathBuf,
}

impl PythonTrace {
    pub(super) fn new(candidate: &Path, sources: &[PathBuf]) -> Result<Self, String> {
        let candidate = candidate
            .canonicalize()
            .map_err(|error| format!("Candidate path unavailable: {error}"))?;
        let mut expected = Vec::with_capacity(sources.len());
        for source in sources {
            let path = candidate
                .join(source)
                .canonicalize()
                .map_err(|error| format!("Edited Python path unavailable: {error}"))?;
            if !path.starts_with(&candidate) || !path.is_file() {
                return Err("Edited Python path leaves the candidate or is not a file".into());
            }
            expected.push(path);
        }
        let root = tempfile::tempdir()
            .map_err(|error| format!("Could not create a private Python trace folder: {error}"))?;
        let expected_json = serde_json::to_vec(&expected)
            .map_err(|error| format!("Could not serialize Python source paths: {error}"))?;
        let hook = root.path().join("hook");
        let output = root.path().join("output");
        fs::create_dir(&hook)
            .and_then(|()| fs::create_dir(&output))
            .and_then(|()| fs::write(hook.join("sitecustomize.py"), SITE_HOOK))
            .and_then(|()| fs::write(hook.join("expected.json"), expected_json))
            .map_err(|error| format!("Could not prepare the Python trace hook: {error}"))?;
        Ok(Self {
            _root: root,
            hook,
            output,
        })
    }

    pub(super) fn hook_dir(&self) -> &Path {
        &self.hook
    }

    pub(super) fn output_dir(&self) -> &Path {
        &self.output
    }
}

pub(super) fn conflicting_sitecustomize(candidate: &Path) -> bool {
    candidate.join("sitecustomize.py").exists() || candidate.join("sitecustomize.pyc").exists()
}

pub(super) fn observed_sources(
    trace_dir: &Path,
    candidate_dir: &Path,
) -> Result<BTreeSet<PathBuf>, String> {
    let root = candidate_dir
        .canonicalize()
        .map_err(|error| format!("Candidate path unavailable: {error}"))?;
    let mut observed = BTreeSet::new();
    let mut files = 0;
    let mut total_bytes = 0_u64;
    for entry in fs::read_dir(trace_dir)
        .map_err(|error| format!("Python trace folder unavailable: {error}"))?
    {
        let entry = entry.map_err(|error| format!("Python trace entry unavailable: {error}"))?;
        files += 1;
        if files > MAX_TRACE_FILES {
            return Err("Python trace file count exceeds inspection bound".into());
        }
        if entry.path().extension().and_then(|part| part.to_str()) != Some("json") {
            return Err("Unexpected Python trace output".into());
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("Python trace metadata unavailable: {error}"))?;
        total_bytes = total_bytes.saturating_add(metadata.len());
        if !metadata.file_type().is_file()
            || metadata.len() > MAX_TRACE_BYTES
            || total_bytes > MAX_TOTAL_TRACE_BYTES
        {
            return Err("Python trace is linked, not a file, or exceeds inspection bound".into());
        }
        let bytes =
            fs::read(entry.path()).map_err(|error| format!("Python trace unreadable: {error}"))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Python trace JSON invalid: {error}"))?;
        if value.get("schema").and_then(serde_json::Value::as_u64) != Some(1)
            || value.get("truncated").and_then(serde_json::Value::as_bool) != Some(false)
        {
            return Err("Python trace schema is unsupported or output was truncated".into());
        }
        let paths = value
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .ok_or("Python trace paths missing")?;
        if paths.len() > 512 {
            return Err("Python trace path count exceeds inspection bound".into());
        }
        for name in paths {
            let name = name.as_str().ok_or("Python trace path is invalid")?;
            if name.len() > 2048 {
                return Err("Python trace path exceeds inspection bound".into());
            }
            let path = Path::new(name);
            if !path.is_absolute() {
                return Err("Python trace path is not absolute".into());
            }
            if let Ok(path) = path.canonicalize() {
                if path.starts_with(&root) {
                    observed.insert(path);
                }
            }
        }
    }
    if files == 0 {
        return Err("Python did not write execution trace output".into());
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_and_malformed_traces_do_not_claim_source_inclusion() {
        let candidate = tempfile::tempdir().unwrap();
        let trace = tempfile::tempdir().unwrap();
        assert!(observed_sources(trace.path(), candidate.path()).is_err());
        fs::write(trace.path().join("1.json"), b"{bad").unwrap();
        assert!(observed_sources(trace.path(), candidate.path()).is_err());
        fs::write(
            trace.path().join("1.json"),
            br#"{"schema":1,"paths":[],"truncated":true}"#,
        )
        .unwrap();
        assert!(observed_sources(trace.path(), candidate.path()).is_err());
    }

    #[test]
    fn exact_candidate_paths_only() {
        let candidate = tempfile::tempdir().unwrap();
        let trace = tempfile::tempdir().unwrap();
        fs::create_dir(candidate.path().join("src")).unwrap();
        let included = candidate.path().join("src/logic.py");
        fs::write(&included, "answer = 42\n").unwrap();
        let sibling = tempfile::tempdir().unwrap();
        let outside = sibling.path().join("logic.py");
        fs::write(&outside, "answer = 0\n").unwrap();
        fs::write(
            trace.path().join("1.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": 1,
                "paths": [included, outside],
                "truncated": false
            }))
            .unwrap(),
        )
        .unwrap();
        let loaded = observed_sources(trace.path(), candidate.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(&included.canonicalize().unwrap()));
    }
}
