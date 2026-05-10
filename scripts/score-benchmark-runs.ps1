param(
    [string]$RunsDir = "benchmarks/runs",
    [string]$OutJson = "",
    [string]$OutMarkdown = ""
)

$ErrorActionPreference = "Stop"

function Read-JsonFile($Path) {
    if (!(Test-Path -LiteralPath $Path)) {
        return $null
    }
    Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}

function Token-Total($Usage) {
    if ($null -eq $Usage) {
        return 0
    }
    $totalTokens = $Usage.PSObject.Properties["total_tokens"]
    if ($null -ne $totalTokens) {
        return [int64]$totalTokens.Value
    }
    $total = 0L
    foreach ($name in @("input_tokens", "prompt_tokens", "output_tokens", "completion_tokens", "cached_tokens", "cache_creation_tokens")) {
        $property = $Usage.PSObject.Properties[$name]
        if ($null -ne $property) {
            $total += [int64]$property.Value
        }
    }
    $total
}

function Is-Verified($Review, $Metadata) {
    if ($null -ne $Review) {
        foreach ($name in @("verified", "success", "passed")) {
            $property = $Review.PSObject.Properties[$name]
            if ($null -ne $property) {
                return [bool]$property.Value
            }
        }
        $score = $Review.PSObject.Properties["score"]
        if ($null -ne $score -and [double]$score.Value -ge 1.0) {
            return $true
        }
    }
    if ($null -ne $Metadata) {
        $status = $Metadata.PSObject.Properties["status"]
        if ($null -ne $status) {
            return ([string]$status.Value -in @("verified", "pass", "passed", "success"))
        }
    }
    return $false
}

$root = Resolve-Path -LiteralPath $RunsDir -ErrorAction SilentlyContinue
if ($null -eq $root) {
    throw "Runs directory not found: $RunsDir"
}

$runs = Get-ChildItem -LiteralPath $root -Recurse -Filter metadata.json | ForEach-Object {
    $runDir = $_.Directory.FullName
    $metadata = Read-JsonFile $_.FullName
    $usage = Read-JsonFile (Join-Path $runDir "token-usage.json")
    $review = Read-JsonFile (Join-Path $runDir "quality-review.json")
    $tool = if ($metadata -and $metadata.tool) { [string]$metadata.tool } else { Split-Path (Split-Path $runDir -Parent) -Leaf }
    [pscustomobject]@{
        tool = $tool
        task = if ($metadata -and $metadata.task_id) { [string]$metadata.task_id } else { Split-Path (Split-Path $runDir -Parent) -Leaf }
        run = Split-Path $runDir -Leaf
        verified = Is-Verified $review $metadata
        tokens = Token-Total $usage
        path = $runDir
    }
}

$summary = $runs | Group-Object tool | ForEach-Object {
    $verified = @($_.Group | Where-Object { $_.verified }).Count
    $tokens = ($_.Group | Measure-Object tokens -Sum).Sum
    $score = if ($tokens -gt 0) { [math]::Round(($verified / ($tokens / 10000.0)), 3) } else { 0 }
    [pscustomobject]@{
        tool = $_.Name
        runs = $_.Count
        verified = $verified
        total_tokens = [int64]$tokens
        verified_success_per_10k_tokens = $score
    }
} | Sort-Object verified_success_per_10k_tokens -Descending

$result = [pscustomobject]@{
    metric = "verified_success_per_10k_tokens"
    runs_dir = $root.Path
    generated_at = (Get-Date).ToUniversalTime().ToString("o")
    summary = $summary
    runs = $runs
}

if ($OutJson) {
    $result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutJson -Encoding UTF8
}

if ($OutMarkdown) {
    $lines = @(
        "# Benchmark Score",
        "",
        "Primary metric: verified success per 10k provider-reported tokens.",
        "",
        "| Tool | Runs | Verified | Total tokens | Verified success / 10k tokens |",
        "|---|---:|---:|---:|---:|"
    )
    foreach ($row in $summary) {
        $lines += "| $($row.tool) | $($row.runs) | $($row.verified) | $($row.total_tokens) | $($row.verified_success_per_10k_tokens) |"
    }
    $lines | Set-Content -LiteralPath $OutMarkdown -Encoding UTF8
}

$summary | Format-Table -AutoSize
