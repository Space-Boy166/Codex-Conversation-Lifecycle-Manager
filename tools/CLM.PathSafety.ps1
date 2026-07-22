function Get-ClmNormalizedPath {
    param([Parameter(Mandatory = $true)][string]$Path)

    $full = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetPathRoot($full)
    if ($full.Length -gt $root.Length) {
        return $full.TrimEnd([char[]]@(
                [IO.Path]::DirectorySeparatorChar,
                [IO.Path]::AltDirectorySeparatorChar))
    }
    return $full
}

function Test-ClmStrictDescendant {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Root
    )

    if ([string]::Equals($Path, $Root, [StringComparison]::OrdinalIgnoreCase)) {
        return $false
    }
    $prefix = $Root
    if (-not $prefix.EndsWith([IO.Path]::DirectorySeparatorChar)) {
        $prefix += [IO.Path]::DirectorySeparatorChar
    }
    return $Path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
}

function Assert-ClmRecursiveDeleteTarget {
    param(
        [Parameter(Mandatory = $true)][string]$TargetPath,
        [Parameter(Mandatory = $true)][string]$AllowedRoot,
        [Parameter(Mandatory = $true)][string]$Purpose
    )

    if (-not (Test-Path -LiteralPath $TargetPath -PathType Container)) {
        throw "$Purpose target is not an existing directory: $TargetPath"
    }
    if (-not (Test-Path -LiteralPath $AllowedRoot -PathType Container)) {
        throw "$Purpose allowed root is not an existing directory: $AllowedRoot"
    }
    $target = Get-ClmNormalizedPath -Path (Resolve-Path -LiteralPath $TargetPath).Path
    $allowed = Get-ClmNormalizedPath -Path (Resolve-Path -LiteralPath $AllowedRoot).Path
    if (-not (Test-ClmStrictDescendant -Path $target -Root $allowed)) {
        throw "$Purpose target must be a strict child of $($allowed): $target"
    }
    if ([string]::Equals(
            $target,
            (Get-ClmNormalizedPath -Path ([IO.Path]::GetPathRoot($target))),
            [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Purpose cannot remove a filesystem root."
    }

    $profile = if ($env:USERPROFILE -and (Test-Path -LiteralPath $env:USERPROFILE)) {
        Get-ClmNormalizedPath -Path (Resolve-Path -LiteralPath $env:USERPROFILE).Path
    }
    $tempRoot = if ($env:TEMP -and (Test-Path -LiteralPath $env:TEMP)) {
        Get-ClmNormalizedPath -Path (Resolve-Path -LiteralPath $env:TEMP).Path
    }
    if ($profile -and
        ([string]::Equals($target, $profile, [StringComparison]::OrdinalIgnoreCase) -or
            (Test-ClmStrictDescendant -Path $target -Root $profile))) {
        $insideTemp = $tempRoot -and (Test-ClmStrictDescendant -Path $target -Root $tempRoot)
        $scopeInsideTemp = $tempRoot -and (
            (Test-ClmStrictDescendant -Path $allowed -Root $tempRoot) -or
            ([string]::Equals($allowed, $tempRoot, [StringComparison]::OrdinalIgnoreCase) -and
                [string]::Equals(
                    (Split-Path -Parent $target),
                    $tempRoot,
                    [StringComparison]::OrdinalIgnoreCase) -and
                (Split-Path -Leaf $target).StartsWith('clm-', [StringComparison]::OrdinalIgnoreCase))
        )
        if (-not $insideTemp -or -not $scopeInsideTemp) {
            throw "$Purpose cannot recursively delete inside the user profile outside a CLM-owned TEMP scope: $target"
        }
    }

    $codexHome = if ($env:CODEX_HOME) {
        $env:CODEX_HOME
    }
    elseif ($env:USERPROFILE) {
        Join-Path $env:USERPROFILE '.codex'
    }
    $protectedRoots = @(
        $env:USERPROFILE,
        $codexHome,
        $env:APPDATA,
        $env:LOCALAPPDATA,
        $env:TEMP,
        $env:TMP
    ) | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | ForEach-Object {
        Get-ClmNormalizedPath -Path (Resolve-Path -LiteralPath $_).Path
    } | Select-Object -Unique
    foreach ($protected in $protectedRoots) {
        if ([string]::Equals($target, $protected, [StringComparison]::OrdinalIgnoreCase) -or
            (Test-ClmStrictDescendant -Path $protected -Root $target)) {
            throw "$Purpose target is or contains protected root $($protected): $target"
        }
    }

    foreach ($value in @(
            $env:SystemRoot,
            $env:ProgramData,
            $env:ProgramFiles,
            ${env:ProgramFiles(x86)})) {
        if (-not $value -or -not (Test-Path -LiteralPath $value)) {
            continue
        }
        $protected = Get-ClmNormalizedPath -Path (Resolve-Path -LiteralPath $value).Path
        if ([string]::Equals($target, $protected, [StringComparison]::OrdinalIgnoreCase) -or
            (Test-ClmStrictDescendant -Path $target -Root $protected)) {
            throw "$Purpose cannot recursively delete inside protected system subtree $($protected): $target"
        }
    }
    return $target
}

function Remove-ClmDirectoryTreeSafely {
    param(
        [Parameter(Mandatory = $true)][string]$TargetPath,
        [Parameter(Mandatory = $true)][string]$AllowedRoot,
        [Parameter(Mandatory = $true)][string]$Purpose
    )

    $resolved = Assert-ClmRecursiveDeleteTarget `
        -TargetPath $TargetPath `
        -AllowedRoot $AllowedRoot `
        -Purpose $Purpose
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
