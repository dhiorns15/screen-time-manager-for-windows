# Builds the installer into target\installer. Run after `cargo build --release`:
#
#   powershell -File installer\build.ps1
#
# -Bin       the folder with screen-time-manager.exe, relative to this folder (CI: ..\target\debug)
# -Version   package the exe as this X.Y.Z instead of its own version. Only for tests, which need
#            a setup newer than the installed one to update to; the exe itself is unchanged.
# -OutputDir write the setup there instead of target\installer (the release takes everything there)
#
# Needs Inno Setup 6 (`winget install JRSoftware.InnoSetup`); CI installs it with Chocolatey.
param(
    [string]$Bin = '..\target\release',
    [string]$Version = '',
    [string]$OutputDir = ''
)

$iscc = @(
    (Get-Command iscc -ErrorAction SilentlyContinue).Source
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (!$iscc -and $env:CI) {
    choco install innosetup --no-progress -y
    $iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
}
if (!$iscc) {
    Write-Error 'Inno Setup 6 not found. Install it with: winget install JRSoftware.InnoSetup'
    exit 1
}
$options = @("/DBin=$Bin")
if ($Version) {
    if ($Version -notmatch '^\d+\.\d+\.\d+$') {
        Write-Error "-Version must be X.Y.Z, not $Version"
        exit 1
    }
    $options += "/DAppVersion=$Version"
}
if ($OutputDir) {
    $options += "/O$($ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDir))"
}
& $iscc @options "$PSScriptRoot\screen-time-manager.iss"
exit $LASTEXITCODE
