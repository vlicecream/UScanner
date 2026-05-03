param(
    [Parameter(Mandatory = $true)]
    [string]$ProjectRoot,

    [int]$Port = 30110,

    [string]$OutputDir = (Join-Path $PSScriptRoot "local")
)

$ErrorActionPreference = "Stop"

$templateDir = Join-Path $PSScriptRoot "templates"
if (-not (Test-Path -LiteralPath $templateDir)) {
    throw "Template directory not found: $templateDir"
}

$resolvedProjectRoot = (Resolve-Path -LiteralPath $ProjectRoot).Path
$projectRootJson = $resolvedProjectRoot.Replace('\', '/')
$testDirJson = $PSScriptRoot.Replace('\', '/')

$output = New-Item -ItemType Directory -Force -Path $OutputDir
$encoding = if ($PSVersionTable.PSVersion.Major -ge 6) { "utf8NoBOM" } else { "utf8" }

Get-ChildItem -LiteralPath $templateDir -Filter "*.json" | ForEach-Object {
    $content = Get-Content -LiteralPath $_.FullName -Raw
    $content = $content.Replace('${SIMPLEBETA_ROOT}', $projectRootJson)
    $content = $content.Replace('${UCORE_TEST_DIR}', $testDirJson)
    $content = $content.Replace('${SERVER_PORT}', [string]$Port)

    $target = Join-Path $output.FullName $_.Name
    Set-Content -LiteralPath $target -Value $content -Encoding $encoding -NoNewline
}

Write-Host "Generated local Unreal project test JSON files in: $($output.FullName)"
