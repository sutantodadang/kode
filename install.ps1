#Requires -Version 5.1
<#
.SYNOPSIS
    Kode installer for Windows.
.DESCRIPTION
    Downloads, verifies, and installs the kode binary for x64 Windows.
    Usage: iwr https://raw.githubusercontent.com/sutantodadang/kode/main/install.ps1 -useb | iex
#>

$ErrorActionPreference = 'Stop'

$Repo = 'sutantodadang/kode'
$BinName = 'kode.exe'

function Write-KodeLog {
    param([string]$Message)
    Write-Host "kode: $Message"
}

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
    throw "kode: error: unsupported architecture '$($env:PROCESSOR_ARCHITECTURE)'. Only x64 Windows is supported."
}

$Target = 'x86_64-pc-windows-msvc'
$Version = $env:KODE_VERSION

if ($Version) {
    $BaseUrl = "https://github.com/$Repo/releases/download/$Version/kode-$Target.zip"
} else {
    $BaseUrl = "https://github.com/$Repo/releases/latest/download/kode-$Target.zip"
}
$ShaUrl = "$BaseUrl.sha256"

$TempDir = Join-Path $env:TEMP ("kode-install-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $TempDir -Force | Out-Null

try {
    $ArchivePath = Join-Path $TempDir "kode-$Target.zip"
    $ShaPath = Join-Path $TempDir "kode-$Target.zip.sha256"

    Write-KodeLog "downloading kode for $Target..."
    Invoke-WebRequest -Uri $BaseUrl -OutFile $ArchivePath -UseBasicParsing

    Write-KodeLog "downloading checksum..."
    $shaOk = $true
    try {
        Invoke-WebRequest -Uri $ShaUrl -OutFile $ShaPath -UseBasicParsing
    } catch {
        $shaOk = $false
        Write-KodeLog "warning: could not download checksum file, skipping verification."
    }

    if ($shaOk) {
        $shaContent = Get-Content -Raw -Path $ShaPath
        $expectedHash = ($shaContent -split '\s+')[0].Trim().ToLowerInvariant()

        $actualHash = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash.ToLowerInvariant()

        if ($expectedHash -ne $actualHash) {
            throw "kode: error: checksum verification failed. Expected $expectedHash, got $actualHash."
        }
        Write-KodeLog "checksum verified."
    }

    $InstallDir = $env:KODE_INSTALL_DIR
    if (-not $InstallDir) {
        $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\kode'
    }
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null

    Write-KodeLog "extracting archive..."
    Expand-Archive -Path $ArchivePath -DestinationPath $InstallDir -Force

    $BinPath = Join-Path $InstallDir $BinName
    if (-not (Test-Path $BinPath)) {
        throw "kode: error: extracted archive does not contain expected binary '$BinName'."
    }

    Write-KodeLog "installed $BinName to $BinPath"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $pathEntries = @()
    if ($userPath) { $pathEntries += $userPath -split ';' }

    $alreadyInUserPath = $pathEntries -contains $InstallDir
    $alreadyInSessionPath = ($env:Path -split ';') -contains $InstallDir

    if (-not $alreadyInUserPath) {
        $newUserPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $newUserPath, 'User')
        Write-KodeLog "added $InstallDir to your user PATH."
    }

    if (-not $alreadyInSessionPath) {
        $env:Path = "$env:Path;$InstallDir"
    }

    & $BinPath --version

    Write-KodeLog "kode installed successfully."
    if (-not $alreadyInUserPath) {
        Write-KodeLog "note: restart your terminal for the updated PATH to take effect."
    }
} finally {
    Remove-Item -Recurse -Force -Path $TempDir -ErrorAction SilentlyContinue
}
