# Installs the release before this one, starts its update task (`--update` as SYSTEM, as the app
# does), and checks that it updates itself to this release. Runs on CI once a release is published,
# with GITHUB_REF_NAME the new tag and GH_TOKEN set for gh.
#
# CI only, on an administrator Windows runner: it installs Screen Time Manager for every account.

$ErrorActionPreference = 'Stop'
if (!$env:CI) { throw 'Run this on CI only: it installs Screen Time Manager for every account.' }

$current = $env:GITHUB_REF_NAME
$tags = @(gh release list --exclude-drafts --exclude-pre-releases --limit 100 --json tagName --jq '.[].tagName')
$index = [array]::IndexOf($tags, $current)
$previous = if ($index -ge 0 -and $index + 1 -lt $tags.Count) { $tags[$index + 1] }
if (!$previous) {
    "No earlier release before $current; nothing to test"
    exit 0
}
# Releases made before the installer have no setup, and no updater to test.
$asset = @(gh release view $previous --json assets --jq '.assets[].name') | Where-Object { $_ -like '*-windows-x64-setup.exe' } | Select-Object -First 1
if (!$asset) {
    "$previous has no setup (it predates the installer); nothing to test"
    exit 0
}
if ([version]$previous.TrimStart('v') -ge [version]$current.TrimStart('v')) {
    "$previous is not older than $current; nothing to test"
    exit 0
}

$dir = "$env:ProgramFiles\ScreenTimeManager"
$exe = "$dir\screen-time-manager.exe"
$uninstallKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{1D3CBE0B-2B68-4A1E-A8F4-96EE93D3D792}_is1'
function InstalledVersion { (Get-ItemProperty $uninstallKey -ErrorAction SilentlyContinue).DisplayVersion }

$download = Join-Path $env:RUNNER_TEMP 'stm-update-test'
gh release download $previous --pattern $asset --dir $download
$p = Start-Process "$download\$asset" -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -PassThru
$null = $p.Handle # keeps the exit code readable
if (!$p.WaitForExit(300000)) { throw "Setup for $previous was still running after 5 minutes" }
if ($p.ExitCode) { throw "Setup for $previous exited with code $($p.ExitCode)" }
$was = InstalledVersion
if ($was -ne $previous.TrimStart('v')) { throw "Installing $previous left version $was installed" }

$since = Get-Date
schtasks /Run /TN ScreenTimeManagerUpdate
if ($LASTEXITCODE) { throw "schtasks /Run /TN ScreenTimeManagerUpdate failed with $LASTEXITCODE" }
$want = $current.TrimStart('v')
for ($i = 0; $i -lt 300; $i++) {
    Start-Sleep 1
    $version = InstalledVersion
    $exeVersion = (Get-Item $exe -ErrorAction SilentlyContinue).VersionInfo.ProductVersion -replace '^(\d+\.\d+\.\d+)\.0$', '$1'
    # Wait for setup itself to finish too: the task's process only starts it.
    $setups = @(Get-CimInstance Win32_Process | Where-Object { $_.Name -like 'screen-time-manager-v*-setup*' })
    $marker = Get-Item "$dir\updating.marker" -ErrorAction SilentlyContinue
    if ($version -eq $want -and $exeVersion -eq $want -and $setups.Count -eq 0 -and $marker -and $marker.LastWriteTime -ge $since) {
        $tasks = @('ScreenTimeManager', 'ScreenTimeManagerWatchdog', 'ScreenTimeManagerUpdate' | ForEach-Object {
                Get-ScheduledTask -TaskName $_ -ErrorAction SilentlyContinue | Where-Object { $_.State -ne 'Disabled' }
            })
        if ($tasks.Count -ne 3) { throw "Updated to $current, but only $($tasks.Count) of the 3 scheduled tasks are registered and enabled" }
        # Up to 1.0.1 the main task let standard accounts read it but not run it, so the watchdog
        # (which runs non-elevated) couldn't relaunch the app; the update must replace that.
        $service = New-Object -ComObject Schedule.Service
        $service.Connect()
        $sddl = $service.GetFolder('\').GetTask('ScreenTimeManager').GetSecurityDescriptor(4)
        $granted = 0
        foreach ($ace in (New-Object Security.AccessControl.RawSecurityDescriptor $sddl).DiscretionaryAcl) {
            # Everyone, Authenticated Users, Interactive, Users.
            if ([string]$ace.AceQualifier -eq 'AccessAllowed' -and @('S-1-1-0', 'S-1-5-11', 'S-1-5-4', 'S-1-5-32-545') -contains $ace.SecurityIdentifier.Value) {
                $granted = $granted -bor $ace.AccessMask
            }
        }
        # FILE_READ_DATA and FILE_EXECUTE.
        if (($granted -band 0x21) -ne 0x21) { throw "Updated to $current, but standard accounts still can't run the ScreenTimeManager task: $sddl" }
        "Updated $previous to $current through the update task; ScreenTimeManager task security: $sddl"
        exit 0
    }
}
foreach ($log in "$dir\updates\update.log", "$dir\updates\update.log.old") {
    if (Test-Path $log) { "---- $log"; Get-Content $log -Tail 80 }
}
throw "After 5 minutes the installed version is $version (exe $exeVersion), not $want"
