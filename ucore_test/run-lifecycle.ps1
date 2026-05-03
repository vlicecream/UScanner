param(
    [int]$Port = 30110,

    [string]$TestDir = $PSScriptRoot
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $TestDir
$localDir = Join-Path $TestDir "local"

$setupPath = Join-Path $localDir "setup.json"
$refreshPath = Join-Path $localDir "refresh.json"
$watchPath = Join-Path $localDir "watch.json"

foreach ($path in @($setupPath, $refreshPath, $watchPath)) {
    if (-not (Test-Path -LiteralPath $path)) {
        throw "Local test file not found: $path. Run make-local.ps1 first."
    }
}

$oldPort = $env:UNL_SERVER_PORT
$env:UNL_SERVER_PORT = [string]$Port

Push-Location $repoRoot
try {
    Write-Host "Using u_scanner server port: $Port"

    Write-Host ""
    Write-Host "== setup =="
    cargo run --bin u_scanner -- setup $setupPath

    Write-Host ""
    Write-Host "== refresh =="
    cargo run --bin u_scanner -- refresh $refreshPath

    Write-Host ""
    Write-Host "== watch =="
    cargo run --bin u_scanner -- watch $watchPath
}
finally {
    Pop-Location

    if ($null -eq $oldPort) {
        Remove-Item Env:\UNL_SERVER_PORT -ErrorAction SilentlyContinue
    }
    else {
        $env:UNL_SERVER_PORT = $oldPort
    }
}

