param(
  [switch]$Clean
)

$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Split-Path -Parent $scriptDir
$manifestPath = Join-Path $repoRoot "Cargo.toml"

if (-not (Test-Path -LiteralPath $manifestPath)) {
  throw "Cargo.toml not found: $manifestPath"
}

if ($Clean) {
  cargo clean --manifest-path $manifestPath
}

cargo build --release --manifest-path $manifestPath --bin u_core_server --bin u_scanner
