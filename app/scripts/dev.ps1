# Build and run helper for Huck's Voice to Text on Windows - the twin of dev.sh.
#
# Three things about this machine and this drive have to be set up before cargo runs, and all
# are easy to forget, so they live here rather than in a README step.
#
#   1. BUILD OFF THE SSD. The project source sits on an exFAT volume. Rust build output goes to
#      the local disk instead (C:\hvb - at the top of the drive on purpose: see step 5).
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
# Usage: app\scripts\dev.ps1 [build|build-release|run|test|bench] [extra cargo args]   (default: run)

param(
    [Parameter(Position = 0)] [string] $Command = 'run',
    [Parameter(Position = 1, ValueFromRemainingArguments = $true)] [string[]] $Rest = @()
)

$ErrorActionPreference = 'Stop'
$appDir = Split-Path -Parent $PSScriptRoot

# 1. Build output on the local disk, never on the exFAT drive.
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = Join-Path $env:SystemDrive '\hvb' }

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

# 5. VULKAN, AND NINJA TO BUILD IT. Recognition runs on the PC's graphics card through Vulkan
#    (desktop\Cargo.toml), and whisper.cpp needs the Vulkan SDK to build that: its headers, its
#    import library and its shader compiler. Only to build - nothing of the SDK is needed to run.
#    Install it once, no administrator needed:
#      vulkansdk-windows-X64-<version>.exe --root "%LOCALAPPDATA%\VulkanSDK\<version>"
#          --accept-licenses --default-answer --confirm-command install copy_only=1
#
#    Visual Studio's own generator cannot build it: whisper.cpp builds its shader generator as a
#    project inside the project, and MSBuild's folders for that pass Windows' 260-character limit
#    even from a build folder as short as C:\hvk (found 2026-10-05). Ninja makes no such folders;
#    it needs the compiler's environment (vcvars64), which is brought in here. Even so the build
#    folder must be at the top of the drive: from %LOCALAPPDATA%\hvtt-build\target the same
#    sub-build's compiler test still passed 260 characters, and from %LOCALAPPDATA%\hvb a debug
#    build fitted and a release build - seven characters deeper - did not. Hence C:\hvb.
if (-not $env:VULKAN_SDK) {
    $sdk = @((Join-Path $env:LOCALAPPDATA 'VulkanSDK'), 'C:\VulkanSDK') | Where-Object { Test-Path $_ } |
        ForEach-Object { Get-ChildItem -Path $_ -Directory } |
        Where-Object { Test-Path (Join-Path $_.FullName 'Lib\vulkan-1.lib') } |
        Sort-Object { [version] ($_.Name -replace '[^\d.]', '') } | Select-Object -Last 1
    if (-not $sdk) { throw 'The Vulkan SDK is missing - see step 5 in app\scripts\dev.ps1.' }
    $env:VULKAN_SDK = $sdk.FullName
}
$env:PATH = "$(Join-Path $env:VULKAN_SDK 'Bin');$env:PATH"
if (-not $env:CMAKE_GENERATOR) {
    $ninja = Join-Path $vs 'Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja'
    if (-not (Test-Path (Join-Path $ninja 'ninja.exe'))) { throw 'Ninja is missing - it comes with "C++ CMake tools for Windows" in the Build Tools.' }
    if (-not $env:VSCMD_VER) {
        $vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
        # vcvars grumbles on stderr about tools it does not need; 'Stop' would take that as failure.
        cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
            if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($matches[1])" -Value $matches[2] }
        }
    }
    $env:PATH = "$ninja;$env:PATH"
    $env:CMAKE_GENERATOR = 'Ninja'
}

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
        # How well quick dictations are recognised, and how long they take:
        # desktop\examples\accuracy.rs explains the cases. Built for THIS processor, like every
        # dev.ps1 build; for what the public download does, set GGML_NATIVE=OFF and a
        # CARGO_TARGET_DIR of its own first (whisper.cpp is not rebuilt for a changed flag).
        'bench'         { cargo run --release -p hvtt-desktop --example accuracy -- @Rest }
        default         { throw 'usage: app\scripts\dev.ps1 [build|build-release|run|test|bench]' }
    }
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
finally {
    Pop-Location
}
