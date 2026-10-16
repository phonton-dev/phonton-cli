//! Direct jest checks: the candidate's own jest entry point, run by node with a
//! JSON report that names the candidate test files and their test results.
use phonton_types::local_run::LocalCheck;
use std::path::Path;

/// The one entry point recognized as jest: the copy installed in the candidate.
const ENTRY: &str = "node_modules/jest/bin/jest.js";

/// True when a node check starts jest from candidate-relative `node_modules`.
/// Such a check is judged only by its JSON report, never by its exit alone.
pub(super) fn names_entry(check: &LocalCheck) -> bool {
    super::is_node_command(check)
        && check.args.first().is_some_and(|arg| {
            let arg = arg.replace('\\', "/");
            arg.strip_prefix("./").unwrap_or(&arg) == ENTRY
        })
}

/// The supported form: `--json` plus flags that select tests or set output,
/// none of which load a runner, reporter, setup file or config the captured
/// repository does not already define.
pub(super) fn is_supported(check: &LocalCheck) -> bool {
    if !names_entry(check) {
        return false;
    }
    let mut json = false;
    let mut args = check.args[1..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--ci" | "--runInBand" | "-i" | "--silent" | "--verbose" | "--no-cache"
            | "--no-watchman" => {}
            "--rootDir" | "--testNamePattern" | "-t" => {
                if args.next().is_none_or(|value| value.starts_with('-')) {
                    return false;
                }
            }
            flag if flag.starts_with("--rootDir=") || flag.starts_with("--testNamePattern=") => {}
            flag if flag.starts_with('-') => return false,
            _ => {} // a test path pattern
        }
    }
    json
}

/// Read jest's `--json` report from stdout. `Ok` describes the completed passing
/// tests in candidate files; `Err` says why the run is not verification.
pub(super) fn report(check: &LocalCheck, directory: &Path, stdout: &str) -> Result<String, String> {
    if !is_supported(check) {
        return Err("This jest command is not a supported verification form. Use node node_modules/jest/bin/jest.js --ci --json with optional --runInBand, --rootDir, --testNamePattern and test path patterns".into());
    }
    let unreadable = || {
        "jest's --json report was not the whole of stdout; candidate output or a missing report leaves the run unproven".to_string()
    };
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|_| unreadable())?;
    let count = |key: &str| value[key].as_u64();
    if value["success"] != true
        || value["wasInterrupted"] == true
        || count("numFailedTests") != Some(0)
        || count("numFailedTestSuites") != Some(0)
        || count("numRuntimeErrorTestSuites") != Some(0)
    {
        return Err("jest's report does not show a completed run without failures".into());
    }
    let root = directory
        .canonicalize()
        .map_err(|error| format!("Candidate path unavailable: {error}"))?;
    let files = value["testResults"].as_array().ok_or_else(unreadable)?;
    let (mut passed, mut skipped) = (0_u64, 0_u64);
    for file in files {
        let inside = file["name"]
            .as_str()
            .and_then(|name| Path::new(name).canonicalize().ok())
            .is_some_and(|path| path.starts_with(&root) && path.is_file());
        if !inside {
            return Err("jest reported a test file outside this candidate".into());
        }
        if file["status"] == "failed" {
            return Err("jest reported a failed test file".into());
        }
        for test in file["assertionResults"].as_array().ok_or_else(unreadable)? {
            match test["status"].as_str() {
                Some("passed") => passed += 1,
                Some("pending" | "skipped" | "todo" | "disabled") => skipped += 1,
                _ => return Err("jest reported a test that did not pass".into()),
            }
        }
    }
    if passed == 0 {
        return Err("jest's report shows no completed passing test".into());
    }
    Ok(format!(
        "jest's report shows {passed} passed and {skipped} skipped or todo tests in {} candidate test files",
        files.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jest(args: &[&str]) -> LocalCheck {
        LocalCheck {
            program: "node".into(),
            args: std::iter::once(ENTRY)
                .chain(args.iter().copied())
                .map(str::to_owned)
                .collect(),
        }
    }

    #[test]
    fn only_the_candidate_entry_with_a_json_report_and_safe_flags_is_supported() {
        assert!(is_supported(&jest(&["--ci", "--json"])));
        assert!(is_supported(&jest(&[
            "--json",
            "--rootDir",
            ".",
            "-t",
            "adds",
            "src"
        ])));
        let mut windows = jest(&["--json"]);
        windows.args[0] = r".\node_modules\jest\bin\jest.js".into();
        assert!(is_supported(&windows));
        assert!(names_entry(&jest(&["--ci"])));
        assert!(!is_supported(&jest(&["--ci"])));
        for unsafe_flag in [
            "--config=jest.other.js",
            "--reporters=./forge.js",
            "--testRunner=./runner.js",
            "--setupFiles=./setup.js",
            "--passWithNoTests",
            "--watchAll",
        ] {
            assert!(
                !is_supported(&jest(&["--json", unsafe_flag])),
                "{unsafe_flag}"
            );
        }
        assert!(!is_supported(&jest(&["--json", "--rootDir"])));
        let mut elsewhere = jest(&["--json"]);
        elsewhere.args[0] = "tools/jest.js".into();
        assert!(!names_entry(&elsewhere));
    }

    #[test]
    fn report_needs_a_passing_test_in_a_candidate_file() {
        let root = tempfile::tempdir().unwrap();
        let spec = root.path().join("sum.spec.js");
        std::fs::write(&spec, "").unwrap();
        let check = jest(&["--ci", "--json"]);
        let report_for = |name: &str, statuses: &[&str]| {
            let tests: Vec<_> = statuses
                .iter()
                .map(|status| format!(r#"{{"status":"{status}","title":"t"}}"#))
                .collect();
            format!(
                r#"{{"success":true,"wasInterrupted":false,"numFailedTests":0,"numFailedTestSuites":0,"numRuntimeErrorTestSuites":0,"testResults":[{{"name":{},"status":"passed","assertionResults":[{}]}}]}}"#,
                serde_json::to_string(name).unwrap(),
                tests.join(",")
            )
        };
        let name = spec.to_string_lossy().into_owned();
        let passing = report_for(&name, &["passed", "pending"]);
        let summary = report(&check, root.path(), &passing).unwrap();
        assert!(summary.contains("1 passed and 1 skipped"), "{summary}");

        assert!(report(&check, root.path(), &report_for(&name, &["pending"])).is_err());
        assert!(report(&check, root.path(), &report_for(&name, &["failed"])).is_err());
        assert!(report(&check, root.path(), &format!("forged\n{passing}")).is_err());
        let failing = passing.replace(r#""numFailedTests":0"#, r#""numFailedTests":1"#);
        assert!(report(&check, root.path(), &failing).is_err());
        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("sum.spec.js");
        std::fs::write(&outside, "").unwrap();
        let outside = report_for(&outside.to_string_lossy(), &["passed"]);
        assert!(report(&check, root.path(), &outside).is_err());
        assert!(report(&jest(&["--ci"]), root.path(), &passing).is_err());
    }
}
