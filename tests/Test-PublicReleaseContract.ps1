$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$setup = Get-Content -LiteralPath (Join-Path $projectRoot 'src\bin\clm-setup.rs') -Raw
$release = Get-Content -LiteralPath (Join-Path $projectRoot 'tools\Build-PublicRelease.ps1') -Raw
$runtimeSource = Get-Content -LiteralPath (Join-Path $projectRoot 'src\runtime.rs') -Raw

foreach ($required in @(
        'Enable lazy history',
        'Restore the original full-file layout',
        'scan_codex_conversations',
        'rehydrate_migration')) {
    if ($setup -notmatch [regex]::Escape($required)) {
        throw "CLMSetup is missing required behavior: $required"
    }
}
if ($setup -match 'PriorityClass|git-review-mode|NoReviewWorkspace|mcp') {
    throw 'CLMSetup must not apply unrelated Git Review, priority, or MCP mitigations.'
}
$machineSpecificPath = 'C:' + '\\Users\\' + '|F:' + '\\'
if ($runtimeSource -match $machineSpecificPath) {
    throw 'Public runtime source contains a machine-specific default path.'
}
if ($release -notmatch 'never redistribute the official Codex backend') {
    throw 'Release packaging does not enforce the official-binary boundary.'
}
foreach ($file in @('LICENSE', 'README.md', 'SECURITY.md')) {
    if (-not (Test-Path -LiteralPath (Join-Path $projectRoot $file) -PathType Leaf)) {
        throw "Public release file is missing: $file"
    }
}

'Public release contract: PASS'
