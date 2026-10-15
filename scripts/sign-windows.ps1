#!/usr/bin/env pwsh
# Authenticode signing through Azure Artifact Signing (ADR 0119).
#
#   sign-windows.ps1 setup            install signtool and the Artifact Signing
#                                     dlib; in CI also export SIGN_* for later
#                                     steps
#   sign-windows.ps1 sign   FILE...   sign each file in place, then verify it
#   sign-windows.ps1 verify FILE...   fail unless each file carries a valid
#                                     signature
#   sign-windows.ps1 overlay [PATH]   write the Tauri config overlay that makes
#                                     `tauri build` call this script for every
#                                     file it produces, and print its path
#
# **Signing is by presence.** A run is configured when SIGN_SIGNTOOL, SIGN_DLIB
# and SIGN_METADATA are set (CI: `.github/actions/windows-signing` exports
# them), or when ARTIFACT_SIGNING_ENDPOINT, _ACCOUNT and _PROFILE are set, in
# which case the tools are installed into a per-user cache on first use (a
# local build: scripts/package-windows.ps1). Otherwise `sign` and `verify` say
# so and exit 0, so a build with no signing configured produces the same
# unsigned artifacts it always did.
#
# Local runs authenticate as whoever DefaultAzureCredential finds: `az login`,
# Visual Studio, or AZURE_CLIENT_ID/_TENANT_ID/_CLIENT_SECRET.
#
# Authentication is not handled here: the dlib uses DefaultAzureCredential, which
# finds the `azure/login` session the composite action opened (OIDC, so no
# secret is stored anywhere).
#
# Used two ways: directly on the server/TUI binaries (cross-build.yml), and as
# Tauri's `bundle.windows.signCommand` (desktop-build.yml), where Tauri calls
# `sign` once per file it produces.

[CmdletBinding()]
param(
    [Parameter(Mandatory, Position = 0)]
    [ValidateSet('setup', 'sign', 'verify', 'overlay')]
    [string]$Command,

    [Parameter(ValueFromRemainingArguments)]
    [string[]]$Files = @()
)

$ErrorActionPreference = 'Stop'

# Microsoft's recommended timestamp authority. Artifact Signing certificates
# are valid for three days, so the timestamp is what keeps a signature valid
# after that.
$TimestampUrl = 'http://timestamp.acs.microsoft.com'

function Get-SigningRoot {
    # CI keeps the tools in the runner's temp; a local run keeps them between
    # builds so the NuGet download happens once.
    if ($env:RUNNER_TEMP) { return Join-Path $env:RUNNER_TEMP 'artifact-signing' }
    $base = $env:LOCALAPPDATA ?? (Join-Path $HOME '.cache')
    return Join-Path $base 'axon/artifact-signing'
}

function Test-ServiceVariables {
    foreach ($name in 'ARTIFACT_SIGNING_ENDPOINT', 'ARTIFACT_SIGNING_ACCOUNT', 'ARTIFACT_SIGNING_PROFILE') {
        if (-not [Environment]::GetEnvironmentVariable($name)) { return $false }
    }
    return $true
}

# Finds (installing on first use) signtool and the dlib, writes metadata.json,
# and sets SIGN_* in this process. Returns the three values.
function Install-SigningTools {
    if (-not (Test-ServiceVariables)) {
        throw 'ARTIFACT_SIGNING_ENDPOINT, ARTIFACT_SIGNING_ACCOUNT and ARTIFACT_SIGNING_PROFILE must be set'
    }

    $root = Get-SigningRoot
    New-Item -ItemType Directory -Force -Path $root | Out-Null

    function Find-Tool([string]$package, [string]$filter) {
        $dir = Join-Path $root $package
        if (-not (Test-Path $dir)) { return $null }
        Get-ChildItem -Recurse -Path $dir -Filter $filter |
            Where-Object { $_.FullName -match '[\\/]x64[\\/]' } |
            Sort-Object FullName -Descending | Select-Object -First 1
    }

    $signtool = Find-Tool 'Microsoft.Windows.SDK.BuildTools' 'signtool.exe'
    $dlib = Find-Tool 'Microsoft.ArtifactSigning.Client' 'Azure.CodeSigning.Dlib.dll'

    if (-not $signtool -or -not $dlib) {
        $nuget = Get-Command nuget -ErrorAction SilentlyContinue
        if ($nuget) {
            $nugetExe = $nuget.Source
        } else {
            $nugetExe = Join-Path $root 'nuget.exe'
            Invoke-WebRequest -Uri 'https://dist.nuget.org/win-x86-commandline/latest/nuget.exe' -OutFile $nugetExe
        }

        # signtool must come from a recent SDK build tools package: the dlib
        # does not work with the 20348 SDK's signtool (Microsoft's docs).
        foreach ($package in 'Microsoft.Windows.SDK.BuildTools', 'Microsoft.ArtifactSigning.Client') {
            & $nugetExe install $package -x -OutputDirectory $root -NonInteractive -Verbosity quiet | Out-Host
            if ($LASTEXITCODE -ne 0) { throw "nuget install $package failed ($LASTEXITCODE)" }
        }
        $signtool = Find-Tool 'Microsoft.Windows.SDK.BuildTools' 'signtool.exe'
        $dlib = Find-Tool 'Microsoft.ArtifactSigning.Client' 'Azure.CodeSigning.Dlib.dll'
    }
    if (-not $signtool) { throw 'x64 signtool.exe not found in the SDK build tools package' }
    if (-not $dlib) { throw 'x64 Azure.CodeSigning.Dlib.dll not found in the client package' }

    $metadata = Join-Path $root 'metadata.json'
    [ordered]@{
        Endpoint               = $env:ARTIFACT_SIGNING_ENDPOINT
        CodeSigningAccountName = $env:ARTIFACT_SIGNING_ACCOUNT
        CertificateProfileName = $env:ARTIFACT_SIGNING_PROFILE
    } | ConvertTo-Json | Set-Content -Path $metadata -Encoding utf8

    # Forward slashes: these values are read back by bash steps and by Tauri's
    # signCommand, and none of them want to parse backslash escapes.
    $exports = [ordered]@{
        SIGN_SIGNTOOL = $signtool.FullName -replace '\\', '/'
        SIGN_DLIB     = $dlib.FullName -replace '\\', '/'
        SIGN_METADATA = $metadata -replace '\\', '/'
    }
    foreach ($key in $exports.Keys) {
        [Environment]::SetEnvironmentVariable($key, $exports[$key])
        if ($env:GITHUB_ENV) { "$key=$($exports[$key])" | Add-Content -Path $env:GITHUB_ENV -Encoding utf8 }
    }
}

# The Tauri overlay: `bundle.windows.signCommand` pointing at this script.
# Written to a file because the command needs this checkout's absolute path,
# and a committed command would break every unsigned local build.
function Write-Overlay([string]$Path) {
    if (-not $Path) {
        $dir = $env:RUNNER_TEMP ?? [IO.Path]::GetTempPath()
        $Path = Join-Path $dir 'tauri-signing.json'
    }
    $self = $PSCommandPath -replace '\\', '/'
    @{
        bundle = @{
            windows = @{
                signCommand = @{
                    cmd  = 'pwsh'
                    args = @('-NoProfile', '-File', $self, 'sign', '%1')
                }
            }
        }
    } | ConvertTo-Json -Depth 6 | Set-Content -Path $Path -Encoding utf8
    return ($Path -replace '\\', '/')
}

function Test-Configured {
    if (-not ($env:SIGN_SIGNTOOL -and $env:SIGN_DLIB -and $env:SIGN_METADATA) -and (Test-ServiceVariables)) {
        Install-SigningTools
    }
    if ($env:SIGN_SIGNTOOL -and $env:SIGN_DLIB -and $env:SIGN_METADATA) { return $true }
    Write-Host "::notice::Windows signing is not configured; leaving $($Files.Count) file(s) unsigned."
    return $false
}

function Assert-Signed([string]$File) {
    $sig = Get-AuthenticodeSignature -FilePath $File
    if ($sig.Status -ne 'Valid') {
        throw "$File is not validly signed (status: $($sig.Status)): $($sig.StatusMessage)"
    }
    if (-not $sig.TimeStamperCertificate) {
        throw "$File is signed without a timestamp; the three-day certificate would make it invalid"
    }
    Write-Host "signed: $File ($($sig.SignerCertificate.Subject))"
}

function Invoke-Sign([string]$File) {
    if (-not (Test-Path -LiteralPath $File -PathType Leaf)) { throw "no such file: $File" }

    # The service is remote and occasionally returns a transient 5xx or a
    # timestamp-server timeout; three tries before failing a release build.
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        & $env:SIGN_SIGNTOOL sign /v /fd SHA256 /tr $TimestampUrl /td SHA256 `
            /dlib $env:SIGN_DLIB /dmdf $env:SIGN_METADATA $File
        if ($LASTEXITCODE -eq 0) { return }
        Write-Warning "signtool failed for $File (exit $LASTEXITCODE), attempt $attempt of 3"
        Start-Sleep -Seconds (5 * $attempt)
    }
    throw "signing $File failed after 3 attempts"
}

switch ($Command) {
    'setup' {
        Install-SigningTools
        foreach ($key in 'SIGN_SIGNTOOL', 'SIGN_DLIB', 'SIGN_METADATA') {
            Write-Host "$key=$([Environment]::GetEnvironmentVariable($key))"
        }
    }
    'overlay' { Write-Output (Write-Overlay ($Files | Select-Object -First 1)) }
    'sign' {
        if (-not (Test-Configured)) { exit 0 }
        if ($Files.Count -eq 0) { throw 'sign needs at least one file' }
        foreach ($file in $Files) {
            Invoke-Sign $file
            Assert-Signed $file
        }
    }
    'verify' {
        if (-not (Test-Configured)) { exit 0 }
        if ($Files.Count -eq 0) { throw 'verify needs at least one file' }
        foreach ($file in $Files) { Assert-Signed $file }
    }
}

# A defined result for callers that read $LASTEXITCODE: without it, a run that
# launched no native command (tools already cached, nothing to sign) leaves the
# caller's value unset or stale. Failures throw before reaching this line.
exit 0
