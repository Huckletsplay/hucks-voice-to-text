# Build the public Windows beta: an installer and its SHA-256 checksum, verified, in artifacts\ -
# the twin of release.sh.
#
#   app\scripts\release.ps1 -UnsignedBeta
#
# Produces, for the version in desktop\tauri.conf.json:
#   artifacts\windows\release\HucksVoiceToText-<version>-windows-x64-setup.exe
#   artifacts\windows\release\HucksVoiceToText-<version>-windows-x64-setup.exe.sha256.txt
#
# These names are the contract with the in-app updater (desktop\src\update.rs); change one and the
# other must change with it.
#
# UNSIGNED BETA. The installer and program carry no code signature, so Windows SmartScreen shows
# "Windows protected your PC" on first run until the user chooses More info > Run anyway. Say so
# on the download page.
#
# A release is NOT dev.ps1's build with a different output folder. That build is for this machine
# only: it is tuned to this processor and links the Visual C++ runtime dynamically. This one:
#
#   - builds in a throwaway folder, with every absolute path Rust and whisper.cpp would embed
#     (the Cargo cache, the build folder, this project, the user profile) replaced by a neutral
#     name, and REFUSES to package if a user profile, the project drive, a source path or an
#     email address survives in the executable;
#   - builds whisper.cpp for AVX2-class processors (Intel 2013 on, AMD 2015 on) instead of this
#     one - the program says so on older machines instead of crashing - and checks it did;
#   - links the C runtime statically, so no Visual C++ Redistributable is needed, and checks it.
#
# The speech model goes inside the installer. It is taken from HVTT_RELEASE_MODEL, or else this
# PC's app-data copy (app\scripts\fetch-model.ps1).

param([switch] $UnsignedBeta)

if (-not $UnsignedBeta) {
    throw "usage: app\scripts\release.ps1 -UnsignedBeta   (code signing is not set up yet)"
}
# Native tools (cargo, ISCC, dumpbin) report progress on stderr, which Windows PowerShell turns
# into error records when output is redirected. Every step below checks its own result and throws.
$ErrorActionPreference = 'Continue'
$PSDefaultParameterValues['*:ErrorAction'] = 'Stop'

$appDir = Split-Path -Parent $PSScriptRoot
$projectRoot = Split-Path -Parent $appDir
$conf = Get-Content -LiteralPath (Join-Path $appDir 'desktop\tauri.conf.json') -Raw | ConvertFrom-Json
$version = $conf.version
$name = "HucksVoiceToText-$version-windows-x64-setup"
$model = if ($env:HVTT_RELEASE_MODEL) { $env:HVTT_RELEASE_MODEL } else {
    Join-Path $env:LOCALAPPDATA "Huck's Voice to Text\models\ggml-base.en.bin"
}
if (-not (Test-Path -LiteralPath $model)) { throw "No speech model at $model - run app\scripts\fetch-model.ps1" }
$license = Join-Path $projectRoot 'LICENSE'
$icon = Join-Path $appDir 'desktop\icons\icon.ico'
$out = Join-Path $projectRoot 'artifacts\windows\release'

$inno = @(
    (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe'),
    'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
    'C:\Program Files\Inno Setup 6\ISCC.exe'
) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
if (-not $inno) { throw 'Inno Setup 6 was not found. Install JRSoftware.InnoSetup before building a release.' }

# --- 1. nothing ships without the suite passing ------------------------------------------------
Write-Host "Running the test suite before packaging..."
& (Join-Path $PSScriptRoot 'dev.ps1') test
if ($LASTEXITCODE -ne 0) { throw 'Tests failed; the release was NOT built.' }

# --- 2. the anonymous, portable build --------------------------------------------------------
# Not under %TEMP%: MSBuild refuses to build there ("cannot reside under the Temporary directory").
# And short: whisper.cpp's CMake build nests deep, and MSBuild cannot create a folder past about
# 248 characters. `hvr` keeps the deepest path shorter than dev.ps1's own (measured 2026-09-26).
$work = Join-Path $env:LOCALAPPDATA 'hvr'
if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
if ($work -match '\s') { throw "The temporary folder path contains a space ($work); the C compiler flags below cannot carry it." }
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }

$saved = @{}
foreach ($v in 'CARGO_TARGET_DIR', 'CARGO_ENCODED_RUSTFLAGS', 'CFLAGS', 'CXXFLAGS', 'GGML_NATIVE') {
    $saved[$v] = [Environment]::GetEnvironmentVariable($v, 'Process')
}
New-Item -ItemType Directory -Path $work | Out-Null
try {
    $target = Join-Path $work 't'
    $env:CARGO_TARGET_DIR = $target
    # Later remaps win, so the most specific come last. The unit separator keeps each flag one
    # argument even where a path has a space in it ("Project Playground").
    $env:CARGO_ENCODED_RUSTFLAGS = @(
        '-Ctarget-feature=+crt-static',
        "--remap-path-prefix=$env:USERPROFILE=home",
        "--remap-path-prefix=$cargoHome=cargo",
        "--remap-path-prefix=$projectRoot=src",
        "--remap-path-prefix=$work=build"
    ) -join [char]0x1f
    # whisper.cpp is compiled from a copy inside the build folder; trim that (and the Cargo cache)
    # from the file names its assertions carry.
    $env:CFLAGS = "/d1trimfile:$work\ /d1trimfile:$cargoHome\"
    $env:CXXFLAGS = $env:CFLAGS
    # AVX2 baseline: with GGML_NATIVE off, ggml enables SSE4.2, AVX, AVX2 and BMI2 (FMA and F16C
    # come with AVX2 under MSVC) and nothing newer.
    $env:GGML_NATIVE = 'OFF'

    Write-Host "Building Huck's Voice to Text $version for release..."
    # No --target: it would add a folder level the path length cannot afford. The static runtime
    # and the remapping then apply to the build scripts as well, which is harmless.
    & (Join-Path $PSScriptRoot 'dev.ps1') build-release --features custom-protocol
    if ($LASTEXITCODE -ne 0) { throw 'The release build failed.' }
    $exe = Join-Path $target 'release\hvtt-desktop.exe'
    if (-not (Test-Path -LiteralPath $exe)) { throw "No executable at $exe" }

    # --- 3. built for the baseline processor, not this one ------------------------------------
    $cache = Get-ChildItem -LiteralPath (Join-Path $target 'release\build') -Directory -Filter 'whisper-rs-sys-*' |
        ForEach-Object { Join-Path $_.FullName 'out\build\CMakeCache.txt' } | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $cache) { throw 'Could not find whisper.cpp''s build settings to check the processor target.' }
    $settings = Get-Content -LiteralPath $cache -Raw
    if ($settings -notmatch 'GGML_NATIVE:BOOL=OFF' -or $settings -notmatch 'GGML_AVX2:BOOL=ON' -or $settings -match 'GGML_AVX512:BOOL=ON') {
        throw 'whisper.cpp was not built for the AVX2 baseline; the release was NOT packaged.'
    }
    $cpuProject = Join-Path (Split-Path -Parent $cache) 'ggml\src\ggml-cpu.vcxproj'
    if ((Get-Content -LiteralPath $cpuProject -Raw) -match 'AdvancedVectorExtensions512') {
        throw 'whisper.cpp uses AVX-512 instructions; the release was NOT packaged.'
    }
    Write-Host 'Processor target: AVX2 baseline, no AVX-512.'

    # --- 4. privacy audit ----------------------------------------------------------------------
    # Nothing about this machine, this drive, or its owner may reach a public download.
    $bytes = [IO.File]::ReadAllBytes($exe)
    $texts = @([Text.Encoding]::ASCII.GetString($bytes), [Text.Encoding]::Unicode.GetString($bytes))
    $account = [regex]::Escape($env:USERNAME)
    $forbidden = [ordered]@{
        'a user profile'      = '(?i)[A-Za-z]:\\Users\\'
        'this PC''s account'  = "(?i)[\\/]$account[\\/]"
        'the project drive'   = '(?i)Project Playground[\\/]'
        'a source path'       = '(?i)[A-Za-z]:\\[^\x00]{0,160}\.(pdb|rs|c|cc|cpp|h|hpp)\b'
        'an email address'    = '[A-Za-z0-9._%+-]+@[A-Za-z][A-Za-z0-9-]*(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}'
    }
    $leaks = @()
    foreach ($kind in $forbidden.Keys) {
        foreach ($text in $texts) {
            $match = [regex]::Match($text, $forbidden[$kind])
            if ($match.Success) { $leaks += ('  {0}: {1}' -f $kind, $match.Value); break }
        }
    }
    if ($leaks.Count -gt 0) {
        throw ("The release build leaks private information and was NOT packaged:`n" + ($leaks -join "`n"))
    }
    Write-Host 'Privacy audit passed: no user profile, account, project drive, source path or email in the executable.'

    # --- 5. no Visual C++ runtime needed -------------------------------------------------------
    $vs = & (Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe') -latest -products * -property installationPath
    $dumpbin = Get-ChildItem -Path (Join-Path $vs 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe') | Select-Object -Last 1
    $imports = & $dumpbin.FullName /nologo /dependents $exe | Where-Object { $_ -match '\.dll' } | ForEach-Object { $_.Trim() }
    $runtime = $imports | Where-Object { $_ -match '(?i)^(vcruntime|msvcp|vcomp|api-ms-win-crt-)' }
    if ($runtime) { throw ("The executable still needs the Visual C++ runtime ($($runtime -join ', ')); the release was NOT packaged.") }
    Write-Host 'Runtime: static - no Visual C++ Redistributable needed.'

    # --- 6. the installer ----------------------------------------------------------------------
    $stage = Join-Path $work 'stage'
    New-Item -ItemType Directory -Path $stage | Out-Null
    $stagedExe = Join-Path $stage 'HucksVoiceToText.exe'
    Copy-Item -LiteralPath $exe -Destination $stagedExe
    & $inno /Qp ("/DAppVersion=$version") ("/DSourceExe=$stagedExe") ("/DSourceModel=$model") `
        ("/DSourceLicense=$license") ("/DOutputDir=$work") ("/DSetupIcon=$icon") (Join-Path $PSScriptRoot 'HucksVoiceToText.iss')
    if ($LASTEXITCODE -ne 0) { throw 'The installer did not compile.' }
    $installer = Join-Path $work "$name.exe"
    if (-not (Test-Path -LiteralPath $installer)) { throw 'The installer compiler produced no setup executable.' }

    # One line, the file name only - the form the updater requires.
    $hash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLower()
    $checksum = "$installer.sha256.txt"
    [IO.File]::WriteAllText($checksum, "$hash  $name.exe`n", (New-Object Text.UTF8Encoding $false))

    New-Item -ItemType Directory -Force -Path $out | Out-Null
    Copy-Item -LiteralPath $installer, $checksum -Destination $out -Force
    $copied = (Get-FileHash -LiteralPath (Join-Path $out "$name.exe") -Algorithm SHA256).Hash.ToLower()
    if ($copied -ne $hash) { throw 'The copied installer does not match its checksum.' }

    Write-Host ''
    Write-Host 'Release built:'
    Write-Host ('  {0}  ({1:N0} bytes)' -f (Join-Path $out "$name.exe"), (Get-Item -LiteralPath $installer).Length)
    Write-Host ('  {0}' -f (Join-Path $out "$name.exe.sha256.txt"))
    Write-Host "  SHA-256 $hash"
}
finally {
    foreach ($v in $saved.Keys) { [Environment]::SetEnvironmentVariable($v, $saved[$v], 'Process') }
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
