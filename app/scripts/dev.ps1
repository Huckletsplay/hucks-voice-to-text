# Build and run helper for Huck's Voice to Text on Windows - the twin of dev.sh.
#
# Three things about this machine and this drive have to be set up before cargo runs, and all
# are easy to forget, so they live here rather than in a README step.
#
#   1. BUILD OFF THE SSD. The project source sits on an exFAT volume. Rust build output goes to
#      the local disk instead (%LOCALAPPDATA%\hvtt-build\target).
#
#   2. CMAKE. whisper.cpp is built with CMake, and the copy that comes with the Visual Studio
#      Build Tools is not on PATH outside a Developer prompt. It is found with vswhere.
#
#   3. LIBCLANG. whisper-rs generates its bindings with bindgen, which needs libclang.dll -
#      also part of the Build Tools ("C++ Clang tools for Windows").
#
# Needs: Rust (rustup, MSVC toolchain) and Visual Studio 2022 Build Tools with the C++ workload,
# CMake and Clang components. See app/README.md.
#
# Usage: app\scripts\dev.ps1 [build|build-release|run|test] [extra cargo args]   (default: run)

param(
    [Parameter(Position = 0)] [string] $Command = 'run',
    [Parameter(Position = 1, ValueFromRemainingArguments = $true)] [string[]] $Rest = @()
)

$ErrorActionPreference = 'Stop'
$appDir = Split-Path -Parent $PSScriptRoot

# 1. Build output on the local disk, never on the exFAT drive.
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $env:LOCALAPPDATA 'hvtt-build\target' }

# Rust, even in a shell opened before it was installed.
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ((Test-Path $cargoBin) -and ($env:PATH -notlike "*$cargoBin*")) { $env:PATH = "$cargoBin;$env:PATH" }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'Rust is not installed - get it from https://rustup.rs' }

# 2 and 3. CMake and libclang from the Visual Studio Build Tools.
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = if (Test-Path $vswhere) { & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath } else { $null }
if (-not $vs) { throw 'Visual Studio Build Tools with the C++ workload are not installed.' }
if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
    $cmake = Join-Path $vs 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin'
    if (-not (Test-Path (Join-Path $cmake 'cmake.exe'))) { throw 'CMake is missing - add "C++ CMake tools for Windows" to the Build Tools.' }
    $env:PATH = "$cmake;$env:PATH"
}
if (-not $env:LIBCLANG_PATH) {
    $clang = Join-Path $vs 'VC\Tools\Llvm\x64\bin'
    if (-not (Test-Path (Join-Path $clang 'libclang.dll'))) { throw 'libclang is missing - add "C++ Clang tools for Windows" to the Build Tools.' }
    $env:LIBCLANG_PATH = $clang
}

# 4. OPTIMISE WHISPER.CPP. With MSVC, the `cmake` crate replaces CMake's Release flags with its
#    own, which carry no /O2 - so a cargo --release build compiled whisper.cpp unoptimised and
#    recognised four times slower than a debug build (4.1 s against 1.2 s for one sentence,
#    measured 2026-09-25). whisper-rs passes any CMAKE_* variable through, and the crate leaves
#    alone flags that are already defined, so the standard ones go in here.
#
#    whisper.cpp is also built for THIS processor (GGML_NATIVE): on this PC that means AVX-512.
#    Right for a local install; a public release must be built for a baseline CPU instead, or it
#    will crash on machines without those instructions.
if (-not $env:CMAKE_C_FLAGS_RELEASE) { $env:CMAKE_C_FLAGS_RELEASE = '/O2 /Ob2 /DNDEBUG' }
if (-not $env:CMAKE_CXX_FLAGS_RELEASE) { $env:CMAKE_CXX_FLAGS_RELEASE = '/O2 /Ob2 /DNDEBUG' }

Push-Location $appDir
# Cargo reports progress on stderr. Windows PowerShell turns redirected stderr into error
# records, which 'Stop' would treat as a failure; the exit code is what decides.
$ErrorActionPreference = 'Continue'
try {
    switch ($Command) {
        'build'         { cargo build -p hvtt-desktop @Rest }
        # Extra arguments let a packager enable Tauri's production custom-protocol feature.
        'build-release' { cargo build --release -p hvtt-desktop @Rest }
        # Extra arguments go to the test binaries, e.g. `dev.ps1 test --ignored` or
        # `dev.ps1 test update_live --ignored`. (PowerShell eats a bare `--` itself, so this
        # script supplies it rather than asking for it as dev.sh does.)
        'test'          { cargo test --workspace -- @Rest }
        'run'           { cargo run -p hvtt-desktop @Rest }
        default         { throw 'usage: app\scripts\dev.ps1 [build|build-release|run|test]' }
    }
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
finally {
    Pop-Location
}
