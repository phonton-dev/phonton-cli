param(
    [switch]$SkipClippy,
    [switch]$SkipBench
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
    Invoke-Checked cargo @("fmt", "--all", "--", "--check")

    if (-not $SkipClippy) {
        Invoke-Checked cargo @("clippy", "--locked", "--workspace", "--all-targets", "--", "-D", "warnings")
    }

    $beforeTestStatus = @(git status --short -- .)
    Invoke-Checked cargo @("test", "--locked", "--workspace")
    $afterTestStatus = @(git status --short -- .)
    $beforeJoined = $beforeTestStatus -join "`n"
    $afterJoined = $afterTestStatus -join "`n"
    if ($afterJoined -ne $beforeJoined) {
        throw "cargo test changed workspace status:`nBefore:`n$beforeJoined`nAfter:`n$afterJoined"
    }
    Invoke-Checked cargo @("build", "--locked", "--release", "-p", "phonton-cli")
    Invoke-Checked ".\target\release\phonton.exe" @("doctor")

    if (-not $SkipBench) {
        .\scripts\benchmark-plan.ps1 -ReleaseBinary
    }

    Write-Host "Release check passed."
} finally {
    Pop-Location
}
