//! Isolated command execution and tool guarding.
//!
//! Canonical home for [`ToolCall`], [`ExecutionGuard`], and [`GuardDecision`]
//! (previously duplicated in `phonton-worker`, which now re-exports them).
//! The [`Sandbox`] wraps the guard and adds OS-level isolation: Linux
//! namespaces via `unshare`, macOS `sandbox-exec`, Windows Job Objects.
//! On every platform, guard decisions are authoritative — `Block` is never
//! overridden.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::process::Command;

#[cfg(target_os = "windows")]
struct JobHandle(windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
unsafe impl Send for JobHandle {}

#[cfg(target_os = "windows")]
unsafe impl Sync for JobHandle {}

#[cfg(target_os = "windows")]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
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
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.project_root.join(path)
        };
        abs.starts_with(&self.project_root)
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

/// Isolated executor for [`ToolCall::Run`] and [`ToolCall::Bash`].
///
/// Pairs an [`ExecutionGuard`] with platform-specific process isolation
/// (Linux unshare, macOS sandbox-exec, Windows Job Objects). Commands run
/// from `project_root` with a scrubbed environment; output is captured and
/// the process is killed on 30s timeout.
pub struct Sandbox {
    guard: ExecutionGuard,
    task_id: String,
}

impl Sandbox {
    /// Create a new sandbox bound to `project_root`. Typically the
    /// orchestrator's working directory.
    pub fn new(project_root: PathBuf, task_id: String) -> Self {
        Self {
            guard: ExecutionGuard::new(project_root),
            task_id,
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
        let mut cmd = self.build_command(call)?;

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    if let Err(e) = nix::sched::unshare(
                        nix::sched::CloneFlags::CLONE_NEWNET
                            | nix::sched::CloneFlags::CLONE_NEWUSER,
                    ) {
                        tracing::warn!("Failed to unshare namespaces: {}", e);
                    }
                    Ok(())
                });
            }
        }

        cmd.kill_on_drop(true);
        let child = cmd.spawn()?;

        #[cfg(target_os = "windows")]
        let _job_handle = {
            use windows::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            use windows::Win32::System::Threading::{
                OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
            };

            tracing::debug!(task_id = %self.task_id, "windows sandbox: attaching job object");

            let mut job_wrapper = None;
            if let Some(pid) = child.id() {
                unsafe {
                    if let Ok(job) = CreateJobObjectW(None, None) {
                        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                        let _ = SetInformationJobObject(
                            job,
                            JobObjectExtendedLimitInformation,
                            &info as *const _ as *const std::ffi::c_void,
                            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                        );

                        if let Ok(process_handle) =
                            OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid)
                        {
                            let _ = AssignProcessToJobObject(job, process_handle);
                            let _ = windows::Win32::Foundation::CloseHandle(process_handle);
                        }
                        job_wrapper = Some(JobHandle(job));
                    }
                }
            }
            job_wrapper
        };

        match tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Err(anyhow!("Command timed out after 30s")),
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
    for key in ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "SYSTEMROOT"] {
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

    #[tokio::test]
    async fn sandbox_allows_safe_command() {
        let root = env::current_dir().expect("get current dir");
        let sandbox = Sandbox::new(root, "test-task-1".to_string());
        let call = ToolCall::Run {
            program: "cargo".to_string(),
            args: vec!["--version".to_string()],
        };
        let res = sandbox.run_tool(call).await;
        assert!(res.is_ok(), "expected cargo --version to succeed: {res:?}");
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

    #[test]
    fn allow_read_inside_root() {
        let g = ExecutionGuard::new(PathBuf::from("/work/proj"));
        let d = g.evaluate(&ToolCall::Read {
            path: PathBuf::from("/work/proj/src/lib.rs"),
        });
        assert_eq!(d, GuardDecision::Allow);
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
