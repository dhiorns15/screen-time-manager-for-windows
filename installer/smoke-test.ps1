# Installs Screen Time Manager with the built setup and checks the install: the exe, the three
# scheduled tasks (and that standard accounts may run the main and update tasks but not change
# them), the shared config folder's permissions and the update marker. Then it reinstalls as SYSTEM
# over a running copy (which must replace a 1.0.1-style descriptor on the main task), interrupts a
# setup after it turned the tasks off and checks its recovery task turns them back on, checks the
# update task can be started by an administrator, and the update and main tasks by a standard
# account (which must not be able to disable or delete any task, or delete the shared config
# folder, but must be able to create, change and delete files in it), updates through the app's own
# updater from a fake release served on localhost, checks the installed watchdog brings the app
# back - one copy - after that update and after a kill (when the runner has a signed-in session for
# it to run in), uninstalls, and checks a junction in place of the shared config folder doesn't
# redirect a new install.
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
$recoveryTask = 'ScreenTimeManagerSetupRecovery'
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
    # Which request Task Scheduler accepted, and whether the watchdog ran non-elevated: only then
    # does this show that a standard user's watchdog may make it (MS-TSCH refuses a non-admin caller
    # a session-targeted run unless it is the user signed in there).
    if ($lines[-1] -match 'as a non-elevated caller with RunEx\(AS_SELF\|USE_SESSION_ID') {
        "$What`: a non-elevated watchdog started the task with RunEx(AS_SELF|USE_SESSION_ID) in its own session" | Out-Host
    } elseif ($lines[-1] -match 'as (an elevated|a non-elevated) caller with \S+') {
        "::warning::$What`: the watchdog started the task $($Matches[0]), so this runner did not exercise the request a non-elevated watchdog (a child's account, or an administrator with UAC on) makes: RunEx(AS_SELF|USE_SESSION_ID)" | Out-Host
    }
    # Exactly one copy: a second start (two watchdog runs, the logon trigger) must leave quietly, not
    # stay up showing "already running".
    Start-Sleep 5
    $copies = @(AppInSession $Session)
    if ($copies.Count -ne 1) { ShowFile "$userDataDir\watchdog.log" }
    Check ($copies.Count -eq 1) "$What`: $($copies.Count) copies of the app run in session $Session ($(AppProcessSummary))"
    $true
}

# The shared config folder as setup leaves it: a real folder owned by Administrators, not
# inheriting from ProgramData; standard accounts may create files in it and change and delete the
# files, but not delete, rename or re-permission the folder or add subfolders.
function CheckSharedFolder {
    $item = Get-Item -LiteralPath $sharedDir -Force -ErrorAction SilentlyContinue
    Check ($null -ne $item -and $item.PSIsContainer) "$sharedDir was not created"
    Check (!($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) "$sharedDir is a link"
    $acl = Get-Acl -LiteralPath $sharedDir
    $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
    Check ($owner -eq 'S-1-5-32-544') "$sharedDir is owned by $owner, not Administrators: $($acl.Sddl)"
    Check ($acl.AreAccessRulesProtected) "$sharedDir inherits from ProgramData: $($acl.Sddl)"
    $standard = 'S-1-1-0', 'S-1-5-11', 'S-1-5-4', $usersSid, 'S-1-3-0', 'S-1-3-4'
    $itself = 0
    $files = 0
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ([string]$rule.AccessControlType -ne 'Allow' -or $standard -notcontains $rule.IdentityReference.Value) { continue }
        $rights = [int]$rule.FileSystemRights
        if (!([int]$rule.PropagationFlags -band 2)) { $itself = $itself -bor $rights }
        if ([int]$rule.InheritanceFlags -band 2) { $files = $files -bor $rights }
    }
    # Add subfolder, delete child, delete, change permissions, take ownership, generic all/write.
    $forbidden = 0x4 -bor 0x40 -bor 0x10000 -bor 0x40000 -bor 0x80000 -bor 0x10000000 -bor 0x40000000
    Check (($itself -band $forbidden) -eq 0) "Standard accounts may delete, rename or re-permission $sharedDir itself (0x$('{0:X}' -f $itself)): $($acl.Sddl)"
    Check (($itself -band 0x2) -eq 0x2) "Standard accounts can't create files in $sharedDir`: $($acl.Sddl)"
    $modify = [int][Security.AccessControl.FileSystemRights]::Modify
    Check (($files -band $modify) -eq $modify) "Standard accounts can't change and delete the files in $sharedDir`: $($acl.Sddl)"
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

# Uninstalls silently and checks the app, its tasks and its install folder are gone.
function Uninstall {
    $uninstaller = (Get-ItemProperty $uninstallKey).UninstallString.Trim('"')
    $log = NewLog
    Start-Process $uninstaller -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$log`"" -Wait
    # The uninstaller reruns itself from a temp copy and may return early, so wait for it.
    for ($i = 0; $i -lt 120 -and ((Test-Path $exe) -or (Test-Path $uninstallKey)); $i++) { Start-Sleep 1 }
    Start-Sleep 2
    if ((Test-Path $exe) -or (Test-Path $uninstallKey)) { ShowFile $log; throw 'Uninstall did not remove the app' }
    foreach ($t in $tasks + $recoveryTask) {
        Check ($null -eq (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue)) "Uninstall left task $t"
    }
    Check (!(Test-Path $marker)) 'Uninstall left updating.marker'
    Check (!(Test-Path "$dir\updates")) 'Uninstall left the updates folder'
    Check ((AppProcesses).Count -eq 0) 'Uninstall left the app running'
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

    $t = CheckTask $mainTask '--quiet-if-running' $usersSid 'Limited' 'Parallel' 'PT0S'
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

    Check ($null -eq (Get-ScheduledTask -TaskName $recoveryTask -ErrorAction SilentlyContinue)) "Setup left task $recoveryTask behind"
    CheckSharedFolder
    # Inherited from Program Files, and no standard account may change it (the update task runs
    # its exe as SYSTEM).
    foreach ($path in $dir, $exe) {
        $acl = Get-Acl -LiteralPath $path
        foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
            $sid = $rule.IdentityReference.Value
            if ([string]$rule.AccessControlType -ne 'Allow' -or $sid -in 'S-1-5-18', 'S-1-5-32-544', 'S-1-3-0' -or $sid -like 'S-1-5-80-*') { continue }
            Check (([int]$rule.FileSystemRights -band (0x2 -bor 0x4 -bor 0x40 -bor 0x10000 -bor 0x40000 -bor 0x80000 -bor 0x10000000 -bor 0x40000000)) -eq 0) "$sid may change $path`: $($acl.Sddl)"
        }
    }
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
param([string]$Out, [string]$Shared)
$r = [ordered]@{}
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$r.user = $id.Name
$r.admin = ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
function HResult($e) { $e = $e.Exception; while ($e.InnerException) { $e = $e.InnerException }; '0x{0:X8}' -f $e.HResult }
cmd /c "schtasks /Run /TN ScreenTimeManagerUpdate <nul >nul 2>&1"
$r.run = $LASTEXITCODE
# As the watchdog's last resort when the app isn't running (1.0.1: "Access is denied.").
$r.runMainOutput = (cmd /c "schtasks /Run /TN ScreenTimeManager <nul 2>&1" | Out-String).Trim()
$r.runMain = $LASTEXITCODE
$service = New-Object -ComObject Schedule.Service
$service.Connect()
$folder = $service.GetFolder('\')
# The watchdog's own requests (src/watchdog.rs, run_requests). This account isn't the one signed in
# to this session (the runner's is), so MS-TSCH says both are refused with E_INVALIDARG - but only
# after the access check, so they must not be refused with E_ACCESSDENIED.
$session = (Get-Process -Id $PID).SessionId
$main = $folder.GetTask('ScreenTimeManager')
try { $null = $main.RunEx($null, 5, $session, $null); $r.runExSelfSession = 'accepted' } catch { $r.runExSelfSession = HResult $_ }
try { $null = $main.RunEx($null, 12, $session, $id.User.Value); $r.runExSidSession = 'accepted' } catch { $r.runExSidSession = HResult $_ }
foreach ($t in 'ScreenTimeManager', 'ScreenTimeManagerWatchdog', 'ScreenTimeManagerUpdate') {
    cmd /c "schtasks /Delete /TN $t /F <nul >nul 2>&1"
    $r["delete $t"] = $LASTEXITCODE
    try { $folder.GetTask($t).Enabled = $false; $r["disable $t"] = 'allowed' } catch { $r["disable $t"] = 'denied' }
}
# The shared config folder: files in it yes, the folder itself no.
$file = Join-Path $Shared "probe-$PID.txt"
try { Set-Content -LiteralPath $file 'one'; Add-Content -LiteralPath $file 'two'; $r.sharedWrite = 'allowed' } catch { $r.sharedWrite = "denied: $_" }
try { Remove-Item -LiteralPath $file -ErrorAction Stop; $r.sharedDeleteFile = 'allowed' } catch { $r.sharedDeleteFile = "denied: $_" }
try { New-Item -ItemType Directory (Join-Path $Shared "probe-$PID") -ErrorAction Stop | Out-Null; $r.sharedSubfolder = 'allowed' } catch { $r.sharedSubfolder = 'denied' }
# Opening the folder itself for delete (= delete or rename it), change permissions, take ownership;
# sharing everything, so only the permissions can refuse it.
Add-Type -Namespace Probe -Name Native -MemberDefinition '[DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)] public static extern Microsoft.Win32.SafeHandles.SafeFileHandle CreateFile(string name, uint access, uint share, IntPtr sa, uint disposition, uint flags, IntPtr template);'
foreach ($right in @(@('sharedDelete', 0x10000), @('sharedAcl', 0x40000), @('sharedOwner', 0x80000))) {
    # OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS (to open a folder) | FILE_FLAG_OPEN_REPARSE_POINT.
    $h = [Probe.Native]::CreateFile($Shared, [uint32]$right[1], 7, [IntPtr]::Zero, 3, 0x02200000, [IntPtr]::Zero)
    $r[$right[0]] = if ($h.IsInvalid) { 'denied' } else { 'allowed' }
    $h.Dispose()
}
$r | ConvertTo-Json | Set-Content $Out
'@
        $credential = New-Object Management.Automation.PSCredential ".\$user", (ConvertTo-SecureString $password -AsPlainText -Force)
        try {
            $p = Start-Process powershell.exe -Credential $credential -LoadUserProfile -WorkingDirectory $probeDir -PassThru `
                -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$probeDir\probe.ps1`"", "`"$out`"", "`"$sharedDir`""
        } catch {
            "::warning::Could not start a process as a standard account here ($($_.Exception.Message)); skipped checking what one can do with the tasks" | Out-Host
            return $null
        }
        $null = $p.Handle
        if (!$p.WaitForExit(180000)) { $p.Kill(); throw 'The standard account probe was still running after 3 minutes' }
        Check (Test-Path $out) "The standard account probe exited with $($p.ExitCode) and no result"
        Get-Content $out -Raw | ConvertFrom-Json
    } finally {
        # Whatever Task Scheduler started for the probe account (it has no session of its own).
        Get-CimInstance Win32_Process -Filter "Name = 'screen-time-manager.exe'" | Where-Object {
            (Invoke-CimMethod -InputObject $_ -MethodName GetOwner -ErrorAction SilentlyContinue).User -eq $user
        } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
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

    # ---- Setup interrupted with the tasks turned off: the recovery task turns them back on ----
    # 1.0.2 left them off for good after a power cut or a forced restart in that window. Setup is
    # killed as soon as it has turned both off (it registers the recovery task first); the recovery
    # task is then run on demand, standing in for the next boot.
    if (!(AppProcesses)) {
        Start-Process $exe | Out-Null
        Start-Sleep 3
    }
    $folder = TaskFolder
    $log = NewLog
    $since = Get-Date
    $p = Start-Process $setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$log`"" -PassThru
    $null = $p.Handle # keeps the exit code readable
    $caught = $false
    while (!$p.HasExited -and ((Get-Date) - $since).TotalSeconds -lt 300) {
        $off = $false
        try { $off = !$folder.GetTask($watchdogTask).Enabled -and !$folder.GetTask($mainTask).Enabled -and $folder.GetTask($recoveryTask) } catch {}
        if ($off) {
            # setup.exe runs the wizard as a child (is-*.tmp\setup.tmp): end both, as a power cut would.
            Get-CimInstance Win32_Process -Filter "ParentProcessId = $($p.Id)" | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
            Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
            $caught = $true
            break
        }
        Start-Sleep -Milliseconds 20
    }
    if ($caught) {
        Start-Sleep 2
        Check (!$folder.GetTask($watchdogTask).Enabled) "Task $watchdogTask was enabled again although setup was killed"
        $recovery = Get-ScheduledTask -TaskName $recoveryTask -ErrorAction SilentlyContinue
        Check ($null -ne $recovery) "Setup was killed with the tasks turned off and no $recoveryTask task to turn them back on"
        Check ((Sid $recovery.Principal.UserId) -eq $systemSid) "Task $recoveryTask runs as $($recovery.Principal.UserId), not SYSTEM"
        Check (@(TriggersOf $recovery 'MSFT_TaskBootTrigger').Count -eq 1) "Task $recoveryTask doesn't run at boot"
        Check (@(TriggersOf $recovery 'MSFT_TaskRegistrationTrigger' | Where-Object { $_.Delay -eq 'PT15M' }).Count -eq 1) "Task $recoveryTask doesn't run 15 minutes after setup registered it"
        $access = StandardAccountAccess $recoveryTask
        Check (($access.Granted -band ($taskWrites -bor 0x20)) -eq 0) "Standard accounts may run, change or delete $recoveryTask`: $($access.Sddl)"
        $since = Get-Date
        Start-ScheduledTask -TaskName $recoveryTask
        for ($i = 0; $i -lt 120 -and (Get-ScheduledTask -TaskName $recoveryTask -ErrorAction SilentlyContinue); $i++) { Start-Sleep 1 }
        Check ($null -eq (Get-ScheduledTask -TaskName $recoveryTask -ErrorAction SilentlyContinue)) "Task $recoveryTask did not remove itself"
        Check ($folder.GetTask($mainTask).Enabled -and $folder.GetTask($watchdogTask).Enabled) "Task $recoveryTask did not turn $mainTask and $watchdogTask back on"
        Check ((Get-Item $marker).LastWriteTime -ge $since.AddSeconds(-2)) "Task $recoveryTask did not touch updating.marker"
        "Setup killed with the tasks off; its recovery task turned them back on and removed itself" | Out-Host
        # Back to a complete install for what follows.
        $since = Get-Date
        Install $setup
        CheckInstall $version $since
    } else {
        $p.WaitForExit()
        "::warning::Setup finished before it could be killed with the tasks off (exit $($p.ExitCode)); the recovery task's run wasn't checked" | Out-Host
        if ($p.ExitCode) { ShowFile $log }
        Check ($p.ExitCode -eq 0) "Setup exited with $($p.ExitCode)"
        CheckInstall $version $since
    }

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
        foreach ($request in 'runExSelfSession', 'runExSidSession') {
            Check ($probe.$request -ne '0x80070005') "A standard account was denied access to $mainTask for the watchdog's $request request"
        }
        Check ($probe.sharedWrite -eq 'allowed') "A standard account could not create and change a file in $sharedDir ($($probe.sharedWrite))"
        Check ($probe.sharedDeleteFile -eq 'allowed') "A standard account could not delete its file in $sharedDir ($($probe.sharedDeleteFile))"
        foreach ($what in 'sharedSubfolder', 'sharedDelete', 'sharedAcl', 'sharedOwner') {
            Check ($probe.$what -eq 'denied') "A standard account could do $what on $sharedDir"
        }
        CheckSharedFolder
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
    Uninstall
    Check (Test-Path "$sharedDir\smoke-test.txt") "Uninstall removed $sharedDir"
    Check (Test-Path "$userDataDir\smoke-test.txt") "Uninstall removed $userDataDir"
    Remove-Item "$sharedDir\smoke-test.txt", "$userDataDir\smoke-test.txt"

    # ---- A junction in place of the shared config folder doesn't redirect setup ----
    # Up to 1.0.2 a standard account could delete the folder (Users had Modify on the folder
    # itself), put a junction to the install folder in its place (ProgramData lets Users create
    # folders), and the next setup - which updates run as SYSTEM - gave Users Modify on the
    # junction's target. Here the target is a decoy: setup must remove the junction without
    # following it, leave the decoy as it was, and make a real folder.
    $decoy = "$tmp\decoy"
    New-Item -ItemType Directory $decoy | Out-Null
    Set-Content "$decoy\keep.txt" 'decoy'
    $decoyAcl = (Get-Acl -LiteralPath $decoy).Sddl
    $keepAcl = (Get-Acl -LiteralPath "$decoy\keep.txt").Sddl
    Remove-Item -LiteralPath $sharedDir -Recurse -Force
    cmd /c mklink /J "$sharedDir" "$decoy" | Out-Host
    Check ([bool]((Get-Item -LiteralPath $sharedDir -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) "Could not put a junction in place of $sharedDir"
    $since = Get-Date
    Install $setup
    CheckInstall $version $since
    Check ((Get-Acl -LiteralPath $decoy).Sddl -eq $decoyAcl) "Setup changed the permissions of the junction's target $decoy`: $((Get-Acl -LiteralPath $decoy).Sddl)"
    Check ((Get-Acl -LiteralPath "$decoy\keep.txt").Sddl -eq $keepAcl) "Setup changed the permissions of a file in the junction's target"
    Check ((Get-Content "$decoy\keep.txt") -eq 'decoy') "Setup changed a file in the junction's target"
    Check (!(Test-Path "$sharedDir\keep.txt")) "$sharedDir still leads to the junction's target"
    "Setup replaced a junction in place of $sharedDir with a real folder and left its target alone" | Out-Host
    Uninstall
    'Install, reinstall as SYSTEM, interrupted setup, task and folder permissions, update, watchdog relaunch, uninstall and the shared folder junction OK'
} finally {
    if ($server) { Stop-Job $server; Remove-Job $server -Force }
}
