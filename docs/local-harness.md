# Local models and scoped coding goals

This is an unreleased development workflow. Build the current CLI source before
using it; published packages do not contain these additions yet.

## Set up a model

`phonton models status` reads CPU, RAM, NVIDIA memory when available, runtime
version and installed model metadata. It always prints JSON and also accepts
`--json` before or after an optional numeric context. Unknown hardware stays
unknown. `models catalog` explicitly contacts the Ollama registry for manifests
and actual sizes. Its default JSON output remains a model array;
`models catalog --snapshot` wraps the same entries in `models` alongside the
point-in-time `hardware` reading used for their fit and first-try guidance.
Later `models status` readings may differ as free memory changes.
Fit estimates reserve runtime/context memory and never pool GPU memory with RAM.
On macOS, `vm_stat` free, inactive and speculative pages give a bounded
available-RAM estimate. Inactive pages may require paging to reclaim. The
estimate is not a reservation, and missing or invalid counters leave fit
unknown rather than creating a recommendation.
Catalog fit is a 4096-token cold-load estimate because a registry manifest does
not report the installed model's context ceiling. When current memory readings
and a manifest support it, one entry has `first_try_reason`: the largest likely
GPU fit, or the smallest CPU/offload fit if none fits a GPU. This is a
resource-based starting point, not a model-quality, speed, compatibility, or
context-limit measurement. Missing readings or insufficient memory produce no
first-try suggestion; an entry whose manifest lookup failed cannot be selected
as the first try, though another valid entry can be. Downloading remains
explicit; calibrate the installed model before selecting it.
Before managed setup, catalog entries with a usable manifest may include
`pre_setup_storage`. The shared engine adds runtime setup headroom, the model's
manifest bytes and the same pull reserve used by live admission. It reports
the chosen root, free bytes and exact shortfall. Browse before setup and pick
another empty folder with `models storage PATH` if the intended model is short;
the storage choice becomes fixed once used. A missing, replaced, used or
unmeasurable folder receives no plan. The plan is conservative and
point-in-time, not a reservation or an install guarantee.
For installed models, `models status` reads `/api/show` metadata and current
memory to report a bounded automatic context choice, or no choice when those
observations are missing. It labels the context used by each cold-load fit;
`models status 8192` previews an override. An existing valid profile's fit uses
its calibrated context by default. An exact loaded model can have separate
resident-model admission even when a fresh cold load would be refused.
If a status-only metadata read times out, an otherwise valid saved profile
remains visible with a context warning. Select and goal execution fetch current
metadata and validate the calibrated context before inference.

`models setup` downloads a pinned, SHA-256 checked portable Ollama runtime on
Windows x64. Model status reports `managed_runtime_supported` so clients can
offer setup only where the shared engine implements it. Setup needs approximately
1.5 GB of download and 6 GB of free disk. `models status` reports the exact
current install reserve as `managed_storage.runtime_setup_min_free_bytes`
alongside measured `available_bytes` and the saved runtime-install marker, so
Desktop can flag a shortfall before a new or unfinished download. The backend
rechecks free space when setup starts and may restart an already verified
installation without another download.
Before setup, `phonton models storage` shows the planned runtime root, its
`models/` folder, the coding-run evidence folder, and free space on that volume. `phonton models storage PATH`
chooses an existing empty dedicated folder on a local Windows drive; Desktop
offers a folder picker for the same setting. The saved choice holds the runtime
archive, verified executable, logs, launch receipt, model weights, and new
coding-run snapshots, check copies, and receipts. Previously saved runs stay
at their original state-adjacent location and can still be reopened. Phonton
will not switch paths after files appear in the old managed folder, including
an interrupted download or stale launch receipt, and will not write into an
occupied target folder. Inspect or recover that folder instead of expecting a
silent migration. A chosen path is a setup plan, not proof that a responding
external Ollama uses that model store.
An unused chosen folder can be reselected if it disappears or its identity
changes. A fresh setup checks free space before creating the runtime folder or
its lock file. A failed setup that leaves only an idle lock does not claim the
folder; partial downloads and other files still prevent switching. Detecting
an external loopback service does not claim an occupied folder as Phonton-owned.
Once a managed runtime is installed or a coding run starts, the state records
that use and requires the old drive to be reconnected for inspection rather
than treating a missing folder as empty. Windows volume and directory IDs
distinguish a reconnected folder from a different folder assigned the same
drive letter and path.
Choosing a folder upgrades the local state schema so an older engine refuses
that state rather than silently dropping the saved location on write.
It creates no PATH entry or startup service. Other platforms currently require
an existing Ollama installation. Phonton-started managed inference is bound to
127.0.0.1:11434, with cloud features disabled and one loaded model at a time.
When an Ollama-compatible service already answers at that address, setup uses
it without installing or starting a runtime. A matching prior Phonton launch
reports `verified_previous_launch`; otherwise `managed_origin` is `unverified`.
Phonton does not infer an external service's process owner, model-store
location, cloud configuration, or resource limits. A newly started managed
runtime reports `started_by_phonton` only after its own child owns the loopback
listener and answers the version check. The runtime-root lease spans installation
and startup separately so competing Phonton setup calls cannot both start a
child; it does not exclude an unrelated process.
`loopback_only` means Phonton connects to a literal loopback endpoint.
`local_only` is true only while Phonton can verify its managed process and
model-store binding; it is false for external or unmatched services, which may
relay inference elsewhere.
If the default port is occupied but does not answer the version probe, setup
refuses it before downloading the runtime. Startup also rejects linked model
store and log paths beneath its managed root.
Managed setup requires a local Windows drive for its runtime root; UNC/device
roots are refused before installation.
Setup downloads into an owned staging directory, verifies the complete archive,
and publishes it without replacing an occupied path. It refuses reparse-point
archive paths and holds the archive against writes and renames while extracting.
It checks the extracted executable and all 82 runtime files against a digest
from the pinned archive, then publishes the complete extraction stage without
replacing an existing final directory. A later setup removes only interrupted
Phonton-owned download or extraction stages under the same runtime-root lease
and retries. Unknown stages, older `.zip.partial` files, and legacy incomplete
final directories remain untouched; inspect and move the latter out of the
managed root before retrying. An offline managed install is rechecked before
Phonton starts it; a separately running local Ollama daemon is identified by
its own version and model metadata, not this managed-file receipt.

`models install NAME` streams runtime-reported progress. Ctrl+C cancels the
Phonton operation; retained runtime layers may be reused by a later install.
An install result requires terminal success from the pull stream, a matching
installed inventory entry with digest and size, and local model metadata. The
reported digest and size come from the runtime; they are not an independent
audit of downloaded layer bytes.
When NAME omits a tag, install, calibrate, select, and remove resolve it to the
explicit `:latest` name reported by Ollama's installed inventory. The default
`library/` namespace is omitted in that inventory name. Operation results and
saved profiles use the resolved tag and still require the current digest.
On Windows x64, a Phonton-started managed runtime records its process creation
time, executable image, exact loopback port, canonical `models/blobs` path, and
volume and directory IDs for the runtime, model, and blob folders.
`models status` reports `model_store.status` as `verified_managed` only while
the same live process still owns that listener and the unredirected blob path
matches the receipt; it reports current `available_bytes` for that volume.
Before a managed pull, Phonton fetches a fresh official registry manifest for
the exact requested tag, sums unique config and layer blobs, and requires that
estimate plus a reserve of at least 1 GiB. It rechecks process ownership,
directory identity, and reserve as progress arrives. Tags, other writers, and disk quotas can
change during a pull, so this is admission evidence rather than a guarantee.
An external or unmatched service remains `unverified`: Phonton cannot infer
its model-store volume from Ollama's version or model list and does not claim a
disk check for it. Its own pull errors remain authoritative. If a prior managed
receipt exists but no longer matches, install stops before contacting the
runtime. If that service still answers on the managed port, setup now reports
the receipt failure rather than returning an existing-runtime success. Stop
the process using that port, reconnect the original managed folder if it
moved, and rerun setup; Phonton will not stop an unverified process or remove
the receipt automatically. An explicitly external service can instead use a
different configured loopback endpoint. A managed pull
whose manifest cannot be verified fails before download; retry after the
registry is reachable or use an explicitly configured external runtime.
`models status` includes `model_store.goal_run_blocked`: a managed-process,
listener, storage-identity or free-space observation error blocks coding goals
even with unverified-runtime consent. A missing prior launch receipt still
requires setup recovery before downloads, but an answering service can be used
for a goal only with explicit unverified-runtime consent. Goal admission
rechecks identity, so a status snapshot is never permission by itself.
Run evidence written to a chosen folder through an external service does not
by itself mean a managed runtime was installed there. A separate persisted
marker records managed installation; older installed runtime files also keep
a missing launch receipt in the recovery path.
That marker upgrades local model settings to schema 3 so an older engine cannot
silently rewrite it away. If a legacy installation lost both its launch
receipt and its runtime folder before this marker existed, provenance cannot
be recovered from the remaining settings alone.
CLI display coalesces ordinary updates to four per second. Stage changes, layer
completion and backwards byte counts are shown immediately. Unknown counts stay
unknown; the display never invents a percentage or smooths actual bytes.
`models calibrate NAME [CONTEXT]` measures two existing-file editing protocols,
new-file output in each edit transport that passes, and a tool-call format
against tiny fixed fixtures. It prefers an edit transport that also passes
creation, while retaining an edit-only transport for existing-file goals if
neither creation probe passes. It records actual runtime token counters when provided.
Without `CONTEXT`, Phonton chooses the largest estimated GPU context that fits
the installed model's reported limit, or a CPU/offload context if none fits the
GPU. Missing model limits or memory readings produce no automatic choice.
An explicit 2048–32768 token override remains available, but calibration
refuses pressure or a value above the reported model limit before inference.
When a cold load does not fit, calibration can use an already loaded exact
model at a sufficient context if at least 1.5 GiB host RAM is free and Ollama
reports at least 30 seconds before unload. Automatic context also requires the
model's reported limit. RAM and the same model identity and allocation are
rechecked before every probe; residency is a point-in-time observation, not a
reservation. If the model unloads or pressure rises, remaining probes stop.
The creation probe checks exact format and fixture text without executing model
code; none of these probes establishes general coding quality.
The small probe output caps do not set the ceiling for a coding goal. The
saved profile allows up to one quarter of its calibrated context for one
complete edit or new-file response, then the shared goal budget can reduce
that reservation. The worker keeps half the remaining token budget for a
later permitted model call. This is a context-based allowance, not measured
model generation capacity. A truncated runtime response is still rejected.
Previously saved profiles retain their smaller ceiling until recalibrated.
If neither edit format passes, calibration still saves and prints the raw probe
JSON, but `models calibrate` exits with code 2. `models status` keeps `profile`
null and exposes the same-endpoint attempt as `calibration_evidence` with a
`profile_error`. The Desktop shows that diagnostic evidence and keeps Select
disabled. A stale digest, runtime or model context can also leave saved
evidence visible; only a current passing profile is selectable.
`models status` also exposes `calibration_attempt` separately when calibration
started but did not finish. It saves each completed probe, with a capped output
excerpt, even if a later probe is stopped by memory pressure or cancellation.
This record remains visible while the runtime is offline and never qualifies
for selection. Its starting digest is not a verified identity for every probe.
A retry starts new probes; a previous passing profile is kept
until a full recalibration completes.

`models select NAME` requires a passing edit probe matching the installed digest,
runtime version, and endpoint. Changed weights or runtime versions require
calibration again. `models remove NAME` refuses to remove the selected model.

State is inspectable in `local-models.json` alongside the normal Phonton config.
`PHONTON_LOCAL_STATE` selects an isolated state location for tests or portable use.
It also keeps the managed runtime beside that isolated state file, overriding
the saved normal-use storage choice so tests do not touch the selected store.
For coding goals, this path must be absolute and outside the repository so
attempt records and lease files cannot change the source tree before preflight.
On Windows, a coding run checks free space in its evidence folder before making
repository copies. The minimum estimate covers the captured source once for
baseline and baseline checks, once per allowed candidate, plus limited check
headroom. Build outputs and other writers are not bounded by this estimate;
choose a roomier folder before setup for repositories with large checks.
Advanced users can run `phonton models endpoint http://127.0.0.1:11435` to use
an existing Ollama runtime on a different loopback port. The command accepts a
stopped runtime and saves only a validated local origin; `models status` then
reports whether it actually responds. Changing origins clears the selected
model. Measured profiles remain inspectable but only validate against their
original endpoint, installed digest and runtime version. Use `phonton models
endpoint` to inspect the current origin, or set the default
`http://127.0.0.1:11434` again before managed setup. Cloud aliases,
non-loopback destinations, redirects and inference proxies are refused. Model
operations and local goals share a cross-process resource lease, so the change
is refused while either owns local model state.

## Run a coding goal

Start with a goal and a repository. Phonton inspects local source, proposes files
and conventional checks, and shows a plan before inference:

```powershell
phonton goal --local --plan "Fix parsePort validation" --repo C:/projects/example
phonton goal --local "Fix parsePort validation" --repo C:/projects/example
```

To execute the exact reviewed scope and selected model in a later CLI invocation,
save the preview outside the repository as UTF-8 JSON, inspect it, and pin its
file hash before passing it back (PowerShell 7):

```powershell
$planPath = Join-Path $env:TEMP 'phonton-reviewed-plan.json'
phonton goal --local --plan "Fix parsePort validation" --repo C:/projects/example | Set-Content -Encoding utf8NoBOM $planPath
Get-Content -LiteralPath $planPath
$planSha = (Get-FileHash -LiteralPath $planPath -Algorithm SHA256).Hash
phonton goal --local --reviewed-plan $planPath --sha256 $planSha --yes --allow-host-checks
```

Use the host-check flag only after reviewing the plan's command list. The saved
file cannot authorize host execution on its own. If its bytes change after the
hash is recorded, the reviewed run refuses. A changed source snapshot,
model selection, or calibration makes the reviewed run refuse; make a new plan.
An external or unmatched loopback runtime requires separate
`--allow-unverified-runtime` consent on the run invocation. A request file or
reviewed plan cannot carry that permission into a later CLI run. The run receipt
labels the runtime origin and notes that process/listener checks around each
chat are point-in-time observations, not authenticated transport.

`--plan` reads files and local model settings without starting inference or
project checks. It checks the selected model's installed digest, runtime version,
edit calibration, and current context metadata through the loopback runtime.
It returns the preview as JSON, including the model and full calibration-profile
fingerprint only when those checks pass. A stale or unavailable selection becomes
null with a warning; the source scope remains reviewable. The second form asks you to accept its scope in an
interactive terminal. `--yes` accepts a newly generated plan in that invocation;
it does **not** bind an earlier `--plan` output or authorize project execution. Add
`--allow-host-checks` only when you approve the displayed or inferred commands on
your machine. Without it, returned candidates stay explicitly unverified.

Use `--files src/a.js,src/b.js` to override discovery and `--check
'["node","test_sum.js"]'` to supply a command. Discovery uses path/word/symbol
matches, not a model-generated semantic implementation plan. Review its relevance.
Inferred checks are proposals, never proof that they cover the goal. Source hashes
bind an accepted preview to its files; changes require a fresh review.
The selected model and calibrated profile are bound too. Run rechecks their live
runtime identity and context before saving an attempt. A changed digest, runtime
version, selected profile or context limit refuses the run; review again after
recalibration. The worker checks once more before sending repository context.
Python planning proposes pytest for pytest configuration or function/class-style
tests, and unittest discovery for conventional `unittest.TestCase` files.
Pytest must already be available on the approved host. A successful pytest
exit is **Not run** unless its final summary reports at least one passed test;
collection-only, all-skipped and missing-summary output cannot verify a change.
A missing pytest module is **Unavailable**, so search does not treat an absent
test runner as a candidate bug.
Wrapped pytest commands such as `uv run pytest` keep their output but are
diagnostic-only on exit 0: the wrapper can change the runner or flags. Select
the environment's Python executable directly for verification.
If a repository-local `pytest` or `unittest` module could replace an explicit
`python -m` runner, the check is refused before execution. Remove the shadow
or choose a different reviewable check; a printed runner summary alone cannot
establish that the real runner executed.
Attached inline-code flags and Python/Node interpreter command wrappers are
refused as checks. Use a direct executable with reviewable script or test paths.

To create one file, name its exact repository-relative path with `--new-file`:

```powershell
phonton goal --local --plan "Add an inclusive range helper" --repo C:/projects/example --new-file src/range.mjs --check '["node","test.mjs"]'
phonton goal --local "Add an inclusive range helper" --repo C:/projects/example --new-file src/range.mjs --check '["node","test.mjs"]' --allow-host-checks
```

`--files` is optional read-only source context for a creation goal. When omitted,
the plan searches existing source for goal terms and parsed symbols, then shows
up to eight matching READ paths with hashes for review. If nothing matches, the
plan says the creation prompt has no repository excerpt; name relevant files
explicitly when the new file must follow an existing API or style. Discovery
never makes an existing file editable. To create a module and edit an existing
caller in one run, include that caller in `--files` and name it in
`--edit-existing`:

```powershell
phonton goal --local --plan "Add a range helper and wire the caller" --repo C:/projects/example --new-file src/range.mjs --files src/caller.mjs --edit-existing src/caller.mjs --check '["node","test.mjs"]'
```

Up to three existing files can be explicitly editable in a mixed creation;
other selected files remain read-only. The first model call creates only the
named file and is saved as an incomplete, unselectable candidate. A second
budgeted call edits the reviewed existing scope against those created bytes.
Only the combined candidate runs checks or becomes review ready. If the second
step is interrupted or its budget expires, the creation remains inspectable
but cannot be applied. A passing combined result uses a separate guarded Apply
journal that can roll forward exact partial states; it is sequential, not
atomic across files.
If a checked candidate fails, a further budgeted repair can edit the created
file or the reviewed existing paths from that exact isolated candidate. It
cannot edit other read-only context or test definitions, and the full combined
snapshot must pass checks again. Creation-only goals can likewise repair their
failed new file without treating its already-present parent copy as absent.
Repair keeps that created file in the model's exact source context, or stops
before inference if the calibrated context cannot hold it. A verifier-named
scoped source anchor survives feedback trimming; rejected model output cannot
create one. Candidate changes that introduce protected test definitions in
any edited source are rejected before project checks run.

The parent directory must already exist and contain no links. The target must
be absent, untracked and not ignored by Git. The plan
records that absence, and the candidate diff uses `--- /dev/null`. A model may
propose content only for the named target during creation, then only reviewed
existing paths during the second step; checks run against the complete isolated
candidate. Creating a file does not permit new test definitions in that file.
The creation-format probe is separate from the ordinary edit probe. Older
profiles report creation as not measured; a failed or unavailable creation probe
is shown as such but does not prevent a verified, reviewable goal. Only a
candidate that passes the selected checks becomes review ready. For constrained
JSON creation, Phonton
adds a missing final newline before materializing the candidate; the canonical
diff and hash describe the normalized file, while the raw model output remains
inspectable in the receipt.

For reproducible runs or custom budgets, use an explicit request:

Create `request.json` with an explicit repository, source scope and checks:

```json
{
  "goal": "Fix the boundary condition in sumInclusive.",
  "repository": "C:/projects/example",
  "files": ["src/sum.js"],
  "checks": [{"program": "node", "args": ["test_sum.js"]}],
  "preparation": null,
  "approve_host_execution": false,
  "budget": {
    "generations": 4,
    "check_runs": 16,
    "wall_seconds": 600,
    "generated_tokens": 4096
  }
}
```

Run `phonton goal --local --request request.json`. A saved
`approve_host_execution: true` is not approval for a later invocation: the CLI
refuses that request unless you also pass `--allow-host-checks` after reviewing
its repository and commands. Passing that flag explicitly approves host setup
and checks for this invocation, even if the saved field is false. The selected local model is
used directly, with its measured context and edit protocol. There is no template
implementation, tier escalation to another model, or remote fallback.

The runner snapshots the current Git working tree and creates separate candidates
without applying or staging changes in your repository. It validates exact old-side
text, canonical hunk counts and scope before running checks. Baseline and candidate
checks use the same captured command definition. Changed source or added source
dependencies during checks invalidate the candidate identity.
The default check budget reserves 16 command launches across the baseline and
candidate passes. A host-approved request needs enough slots for one complete
baseline and one complete candidate pass: twice the total of selected checks
and any offline dependency-preparation command. A smaller explicit budget is
shown as a plan warning and refused before host checks run.
When a selected model uses SearchReplace, each model response edits one path.
Reviewed plans with an implicit budget reserve at least one call per editable
path and scale check/output reserves up to the worker caps. The displayed plan
warns about caps and the lack of repair slack; it does not promise that every
path will be edited. Explicit budgets submitted for plan review are preserved
and warned about when they cannot cover even a straight sequence.
The original Git index is captured separately before checks. Its resolved path
and exact byte hash are compared before each generation and at final review.
Changes or unavailable observations stop the run and clear candidate selection;
Phonton does not automatically restore the index. Receipts name the observed
stage. This detects differences at those points, not transient changes between
observations, and is not containment. Older receipts have no index evidence.

A repair can build on a failed candidate whose identity still matches its checks.
Its receipt names the parent and why the controller chose repair. The displayed
diff contains all changes from the original baseline, including inherited edits.
Unchanged failed-check identity and normalized output after a repair start a
fresh baseline approach unless another selected check newly passes. Losing a
pass without either signal also restarts. If a check gains a pass, or a
different selected check now fails with the same message, the controller keeps
that changed candidate for another repair;
repeated candidate identities stop further inference. Feedback includes the
rejected edit and bounded check output. The most recent failed checks precede
copied edits so they survive tight context limits; uncheckable proposals retain
that evidence with its candidate identity (or baseline evidence if none ran).
Two identical rejected raw outputs that never formed a checkable candidate also
stop inference instead of spending the remaining generations on the same reply.
Different diagnostics are a search signal,
not proof that code quality improved. Every branch uses the same run budget.
Feedback is shortened when necessary to admit source; the exact transmitted
feedback and omission flag are recorded, while full rejected outputs remain saved.

The existing-file runner supports 1–8 explicitly scoped, existing,
newline-terminated text files. A creation run supports one explicit new text
file and up to eight existing read-only context files. Retrieval ranks parsed symbols and lexical windows
inside that scope, then admits exact source excerpts within the calibrated context.
JavaScript and TypeScript module extensions (`.mjs`, `.cjs`, `.mts`, `.cts`) use
parsed function spans and are included in read-only inferred file discovery.
A root `go.mod` plus selected Go source proposes
`go test -json -count=1 ./...`; preview does not run it or grant host
execution. Go `_test.go` files and `go.mod`, `go.sum`,
`go.work`, and `go.work.sum` are excluded from model edit scope, while their
original bytes remain part of the captured repository identity. Automatic
checks follow the editable targets: creation uses the new file rather than
read-only context, mixed or unknown source families get no inferred check,
and nested Go modules or Node packages require an explicit package-specific
command. Rust files propose `cargo test --locked --package <name>` for their
nearest readable package manifest, with `--manifest-path` for a nested package.
This bounds the inferred check to the edited package; select a broader workspace
check explicitly when dependent packages matter. A virtual or unreadable Cargo
manifest yields no inferred check. The saved receipt repeats the limit for
each package-scoped Cargo command and asks reviewers to inspect any other
selected checks for dependent coverage, including requests that bypassed plan
preview. Conventional Node
test filenames and files under `test/` are protected from model edits even if
they contain only top-level assertions. Node fallback checks include only test
files owned by the selected package; a suite with more than 32 matching files
needs an explicit check instead of a silently truncated subset. A named symbol
in the latest failing-check evidence is
ranked ahead of other scoped symbols in the next prompt and edit choices; this
is a search heuristic, not proof that the named function caused the failure.
The conservative bound includes prompt bytes, edit-schema bytes, framing and the
output reserve; it is not measured input-token usage. Search/replace chooses one
unique original line or compact complete parsed symbol per candidate; its
replacement can contain multiple lines. Search-choice block bytes retain a
1 KiB cap, and their schema must fit the context budget. A compact
symbol is offered as a whole block, without overlapping header/body line choices.
Its trailing line separator stays outside the editable span.
Otherwise the runner retains line choices. An unchanged proposal is rejected.
Unproductive line choices are omitted on retry; complete function targets remain
available so a repair cannot degrade into a partial-header replacement.
Snapshots are limited to 1 GiB / 20,000 files and omit sensitive paths and
build directories. A full bounded snapshot includes unrelated tracked assets
so checks run against the same repository content; identity hashing streams
file bytes instead of loading a large asset into memory. Windows run admission
estimates the space for baseline, check, and candidate copies before inference,
but it cannot reserve space against other writers or bound check output.
Snapshot copies proceed in cancellable chunks, including within a large file.
Test files and package/build manifests are protected from
model edits. The local runner also refuses scoped check scripts, conventional
test configuration, and source files with embedded tests it cannot separate.
For Rust source with a top-level, line-starting test attribute, production code
before the first real test item is editable. The complete test tail must remain
byte-for-byte unchanged, and its parsed test items and crate attributes must
also match the original. New test attributes anywhere in the editable prefix,
including inside functions and macro invocations, are rejected. Macro and import
declarations and module scope are held fixed so an edit cannot shadow the tests;
function bodies inside earlier inline modules can still change. Ambiguous
inline test layouts remain whole-file protected. Direct check script
paths are matched with common extensionless module forms; npm-family script
entry points and their lifecycle/chained scripts are inspected from the captured
package manifest; unrelated scripts do not block ordinary source edits. Package
dispatch that cannot be resolved statically (including nested workspace flags or
Corepack) is refused. Shell wrappers and inline interpreter code are not
supported as guarded checks. Check commands
must refer to project files relative to the candidate; rooted arguments, file
URLs, and relative paths that climb outside the candidate are refused, including
flag values, space-separated path lists and Windows separators. An in-candidate
normalization such as `tests/../tests` remains valid. This is a
conservative static guard, not a complete dependency analysis: custom imports
or dynamic test configuration may still influence a passing command. Review
the exact scope and check definitions before accepting a plan or applying a diff.
Pytest `--pyargs` is refused; unittest module selectors and discovery roots
must resolve to candidate-local files or directories.
Direct pytest checks also inspect captured INI/TOML configuration, including
an explicit `-c` file and nested config along selected test paths, for external
`testpaths`, `pythonpath`, and `addopts` paths. Command-line `-o` overrides
receive the same check, including quoted values and `addopts --pyargs`.
Unsafe values refuse the run before any check or inference, since a genuine
pytest pass against the original repository cannot verify candidate bytes.
Configured `addopts --pyargs` is also refused because it can select an
installed package.
Go package targets must be `.` or candidate-relative `./` paths; a bare
standard-library or remote package name cannot verify candidate code.
Go `-exec`, `-toolexec`, and `-overlay` are refused because they can replace the
test binary, toolchain execution, or candidate source bytes.
For a supported root npm script that declares registry dependencies, the
read-only plan proposes a direct Node TAP check and adds an exact `preparation`
command before verification: `npm ci --offline
--ignore-scripts --no-audit --no-fund --no-update-notifier`. It requires a
captured root `package.json` and v2/v3 `package-lock.json`, at most 512 locked
package entries, and registry tarballs with integrity hashes. Workspaces,
local/Git/URL dependencies, linked paths, unsupported lockfiles and missing
locks are not prepared automatically. An inferred Node check is withheld in
those cases; an explicitly selected check remains visible with a warning.
Preparation runs only in each separate baseline/candidate check copy after
explicit host approval, using npm's existing cache without fetching packages.
An empty cache or nonzero install result is **Unavailable**, not a failed coding
task. The test is **Not run** and no inference begins when
baseline preparation fails after host approval. Preparation and verification
each consume a check-budget slot and have separate command/output/status journal
entries. A passing install is never counted as task verification; only the
following selected check can verify a candidate. This is still host execution,
not filesystem containment, and npm's cache is outside the copied repository.
If the install succeeds but a native package requires a disabled lifecycle
script, the test can fail; inspect that output before treating it as a code bug.
Automatic broad retrieval, other dependency installation, multiple new files
and broad repository tasks are not yet supported by this path.

## Execution and evidence

Cold model admission estimates weights, context and a host reserve. Before each
strategy or edit request, the runner refreshes runtime version, installed model
digest and size, observes current host memory, and re-estimates fit at the
calibrated context. If cold loading no longer fits, Ollama must report the exact
calibrated digest already loaded at that context, at least 1.5 GiB available
host RAM, and at least 30 seconds of runtime-reported load time remaining. A
resident model admitted at the start must keep its allocation identity.
Residency is checked again immediately before chat. Failed admission stops
before reserving that model call; the receipt records the reason. These
point-in-time checks do not reserve memory, guarantee a future load, or permit
a different model or context under the smaller reserve.

Required filesystem/network isolation is currently unavailable. Without host
approval, no project checks execute and their state is **Unavailable**. Setting
`approve_host_execution` to `true` explicitly permits the listed commands to
execute project code on your machine. This is not a sandbox. On Windows, each
approved check starts suspended, joins a Job Object before running, and closes
that job when the direct command exits or the check is cancelled or times out.
This cleans up descendants; it does not contain filesystem or network effects.
The OS exit status is observed, but candidate code can terminate an in-process
runner or forge its output (including plausible pytest/unittest summaries).
A `Passed` check means the approved command reported success under the current
parser; it is not independent attestation that each test executed. Inspect the
diff, check scope, and captured logs before applying a candidate.
On other platforms, descendant cleanup is not established. Commands have
bounded time and output; they do not inherit provider secrets from Phonton's
process environment.
Legacy Cargo/npm/browser verification now uses this same executor. Its default
entry points require isolation and stop as unavailable; legacy automatic goals
do not infer host permission from `--yes` or opening a workspace. Their Rust API
has a separate caller-owned `VerificationExecution::HostApproved` policy.
Typed `Unavailable` and `NotRun` outcomes stop legacy orchestration without
retrying at a more expensive model tier and remain visible in the handoff.
Browser smoke checks use temporary helpers outside candidate source, reject
off-origin page requests and report missing browser dependencies as unavailable.
They check rendering errors only, not product correctness.
The original Git-visible tracked and untracked inventory and captured source bytes
are checked after baseline verification, before each generation and at final review.
Changes or an unavailable observation invalidate selection. Candidate bytes are
also checked again before final selection. Ignored files and external host effects
are outside this observation. Host commands still have host access, so detection
does not provide containment or prevent external effects.

Verification and legacy diff application share exact old-side/count/offset
validation. A summarized old side, omitted `export`, or stale source is rejected.
Insertion hunks preserve existing content. This does not make legacy checkpoint
rollback or multi-file write failures transactional; the local candidate path
still keeps source application separate.

No checks means **Not run**, not Passed. An approved command that exits zero
without using a supported test runner or candidate-local script is also
**Not run**. Version, status and build-only
commands stay visible with their exit codes but cannot alone make a candidate
review-ready. Direct Python and Node scripts inside the candidate can count as
user-selected checks; their success is still only process-reported evidence,
so review the script and diff before Apply.
A `python -m unittest` check that reports
zero collected tests, only skips or expected failures, or a successful exit
without a runner summary is also **Not run**; its actual exit code remains
recorded. At least one ordinary passing test is needed for a reported pass. The
default runner does not count successful subtests separately, so a mixed
subtest suite with only skip counts may also
remain **Not run** until a check with clearer evidence is selected.
Runner text is process-reported evidence, not an authenticated test attestation:
project code can print a forged summary or exit early. Review check definitions
and candidate code before relying on a passing receipt.
A successful Go check needs uncached JSON output with a named passing leaf test
and completed package results. Plain `ok` summaries look the same for passing
and skipped-only suites, so explicit plain `go test` checks are **Not run** even
when they exit 0; choose `go test -json -count=1` for a review-ready check.
Skipped-only tests and subtests, cached output, packages with no tests or no
selected tests, and `go test -list`, `-c` or `-n` are likewise **Not run**. Go's
leading `-C .` directory flag does not bypass these evidence requirements;
another `-C` target remains **Not run** because it may test another tree.
Command output and exit code remain recorded. Parent test passes are not counted
when all of their reported subtests skip; a parent assertion outside those
subtests may therefore require a clearer separate test to prove execution.
A successful `cargo test --no-run` or `cargo test -- --list` is also **Not run**.
When every reported Cargo test suite has zero passed or failed tests, the check
is **Not run** even if compilation succeeds. An exit-zero Cargo test with no
parseable completed-test summary is also **Not run**; missing or truncated
output cannot establish that a test executed. Output and exit code remain visible.
A successful direct `node --test` check is **Not run** unless it explicitly
requests the TAP reporter and reports at least one completed named test. The
root npm script is parsed to propose that direct command, but `npm test` itself
is diagnostic only: npm can change the script shell and prepend project binaries
to `PATH`. Spec output forwards text printed by
test code, so it cannot prove a named test ran. Skipped-only runs, empty test
files, unmatched name filters, and other reporters cannot verify a
candidate. Root npm scripts are classified when a Node test invocation is
the only executable in a literal script or bounded `npm run` delegation.
Scripts with pre/post hooks, shell chains, glob expansion or shell interpolation
remain opaque. Node preloads in the approved command are outside this evidence
rule; the check subprocess does not inherit host `NODE_OPTIONS`. Printed
TAP text alone does not prove that an opaque script ran tests. An inferred
opaque npm script is withheld; an explicit `npm test` retains its exit and output
but records **Not run** after exit 0. Other runners still need
separate evidence rules.
Bare top-level assertions in a test file do run under Node, but they cannot be
distinguished from an empty file-wrapper success in this output. Register a
named `node:test` case for a review-ready result from this guard.
A successful run means the selected checks passed on the identified candidate;
it is not proof of correctness.
CLI exit 0 means review-ready with passing checks, 3 means an unverified candidate
is available, and 1 means no accepted candidate or an execution error.

Each run saves a JSON receipt, separate baseline/candidate directories, raw model
outputs and canonical diffs under the local state directory's `runs/` folder.
Each generation also records its exact source excerpts, line ranges, source hashes,
selection reasons and context bounds. Retrieval uses the captured local source and
existing parsers; it does not download an embedding model or call a remote service.
After an unchanged repair or repeated candidate, a baseline restart uses a separate
bounded model call to propose a strategy anchored to an exact reviewed source span.
The receipt keeps its raw reply, selected anchor, claimed difference, context and
token usage. A missing or repeated strategy stops that branch. The strategy
and following edit each reserve one of the same `generations` model-attempt
budget, even if admission later stops a chat request; the
strategy is a proposal, and only the resulting candidate bytes and fixed checks
can justify review. The edit's actual returned path and source anchor are
checked before materialization. An interrupted strategy remains pending evidence, not a
candidate.
Cancellation retains evidence. Reopening an unfinished receipt labels it
interrupted; it does not silently resume or mark it complete. Inspect the selected
candidate and its diff before applying it.
An attempt record is written before preflight. If resource pressure, changed
source or another preflight error prevents the first receipt, reopening the run
shows that exact refusal. A hard engine stop before the first receipt is labeled
interrupted before receipt; it does not imply checks or inference ran. Errors
after a receipt are distinguished from explicit cancellation.
Desktop stores a fresh run ID before asking the engine to start, so it can find
the attempt even if the start response is lost. A saved record that cannot be
read is a retryable recovery error, not evidence that the goal completed or
ended before a receipt. A confirmed start refusal with no saved evidence keeps
the reviewed plan editable.
Each check has a separate journal written before launch and after completion, so
an interrupted check remains counted against the shared budget. The wall-time
limit covers preparation, inference and final identity checks as well as commands.
Before strategy and edit inference, the worker rechecks the time remaining
after context assembly and model admission. An expired budget stops before
reserving another call; edit chat uses a fresh remainder at dispatch.

The new Desktop local composer uses these same core methods. It displays the
scope, model, permission choice and budget before execution, then shows candidate
diffs, check results, tokens and evidence limits on the same surface.
It requires the engine's separate `local_creation_schema: 1` capability before
presenting creation controls; an older local-run endpoint is labeled incompatible.

## Apply a reviewed candidate

After a run reaches **review ready**, inspect the selected diff and checks.
The CLI can reopen saved evidence without starting a model:

```powershell
phonton goal --local list
phonton goal --local show RUN_ID
```

`list` returns recent run IDs; `show` returns the saved receipt and the separate
Apply journal as JSON. If no terminal record exists, the CLI labels that
evidence provisional because another engine may still be working. `show`
rejects a receipt whose embedded run ID differs from the requested ID and
reports a damaged or mismatched Apply journal as an error, not valid status.

Desktop's **Apply selected changes** action and the equivalent CLI command
replace the complete changed set of reviewed, existing scoped source files, or
publish the one explicitly reviewed new file:

```powershell
phonton goal --local apply RUN_ID --yes
phonton goal --local rollback RUN_ID --yes  # existing edits; witnessed single creation on Windows
```

Apply requires a selected candidate with passing executed checks and passing
final Git-index integrity evidence. It rechecks the saved baseline, full candidate,
diff, current repository source and Git index before writing. A changed or
unreadable source/index refuses the operation. Schema-2 `apply.json` records every
changed path and hash, per-file backup, and exact same-parent temporary path
before replacement. Backups and creation anchors request file and directory
sync before the prepared journal is saved. Journal publication also requests a
directory sync; a sync failure stops Apply before project mutation. This
narrows a power-loss window. Before saving `applied` or `rolled_back`, Apply
requests sync of each changed project directory. If that later sync fails,
the nonterminal journal remains available for recovery. Baseline/candidate
evidence and the full operation are not proven crash-durable.
Files are replaced sequentially, so interruption can leave a partial batch;
retry classifies each file as original or candidate and continues only when all
other source, temporary and index evidence still matches. Unknown edits or a
missing/linked file require manual recovery. Existing schema-1 one-file journals
remain readable. Permissions and attributes are retained where supported.
On Windows, Apply holds the parent directories of every existing changed file
without delete sharing through replacement, including mixed new-file goals.
This blocks parent-directory relocation during a path-based write. File-level
concurrent edits and non-Windows parent swaps still need revalidation; Apply is
not a filesystem sandbox or an atomic multi-file transaction.
Phonton does not stage, commit or run new checks during apply. Repeating an
already completed apply confirms candidate bytes without another write.
For schema-1 and schema-2 existing-file journals, Desktop **Restore original
files** and the CLI rollback command recheck the selected candidate, every
original-byte backup, current source, journaled temporaries and original Git
index. They restore only exact candidate bytes, skip files already at the
captured baseline, and refuse other observed edits before starting. The
`rollback_prepared` journal supports explicit resume after interruption; a
`rolled_back` journal verifies a repeat without writing. A multi-file restore
is sequential, not atomic. On Windows, an applied schema-3 single creation can
also be rolled back. Phonton verifies the retained hard-link identity witness,
the created file's exact identity and bytes, unchanged existing source and Git
index, then deletes through the validated file handle. The journal records a
deletion attempt before mutation. An absent target can finish recovery only
after that marker; a present target after the marker needs manual review
because its link history is ambiguous. A byte-identical replacement is refused.
On Windows, applied schema-4 mixed create/edit journals restore original-byte
backups for the existing edits before disposing the witnessed new file.
Interrupted source restore can resume only from exact saved states. Prepared
mixed Apply must finish Apply before rollback. Old byte-only creation journals
and non-Windows created-file rollback remain manual. A concurrent
editor can still change a source between the last byte check and path-based
replacement; inspect the final working tree.
Legacy schema-1 Apply used an unrecorded random forward temporary. If it was
interrupted before replacement and left that temporary in the repository,
rollback refuses the extra inventory entry; identify and inspect it manually
before retrying. Phonton does not guess which unrecorded file to remove.
For creation, schema-3 `apply.json` binds the one new path and exact candidate
hash and a retained same-volume hard-link identity witness. Apply refuses an
occupied or newly tracked path, checks unchanged existing source and Git index,
journals a same-parent temporary, then publishes without replacing any file.
An interrupted apply resumes only when the target's file identity and bytes
match its witness; an absent target after attempted publication, unknown target,
or changed temporary requires manual recovery.
Host checks still had no filesystem or network containment, and selected checks
do not prove the change correct. Review the resulting working tree yourself.
