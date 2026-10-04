# Download a Whisper model into the Windows app-data directory - the twin of fetch-model.sh.
#
#   app\scripts\fetch-model.ps1 [model]      no model: base.en ("Quick", the one built in on
#                                             Windows) and the voice detector silero-v5.1.2
#
#   Or one of: tiny.en ("Tiny"), base.en ("Quick"), small.en ("Better"), medium.en-q5_0 ("Medium"),
#   large-v3-turbo-q5_0 ("Best"), large-v3-q5_0 ("Large"), silero-v5.1.2.
#
# Models live in %LOCALAPPDATA%\Huck's Voice to Text\models, never in the project and never on
# the SSD: the program must keep working with the drive unplugged. They are MIT licensed, from
# the whisper.cpp project.

param([string] $Model = '')

$ErrorActionPreference = 'Stop'
if ($Model -eq '') {
    & $PSCommandPath 'base.en'
    & $PSCommandPath 'silero-v5.1.2'
    exit 0
}
$file = "ggml-$Model.bin"
# The files the program knows, pinned as hvtt_core::models pins them: one commit, size, SHA-256.
$pinned = 'https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1'
$known = @{
    'tiny.en'             = @(77704715, '921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f', "$pinned/$file")
    'medium.en-q5_0'      = @(539225533, '76733e26ad8fe1c7a5bf7531a9d41917b2adc0f20f2e4f5531688a8c6cd88eb0', "$pinned/$file")
    'large-v3-q5_0'       = @(1081140203, 'd75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1', "$pinned/$file")
    'base.en'             = @(147964211, 'a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002', "$pinned/$file")
    'small.en'            = @(487614201, 'c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d', "$pinned/$file")
    'large-v3-turbo-q5_0' = @(574041195, '394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2', "$pinned/$file")
    'silero-v5.1.2'       = @(885098, '29940d98d42b91fbd05ce489f3ecf7c72f0a42f027e4875919a28fb4c04ea2cf', "https://huggingface.co/ggml-org/whisper-vad/resolve/9ffd54a1e1ee413ddf265af9913beaf518d1639b/$file")
}
if ($known.ContainsKey($Model)) {
    $size, $sha, $url = $known[$Model]
} else {
    $size, $sha = $null, $null
    $url = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/$file"
    Write-Host "Note: $Model is not one the program knows; it is not checked."
}
# A file is good if it is the published one (when known) - an empty or broken one is not.
function Test-Good([string] $path) {
    if (-not (Test-Path -LiteralPath $path)) { return $false }
    $item = Get-Item -LiteralPath $path
    if ($item.Length -eq 0) { return $false }
    if ($null -eq $sha) { return $true }
    return ($item.Length -eq $size) -and ((Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLower() -eq $sha)
}
$dest = Join-Path $env:LOCALAPPDATA "Huck's Voice to Text\models"
New-Item -ItemType Directory -Force -Path $dest | Out-Null

$target = Join-Path $dest $file
if (Test-Path -LiteralPath $target) {
    if (Test-Good $target) {
        Write-Host "Already present: $target"
        exit 0
    }
    Write-Host "The copy in $dest is not the published file - downloading it again."
    Remove-Item -LiteralPath $target
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
if (-not (Test-Good $part)) {
    Remove-Item -LiteralPath $part -ErrorAction SilentlyContinue
    throw "The download is not the published file (size or SHA-256), so it was deleted."
}
Move-Item -LiteralPath $part -Destination $target
Write-Host ("Done: {0} ({1:N0} bytes)" -f $target, (Get-Item -LiteralPath $target).Length)
