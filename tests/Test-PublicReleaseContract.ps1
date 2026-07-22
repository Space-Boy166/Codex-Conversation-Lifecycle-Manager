$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$setup = Get-Content -LiteralPath (Join-Path $projectRoot 'src\bin\clm-setup.rs') -Raw
$release = Get-Content -LiteralPath (Join-Path $projectRoot 'tools\Build-PublicRelease.ps1') -Raw
$pathSafetyPath = Join-Path $projectRoot 'tools\CLM.PathSafety.ps1'
if (-not (Test-Path -LiteralPath $pathSafetyPath -PathType Leaf)) {
    throw 'Public release path-safety helper is missing.'
}
$pathSafety = Get-Content -LiteralPath $pathSafetyPath -Raw
$exporterPath = Join-Path $projectRoot 'tools\Export-PublicRepository.ps1'
$exporter = if (Test-Path -LiteralPath $exporterPath -PathType Leaf) {
    Get-Content -LiteralPath $exporterPath -Raw
}
else {
    ''
}
$runtimeSource = Get-Content -LiteralPath (Join-Path $projectRoot 'src\runtime.rs') -Raw
$healthTool = Get-Content -LiteralPath (Join-Path $projectRoot 'tools\Get-CodexClmHealth.ps1') -Raw
$refreshDocPath = Join-Path $projectRoot 'docs\MANAGED_TAIL_REFRESH.md'
$refreshDocExists = Test-Path -LiteralPath $refreshDocPath -PathType Leaf
if (-not $refreshDocExists) {
    throw 'Public managed-tail refresh guidance is missing.'
}
$refreshDoc = Get-Content -LiteralPath $refreshDocPath -Raw
$healthDocPath = Join-Path $projectRoot 'docs\HEALTH_REPORT.md'
$skillsCacheDocPath = Join-Path $projectRoot 'docs\SKILLS_LIST_CACHE.md'
foreach ($requiredDoc in @($healthDocPath, $skillsCacheDocPath)) {
    if (-not (Test-Path -LiteralPath $requiredDoc -PathType Leaf)) {
        throw "Public operator document is missing: $requiredDoc"
    }
}
$architecture = Get-Content -LiteralPath (Join-Path $projectRoot 'docs\ARCHITECTURE.md') -Raw

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
if ($healthTool -match $machineSpecificPath) {
    throw 'Public health tool contains a machine-specific default path.'
}
if ($release -notmatch 'never redistribute the official Codex backend') {
    throw 'Release packaging does not enforce the official-binary boundary.'
}
if ($release -notmatch [regex]::Escape('CLM.PathSafety.ps1') -or
    $release -notmatch 'Remove-ClmDirectoryTreeSafely') {
    throw 'Release packaging bypasses the recursive-delete path guard.'
}
if ($exporter -and $exporter -notmatch [regex]::Escape('docs\MANAGED_TAIL_REFRESH.md')) {
    throw 'Public export omits the managed-tail refresh document referenced by Architecture.'
}
foreach ($publicArtifact in @(
        'docs\HEALTH_REPORT.md',
        'docs\SKILLS_LIST_CACHE.md',
        'tools\Get-CodexClmHealth.ps1',
        'tests\Test-HealthReportContract.ps1',
        'tests\Test-SkillsListCacheCanary.ps1')) {
    if ($exporter -and $exporter -notmatch [regex]::Escape($publicArtifact)) {
        throw "Public export omits the documented artifact: $publicArtifact"
    }
}
foreach ($releaseArtifact in @(
        'docs\HEALTH_REPORT.md',
        'docs\SKILLS_LIST_CACHE.md',
        'tools\Get-CodexClmHealth.ps1')) {
    if ($release -notmatch [regex]::Escape($releaseArtifact)) {
        throw "Public release omits the documented artifact: $releaseArtifact"
    }
}
if ($exporter -and
    ($exporter -notmatch [regex]::Escape('tools\CLM.PathSafety.ps1') -or
        $exporter -notmatch 'Remove-ClmDirectoryTreeSafely')) {
    throw 'Public export bypasses or omits the recursive-delete path guard.'
}
if ($pathSafety -notmatch 'Assert-ClmRecursiveDeleteTarget' -or
    $pathSafety -notmatch 'USERPROFILE' -or
    $pathSafety -notmatch 'CODEX_HOME') {
    throw 'Path-safety helper does not protect the user profile and Codex home.'
}
$internalDrivePattern = [regex]::Escape(('F:' + [IO.Path]::DirectorySeparatorChar))
$internalIntegrationPattern = @(
    ('Start-CodexClmRefresh' + 'Handoff'),
    ('Codex Micro' + ' Guard'),
    $internalDrivePattern
) -join '|'
if (($refreshDoc + $architecture) -match $internalIntegrationPattern) {
    throw 'Public lifecycle guidance contains an internal machine integration.'
}
foreach ($file in @('LICENSE', 'README.md', 'SECURITY.md')) {
    if (-not (Test-Path -LiteralPath (Join-Path $projectRoot $file) -PathType Leaf)) {
        throw "Public release file is missing: $file"
    }
}

'Public release contract: PASS'
