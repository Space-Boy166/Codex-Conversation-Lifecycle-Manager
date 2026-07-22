[CmdletBinding()]
param(
    [string]$Configuration = 'release'
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'CLM.PathSafety.ps1')
$projectRoot = Split-Path -Parent $PSScriptRoot
$manifest = Get-Content -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Raw
$versionMatch = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) {
    throw 'Could not read the package version from Cargo.toml.'
}
$version = $versionMatch.Groups[1].Value
$metadata = cargo metadata --format-version 1 --no-deps --manifest-path (Join-Path $projectRoot 'Cargo.toml') |
    ConvertFrom-Json
$targetRoot = [IO.Path]::GetFullPath([string]$metadata.target_directory)
$artifactRoot = Join-Path $projectRoot 'artifacts\release'
$packageName = "conversation-lifecycle-manager-v$version-windows-x64"
$stage = Join-Path $artifactRoot $packageName
$zip = Join-Path $artifactRoot "$packageName.zip"

New-Item -ItemType Directory -Path $artifactRoot -Force | Out-Null
if (Test-Path -LiteralPath $stage) {
    $resolvedStage = [IO.Path]::GetFullPath($stage)
    Remove-ClmDirectoryTreeSafely `
        -TargetPath $resolvedStage `
        -AllowedRoot $artifactRoot `
        -Purpose 'release staging cleanup'
}
if (Test-Path -LiteralPath $zip) {
    Remove-Item -LiteralPath $zip -Force
}

cargo build --manifest-path (Join-Path $projectRoot 'Cargo.toml') --locked --release `
    --bin CLMSetup --bin conversation-lifecycle-manager --bin codex-clm-proxy
if ($LASTEXITCODE -ne 0) {
    throw 'Cargo release build failed.'
}

New-Item -ItemType Directory -Path $stage | Out-Null
foreach ($name in @('CLMSetup.exe', 'conversation-lifecycle-manager.exe', 'codex-clm-proxy.exe')) {
    $source = Join-Path (Join-Path $targetRoot $Configuration) $name
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "Missing release binary: $source"
    }
    Copy-Item -LiteralPath $source -Destination (Join-Path $stage $name)
}
Copy-Item -LiteralPath (Join-Path $projectRoot 'README.md') -Destination $stage
Copy-Item -LiteralPath (Join-Path $projectRoot 'LICENSE') -Destination $stage
Copy-Item -LiteralPath (Join-Path $projectRoot 'SECURITY.md') -Destination $stage
New-Item -ItemType Directory -Path (Join-Path $stage 'docs') | Out-Null
Copy-Item -LiteralPath (Join-Path $projectRoot 'docs\CODEX_DESKTOP_TROUBLESHOOTING.md') `
    -Destination (Join-Path $stage 'docs')
Copy-Item -LiteralPath (Join-Path $projectRoot 'docs\HEALTH_REPORT.md') `
    -Destination (Join-Path $stage 'docs')
Copy-Item -LiteralPath (Join-Path $projectRoot 'docs\SKILLS_LIST_CACHE.md') `
    -Destination (Join-Path $stage 'docs')
New-Item -ItemType Directory -Path (Join-Path $stage 'tools') | Out-Null
Copy-Item -LiteralPath (Join-Path $projectRoot 'tools\CLM.PathSafety.ps1') `
    -Destination (Join-Path $stage 'tools')
Copy-Item -LiteralPath (Join-Path $projectRoot 'tools\Get-CodexClmHealth.ps1') `
    -Destination (Join-Path $stage 'tools')

$officialBinary = Get-ChildItem -LiteralPath $stage -File -Recurse |
    Where-Object { $_.Name -ieq 'codex.exe' }
if ($officialBinary) {
    throw 'The release must never redistribute the official Codex backend.'
}

$hashLines = Get-ChildItem -LiteralPath $stage -File -Recurse |
    Sort-Object FullName |
    ForEach-Object {
        $relative = $_.FullName.Substring($stage.Length).TrimStart('\').Replace('\', '/')
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $relative"
    }
$hashLines | Set-Content -LiteralPath (Join-Path $stage 'SHA256SUMS.txt') -Encoding ascii
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -CompressionLevel Optimal

[pscustomobject]@{
    state = 'release_ready'
    version = $version
    stage = $stage
    zip = $zip
    zipSha256 = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
}
