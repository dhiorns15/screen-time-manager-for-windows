# Screen Time Manager - Install Script
# Run as Administrator for best results

param(
    [string]$ExePath = ""
)

$ErrorActionPreference = "Stop"
$TaskName = "ScreenTimeManager"
$Description = "Screen Time Manager - Manages daily computer time limits"
$WatchdogTaskName = "ScreenTimeManagerWatchdog"
$WatchdogDescription = "Screen Time Manager - relaunches the app if it's stopped outside of the passcode-protected Quit option"

Write-Host "Screen Time Manager - Installation" -ForegroundColor Cyan
Write-Host "===================================" -ForegroundColor Cyan
Write-Host ""

# Find the executable
if ($ExePath -eq "") {
    # Try to find it in common locations
    $scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
    $possiblePaths = @(
        (Join-Path $scriptDir "screen-time-manager.exe"),
        (Join-Path $scriptDir "target\release\screen-time-manager.exe"),
        (Join-Path $scriptDir "target\debug\screen-time-manager.exe")
    )

    foreach ($path in $possiblePaths) {
        if (Test-Path $path) {
            $ExePath = $path
            break
        }
    }
}

if ($ExePath -eq "" -or -not (Test-Path $ExePath)) {
    Write-Host "ERROR: Could not find screen-time-manager.exe" -ForegroundColor Red
    Write-Host ""
    Write-Host "Usage: .\install.ps1 -ExePath 'C:\path\to\screen-time-manager.exe'" -ForegroundColor Yellow
    Write-Host ""
    exit 1
}

$ExePath = (Resolve-Path $ExePath).Path
Write-Host "Found executable: $ExePath" -ForegroundColor Green
Write-Host ""

# Create the shared config folder (used for Telegram/Discord bot settings so
# they're the same no matter which Windows account runs the app) and grant
# standard users write access - %ProgramData% isn't writable by non-admin
# accounts by default.
$SharedConfigDir = Join-Path $env:ProgramData "ScreenTimeManager"
Write-Host "Setting up shared config folder..." -ForegroundColor White
if (-not (Test-Path $SharedConfigDir)) {
    New-Item -ItemType Directory -Path $SharedConfigDir -Force | Out-Null
}
icacls $SharedConfigDir /grant "*S-1-5-32-545:(OI)(CI)M" | Out-Null
Write-Host "Shared config folder ready: $SharedConfigDir" -ForegroundColor Green
Write-Host ""

# Check if task already exists
$existingTask = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if ($existingTask) {
    Write-Host "Existing installation found. Removing..." -ForegroundColor Yellow
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
}

# Create the scheduled task
Write-Host "Creating scheduled task..." -ForegroundColor White

$action = New-ScheduledTaskAction -Execute $ExePath
$trigger = New-ScheduledTaskTrigger -AtLogOn
# GroupId (not UserId) so the task runs for *any* account that logs on, in
# that account's own session - not just the account that ran this installer.
# Without this, other Windows accounts never get auto-started at all.
$principal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Limited
# MultipleInstances defaults to IgnoreNew, and Task Scheduler tracks "is this
# task running" per task definition, not per session - so with the default,
# a second account logging on (e.g. via Fast User Switching) while the first
# account's instance is still running gets its AtLogOn trigger silently
# dropped, and the app never starts for them. Parallel lets each session's
# trigger launch its own instance; the per-session mutex (see constants.rs)
# still prevents duplicate launches within the same session.
$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -MultipleInstances Parallel `
    -ExecutionTimeLimit (New-TimeSpan -Days 0) `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1)

$task = New-ScheduledTask -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description $Description

Register-ScheduledTask -TaskName $TaskName -InputObject $task | Out-Null

# Watchdog task: fires every minute and relaunches the app if it's not
# running (see src/watchdog.rs). Registered here while elevated so it's
# owned by Administrators - by default a standard user can't disable or
# delete a scheduled task they don't own, which is what keeps this from
# being trivially turned off the same way the app itself can be killed.
Write-Host "Creating watchdog task..." -ForegroundColor White

$existingWatchdog = Get-ScheduledTask -TaskName $WatchdogTaskName -ErrorAction SilentlyContinue
if ($existingWatchdog) {
    Unregister-ScheduledTask -TaskName $WatchdogTaskName -Confirm:$false
}

$watchdogAction = New-ScheduledTaskAction -Execute $ExePath -Argument "--watchdog-check"
$watchdogTrigger = New-ScheduledTaskTrigger -Once -At (Get-Date) `
    -RepetitionInterval (New-TimeSpan -Minutes 1) `
    -RepetitionDuration (New-TimeSpan -Days 3650)
$watchdogTrigger2 = New-ScheduledTaskTrigger -AtLogOn
# Delay this trigger's first check so it doesn't race the main app task's own
# -AtLogOn trigger: both fire at logon with no ordering guarantee, and the
# watchdog's mutex check is fast enough to easily run before the main app has
# finished launching and grabbed the mutex - causing a false "process was
# killed" tamper alert on every ordinary restart/login. The main app task has
# no such delay, so this gives it a head start.
$watchdogTrigger2.Delay = "PT2M"
# Same GroupId reasoning as the main task's principal above - the watchdog
# needs to run per-user too, not just for whoever installed it.
$watchdogPrincipal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Limited
# Parallel, not IgnoreNew: with two sessions active, the per-minute repetition
# trigger fires for both sessions' instances at the same tick, and Task
# Scheduler tracks "is this task running" per task definition rather than per
# session - so IgnoreNew would silently drop one session's check on every
# collision, leaving that user's session unmonitored for tampering.
$watchdogSettings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -MultipleInstances Parallel `
    -ExecutionTimeLimit (New-TimeSpan -Minutes 5)

$watchdogTask = New-ScheduledTask -Action $watchdogAction -Trigger @($watchdogTrigger, $watchdogTrigger2) `
    -Principal $watchdogPrincipal -Settings $watchdogSettings -Description $WatchdogDescription

Register-ScheduledTask -TaskName $WatchdogTaskName -InputObject $watchdogTask | Out-Null

Write-Host ""
Write-Host "Installation complete!" -ForegroundColor Green
Write-Host ""
Write-Host "Screen Time Manager will now start automatically when you log in," -ForegroundColor White
Write-Host "and will be relaunched within a minute if it's ever stopped outside" -ForegroundColor White
Write-Host "of the passcode-protected Quit option." -ForegroundColor White
Write-Host ""

# Ask if user wants to start it now
$response = Read-Host "Do you want to start Screen Time Manager now? (Y/n)"
if ($response -eq "" -or $response -match "^[Yy]") {
    Write-Host "Starting Screen Time Manager..." -ForegroundColor White
    Start-ScheduledTask -TaskName $TaskName
    Write-Host "Started!" -ForegroundColor Green
}

Write-Host ""
Write-Host "To uninstall, run: .\uninstall.ps1" -ForegroundColor Cyan
Write-Host ""
