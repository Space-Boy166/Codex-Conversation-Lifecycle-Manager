[CmdletBinding()]
param(
    [string]$RuntimeRoot = $(if ($env:CLM_RUNTIME_ROOT) {
            $env:CLM_RUNTIME_ROOT
        } else {
            Join-Path $env:LOCALAPPDATA 'ConversationLifecycleManager'
        }),
    [string]$CodexHome = (Join-Path $env:USERPROFILE '.codex'),
    [string]$CodexLogRoot = (Join-Path $env:LOCALAPPDATA 'Packages\OpenAI.Codex_2p2nqsd0c76g0\LocalCache\Local\Codex\Logs'),
    [string]$ShortcutPath = (Join-Path ([Environment]::GetFolderPath('Desktop')) 'ChatGPT.lnk'),
    [int]$TopTaskCount = 10,
    [switch]$DeepIntegrity,
    [switch]$AsJson
)

$ErrorActionPreference = 'Stop'
$thresholdBytes = 64MB
$manifestRoot = Join-Path $RuntimeRoot 'Data\Vault\Codex'
$indexRoot = Join-Path $RuntimeRoot 'Data\Indexes'
$exitRoot = Join-Path $RuntimeRoot 'Work\ExitMaintenance'
$markerPath = Join-Path $exitRoot 'session-active.json'
$installStatePath = Join-Path $exitRoot 'install-state.json'
$latestMaintenancePath = Join-Path $exitRoot 'latest-status.json'

function ConvertTo-ProviderPath {
    param([AllowNull()][string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $null
    }
    if ($Path.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
        return '\\' + $Path.Substring(8)
    }
    if ($Path.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) {
        return $Path.Substring(4)
    }
    return $Path
}

function Read-JsonFile {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    try {
        return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    }
    catch {
        return [pscustomobject]@{
            parseError = $_.Exception.Message
            path = $Path
        }
    }
}

function Limit-Text {
    param(
        [AllowNull()][string]$Text,
        [int]$MaximumLength = 160
    )

    if ([string]::IsNullOrEmpty($Text) -or $Text.Length -le $MaximumLength) {
        return $Text
    }
    return $Text.Substring(0, $MaximumLength) + '...'
}

function Open-SharedTextReader {
    param([Parameter(Mandatory = $true)][string]$Path)

    $share = [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
    $stream = [IO.File]::Open(
        $Path,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        $share
    )
    try {
        return [IO.StreamReader]::new($stream, [Text.Encoding]::UTF8, $true, 4096, $false)
    }
    catch {
        $stream.Dispose()
        throw
    }
}

function Get-MaintenanceSummary {
    param([AllowNull()]$State)

    if ($null -eq $State) {
        return $null
    }
    if ($State.PSObject.Properties.Name -contains 'parseError') {
        return $State
    }
    $tasks = @($State.tasks)
    return [pscustomobject]@{
        updatedAt = $State.updatedAt
        state = $State.state
        detail = $State.detail
        mode = $State.mode
        attemptId = $State.attemptId
        taskCount = $tasks.Count
        failedTaskCount = @($tasks | Where-Object { [string]$_.state -like '*failed*' }).Count
        deferredTaskCount = @($tasks | Where-Object { [string]$_.state -like 'deferred_*' }).Count
    }
}

function Get-ShortcutTarget {
    param([Parameter(Mandatory = $true)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    try {
        $shell = New-Object -ComObject WScript.Shell
        return [string]$shell.CreateShortcut($Path).TargetPath
    }
    catch {
        return $null
    }
}

function Get-TitleMap {
    param([Parameter(Mandatory = $true)][string]$IndexPath)

    $map = @{}
    if (-not (Test-Path -LiteralPath $IndexPath -PathType Leaf)) {
        return $map
    }
    $reader = Open-SharedTextReader -Path $IndexPath
    try {
        for ($line = $reader.ReadLine(); $null -ne $line; $line = $reader.ReadLine()) {
            try {
                $entry = $line | ConvertFrom-Json
                if ($entry.id -and $entry.thread_name) {
                    $map[[string]$entry.id] = [string]$entry.thread_name
                }
            }
            catch {
                continue
            }
        }
    }
    finally {
        $reader.Dispose()
    }
    return $map
}

function Get-LogMetrics {
    param([AllowNull()][IO.FileInfo]$LogFile)

    $durations = New-Object Collections.Generic.List[long]
    $unknownByThread = @{}
    $counts = [ordered]@{
        unknownConversation = 0
        skillsList = 0
        skillsListCacheHit = 0
        skillsListCacheStore = 0
        reviewCapture = 0
        gitUnavailable = 0
        turnsList = 0
        resume = 0
        clmDrainBlocked = 0
    }
    if ($null -eq $LogFile) {
        return [pscustomobject]@{
            path = $null
            bytes = 0L
            lastWriteTime = $null
            counts = [pscustomobject]$counts
            skillsListDurationMs = $null
            topUnknownThreads = @()
        }
    }

    $reader = Open-SharedTextReader -Path $LogFile.FullName
    try {
        for ($line = $reader.ReadLine(); $null -ne $line; $line = $reader.ReadLine()) {
            if ($line.IndexOf('unknown conversation', [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                $counts.unknownConversation++
                $match = [regex]::Match($line, 'conversationId=([^\s]+)')
                if ($match.Success) {
                    $id = $match.Groups[1].Value
                    $unknownByThread[$id] = 1 + [int]($unknownByThread[$id])
                }
            }
            if ($line.IndexOf('method=skills/list', [StringComparison]::Ordinal) -ge 0) {
                $counts.skillsList++
                $match = [regex]::Match($line, 'durationMs=(\d+)')
                if ($match.Success) {
                    $durations.Add([long]$match.Groups[1].Value)
                }
            }
            if ($line.IndexOf('CLM skills/list cache hit', [StringComparison]::Ordinal) -ge 0) {
                $counts.skillsListCacheHit++
            }
            if ($line.IndexOf('CLM skills/list cache store', [StringComparison]::Ordinal) -ge 0) {
                $counts.skillsListCacheStore++
            }
            if ($line.IndexOf('turn-diff-capture-start', [StringComparison]::Ordinal) -ge 0) {
                $counts.reviewCapture++
            }
            if ($line.IndexOf('Git is unavailable', [StringComparison]::Ordinal) -ge 0) {
                $counts.gitUnavailable++
            }
            if ($line.IndexOf('method=thread/turns/list', [StringComparison]::Ordinal) -ge 0) {
                $counts.turnsList++
            }
            if ($line.IndexOf('method=thread/resume', [StringComparison]::Ordinal) -ge 0) {
                $counts.resume++
            }
            if ($line.IndexOf('CLM stopped Codex', [StringComparison]::Ordinal) -ge 0 -or
                $line.IndexOf('errorCode=-32072', [StringComparison]::Ordinal) -ge 0) {
                $counts.clmDrainBlocked++
            }
        }
    }
    finally {
        $reader.Dispose()
    }

    $orderedDurations = @($durations | Sort-Object)
    $durationSummary = if ($orderedDurations.Count -eq 0) {
        $null
    }
    else {
        $p95Index = [Math]::Min(
            $orderedDurations.Count - 1,
            [Math]::Max(0, [Math]::Ceiling($orderedDurations.Count * 0.95) - 1)
        )
        [pscustomobject]@{
            count = $orderedDurations.Count
            minimum = $orderedDurations[0]
            median = $orderedDurations[[Math]::Floor(($orderedDurations.Count - 1) / 2)]
            p95 = $orderedDurations[$p95Index]
            maximum = $orderedDurations[-1]
        }
    }
    $topUnknown = @($unknownByThread.GetEnumerator() |
        Sort-Object Value -Descending |
        Select-Object -First 10 |
        ForEach-Object {
            [pscustomobject]@{
                threadId = [string]$_.Key
                count = [int]$_.Value
            }
        })
    $logSnapshot = Get-Item -LiteralPath $LogFile.FullName

    return [pscustomobject]@{
        path = $LogFile.FullName
        bytes = [long]$logSnapshot.Length
        lastWriteTime = $logSnapshot.LastWriteTime.ToString('o')
        counts = [pscustomobject]$counts
        skillsListDurationMs = $durationSummary
        topUnknownThreads = $topUnknown
    }
}

$titles = Get-TitleMap -IndexPath (Join-Path $CodexHome 'session_index.jsonl')
$manifestErrors = New-Object Collections.Generic.List[object]
$managedTasks = New-Object Collections.Generic.List[object]
if (Test-Path -LiteralPath $manifestRoot -PathType Container) {
    foreach ($directory in @(Get-ChildItem -LiteralPath $manifestRoot -Directory)) {
        if ($directory.Name -match '\.clm-cycle-\d+$') {
            continue
        }
        $manifestPath = Join-Path $directory.FullName 'manifest.json'
        if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
            continue
        }
        try {
            $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
            $threadId = [string]$manifest.threadId
            $activePath = ConvertTo-ProviderPath -Path ([string]$manifest.originalPath)
            $archivePath = ConvertTo-ProviderPath -Path ([string]$manifest.archivePath)
            $rollbackPath = ConvertTo-ProviderPath -Path ([string]$manifest.rollbackPath)
            $managedIndexPath = ConvertTo-ProviderPath -Path ([string]$manifest.indexPath)
            $activeItem = Get-Item -LiteralPath $activePath -ErrorAction SilentlyContinue
            $archiveItem = Get-Item -LiteralPath $archivePath -ErrorAction SilentlyContinue
            $rollbackItem = Get-Item -LiteralPath $rollbackPath -ErrorAction SilentlyContinue
            $indexItem = Get-Item -LiteralPath $managedIndexPath -ErrorAction SilentlyContinue
            $activeBytes = if ($activeItem) { [long]$activeItem.Length } else { $null }
            $candidateBytes = [long]$manifest.candidateBytes
            $archiveHashMatches = $null
            if ($DeepIntegrity -and $archiveItem) {
                $archiveHashMatches = [string]::Equals(
                    (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash,
                    [string]$manifest.sourceSha256,
                    [StringComparison]::OrdinalIgnoreCase
                )
            }
            $managedTasks.Add([pscustomobject]@{
                    threadId = $threadId
                    title = if ($titles.ContainsKey($threadId)) {
                        Limit-Text -Text $titles[$threadId]
                    } else { $null }
                    activeBytes = $activeBytes
                    activationBytes = $candidateBytes
                    growthBytes = if ($null -ne $activeBytes) {
                        [Math]::Max(0L, $activeBytes - $candidateBytes)
                    } else { $null }
                    aboveMaintenanceThreshold = $null -ne $activeBytes -and $activeBytes -ge $thresholdBytes
                    activeExists = $null -ne $activeItem
                    archiveExists = $null -ne $archiveItem
                    archiveBytes = if ($archiveItem) { [long]$archiveItem.Length } else { $null }
                    archiveHashMatches = $archiveHashMatches
                    rollbackExists = $null -ne $rollbackItem
                    indexExists = $null -ne $indexItem
                    manifestPath = $manifestPath
                    activePath = $activePath
                })
        }
        catch {
            $manifestErrors.Add([pscustomobject]@{
                    manifestPath = $manifestPath
                    error = $_.Exception.Message
                })
        }
    }
}

$managed = $managedTasks.ToArray()
$largestTasks = @($managed |
    Sort-Object @{ Expression = { if ($null -eq $_.activeBytes) { -1 } else { $_.activeBytes } }; Descending = $true } |
    Select-Object -First $TopTaskCount)
$aboveThreshold = @($managed | Where-Object aboveMaintenanceThreshold)
$missingArchive = @($managed | Where-Object { -not $_.archiveExists })
$missingRollback = @($managed | Where-Object { -not $_.rollbackExists })
$missingIndex = @($managed | Where-Object { -not $_.indexExists })

$allProcesses = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue)
$codexProcesses = @($allProcesses | Where-Object {
        $_.Name -in @('ChatGPT.exe', 'codex.exe', 'codex-clm-proxy.exe', 'codex-code-mode-host.exe')
    })
$rendererProcesses = @($codexProcesses | Where-Object {
        $_.Name -eq 'ChatGPT.exe' -and [string]$_.CommandLine -match '--type=renderer'
    })
$proxyProcesses = @($codexProcesses | Where-Object Name -eq 'codex-clm-proxy.exe')
$backendProcesses = @($codexProcesses | Where-Object Name -eq 'codex.exe')
$desktopRoots = @($codexProcesses | Where-Object {
        $_.Name -eq 'ChatGPT.exe' -and [string]$_.CommandLine -notmatch '--type='
    })
$workingSet = 0L
foreach ($processId in @($codexProcesses.ProcessId)) {
    $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
    if ($process) {
        $workingSet += [long]$process.WorkingSet64
    }
}

$latestLog = if (Test-Path -LiteralPath $CodexLogRoot -PathType Container) {
    Get-ChildItem -LiteralPath $CodexLogRoot -Filter '*.log' -File -Recurse -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending |
        Select-Object -First 1
} else { $null }
$logMetrics = Get-LogMetrics -LogFile $latestLog
$shortcutTarget = Get-ShortcutTarget -Path $ShortcutPath
$installState = Read-JsonFile -Path $installStatePath
$maintenanceState = Read-JsonFile -Path $latestMaintenancePath
$maintenanceSummary = Get-MaintenanceSummary -State $maintenanceState
$lifecycleInstalled = $shortcutTarget -like '*CodexClmLifecycleLauncher.exe'
$pagingProxyActive = $proxyProcesses.Count -eq 1 -and $backendProcesses.Count -eq 1

$warnings = New-Object Collections.Generic.List[string]
if (-not $pagingProxyActive) {
    $warnings.Add("Expected one CLM proxy and one backend; observed $($proxyProcesses.Count) proxy and $($backendProcesses.Count) backend process(es).")
}
if (-not $lifecycleInstalled) {
    $warnings.Add('Exit lifecycle is not installed on the desktop shortcut; managed tails will not refresh automatically after exit.')
}
if ($aboveThreshold.Count -gt 0) {
    $warnings.Add("$($aboveThreshold.Count) managed active tail(s) are at or above 64 MiB.")
}
if ($missingArchive.Count -gt 0 -or $missingIndex.Count -gt 0) {
    $warnings.Add("Managed evidence is incomplete: missing archive=$($missingArchive.Count), missing index=$($missingIndex.Count).")
}
if ($missingRollback.Count -gt 0) {
    $warnings.Add("$($missingRollback.Count) previous same-volume rollback(s) are absent; verified refresh can re-establish them only during an offline transaction.")
}
if ($maintenanceState -and [string]$maintenanceState.state -like '*failed*') {
    $warnings.Add("Latest exit maintenance state is $($maintenanceState.state).")
}
if ($logMetrics.counts.unknownConversation -gt 100) {
    $warnings.Add("Current Store log contains $($logMetrics.counts.unknownConversation) cross-renderer unknown-conversation deliveries.")
}
if ($logMetrics.skillsListDurationMs -and $logMetrics.skillsListDurationMs.p95 -gt 1000) {
    $warnings.Add("skills/list p95 is $($logMetrics.skillsListDurationMs.p95) ms.")
}
if ($logMetrics.counts.reviewCapture -gt 0) {
    $warnings.Add("Current Store log still contains $($logMetrics.counts.reviewCapture) Review capture calls.")
}

$state = if (-not $pagingProxyActive) {
    'runtime_not_effective'
}
elseif (-not $lifecycleInstalled) {
    'paging_effective_lifecycle_disarmed'
}
elseif ($aboveThreshold.Count -gt 0) {
    'paging_effective_tail_maintenance_needed'
}
else {
    'paging_and_lifecycle_effective'
}

$report = [pscustomobject]@{
    schemaVersion = 1
    generatedAt = (Get-Date).ToString('o')
    state = $state
    capabilities = [pscustomobject]@{
        pagingProxyActive = $pagingProxyActive
        lifecycleInstalled = $lifecycleInstalled
        sessionMarkerPresent = Test-Path -LiteralPath $markerPath -PathType Leaf
        deepIntegrityChecked = [bool]$DeepIntegrity
    }
    runtime = [pscustomobject]@{
        root = $RuntimeRoot
        desktopRootCount = $desktopRoots.Count
        proxyCount = $proxyProcesses.Count
        backendCount = $backendProcesses.Count
        rendererCount = $rendererProcesses.Count
        codexProcessCount = $codexProcesses.Count
        codexWorkingSetBytes = $workingSet
        liveGitCount = @(Get-Process git -ErrorAction SilentlyContinue).Count
        shortcutPath = $ShortcutPath
        shortcutTarget = $shortcutTarget
        installState = $installState
        latestMaintenance = $maintenanceSummary
    }
    managed = [pscustomobject]@{
        taskCount = $managed.Count
        activeBytesTotal = [long](($managed | Measure-Object activeBytes -Sum).Sum)
        aboveThresholdCount = $aboveThreshold.Count
        missingArchiveCount = $missingArchive.Count
        missingRollbackCount = $missingRollback.Count
        missingIndexCount = $missingIndex.Count
        manifestErrorCount = $manifestErrors.Count
        largestTasks = $largestTasks
        manifestErrors = $manifestErrors.ToArray()
    }
    storeLog = $logMetrics
    warnings = $warnings.ToArray()
}

if ($AsJson) {
    $report | ConvertTo-Json -Depth 12
}
else {
    $report
}
