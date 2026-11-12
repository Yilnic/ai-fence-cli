<# Isolated installer/lifecycle tests. Scheduled Task cmdlets are mocked. #>
[CmdletBinding()]
param([string]$Python = 'python.exe')
$ErrorActionPreference = 'Stop'
$Python = (Get-Command $Python -ErrorAction Stop).Source
$installer = Join-Path $PSScriptRoot 'install-windows-proxy.ps1'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('af-proxy-test-' + [guid]::NewGuid().ToString('N'))
$null = New-Item -ItemType Directory -Path $fixture
$global:proxyTest = @{Task=$null; Registers=0; Repairs=0; Starts=0; Candidate=$null; Stopping=$false}
$passed = 0

function Assert-Proxy($Condition, [string]$Message) { if (-not $Condition) { throw $Message } }
function Get-ScheduledTask { param($TaskName,$TaskPath,$ErrorAction) return $global:proxyTest.Task }
function New-ScheduledTaskSettingsSet {
    param($RestartCount,$RestartInterval,$ExecutionTimeLimit,$MultipleInstances,
          [switch]$AllowStartIfOnBatteries,[switch]$DontStopIfGoingOnBatteries,[switch]$StartWhenAvailable)
    [pscustomobject]@{RestartCount=$RestartCount; RestartInterval=$RestartInterval;
        ExecutionTimeLimit=$ExecutionTimeLimit; MultipleInstances=$MultipleInstances;
        AllowStartIfOnBatteries=[bool]$AllowStartIfOnBatteries; DontStopIfGoingOnBatteries=[bool]$DontStopIfGoingOnBatteries}
}
function New-ScheduledTaskAction { param($Execute,$Argument,$WorkingDirectory) [pscustomobject]@{Execute=$Execute;Arguments=$Argument;WorkingDirectory=$WorkingDirectory} }
function New-ScheduledTaskTrigger { param([switch]$AtLogOn,$User) [pscustomobject]@{AtLogOn=[bool]$AtLogOn;User=$User} }
function New-ScheduledTaskPrincipal { param($UserId,$LogonType,$RunLevel) [pscustomobject]@{UserId=$UserId;LogonType=$LogonType;RunLevel=$RunLevel} }
function Register-ScheduledTask {
    param($TaskName,$TaskPath,$Action,$Trigger,$Principal,$Settings,$Description)
    $global:proxyTest.Registers++
    $global:proxyTest.Task=[pscustomobject]@{State='Ready';Actions=@($Action);Principal=$Principal;Settings=$Settings;Triggers=@($Trigger)}
}
function Set-ScheduledTask {
    param($TaskName,$TaskPath,$Settings)
    $global:proxyTest.Repairs++
    $global:proxyTest.Task.Settings=$Settings
}
function Start-ScheduledTask { param($TaskName,$TaskPath,$ErrorAction) $global:proxyTest.Starts++ }
function Get-CimInstance { param($ClassName,$Filter,$ErrorAction) return $global:proxyTest.Candidate }
function Start-Process { throw 'Persistent supervisors must not be spawned directly' }
function Start-Sleep { param($Milliseconds) if ($global:proxyTest.Stopping) { $global:proxyTest.Task.State='Ready' } }

try {
    $binary = Join-Path $fixture 'fixture.exe'
    [IO.File]::WriteAllText($binary,'fixture executable; never executed')
    $manifestFile = Join-Path $fixture 'manifest.json'
    $keyFile = Join-Path $fixture 'key-input'
    [IO.File]::WriteAllText($keyFile,'fixture-key')
    $manifest = @{target='windows-x86_64';filename='ai-fence-cli-windows-x86_64.exe';cli_name='ai-fence-cli-dev';
        sha256=(Get-FileHash $binary -Algorithm SHA256).Hash.ToLowerInvariant();size_bytes=(Get-Item $binary).Length}
    [IO.File]::WriteAllText($manifestFile,($manifest|ConvertTo-Json))
    $root = Join-Path $fixture 'install with spaces'
    $inputArgs = @{Root=$root;Python=$Python;CliPath=$binary;ManifestPath=$manifestFile;ApiKeyPath=$keyFile;
        FenceUrl='https://fence.example.test';Channel='dev';NoStart=$true}
    $result = & $installer @inputArgs
    Assert-Proxy ($global:proxyTest.Registers -eq 1 -and $global:proxyTest.Starts -eq 0) 'Fresh install must register exactly one task'
    Assert-Proxy ($global:proxyTest.Task.Settings.RestartCount -eq 3) 'Fresh install must enable failure recovery'
    Assert-Proxy ($global:proxyTest.Task.Settings.RestartInterval.TotalSeconds -eq 60) 'Recovery interval must be one minute'
    Assert-Proxy ($global:proxyTest.Task.Settings.ExecutionTimeLimit.TotalSeconds -eq 0) 'Long-running proxy must have no execution limit'
    Assert-Proxy ($global:proxyTest.Task.Settings.MultipleInstances -eq 'IgnoreNew') 'Duplicate tasks must be prevented'
    Assert-Proxy ($global:proxyTest.Task.Triggers[0].AtLogOn) 'Proxy must recover after sign-in'
    Assert-Proxy ($global:proxyTest.Task.Principal.LogonType -eq 'Interactive' -and $global:proxyTest.Task.Principal.RunLevel -eq 'Limited') 'Proxy must run as the owning user'
    $runtime = Get-Content (Join-Path $root 'runtime-paths.json') -Raw|ConvertFrom-Json
    Assert-Proxy ($runtime.task_name -eq $result.TaskName) 'Launcher must retain its own task identity'
    $passed++

    # Upgrade an old installation with the legacy launcher and no task_name.
    [IO.File]::WriteAllText((Join-Path $root 'runtime-paths.json'),(@{python=$Python}|ConvertTo-Json))
    [IO.File]::WriteAllText((Join-Path $root 'start-dev-proxy.ps1'),'throw "obsolete launcher"')
    [IO.File]::WriteAllText((Join-Path $root 'proxy-state.json'),'{"manager_pid":42,"proxy_pid":43}')
    $global:proxyTest.Task.State='Running'
    $global:proxyTest.Candidate=[pscustomobject]@{ExecutablePath=$Python;CommandLine=$global:proxyTest.Task.Actions[0].Arguments}
    $before = @{}
    foreach ($name in @('api-key','proxy-settings.json','release-manifest.json','proxy-state.json')) {
        $before[$name]=(Get-FileHash (Join-Path $root $name) -Algorithm SHA256).Hash
    }
    $null = & $installer -Root $root -Repair -TaskName $result.TaskName
    Assert-Proxy ($global:proxyTest.Registers -eq 1 -and $global:proxyTest.Repairs -eq 1 -and $global:proxyTest.Starts -eq 0) 'Repair must preserve the healthy task and supervisor'
    foreach ($name in $before.Keys) { Assert-Proxy ((Get-FileHash (Join-Path $root $name) -Algorithm SHA256).Hash -eq $before[$name]) ('Repair changed '+$name) }
    & (Join-Path $root 'start-dev-proxy.ps1')
    Assert-Proxy ($global:proxyTest.Starts -eq 0) 'Legacy updater entry point must preserve a healthy managed task'
    $passed++

    $null = & $installer -Root $root -Repair
    Assert-Proxy ($global:proxyTest.Registers -eq 1 -and $global:proxyTest.Starts -eq 0) 'Repeated repair must be idempotent'
    $passed++

    $global:proxyTest.Task.Principal.UserId=[Security.Principal.WindowsIdentity]::GetCurrent().Name
    $null = & $installer -Root $root -Repair
    Assert-Proxy ($global:proxyTest.Registers -eq 1 -and $global:proxyTest.Starts -eq 0) 'Equivalent account-name and SID principals must be accepted'
    $passed++

    $global:proxyTest.Candidate=$null
    $global:proxyTest.Task.State='Ready'
    [IO.File]::WriteAllText((Join-Path $root 'stop-proxy'),'')
    & (Join-Path $root 'start-proxy.ps1')
    Assert-Proxy ($global:proxyTest.Starts -eq 1 -and -not (Test-Path (Join-Path $root 'stop-proxy'))) 'Stopped service must start through its task and clear its stop request'
    $passed++

    $global:proxyTest.Task.State='Running'
    $global:proxyTest.Stopping=$true
    & (Join-Path $root 'start-proxy.ps1')
    Assert-Proxy ($global:proxyTest.Starts -eq 2) 'A stopping-task race must not discard startup'
    $passed++

    $global:proxyTest.Candidate=[pscustomobject]@{ExecutablePath=$Python;CommandLine=$global:proxyTest.Task.Actions[0].Arguments}
    $rejected=$false
    try { & (Join-Path $root 'start-proxy.ps1') } catch { $rejected=$true }
    Assert-Proxy ($rejected -and $global:proxyTest.Starts -eq 2) 'An unmanaged supervisor must not cause a duplicate start'
    $passed++

    $global:proxyTest.Task.Actions[0].Arguments += ' --unrelated'
    $rejected=$false
    try { $null = & $installer -Root $root -Repair } catch { $rejected=$true }
    Assert-Proxy $rejected 'Repair must refuse an unrelated task action'
    $rejected=$false
    try { & (Join-Path $root 'start-proxy.ps1') } catch { $rejected=$true }
    Assert-Proxy $rejected 'Launcher must refuse an unrelated task action'
    $passed++

    $global:proxyTest.Task.Actions[0].Arguments=$global:proxyTest.Candidate.CommandLine
    $global:proxyTest.Task.Principal.UserId='S-1-5-18'
    $rejected=$false
    try { $null = & $installer -Root $root -Repair } catch { $rejected=$true }
    Assert-Proxy $rejected 'Repair must refuse a task owned by another user'
    $passed++

    $global:proxyTest.Task=$null
    $badArgs=$inputArgs.Clone()
    $badArgs.Root=Join-Path $fixture 'invalid-release'
    [IO.File]::AppendAllText($binary,'tampering')
    $rejected=$false
    try { $null = & $installer @badArgs } catch { $rejected=$true }
    Assert-Proxy ($rejected -and -not (Test-Path $badArgs.Root) -and $global:proxyTest.Registers -eq 1) 'Invalid release must be refused before installation'
    $passed++
    [pscustomobject]@{Tests=$passed;Passed=$passed;RealScheduledTasksModified=$false}|ConvertTo-Json -Compress
} finally {
    Remove-Item -LiteralPath $fixture -Recurse -Force
    Remove-Variable -Name proxyTest -Scope Global
}
