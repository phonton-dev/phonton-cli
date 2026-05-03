param(
    [ValidateSet("stable", "dev", "nightly", "main")]
    [string] $Channel = $env:PHONTON_CHANNEL,
    [switch] $DryRun
)

if (-not $Channel) {
    $Channel = "stable"
}

$repo = "https://github.com/phonton-dev/phonton-cli"

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "cargo is required. Install Rust from https://rustup.rs/ and rerun this script."
    exit 1
}

$refArgs = switch ($Channel) {
    "stable" { @("--tag", "v0.1.0") }
    "dev" { @("--branch", "dev") }
    "nightly" { @("--branch", "nightly") }
    "main" { @("--branch", "main") }
}

$cmd = @("cargo", "install", "--git", $repo) + $refArgs + @("phonton-cli", "--locked", "--force")
Write-Host ($cmd -join " ")

if (-not $DryRun) {
    & cargo install --git $repo @refArgs phonton-cli --locked --force
}
