[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$proxyRoot = $PSScriptRoot
$runtime = Get-Content -LiteralPath (Join-Path $proxyRoot 'runtime-paths.json') -Raw | ConvertFrom-Json
$service = Join-Path $proxyRoot 'proxy_service.py'
$arguments = '"{0}" --root "{1}"' -f $service, $proxyRoot
if (-not $runtime.task_name) { throw 'Run install-windows-proxy.ps1 -Repair to register the managed task.' }
$task = Get-ScheduledTask -TaskName $runtime.task_name -TaskPath '\' -ErrorAction Stop
if (@($task.Actions).Count -ne 1 -or $task.Actions[0].Execute -ne $runtime.python -or
    $task.Actions[0].Arguments -ne $arguments -or $task.Actions[0].WorkingDirectory -ne $proxyRoot) {
    throw 'The registered proxy task does not match this installation.'
}
$existing = $null
$stateFile = Join-Path $proxyRoot 'proxy-state.json'
if (Test-Path -LiteralPath $stateFile) {
    $state = Get-Content -LiteralPath $stateFile -Raw | ConvertFrom-Json
    $candidate = Get-CimInstance Win32_Process -Filter "ProcessId = $([int]$state.manager_pid)" -ErrorAction SilentlyContinue
    if ($candidate -and $candidate.ExecutablePath -eq $runtime.python -and
        $candidate.CommandLine.EndsWith($arguments, [StringComparison]::OrdinalIgnoreCase)) {
        $existing = $candidate
    }
}
if ($existing) {
    if ([string]$task.State -ne 'Running') { throw 'An unmanaged proxy supervisor is active; use a managed cutover before starting.' }
    return
}
# IgnoreNew can otherwise discard the start just after a cooperative stop.
$deadline = (Get-Date).AddSeconds(8)
while ([string]$task.State -eq 'Running' -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 200
    $task = Get-ScheduledTask -TaskName $runtime.task_name -TaskPath '\' -ErrorAction Stop
}
if ([string]$task.State -eq 'Running') { throw 'The previous proxy task is still stopping.' }
$stopFile = Join-Path $proxyRoot 'stop-proxy'
if (Test-Path -LiteralPath $stopFile) { Remove-Item -LiteralPath $stopFile }
Start-ScheduledTask -TaskName $runtime.task_name -TaskPath '\' -ErrorAction Stop
