#!/usr/bin/env pwsh
# Kode installer for Windows.
#
# Usage:
#   irm https://raw.githubusercontent.com/sutantodadang/kode/main/scripts/install.ps1 | iex
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/sutantodadang/kode/main/scripts/install.ps1))) -Version v0.1.0
#
# Env overrides:
#   KODE_INSTALL_DIR   install directory (default: $env:LOCALAPPDATA\kode\bin)
param(
    [string]$Version = ""
)

$ErrorActionPreference = "Stop"

$Repo = "sutantodadang/kode"
$InstallDir = if ($env:KODE_INSTALL_DIR) { $env:KODE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "kode\bin" }

function Write-Info($msg) {
    Write-Host "kode-install: $msg"
}

function Fail($msg) {
    Write-Error "kode-install: error: $msg"
    exit 1
}

# ---- detect arch ----
# PROCESSOR_ARCHITEW6432 is set when running a 32-bit PowerShell on a 64-bit
# OS; env vars work on both Windows PowerShell 5.1 and PowerShell 7+.
$archRaw = $env:PROCESSOR_ARCHITEW6432
if (-not $archRaw) { $archRaw = $env:PROCESSOR_ARCHITECTURE }
if ($archRaw -ne "AMD64") {
    Fail "unsupported architecture: $archRaw (Kode ships an x86_64 Windows binary only)"
}
$target = "x86_64-pc-windows-msvc"
$asset = "kode-$target.zip"

# ---- resolve version/tag ----
if ($Version) {
    $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
    Write-Info "using requested version $tag"
} else {
    Write-Info "resolving latest release..."
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "kode-install" }
        $tag = $release.tag_name
    } catch {
        Write-Info "GitHub API lookup failed, falling back to redirect resolution..."
        try {
            # -SkipHttpErrorCheck is PowerShell 7-only; on 5.1 the 302 comes
            # through as an exception, so read Location from either shape.
            $location = $null
            try {
                $resp = Invoke-WebRequest -Uri "https://github.com/$Repo/releases/latest" -MaximumRedirection 0 -UseBasicParsing -ErrorAction Stop
                $location = $resp.Headers.Location
            } catch {
                $webResp = $_.Exception.Response
                if ($webResp) { $location = $webResp.Headers["Location"] }
            }
            if ("$location" -match "/releases/tag/(v[^/]+)$") {
                $tag = $Matches[1]
            }
        } catch {
            $tag = $null
        }
    }

    if (-not $tag) {
        Fail "could not resolve the latest release tag - pass a version explicitly, e.g. '-Version v0.1.0'"
    }
}

Write-Info "installing kode $tag for $target..."

# ---- download ----
$tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "kode-install-$([System.Guid]::NewGuid())"
New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

try {
    $assetUrl = "https://github.com/$Repo/releases/download/$tag/$asset"
    $shaUrl = "$assetUrl.sha256"
    $archivePath = Join-Path $tmpDir $asset
    $shaPath = "$archivePath.sha256"

    try {
        Invoke-WebRequest -Uri $assetUrl -OutFile $archivePath -UseBasicParsing
    } catch {
        Fail "failed to download $assetUrl`nCheck that a release exists for $tag and target $target at:`n  https://github.com/$Repo/releases"
    }

    # ---- verify checksum (best effort - skip if sidecar missing) ----
    try {
        Invoke-WebRequest -Uri $shaUrl -OutFile $shaPath -UseBasicParsing
        $expected = (Get-Content $shaPath -Raw).Trim().Split(" ")[0].ToLower()
        $actual = (Get-FileHash -Algorithm SHA256 -Path $archivePath).Hash.ToLower()
        if ($expected -ne $actual) {
            Fail "checksum mismatch for $asset - aborting install"
        }
        Write-Info "checksum verified"
    } catch {
        Write-Info "no checksum sidecar found for $asset - skipping checksum verification"
    }

    # ---- extract ----
    try {
        Expand-Archive -Path $archivePath -DestinationPath $tmpDir -Force
    } catch {
        Fail "failed to extract $archivePath"
    }

    $exePath = Join-Path $tmpDir "kode.exe"
    if (-not (Test-Path $exePath)) {
        Fail "extracted archive did not contain kode.exe"
    }

    # ---- install ----
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -Path $exePath -Destination (Join-Path $InstallDir "kode.exe") -Force

    Write-Info "installed kode $tag to $InstallDir\kode.exe"
} finally {
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
}

# ---- PATH check (user-level only, no admin required) ----
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathEntries = @()
if ($userPath) { $pathEntries = $userPath -split ";" }

if ($pathEntries -contains $InstallDir) {
    Write-Info "$InstallDir is already on your PATH"
} else {
    $newUserPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")
    Write-Info "added $InstallDir to your user PATH. Open a new terminal for it to take effect."
}

Write-Info "run 'kode --version' to verify, then 'kode doctor' for a full diagnostic."
