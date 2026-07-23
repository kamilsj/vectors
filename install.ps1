<#
.SYNOPSIS
Installs vectors for Windows x86-64 and starts the local web console. The server
uses VECTORS_DATA_DIR for durable storage by default; set VECTORS_SNAPSHOT to
retain legacy interval-based snapshot mode.

.PARAMETER Version
Release tag such as v0.5.0. The latest release is used by default.

.PARAMETER InstallDir
Destination for vectors.exe and vectors-server.exe.

.PARAMETER BindAddress
Server address. Defaults to 127.0.0.1:8080.

.PARAMETER NoStart
Install without starting vectors-server.

.PARAMETER NoOpen
Do not open the web console after the server becomes ready.
#>
[CmdletBinding()]
param(
    [string]$Version = $env:VECTORS_VERSION,
    [string]$InstallDir = $env:VECTORS_INSTALL_DIR,
    [string]$BindAddress = $env:VECTORS_BIND,
    [switch]$NoStart,
    [switch]$NoOpen
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$Repository = "kamilsj/vectors"
$Asset = "vectors-x86_64-pc-windows-msvc.zip"
if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\vectors"
}
if ([string]::IsNullOrWhiteSpace($BindAddress)) {
    $BindAddress = "127.0.0.1:8080"
}
$SkipStart = $NoStart.IsPresent -or $env:VECTORS_NO_START -eq "1"
$SkipOpen = $NoOpen.IsPresent -or $env:VECTORS_NO_OPEN -eq "1"

function Receive-ReleaseFile {
    param([string]$Uri, [string]$OutFile)
    for ($Attempt = 1; $Attempt -le 3; $Attempt++) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $OutFile
            return
        } catch {
            if ($Attempt -eq 3) {
                throw
            }
            Start-Sleep -Seconds 2
        }
    }
}

if ($env:OS -ne "Windows_NT") {
    throw "This installer supports Windows; use install.sh on Linux."
}
if ($env:PROCESSOR_ARCHITECTURE -notin @("AMD64", "x86_64")) {
    throw "No release binary is available for architecture $env:PROCESSOR_ARCHITECTURE."
}

if (-not [string]::IsNullOrWhiteSpace($Version)) {
    if (-not $Version.StartsWith("v")) {
        $Version = "v$Version"
    }
    $ReleaseUrl = "https://github.com/$Repository/releases/download/$Version"
} else {
    $ReleaseUrl = "https://github.com/$Repository/releases/latest/download"
}

$Temporary = Join-Path ([IO.Path]::GetTempPath()) ("vectors-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $Temporary | Out-Null
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    $Archive = Join-Path $Temporary $Asset
    $Checksums = Join-Path $Temporary "SHA256SUMS"
    Write-Host "Downloading vectors from $ReleaseUrl..."
    Receive-ReleaseFile -Uri "$ReleaseUrl/$Asset" -OutFile $Archive
    Receive-ReleaseFile -Uri "$ReleaseUrl/SHA256SUMS" -OutFile $Checksums

    $ChecksumLine = Get-Content $Checksums | Where-Object {
        $_ -match ("\s\*?" + [regex]::Escape($Asset) + "$")
    } | Select-Object -First 1
    if ([string]::IsNullOrWhiteSpace($ChecksumLine)) {
        throw "Release checksum is missing for $Asset."
    }
    $Expected = ($ChecksumLine -split "\s+")[0].ToLowerInvariant()
    $Actual = (Get-FileHash -Algorithm SHA256 -Path $Archive).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) {
        throw "Archive checksum does not match."
    }

    $Expanded = Join-Path $Temporary "expanded"
    Expand-Archive -LiteralPath $Archive -DestinationPath $Expanded
    $VectorsSource = Join-Path $Expanded "vectors.exe"
    $ServerSource = Join-Path $Expanded "vectors-server.exe"
    if (-not (Test-Path -LiteralPath $VectorsSource) -or
        -not (Test-Path -LiteralPath $ServerSource)) {
        throw "Release archive is missing vectors.exe or vectors-server.exe."
    }
    & $VectorsSource --version | Out-Null
    & $ServerSource --version | Out-Null

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    foreach ($Name in @("vectors.exe", "vectors-server.exe")) {
        $Source = Join-Path $Expanded $Name
        $Pending = Join-Path $InstallDir "$Name.new"
        $Destination = Join-Path $InstallDir $Name
        Copy-Item -LiteralPath $Source -Destination $Pending -Force
        Move-Item -LiteralPath $Pending -Destination $Destination -Force
    }
} finally {
    Remove-Item -LiteralPath $Temporary -Recurse -Force -ErrorAction SilentlyContinue
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$PathEntries = @($UserPath -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
if (-not ($PathEntries | Where-Object { $_.TrimEnd("\") -ieq $InstallDir.TrimEnd("\") })) {
    $UpdatedPath = (@($PathEntries) + $InstallDir) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $UpdatedPath, "User")
    Write-Host "Added $InstallDir to the user PATH."
}
$env:Path = "$InstallDir;$env:Path"

$InstalledVersion = & (Join-Path $InstallDir "vectors.exe") --version
Write-Host "Installed $InstalledVersion in $InstallDir."
if ($SkipStart) {
    Write-Host "Run 'vectors' for the SQL shell or 'vectors-server' for the web console."
    exit 0
}

$StateDir = if ($env:VECTORS_STATE_DIR) {
    $env:VECTORS_STATE_DIR
} else {
    Join-Path $env:LOCALAPPDATA "vectors"
}
$DataDir = if ($env:VECTORS_DATA_DIR) {
    $env:VECTORS_DATA_DIR
} else {
    Join-Path $StateDir "data"
}
$Snapshot = $env:VECTORS_SNAPSHOT
$Autosave = if ($Snapshot -and $env:VECTORS_AUTOSAVE_INTERVAL_SECS) {
    $env:VECTORS_AUTOSAVE_INTERVAL_SECS
} elseif ($Snapshot) { "30" } else { $null }
$PidFile = Join-Path $StateDir "server.pid"
$StdoutLog = Join-Path $StateDir "server.stdout.log"
$StderrLog = Join-Path $StateDir "server.stderr.log"
New-Item -ItemType Directory -Force -Path $StateDir, $DataDir | Out-Null

if (Test-Path -LiteralPath $PidFile) {
    $OldPid = Get-Content -LiteralPath $PidFile -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($OldPid -and (Get-Process -Id $OldPid -ErrorAction SilentlyContinue)) {
        Write-Host "vectors-server is already running with PID $OldPid."
        exit 0
    }
    Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
}

$RuntimeServer = Join-Path $StateDir "vectors-server.exe"
Copy-Item -LiteralPath (Join-Path $InstallDir "vectors-server.exe") -Destination $RuntimeServer -Force
$PreviousBind = $env:VECTORS_BIND
$PreviousDataDir = $env:VECTORS_DATA_DIR
$PreviousSnapshot = $env:VECTORS_SNAPSHOT
$PreviousAutosave = $env:VECTORS_AUTOSAVE_INTERVAL_SECS
try {
    $env:VECTORS_BIND = $BindAddress
    if ($Snapshot) {
        Remove-Item Env:VECTORS_DATA_DIR -ErrorAction SilentlyContinue
        $env:VECTORS_SNAPSHOT = $Snapshot
        $env:VECTORS_AUTOSAVE_INTERVAL_SECS = $Autosave
    } else {
        $env:VECTORS_DATA_DIR = $DataDir
        Remove-Item Env:VECTORS_SNAPSHOT -ErrorAction SilentlyContinue
        Remove-Item Env:VECTORS_AUTOSAVE_INTERVAL_SECS -ErrorAction SilentlyContinue
    }
    $Server = Start-Process -FilePath $RuntimeServer -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput $StdoutLog -RedirectStandardError $StderrLog
} finally {
    $env:VECTORS_BIND = $PreviousBind
    $env:VECTORS_DATA_DIR = $PreviousDataDir
    $env:VECTORS_SNAPSHOT = $PreviousSnapshot
    $env:VECTORS_AUTOSAVE_INTERVAL_SECS = $PreviousAutosave
}
Set-Content -LiteralPath $PidFile -Value $Server.Id -Encoding Ascii

$Port = $BindAddress.Substring($BindAddress.LastIndexOf(":") + 1)
$ConsoleUrl = "http://127.0.0.1:$Port"
$Ready = $false
for ($Attempt = 0; $Attempt -lt 40; $Attempt++) {
    try {
        $Health = Invoke-RestMethod -Uri "$ConsoleUrl/healthz" -TimeoutSec 1
        if ($Health.status -eq "ok") {
            $Ready = $true
            break
        }
    } catch {
        # The server may still be binding the socket.
    }
    if (-not (Get-Process -Id $Server.Id -ErrorAction SilentlyContinue)) {
        break
    }
    Start-Sleep -Milliseconds 250
}

if (-not $Ready) {
    Stop-Process -Id $Server.Id -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
    throw "vectors-server did not become ready. See $StderrLog."
}

Write-Host "vectors-server started with PID $($Server.Id)."
Write-Host "Web console: $ConsoleUrl"
if ($Snapshot) {
    Write-Host "Snapshot: $Snapshot"
} else {
    Write-Host "Durable data: $DataDir"
}
Write-Host "Logs: $StdoutLog and $StderrLog"
Write-Host "Stop: Stop-Process -Id (Get-Content '$PidFile')"
if (-not $SkipOpen) {
    Start-Process $ConsoleUrl | Out-Null
}
