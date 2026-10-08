# Unreleased local harness preview

Direct pytest checks now accept standard ANSI-colored passing summaries.
`-m pytest` after a Python script or inline-code operand remains a script
argument and cannot certify that the pytest runner executed.

Desktop setup, download, calibration, selection, deselection, and removal now
bind to the local endpoint and managed storage folder the model page displayed.
If a separate CLI changes either setting before admission, the operation is
rejected before network or file mutation and Desktop refreshes its status.
The bundled engine advertises this operation-binding API; older engines need
reconnection to the current bundle.

For JS/TS edits, a passing direct Node TAP test now needs process-reported V8
coverage showing each edited source loaded from the complete candidate, even
when an edit was inherited from an earlier repair or mixed creation. An
unrelated passing test stays visible but adds Not run inclusion evidence, so
the candidate cannot become review-ready. The plan warns when its checks
cannot prove inclusion. Passing checks save their matched source paths in the
receipt; older JS/TS reviews need a current run before Apply. Loading source
does not prove assertion quality.

Edited Python files now need process-reported execution evidence from a direct
Python `unittest`, pytest, or candidate-local script check. The runner injects
a private startup hook without changing the selected command. Every edited
`.py` file in the complete candidate must appear in its bounded trace before
the candidate can become review-ready. The hook filters to those exact edited
paths and preserves interpreter-level `sitecustomize` startup effects. An
unrelated passing check retains its
result and gains a separate Not run inclusion item. Missing, malformed, or
suppressed traces and a conflicting candidate `sitecustomize` leave inclusion
unverified; plans warn about the limitation. Loading a file does not prove an
assertion exercised it. Older Python reviews need a current run before Apply.

A malformed first edit can now use the final allowed model call for a direct
retry from the captured baseline when a separate strategy proposal would exceed
the search budget. Identical rejected output still stops; mixed creation does
not start a retry it cannot complete.

`models catalog --snapshot` now includes the hardware reading used to compute
each model's fit and first-try guidance. The default catalog command still
prints the existing model array; a later status reading can differ as memory
availability changes.

Selected Go tests now require a terminal passing result for each edited `.go` file's
root-module package. A passing test in an unrelated package leaves separate
Not run evidence instead of making an invalid or uncompiled edit review-ready.
Nested modules and ambiguous module identity remain unverified; package
inclusion alone does not prove assertion quality.
Non-`.go` Go package source remains Not run until file-level inclusion can be
attested.

Cargo tests now need both inspectable compiler dependency evidence and an active
module path for every edited file before a local candidate becomes
review-ready. This includes Rust modules with nonstandard extensions and can
leave mixed non-Rust edits unverified. An unrelated passing test cannot verify a
feature-gated file Cargo skipped, even if another part of the crate reads it
as text. Missing inclusion adds separate Not run evidence without changing the
command's result; source inclusion alone does not establish assertion quality.

Local search now recognizes JSON-escaped Windows candidate directories in
failed-check output. If a repair produces the same failure at a new candidate
path and gains no passing check, the controller restarts from baseline rather
than treating the changed path text as progress. The saved check output and
verification status are unchanged.

A selected Go test now rejects conditional package source before verification,
including tagged Go and assembly files, ignored names, and cgo imports. A
passing `go test ./...` can otherwise omit the edited file while another test
passes. The current rule is conservative: it can also reject a tagged or
platform-specific file that an explicit check would compile.

Up to eight unconfirmed model-install requests now retain their exact tags
and endpoints in local state, including requests that stop before transfer.
`models status` reconciles each with current runtime inventory, distinguishing
installed, absent, unavailable, ambiguous, and changed-endpoint results. A
retry is explicit; partial layers are not counted as installed or guaranteed
reusable. State saves also recover from a stale temporary file left by an
earlier process with the same PID.

Interrupted calibration now saves each completed probe in an explicitly
incomplete local-state record. `models status` exposes it even when the runtime
is offline. It cannot be selected; retrying starts fresh, and an earlier valid
profile remains until full recalibration finishes.

Pytest checks now inspect captured pytest configuration relevant to the check
before any project command or model edit, including an explicit `-c` file.
Command-line `-o` overrides are inspected as well.
`testpaths`, `pythonpath`, and `addopts` must
stay candidate-relative; a configuration pointing at tests or imports in the
original repository or selecting an installed package with `--pyargs` is
refused. Unrelated nested config files do not block a root check. Other project
code and plugins still need review. Strategy and edit inference also recheck
the shared wall-time budget after prompt assembly and runtime admission, so an
expired run sends no new model request.

Opening a completed run rechecks its saved baseline and selected candidate
against the recorded hashes and canonical diff. If that evidence changed or is
unavailable, the receipt keeps its history but clears the review selection and
blocks Apply. The same check runs when the active engine reports a terminal
goal; recent-run listing reads only metadata.

After a failed candidate, the shared runner reserves enough remaining check
slots for every selected command and any dependency preparation before another
model call. If only a partial check set fits, it keeps the earlier failure and
ends the search budget instead of generating an uncheckable repair.

Restart search now distinguishes a repeated strategy at the same source target
from the same kind of edit at another reviewed path or span. The latter may
consume the remaining search budget; candidate bytes and selected checks still
decide whether it is useful.

An interrupted candidate check now leaves the exact pre-check diff and hash in
the run receipt. Its unfinished results remain Unavailable, separate check
journals can be inspected, and Apply remains blocked. Orderly cancellation
records elapsed time through the stop; a hard crash retains the last saved
timing observation.

Windows model fit now measures physical RAM through the OS API. A failed or
slow optional CIM probe does not erase that reading; calibration and goal
admission still remeasure memory when they run.

The shared model-removal API now checks for a unique usable installed identity
before sending DELETE. An ambiguous or incomplete inventory leaves weights
and saved calibration untouched; confirmed absence is still required afterward.

Equivalent default-library model names now resolve to the same saved
calibration across status, selection and local goal admission when the digest
and runtime still match. This also covers an already loaded model; failed
recalibration clears selection, and duplicate installed or resident aliases
are refused. Calibration's final inventory recheck accepts an equivalent name
only when the measured digest and size still match uniquely.

`models status --json` now reports the same complete calibration profile
fingerprint used by local goal admission. Desktop uses it to detect a
same-model recalibration before dispatch; the engine still checks at start.

Verbose failed checks now retain marked start/end excerpts in local receipts.
Baseline and candidate repair feedback budgets stdout and stderr separately,
so a trailing assertion can reach the next model call even after long setup logs.
The omitted middle is still unavailable to search.

Read-only goal planning now finds a parsed `parse_port` symbol when the goal
names `parsePort`, and bounded context retrieval prioritizes that symbol over
unrelated source. Exact names remain preferred and the proposed scope remains
visible for review.

Mixed Apply preserves its prepared recovery journal when the final
creation-path recheck fails, and reports that validation error separately
from an already-published target.

A successful `cargo run -- test` no longer counts as a Cargo test. The word
`test` after `--` belongs to the binary, so the check is diagnostic **Not run**
and cannot make a candidate review-ready. Real `cargo test` still needs a
completed passing-test summary.

Direct local Cargo test checks refuse `--config` overrides and applicable
repository, ancestor, or Cargo-home config that selects a target runner or
includes other files. The selected Cargo executable is resolved outside the
candidate and run by absolute path, including when the request names plain
`cargo`; the candidate is checked again immediately before execution. This
does not make arbitrary project test output independent attestation.

Hosted goal review descriptions, rollback labels, and new completion memories
now use the task text without the retrieved-memory preamble. The worker still
receives that prior context. Existing memory-prefixed rollback labels are
cleaned when `phonton review` renders them; keyword memory search is still
global to the local store and can suggest unrelated same-name work. A legacy
memory preamble and a current multi-paragraph task no longer lose early task
paragraphs. Local template matching also uses the complete task after context
separation.

Hosted verified diffs in a nested project now apply at the selected working
directory; staging, checkpoint trees and approval checks use the corresponding
Git-root paths. Review from that same project directory so path identity is
checked against the saved checkpoint.

`phonton models deselect` clears the selected local model while preserving its
download and calibration, including when the runtime is offline. Removal is a
separate explicit operation, which can free only unshared model layers.

Desktop Local models now surfaces saved managed-runtime recovery while Ollama is
offline. A blocked folder identity or redirected path must be restored before
setup; eligible missing or stale regular launch receipts can retry once the port is
free. The engine reports setup retry eligibility separately from whether a
coding goal may run. An interrupted owned download stage remains retryable.

Adaptive search now tracks newly passing checks and which check failed. If a
repair gains a pass or moves a generic failure to another check, it keeps the
new candidate as a repair branch. Losing a pass without either signal restarts
from baseline.

An approved `python --version` or other successful command that does not run a
supported candidate test or candidate-local script now stays diagnostic **Not
run** evidence. It cannot make a broken candidate review-ready. Direct local
scripts remain available as user-selected checks, with their reported exit
status and captured output visible for review.

Calibration can use an exact installed model already loaded in Ollama when a
cold load does not fit. It still requires at least 1.5 GiB available host RAM,
sufficient context and unload time, and a model limit for automatic context.
Memory and resident identity are checked before each probe; this is a
point-in-time retry, not a reservation or a successful real-model calibration.

Local goal checks now refuse command paths that escape the candidate copy,
including parent-relative paths, rooted arguments, file URLs, and paths in
space-separated flag values. A direct `../../../repo/tests` check can no longer
pass by testing untouched original files. This is a static path guard, not
isolation of approved host commands. Go checks also require candidate-relative
package targets, so a standard-library package cannot substitute for candidate
tests. Go runner replacement flags are also refused.
Pytest `--pyargs` and unittest selectors resolving outside the candidate are
refused for the same reason.

Local goal plans now confirm the selected model's installed digest, runtime
version, and context metadata before showing it as ready. A stale selection
leaves the source plan reviewable with a warning and no runnable model. Start
rechecks before writing attempt evidence; the worker checks again before
repository context reaches the runtime.

`models catalog` now reports a conservative `pre_setup_storage` allowance for
manifest-backed entries while the managed folder remains unused and valid.
The shared Rust calculation includes runtime setup headroom, model bytes and
the pull reserve; CLI and Desktop read the same result before setup. Live
setup and pull checks remain authoritative.

Model install now waits for a terminal success event and confirms the requested
tag, digest and size in the runtime's installed inventory before reporting it.
Removal likewise requires the model to disappear from installed inventory;
unconfirmed removal keeps its calibration evidence.
Explicit `python -m pytest` and `python -m unittest` checks refuse repository
modules that could replace those runners in a candidate check copy. Attached
module and inline-code flags, windowed launchers, and command wrappers receive
the same fail-closed treatment.

Inferred Python checks now cover nested `unittest` directories without package
markers and recognized pytest files that default filename discovery can skip.
Large or ambiguous scopes ask for explicit verification commands. Passing
runner output still requires review of actual test collection. The explicit
pytest check clears project `addopts`; pytest configuration and runner-shadow
files are protected from model edits or withhold inference.
Host-check receipts now state the proof limit explicitly: candidate code can
terminate a runner or forge output, so a reported pass needs human review of
the exact diff and logs before Apply.

`python -m unittest` now requires at least one ordinary passing test to report
verification as passed. An expected-failure-only suite remains visible with its
output and exit code but is **Not run** for review and Apply.

This is a local source note, not a published CLI release.
