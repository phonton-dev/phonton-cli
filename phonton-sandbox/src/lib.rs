//! Explicit command execution authority and tool guarding.
//!
//! Canonical home for [`ToolCall`], [`ExecutionGuard`], and [`GuardDecision`]
//! (previously duplicated in `phonton-worker`, which now re-exports them).
//! The [`Sandbox`] refuses commands without explicit host execution approval
//! while filesystem/network containment is unavailable. Windows Job Objects
//! supervise process cleanup; they are not a security boundary.
//! On every platform, guard decisions are authoritative — `Block` is never
//! overridden.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::process::Command;

#[cfg(target_os = "windows")]
struct OwnedWinHandle(windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
unsafe impl Send for OwnedWinHandle {}

#[cfg(target_os = "windows")]
unsafe impl Sync for OwnedWinHandle {}

#[cfg(target_os = "windows")]
impl Drop for OwnedWinHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(target_os = "windows")]
fn resume_suspended_child(pid: u32) -> Result<()> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // CREATE_SUSPENDED leaves only the initial thread. Find it after the job
    // assignment, so project code cannot create a child outside the job.
    let snapshot = OwnedWinHandle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)? });
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    unsafe { Thread32First(snapshot.0, &mut entry)? };
    loop {
        if entry.th32OwnerProcessID == pid {
            let thread = OwnedWinHandle(unsafe {
                OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)?
            });
            let previous = unsafe { ResumeThread(thread.0) };
            if previous != 1 {
                return Err(anyhow!(
                    "Could not resume supervised process: initial suspend count was {previous}"
                ));
            }
            return Ok(());
        }
        if unsafe { Thread32Next(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    Err(anyhow!(
        "Could not find initial thread for supervised process"
    ))
}

// ---------------------------------------------------------------------------
// Tool calls and the execution guard
// ---------------------------------------------------------------------------

/// A tool invocation a worker would like to perform.
///
/// Workers translate model output into one of these variants and submit
/// every call through [`ExecutionGuard::evaluate`] before executing.
#[derive(Debug, Clone)]
pub enum ToolCall {
    /// Read a file from disk.
    Read {
        /// Target path.
        path: PathBuf,
    },
    /// Write or patch a file.
    Write {
        /// Target path.
        path: PathBuf,
        /// File contents to write.
        content: String,
    },
    /// Run a known well-formed binary (`cargo`, `git`, `npm`, ...).
    Run {
        /// Program name, *not* a shell line. Use [`ToolCall::Bash`] for
        /// free-form input.
        program: String,
        /// Arguments. Inspected by the guard for path-targeting `rm`/`mv`/`cp`.
        args: Vec<String>,
    },
    /// Execute an arbitrary shell command.
    Bash {
        /// The full command line as the model proposed it.
        command: String,
    },
    /// Make an outbound network request.
    Network {
        /// Destination URL.
        url: String,
    },
}

/// Result of evaluating a [`ToolCall`] against the permission policy.
///
/// `Allow` runs immediately. `Approve` halts the worker until the user
/// confirms via the orchestrator. `Block` is terminal — the worker must
/// fail the subtask rather than execute it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Permitted with no prompt.
    Allow,
    /// Requires explicit user approval each time.
    Approve {
        /// Why the user is being asked.
        reason: String,
    },
    /// Hard refusal — never executable, no override.
    Block {
        /// Why the call was refused.
        reason: String,
    },
}

/// Permission filter for outgoing tool calls.
///
/// Holds the project root used to discriminate "inside" from "outside" the
/// workspace.
#[derive(Debug, Clone)]
pub struct ExecutionGuard {
    project_root: PathBuf,
}

impl ExecutionGuard {
    /// Construct a guard scoped to `project_root`.
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }

    /// Project root this guard was constructed with.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Evaluate a tool call. The returned [`GuardDecision`] is the only
    /// signal the worker uses to decide whether to proceed.
    pub fn evaluate(&self, call: &ToolCall) -> GuardDecision {
        match call {
            ToolCall::Read { path } | ToolCall::Write { path, .. } => {
                if let Some(reason) = blocked_path(path) {
                    return GuardDecision::Block { reason };
                }
            }
            ToolCall::Run { .. } | ToolCall::Bash { .. } => {
                for arg in arg_iter(call) {
                    if looks_like_path(arg) {
                        if let Some(reason) = blocked_path(Path::new(arg)) {
                            return GuardDecision::Block { reason };
                        }
                    }
                }
            }
            ToolCall::Network { .. } => {}
        }

        match call {
            ToolCall::Read { path } => {
                if self.is_inside_root(path) {
                    GuardDecision::Allow
                } else {
                    GuardDecision::Approve {
                        reason: format!("read of {} is outside project root", path.display()),
                    }
                }
            }
            ToolCall::Write { path, .. } => {
                if self.is_inside_root(path) {
                    GuardDecision::Allow
                } else {
                    GuardDecision::Approve {
                        reason: format!("write to {} is outside project root", path.display()),
                    }
                }
            }
            ToolCall::Run { program, args } => {
                if !is_allowed_program(program) {
                    return GuardDecision::Approve {
                        reason: format!("program `{program}` is not on the allowlist"),
                    };
                }
                if is_destructive_program(program) {
                    for arg in args {
                        if looks_like_path(arg) && !self.is_inside_root(Path::new(arg)) {
                            return GuardDecision::Approve {
                                reason: format!(
                                    "destructive op {program} targets {arg} outside project root"
                                ),
                            };
                        }
                    }
                }
                GuardDecision::Allow
            }
            ToolCall::Bash { command } => GuardDecision::Approve {
                reason: format!("arbitrary bash requires approval: {command}"),
            },
            ToolCall::Network { url } => GuardDecision::Approve {
                reason: format!("network request to {url} requires approval"),
            },
        }
    }

    fn is_inside_root(&self, path: &Path) -> bool {
        if path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return false;
        }
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.project_root.join(path)
        };
        if !abs.starts_with(&self.project_root) {
            return false;
        }
        // Resolve every existing ancestor, including junctions, before granting
        // access to a file that might not exist yet.
        if let Ok(root) = std::fs::canonicalize(&self.project_root) {
            let mut existing = abs.as_path();
            while !existing.exists() {
                let Some(parent) = existing.parent() else {
                    return false;
                };
                existing = parent;
            }
            return std::fs::canonicalize(existing).is_ok_and(|p| p.starts_with(root));
        }
        false
    }
}

fn is_allowed_program(program: &str) -> bool {
    matches!(
        program,
        "cargo" | "rustc" | "git" | "npm" | "yarn" | "pip" | "python" | "python3" | "node"
    )
}

fn is_destructive_program(program: &str) -> bool {
    matches!(program, "rm" | "mv" | "cp")
}

fn arg_iter(call: &ToolCall) -> Vec<&str> {
    match call {
        ToolCall::Run { args, .. } => args.iter().map(String::as_str).collect(),
        ToolCall::Bash { command } => command.split_whitespace().collect(),
        _ => Vec::new(),
    }
}

fn looks_like_path(s: &str) -> bool {
    s.starts_with('/') || s.starts_with('~') || s.contains('\\') || s.starts_with("C:")
}

fn blocked_path(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let lower = s.to_ascii_lowercase();

    for needle in [
        "/.ssh/",
        "\\.ssh\\",
        "/.aws/",
        "\\.aws\\",
        "/.config/anthropic",
        "\\.config\\anthropic",
        "/.env",
        "\\.env",
    ] {
        if lower.contains(needle) {
            return Some(format!("blocked: sensitive path {s}"));
        }
    }
    if lower.ends_with("/.env") || lower.ends_with("\\.env") || lower == ".env" {
        return Some(format!("blocked: sensitive path {s}"));
    }

    for prefix in ["/etc/", "/usr/", "/bin/", "/sbin/", "/boot/"] {
        if lower.starts_with(prefix) {
            return Some(format!("blocked: system path {s}"));
        }
    }
    for prefix in [
        "c:\\windows",
        "c:\\program files",
        "c:/windows",
        "c:/program files",
    ] {
        if lower.starts_with(prefix) {
            return Some(format!("blocked: system path {s}"));
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// Fail-closed executor for [`ToolCall::Run`] and [`ToolCall::Bash`].
///
/// Pairs an [`ExecutionGuard`] with explicit host authority. Approved commands
/// run from `project_root` with a scrubbed environment, bounded output and a
/// deadline. This executor does not currently establish required containment.
pub struct Sandbox {
    guard: ExecutionGuard,
    task_id: String,
    host_execution_approved: bool,
}

impl Sandbox {
    /// Create a new sandbox bound to `project_root`. Typically the
    /// orchestrator's working directory.
    pub fn new(project_root: PathBuf, task_id: String) -> Self {
        Self {
            guard: ExecutionGuard::new(project_root),
            task_id,
            host_execution_approved: false,
        }
    }

    /// Access the wrapped guard — useful for orchestrator-side pre-flight
    /// approval decisions before dispatching a tool call.
    pub fn guard(&self) -> &ExecutionGuard {
        &self.guard
    }

    /// Project root this sandbox was constructed with.
    pub fn project_root(&self) -> &Path {
        self.guard.project_root()
    }

    /// Explicit permission to execute trusted project commands on the host.
    /// This grants no filesystem or network isolation and must be labeled so.
    pub fn with_host_execution_approval(mut self) -> Self {
        self.host_execution_approved = true;
        self
    }

    /// Execute an immutable user-approved verification command, never a command
    /// selected by model output. Required isolation currently fails closed.
    pub async fn run_approved_check(
        &self,
        program: String,
        args: Vec<String>,
        timeout: Duration,
    ) -> Result<Output> {
        if !self.host_execution_approved {
            return Err(anyhow!("Isolation unavailable: no filesystem/network containment backend is configured. Explicit host execution approval is required."));
        }
        let call = ToolCall::Run { program, args };
        if let GuardDecision::Block { reason } = self.guard.evaluate(&call) {
            return Err(anyhow!("BLOCKED by sandbox: {reason}"));
        }
        self.execute_with_timeout(call, timeout, None, None).await
    }

    /// Run an approved Node check with V8's coverage output directed to a
    /// harness-owned folder. Coverage is process evidence, not isolation.
    pub async fn run_approved_check_with_node_coverage(
        &self,
        program: String,
        args: Vec<String>,
        timeout: Duration,
        coverage_dir: &Path,
    ) -> Result<Output> {
        if !self.host_execution_approved {
            return Err(anyhow!(
                "Isolation unavailable: explicit host execution approval is required."
            ));
        }
        if !coverage_dir.is_absolute() || !coverage_dir.is_dir() {
            return Err(anyhow!(
                "Node coverage folder must be an existing absolute directory"
            ));
        }
        let call = ToolCall::Run { program, args };
        if let GuardDecision::Block { reason } = self.guard.evaluate(&call) {
            return Err(anyhow!("BLOCKED by sandbox: {reason}"));
        }
        self.execute_with_timeout(call, timeout, Some(coverage_dir), None)
            .await
    }

    /// Run an approved Python check with a harness-owned startup hook. The
    /// original program and arguments remain unchanged. This trace is only
    /// process-reported source-loading evidence, not isolation or attestation.
    pub async fn run_approved_check_with_python_trace(
        &self,
        program: String,
        args: Vec<String>,
        timeout: Duration,
        hook_dir: &Path,
        trace_dir: &Path,
    ) -> Result<Output> {
        if !self.host_execution_approved {
            return Err(anyhow!(
                "Isolation unavailable: explicit host execution approval is required."
            ));
        }
        let hook = hook_dir.join("sitecustomize.py");
        if !hook_dir.is_absolute()
            || !trace_dir.is_absolute()
            || !std::fs::symlink_metadata(hook_dir)?.file_type().is_dir()
            || !std::fs::symlink_metadata(trace_dir)?.file_type().is_dir()
            || !std::fs::symlink_metadata(hook)?.file_type().is_file()
        {
            return Err(anyhow!(
                "Python trace hook and output must be regular, existing absolute paths"
            ));
        }
        let call = ToolCall::Run { program, args };
        if let GuardDecision::Block { reason } = self.guard.evaluate(&call) {
            return Err(anyhow!("BLOCKED by sandbox: {reason}"));
        }
        self.execute_with_timeout(call, timeout, None, Some((hook_dir, trace_dir)))
            .await
    }

    /// Run a tool call through the sandbox. Evaluates the guard first;
    /// `Block` short-circuits without execution.
    pub async fn run_tool(&self, call: ToolCall) -> Result<Output> {
        match self.guard.evaluate(&call) {
            GuardDecision::Allow => self.execute(call).await,
            GuardDecision::Approve { reason } => Err(anyhow!("Approval required: {}", reason)),
            GuardDecision::Block { reason } => Err(anyhow!("BLOCKED by sandbox: {}", reason)),
        }
    }

    async fn execute(&self, call: ToolCall) -> Result<Output> {
        if !self.host_execution_approved {
            return Err(anyhow!(
                "Isolation unavailable: refusing unattended host execution"
            ));
        }
        self.execute_with_timeout(call, Duration::from_secs(30), None, None)
            .await
    }

    async fn execute_with_timeout(
        &self,
        call: ToolCall,
        timeout: Duration,
        node_coverage: Option<&Path>,
        python_trace: Option<(&Path, &Path)>,
    ) -> Result<Output> {
        let mut cmd = self.build_command(call)?;
        if let Some(directory) = node_coverage {
            cmd.env("NODE_V8_COVERAGE", directory);
        }
        if let Some((hook_dir, trace_dir)) = python_trace {
            cmd.env("PYTHONPATH", hook_dir)
                .env("PHONTON_PYTHON_TRACE_DIR", trace_dir);
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());
        #[cfg(windows)]
        cmd.creation_flags(0x08000000 | 0x00000004); // CREATE_NO_WINDOW | CREATE_SUSPENDED
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn()?;

        #[cfg(target_os = "windows")]
        let job_handle = {
            use windows::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            use windows::Win32::System::Threading::{
                OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
            };

            tracing::debug!(task_id = %self.task_id, "windows sandbox: attaching job object");

            if let Some(pid) = child.id() {
                unsafe {
                    let job = CreateJobObjectW(None, None)?;
                    let wrapper = OwnedWinHandle(job);
                    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                    SetInformationJobObject(
                        job,
                        JobObjectExtendedLimitInformation,
                        &info as *const _ as *const std::ffi::c_void,
                        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    )?;

                    let process_handle = OwnedWinHandle(OpenProcess(
                        PROCESS_SET_QUOTA | PROCESS_TERMINATE,
                        false,
                        pid,
                    )?);
                    AssignProcessToJobObject(job, process_handle.0)?;
                    resume_suspended_child(pid)?;
                    Some(wrapper)
                }
            } else {
                return Err(anyhow!("Cannot supervise command process"));
            }
        };

        use tokio::io::AsyncReadExt;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Missing command stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Missing command stderr"))?;
        async fn bounded_read(
            stream: impl tokio::io::AsyncRead + Unpin,
        ) -> std::io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            stream.take(1024 * 1024).read_to_end(&mut bytes).await?;
            if bytes.len() == 1024 * 1024 {
                return Err(std::io::Error::other(
                    "Command output exceeded 1 MiB capture limit",
                ));
            }
            Ok(bytes)
        }
        let capture = async {
            let wait = async move {
                let status = child.wait().await?;
                // A direct child can exit while a descendant retains an output
                // pipe. Close the job at that point, before waiting for EOF.
                #[cfg(target_os = "windows")]
                drop(job_handle);
                Ok::<_, std::io::Error>(status)
            };
            let (status, out, err) =
                tokio::try_join!(wait, bounded_read(stdout), bounded_read(stderr))?;
            Ok::<Output, std::io::Error>(Output {
                status,
                stdout: out,
                stderr: err,
            })
        };
        match tokio::time::timeout(timeout, capture).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(anyhow!("Command timed out after {}s", timeout.as_secs())),
        }
    }

    fn build_command(&self, call: ToolCall) -> Result<Command> {
        let project_root = self.guard.project_root();
        match call {
            ToolCall::Run { program, args } => {
                let mut cmd;
                #[cfg(target_os = "macos")]
                {
                    cmd = Command::new("sandbox-exec");
                    cmd.arg("-n").arg("no-network").arg(&program);
                }
                #[cfg(not(target_os = "macos"))]
                {
                    cmd = Command::new(&program);
                }
                cmd.args(args);
                cmd.current_dir(project_root);
                apply_env_scrub(&mut cmd);
                Ok(cmd)
            }
            ToolCall::Bash { command } => {
                let mut cmd;
                let arg;
                #[cfg(target_os = "macos")]
                {
                    cmd = Command::new("sandbox-exec");
                    cmd.arg("-n").arg("no-network").arg("sh");
                    arg = "-c";
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let shell = if cfg!(windows) { "cmd" } else { "sh" };
                    cmd = Command::new(shell);
                    arg = if cfg!(windows) { "/C" } else { "-c" };
                }
                cmd.arg(arg);
                cmd.arg(command);
                cmd.current_dir(project_root);
                apply_env_scrub(&mut cmd);
                Ok(cmd)
            }
            _ => Err(anyhow!("Unsupported tool call for sandbox execution")),
        }
    }
}

/// Drop the host environment and re-inject only the variables every
/// Phonton-dispatched build tool realistically needs. Keeps secrets out of
/// the child process while still letting `cargo` find its toolchain.
fn apply_env_scrub(cmd: &mut Command) {
    cmd.env_clear();
    for key in [
        "PATH",
        "PATHEXT",
        "HOME",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "SYSTEMROOT",
        "SYSTEMDRIVE",
        "COMSPEC",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
}

// ---------------------------------------------------------------------------
// CrateLock — coarse-grained per-crate exclusion for `cargo` invocations
// ---------------------------------------------------------------------------

/// Per-crate mutex registry.
///
/// `cargo` serialises its own work behind `target/debug/.cargo-lock`, but
/// when two parallel workers race for the same crate the result is one
/// of them blocking on the lock for tens of seconds — wasted wall-clock
/// time that the orchestrator's parallel scheduler is supposed to save.
/// `CrateLock` short-circuits that contention in-process: a worker
/// announces the crate it's about to touch via [`acquire`], gets back an
/// async-safe RAII guard, and only then spawns the `cargo` child.
///
/// The registry is `Arc<Mutex<HashMap<...>>>`-backed so it can be cloned
/// into every worker; per-crate entries are `Arc<tokio::sync::Mutex<()>>`
/// so the actual await happens off the registry's blocking lock. This is
/// the key parallelism invariant: independent crates run concurrently,
/// same-crate work serialises.
///
/// [`acquire`]: CrateLock::acquire
#[derive(Clone, Default)]
pub struct CrateLock {
    inner: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    >,
}

/// RAII guard returned by [`CrateLock::acquire`].
///
/// Holds the per-crate `tokio::sync::Mutex` for the lifetime of the
/// guard; dropping it releases the lock and any other worker awaiting
/// the same crate is woken. Carries the crate name only for logging.
pub struct CrateLockGuard {
    _inner: tokio::sync::OwnedMutexGuard<()>,
    krate: String,
}

impl CrateLockGuard {
    /// Crate name this guard is currently holding the lock for.
    pub fn crate_name(&self) -> &str {
        &self.krate
    }
}

impl CrateLock {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire the per-crate mutex for `crate_name`.
    ///
    /// Awaits if another worker currently holds it. The returned guard
    /// must outlive the spawned `cargo` child; the worker should drop
    /// it as soon as the build/test/check finishes so independent crates
    /// can keep flowing in parallel.
    pub async fn acquire(&self, crate_name: &str) -> CrateLockGuard {
        let mutex: std::sync::Arc<tokio::sync::Mutex<()>> = {
            let mut map = self
                .inner
                .lock()
                .expect("CrateLock registry mutex poisoned");
            map.entry(crate_name.to_string())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let owned = mutex.lock_owned().await;
        tracing::debug!(crate_name, "crate-lock acquired");
        CrateLockGuard {
            _inner: owned,
            krate: crate_name.to_string(),
        }
    }

    /// Try to acquire without blocking. Returns `None` if another worker
    /// already holds the lock — the caller should treat that as "skip
    /// this crate for now and revisit later".
    pub fn try_acquire(&self, crate_name: &str) -> Option<CrateLockGuard> {
        let mutex: std::sync::Arc<tokio::sync::Mutex<()>> = {
            let mut map = self
                .inner
                .lock()
                .expect("CrateLock registry mutex poisoned");
            map.entry(crate_name.to_string())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let owned = mutex.try_lock_owned().ok()?;
        Some(CrateLockGuard {
            _inner: owned,
            krate: crate_name.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn environment_scrub_removes_unapproved_values_but_preserves_os_paths() {
        let mut command = Command::new("unused-test-command");
        command.env("PHONTON_TEST_SECRET", "must-not-reach-child");
        apply_env_scrub(&mut command);
        let values: std::collections::BTreeMap<_, _> = command.as_std().get_envs().collect();
        assert!(!values.contains_key(std::ffi::OsStr::new("PHONTON_TEST_SECRET")));
        #[cfg(windows)]
        assert_eq!(
            values
                .get(std::ffi::OsStr::new("SYSTEMDRIVE"))
                .copied()
                .flatten(),
            env::var_os("SYSTEMDRIVE").as_deref()
        );
    }

    #[tokio::test]
    async fn sandbox_allows_safe_command() {
        let root = env::current_dir().expect("get current dir");
        let sandbox = Sandbox::new(root, "test-task-1".to_string()).with_host_execution_approval();
        let call = ToolCall::Run {
            program: "cargo".to_string(),
            args: vec!["--version".to_string()],
        };
        let res = sandbox.run_tool(call).await;
        assert!(res.is_ok(), "expected cargo --version to succeed: {res:?}");
    }

    #[cfg(windows)]
    #[test]
    #[allow(clippy::zombie_processes)] // Intentional detached child exercises Job Object cleanup.
    fn windows_job_child_helper() {
        if !Path::new("spawn.flag").exists() {
            return;
        }
        if env::var_os("PHONTON_JOB_DESCENDANT").is_some() {
            std::fs::write("descendant.started", "started").unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            std::fs::write("descendant.marker", "escaped").unwrap();
            return;
        }
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", "tests::windows_job_child_helper"])
            .env("PHONTON_JOB_DESCENDANT", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command.creation_flags(0x08000000);
        command.spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !Path::new("descendant.started").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(Path::new("descendant.started").exists());
        std::fs::write("spawned.marker", "spawned").unwrap();
        if Path::new("wait.flag").exists() {
            std::thread::sleep(Duration::from_secs(10));
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_closes_with_a_detached_descendant() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("phonton-job-{}-{suffix}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("spawn.flag"), "spawn").unwrap();
        let sandbox =
            Sandbox::new(root.clone(), "job-child-test".into()).with_host_execution_approval();
        let started = std::time::Instant::now();
        let output = sandbox
            .run_approved_check(
                env::current_exe().unwrap().to_string_lossy().into_owned(),
                vec!["--exact".into(), "tests::windows_job_child_helper".into()],
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        let check_elapsed = started.elapsed();
        assert!(output.status.success(), "{output:?}");
        assert!(root.join("spawned.marker").exists());
        assert!(root.join("descendant.started").exists());
        tokio::time::sleep(Duration::from_millis(1900)).await;
        assert!(
            !root.join("descendant.marker").exists(),
            "check returned after {:?}; stdout={} stderr={}",
            check_elapsed,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_file(root.join("spawned.marker")).unwrap();
        std::fs::remove_file(root.join("descendant.started")).unwrap();
        std::fs::remove_file(root.join("spawn.flag")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_cancellation_terminates_descendants() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "phonton-job-cancel-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("spawn.flag"), "spawn").unwrap();
        std::fs::write(root.join("wait.flag"), "wait").unwrap();
        let sandbox =
            Sandbox::new(root.clone(), "job-cancel-test".into()).with_host_execution_approval();
        let executable = env::current_exe().unwrap().to_string_lossy().into_owned();
        let task = tokio::spawn(async move {
            sandbox
                .run_approved_check(
                    executable,
                    vec!["--exact".into(), "tests::windows_job_child_helper".into()],
                    Duration::from_secs(10),
                )
                .await
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !root.join("spawned.marker").exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(root.join("spawned.marker").exists());
        assert!(root.join("descendant.started").exists());
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(1900)).await;
        assert!(!root.join("descendant.marker").exists());
        std::fs::remove_file(root.join("spawned.marker")).unwrap();
        std::fs::remove_file(root.join("descendant.started")).unwrap();
        std::fs::remove_file(root.join("spawn.flag")).unwrap();
        std::fs::remove_file(root.join("wait.flag")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[tokio::test]
    async fn sandbox_blocks_sensitive_path() {
        let root = env::current_dir().expect("get current dir");
        let sandbox = Sandbox::new(root, "test-task-2".to_string());
        let call = ToolCall::Read {
            path: PathBuf::from("/etc/passwd"),
        };
        let res = sandbox.run_tool(call).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("BLOCKED"));
    }

    #[tokio::test]
    async fn approved_check_does_not_override_a_blocked_path() {
        let root = env::current_dir().expect("get current dir");
        let sandbox =
            Sandbox::new(root, "approved-block-test".into()).with_host_execution_approval();
        let error = sandbox
            .run_approved_check(
                "cargo".into(),
                vec!["C:\\Users\\fixture\\.ssh\\id_rsa".into()],
                Duration::from_secs(1),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("BLOCKED by sandbox"));
    }

    #[test]
    fn allow_read_inside_root() {
        let root = env::current_dir().unwrap();
        let g = ExecutionGuard::new(root.clone());
        let d = g.evaluate(&ToolCall::Read {
            path: root.join("src/lib.rs"),
        });
        assert_eq!(d, GuardDecision::Allow);
    }

    #[test]
    fn traversal_never_gets_implicit_permission() {
        let g = ExecutionGuard::new(env::current_dir().unwrap());
        assert!(!matches!(
            g.evaluate(&ToolCall::Write {
                path: "../outside".into(),
                content: "x".into()
            }),
            GuardDecision::Allow
        ));
    }

    #[tokio::test]
    async fn required_isolation_fails_before_command_execution() {
        let sandbox = Sandbox::new(env::current_dir().unwrap(), "isolation-test".into());
        let error = sandbox
            .run_tool(ToolCall::Run {
                program: "cargo".into(),
                args: vec!["--version".into()],
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Isolation unavailable"));
    }

    #[test]
    fn block_ssh() {
        let g = ExecutionGuard::new(PathBuf::from("/work/proj"));
        let d = g.evaluate(&ToolCall::Read {
            path: PathBuf::from("/home/u/.ssh/id_rsa"),
        });
        assert!(matches!(d, GuardDecision::Block { .. }));
    }

    #[tokio::test]
    async fn crate_lock_serialises_same_crate() {
        let lock = CrateLock::new();
        let g1 = lock.acquire("phonton-types").await;
        // try_acquire on the same crate must fail while g1 lives.
        assert!(lock.try_acquire("phonton-types").is_none());
        drop(g1);
        // Now it succeeds.
        assert!(lock.try_acquire("phonton-types").is_some());
    }

    #[tokio::test]
    async fn crate_lock_independent_crates_concurrent() {
        let lock = CrateLock::new();
        let _a = lock.acquire("crate-a").await;
        // Different crate must not be blocked.
        let b = lock.try_acquire("crate-b");
        assert!(b.is_some());
        assert_eq!(b.unwrap().crate_name(), "crate-b");
    }

    #[tokio::test]
    async fn crate_lock_second_acquire_awaits_release() {
        use std::sync::Arc;
        use tokio::sync::oneshot;
        let lock = Arc::new(CrateLock::new());
        let g1 = lock.acquire("phonton-verify").await;
        let lock2 = Arc::clone(&lock);
        let (tx, mut rx) = oneshot::channel::<()>();
        let h = tokio::spawn(async move {
            let _g = lock2.acquire("phonton-verify").await;
            let _ = tx.send(());
        });
        // The spawned task must NOT have signalled yet — we still hold g1.
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        drop(g1);
        // After release, the waiter completes promptly.
        h.await.unwrap();
    }

    #[test]
    fn approve_arbitrary_bash() {
        let g = ExecutionGuard::new(PathBuf::from("/work/proj"));
        let d = g.evaluate(&ToolCall::Bash {
            command: "echo hi".into(),
        });
        assert!(matches!(d, GuardDecision::Approve { .. }));
    }
}
