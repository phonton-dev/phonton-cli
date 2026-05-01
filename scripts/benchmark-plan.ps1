param(
    [switch]$ReleaseBinary,
    [string[]]$Goals = @(
        "add input validation to config loading",
        "improve provider auth error messages",
        "write tests for rollback failure handling",
        "add a review summary for verified diffs"
    ),
    [string]$OutDir = "benchmarks/results"
)

$ErrorActionPreference = "Stop"

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [string]$FilePath,
        [string[]]$Arguments = @()
    )

    & $FilePath @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $repo
try {
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $jsonPath = Join-Path $OutDir "plan-benchmark-$stamp.json"
    $mdPath = Join-Path $OutDir "plan-benchmark-$stamp.md"

    $binary = if ($ReleaseBinary) {
        Invoke-Checked cargo @("build", "--locked", "--release", "-p", "phonton-cli")
        Join-Path $repo "target/release/phonton.exe"
    } else {
        Invoke-Checked cargo @("build", "--locked", "-p", "phonton-cli")
        Join-Path $repo "target/debug/phonton.exe"
    }

    if (-not (Test-Path $binary)) {
        $binary = if ($ReleaseBinary) {
            Join-Path $repo "target/release/phonton"
        } else {
            Join-Path $repo "target/debug/phonton"
        }
    }

    $results = @()
    foreach ($goal in $Goals) {
        $started = Get-Date
        $status = "pass"
        $errorMessage = $null
        $plan = $null

        try {
            $raw = & $binary plan --json $goal 2>&1
            if ($LASTEXITCODE -ne 0) {
                throw "phonton plan failed with exit code ${LASTEXITCODE}: $raw"
            }
            $plan = ($raw | Out-String) | ConvertFrom-Json
        } catch {
            $status = "fail"
            $errorMessage = $_.Exception.Message
        }

        $elapsedMs = [int]((Get-Date) - $started).TotalMilliseconds
        $estimated = if ($plan) { [double]$plan.plan.estimated_total_tokens } else { 0 }
        $baseline = if ($plan) { [double]$plan.plan.naive_baseline_tokens } else { 0 }
        $reduction = if ($baseline -gt 0) { [math]::Round((1 - ($estimated / $baseline)) * 100, 2) } else { $null }
        $subtasks = if ($plan) { @($plan.plan.subtasks).Count } else { 0 }

        $results += [pscustomobject]@{
            goal = $goal
            status = $status
            subtasks = $subtasks
            estimated_total_tokens = [int]$estimated
            naive_baseline_tokens = [int]$baseline
            estimated_reduction_percent = $reduction
            elapsed_ms = $elapsedMs
            error = $errorMessage
        }
    }

    $commit = (git rev-parse --short HEAD)
    if ($LASTEXITCODE -ne 0) {
        throw "git rev-parse failed with exit code $LASTEXITCODE"
    }

    $version = (& $binary version).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "$binary version failed with exit code $LASTEXITCODE"
    }

    $summary = [pscustomobject]@{
        generated_at = (Get-Date).ToString("o")
        commit = $commit
        phonton_version = $version
        benchmark_type = "plan-preview-token-estimate"
        results = $results
    }

    $summary | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 $jsonPath

    $lines = @()
    $lines += "# Phonton Plan Benchmark"
    $lines += ""
    $lines += "- Generated: $($summary.generated_at)"
    $lines += "- Commit: $($summary.commit)"
    $lines += "- Version: $($summary.phonton_version)"
    $lines += "- Benchmark type: $($summary.benchmark_type)"
    $lines += ""
    $lines += "> These are planner estimates, not provider billing records. Use them as release evidence, not as public proof of end-to-end token savings."
    $lines += ""
    $lines += "| Goal | Status | Subtasks | Phonton estimate | Naive baseline | Estimated reduction | Time |"
    $lines += "|---|---:|---:|---:|---:|---:|---:|"
    foreach ($r in $results) {
        $reductionText = if ($null -eq $r.estimated_reduction_percent) { "n/a" } else { "$($r.estimated_reduction_percent)%" }
        $lines += "| $($r.goal) | $($r.status) | $($r.subtasks) | $($r.estimated_total_tokens) | $($r.naive_baseline_tokens) | $reductionText | $($r.elapsed_ms)ms |"
    }
    $lines += ""
    $lines += "Raw JSON: ``$jsonPath``"
    $lines | Set-Content -Encoding UTF8 $mdPath

    Write-Host "Benchmark complete:"
    Write-Host "  $mdPath"
    Write-Host "  $jsonPath"

    if ($results | Where-Object { $_.status -ne "pass" }) {
        throw "One or more benchmark goals failed. See $mdPath"
    }
} finally {
    Pop-Location
}
