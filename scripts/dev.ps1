#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = Resolve-Path "$repoRoot/.."

function Cleanup {
    param($processes)
    foreach ($process in $processes) {
        if ($null -ne $process -and -not $process.HasExited) {
            try { $process.Kill() } catch { }
        }
    }
}

$server = Start-Process cargo -ArgumentList "run", "-p", "twi-overlay-app" -WorkingDirectory $repoRoot -PassThru
$webWorkingDirectory = Join-Path $repoRoot "web/overlay"
if (-Not (Test-Path (Join-Path $webWorkingDirectory "node_modules"))) {
    Start-Process npm -ArgumentList "install" -WorkingDirectory $webWorkingDirectory -Wait | Out-Null
}
$web = Start-Process npm -ArgumentList "run", "dev", "--", "--host" -WorkingDirectory $webWorkingDirectory -PassThru

try {
    Wait-Process -Id @($server.Id, $web.Id)
}
finally {
    Cleanup @($server, $web)
}
