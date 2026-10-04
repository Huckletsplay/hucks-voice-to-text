# Install Huck's Voice to Text on this Windows PC as an ordinary program - the twin of install.sh.
#
#   app\scripts\install.ps1            build a release, install it, start it
#   app\scripts\install.ps1 -NoBuild   install the release already built
#
# The program is copied to %LOCALAPPDATA%\Programs\HucksVoiceToText - the same per-user place
# Huck's Snip 'n' Clip uses - with a Start menu entry. It must never run from the SSD: the copy on
# this machine keeps working with the drive unplugged. The speech model is the one in
# %LOCALAPPDATA%\Huck's Voice to Text\models (app\scripts\fetch-model.ps1); settings and drafts
# live beside it and survive a reinstall.
#
# This is the developer install, not the public installer: nothing is signed, and there is no
# entry in Installed Apps. To remove it, quit it from the tray H and delete the folder and the
# Start menu entry.

param([switch] $NoBuild)

$ErrorActionPreference = 'Stop'
$name = "Huck's Voice to Text"
$targetDir = Join-Path $env:LOCALAPPDATA 'Programs\HucksVoiceToText'
$targetExe = Join-Path $targetDir 'HucksVoiceToText.exe'
$targetRoot = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $env:LOCALAPPDATA 'hvtt-build\target' }
$built = Join-Path $targetRoot 'release\hvtt-desktop.exe'

if (-not $NoBuild) {
    # The production context: the UI is served from inside the program, not the build folder.
    & (Join-Path $PSScriptRoot 'dev.ps1') build-release --features custom-protocol
    if ($LASTEXITCODE -ne 0) { throw "Build failed ($LASTEXITCODE)." }
}
if (-not (Test-Path -LiteralPath $built)) { throw "No release build at $built - run without -NoBuild." }

$model = Join-Path $env:LOCALAPPDATA "$name\models\ggml-base.en.bin"
if (-not (Test-Path -LiteralPath $model)) {
    Write-Warning "No speech model yet - run app\scripts\fetch-model.ps1 before dictating."
}

# The running copy holds its file open; quit it first, as the tray's Quit would.
Get-Process -Name 'HucksVoiceToText', 'hvtt-desktop' -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 500

New-Item -ItemType Directory -Force -Path $targetDir | Out-Null
Copy-Item -LiteralPath $built -Destination $targetExe -Force

$startMenu = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\$name.lnk"
$shell = New-Object -ComObject WScript.Shell
$link = $shell.CreateShortcut($startMenu)
$link.TargetPath = $targetExe
$link.WorkingDirectory = $targetDir
$link.Description = 'Local, offline dictation that never loses your words.'
$link.Save()

Start-Process -FilePath $targetExe -WorkingDirectory $targetDir
$size = (Get-Item -LiteralPath $targetExe).Length
Write-Host ("Installed {0} ({1:N0} bytes) and started it. Its H is in the notification area." -f $targetExe, $size)
