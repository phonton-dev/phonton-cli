use phonton_types::verification::VerificationExecution;
use phonton_types::VerifyResult;
use std::{path::Path, process::Output, time::Duration};

pub(crate) async fn run(
    root: &Path,
    program: &str,
    args: Vec<String>,
    timeout: Duration,
    policy: VerificationExecution,
) -> std::result::Result<Output, VerifyResult> {
    let mut executor = phonton_sandbox::Sandbox::new(root.into(), "verification".into());
    if policy == VerificationExecution::HostApproved {
        executor = executor.with_host_execution_approval();
    }
    executor
        .run_approved_check(program.into(), args, timeout)
        .await
        .map_err(|error| VerifyResult::Unavailable {
            reason: format!(
                "Verification unavailable; no passing evidence from {program}: {error}"
            ),
        })
}
