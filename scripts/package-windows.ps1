#!/usr/bin/env pwsh
# Build, sign and package every Windows application from a source checkout:
# the server (carrying the web client), the TUI, and the desktop installer.
# It is the local counterpart of the `windows` jobs in cross-build.yml and
# desktop-build.yml, and produces the same files with the same names.
#
#   scripts/package-windows.ps1 [-OutDir dist-windows] [-NoSign]
#                               [-SkipServer] [-SkipTui] [-SkipDesktop]
#
# Output, in -OutDir (default dist-windows/):
#   axon-server-windows.zip, axon-tui-windows.zip, axon-windows.zip
#   Axon_<version>_x64-setup.exe
#
# **Signing is by presence** (ADR 0119), as in CI. When
# ARTIFACT_SIGNING_ENDPOINT, ARTIFACT_SIGNING_ACCOUNT and
# ARTIFACT_SIGNING_PROFILE are all set, the executables are signed before they
# are zipped, and Tauri signs the shell, the uninstaller and the installer
# through scripts/sign-windows.ps1. With them unset the outputs are unsigned,
# as is any run with -NoSign. Authentication is whatever
# DefaultAzureCredential finds: `az login` is the usual one.
#
# Requires: Rust (MSVC), Node + pnpm, and for the desktop installer the
# prerequisites in clients/web/src-tauri/README.md. Git for Windows' `bash` is
# used, when present, for the embedded-web assertion that CI runs.
# Not a gate and not run by CI.

#Requires -Version 7
[CmdletBinding()]
param(
    [string]$OutDir = 'dist-windows',
    [switch]$NoSign,
    [switch]$SkipServer,
    [switch]$SkipTui,
    [switch]$SkipDesktop
)

$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'package-windows.ps1 builds Windows binaries and runs on Windows only' }

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$signer = Join-Path $PSScriptRoot 'sign-windows.ps1'
Set-Location $repo

function Step([string]$Message) { Write-Host "`n==> $Message" -ForegroundColor Cyan }

function Invoke-Checked {
    $exe, $rest = $args
    & $exe @rest
    if ($LASTEXITCODE -ne 0) { throw "$exe $($rest -join ' ') failed (exit $LASTEXITCODE)" }
}

function Require-Command([string]$Name, [string]$Hint) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) { throw "$Name not found. $Hint" }
}

$buildServer = -not $SkipServer
$buildTui = -not $SkipTui
$buildDesktop = -not $SkipDesktop
if (-not ($buildServer -or $buildTui -or $buildDesktop)) { throw 'nothing to build' }

# --- Prerequisites, checked before any build starts ---

Require-Command cargo 'Install Rust (rustup) with the MSVC toolchain.'
if ($buildServer -or $buildDesktop) { Require-Command pnpm 'Install pnpm (corepack enable).' }
if ($buildServer -or $buildDesktop) { Require-Command node 'Install Node 24.' }

# `bash` on PATH is often WSL's launcher (System32\bash.exe), which cannot
# translate a Windows checkout path. Take Git for Windows' bash explicitly.
function Find-GitBash {
    $candidates = @()
    $git = Get-Command git -ErrorAction SilentlyContinue
    if ($git) {
        $gitRoot = Split-Path (Split-Path $git.Source)
        $candidates += Join-Path $gitRoot 'bin/bash.exe'
        $candidates += Join-Path $gitRoot 'usr/bin/bash.exe'
    }
    foreach ($base in $env:ProgramFiles, ${env:ProgramFiles(x86)}, (Join-Path $env:LOCALAPPDATA 'Programs')) {
        if ($base) { $candidates += Join-Path $base 'Git/bin/bash.exe' }
    }
    $candidates | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
}
$bash = Find-GitBash
# Scripts run by that bash start with `#!/usr/bin/env bash`, which looks `bash`
# up on PATH and would otherwise find WSL's launcher again.
if ($bash) { $env:PATH = (Split-Path $bash) + [IO.Path]::PathSeparator + $env:PATH }

$serviceVars = 'ARTIFACT_SIGNING_ENDPOINT', 'ARTIFACT_SIGNING_ACCOUNT', 'ARTIFACT_SIGNING_PROFILE'
$missing = @($serviceVars | Where-Object { -not [Environment]::GetEnvironmentVariable($_) })
$signing = (-not $NoSign) -and ($missing.Count -eq 0)

if ($NoSign) {
    Write-Host 'Signing disabled (-NoSign): the outputs will be unsigned.'
    # A configured shell must not sign behind our back through the child
    # processes (Tauri calls the signer itself).
    foreach ($v in $serviceVars + 'SIGN_SIGNTOOL', 'SIGN_DLIB', 'SIGN_METADATA') {
        [Environment]::SetEnvironmentVariable($v, $null)
    }
} elseif ($signing) {
    Step 'Preparing Windows signing'
    & $signer setup
    if ($LASTEXITCODE -ne 0) { throw 'signing setup failed' }
    if ((Get-Command az -ErrorAction SilentlyContinue)) {
        az account show --output none 2>$null
        if ($LASTEXITCODE -ne 0) {
            Write-Warning 'az is not logged in. Signing will fail unless another DefaultAzureCredential source (environment, Visual Studio) applies; run `az login` first.'
        }
    }
} else {
    Write-Host "Signing is not configured ($($missing -join ', ') not set): the outputs will be unsigned."
}

$out = [IO.Path]::GetFullPath($OutDir, $repo)
New-Item -ItemType Directory -Force -Path $out | Out-Null
Get-ChildItem -Path (Join-Path $out '*') -Include '*.zip', '*-setup.exe' -File | Remove-Item

# --- Version and web bundle ---

if ($buildServer -or $buildDesktop) {
    Step 'Syncing the bundle version'
    # The same operation as `scripts/bundle-version.sh sync`: the release
    # version is axon-server's Cargo version, and clients/web/package.json
    # carries it into the installer's name and metadata. Done here rather than
    # through bash so a Windows build needs neither jq nor the right bash; a
    # textual replace of the one field keeps the file's formatting.
    $meta = cargo metadata --no-deps --format-version 1 --manifest-path (Join-Path $repo 'Cargo.toml') | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $version = ($meta.packages | Where-Object name -eq 'axon-server').version
    if (-not $version) { throw 'axon-server version not found in cargo metadata' }
    $packageJson = Join-Path $repo 'clients/web/package.json'
    $text = [IO.File]::ReadAllText($packageJson)
    $pattern = [regex]::new('(?m)^(\s*"version"\s*:\s*")[^"]*(")')
    $updated = $pattern.Replace($text, "`${1}$version`${2}", 1)
    if ($updated -ne $text) {
        [IO.File]::WriteAllText($packageJson, $updated, [Text.UTF8Encoding]::new($false))
        Write-Host "Bundle version set to $version."
    } else {
        Write-Host "Bundle version already $version."
    }

    # A release build is the commit it was cut from. The version sync above
    # edits package.json, which would otherwise make the build id `-dirty`.
    if (-not $env:VITE_AXON_WEB_VERSION) {
        $env:VITE_AXON_WEB_VERSION = (git rev-parse --short=12 HEAD).Trim()
    }

    Step 'Building the web client'
    Invoke-Checked pnpm --dir clients/web install --frozen-lockfile
    Invoke-Checked pnpm --dir clients/web build
    $dist = Join-Path $repo 'clients/web/dist'
    if (-not (Test-Path (Join-Path $dist 'index.html'))) { throw "no web bundle at $dist" }
    # An explicit path makes a missing bundle a build error (ADR 0116).
    $env:AXON_WEB_DIST = $dist
}

# --- Server and TUI ---

$bin = Join-Path $repo 'target/release'
$thirdparty = @()
if (Test-Path build/THIRDPARTY) { $thirdparty += 'build/THIRDPARTY' }
else { Write-Warning 'build/THIRDPARTY missing: the zips will not carry the license disclosure.' }
$thirdparty += 'LICENSE.md'

if ($buildServer -or $buildTui) {
    Step 'Building the server and TUI'
    $cargo = @('build', '--release')
    if ($buildServer) { $cargo += @('-p', 'axon-server', '--features', 'axon-server/embed-web') }
    if ($buildTui) { $cargo += @('-p', 'axon-tui') }
    Invoke-Checked cargo @cargo

    $exes = @()
    if ($buildServer) {
        if ($bash) {
            Invoke-Checked $bash scripts/assert-web-embedded.sh ((Join-Path $bin 'axon-server.exe') -replace '\\', '/') ($dist -replace '\\', '/')
        } else {
            Write-Warning 'Git for Windows bash not found (WSL bash does not count): skipping the check that the server carries the web client.'
        }
        $exes += Join-Path $bin 'axon-server.exe'
    }
    if ($buildTui) { $exes += Join-Path $bin 'axon-tui.exe' }

    # Sign before zipping: a zip cannot carry a signature, so the .exe inside
    # it is what SmartScreen and Defender judge.
    if ($signing) {
        Step 'Signing the executables'
        & $signer sign @exes
        if ($LASTEXITCODE -ne 0) { throw 'signing failed' }
    }

    Step 'Packaging the zips'
    if ($buildTui) {
        Compress-Archive -Force -DestinationPath (Join-Path $out 'axon-tui-windows.zip') `
            -Path (@(Join-Path $bin 'axon-tui.exe') + $thirdparty)
    }
    if ($buildServer) {
        Compress-Archive -Force -DestinationPath (Join-Path $out 'axon-server-windows.zip') `
            -Path (@(Join-Path $bin 'axon-server.exe') + $thirdparty)
    }
    if ($buildServer -and $buildTui) {
        Compress-Archive -Force -DestinationPath (Join-Path $out 'axon-windows.zip') `
            -Path (@((Join-Path $bin 'axon-server.exe'), (Join-Path $bin 'axon-tui.exe')) + $thirdparty)
    }
}

# --- Desktop ---

if ($buildDesktop) {
    Step 'Building the desktop installer'
    $tauriArgs = @('tauri', 'build', '--bundles', 'nsis')
    if ($signing) {
        # Tauri signs the shell executable, the uninstaller and the installer
        # by calling the signer once per file, so the binary inside the
        # installer is signed before it is packed.
        $overlay = & $signer overlay
        if ($LASTEXITCODE -ne 0) { throw 'could not write the signing overlay' }
        $tauriArgs += @('--config', ($overlay | Select-Object -Last 1))
    }
    # An empty updater key selects Tauri's signing path and fails partway
    # through (the tauri-build action's comment); unset means unsigned updater.
    foreach ($v in 'TAURI_SIGNING_PRIVATE_KEY', 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD') {
        if (-not [Environment]::GetEnvironmentVariable($v)) { [Environment]::SetEnvironmentVariable($v, $null) }
    }
    Push-Location clients/web
    try { Invoke-Checked pnpm @tauriArgs } finally { Pop-Location }

    $installer = Get-ChildItem 'clients/web/src-tauri/target/release/bundle/nsis/*.exe' |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $installer) { throw 'tauri build produced no NSIS installer' }
    Copy-Item $installer.FullName $out
}

# --- Verify and report ---

$artifacts = Get-ChildItem -Path $out -File
if ($signing) {
    Step 'Verifying signatures'
    $verify = @()
    # The zips hold copies of the executables verified above; check the
    # shipped executables and the installer directly.
    if ($buildServer) { $verify += Join-Path $bin 'axon-server.exe' }
    if ($buildTui) { $verify += Join-Path $bin 'axon-tui.exe' }
    $verify += @($artifacts | Where-Object { $_.Extension -eq '.exe' } | ForEach-Object FullName)
    & $signer verify @verify
    if ($LASTEXITCODE -ne 0) { throw 'signature verification failed' }
}

Step "Done: $out"
foreach ($f in $artifacts) {
    $hash = (Get-FileHash -Algorithm SHA256 -Path $f.FullName).Hash.ToLower()
    '{0,-34} {1,10:N1} MB  sha256:{2}' -f $f.Name, ($f.Length / 1MB), $hash
}
if (-not $signing) { Write-Host "`nThese files are UNSIGNED." -ForegroundColor Yellow }
