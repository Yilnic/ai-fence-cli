<#
.SYNOPSIS
Install or repair the native Windows subscription proxy's managed lifecycle.
.DESCRIPTION
Use a verified official Windows executable, its release manifest and an API key
file. Does not edit Codex configuration, authentication or history. Persistent
starts always use the same per-user task. See ../WINDOWS_PROXY.md.
#>
[CmdletBinding(DefaultParameterSetName = 'Install')]
param(
    [Parameter(Mandatory)][string]$Root,
    [Parameter(Mandatory, ParameterSetName = 'Install')][string]$Python,
    [Parameter(Mandatory, ParameterSetName = 'Install')][string]$CliPath,
    [Parameter(Mandatory, ParameterSetName = 'Install')][string]$ManifestPath,
    [Parameter(Mandatory, ParameterSetName = 'Install')][string]$ApiKeyPath,
    [Parameter(Mandatory, ParameterSetName = 'Install')][uri]$FenceUrl,
    [Parameter(ParameterSetName = 'Install')][ValidateRange(1024,65535)][int]$Port = 60441,
    [Parameter(ParameterSetName = 'Install')][ValidateSet('dev','prod')][string]$Channel = 'prod',
    [Parameter(Mandatory, ParameterSetName = 'Repair')][switch]$Repair,
    [ValidatePattern('^[A-Za-z0-9 ._-]{1,100}$')][string]$TaskName,
    [switch]$NoStart
)
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'Run this installer in Windows PowerShell.' }
$Root = [IO.Path]::GetFullPath($Root).TrimEnd('\')
if ($Root -notmatch '^[A-Za-z]:\\.+' -or $Root -match '["\r\n]') { throw 'Choose an absolute local Windows installation directory.' }
$assets = Join-Path $PSScriptRoot 'windows-proxy'
$runtimeFile = Join-Path $Root 'runtime-paths.json'
$service = Join-Path $Root 'proxy_service.py'
$arguments = '"{0}" --root "{1}"' -f $service, $Root
$user = [Security.Principal.WindowsIdentity]::GetCurrent()
$runtime = $null
if ($Repair) {
    $runtime = Get-Content -LiteralPath $runtimeFile -Raw | ConvertFrom-Json
    $Python = $runtime.python
    if (-not $TaskName) { $TaskName = $runtime.task_name }
    if (-not $TaskName) { throw 'Supply -TaskName once to adopt the existing proxy task.' }
} else {
    if (Test-Path -LiteralPath $runtimeFile) { throw 'This installation already exists. Use -Repair to retain its configuration.' }
    if ($FenceUrl.Scheme -ne 'https' -or $FenceUrl.UserInfo -or $FenceUrl.Query -or $FenceUrl.Fragment) { throw 'FenceUrl must be an HTTPS URL without credentials, query or fragment.' }
    $manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
    $hash = (Get-FileHash -LiteralPath $CliPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $expectedName = if ($Channel -eq 'dev') { 'ai-fence-cli-dev' } else { 'ai-fence-cli' }
    if ($manifest.target -ne 'windows-x86_64' -or $manifest.filename -ne 'ai-fence-cli-windows-x86_64.exe' -or
        $manifest.cli_name -ne $expectedName -or $manifest.sha256 -ne $hash -or
        $manifest.size_bytes -ne (Get-Item -LiteralPath $CliPath).Length) { throw 'Windows executable does not match the selected channel release manifest.' }
    $apiKey = [IO.File]::ReadAllText((Resolve-Path -LiteralPath $ApiKeyPath).Path).Trim()
    if (-not $apiKey -or $apiKey -match '[\r\n]') { throw 'The API key file must contain one nonempty key.' }
    if (-not $TaskName) {
        $hasher = [Security.Cryptography.SHA256]::Create()
        try { $rootHash = [BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($Root.ToLowerInvariant()))).Replace('-','').Substring(0,12) }
        finally { $hasher.Dispose() }
        $TaskName = 'AI Fence Proxy ' + $rootHash
    }
}
$Python = (Resolve-Path -LiteralPath $Python -ErrorAction Stop).Path
if ($Python -match '["\r\n]') { throw 'Invalid Python executable path.' }
$pythonVersion = & $Python -c 'import sys; print(int(sys.version_info >= (3, 11)))'
if ($LASTEXITCODE -ne 0 -or $pythonVersion -ne '1') { throw 'Python 3.11 or newer is required.' }
$task = Get-ScheduledTask -TaskName $TaskName -TaskPath '\' -ErrorAction SilentlyContinue
if ($task) {
    if (-not $Repair -or @($task.Actions).Count -ne 1 -or $task.Actions[0].Execute -ne $Python -or
        $task.Actions[0].Arguments -ne $arguments -or $task.Actions[0].WorkingDirectory -ne $Root) {
        throw 'Refusing to replace a task that does not belong to this installation.'
    }
    $principal = $task.Principal.UserId
    if ($principal -match '^S-1-') { $principalSid = [Security.Principal.SecurityIdentifier]$principal }
    else { $principalSid = ([Security.Principal.NTAccount]$principal).Translate([Security.Principal.SecurityIdentifier]) }
    if ($principalSid.Value -ne $user.User.Value) { throw 'The proxy task belongs to another Windows user.' }
} elseif ($Repair) {
    throw 'The existing task is missing. Inspect the installation before registering a replacement.'
}

function Write-ProxyFile([string]$Path, [byte[]]$Bytes) {
    $temporary = $Path + '.install-' + [guid]::NewGuid().ToString('N')
    $backup = $temporary + '.previous'
    try {
        [IO.File]::WriteAllBytes($temporary, $Bytes)
        if (Test-Path -LiteralPath $Path) { [IO.File]::Replace($temporary, $Path, $backup) }
        else { [IO.File]::Move($temporary, $Path) }
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary }
        if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup }
    }
}
function Write-ProxyJson([string]$Path, $Value) {
    Write-ProxyFile $Path ([Text.Encoding]::UTF8.GetBytes(($Value | ConvertTo-Json -Depth 8)))
}

if (-not $Repair) {
    if (Test-Path -LiteralPath $Root) {
        if (@(Get-ChildItem -LiteralPath $Root -Force).Count) { throw 'Choose an empty directory for a new proxy installation.' }
    } else { $null = New-Item -ItemType Directory -Path $Root }
    # API keys and runtime metadata inherit an owner-only directory ACL.
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetOwner($user.User)
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @($user.User, [Security.Principal.SecurityIdentifier]'S-1-5-18')) {
        $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($sid,'FullControl','ContainerInherit,ObjectInherit','None','Allow')))
    }
    Set-Acl -LiteralPath $Root -AclObject $acl
    $release = Join-Path $Root ('releases\' + $hash)
    $null = New-Item -ItemType Directory -Path $release -Force
    Write-ProxyFile (Join-Path $release $manifest.filename) ([IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $CliPath).Path))
    if ((Get-FileHash -LiteralPath (Join-Path $release $manifest.filename) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $hash) {
        throw 'The executable changed while it was being installed.'
    }
    $manifest.filename = 'releases/' + $hash + '/' + $manifest.filename
    Write-ProxyJson (Join-Path $Root 'release-manifest.json') $manifest
    Write-ProxyJson (Join-Path $Root 'proxy-settings.json') @{
        channel=$Channel; fence_url=$FenceUrl.AbsoluteUri.TrimEnd('/'); port=$Port; local_url="http://127.0.0.1:$Port/v1"
    }
    Write-ProxyFile (Join-Path $Root 'api-key') ([Text.Encoding]::UTF8.GetBytes($apiKey))
}

# Updating a running task's recovery settings does not stop its current action.
$settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries -StartWhenAvailable
if ($task) {
    $null = Set-ScheduledTask -TaskName $TaskName -TaskPath '\' -Settings $settings
} else {
    $action = New-ScheduledTaskAction -Execute $Python -Argument $arguments -WorkingDirectory $Root
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user.Name
    $principal = New-ScheduledTaskPrincipal -UserId $user.User.Value -LogonType Interactive -RunLevel Limited
    $null = Register-ScheduledTask -TaskName $TaskName -TaskPath '\' -Action $action -Trigger $trigger `
        -Principal $principal -Settings $settings -Description 'AI Fence native Windows proxy with automatic failure recovery.'
}
Write-ProxyFile $service ([IO.File]::ReadAllBytes((Join-Path $assets 'proxy_service.py')))
Write-ProxyFile (Join-Path $Root 'start-proxy.ps1') ([IO.File]::ReadAllBytes((Join-Path $assets 'start-proxy.ps1')))
if ($Repair -and (Test-Path -LiteralPath (Join-Path $Root 'start-dev-proxy.ps1'))) {
    # Older updaters use this filename. Keep it on the same managed start path.
    Write-ProxyFile (Join-Path $Root 'start-dev-proxy.ps1') ([IO.File]::ReadAllBytes((Join-Path $assets 'start-proxy.ps1')))
}
Write-ProxyJson $runtimeFile @{python=$Python; task_name=$TaskName; lifecycle_version=1}
if (-not $NoStart) { & (Join-Path $Root 'start-proxy.ps1') }
[pscustomobject]@{Root=$Root; TaskName=$TaskName; Repaired=[bool]$Repair; StartRequested=(-not $NoStart); RestartCount=3; RestartInterval='PT1M'}
