# Download a Whisper model into the Windows app-data directory - the twin of fetch-model.sh.
#
#   app\scripts\fetch-model.ps1 [model]      (default: base.en)
#
# Models live in %LOCALAPPDATA%\Huck's Voice to Text\models, never in the project and never on
# the SSD: the program must keep working with the drive unplugged. They are MIT licensed, from
# the whisper.cpp project.

param([string] $Model = 'base.en')

$ErrorActionPreference = 'Stop'
$file = "ggml-$Model.bin"
$url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$file"
$dest = Join-Path $env:LOCALAPPDATA "Huck's Voice to Text\models"
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$target = Join-Path $dest $file
if (Test-Path -LiteralPath $target) {
    Write-Host "Already present: $target"
    exit 0
}

Write-Host "Downloading $file to $dest"
Write-Host "(models are MIT licensed, from the whisper.cpp project)"
$part = "$target.part"
# Windows' own curl. Its progress bar is on stderr, which Windows PowerShell would otherwise
# report as an error when output is redirected; the exit code is what decides.
$ErrorActionPreference = 'Continue'
& (Join-Path $env:SystemRoot 'System32\curl.exe') --fail --location --progress-bar --proto '=https' --output $part $url
$ErrorActionPreference = 'Stop'
if ($LASTEXITCODE -ne 0) {
    Remove-Item -LiteralPath $part -ErrorAction SilentlyContinue
    throw "Download failed ($LASTEXITCODE)."
}
Move-Item -LiteralPath $part -Destination $target
Write-Host ("Done: {0} ({1:N0} bytes)" -f $target, (Get-Item -LiteralPath $target).Length)
