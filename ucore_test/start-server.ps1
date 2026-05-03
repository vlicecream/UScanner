param(
    [int]$Port = 30110,

    [string]$TestDir = $PSScriptRoot
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $TestDir
$registryPath = Join-Path $TestDir "local\registry.json"

if (-not (Test-Path -LiteralPath $registryPath)) {
    throw "Local registry not found: $registryPath. Run make-local.ps1 first."
}

Push-Location $repoRoot
try {
    Write-Host "Starting u_core_server on port $Port"
    Write-Host "Registry: $registryPath"
    cargo run --bin u_core_server -- $Port $registryPath
}
finally {
    Pop-Location
}

