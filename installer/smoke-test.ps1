# Installs Screen Time Manager with the built setup and checks the install: the exe, the three
# scheduled tasks (and that standard accounts may run the main and update tasks but not change
# them), the shared config folder's permissions and the update marker. Then it reinstalls as SYSTEM
# over a running copy (which must replace a 1.0.1-style descriptor on the main task), checks the
# update task can be started by an administrator, and the update and main tasks by a standard
# account (which must not be able to disable or delete any task), updates through the app's own
# updater from a fake release served on localhost, checks the installed watchdog brings the app
# back after that update and after a kill (when the runner has a signed-in session for it to run
# in), and uninstalls.
#
#   installer\build.ps1 -Bin ..\target\debug
#   installer\smoke-test.ps1 [-Bin ..\target\debug]
#
# -Bin is the folder build.ps1 packaged (relative to this folder); it must be a debug build, as
# only those read STM_UPDATE_FEED. The update is a second setup built here from the same exe with
# the next patch version (build.ps1 -Version), so the version that changes is the installed one
# (Windows' uninstall entry), not the exe's own.
#
# CI only, on an administrator Windows runner: it installs into Program Files, registers and
# removes scheduled tasks, closes every running copy, and adds and removes a local account.
param([string]$Bin = '..\target\debug')

$ErrorActionPreference = 'Stop'
if (!$env:CI) { throw 'Run this on CI only: it installs Screen Time Manager for every account and adds a local account.' }
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (!([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this as an administrator, as setup needs.'
}

$root = (Resolve-Path "$PSScriptRoot\..").Path
$dir = "$env:ProgramFiles\ScreenTimeManager"
$exe = "$dir\screen-time-manager.exe"
$marker = "$dir\updating.marker"
$sharedDir = "$env:ProgramData\ScreenTimeManager"
$userDataDir = "$env:LOCALAPPDATA\.screen-time-manager"
$uninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{1D3CBE0B-2B68-4A1E-A8F4-96EE93D3D792}_is1'
$mainTask = 'ScreenTimeManager'
$watchdogTask = 'ScreenTimeManagerWatchdog'
$updateTask = 'ScreenTimeManagerUpdate'
$tasks = $mainTask, $watchdogTask, $updateTask
$usersSid = 'S-1-5-32-545'
$systemSid = 'S-1-5-18'
$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$tmp = Join-Path $tempRoot 'stm-smoke-test'
Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $tmp | Out-Null

function Check([bool]$Ok, [string]$Message) {
    if (!$Ok) { throw $Message }
}

# The version setup is named after: the exe's product version, without build.rs's fourth ".0".
function VersionOf([string]$Path) {
    $v = (Get-Item $Path).VersionInfo.ProductVersion
    if ($v -match '^(\d+\.\d+\.\d+)\.0$') { $Matches[1] } else { $v }
}

function InstalledVersion {
    (Get-ItemProperty $uninstallKey -ErrorAction SilentlyContinue).DisplayVersion
}

function AppProcesses {
    @(Get-Process screen-time-manager -ErrorAction SilentlyContinue)
}

# The app itself (not a --watchdog-check or --update run) in session $Session.
function AppInSession([int]$Session) {
    @(Get-CimInstance Win32_Process -Filter "Name = 'screen-time-manager.exe'" | Where-Object {
            $_.SessionId -eq $Session -and $_.CommandLine -notmatch '--watchdog-check|--update'
        })
}

# One line per running screen-time-manager.exe: pid, session, account, arguments.
function AppProcessSummary {
    $procs = @(Get-CimInstance Win32_Process -Filter "Name = 'screen-time-manager.exe'")
    if (!$procs.Count) { return 'no screen-time-manager.exe running' }
    ($procs | ForEach-Object {
            $owner = Invoke-CimMethod -InputObject $_ -MethodName GetOwner -ErrorAction SilentlyContinue
            "pid $($_.ProcessId) session $($_.SessionId) $($owner.Domain)\$($owner.User) [$($_.CommandLine)]"
        }) -join '; '
}

# This account's watchdog.log lines written at or after $Since (each starts with [unix seconds]).
function WatchdogLogSince([datetime]$Since) {
    $log = "$userDataDir\watchdog.log"
    if (!(Test-Path $log)) { return @() }
    $from = ([DateTimeOffset]$Since).ToUnixTimeSeconds() - 1
    @(Get-Content $log | Where-Object { $_ -match '^\[(\d+)\]' -and [int64]$Matches[1] -ge $from })
}

# Waits for the installed watchdog task, by itself, to bring the app back in this runner's session
# after $Since, and checks that it survives the watchdog run that started it (1.0.1's fallback
# started it as a child of that run, which ended it). $LogPattern must match the watchdog.log line
# for the relaunch. Returns $false if the watchdog task never ran here, so this can't be checked.
function CheckWatchdogRelaunch([int]$Session, [datetime]$Since, [string]$LogPattern, [string]$What) {
    $deadline = (Get-Date).AddSeconds(180)
    $app = $null
    while (!$app -and (Get-Date) -lt $deadline) {
        $app = @(AppInSession $Session) | Select-Object -First 1
        if (!$app) { Start-Sleep 2 }
    }
    if (!$app) {
        if ((Get-ScheduledTaskInfo -TaskName $watchdogTask).LastRunTime -lt $Since) { return $false }
        ShowFile "$userDataDir\watchdog.log"
        throw "$What`: the watchdog ran but the app was not running in session $Session 3 minutes later ($(AppProcessSummary))"
    }
    # The watchdog checks that started before the app; the one that relaunched it is among them.
    $started = $app.CreationDate
    for ($i = 0; $i -lt 90; $i++) {
        $checks = @(Get-CimInstance Win32_Process -Filter "Name = 'screen-time-manager.exe'" | Where-Object {
                $_.CommandLine -match '--watchdog-check' -and $_.CreationDate -le $started
            })
        if (!$checks.Count) { break }
        Start-Sleep 1
    }
    Start-Sleep 5
    $still = Get-Process -Id $app.ProcessId -ErrorAction SilentlyContinue
    if (!$still) { ShowFile "$userDataDir\watchdog.log" }
    Check ($null -ne $still) "$What`: the app the watchdog started (pid $($app.ProcessId)) was gone once the watchdog run had finished"
    $parent = Get-CimInstance Win32_Process -Filter "ProcessId = $($app.ParentProcessId)"
    Check (!$parent -or $parent.Name -ne 'screen-time-manager.exe') "$What`: the app was started as a child of $($parent.CommandLine), not by Task Scheduler"
    $lines = @(WatchdogLogSince $Since | Where-Object { $_ -match $LogPattern })
    if (!$lines.Count) { ShowFile "$userDataDir\watchdog.log" }
    Check ($lines.Count -gt 0) "$What`: watchdog.log has no line matching '$LogPattern' since $Since"
    "$What`: the watchdog relaunched the app (pid $($app.ProcessId), session $Session, parent $($parent.Name)); it logged: $($lines[-1])" | Out-Host
    $true
}

function ShowFile([string]$Path) {
    if (Test-Path $Path) {
        "---- $Path" | Out-Host
        Get-Content $Path -Tail 80 | Out-Host
    }
}

$script:logs = 0
function NewLog {
    $script:logs++
    "$tmp\setup-$script:logs.log"
}

# Runs setup silently as this administrator. It must finish by itself: nothing may wait on a user.
function Install([string]$Setup) {
    $log = NewLog
    $p = Start-Process $Setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$log`"" -PassThru
    $null = $p.Handle # keeps the exit code readable
    if (!$p.WaitForExit(300000)) { ShowFile $log; throw 'Setup was still running after 5 minutes' }
    if ($p.ExitCode) { ShowFile $log; throw "Setup exited with code $($p.ExitCode)" }
}

# Waits for a run of a task that started at or after $Since to finish, and returns its info.
function WaitForTaskRun([string]$Name, [datetime]$Since, [int]$Seconds) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $info = Get-ScheduledTaskInfo -TaskName $Name
        $state = (Get-ScheduledTask -TaskName $Name).State
        # 0x41301: the task is running.
        if ($info.LastRunTime -ge $Since.AddSeconds(-1) -and $state -ne 'Running' -and $info.LastTaskResult -ne 0x41301) {
            return $info
        }
        Start-Sleep 1
    }
    throw "Task $Name did not run and finish within $Seconds seconds"
}

# Runs setup silently as SYSTEM in session 0, as the app's updater does, through a temporary task.
function InstallAsSystem([string]$Setup) {
    $log = NewLog
    $name = 'ScreenTimeManagerSmokeTestSetup'
    $action = New-ScheduledTaskAction -Execute $Setup -Argument "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /LOG=`"$log`""
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 10)
    Register-ScheduledTask -TaskName $name -Action $action -Principal $principal -Settings $settings -Force | Out-Null
    try {
        $since = Get-Date
        Start-ScheduledTask -TaskName $name
        $info = WaitForTaskRun $name $since 300
        if ($info.LastTaskResult) { ShowFile $log; throw "Setup run as SYSTEM exited with code $($info.LastTaskResult)" }
    } finally {
        Unregister-ScheduledTask -TaskName $name -Confirm:$false
    }
}

function Sid([string]$Name) {
    if ($Name -match '^S-1-') { return $Name }
    ([Security.Principal.NTAccount]$Name).Translate([Security.Principal.SecurityIdentifier]).Value
}

function CheckTask([string]$Name, [string]$Arguments, [string]$PrincipalSid, [string]$RunLevel, [string]$Instances, [string]$TimeLimit) {
    $t = Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue
    Check ($null -ne $t) "Task $Name is missing"
    Check ($t.State -ne 'Disabled') "Task $Name is disabled"
    $actions = @($t.Actions)
    Check ($actions.Count -eq 1 -and $actions[0].Execute.Trim('"') -eq $exe) "Task $Name runs $($actions[0].Execute), not $exe"
    Check ([string]$actions[0].Arguments -eq $Arguments) "Task $Name has arguments '$($actions[0].Arguments)', not '$Arguments'"
    $principal = if ($t.Principal.GroupId) { Sid $t.Principal.GroupId } else { Sid $t.Principal.UserId }
    Check ($principal -eq $PrincipalSid) "Task $Name runs as $principal, not $PrincipalSid"
    Check ([string]$t.Principal.RunLevel -eq $RunLevel) "Task $Name has run level $($t.Principal.RunLevel), not $RunLevel"
    Check ([string]$t.Settings.MultipleInstances -eq $Instances) "Task $Name has multiple instances $($t.Settings.MultipleInstances), not $Instances"
    Check ($t.Settings.ExecutionTimeLimit -eq $TimeLimit) "Task $Name has time limit $($t.Settings.ExecutionTimeLimit), not $TimeLimit"
    Check (!$t.Settings.DisallowStartIfOnBatteries -and !$t.Settings.StopIfGoingOnBatteries) "Task $Name doesn't run on battery"
    $t
}

# A task's triggers as an array. A task without any has $null Triggers, and @($null) is one item.
function TriggersOf($Task, [string]$Class = '') {
    @($Task.Triggers | Where-Object { $null -ne $_ -and (!$Class -or $_.CimClass.CimClassName -eq $Class) })
}

function TaskFolder {
    $service = New-Object -ComObject Schedule.Service
    $service.Connect()
    $service.GetFolder('\')
}

# FILE_WRITE_DATA, FILE_APPEND_DATA, FILE_WRITE_EA, FILE_WRITE_ATTRIBUTES, DELETE, WRITE_DAC,
# WRITE_OWNER, GENERIC_ALL, GENERIC_WRITE: what it takes to change, disable or delete a task.
$taskWrites = 0x2 -bor 0x4 -bor 0x10 -bor 0x100 -bor 0x10000 -bor 0x40000 -bor 0x80000 -bor 0x10000000 -bor 0x40000000
# FILE_READ_DATA and FILE_EXECUTE: to open a task and to run it.
$taskRun = 0x21

# What a task's DACL allows standard accounts (Everyone, Authenticated Users, Interactive, Users).
function StandardAccountAccess([string]$Name) {
    $sddl = (TaskFolder).GetTask($Name).GetSecurityDescriptor(4)
    $sd = New-Object Security.AccessControl.RawSecurityDescriptor $sddl
    $standard = 'S-1-1-0', 'S-1-5-11', 'S-1-5-4', $usersSid
    $granted = 0
    foreach ($ace in $sd.DiscretionaryAcl) {
        if ([string]$ace.AceQualifier -eq 'AccessAllowed' -and $standard -contains $ace.SecurityIdentifier.Value) {
            $granted = $granted -bor $ace.AccessMask
        }
    }
    [pscustomobject]@{ Sddl = $sddl; Granted = $granted }
}

# Standard accounts may read and run the task, and nothing more. The main task needs this for the
# watchdog (which runs with the account's non-elevated token) to relaunch the app through it, the
# update task for the app to start an update.
function CheckStandardAccountsMayOnlyRun([string]$Name) {
    $access = StandardAccountAccess $Name
    Check (($access.Granted -band $taskWrites) -eq 0) "Task $Name lets standard accounts change or delete it: $($access.Sddl)"
    Check (($access.Granted -band $taskRun) -eq $taskRun) "Task $Name doesn't let standard accounts run it: $($access.Sddl)"
}

function CheckInstall([string]$Version, [datetime]$Since) {
    Check (Test-Path $exe) "$exe was not installed"
    Check ((InstalledVersion) -eq $Version) "The installed version is $(InstalledVersion), not $Version"
    Check (Test-Path $marker) 'updating.marker was not written'
    Check ((Get-Item $marker).LastWriteTime -ge $Since.AddSeconds(-2)) 'updating.marker was not rewritten'

    $t = CheckTask $mainTask '' $usersSid 'Limited' 'Parallel' 'PT0S'
    Check (@(TriggersOf $t).Count -eq 1) "Task $mainTask should only have a logon trigger"
    $logon = @(TriggersOf $t 'MSFT_TaskLogonTrigger')
    Check ($logon.Count -eq 1 -and !$logon[0].UserId) "Task $mainTask should start at the sign-in of any user"
    Check ($t.Settings.RestartCount -eq 3 -and $t.Settings.RestartInterval -eq 'PT1M') "Task $mainTask should restart 3 times, every minute"
    Check ($t.Settings.StartWhenAvailable) "Task $mainTask should start when available"
    Check ($t.Settings.AllowDemandStart) "Task $mainTask should allow starting on demand, as the watchdog does"
    CheckStandardAccountsMayOnlyRun $mainTask

    $t = CheckTask $watchdogTask '--watchdog-check' $usersSid 'Limited' 'Parallel' 'PT5M'
    $time = @(TriggersOf $t 'MSFT_TaskTimeTrigger')
    Check ($time.Count -eq 1 -and $time[0].Repetition.Interval -eq 'PT1M' -and !$time[0].Repetition.Duration) "Task $watchdogTask should run every minute, indefinitely"
    $logon = @(TriggersOf $t 'MSFT_TaskLogonTrigger')
    Check ($logon.Count -eq 1 -and !$logon[0].UserId -and $logon[0].Delay -eq 'PT2M') "Task $watchdogTask should run 2 minutes after the sign-in of any user"

    $t = CheckTask $updateTask '--update' $systemSid 'Highest' 'IgnoreNew' 'PT2H'
    Check (@(TriggersOf $t).Count -eq 0) "Task $updateTask should only run on demand"
    CheckStandardAccountsMayOnlyRun $updateTask

    Check (Test-Path $sharedDir) "$sharedDir was not created"
    $rules = (Get-Acl $sharedDir).GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])
    $modify = [Security.AccessControl.FileSystemRights]::Modify
    $users = @($rules | Where-Object {
            $_.IdentityReference.Value -eq $usersSid -and [string]$_.AccessControlType -eq 'Allow' -and
            ($_.FileSystemRights -band $modify) -eq $modify -and ([int]$_.InheritanceFlags -band 3) -eq 3
        })
    Check ($users.Count -gt 0) "Standard accounts can't modify everything in $sharedDir"
}

# Runs a script as a new standard account: it starts the update task and the main task, then tries
# to delete and to disable each task. Returns its results, or $null if a process couldn't be started
# as that account.
function StandardAccountProbe {
    $user = 'stmsmoketest'
    # 14 characters at most, or `net user` asks to confirm.
    $password = 'Aa1!' + [guid]::NewGuid().ToString('N').Substring(0, 10)
    net user $user $password /add /comment:"Screen Time Manager smoke test" | Out-Null
    Check ($LASTEXITCODE -eq 0) "Could not add the local account $user"
    try {
        $probeDir = "$tmp\probe"
        New-Item -ItemType Directory $probeDir | Out-Null
        icacls $probeDir /grant "${user}:(OI)(CI)M" | Out-Null
        $out = "$probeDir\result.json"
        Set-Content "$probeDir\probe.ps1" -Encoding ASCII -Value @'
param([string]$Out)
$r = [ordered]@{}
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$r.user = $id.Name
$r.admin = ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
cmd /c "schtasks /Run /TN ScreenTimeManagerUpdate <nul >nul 2>&1"
$r.run = $LASTEXITCODE
# As the watchdog does when the app isn't running (1.0.1: "Access is denied.").
$r.runMainOutput = (cmd /c "schtasks /Run /TN ScreenTimeManager <nul 2>&1" | Out-String).Trim()
$r.runMain = $LASTEXITCODE
$service = New-Object -ComObject Schedule.Service
$service.Connect()
$folder = $service.GetFolder('\')
foreach ($t in 'ScreenTimeManager', 'ScreenTimeManagerWatchdog', 'ScreenTimeManagerUpdate') {
    cmd /c "schtasks /Delete /TN $t /F <nul >nul 2>&1"
    $r["delete $t"] = $LASTEXITCODE
    try { $folder.GetTask($t).Enabled = $false; $r["disable $t"] = 'allowed' } catch { $r["disable $t"] = 'denied' }
}
$r | ConvertTo-Json | Set-Content $Out
'@
        $credential = New-Object Management.Automation.PSCredential ".\$user", (ConvertTo-SecureString $password -AsPlainText -Force)
        try {
            $p = Start-Process powershell.exe -Credential $credential -LoadUserProfile -WorkingDirectory $probeDir -PassThru `
                -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$probeDir\probe.ps1`"", "`"$out`""
        } catch {
            "::warning::Could not start a process as a standard account here ($($_.Exception.Message)); skipped checking what one can do with the tasks" | Out-Host
            return $null
        }
        $null = $p.Handle
        if (!$p.WaitForExit(180000)) { $p.Kill(); throw 'The standard account probe was still running after 3 minutes' }
        Check (Test-Path $out) "The standard account probe exited with $($p.ExitCode) and no result"
        Get-Content $out -Raw | ConvertFrom-Json
    } finally {
        net user $user /delete | Out-Null
        Get-CimInstance Win32_UserProfile | Where-Object { $_.LocalPath -like "*\$user" } | Remove-CimInstance -ErrorAction SilentlyContinue
    }
}

$version = VersionOf (Join-Path $PSScriptRoot "$Bin\screen-time-manager.exe")
$setup = "$root\target\installer\screen-time-manager-v$version-windows-x64-setup.exe"
Check (Test-Path $setup) "$setup not found: run installer\build.ps1 -Bin $Bin first"
Check (!(Test-Path $exe)) "Screen Time Manager is already installed in $dir"
$server = $null

try {
    # ---- Install ----
    $since = Get-Date
    Install $setup
    CheckInstall $version $since
    "Installed $version" | Out-Host

    # ---- Setup must replace an existing main task's descriptor ----
    # 1.0.1 registered it with the default one, which lets standard accounts read it but not run
    # it, so its watchdog couldn't relaunch the app. Give it an equivalent here; the reinstall below
    # (CheckInstall) must leave it runnable again.
    (TaskFolder).GetTask($mainTask).SetSecurityDescriptor('D:(A;;FA;;;BA)(A;;FA;;;SY)(A;;FR;;;BU)', 0)
    $oldAccess = StandardAccountAccess $mainTask
    $upgradeChecked = ($oldAccess.Granted -band $taskRun) -ne $taskRun
    if (!$upgradeChecked) {
        "::warning::Could not give $mainTask a 1.0.1-style descriptor ($($oldAccess.Sddl)); not checking that setup replaces it" | Out-Host
    }

    # ---- Reinstall as SYSTEM over a running copy: setup must close it, from session 0 ----
    # The watchdog may already have relaunched it (the marker is fresh).
    if (!(AppProcesses)) {
        Start-Process $exe | Out-Null
        Start-Sleep 3
    }
    $running = AppProcesses
    Check ($running.Count -gt 0) 'The app did not start'
    $since = Get-Date
    InstallAsSystem $setup
    foreach ($p in $running) {
        Check ($null -eq (Get-Process -Id $p.Id -ErrorAction SilentlyContinue)) "Setup left the app running (process $($p.Id))"
    }
    CheckInstall $version $since
    "Reinstalled as SYSTEM over the running app, which setup closed" | Out-Host
    if ($upgradeChecked) { "Setup replaced $mainTask's 1.0.1-style descriptor: $((StandardAccountAccess $mainTask).Sddl)" | Out-Host }

    # ---- The update task, started by an administrator ----
    # It checks the real latest release, which is expected to be no newer than this build.
    $since = Get-Date
    schtasks /Run /TN $updateTask
    Check ($LASTEXITCODE -eq 0) "schtasks /Run /TN $updateTask failed with $LASTEXITCODE"
    $info = WaitForTaskRun $updateTask $since 300
    "The update task ran; --update exited with $($info.LastTaskResult)" | Out-Host
    Check ((InstalledVersion) -eq $version) "The update task installed $(InstalledVersion) from the real latest release, which is newer than this build"

    # ---- ... and by a standard account, which can't change or delete any task ----
    $since = Get-Date
    $probe = StandardAccountProbe
    if ($probe) {
        "Standard account probe: $($probe | ConvertTo-Json -Compress)" | Out-Host
        Check (!$probe.admin) "The probe account $($probe.user) is an administrator"
        Check ($probe.run -eq 0) "A standard account could not start $updateTask (schtasks exited with $($probe.run))"
        Check ($probe.runMain -eq 0) "A standard account could not start $mainTask, so the watchdog can't relaunch the app (schtasks exited with $($probe.runMain): $($probe.runMainOutput))"
        foreach ($t in $tasks) {
            Check ($probe."delete $t" -ne 0) "A standard account deleted task $t"
            Check ($probe."disable $t" -eq 'denied') "A standard account disabled task $t"
        }
        # Where Task Scheduler started the main task for an account that has no session of its own
        # (the probe runs in this runner's session) isn't asserted: it's not a case the app meets.
        "After the probe started ${mainTask}: $(AppProcessSummary)" | Out-Host
        $null = WaitForTaskRun $updateTask $since 300
        CheckInstall $version ([datetime]'2000-01-01')
        "A standard account started the update task and the main task, and could not delete or disable any task" | Out-Host
    }

    # ---- Update through the app's updater, from a fake release on localhost ----
    $v = [version]$version
    $next = '{0}.{1}.{2}' -f $v.Major, $v.Minor, ($v.Build + 1)
    $feedDir = "$tmp\feed"
    & "$PSScriptRoot\build.ps1" -Bin $Bin -Version $next -OutputDir $feedDir | Out-Host
    Check ($LASTEXITCODE -eq 0) "Building the $next setup failed"
    $asset = "screen-time-manager-v$next-windows-x64-setup.exe"
    Check (Test-Path "$feedDir\$asset") "build.ps1 did not write $feedDir\$asset"
    $hash = (Get-FileHash "$feedDir\$asset" -Algorithm SHA256).Hash.ToLowerInvariant()

    $listener = New-Object Net.Sockets.TcpListener ([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    $base = "http://127.0.0.1:$port"
    $feed = "$tmp\latest.json"
    $release = [ordered]@{
        tag_name   = "v$next"
        name       = "Screen Time Manager v$next"
        html_url   = "$base/releases/tag/v$next"
        draft      = $false
        prerelease = $false
        assets     = @([ordered]@{
                name                 = $asset
                browser_download_url = "$base/download/$asset"
                size                 = (Get-Item "$feedDir\$asset").Length
                digest               = "sha256:$hash"
            })
    }
    # UTF-8 without a BOM.
    [IO.File]::WriteAllText($feed, ($release | ConvertTo-Json -Depth 5))

    $server = Start-Job -ArgumentList $port, $feed, $feedDir -ScriptBlock {
        param($Port, $Feed, $Dir)
        $http = New-Object Net.HttpListener
        $http.Prefixes.Add("http://127.0.0.1:$Port/")
        $http.Start()
        while ($true) {
            $context = $http.GetContext()
            $path = $context.Request.Url.AbsolutePath
            $file = $null
            if ($path -eq '/releases/latest') { $file = $Feed }
            elseif ($path -like '/download/*') { $file = Join-Path $Dir ($path.Substring(10)) }
            $response = $context.Response
            if ($file -and (Test-Path -LiteralPath $file -PathType Leaf)) {
                $bytes = [IO.File]::ReadAllBytes($file)
                $response.ContentType = if ($file -eq $Feed) { 'application/json' } else { 'application/octet-stream' }
                $response.ContentLength64 = $bytes.Length
                $response.OutputStream.Write($bytes, 0, $bytes.Length)
            } else {
                $response.StatusCode = 404
            }
            "$($context.Request.HttpMethod) $path -> $($response.StatusCode)"
            $response.Close()
        }
    }
    $ready = $false
    for ($i = 0; $i -lt 30 -and !$ready; $i++) {
        try { $ready = (Invoke-WebRequest "$base/releases/latest" -UseBasicParsing).StatusCode -eq 200 } catch { Start-Sleep 1 }
    }
    Check $ready "The fake release server didn't start: $(Receive-Job $server 2>&1 | Out-String)"

    # The watchdog logs a conclusion only when it differs from the last one (watchdog.state); start
    # afresh, so the relaunch after this update is logged even if an earlier one was the last thing.
    Remove-Item "$userDataDir\watchdog.state" -ErrorAction SilentlyContinue
    $env:STM_UPDATE_FEED = "$base/releases/latest"
    try {
        $since = Get-Date
        $p = Start-Process $exe -ArgumentList '--update' -PassThru
        $null = $p.Handle
        if (!$p.WaitForExit(300000)) { throw '--update was still running after 5 minutes' }
        if ($p.ExitCode) { ShowFile "$dir\updates\update.log"; throw "--update exited with code $($p.ExitCode)" }
    } finally {
        Remove-Item Env:\STM_UPDATE_FEED
    }
    # --update only starts setup; wait for that to finish.
    $done = $false
    for ($i = 0; $i -lt 300 -and !$done; $i++) {
        $setups = @(Get-CimInstance Win32_Process | Where-Object { $_.Name -like 'screen-time-manager-v*-setup*' })
        $done = (InstalledVersion) -eq $next -and $setups.Count -eq 0
        if (!$done) { Start-Sleep 1 }
    }
    if (!$done) {
        ShowFile "$dir\updates\update.log"
        ShowFile "$dir\updates\setup.log"
        Receive-Job $server | Out-Host
        throw "5 minutes after --update the installed version is $(InstalledVersion), not $next"
    }
    CheckInstall $next $since
    Check (Test-Path "$dir\updates\$asset") "The updater did not keep its download in $dir\updates"
    # The updater passes "/LOG=<path with a space>" as one quoted argument; setup must accept it.
    Check (Test-Path "$dir\updates\setup.log") "Setup run by --update wrote no $dir\updates\setup.log"
    Check ((Get-Item "$dir\updates\setup.log").LastWriteTime -ge $since) 'setup.log is not from this update'
    "Updated $version to $next through --update; the fake release server saw:" | Out-Host
    Receive-Job $server | Out-Host

    # ---- The watchdog brings the app back after the update, and after a kill ----
    # 1.0.1 in the field: setup closed the app, and the watchdog's relaunch failed ("Access is
    # denied" starting the main task as a standard user, then a direct child that ended with the
    # watchdog run) while logging relaunched=true. Here the installed watchdog task, running
    # non-elevated in this runner's session, must bring the app back by itself, and the app must
    # outlive the watchdog run. Nothing here starts the app.
    $session = (Get-Process -Id $PID).SessionId
    if ($session -eq 0) {
        "::warning::This runner runs in session 0, where nobody is signed in and the watchdog task doesn't run; skipped checking the watchdog's relaunch" | Out-Host
    } elseif (!(CheckWatchdogRelaunch $session $since 'install/update finished .*relaunched=true' 'After the update')) {
        "::warning::The watchdog task did not run in this runner's session ($session) within 3 minutes; skipped checking the watchdog's relaunch. $(AppProcessSummary)" | Out-Host
    } else {
        # A stop only counts as a kill once a watchdog run has seen the app running in this session
        # (until then it may still be starting); that run sets watchdog.state to 'running'.
        $state = "$userDataDir\watchdog.state"
        for ($i = 0; $i -lt 150 -and (Get-Content $state -ErrorAction SilentlyContinue) -ne 'running'; $i++) { Start-Sleep 1 }
        Check ((Get-Content $state -ErrorAction SilentlyContinue) -eq 'running') "No watchdog run saw the relaunched app running: watchdog.state is '$(Get-Content $state -ErrorAction SilentlyContinue)'"
        $since = Get-Date
        AppInSession $session | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }
        Check (CheckWatchdogRelaunch $session $since 'relaunched=true .*sending tamper alert' 'After a kill') "The watchdog task stopped running in session $session"
    }

    # ---- Uninstall, while the app runs ----
    if (!(AppProcesses)) {
        Start-Process $exe | Out-Null
        Start-Sleep 3
    }
    New-Item -ItemType Directory -Force "$dir\updates" | Out-Null
    Set-Content "$dir\updates\smoke-test.txt" 'removed by uninstall'
    New-Item -ItemType Directory -Force $userDataDir | Out-Null
    Set-Content "$sharedDir\smoke-test.txt" 'kept by uninstall'
    Set-Content "$userDataDir\smoke-test.txt" 'kept by uninstall'
    $uninstaller = (Get-ItemProperty $uninstallKey).UninstallString.Trim('"')
    $log = NewLog
    Start-Process $uninstaller -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$log`"" -Wait
    # The uninstaller reruns itself from a temp copy and may return early, so wait for it.
    for ($i = 0; $i -lt 120 -and ((Test-Path $exe) -or (Test-Path $uninstallKey)); $i++) { Start-Sleep 1 }
    Start-Sleep 2
    if ((Test-Path $exe) -or (Test-Path $uninstallKey)) { ShowFile $log; throw 'Uninstall did not remove the app' }
    foreach ($t in $tasks) {
        Check ($null -eq (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue)) "Uninstall left task $t"
    }
    Check (!(Test-Path $marker)) 'Uninstall left updating.marker'
    Check (!(Test-Path "$dir\updates")) 'Uninstall left the updates folder'
    Check ((AppProcesses).Count -eq 0) 'Uninstall left the app running'
    Check (Test-Path "$sharedDir\smoke-test.txt") "Uninstall removed $sharedDir"
    Check (Test-Path "$userDataDir\smoke-test.txt") "Uninstall removed $userDataDir"
    Remove-Item "$sharedDir\smoke-test.txt", "$userDataDir\smoke-test.txt"
    'Install, reinstall as SYSTEM, task permissions, update, watchdog relaunch and uninstall OK'
} finally {
    if ($server) { Stop-Job $server; Remove-Job $server -Force }
}
