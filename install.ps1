<#
.SYNOPSIS
Installs or upgrades vectors on 64-bit Windows and optionally starts its local
web console with durable storage.

.DESCRIPTION
Downloads the official x86-64 release archive, verifies its SHA-256 checksum,
validates both executables, and updates them transactionally in a per-user
directory. Managed restarts reuse their recorded bind and storage settings and
request graceful shutdown before replacement. Existing database files are never
removed.

.PARAMETER Version
Release tag such as v0.6.0. The latest release is used by default.

.PARAMETER InstallDir
Destination for vectors.exe and vectors-server.exe. Defaults to
%LOCALAPPDATA%\Programs\vectors.

.PARAMETER BindAddress
Server listen address. Defaults to 127.0.0.1:8080. IPv6 addresses must use
brackets, for example [::1]:8080.

.PARAMETER NoStart
Install or upgrade the binaries without starting vectors-server.

.PARAMETER NoOpen
Do not open the web console after the server becomes ready.

.PARAMETER PrintTarget
Print the resolved release, asset, paths, and server settings without using the
network or changing the computer. Intended for diagnostics and CI.

.EXAMPLE
.\install.ps1

.EXAMPLE
.\install.ps1 -Version v0.6.0 -NoStart

.EXAMPLE
.\install.ps1 -BindAddress 127.0.0.1:8081 -NoOpen

.EXAMPLE
.\install.ps1 -PrintTarget

.EXAMPLE
.\install.ps1 -WhatIf
#>
[CmdletBinding(SupportsShouldProcess = $true, ConfirmImpact = "Medium")]
param(
    [string]$Version = $env:VECTORS_VERSION,
    [string]$InstallDir = $env:VECTORS_INSTALL_DIR,
    [string]$BindAddress = $env:VECTORS_BIND,
    [switch]$NoStart,
    [switch]$NoOpen,
    [switch]$PrintTarget
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$Repository = "kamilsj/vectors"
$Asset = "vectors-x86_64-pc-windows-msvc.zip"
$SemverPattern = '(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?'

function Test-EnvironmentSwitch {
    param([AllowNull()][string]$Value)

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $false
    }
    return $Value.Trim() -match '^(?i:1|true|yes|on)$'
}

function Set-ProcessEnvironmentValue {
    param(
        [Parameter(Mandatory)][string]$Name,
        [AllowNull()][string]$Value
    )

    if ([string]::IsNullOrEmpty($Value)) {
        [Environment]::SetEnvironmentVariable($Name, $null, "Process")
        Remove-Item -Path "Env:\$Name" -Force -ErrorAction SilentlyContinue
    } else {
        [Environment]::SetEnvironmentVariable($Name, $Value, "Process")
    }
}

function Resolve-FullPath {
    param(
        [Parameter(Mandatory)][string]$Path,
        [string]$Description = "Path"
    )

    $Expanded = [Environment]::ExpandEnvironmentVariables($Path.Trim().Trim('"'))
    if ([string]::IsNullOrWhiteSpace($Expanded)) {
        throw "$Description cannot be empty."
    }
    if ($Expanded.IndexOf([IO.Path]::PathSeparator) -ge 0) {
        throw "$Description cannot contain '$([IO.Path]::PathSeparator)'."
    }
    if (-not [IO.Path]::IsPathRooted($Expanded)) {
        $Expanded = Join-Path (Get-Location).ProviderPath $Expanded
    }
    try {
        return [IO.Path]::GetFullPath($Expanded)
    } catch {
        throw "$Description is not a valid Windows path: $Path"
    }
}

function Assert-NotFileSystemRoot {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Description
    )

    $Root = [IO.Path]::GetPathRoot($Path)
    if ($Root -and $Path.TrimEnd('\', '/') -ieq $Root.TrimEnd('\', '/')) {
        throw "$Description cannot be a drive or filesystem root: $Path"
    }
}

function Get-ComparablePath {
    param([Parameter(Mandatory)][string]$Path)

    $FullPath = Resolve-FullPath -Path $Path
    $Root = [IO.Path]::GetPathRoot($FullPath)
    if ($Root -and $FullPath.TrimEnd('\', '/') -ieq $Root.TrimEnd('\', '/')) {
        return $Root.ToUpperInvariant()
    }
    return $FullPath.TrimEnd('\', '/').ToUpperInvariant()
}

function Get-WindowsArchitecture {
    $Architecture = if (-not [string]::IsNullOrWhiteSpace($env:PROCESSOR_ARCHITEW6432)) {
        $env:PROCESSOR_ARCHITEW6432
    } elseif (-not [string]::IsNullOrWhiteSpace($env:PROCESSOR_ARCHITECTURE)) {
        $env:PROCESSOR_ARCHITECTURE
    } else {
        try {
            [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        } catch {
            "unknown"
        }
    }

    switch ($Architecture.Trim().ToUpperInvariant()) {
        { $_ -in @("AMD64", "X86_64", "X64") } { return "x86_64" }
        "ARM64" { return "arm64" }
        { $_ -in @("X86", "I386", "I686") } { return "x86" }
        default { return $Architecture.Trim().ToLowerInvariant() }
    }
}

function Assert-SupportedHost {
    if ($PSVersionTable.PSVersion -lt [version]"5.1") {
        throw "PowerShell 5.1 or newer is required; found $($PSVersionTable.PSVersion)."
    }

    $IsWindowsHost = $env:OS -eq "Windows_NT"
    try {
        $IsWindowsHost = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
            [System.Runtime.InteropServices.OSPlatform]::Windows
        )
    } catch {
        # Windows PowerShell 5.1 reliably exposes OS=Windows_NT.
    }
    if (-not $IsWindowsHost) {
        throw "This installer supports Windows. Use install.sh on Linux or macOS."
    }

    $Architecture = Get-WindowsArchitecture
    if ($Architecture -ne "x86_64") {
        throw "No Windows $Architecture release is published. This installer currently supports x86-64 Windows with asset '$Asset'."
    }
    return $Architecture
}

function Get-NormalizedRelease {
    param([AllowNull()][string]$RequestedVersion)

    if ([string]::IsNullOrWhiteSpace($RequestedVersion)) {
        return [pscustomobject]@{
            Tag = $null
            Version = $null
            Label = "latest"
            Url = "https://github.com/$Repository/releases/latest/download"
        }
    }

    $Tag = $RequestedVersion.Trim()
    if (-not $Tag.StartsWith("v", [StringComparison]::OrdinalIgnoreCase)) {
        $Tag = "v$Tag"
    }
    if ($Tag -notmatch "^v(?<version>$SemverPattern)$") {
        throw "Version must be a semantic release tag such as v0.6.0 or v0.7.0-rc.1."
    }
    $ReleaseVersion = $Matches.version
    $EncodedTag = [Uri]::EscapeDataString("v$ReleaseVersion")
    return [pscustomobject]@{
        Tag = "v$ReleaseVersion"
        Version = $ReleaseVersion
        Label = "v$ReleaseVersion"
        Url = "https://github.com/$Repository/releases/download/$EncodedTag"
    }
}

function Get-ConsoleEndpoint {
    param([Parameter(Mandatory)][string]$Address)

    $Candidate = $Address.Trim()
    $HostName = $null
    $PortText = $null
    if ($Candidate -match '^\[(?<host>[^\]]+)\]:(?<port>[0-9]+)$') {
        $HostName = $Matches.host
        $PortText = $Matches.port
    } elseif ($Candidate -match '^(?<host>[^:]+):(?<port>[0-9]+)$') {
        $HostName = $Matches.host
        $PortText = $Matches.port
    } elseif ($Candidate.Length -gt 0 -and $Candidate.Contains(':')) {
        throw "BindAddress is invalid. IPv6 addresses must use brackets, for example [::1]:8080."
    } else {
        throw "BindAddress must include a host and port, for example 127.0.0.1:8080."
    }

    $Port = 0
    if (-not [int]::TryParse($PortText, [ref]$Port) -or $Port -lt 1 -or $Port -gt 65535) {
        throw "BindAddress port must be an integer from 1 to 65535."
    }

    $ParsedAddress = $null
    $IsIpAddress = [Net.IPAddress]::TryParse($HostName, [ref]$ParsedAddress)
    if (-not $IsIpAddress -and [Uri]::CheckHostName($HostName) -ne [UriHostNameType]::Dns) {
        throw "BindAddress host '$HostName' is not a valid IP address or DNS name."
    }

    $ConnectHost = $HostName
    if ($IsIpAddress -and $ParsedAddress.Equals([Net.IPAddress]::Any)) {
        $ConnectHost = "127.0.0.1"
    } elseif ($IsIpAddress -and $ParsedAddress.Equals([Net.IPAddress]::IPv6Any)) {
        $ConnectHost = "::1"
    }
    $UrlHost = if ($ConnectHost.Contains(':')) { "[$ConnectHost]" } else { $ConnectHost }

    return [pscustomobject]@{
        BindAddress = $Candidate
        Host = $HostName
        Port = $Port
        IpAddress = $ParsedAddress
        ConsoleUrl = "http://${UrlHost}:$Port"
    }
}

function Format-BindAddress {
    param(
        [Parameter(Mandatory)][string]$HostName,
        [Parameter(Mandatory)][int]$Port
    )

    if ($HostName.Contains(':')) {
        return "[$HostName]:$Port"
    }
    return "${HostName}:$Port"
}

function Resolve-ProbeAddress {
    param([Parameter(Mandatory)]$Endpoint)

    if ($null -ne $Endpoint.IpAddress) {
        return $Endpoint.IpAddress
    }
    try {
        $Addresses = @([Net.Dns]::GetHostAddresses($Endpoint.Host))
    } catch {
        throw "BindAddress host '$($Endpoint.Host)' could not be resolved: $($_.Exception.Message)"
    }
    if ($Addresses.Count -eq 0) {
        throw "BindAddress host '$($Endpoint.Host)' did not resolve to an IP address."
    }
    $Preferred = $Addresses | Where-Object {
        $_.AddressFamily -eq [Net.Sockets.AddressFamily]::InterNetwork
    } | Select-Object -First 1
    if ($null -ne $Preferred) {
        return $Preferred
    }
    return $Addresses[0]
}

function Test-BindPort {
    param(
        [Parameter(Mandatory)]$Endpoint,
        [int]$Port = $Endpoint.Port
    )

    $Listener = $null
    try {
        $ProbeAddress = Resolve-ProbeAddress -Endpoint $Endpoint
        $Listener = [Net.Sockets.TcpListener]::new($ProbeAddress, $Port)
        $Listener.Server.ExclusiveAddressUse = $true
        $Listener.Start()
        return [pscustomobject]@{ Available = $true; Error = $null; SocketError = $null }
    } catch {
        $SocketError = $null
        if ($_.Exception -is [Net.Sockets.SocketException]) {
            $SocketError = $_.Exception.SocketErrorCode
        } elseif ($_.Exception.InnerException -is [Net.Sockets.SocketException]) {
            $SocketError = $_.Exception.InnerException.SocketErrorCode
        }
        return [pscustomobject]@{
            Available = $false
            Error = $_.Exception.Message
            SocketError = $SocketError
        }
    } finally {
        if ($null -ne $Listener) {
            $Listener.Stop()
        }
    }
}

function Get-NextAvailableBindAddress {
    param([Parameter(Mandatory)]$Endpoint)

    $LastPort = [Math]::Min(65535, $Endpoint.Port + 20)
    for ($Port = $Endpoint.Port + 1; $Port -le $LastPort; $Port++) {
        $Result = Test-BindPort -Endpoint $Endpoint -Port $Port
        if ($Result.Available) {
            return Format-BindAddress -HostName $Endpoint.Host -Port $Port
        }
    }
    return $null
}

function Receive-ReleaseFile {
    param(
        [Parameter(Mandatory)][string]$Uri,
        [Parameter(Mandatory)][string]$OutFile
    )

    $Partial = "$OutFile.download"
    for ($Attempt = 1; $Attempt -le 3; $Attempt++) {
        Remove-Item -LiteralPath $Partial -Force -ErrorAction SilentlyContinue
        try {
            $OldProgressPreference = $ProgressPreference
            try {
                $ProgressPreference = "SilentlyContinue"
                Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $Partial `
                    -TimeoutSec 120 -Headers @{ "User-Agent" = "vectors-installer" }
            } finally {
                $ProgressPreference = $OldProgressPreference
            }
            if (-not (Test-Path -LiteralPath $Partial -PathType Leaf) -or
                (Get-Item -LiteralPath $Partial).Length -eq 0) {
                throw "The server returned an empty file."
            }
            [IO.File]::Move($Partial, $OutFile)
            return
        } catch {
            Remove-Item -LiteralPath $Partial -Force -ErrorAction SilentlyContinue
            if ($Attempt -eq 3) {
                throw "Could not download $Uri after 3 attempts: $($_.Exception.Message)"
            }
            Write-Warning "Download attempt $Attempt failed; retrying. $($_.Exception.Message)"
            Start-Sleep -Seconds $Attempt
        }
    }
}

function Get-ExpectedChecksum {
    param(
        [Parameter(Mandatory)][string]$ChecksumFile,
        [Parameter(Mandatory)][string]$AssetName
    )

    $Pattern = '^\s*(?<hash>[0-9A-Fa-f]{64})\s+\*?' + [regex]::Escape($AssetName) + '\s*$'
    $Hashes = @(
        Get-Content -LiteralPath $ChecksumFile | ForEach-Object {
            if ($_ -match $Pattern) {
                $Matches.hash.ToLowerInvariant()
            }
        } | Select-Object -Unique
    )
    if ($Hashes.Count -eq 0) {
        throw "Release checksum file is missing a valid SHA-256 entry for $AssetName."
    }
    if ($Hashes.Count -ne 1) {
        throw "Release checksum file contains conflicting entries for $AssetName."
    }
    return $Hashes[0]
}

function Get-BinaryVersion {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$ProgramName
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Release archive is missing $ProgramName.exe."
    }
    $Output = @(& $Path --version 2>&1)
    $ExitCode = $LASTEXITCODE
    $Text = ($Output -join "`n").Trim()
    if ($ExitCode -ne 0) {
        throw "$ProgramName.exe --version exited with code $ExitCode. $Text"
    }
    if ($Text -notmatch "^$([regex]::Escape($ProgramName)) (?<version>$SemverPattern)$") {
        throw "$ProgramName.exe returned an unexpected version string: '$Text'."
    }
    return $Matches.version
}

function Restore-FileAtomically {
    param(
        [Parameter(Mandatory)][string]$Source,
        [Parameter(Mandatory)][string]$Destination
    )

    $Discard = "$Destination.discard.$([guid]::NewGuid().ToString('N'))"
    try {
        [IO.File]::Replace($Source, $Destination, $Discard, $true)
    } finally {
        Remove-Item -LiteralPath $Discard -Force -ErrorAction SilentlyContinue
    }
}

function Install-ReleaseBinaries {
    param(
        [Parameter(Mandatory)][string]$ExpandedDirectory,
        [Parameter(Mandatory)][string]$DestinationDirectory,
        [AllowNull()][string]$RequestedVersion
    )

    New-Item -ItemType Directory -Force -Path $DestinationDirectory | Out-Null
    $Transaction = [guid]::NewGuid().ToString("N")
    $Entries = @()
    try {
        foreach ($Name in @("vectors.exe", "vectors-server.exe")) {
            $Source = Join-Path $ExpandedDirectory $Name
            $Destination = Join-Path $DestinationDirectory $Name
            if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
                throw "Release archive is missing $Name."
            }
            if (Test-Path -LiteralPath $Destination -PathType Container) {
                throw "Cannot install $Name because $Destination is a directory."
            }
            $Pending = Join-Path $DestinationDirectory ".$Name.new.$Transaction"
            $Backup = Join-Path $DestinationDirectory ".$Name.previous.$Transaction"
            [IO.File]::Copy($Source, $Pending, $false)
            if ((Get-FileHash -Algorithm SHA256 -LiteralPath $Source).Hash -ne
                (Get-FileHash -Algorithm SHA256 -LiteralPath $Pending).Hash) {
                throw "Staging $Name changed its checksum."
            }
            $Entries += @{
                Name = $Name
                Pending = $Pending
                Destination = $Destination
                Backup = $Backup
                HadPrevious = Test-Path -LiteralPath $Destination -PathType Leaf
                Applied = $false
            }
        }
    } catch {
        foreach ($Name in @("vectors.exe", "vectors-server.exe")) {
            $Pending = Join-Path $DestinationDirectory ".$Name.new.$Transaction"
            Remove-Item -LiteralPath $Pending -Force -ErrorAction SilentlyContinue
        }
        throw
    }

    $PreserveBackups = $false
    try {
        foreach ($Entry in $Entries) {
            if ($Entry.HadPrevious) {
                [IO.File]::Replace($Entry.Pending, $Entry.Destination, $Entry.Backup, $true)
            } else {
                [IO.File]::Move($Entry.Pending, $Entry.Destination)
            }
            $Entry.Applied = $true
        }

        $VectorsVersion = Get-BinaryVersion `
            -Path (Join-Path $DestinationDirectory "vectors.exe") -ProgramName "vectors"
        $ServerVersion = Get-BinaryVersion `
            -Path (Join-Path $DestinationDirectory "vectors-server.exe") -ProgramName "vectors-server"
        if ($VectorsVersion -cne $ServerVersion) {
            throw "Installed executables disagree on version: vectors $VectorsVersion, vectors-server $ServerVersion."
        }
        if ($RequestedVersion -and $VectorsVersion -cne $RequestedVersion) {
            throw "Release tag v$RequestedVersion contains binaries for version $VectorsVersion."
        }

        foreach ($Entry in $Entries) {
            Remove-Item -LiteralPath $Entry.Backup -Force -ErrorAction SilentlyContinue
        }
        return $VectorsVersion
    } catch {
        $InstallError = $_
        [array]::Reverse($Entries)
        foreach ($Entry in $Entries) {
            if (-not $Entry.Applied) {
                continue
            }
            try {
                if ($Entry.HadPrevious -and (Test-Path -LiteralPath $Entry.Backup -PathType Leaf)) {
                    if (Test-Path -LiteralPath $Entry.Destination -PathType Leaf) {
                        Restore-FileAtomically -Source $Entry.Backup `
                            -Destination $Entry.Destination
                    } else {
                        [IO.File]::Move($Entry.Backup, $Entry.Destination)
                    }
                } elseif (-not $Entry.HadPrevious) {
                    Remove-Item -LiteralPath $Entry.Destination -Force -ErrorAction SilentlyContinue
                }
            } catch {
                $PreserveBackups = $true
                Write-Warning "Could not roll back $($Entry.Name): $($_.Exception.Message)"
            }
        }
        throw "Could not atomically update the vectors binaries. Close running vectors commands and retry. $($InstallError.Exception.Message)"
    } finally {
        foreach ($Entry in $Entries) {
            Remove-Item -LiteralPath $Entry.Pending -Force -ErrorAction SilentlyContinue
            if (-not $PreserveBackups) {
                Remove-Item -LiteralPath $Entry.Backup -Force -ErrorAction SilentlyContinue
            }
        }
    }
}

function Add-DirectoryToUserPath {
    param([Parameter(Mandatory)][string]$Directory)

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $Entries = @(
        $UserPath -split [regex]::Escape([string][IO.Path]::PathSeparator) |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    )
    $Target = Get-ComparablePath -Path $Directory
    $AlreadyPresent = $false
    foreach ($Entry in $Entries) {
        try {
            if ((Get-ComparablePath -Path $Entry) -ceq $Target) {
                $AlreadyPresent = $true
                break
            }
        } catch {
            # Preserve unusual PATH entries, but do not treat them as a match.
        }
    }

    $Persisted = $AlreadyPresent
    $Changed = $false
    if (-not $AlreadyPresent) {
        $UpdatedPath = (@($Entries) + $Directory) -join [IO.Path]::PathSeparator
        if ($UpdatedPath.Length -gt 32767) {
            Write-Warning "The user PATH is too long to update safely. Add '$Directory' manually."
        } else {
            try {
                [Environment]::SetEnvironmentVariable("Path", $UpdatedPath, "User")
                $Persisted = $true
                $Changed = $true
            } catch {
                Write-Warning "Could not update the user PATH. Add '$Directory' manually. $($_.Exception.Message)"
            }
        }
    }

    $ProcessEntries = @(
        $env:Path -split [regex]::Escape([string][IO.Path]::PathSeparator) |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    )
    $InProcessPath = $false
    foreach ($Entry in $ProcessEntries) {
        try {
            if ((Get-ComparablePath -Path $Entry) -ceq $Target) {
                $InProcessPath = $true
                break
            }
        } catch {
            # Ignore malformed inherited entries.
        }
    }
    if (-not $InProcessPath) {
        $env:Path = if ([string]::IsNullOrWhiteSpace($env:Path)) {
            $Directory
        } else {
            "$Directory$([IO.Path]::PathSeparator)$env:Path"
        }
    }

    return [pscustomobject]@{ Changed = $Changed; Persisted = $Persisted }
}

function Read-ManagedServerConfig {
    param([Parameter(Mandatory)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }
    try {
        $Config = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
        $RequiredProperties = @(
            "schema",
            "bind_address",
            "storage_mode",
            "data_directory",
            "snapshot",
            "autosave_interval_seconds",
            "shutdown_file",
            "process_start_utc_ticks",
            "cooperative_shutdown",
            "api_token_required"
        )
        foreach ($Property in $RequiredProperties) {
            if ($Config.PSObject.Properties.Name -notcontains $Property) {
                throw "missing '$Property'"
            }
        }
        if ($Config.schema -ne 1 -or
            [string]::IsNullOrWhiteSpace([string]$Config.bind_address) -or
            [string]::IsNullOrWhiteSpace([string]$Config.storage_mode)) {
            throw "unsupported or incomplete configuration"
        }
        if ($Config.storage_mode -notin @("durable", "snapshot")) {
            throw "unknown storage mode '$($Config.storage_mode)'"
        }
        if ($Config.storage_mode -eq "durable" -and
            [string]::IsNullOrWhiteSpace([string]$Config.data_directory)) {
            throw "durable storage path is empty"
        }
        if ($Config.storage_mode -eq "snapshot" -and
            [string]::IsNullOrWhiteSpace([string]$Config.snapshot)) {
            throw "snapshot path is empty"
        }
        return $Config
    } catch {
        throw "Managed server configuration is invalid: $Path. $($_.Exception.Message)"
    }
}

function Write-ManagedServerConfig {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][string]$Bind,
        [AllowNull()][string]$DataDirectory,
        [AllowNull()][string]$Snapshot,
        [AllowNull()][string]$Autosave,
        [Parameter(Mandatory)][string]$ShutdownFile,
        [Parameter(Mandatory)][bool]$ApiTokenRequired,
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process
    )

    $Config = [ordered]@{
        schema = 1
        bind_address = $Bind
        storage_mode = if ($Snapshot) { "snapshot" } else { "durable" }
        data_directory = $DataDirectory
        snapshot = $Snapshot
        autosave_interval_seconds = $Autosave
        shutdown_file = $ShutdownFile
        process_start_utc_ticks = $Process.StartTime.ToUniversalTime().Ticks
        cooperative_shutdown = $true
        api_token_required = $ApiTokenRequired
    }
    $Pending = "$Path.new.$($Process.Id)"
    $Backup = "$Path.previous.$($Process.Id)"
    try {
        $Json = $Config | ConvertTo-Json -Compress
        $Utf8 = New-Object Text.UTF8Encoding -ArgumentList $false
        [IO.File]::WriteAllText($Pending, $Json, $Utf8)
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            try {
                [IO.File]::Replace($Pending, $Path, $Backup, $true)
            } finally {
                Remove-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue
            }
        } else {
            [IO.File]::Move($Pending, $Path)
        }
    } finally {
        Remove-Item -LiteralPath $Pending -Force -ErrorAction SilentlyContinue
    }
}

function Get-ManagedServerProcess {
    param(
        [Parameter(Mandatory)][string]$PidFile,
        [Parameter(Mandatory)][string]$ExpectedExecutable
    )

    if (-not (Test-Path -LiteralPath $PidFile -PathType Leaf)) {
        return $null
    }
    $PidLines = @(Get-Content -LiteralPath $PidFile -ErrorAction SilentlyContinue)
    $PidText = $PidLines | Select-Object -First 1
    $ServerPid = 0
    if (-not [int]::TryParse($PidText, [ref]$ServerPid) -or $ServerPid -le 0) {
        Write-Warning "Ignoring invalid managed-server PID file: $PidFile"
        Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
        return $null
    }

    $Process = Get-Process -Id $ServerPid -ErrorAction SilentlyContinue
    if ($null -eq $Process) {
        Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
        return $null
    }
    try {
        $ActualPath = $Process.Path
    } catch {
        throw "PID $ServerPid is running, but its executable path cannot be verified. Refusing to stop it. Remove '$PidFile' only after verifying the process manually."
    }
    if ([string]::IsNullOrWhiteSpace($ActualPath)) {
        throw "PID $ServerPid is running, but its executable path cannot be verified. Refusing to stop it."
    }
    if ((Get-ComparablePath -Path $ActualPath) -cne
        (Get-ComparablePath -Path $ExpectedExecutable)) {
        Write-Warning "Ignoring stale PID $ServerPid because it belongs to '$ActualPath', not vectors-server."
        Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
        return $null
    }
    if ($PidLines.Count -ge 2) {
        $ExpectedStartTicks = [long]0
        if (-not [long]::TryParse($PidLines[1], [ref]$ExpectedStartTicks) -or
            $ExpectedStartTicks -le 0) {
            throw "Managed-server PID file has an invalid process identity: $PidFile"
        }
        if ($Process.StartTime.ToUniversalTime().Ticks -ne $ExpectedStartTicks) {
            throw "PID $ServerPid was reused by a different process. Refusing to stop it; remove '$PidFile' after verifying the process manually."
        }
    }
    return $Process
}

function Stop-ManagedServerProcess {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process,
        [Parameter(Mandatory)][string]$ExpectedExecutable,
        [Parameter(Mandatory)][string]$ShutdownFile,
        [bool]$SupportsCooperativeShutdown,
        [bool]$AllowHardStop
    )

    if ($Process.HasExited) {
        return
    }

    if ($SupportsCooperativeShutdown) {
        $ShutdownDirectory = Split-Path -Parent $ShutdownFile
        New-Item -ItemType Directory -Force -Path $ShutdownDirectory | Out-Null
        $Pending = "$ShutdownFile.new.$($Process.Id)"
        try {
            Remove-Item -LiteralPath $ShutdownFile -Force -ErrorAction SilentlyContinue
            [IO.File]::WriteAllBytes($Pending, [byte[]]@())
            [IO.File]::Move($Pending, $ShutdownFile)
            for ($Attempt = 0; $Attempt -lt 240; $Attempt++) {
                Start-Sleep -Milliseconds 250
                $Process.Refresh()
                if ($Process.HasExited) {
                    Remove-Item -LiteralPath $ShutdownFile -Force -ErrorAction SilentlyContinue
                    return
                }
            }
        } finally {
            Remove-Item -LiteralPath $Pending -Force -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath $ShutdownFile -Force -ErrorAction SilentlyContinue
        }
    }

    if (-not $AllowHardStop) {
        throw "Managed vectors-server PID $($Process.Id) could not be stopped cooperatively. It was left running because a hard stop could lose unsaved snapshot writes."
    }

    $Current = Get-Process -Id $Process.Id -ErrorAction SilentlyContinue
    if ($null -eq $Current) {
        return
    }
    $ExpectedStartTicks = $Process.StartTime.ToUniversalTime().Ticks
    if ($Current.StartTime.ToUniversalTime().Ticks -ne $ExpectedStartTicks -or
        (Get-ComparablePath -Path $Current.Path) -cne
        (Get-ComparablePath -Path $ExpectedExecutable)) {
        throw "Managed server identity changed before shutdown. Refusing to stop PID $($Process.Id)."
    }
    Stop-Process -Id $Current.Id -Force -ErrorAction Stop
    $Current.WaitForExit(5000) | Out-Null
    if (-not $Current.HasExited) {
        throw "Managed vectors-server PID $($Current.Id) did not stop."
    }
}

function Stage-RuntimeBinary {
    param(
        [Parameter(Mandatory)][string]$Source,
        [Parameter(Mandatory)][string]$Destination
    )

    $Directory = Split-Path -Parent $Destination
    New-Item -ItemType Directory -Force -Path $Directory | Out-Null
    $Transaction = [guid]::NewGuid().ToString("N")
    $Pending = Join-Path $Directory ".vectors-server.exe.new.$Transaction"
    $Backup = Join-Path $Directory ".vectors-server.exe.previous.$Transaction"
    $HadPrevious = Test-Path -LiteralPath $Destination -PathType Leaf
    $Changed = $true
    try {
        [IO.File]::Copy($Source, $Pending, $false)
        if ((Get-FileHash -Algorithm SHA256 -LiteralPath $Source).Hash -ne
            (Get-FileHash -Algorithm SHA256 -LiteralPath $Pending).Hash) {
            throw "Staging the managed server changed its checksum."
        }
        if ($HadPrevious -and
            (Get-FileHash -Algorithm SHA256 -LiteralPath $Source).Hash -eq
            (Get-FileHash -Algorithm SHA256 -LiteralPath $Destination).Hash) {
            $Changed = $false
            Remove-Item -LiteralPath $Pending -Force
        } elseif ($HadPrevious) {
            [IO.File]::Replace($Pending, $Destination, $Backup, $true)
        } else {
            [IO.File]::Move($Pending, $Destination)
        }
        return [pscustomobject]@{
            Destination = $Destination
            Backup = $Backup
            HadPrevious = $HadPrevious
            Changed = $Changed
        }
    } catch {
        Remove-Item -LiteralPath $Pending -Force -ErrorAction SilentlyContinue
        throw "Could not update the managed server runtime: $($_.Exception.Message)"
    }
}

function Restore-RuntimeBinary {
    param([Parameter(Mandatory)]$RuntimeUpdate)

    if (-not $RuntimeUpdate.Changed) {
        return
    }
    if ($RuntimeUpdate.HadPrevious -and
        (Test-Path -LiteralPath $RuntimeUpdate.Backup -PathType Leaf)) {
        if (Test-Path -LiteralPath $RuntimeUpdate.Destination -PathType Leaf) {
            Restore-FileAtomically -Source $RuntimeUpdate.Backup `
                -Destination $RuntimeUpdate.Destination
        } else {
            [IO.File]::Move($RuntimeUpdate.Backup, $RuntimeUpdate.Destination)
        }
    } elseif (-not $RuntimeUpdate.HadPrevious) {
        Remove-Item -LiteralPath $RuntimeUpdate.Destination -Force -ErrorAction SilentlyContinue
    }
}

function Complete-RuntimeUpdate {
    param([Parameter(Mandatory)]$RuntimeUpdate)

    if ($RuntimeUpdate.Backup) {
        Remove-Item -LiteralPath $RuntimeUpdate.Backup -Force -ErrorAction SilentlyContinue
    }
}

function Start-ManagedServer {
    param(
        [Parameter(Mandatory)][string]$Executable,
        [Parameter(Mandatory)][string]$WorkingDirectory,
        [Parameter(Mandatory)][string]$Bind,
        [AllowNull()][string]$DataDirectory,
        [AllowNull()][string]$Snapshot,
        [AllowNull()][string]$Autosave,
        [Parameter(Mandatory)][string]$ShutdownFile,
        [Parameter(Mandatory)][string]$StdoutLog,
        [Parameter(Mandatory)][string]$StderrLog
    )

    $Names = @(
        "VECTORS_BIND",
        "VECTORS_DATA_DIR",
        "VECTORS_SNAPSHOT",
        "VECTORS_AUTOSAVE_INTERVAL_SECS",
        "VECTORS_SHUTDOWN_FILE"
    )
    $Previous = @{}
    foreach ($Name in $Names) {
        $Previous[$Name] = [Environment]::GetEnvironmentVariable($Name, "Process")
    }
    try {
        Set-ProcessEnvironmentValue -Name "VECTORS_BIND" -Value $Bind
        Set-ProcessEnvironmentValue -Name "VECTORS_SHUTDOWN_FILE" -Value $ShutdownFile
        if ($Snapshot) {
            Set-ProcessEnvironmentValue -Name "VECTORS_DATA_DIR" -Value $null
            Set-ProcessEnvironmentValue -Name "VECTORS_SNAPSHOT" -Value $Snapshot
            Set-ProcessEnvironmentValue `
                -Name "VECTORS_AUTOSAVE_INTERVAL_SECS" -Value $Autosave
        } else {
            Set-ProcessEnvironmentValue -Name "VECTORS_DATA_DIR" -Value $DataDirectory
            Set-ProcessEnvironmentValue -Name "VECTORS_SNAPSHOT" -Value $null
            Set-ProcessEnvironmentValue `
                -Name "VECTORS_AUTOSAVE_INTERVAL_SECS" -Value $null
        }
        return Start-Process -FilePath $Executable -WorkingDirectory $WorkingDirectory `
            -PassThru -WindowStyle Hidden -RedirectStandardOutput $StdoutLog `
            -RedirectStandardError $StderrLog
    } finally {
        foreach ($Name in $Names) {
            Set-ProcessEnvironmentValue -Name $Name -Value $Previous[$Name]
        }
    }
}

function Write-PidFile {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process
    )

    $Pending = "$Path.new.$($Process.Id)"
    try {
        $Started = $Process.StartTime.ToUniversalTime().Ticks
        [IO.File]::WriteAllText(
            $Pending,
            "$($Process.Id)`r`n$Started`r`n",
            [Text.Encoding]::ASCII
        )
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            $Backup = "$Path.previous.$($Process.Id)"
            try {
                [IO.File]::Replace($Pending, $Path, $Backup, $true)
            } finally {
                Remove-Item -LiteralPath $Backup -Force -ErrorAction SilentlyContinue
            }
        } else {
            [IO.File]::Move($Pending, $Path)
        }
    } finally {
        Remove-Item -LiteralPath $Pending -Force -ErrorAction SilentlyContinue
    }
}

function Wait-ForServerReady {
    param(
        [Parameter(Mandatory)][System.Diagnostics.Process]$Process,
        [Parameter(Mandatory)][string]$ConsoleUrl
    )

    $LastError = $null
    for ($Attempt = 0; $Attempt -lt 60; $Attempt++) {
        $Process.Refresh()
        if ($Process.HasExited) {
            return [pscustomobject]@{
                Ready = $false
                Reason = "process exited with code $($Process.ExitCode)"
            }
        }
        try {
            $Health = Invoke-RestMethod -UseBasicParsing -Uri "$ConsoleUrl/healthz" -TimeoutSec 1
            if ($Health.status -eq "ok") {
                $Process.Refresh()
                if (-not $Process.HasExited) {
                    return [pscustomobject]@{ Ready = $true; Reason = $null }
                }
            }
        } catch {
            $LastError = $_.Exception.Message
        }
        Start-Sleep -Milliseconds 250
    }
    $Reason = "health check timed out after 15 seconds"
    if ($LastError) {
        $Reason += ": $LastError"
    }
    return [pscustomobject]@{ Ready = $false; Reason = $Reason }
}

function Get-LogTail {
    param([Parameter(Mandatory)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return "(no stderr log was created)"
    }
    $Lines = @(Get-Content -LiteralPath $Path -Tail 20 -ErrorAction SilentlyContinue)
    if ($Lines.Count -eq 0) {
        return "(stderr log is empty)"
    }
    return $Lines -join "`n"
}

function Remove-InstallerTemporaryDirectory {
    param([Parameter(Mandatory)][string]$Path)

    $ResolvedPath = [IO.Path]::GetFullPath($Path)
    $TempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $Leaf = Split-Path -Leaf $ResolvedPath
    if (-not $ResolvedPath.StartsWith($TempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        -not $Leaf.StartsWith("vectors-install-", [StringComparison]::Ordinal)) {
        throw "Refusing to remove unexpected installer directory: $ResolvedPath"
    }
    Remove-Item -LiteralPath $ResolvedPath -Recurse -Force -ErrorAction SilentlyContinue
}

try {
    $Architecture = Assert-SupportedHost
    $Release = Get-NormalizedRelease -RequestedVersion $Version

    if ([string]::IsNullOrWhiteSpace($InstallDir)) {
        $LocalAppData = [Environment]::GetFolderPath("LocalApplicationData")
        if ([string]::IsNullOrWhiteSpace($LocalAppData)) {
            throw "LOCALAPPDATA is unavailable. Supply -InstallDir explicitly."
        }
        $InstallDir = Join-Path $LocalAppData "Programs\vectors"
    }
    $InstallDir = Resolve-FullPath -Path $InstallDir -Description "InstallDir"
    Assert-NotFileSystemRoot -Path $InstallDir -Description "InstallDir"

    $SkipStart = $NoStart.IsPresent -or (Test-EnvironmentSwitch $env:VECTORS_NO_START)
    $SkipOpen = $NoOpen.IsPresent -or (Test-EnvironmentSwitch $env:VECTORS_NO_OPEN)

    $BindWasExplicit = -not [string]::IsNullOrWhiteSpace($BindAddress)
    $SnapshotWasExplicit = -not [string]::IsNullOrWhiteSpace($env:VECTORS_SNAPSHOT)
    $DataDirectoryWasExplicit = -not [string]::IsNullOrWhiteSpace($env:VECTORS_DATA_DIR)
    $AutosaveWasExplicit = -not [string]::IsNullOrWhiteSpace(
        $env:VECTORS_AUTOSAVE_INTERVAL_SECS
    )

    $DefaultStateRoot = [Environment]::GetFolderPath("LocalApplicationData")
    $StateDir = if (-not [string]::IsNullOrWhiteSpace($env:VECTORS_STATE_DIR)) {
        Resolve-FullPath -Path $env:VECTORS_STATE_DIR -Description "VECTORS_STATE_DIR"
    } else {
        Resolve-FullPath -Path (Join-Path $DefaultStateRoot "vectors") `
            -Description "VECTORS_STATE_DIR"
    }
    Assert-NotFileSystemRoot -Path $StateDir -Description "VECTORS_STATE_DIR"

    $ManagedConfigFile = Join-Path $StateDir "server.config.json"
    $ShutdownFile = Join-Path $StateDir "shutdown.request"
    $ManagedConfig = Read-ManagedServerConfig -Path $ManagedConfigFile
    $ApiTokenProvided = -not [string]::IsNullOrEmpty($env:VECTORS_API_TOKEN)

    if (-not $BindWasExplicit -and $ManagedConfig) {
        $BindAddress = [string]$ManagedConfig.bind_address
    }
    if ([string]::IsNullOrWhiteSpace($BindAddress)) {
        $BindAddress = "127.0.0.1:8080"
    }
    $Endpoint = Get-ConsoleEndpoint -Address $BindAddress
    $BindAddress = $Endpoint.BindAddress

    if ($SnapshotWasExplicit -and $DataDirectoryWasExplicit) {
        throw "VECTORS_SNAPSHOT and VECTORS_DATA_DIR are mutually exclusive; set only one storage mode."
    }
    $UseManagedSnapshot = -not $SnapshotWasExplicit -and
        -not $DataDirectoryWasExplicit -and $ManagedConfig -and
        $ManagedConfig.storage_mode -eq "snapshot"
    $SnapshotValue = if ($SnapshotWasExplicit) {
        $env:VECTORS_SNAPSHOT
    } elseif ($UseManagedSnapshot) {
        [string]$ManagedConfig.snapshot
    } else {
        $null
    }
    $Snapshot = if (-not [string]::IsNullOrWhiteSpace($SnapshotValue)) {
        Resolve-FullPath -Path $SnapshotValue -Description "VECTORS_SNAPSHOT"
    } else {
        $null
    }
    if ($Snapshot -and (Test-Path -LiteralPath $Snapshot -PathType Container)) {
        throw "VECTORS_SNAPSHOT must name a snapshot file, not a directory: $Snapshot"
    }
    $UseManagedDataDirectory = -not $SnapshotWasExplicit -and
        -not $DataDirectoryWasExplicit -and $ManagedConfig -and
        $ManagedConfig.storage_mode -eq "durable"
    $DataDir = if ($DataDirectoryWasExplicit) {
        Resolve-FullPath -Path $env:VECTORS_DATA_DIR -Description "VECTORS_DATA_DIR"
    } elseif ($UseManagedDataDirectory) {
        Resolve-FullPath -Path ([string]$ManagedConfig.data_directory) `
            -Description "managed VECTORS_DATA_DIR"
    } else {
        Resolve-FullPath -Path (Join-Path $StateDir "data") -Description "VECTORS_DATA_DIR"
    }
    Assert-NotFileSystemRoot -Path $DataDir -Description "VECTORS_DATA_DIR"
    if ((Get-ComparablePath -Path $InstallDir) -ceq (Get-ComparablePath -Path $StateDir)) {
        throw "InstallDir and VECTORS_STATE_DIR must be different directories."
    }
    if ((Get-ComparablePath -Path $InstallDir) -ceq (Get-ComparablePath -Path $DataDir)) {
        throw "InstallDir and VECTORS_DATA_DIR must be different directories."
    }

    $Autosave = $null
    if ($Snapshot) {
        $Autosave = if ($AutosaveWasExplicit) {
            $env:VECTORS_AUTOSAVE_INTERVAL_SECS.Trim()
        } elseif ($UseManagedSnapshot -and $ManagedConfig.autosave_interval_seconds) {
            [string]$ManagedConfig.autosave_interval_seconds
        } else {
            "30"
        }
        $AutosaveNumber = [uint64]0
        if (-not [uint64]::TryParse($Autosave, [ref]$AutosaveNumber) -or $AutosaveNumber -eq 0) {
            throw "VECTORS_AUTOSAVE_INTERVAL_SECS must be a positive integer."
        }
    }

    $AssetUrl = "$($Release.Url)/$Asset"
    $ChecksumUrl = "$($Release.Url)/SHA256SUMS"
    if ($PrintTarget.IsPresent) {
        Write-Output "platform=windows"
        Write-Output "architecture=$Architecture"
        Write-Output "version=$($Release.Label)"
        Write-Output "asset=$Asset"
        Write-Output "asset_url=$AssetUrl"
        Write-Output "checksum_url=$ChecksumUrl"
        Write-Output "install_dir=$InstallDir"
        Write-Output "bind_address=$BindAddress"
        Write-Output "console_url=$($Endpoint.ConsoleUrl)"
        Write-Output "state_dir=$StateDir"
        Write-Output "managed_config=$ManagedConfigFile"
        Write-Output "shutdown_file=$ShutdownFile"
        if ($Snapshot) {
            Write-Output "snapshot=$Snapshot"
        } else {
            Write-Output "data_dir=$DataDir"
        }
        Write-Output "start=$(([string](-not $SkipStart)).ToLowerInvariant())"
        Write-Output "open=$(([string](-not $SkipStart -and -not $SkipOpen)).ToLowerInvariant())"
        return
    }

    Write-Host "vectors Windows installer"
    Write-Host "  Release: $($Release.Label)"
    Write-Host "  Asset:   $Asset"
    Write-Host "  Install: $InstallDir"
    if (-not $SkipStart) {
        Write-Host "  Console: $($Endpoint.ConsoleUrl) (listen: $BindAddress)"
    }
    if (-not $PSCmdlet.ShouldProcess(
        $InstallDir,
        "Install or upgrade vectors $($Release.Label) and configure the user PATH"
    )) {
        return
    }

    $PidFile = Join-Path $StateDir "server.pid"
    $StdoutLog = Join-Path $StateDir "server.stdout.log"
    $StderrLog = Join-Path $StateDir "server.stderr.log"
    $RuntimeServer = Join-Path $StateDir "vectors-server.exe"
    $ManagedServer = Get-ManagedServerProcess -PidFile $PidFile `
        -ExpectedExecutable $RuntimeServer
    if ($ManagedServer -and $ManagedConfig -and
        $ManagedServer.StartTime.ToUniversalTime().Ticks -ne
        [long]$ManagedConfig.process_start_utc_ticks) {
        throw "Managed server configuration does not belong to PID $($ManagedServer.Id). Refusing to restart it."
    }
    if (-not $SkipStart -and $ManagedConfig -and
        [bool]$ManagedConfig.api_token_required -and -not $ApiTokenProvided) {
        throw "The managed server requires VECTORS_API_TOKEN. Set its existing token before upgrading so authentication cannot be disabled accidentally."
    }
    if (-not $SkipStart -and $ManagedServer -and -not $ManagedConfig) {
        throw "The running server predates managed configuration and was left untouched because its bind, storage mode, and shutdown safety cannot be verified. Re-run with -NoStart, stop the legacy server after protecting its data, then run the installer again with its existing VECTORS_BIND and VECTORS_DATA_DIR (or VECTORS_SNAPSHOT)."
    }

    $ExistingVersion = $null
    $ExistingVectors = Join-Path $InstallDir "vectors.exe"
    if (Test-Path -LiteralPath $ExistingVectors -PathType Leaf) {
        try {
            $ExistingVersion = Get-BinaryVersion -Path $ExistingVectors -ProgramName "vectors"
        } catch {
            Write-Warning "The existing vectors.exe could not be identified and will be replaced. $($_.Exception.Message)"
        }
    }

    $Temporary = Join-Path ([IO.Path]::GetTempPath()) (
        "vectors-install-" + [guid]::NewGuid().ToString("N")
    )
    New-Item -ItemType Directory -Path $Temporary | Out-Null
    try {
        $Tls12 = [Net.SecurityProtocolType]::Tls12
        [Net.ServicePointManager]::SecurityProtocol =
            [Net.ServicePointManager]::SecurityProtocol -bor $Tls12

        $Archive = Join-Path $Temporary $Asset
        $Checksums = Join-Path $Temporary "SHA256SUMS"
        Write-Host "Downloading $Asset..."
        Receive-ReleaseFile -Uri $AssetUrl -OutFile $Archive
        Receive-ReleaseFile -Uri $ChecksumUrl -OutFile $Checksums

        $Expected = Get-ExpectedChecksum -ChecksumFile $Checksums -AssetName $Asset
        $Actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $Archive).Hash.ToLowerInvariant()
        if (-not [string]::Equals($Actual, $Expected, [StringComparison]::Ordinal)) {
            throw "Archive checksum mismatch. Expected $Expected but received $Actual."
        }
        Write-Host "Verified SHA-256: $Actual"

        $Expanded = Join-Path $Temporary "expanded"
        Expand-Archive -LiteralPath $Archive -DestinationPath $Expanded
        $VectorsSource = Join-Path $Expanded "vectors.exe"
        $ServerSource = Join-Path $Expanded "vectors-server.exe"
        if (-not (Test-Path -LiteralPath $VectorsSource -PathType Leaf) -or
            -not (Test-Path -LiteralPath $ServerSource -PathType Leaf)) {
            throw "Release archive is missing vectors.exe or vectors-server.exe at its root."
        }
        if (Get-Command Unblock-File -ErrorAction SilentlyContinue) {
            Unblock-File -LiteralPath $VectorsSource, $ServerSource
        }

        $VectorsVersion = Get-BinaryVersion -Path $VectorsSource -ProgramName "vectors"
        $ServerVersion = Get-BinaryVersion -Path $ServerSource -ProgramName "vectors-server"
        if ($VectorsVersion -cne $ServerVersion) {
            throw "Release executables disagree on version: vectors $VectorsVersion, vectors-server $ServerVersion."
        }
        if ($Release.Version -and $VectorsVersion -cne $Release.Version) {
            throw "Requested $($Release.Tag), but the archive contains version $VectorsVersion."
        }

        $InstalledVersion = Install-ReleaseBinaries -ExpandedDirectory $Expanded `
            -DestinationDirectory $InstallDir -RequestedVersion $Release.Version
    } finally {
        Remove-InstallerTemporaryDirectory -Path $Temporary
    }

    $PathResult = Add-DirectoryToUserPath -Directory $InstallDir
    if ($ExistingVersion) {
        if ($ExistingVersion -ceq $InstalledVersion) {
            Write-Host "Reinstalled vectors $InstalledVersion in $InstallDir."
        } else {
            Write-Host "Upgraded vectors $ExistingVersion -> $InstalledVersion in $InstallDir."
        }
    } else {
        Write-Host "Installed vectors $InstalledVersion in $InstallDir."
    }
    if ($PathResult.Changed) {
        Write-Host "Added the install directory to your user PATH. New terminals can run 'vectors'."
    } elseif (-not $PathResult.Persisted) {
        Write-Host "This terminal can run vectors, but future terminals need '$InstallDir' on PATH."
    }

    $PreviousBind = $BindAddress
    $PreviousDataDir = $DataDir
    $PreviousSnapshot = $Snapshot
    $PreviousAutosave = $Autosave
    $PreviousEndpoint = $Endpoint
    $PreviousShutdownFile = $ShutdownFile
    $PreviousSupportsCooperativeShutdown = $false
    if ($ManagedConfig) {
        $PreviousBind = [string]$ManagedConfig.bind_address
        $PreviousEndpoint = Get-ConsoleEndpoint -Address $PreviousBind
        if ($ManagedConfig.storage_mode -eq "snapshot") {
            $PreviousSnapshot = Resolve-FullPath -Path ([string]$ManagedConfig.snapshot) `
                -Description "managed snapshot"
            $PreviousDataDir = $null
            $PreviousAutosave = [string]$ManagedConfig.autosave_interval_seconds
        } else {
            $PreviousDataDir = Resolve-FullPath `
                -Path ([string]$ManagedConfig.data_directory) `
                -Description "managed data directory"
            $PreviousSnapshot = $null
            $PreviousAutosave = $null
        }
        $PreviousShutdownFile = Resolve-FullPath `
            -Path ([string]$ManagedConfig.shutdown_file) `
            -Description "managed shutdown file"
        $PreviousSupportsCooperativeShutdown = [bool]$ManagedConfig.cooperative_shutdown
    }

    if ($SkipStart) {
        Write-Host "Installation complete; automatic startup was skipped."
        if ($ManagedServer) {
            Write-Host "Managed vectors-server PID $($ManagedServer.Id) is still running its existing runtime."
            Write-Host "Restart it when convenient to use vectors $InstalledVersion."
        }
        Write-Host "SQL shell: vectors"
        if ($Snapshot) {
            Write-Host "Web server: `$env:VECTORS_SNAPSHOT='$Snapshot'; `$env:VECTORS_AUTOSAVE_INTERVAL_SECS='$Autosave'; vectors-server --bind $BindAddress"
        } else {
            Write-Host "Web server: vectors-server --data-dir `"$DataDir`" --bind $BindAddress"
        }
        Write-Host "Tutorial: run 'vectors' and type '.tutorial'."
        return
    }

    New-Item -ItemType Directory -Force -Path $StateDir | Out-Null
    if ($Snapshot) {
        $SnapshotParent = Split-Path -Parent $Snapshot
        New-Item -ItemType Directory -Force -Path $SnapshotParent | Out-Null
    } else {
        New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
    }

    $HadRunningServer = $null -ne $ManagedServer
    if ($ManagedServer) {
        Write-Host "Stopping managed vectors-server PID $($ManagedServer.Id) for upgrade..."
        Stop-ManagedServerProcess -Process $ManagedServer `
            -ExpectedExecutable $RuntimeServer `
            -ShutdownFile $PreviousShutdownFile `
            -SupportsCooperativeShutdown $PreviousSupportsCooperativeShutdown `
            -AllowHardStop ($null -eq $PreviousSnapshot)
        Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
    }

    $RuntimeUpdate = $null
    $Server = $null
    $ServerLaunchAttempted = $false
    try {
        Remove-Item -LiteralPath $ShutdownFile -Force -ErrorAction SilentlyContinue
        $BindTest = Test-BindPort -Endpoint $Endpoint
        if (-not $BindTest.Available) {
            $Suggestion = Get-NextAvailableBindAddress -Endpoint $Endpoint
            $Retry = if ($Suggestion) {
                " Re-run with -BindAddress '$Suggestion' or set VECTORS_BIND=$Suggestion."
            } else {
                " Choose another port with -BindAddress or VECTORS_BIND."
            }
            throw "Cannot listen on ${BindAddress}: $($BindTest.Error).$Retry"
        }

        $RuntimeUpdate = Stage-RuntimeBinary `
            -Source (Join-Path $InstallDir "vectors-server.exe") -Destination $RuntimeServer
        $ServerLaunchAttempted = $true
        $Server = Start-ManagedServer -Executable $RuntimeServer -WorkingDirectory $StateDir `
            -Bind $BindAddress -DataDirectory $DataDir -Snapshot $Snapshot `
            -Autosave $Autosave -ShutdownFile $ShutdownFile `
            -StdoutLog $StdoutLog -StderrLog $StderrLog
        Write-PidFile -Path $PidFile -Process $Server

        $ReadyResult = Wait-ForServerReady -Process $Server -ConsoleUrl $Endpoint.ConsoleUrl
        if (-not $ReadyResult.Ready) {
            throw "vectors-server did not become ready: $($ReadyResult.Reason)"
        }
        Write-ManagedServerConfig -Path $ManagedConfigFile -Bind $BindAddress `
            -DataDirectory $DataDir -Snapshot $Snapshot -Autosave $Autosave `
            -ShutdownFile $ShutdownFile -ApiTokenRequired $ApiTokenProvided `
            -Process $Server
        Complete-RuntimeUpdate -RuntimeUpdate $RuntimeUpdate
    } catch {
        $StartError = $_
        $NewServerLog = if ($ServerLaunchAttempted) {
            Get-LogTail -Path $StderrLog
        } else {
            "(server launch was not attempted)"
        }
        $ServerStopped = $true
        if ($Server) {
            $Server.Refresh()
            if (-not $Server.HasExited) {
                try {
                    Stop-ManagedServerProcess -Process $Server `
                        -ExpectedExecutable $RuntimeServer `
                        -ShutdownFile $ShutdownFile `
                        -SupportsCooperativeShutdown $true `
                        -AllowHardStop ($null -eq $Snapshot)
                } catch {
                    $ServerStopped = $false
                    Write-Warning "The new server could not be stopped safely: $($_.Exception.Message)"
                }
            }
        }
        if ($ServerStopped) {
            Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
        }
        $RuntimeRestored = $ServerStopped -and $null -eq $RuntimeUpdate
        if ($ServerStopped -and $null -ne $RuntimeUpdate) {
            try {
                Restore-RuntimeBinary -RuntimeUpdate $RuntimeUpdate
                $RuntimeRestored = $true
            } catch {
                $RuntimeRestored = $false
                Write-Warning "Could not restore the previous managed runtime: $($_.Exception.Message)"
            }
        }

        $RollbackMessage = ""
        if ($HadRunningServer -and $RuntimeRestored -and
            (Test-Path -LiteralPath $RuntimeServer -PathType Leaf)) {
            $PreviousServer = $null
            $PreviousBecameReady = $false
            try {
                $PreviousServer = Start-ManagedServer -Executable $RuntimeServer `
                    -WorkingDirectory $StateDir -Bind $PreviousBind `
                    -DataDirectory $PreviousDataDir -Snapshot $PreviousSnapshot `
                    -Autosave $PreviousAutosave -ShutdownFile $PreviousShutdownFile `
                    -StdoutLog $StdoutLog -StderrLog $StderrLog
                Write-PidFile -Path $PidFile -Process $PreviousServer
                $PreviousReady = Wait-ForServerReady `
                    -Process $PreviousServer -ConsoleUrl $PreviousEndpoint.ConsoleUrl
                if ($PreviousReady.Ready) {
                    $PreviousBecameReady = $true
                    Write-ManagedServerConfig -Path $ManagedConfigFile `
                        -Bind $PreviousBind -DataDirectory $PreviousDataDir `
                        -Snapshot $PreviousSnapshot -Autosave $PreviousAutosave `
                        -ShutdownFile $PreviousShutdownFile `
                        -ApiTokenRequired $ApiTokenProvided -Process $PreviousServer
                    $RollbackMessage = " The previous managed runtime was restored and restarted as PID $($PreviousServer.Id)."
                } else {
                    Stop-ManagedServerProcess -Process $PreviousServer `
                        -ExpectedExecutable $RuntimeServer `
                        -ShutdownFile $PreviousShutdownFile `
                        -SupportsCooperativeShutdown $PreviousSupportsCooperativeShutdown `
                        -AllowHardStop ($null -eq $PreviousSnapshot)
                    Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
                    $RollbackMessage = " The previous runtime was restored but could not be restarted."
                }
            } catch {
                $RollbackError = $_.Exception.Message
                $PreviousStillRunning = $false
                if ($PreviousServer) {
                    $PreviousServer.Refresh()
                    if (-not $PreviousServer.HasExited) {
                        if ($PreviousBecameReady) {
                            $PreviousStillRunning = $true
                        } else {
                            try {
                                Stop-ManagedServerProcess -Process $PreviousServer `
                                    -ExpectedExecutable $RuntimeServer `
                                    -ShutdownFile $PreviousShutdownFile `
                                    -SupportsCooperativeShutdown $PreviousSupportsCooperativeShutdown `
                                    -AllowHardStop ($null -eq $PreviousSnapshot)
                            } catch {
                                $PreviousStillRunning = $true
                                Write-Warning "Could not stop the failed rollback server: $($_.Exception.Message)"
                            }
                        }
                    }
                }
                if ($PreviousStillRunning) {
                    try {
                        Write-PidFile -Path $PidFile -Process $PreviousServer
                    } catch {
                        Write-Warning "Could not refresh the rollback PID file for PID $($PreviousServer.Id): $($_.Exception.Message)"
                    }
                    try {
                        Write-ManagedServerConfig -Path $ManagedConfigFile `
                            -Bind $PreviousBind -DataDirectory $PreviousDataDir `
                            -Snapshot $PreviousSnapshot -Autosave $PreviousAutosave `
                            -ShutdownFile $PreviousShutdownFile `
                            -ApiTokenRequired $ApiTokenProvided -Process $PreviousServer
                    } catch {
                        Remove-Item -LiteralPath $ManagedConfigFile -Force `
                            -ErrorAction SilentlyContinue
                        Write-Warning "Could not refresh the rollback configuration: $($_.Exception.Message)"
                    }
                    $RollbackMessage = " The previous runtime is still running as PID $($PreviousServer.Id), but its state record needs attention: $RollbackError"
                } else {
                    Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
                    $RollbackMessage = " The previous runtime was restored but restart failed: $RollbackError"
                }
            }
        }
        throw "vectors $InstalledVersion was installed, but the managed server upgrade could not complete. $($StartError.Exception.Message)$RollbackMessage`nServer stderr:`n$NewServerLog"
    }

    Write-Host "vectors-server $InstalledVersion started with PID $($Server.Id)."
    Write-Host "Web console: $($Endpoint.ConsoleUrl)"
    Write-Host "Tutorial: open 'Start here', or run 'vectors' and type '.tutorial'."
    if ($Snapshot) {
        Write-Host "Snapshot: $Snapshot (autosave every $Autosave seconds)"
    } else {
        Write-Host "Durable data: $DataDir"
    }
    Write-Host "Logs: $StdoutLog and $StderrLog"
    Write-Host "Stop safely: [IO.File]::WriteAllBytes('$ShutdownFile', [byte[]]@())"
    Write-Host "This installer does not register a startup service or remove database files."

    if (-not $SkipOpen) {
        try {
            Start-Process -FilePath $Endpoint.ConsoleUrl | Out-Null
        } catch {
            Write-Warning "The server is ready, but the browser could not be opened. Visit $($Endpoint.ConsoleUrl) manually."
        }
    }
} catch {
    Write-Error "vectors installer failed: $($_.Exception.Message)"
    exit 1
}
