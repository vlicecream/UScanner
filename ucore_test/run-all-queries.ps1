param(
    [int]$Port = 30110,

    [string]$TestDir = $PSScriptRoot
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $TestDir
$localDir = Join-Path $TestDir "local"
$stamp = Get-Date -Format "yyyyMMdd_HHmmss"
$outDir = Join-Path $TestDir "out\$stamp"
$summaryPath = Join-Path $outDir "summary.txt"

if (-not (Test-Path -LiteralPath $localDir)) {
    throw "Local test directory not found: $localDir. Run make-local.ps1 first."
}

$queries = Get-ChildItem -LiteralPath $localDir -Filter "query_*.json" | Sort-Object Name
if ($queries.Count -eq 0) {
    throw "No query_*.json files found in: $localDir"
}

New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$oldPort = $env:UNL_SERVER_PORT
$env:UNL_SERVER_PORT = [string]$Port

Push-Location $repoRoot
try {
    foreach ($query in $queries) {
        $payload = Get-Content -LiteralPath $query.FullName -Raw
        $baseName = [System.IO.Path]::GetFileNameWithoutExtension($query.Name)
        $resultPath = Join-Path $outDir "$baseName.result.json"
        $errorPath = Join-Path $outDir "$baseName.error.txt"

        Write-Host "== $($query.Name) =="

        $output = & cargo run --quiet --bin u_scanner -- query $payload 2> $errorPath
        $exitCode = $LASTEXITCODE

        if ($exitCode -eq 0) {
            $text = ($output | Out-String).Trim()
            Set-Content -LiteralPath $resultPath -Value $text -Encoding utf8
            if ((Test-Path -LiteralPath $errorPath) -and ((Get-Item -LiteralPath $errorPath).Length -eq 0)) {
                Remove-Item -LiteralPath $errorPath
            }

            try {
                $json = $text | ConvertFrom-Json
                if ($json -is [System.Array]) {
                    $shape = "array count=$($json.Count)"
                }
                elseif ($null -eq $json) {
                    $shape = "null"
                }
                else {
                    $shape = "object"
                }
            }
            catch {
                $shape = "raw"
            }

            "OK`t$($query.Name)`t$shape`t$resultPath" | Add-Content -LiteralPath $summaryPath
            Write-Host "OK -> $resultPath"
        }
        else {
            $text = ($output | Out-String).Trim()
            if ($text.Length -gt 0) {
                Add-Content -LiteralPath $errorPath -Value $text
            }
            "ERR`t$($query.Name)`t$errorPath" | Add-Content -LiteralPath $summaryPath
            Write-Host "ERR -> $errorPath"
        }
    }
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

Write-Host ""
Write-Host "Summary: $summaryPath"
