[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$healthPath = Join-Path $projectRoot 'tools\Get-CodexClmHealth.ps1'
. (Join-Path $projectRoot 'tools\CLM.PathSafety.ps1')

if (-not (Test-Path -LiteralPath $healthPath -PathType Leaf)) {
    throw "Health reporter is missing: $healthPath"
}

$tokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    $healthPath,
    [ref]$tokens,
    [ref]$parseErrors
)
if ($parseErrors.Count -gt 0) {
    throw "Health reporter does not parse: $($parseErrors.Message -join '; ')"
}

$forbiddenCommands = @(
    'Remove-Item',
    'Move-Item',
    'Copy-Item',
    'Rename-Item',
    'Set-Content',
    'Add-Content',
    'Clear-Content',
    'Out-File',
    'New-Item',
    'Set-Item',
    'Start-Process',
    'Stop-Process',
    'Register-ObjectEvent',
    'Register-WmiEvent',
    'Register-ScheduledTask',
    'Set-ScheduledTask',
    'Start-Job'
)
$commands = @($ast.FindAll({
            param($node)
            $node -is [Management.Automation.Language.CommandAst]
        }, $true) | ForEach-Object { $_.GetCommandName() } | Where-Object { $_ })
$unsafe = @($commands | Where-Object { $_ -in $forbiddenCommands } | Select-Object -Unique)
if ($unsafe.Count -gt 0) {
    throw "Health reporter contains mutating or background commands: $($unsafe -join ', ')"
}
$pollingLoops = @($ast.FindAll({
            param($node)
            $node -is [Management.Automation.Language.WhileStatementAst] -or
                $node -is [Management.Automation.Language.DoWhileStatementAst] -or
                $node -is [Management.Automation.Language.DoUntilStatementAst]
        }, $true))
if ($pollingLoops.Count -gt 0) {
    throw 'Health reporter must be one-shot and may not contain polling loops.'
}

$tempRoot = Get-ClmNormalizedPath -Path (Resolve-Path -LiteralPath $env:TEMP).Path
$fixtureRoot = Join-Path $tempRoot ("clm-health-{0}" -f [guid]::NewGuid())
$utf8 = New-Object Text.UTF8Encoding($false)

try {
    $runtimeRoot = Join-Path $fixtureRoot 'runtime'
    $vault = Join-Path $runtimeRoot 'Data\Vault\Codex\thread-health'
    $indexRoot = Join-Path $runtimeRoot 'Data\Indexes'
    $exitRoot = Join-Path $runtimeRoot 'Work\ExitMaintenance'
    $codexHome = Join-Path $fixtureRoot 'codex-home'
    $logRoot = Join-Path $fixtureRoot 'logs'
    $sessionRoot = Join-Path $fixtureRoot 'sessions'
    foreach ($directory in @($vault, $indexRoot, $exitRoot, $codexHome, $logRoot, $sessionRoot)) {
        [void](New-Item -ItemType Directory -Path $directory -Force)
    }

    $activePath = Join-Path $sessionRoot 'rollout.jsonl'
    $archivePath = Join-Path $vault 'archive.jsonl'
    $rollbackPath = Join-Path $sessionRoot 'rollout.jsonl.clm-rollback'
    $managedIndexPath = Join-Path $indexRoot 'thread-health.sqlite'
    $stream = [IO.File]::Open($activePath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    try {
        $stream.SetLength(70MB)
    }
    finally {
        $stream.Dispose()
    }
    [IO.File]::WriteAllText($archivePath, "archive`n", $utf8)
    [IO.File]::WriteAllText($managedIndexPath, 'index', $utf8)

    $manifest = [ordered]@{
        threadId = 'thread-health'
        originalPath = $activePath
        archivePath = $archivePath
        rollbackPath = $rollbackPath
        indexPath = $managedIndexPath
        candidateBytes = 1024
        sourceSha256 = 'NOT_CHECKED_WITHOUT_DEEP_INTEGRITY'
    }
    [IO.File]::WriteAllText(
        (Join-Path $vault 'manifest.json'),
        ($manifest | ConvertTo-Json -Depth 5),
        $utf8
    )
    [IO.File]::WriteAllText(
        (Join-Path $codexHome 'session_index.jsonl'),
        (([ordered]@{ id = 'thread-health'; thread_name = 'Health Fixture Task' } | ConvertTo-Json -Compress) + "`n"),
        $utf8
    )
    $maintenance = [ordered]@{
        updatedAt = '2026-07-19T00:00:00+08:00'
        state = 'maintenance_failed'
        detail = 'fixture'
        mode = 'PostExit'
        attemptId = 'fixture-attempt'
        tasks = @(
            [ordered]@{ threadId = 'one'; state = 'refresh_failed' },
            [ordered]@{ threadId = 'two'; state = 'deferred_no_checkpoint' }
        )
    }
    [IO.File]::WriteAllText(
        (Join-Path $exitRoot 'latest-status.json'),
        ($maintenance | ConvertTo-Json -Depth 5),
        $utf8
    )
    $fixtureLog = Join-Path $logRoot 'fixture.log'
    [IO.File]::WriteAllLines(
        $fixtureLog,
        @(
            'unknown conversation conversationId=thread-health',
            'method=skills/list durationMs=1234',
            'CLM skills/list cache store',
            'CLM skills/list cache hit',
            'turn-diff-capture-start',
            'Git is unavailable',
            'method=thread/turns/list durationMs=9',
            'method=thread/resume durationMs=8'
        ),
        $utf8
    )

    $logLease = [IO.File]::Open(
        $fixtureLog,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Write,
        [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
    )
    try {
        $json = & $healthPath `
            -RuntimeRoot $runtimeRoot `
            -CodexHome $codexHome `
            -CodexLogRoot $logRoot `
            -ShortcutPath (Join-Path $fixtureRoot 'missing.lnk') `
            -AsJson
    }
    finally {
        $logLease.Dispose()
    }
    $report = ($json | Out-String) | ConvertFrom-Json

    if ($report.managed.taskCount -ne 1 -or $report.managed.aboveThresholdCount -ne 1) {
        throw 'Health reporter did not classify the managed fixture tail.'
    }
    if ($report.managed.missingArchiveCount -ne 0 -or
        $report.managed.missingIndexCount -ne 0 -or
        $report.managed.missingRollbackCount -ne 1 -or
        $report.managed.manifestErrorCount -ne 0) {
        throw 'Health reporter returned incorrect managed-evidence counts.'
    }
    if ($report.storeLog.counts.unknownConversation -ne 1 -or
        $report.storeLog.counts.skillsList -ne 1 -or
        $report.storeLog.counts.skillsListCacheHit -ne 1 -or
        $report.storeLog.counts.skillsListCacheStore -ne 1 -or
        $report.storeLog.skillsListDurationMs.maximum -ne 1234 -or
        $report.storeLog.counts.reviewCapture -ne 1 -or
        $report.storeLog.counts.gitUnavailable -ne 1) {
        throw 'Health reporter returned incorrect Store-log metrics.'
    }
    if ($report.storeLog.bytes -le 0) {
        throw 'Health reporter returned a stale size for a writer-held Store log.'
    }
    if ($report.runtime.latestMaintenance.taskCount -ne 2 -or
        $report.runtime.latestMaintenance.failedTaskCount -ne 1 -or
        $report.runtime.latestMaintenance.deferredTaskCount -ne 1) {
        throw 'Health reporter did not compact the maintenance status correctly.'
    }
    if ($report.capabilities.deepIntegrityChecked) {
        throw 'Default health report unexpectedly performed deep archive hashing.'
    }

    'HEALTH_REPORT_CONTRACT_OK'
}
finally {
    if (Test-Path -LiteralPath $fixtureRoot -PathType Container) {
        Remove-ClmDirectoryTreeSafely `
            -TargetPath $fixtureRoot `
            -AllowedRoot $tempRoot `
            -Purpose 'Health report contract fixture cleanup'
    }
}
