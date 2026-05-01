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

    Invoke-Checked cargo @("test", "--locked", "--workspace")
    Invoke-Checked cargo @("build", "--locked", "--release", "-p", "phonton-cli")
    Invoke-Checked ".\target\release\phonton.exe" @("doctor")

    if (-not $SkipBench) {
        .\scripts\benchmark-plan.ps1 -ReleaseBinary
    }

    Write-Host "Release check passed."
} finally {
    Pop-Location
}
