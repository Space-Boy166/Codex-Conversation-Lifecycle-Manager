[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ProxyPath,
    [Parameter(Mandatory = $true)][string]$BackendPath,
    [Parameter(Mandatory = $true)][string]$PythonPath
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $projectRoot 'tools\CLM.PathSafety.ps1')

foreach ($required in @($ProxyPath, $BackendPath, $PythonPath)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Missing skills cache canary dependency: $required"
    }
}

$tempRoot = Get-ClmNormalizedPath -Path (Resolve-Path -LiteralPath $env:TEMP).Path
$fixtureRoot = Join-Path $tempRoot ("clm-skills-cache-{0}" -f [guid]::NewGuid())

try {
    $codexHome = Join-Path $fixtureRoot 'codex-home'
    $runtimeRoot = Join-Path $fixtureRoot 'runtime'
    [void](New-Item -ItemType Directory -Path $codexHome, $runtimeRoot -Force)

    $driver = @'
import json
import os
import queue
import subprocess
import sys
import threading
import time

proxy, backend, runtime, codex_home, cwd = sys.argv[1:6]
env = os.environ.copy()
env["CODEX_HOME"] = codex_home
env["CLM_RUNTIME_ROOT"] = runtime
env["CLM_CODEX_BACKEND"] = backend
process = subprocess.Popen(
    [proxy, "app-server", "--listen", "stdio://"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
    encoding="utf-8",
    bufsize=1,
    env=env,
    cwd=cwd,
)
output = queue.Queue()
errors = []


def read_output():
    for line in process.stdout:
        output.put(line.rstrip("\r\n"))


def read_errors():
    for line in process.stderr:
        errors.append(line.rstrip("\r\n"))


threading.Thread(target=read_output, daemon=True).start()
threading.Thread(target=read_errors, daemon=True).start()


def send(value):
    process.stdin.write(json.dumps(value, separators=(",", ":")) + "\n")
    process.stdin.flush()


def wait_for_id(expected):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        line = output.get(timeout=max(0.01, deadline - time.monotonic()))
        value = json.loads(line)
        if value.get("id") == expected:
            return value
    raise RuntimeError("timed out waiting for response " + str(expected))


try:
    send(
        {
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "clm-skills-cache-canary",
                    "title": "CLM Skills Cache Canary",
                    "version": "0.1.0",
                }
            },
        }
    )
    wait_for_id(1)
    send({"method": "initialized", "params": {}})
    params = {"cwds": [cwd], "forceReload": False}

    started = time.perf_counter()
    send({"id": 2, "method": "skills/list", "params": params})
    first = wait_for_id(2)
    first_ms = (time.perf_counter() - started) * 1000

    started = time.perf_counter()
    send({"id": 3, "method": "skills/list", "params": params})
    second = wait_for_id(3)
    second_ms = (time.perf_counter() - started) * 1000

    if first.get("result") != second.get("result"):
        raise RuntimeError("cached result differs from backend result")
    if second.get("id") != 3:
        raise RuntimeError("cached response did not preserve the caller request id")
finally:
    if process.stdin and not process.stdin.closed:
        process.stdin.close()
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()

cache_lines = [line for line in errors if "CLM skills/list cache" in line]
if not any("cache store" in line for line in cache_lines):
    raise RuntimeError("missing cache store evidence")
if not any("cache hit" in line for line in cache_lines):
    raise RuntimeError("missing cache hit evidence")
if process.returncode != 0:
    raise RuntimeError("proxy exited with " + str(process.returncode))

print(
    json.dumps(
        {
            "state": "skills_cache_canary_ok",
            "firstMs": round(first_ms, 3),
            "secondMs": round(second_ms, 3),
            "resultCwds": len(first.get("result", {}).get("data", [])),
            "cacheStore": True,
            "cacheHit": True,
        },
        separators=(",", ":"),
    )
)
'@

    $priorPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $driverVariable = 'CLM_SKILLS_CACHE_CANARY_DRIVER'
    $hadDriverVariable = Test-Path -LiteralPath "Env:\$driverVariable"
    $previousDriver = [Environment]::GetEnvironmentVariable($driverVariable, 'Process')
    [Environment]::SetEnvironmentVariable($driverVariable, $driver, 'Process')
    try {
        $bootstrap = "import os; exec(os.environ['CLM_SKILLS_CACHE_CANARY_DRIVER'])"
        $output = & $PythonPath -c $bootstrap $ProxyPath $BackendPath $runtimeRoot $codexHome $projectRoot 2>&1
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $priorPreference
        if ($hadDriverVariable) {
            [Environment]::SetEnvironmentVariable($driverVariable, $previousDriver, 'Process')
        }
        else {
            [Environment]::SetEnvironmentVariable($driverVariable, $null, 'Process')
        }
    }
    if ($exitCode -ne 0) {
        throw "Skills cache canary failed ($exitCode):`n$($output | Out-String)"
    }
    $result = [string]($output | Select-Object -Last 1) | ConvertFrom-Json
    if ($result.state -ne 'skills_cache_canary_ok' -or
        -not $result.cacheStore -or
        -not $result.cacheHit -or
        $result.resultCwds -lt 1) {
        throw "Skills cache canary returned an invalid result: $($result | ConvertTo-Json -Compress)"
    }

    $result
}
finally {
    if (Test-Path -LiteralPath $fixtureRoot -PathType Container) {
        Remove-ClmDirectoryTreeSafely `
            -TargetPath $fixtureRoot `
            -AllowedRoot $tempRoot `
            -Purpose 'Skills cache canary cleanup'
    }
}
