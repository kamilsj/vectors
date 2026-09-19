<#
.SYNOPSIS
Checks for or applies stable vectors updates on Windows.
.DESCRIPTION
Pins the latest official release, verifies the install.ps1 checksum before
execution, and keeps both installed binaries for rollback. Watch mode runs in
the foreground; it does not create a scheduled task or service.
.PARAMETER Check
Report the installed and latest stable versions without creating a lock or
changing files.
.PARAMETER Watch
Check immediately, then repeat every IntervalSeconds. Failures are reported
and retried only after the full interval.
.PARAMETER IntervalSeconds
Watch interval, from 60 to 604800 seconds. Defaults to six hours.
.PARAMETER InstallDir
Existing installation directory. Defaults to VECTORS_INSTALL_DIR or the
per-user Windows installer destination.
.PARAMETER NoStart
Update binaries without restarting the managed server. A stopped server is
never started by the updater, even without this option.
.PARAMETER WaitForProcessId
Wait up to 60 seconds for the CLI launcher to exit and release vectors.exe.
.PARAMETER CleanupScriptPath
Remove the updater's own temporary script on exit. Only its actual temporary
script path, with a vectors-update- prefix, is accepted.
.EXAMPLE
.\update.ps1 -Check
.EXAMPLE
.\update.ps1 -Watch -IntervalSeconds 21600
#>
[CmdletBinding()]
param(
    [switch]$Check,
    [switch]$Watch,
    [ValidateRange(60, 604800)][int]$IntervalSeconds = 21600,
    [string]$InstallDir = $env:VECTORS_INSTALL_DIR,
    [switch]$NoStart,
    [ValidateRange(0, 2147483647)][int]$WaitForProcessId = 0,
    [string]$CleanupScriptPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function ConvertTo-UpdateVersion {
    param([Parameter(Mandatory)][string]$Text, [switch]$ReleaseTag)
    $Candidate = $Text
    if ($ReleaseTag) {
        if (-not $Candidate.StartsWith("v", [StringComparison]::Ordinal)) {
            throw "The latest release must have a stable vX.Y.Z tag."
        }
        $Candidate = $Candidate.Substring(1)
    }
    if ($Candidate -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\z') {
        throw "Only stable X.Y.Z versions are supported; received '$Text'."
    }
    $Parts = @()
    foreach ($Part in $Candidate.Split('.')) {
        $Number = [uint32]0
        if (-not [uint32]::TryParse($Part, [ref]$Number)) {
            throw "A version component exceeds the unsigned 32-bit range."
        }
        $Parts += $Number
    }
    return [pscustomobject]@{ Text = $Candidate; Tag = "v$Candidate"; Parts = $Parts }
}

function Compare-UpdateVersions {
    param([Parameter(Mandatory)]$Left, [Parameter(Mandatory)]$Right)
    for ($Index = 0; $Index -lt 3; $Index++) {
        if ($Left.Parts[$Index] -lt $Right.Parts[$Index]) { return -1 }
        if ($Left.Parts[$Index] -gt $Right.Parts[$Index]) { return 1 }
    }
    return 0
}

function Resolve-UpdateInstallDirectory {
    param([AllowEmptyString()][string]$Directory)
    if ([string]::IsNullOrWhiteSpace($Directory)) {
        $Base = [Environment]::GetFolderPath("LocalApplicationData")
        if ([string]::IsNullOrWhiteSpace($Base)) { throw "Supply -InstallDir; LOCALAPPDATA is unavailable." }
        $Directory = Join-Path $Base "Programs\vectors"
    }
    $Expanded = [Environment]::ExpandEnvironmentVariables($Directory)
    if ($Expanded.IndexOf('"') -ge 0 -or $Expanded.IndexOf(';') -ge 0) {
        throw "InstallDir cannot contain quotation marks or semicolons."
    }
    $FullPath = [IO.Path]::GetFullPath($Expanded)
    if ($FullPath.TrimEnd('\', '/') -eq [IO.Path]::GetPathRoot($FullPath).TrimEnd('\', '/')) {
        throw "InstallDir cannot be a filesystem root."
    }
    $FullPath = $FullPath.TrimEnd('\', '/')
    if (-not (Test-Path -LiteralPath $FullPath -PathType Container)) {
        throw "No installation exists at '$FullPath'. Run the installer first."
    }
    foreach ($Candidate in @($FullPath, (Join-Path $FullPath "vectors.exe"), (Join-Path $FullPath "vectors-server.exe"))) {
        if (([IO.File]::GetAttributes($Candidate) -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Symbolic links and reparse-point installations are not updated. Use the installation's package manager instead."
        }
    }
    foreach ($Name in @("vectors.exe", "vectors-server.exe")) {
        if (-not (Test-Path -LiteralPath (Join-Path $FullPath $Name) -PathType Leaf)) {
            throw "The installation is missing $Name. Repair it with the installer first."
        }
    }
    return $FullPath
}

function Get-UpdateBinaryVersion {
    param([Parameter(Mandatory)][string]$Path, [string]$Program = "vectors")
    $Process = New-Object Diagnostics.Process
    try {
        $Process.StartInfo = New-Object Diagnostics.ProcessStartInfo
        $Process.StartInfo.FileName = $Path
        $Process.StartInfo.Arguments = "--version"
        $Process.StartInfo.UseShellExecute = $false
        $Process.StartInfo.CreateNoWindow = $true
        $Process.StartInfo.RedirectStandardOutput = $true
        $Process.StartInfo.RedirectStandardError = $true
        if (-not $Process.Start()) { throw "Could not read the installed version." }
        # A normal version line fits in the pipe. Excessive output times out
        # instead of allocating an unbounded ReadToEndAsync buffer.
        if (-not $Process.WaitForExit(10000)) {
            $Process.Kill()
            $Process.WaitForExit(5000) | Out-Null
            throw "The installed $Program --version command timed out."
        }
        $Output = $Process.StandardOutput.ReadToEnd().Trim()
        $Errors = $Process.StandardError.ReadToEnd().Trim()
        if ($Process.ExitCode -ne 0 -or $Errors.Length -gt 0 -or $Output.Length -gt 128 -or
            $Output -cnotmatch ('^' + [regex]::Escape($Program) + ' ([^\r\n]+)\z')) {
            throw "The installed $Program returned an unexpected version response."
        }
        return ConvertTo-UpdateVersion -Text $Matches[1]
    } finally { $Process.Dispose() }
}

function Assert-UpdateDownloadUri {
    param([Parameter(Mandatory)][uri]$Uri, [switch]$Redirect)
    if (-not $Uri.IsAbsoluteUri -or $Uri.Scheme -cne "https" -or $Uri.Port -ne 443 -or
        $Uri.UserInfo -or $Uri.Fragment) { throw "Updater downloads require trusted HTTPS URLs." }
    if ($Uri.Host -ceq "api.github.com" -and $Uri.AbsolutePath -ceq "/repos/kamilsj/vectors/releases/latest" -and -not $Uri.Query) { return }
    if ($Uri.Host -ceq "github.com" -and -not $Uri.Query -and
        $Uri.AbsolutePath -cmatch '^/kamilsj/vectors/releases/download/v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)/(?:SHA256SUMS|install\.ps1)\z') { return }
    # GitHub signs release-asset redirects to these GitHub-owned hosts. They
    # are accepted only after an allowed fixed-repository request redirects.
    if ($Redirect -and $Uri.Host -in @("release-assets.githubusercontent.com", "objects.githubusercontent.com")) { return }
    throw "Refusing an updater download outside the official vectors release URLs."
}

function Receive-UpdateBytes {
    param([Parameter(Mandatory)][string]$Uri, [Parameter(Mandatory)][int]$MaxBytes)
    Add-Type -AssemblyName System.Net.Http
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $Handler = New-Object Net.Http.HttpClientHandler
    $Handler.AllowAutoRedirect = $false
    $Handler.UseCookies = $false
    $Handler.UseDefaultCredentials = $false
    $Client = New-Object Net.Http.HttpClient -ArgumentList $Handler
    $Cancellation = New-Object Threading.CancellationTokenSource
    $Cancellation.CancelAfter(30000)
    $Deadline = [Diagnostics.Stopwatch]::StartNew()
    $Response = $null
    try {
        $CurrentUri = [uri]$Uri
        Assert-UpdateDownloadUri -Uri $CurrentUri
        for ($Hop = 0; $Hop -le 5; $Hop++) {
            $Request = New-Object Net.Http.HttpRequestMessage -ArgumentList ([Net.Http.HttpMethod]::Get), $CurrentUri
            try {
                $Request.Headers.UserAgent.ParseAdd("vectors-updater")
                $DownloadTask = $Client.SendAsync($Request, [Net.Http.HttpCompletionOption]::ResponseHeadersRead, $Cancellation.Token)
                $Remaining = [Math]::Max(0, 30000 - [int]$Deadline.ElapsedMilliseconds)
                if (-not $DownloadTask.Wait($Remaining)) { throw "Release download timed out after 30 seconds." }
                $Response = $DownloadTask.GetAwaiter().GetResult()
            } finally { $Request.Dispose() }
            $Status = [int]$Response.StatusCode
            if ($Status -in @(301, 302, 303, 307, 308)) {
                if ($Hop -eq 5 -or $null -eq $Response.Headers.Location) { throw "Too many or invalid release redirects." }
                $NextUri = New-Object Uri -ArgumentList $CurrentUri, $Response.Headers.Location
                Assert-UpdateDownloadUri -Uri $NextUri -Redirect
                $Response.Dispose()
                $Response = $null
                $CurrentUri = $NextUri
                continue
            }
            if ($Status -ne 200) { throw "Official release download returned HTTP $Status." }
            if ($Response.Content.Headers.ContentLength -gt $MaxBytes) { throw "Release response exceeds its size limit." }
            $StreamTask = $Response.Content.ReadAsStreamAsync()
            $Remaining = [Math]::Max(0, 30000 - [int]$Deadline.ElapsedMilliseconds)
            if (-not $StreamTask.Wait($Remaining)) { throw "Release download timed out after 30 seconds." }
            $Stream = $StreamTask.GetAwaiter().GetResult()
            $Output = New-Object IO.MemoryStream
            try {
                $Buffer = New-Object byte[] 8192
                while ($true) {
                    $Remaining = [Math]::Max(0, 30000 - [int]$Deadline.ElapsedMilliseconds)
                    if ($Remaining -eq 0) { throw "Release download timed out after 30 seconds." }
                    $ReadTask = $Stream.ReadAsync($Buffer, 0, $Buffer.Length, $Cancellation.Token)
                    if (-not $ReadTask.Wait($Remaining)) { throw "Release download timed out after 30 seconds." }
                    $Read = $ReadTask.GetAwaiter().GetResult()
                    if ($Read -eq 0) { break }
                    if ($Output.Length + $Read -gt $MaxBytes) { throw "Release response exceeds its size limit." }
                    $Output.Write($Buffer, 0, $Read)
                }
                if ($Output.Length -eq 0) { throw "The release response was empty." }
                return ,$Output.ToArray()
            } finally { $Stream.Dispose(); $Output.Dispose() }
        }
    } finally {
        if ($null -ne $Response) { $Response.Dispose() }
        $Cancellation.Dispose()
        $Client.Dispose()
        $Handler.Dispose()
    }
}

function ConvertFrom-UpdateUtf8 {
    param([Parameter(Mandatory)][byte[]]$Bytes)
    $Encoding = New-Object Text.UTF8Encoding -ArgumentList $false, $true
    return $Encoding.GetString($Bytes).TrimStart([char]0xfeff)
}

function Get-LatestUpdateVersion {
    $Bytes = Receive-UpdateBytes -Uri "https://api.github.com/repos/kamilsj/vectors/releases/latest" -MaxBytes 1048576
    $Release = (ConvertFrom-UpdateUtf8 -Bytes $Bytes) | ConvertFrom-Json
    foreach ($Name in @("tag_name", "draft", "prerelease")) {
        if ($Release.PSObject.Properties.Name -notcontains $Name) { throw "The release response is missing $Name." }
    }
    if ($Release.draft -isnot [bool] -or $Release.prerelease -isnot [bool] -or $Release.draft -or $Release.prerelease) {
        throw "The latest release is not a published stable release."
    }
    return ConvertTo-UpdateVersion -Text ([string]$Release.tag_name) -ReleaseTag
}

function Get-UpdateInstallerChecksum {
    param([Parameter(Mandatory)][string]$Checksums)
    $Entries = @($Checksums -split "`n" | Where-Object { $_.TrimEnd([char]13) -match '[ \t]\*?install\.ps1[ \t]*\z' })
    if ($Entries.Count -ne 1 -or $Entries[0].TrimEnd([char]13) -cnotmatch '^(?<hash>[0-9a-fA-F]{64})[ \t]+\*?install\.ps1[ \t]*\z') {
        throw "SHA256SUMS must contain exactly one valid SHA-256 entry for install.ps1."
    }
    return $Matches.hash.ToLowerInvariant()
}

function Get-UpdateFileHash {
    param([Parameter(Mandatory)][string]$Path)
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Read-UpdateStateText {
    param([Parameter(Mandatory)][string]$Path, [int]$MaxBytes = 65536)
    $Stream = [IO.File]::OpenRead($Path)
    try {
        if ($Stream.Length -gt $MaxBytes) { throw "Managed server state exceeds its size limit." }
        $Bytes = New-Object byte[] ($MaxBytes + 1)
        $Count = 0
        while (($Read = $Stream.Read($Bytes, $Count, $Bytes.Length - $Count)) -gt 0) {
            $Count += $Read
            if ($Count -gt $MaxBytes) { throw "Managed server state exceeds its size limit." }
        }
        $Encoding = New-Object Text.UTF8Encoding -ArgumentList $false, $true
        return $Encoding.GetString($Bytes, 0, $Count).TrimStart([char]0xfeff)
    } finally { $Stream.Dispose() }
}

function Get-UpdateManagedServer {
    $Directory = if ($env:VECTORS_STATE_DIR) { [IO.Path]::GetFullPath($env:VECTORS_STATE_DIR) }
        else { Join-Path ([Environment]::GetFolderPath("LocalApplicationData")) "vectors" }
    $ConfigPath = Join-Path $Directory "server.config.json"
    $Config = $null
    if (Test-Path -LiteralPath $ConfigPath -PathType Leaf) {
        $Config = (Read-UpdateStateText -Path $ConfigPath) | ConvertFrom-Json
        foreach ($Name in @("schema", "api_token_required", "process_start_utc_ticks", "bind_address", "storage_mode", "shutdown_file")) {
            if ($Config.PSObject.Properties.Name -notcontains $Name) { throw "Managed server configuration is incomplete; missing $Name." }
        }
        if ($Config.schema -ne 1 -or $Config.api_token_required -isnot [bool]) { throw "Managed server configuration is invalid." }
    }
    $PidPath = Join-Path $Directory "server.pid"
    if (-not (Test-Path -LiteralPath $PidPath -PathType Leaf)) { return [pscustomobject]@{ Running = $false; Config = $Config } }
    $Lines = (Read-UpdateStateText -Path $PidPath -MaxBytes 1024).Trim() -split '\r?\n'
    $ServerId = 0
    if (-not [int]::TryParse($Lines[0], [ref]$ServerId) -or $ServerId -le 0) { throw "The managed-server PID file is invalid; inspect it before updating." }
    $Server = Get-Process -Id $ServerId -ErrorAction SilentlyContinue
    if ($null -eq $Server) { return [pscustomobject]@{ Running = $false; Config = $Config } }
    if ($null -eq $Config) { throw "The running server has no managed configuration. Use the installer manually after reviewing its settings." }
    $ExpectedPath = [IO.Path]::GetFullPath((Join-Path $Directory "vectors-server.exe"))
    if (-not $Server.Path -or -not [string]::Equals([IO.Path]::GetFullPath($Server.Path), $ExpectedPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw "The recorded PID does not belong to the managed server. It was left untouched."
    }
    $StartTicks = $Server.StartTime.ToUniversalTime().Ticks
    $RecordedTicks = [long]0
    if (-not [long]::TryParse([string]$Config.process_start_utc_ticks, [ref]$RecordedTicks) -or $RecordedTicks -ne $StartTicks) {
        throw "Managed server process identity changed. It was left untouched."
    }
    if ($Lines.Count -gt 1 -and (-not [long]::TryParse($Lines[1], [ref]$RecordedTicks) -or $RecordedTicks -ne $StartTicks)) {
        throw "The managed-server PID file identifies a different process. It was left untouched."
    }
    return [pscustomobject]@{ Running = $true; Config = $Config }
}

function Invoke-UpdateInstaller {
    param([string]$ScriptPath, [string]$Tag, [string]$Directory, [bool]$SkipStart)
    $PowerShellPath = Join-Path $env:WINDIR "System32\WindowsPowerShell\v1.0\powershell.exe"
    if (-not (Test-Path -LiteralPath $PowerShellPath -PathType Leaf)) { throw "Windows PowerShell could not be found." }
    $Options = @{ Version = $Tag; InstallDir = $Directory; NoOpen = $true; NoStart = $SkipStart }
    $Command = '$options = @{}; ($env:VECTORS_VERIFIED_INSTALLER_OPTIONS | ConvertFrom-Json).PSObject.Properties | ForEach-Object { $options[$_.Name] = $_.Value }; & ([scriptblock]::Create([IO.File]::ReadAllText($env:VECTORS_VERIFIED_INSTALLER_SCRIPT))) @options; if (-not $?) { exit 1 }'
    $Process = New-Object Diagnostics.Process
    try {
        $Process.StartInfo = New-Object Diagnostics.ProcessStartInfo
        $Process.StartInfo.FileName = $PowerShellPath
        $Process.StartInfo.Arguments = '-NoProfile -NonInteractive -Command "' + $Command + '"'
        $Process.StartInfo.UseShellExecute = $false
        $Process.StartInfo.EnvironmentVariables["VECTORS_VERIFIED_INSTALLER_SCRIPT"] = $ScriptPath
        $Process.StartInfo.EnvironmentVariables["VECTORS_VERIFIED_INSTALLER_OPTIONS"] = ($Options | ConvertTo-Json -Compress)
        $Process.StartInfo.EnvironmentVariables.Remove("VECTORS_UPDATER_SCRIPT")
        $Process.StartInfo.EnvironmentVariables.Remove("VECTORS_UPDATER_OPTIONS")
        if (-not $Process.Start()) { throw "Could not start the verified installer." }
        $Process.WaitForExit()
        if ($Process.ExitCode -ne 0) { throw "The verified installer exited with code $($Process.ExitCode)." }
    } finally { $Process.Dispose() }
}

function Restore-UpdateBinaries {
    param([string]$Directory, [string]$BackupDirectory)
    foreach ($Name in @("vectors.exe", "vectors-server.exe")) {
        $Backup = Join-Path $BackupDirectory $Name
        $Destination = Join-Path $Directory $Name
        if ((Test-Path -LiteralPath $Destination -PathType Leaf) -and (Get-UpdateFileHash $Destination) -ceq (Get-UpdateFileHash $Backup)) { continue }
        $Pending = Join-Path $Directory (".$Name.rollback." + [guid]::NewGuid().ToString("N"))
        $Discard = "$Pending.discard"
        try {
            [IO.File]::Copy($Backup, $Pending, $false)
            if (Test-Path -LiteralPath $Destination -PathType Leaf) { [IO.File]::Replace($Pending, $Destination, $Discard, $true) }
            else { [IO.File]::Move($Pending, $Destination) }
            if ((Get-UpdateFileHash $Destination) -cne (Get-UpdateFileHash $Backup)) { throw "Rollback checksum mismatch for $Name." }
        } finally {
            Remove-Item -LiteralPath $Pending, $Discard -Force -ErrorAction SilentlyContinue
        }
    }
}

function Invoke-UpdateIteration {
    param([string]$Directory, [bool]$CheckOnly, [bool]$SkipStart)
    $Lock = $null
    $Temporary = $null
    $PreserveBackup = $false
    try {
        if (-not $CheckOnly) {
            try { $Lock = [IO.File]::Open((Join-Path $Directory ".vectors-update.lock"), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None) }
            catch { throw "Another updater is using this installation, or its lock cannot be opened." }
        }
        $Installed = Get-UpdateBinaryVersion -Path (Join-Path $Directory "vectors.exe")
        $InstalledServer = Get-UpdateBinaryVersion -Path (Join-Path $Directory "vectors-server.exe") -Program "vectors-server"
        if ((Compare-UpdateVersions $Installed $InstalledServer) -ne 0) { throw "The installed vectors and vectors-server versions disagree. Repair the installation first." }
        $Latest = Get-LatestUpdateVersion
        Write-Host "Installed: vectors $($Installed.Text); latest stable release: $($Latest.Tag)."
        if ((Compare-UpdateVersions -Left $Latest -Right $Installed) -le 0) {
            Write-Host "No newer stable version is available. The installation was left unchanged."
            return
        }
        if ($CheckOnly) { Write-Host "Update available: $($Installed.Text) -> $($Latest.Text)."; return }
        $Managed = Get-UpdateManagedServer
        $DoNotStart = $SkipStart -or -not $Managed.Running
        if (-not $DoNotStart -and $Managed.Config.api_token_required -and [string]::IsNullOrEmpty($env:VECTORS_API_TOKEN)) {
            throw "The managed server requires its existing VECTORS_API_TOKEN before an update can restart it. Supply it or use -NoStart."
        }
        $Temporary = Join-Path ([IO.Path]::GetTempPath()) ("vectors-update-" + [guid]::NewGuid().ToString("N"))
        [IO.Directory]::CreateDirectory($Temporary) | Out-Null
        $BaseUrl = "https://github.com/kamilsj/vectors/releases/download/$($Latest.Tag)"
        $Checksums = ConvertFrom-UpdateUtf8 -Bytes (Receive-UpdateBytes -Uri "$BaseUrl/SHA256SUMS" -MaxBytes 262144)
        $Expected = Get-UpdateInstallerChecksum -Checksums $Checksums
        $ScriptBytes = Receive-UpdateBytes -Uri "$BaseUrl/install.ps1" -MaxBytes 1048576
        $InstallerPath = Join-Path $Temporary "install.ps1"
        [IO.File]::WriteAllBytes($InstallerPath, $ScriptBytes)
        if ((Get-UpdateFileHash $InstallerPath) -cne $Expected) { throw "Installer SHA-256 verification failed. Nothing was installed." }
        foreach ($Name in @("vectors.exe", "vectors-server.exe")) {
            $Source = Join-Path $Directory $Name
            $Backup = Join-Path $Temporary $Name
            [IO.File]::Copy($Source, $Backup, $false)
            if ((Get-UpdateFileHash $Source) -cne (Get-UpdateFileHash $Backup)) { throw "Could not verify the backup of $Name." }
        }
        try {
            Invoke-UpdateInstaller -ScriptPath $InstallerPath -Tag $Latest.Tag -Directory $Directory -SkipStart $DoNotStart
            foreach ($Program in @("vectors", "vectors-server")) {
                $Version = Get-UpdateBinaryVersion -Path (Join-Path $Directory "$Program.exe") -Program $Program
                if ((Compare-UpdateVersions $Version $Latest) -ne 0) { throw "The installed $Program version does not match the pinned release." }
            }
        } catch {
            $UpdateError = $_
            try { Restore-UpdateBinaries -Directory $Directory -BackupDirectory $Temporary }
            catch { $PreserveBackup = $true; throw "Update failed and binary rollback needs attention. Backups remain in '$Temporary'. $($_.Exception.Message)" }
            throw "Update failed; the previous installed binaries were restored. $($UpdateError.Exception.Message)"
        }
        Write-Host "Updated vectors $($Installed.Text) -> $($Latest.Text)."
    } finally {
        if ($Temporary -and -not $PreserveBackup) { Remove-Item -LiteralPath $Temporary -Recurse -Force -ErrorAction SilentlyContinue }
        if ($null -ne $Lock) { $Lock.Dispose() }
    }
}

function Assert-UpdateCleanupPath {
    param([string]$CleanupPath, [string]$RunningScriptPath)
    if ([string]::IsNullOrWhiteSpace($CleanupPath)) { return $null }
    $Expected = if ($RunningScriptPath) { $RunningScriptPath } else { $env:VECTORS_UPDATER_SCRIPT }
    if (-not $Expected) { throw "Cannot verify the updater's temporary script path." }
    $FullPath = [IO.Path]::GetFullPath($CleanupPath)
    $TempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not [string]::Equals($FullPath, [IO.Path]::GetFullPath($Expected), [StringComparison]::OrdinalIgnoreCase) -or
        -not $FullPath.StartsWith($TempRoot, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($FullPath) -cnotmatch '^vectors-update-[A-Za-z0-9-]+\.ps1\z') {
        throw "CleanupScriptPath must identify this updater's own temporary script."
    }
    return $FullPath
}

function Invoke-VectorsUpdate {
    param([bool]$CheckOnly, [bool]$WatchMode, [int]$Interval, [string]$Directory, [bool]$SkipStart, [int]$ParentProcessId)
    if ($CheckOnly -and $WatchMode) { throw "-Check and -Watch cannot be combined." }
    if ($Interval -lt 60 -or $Interval -gt 604800) { throw "IntervalSeconds must be between 60 and 604800." }
    if (-not [string]::IsNullOrWhiteSpace($env:VECTORS_VERSION)) { throw "VECTORS_VERSION pins this installation. Clear it before enabling automatic stable updates." }
    $SkipStart = $SkipStart -or ($env:VECTORS_NO_START -match '^(?i:1|true|yes|on)\z')
    if ($PSVersionTable.PSVersion -lt [version]"5.1" -or $env:OS -ne "Windows_NT") { throw "This updater requires Windows PowerShell 5.1 or newer on Windows." }
    $Directory = Resolve-UpdateInstallDirectory -Directory $Directory
    if ($ParentProcessId -gt 0) {
        if ($ParentProcessId -eq $PID) { throw "The updater cannot wait for its own process." }
        $Parent = Get-Process -Id $ParentProcessId -ErrorAction SilentlyContinue
        if ($null -ne $Parent) {
            try {
                # Open the process handle before checking identity so PID reuse
                # cannot switch the process observed by WaitForExit.
                [void]$Parent.Handle
                if (-not $Parent.HasExited -and ([IO.Path]::GetFileName($Parent.Path) -ine "vectors.exe")) {
                    throw "The launcher PID no longer belongs to vectors.exe. No update was attempted."
                }
                if (-not $Parent.WaitForExit(60000)) { throw "The vectors command did not exit within 60 seconds. No update was attempted." }
            } finally { $Parent.Dispose() }
        }
    }
    do {
        try { Invoke-UpdateIteration -Directory $Directory -CheckOnly $CheckOnly -SkipStart $SkipStart }
        catch {
            if (-not $WatchMode) { throw }
            Write-Warning "Update check failed: $($_.Exception.Message) Will retry in $Interval seconds."
        }
        if ($WatchMode) { Start-Sleep -Seconds $Interval }
    } while ($WatchMode)
}

$VerifiedCleanup = $null
try {
    $VerifiedCleanup = Assert-UpdateCleanupPath -CleanupPath $CleanupScriptPath -RunningScriptPath $PSCommandPath
    Invoke-VectorsUpdate -CheckOnly $Check.IsPresent -WatchMode $Watch.IsPresent -Interval $IntervalSeconds `
        -Directory $InstallDir -SkipStart $NoStart.IsPresent -ParentProcessId $WaitForProcessId
} catch {
    throw "vectors updater failed: $($_.Exception.Message)"
} finally {
    if ($VerifiedCleanup -and -not $Check.IsPresent) { Remove-Item -LiteralPath $VerifiedCleanup -Force -ErrorAction SilentlyContinue }
}
