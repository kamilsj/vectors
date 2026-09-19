# Network-free updater tests. Run with Windows PowerShell 5.1 in CI.
# Loading function ASTs avoids executing the script entry point or any real
# installation. All release requests and installer invocations are replaced.
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$Updater = Join-Path (Split-Path -Parent $PSScriptRoot) "update.ps1"
$Tokens = $null
$ParseErrors = $null
$Ast = [Management.Automation.Language.Parser]::ParseFile($Updater, [ref]$Tokens, [ref]$ParseErrors)
if ($ParseErrors.Count) { throw ($ParseErrors | Out-String) }
foreach ($Statement in $Ast.EndBlock.Statements) {
    if ($Statement -is [Management.Automation.Language.FunctionDefinitionAst]) {
        . ([scriptblock]::Create($Statement.Extent.Text))
    }
}

function Assert-Equal {
    param($Actual, $Expected, [string]$Because = "Values differ")
    if ($Actual -cne $Expected) { throw "$Because. Expected '$Expected', received '$Actual'." }
}
function Assert-Fails {
    param([scriptblock]$Action, [string]$Pattern)
    $Failed = $false
    try { & $Action } catch {
        $Failed = $true
        if ($_.Exception.Message -notmatch $Pattern) { throw "Unexpected error: $($_.Exception.Message); expected $Pattern" }
    }
    if (-not $Failed) { throw "Expected failure matching $Pattern." }
}
function Test-Case {
    param([string]$Name, [scriptblock]$Body)
    & $Body
    $script:Passed += 1
    Write-Host "PASS $Name"
}
function New-UpdateFixture {
    $Directory = Join-Path $script:TestRoot ([guid]::NewGuid().ToString("N"))
    [IO.Directory]::CreateDirectory($Directory) | Out-Null
    [IO.File]::WriteAllText((Join-Path $Directory "vectors.exe"), "0.7.0")
    [IO.File]::WriteAllText((Join-Path $Directory "vectors-server.exe"), "0.7.0")
    [IO.File]::WriteAllText((Join-Path $Directory "database.sentinel"), "preserve my data")
    return $Directory
}
function Get-UpdateBinaryVersion {
    param([string]$Path, [string]$Program)
    return ConvertTo-UpdateVersion -Text ([IO.File]::ReadAllText($Path))
}
function Get-LatestUpdateVersion { return ConvertTo-UpdateVersion "0.8.0" }
function Get-UpdateManagedServer { return [pscustomobject]@{ Running = $false; Config = $null } }
function Receive-UpdateBytes {
    param([string]$Uri, [int]$MaxBytes)
    $script:DownloadUris.Add($Uri)
    if ($Uri.EndsWith("/SHA256SUMS")) { return ,[Text.Encoding]::UTF8.GetBytes("$script:FixtureHash  install.ps1`n") }
    if ($Uri.EndsWith("/install.ps1")) { return ,$script:FixtureBytes }
    throw "Unexpected network request: $Uri"
}
function Invoke-UpdateInstaller {
    param([string]$ScriptPath, [string]$Tag, [string]$Directory, [bool]$SkipStart)
    $script:Invocations.Add([pscustomobject]@{ Tag = $Tag; Directory = $Directory; SkipStart = $SkipStart })
    [IO.File]::WriteAllText((Join-Path $Directory "vectors.exe"), "0.8.0")
    [IO.File]::WriteAllText((Join-Path $Directory "vectors-server.exe"), "0.8.0")
}
function Reset-UpdateCalls {
    $script:DownloadUris.Clear()
    $script:Invocations.Clear()
}

$script:Passed = 0
$script:TestRoot = Join-Path ([IO.Path]::GetTempPath()) ("vectors-updater-tests-" + [guid]::NewGuid().ToString("N"))
[IO.Directory]::CreateDirectory($script:TestRoot) | Out-Null
$script:FixtureBytes = [Text.Encoding]::UTF8.GetBytes("# verified network-free installer fixture`n")
$HashAlgorithm = [Security.Cryptography.SHA256]::Create()
try { $script:FixtureHash = -join ($HashAlgorithm.ComputeHash($script:FixtureBytes) | ForEach-Object { $_.ToString("x2") }) }
finally { $HashAlgorithm.Dispose() }
$script:DownloadUris = New-Object 'Collections.Generic.List[string]'
$script:Invocations = New-Object 'Collections.Generic.List[object]'
$OriginalToken = $env:VECTORS_API_TOKEN
$OriginalPin = $env:VECTORS_VERSION
$OriginalNoStart = $env:VECTORS_NO_START
$OriginalOS = $env:OS
try {
    $env:VECTORS_API_TOKEN = $null
    $env:VECTORS_VERSION = $null
    $env:VECTORS_NO_START = $null

    Test-Case "stable versions compare numerically and allow the u32 boundary" {
        Assert-Equal (Compare-UpdateVersions (ConvertTo-UpdateVersion "1.10.0") (ConvertTo-UpdateVersion "1.9.9")) 1
        Assert-Equal (Compare-UpdateVersions (ConvertTo-UpdateVersion "0.7.0") (ConvertTo-UpdateVersion "0.7.0")) 0
        Assert-Equal (Compare-UpdateVersions (ConvertTo-UpdateVersion "1.0.0") (ConvertTo-UpdateVersion "2.0.0")) -1
        Assert-Equal (ConvertTo-UpdateVersion "v4294967295.0.0" -ReleaseTag).Parts[0] ([uint32]::MaxValue)
        foreach ($Bad in @("01.2.3", "1.2", "1.2.3-rc.1", "1.2.3+build", "1.2.3`n", "4294967296.0.0", "-1.2.3")) {
            Assert-Fails { ConvertTo-UpdateVersion $Bad } "stable|32-bit"
        }
        Assert-Fails { ConvertTo-UpdateVersion "1.2.3" -ReleaseTag } "stable"
    }
    Test-Case "checksum requires exactly one valid installer entry" {
        Assert-Equal (Get-UpdateInstallerChecksum "$script:FixtureHash  install.ps1`r`n") $script:FixtureHash
        Assert-Equal (Get-UpdateInstallerChecksum "$script:FixtureHash *install.ps1`n") $script:FixtureHash
        Assert-Fails { Get-UpdateInstallerChecksum "$script:FixtureHash  install.ps1`n$script:FixtureHash  install.ps1" } "exactly one"
        Assert-Fails { Get-UpdateInstallerChecksum "invalid install.ps1" } "exactly one"
        Assert-Fails { Get-UpdateInstallerChecksum "$script:FixtureHash  another.ps1" } "exactly one"
        Assert-Fails { Get-UpdateInstallerChecksum "$script:FixtureHash  install.ps1`ninvalid install.ps1" } "exactly one"
    }
    Test-Case "only the fixed official HTTPS locations and redirected GitHub assets are allowed" {
        Assert-UpdateDownloadUri "https://api.github.com/repos/kamilsj/vectors/releases/latest"
        Assert-UpdateDownloadUri "https://github.com/kamilsj/vectors/releases/download/v0.8.0/install.ps1"
        Assert-UpdateDownloadUri "https://release-assets.githubusercontent.com/asset?signature=fixture" -Redirect
        foreach ($Bad in @("http://github.com/kamilsj/vectors/releases/download/v0.8.0/install.ps1", "https://github.com/other/project/releases/download/v0.8.0/install.ps1", "https://github.com:444/kamilsj/vectors/releases/download/v0.8.0/install.ps1", "https://github.com@evil.example/x", "https://github.com/kamilsj/vectors/releases/download/v0.8.0-rc.1/install.ps1", "https://evil.example/install.ps1")) {
            Assert-Fails { Assert-UpdateDownloadUri $Bad -Redirect } "trusted HTTPS|outside"
        }
        Assert-Fails { Assert-UpdateDownloadUri "https://release-assets.githubusercontent.com/asset" } "outside"
    }
    Test-Case "invalid UTF-8 is rejected" {
        Assert-Fails { ConvertFrom-UpdateUtf8 -Bytes ([byte[]]@(255)) } "translate|Unable|invalid"
    }
    Test-Case "check reports an available update without creating any files" {
        Reset-UpdateCalls
        $Directory = New-UpdateFixture
        $Before = @([IO.Directory]::GetFiles($Directory)).Count
        Invoke-UpdateIteration -Directory $Directory -CheckOnly $true -SkipStart $false
        Assert-Equal @([IO.Directory]::GetFiles($Directory)).Count $Before
        Assert-Equal (Test-Path (Join-Path $Directory ".vectors-update.lock")) $false
        Assert-Equal $script:DownloadUris.Count 0
        Assert-Equal $script:Invocations.Count 0
        Assert-Equal ([IO.File]::ReadAllText((Join-Path $Directory "vectors.exe"))) "0.7.0"
    }
    Test-Case "equal or older latest versions do not reinstall or downgrade" {
        Reset-UpdateCalls
        $Directory = New-UpdateFixture
        function Get-LatestUpdateVersion { return ConvertTo-UpdateVersion "0.7.0" }
        Invoke-UpdateIteration -Directory $Directory -CheckOnly $false -SkipStart $false
        function Get-LatestUpdateVersion { return ConvertTo-UpdateVersion "0.6.99" }
        Invoke-UpdateIteration -Directory $Directory -CheckOnly $false -SkipStart $false
        Assert-Equal $script:DownloadUris.Count 0
        Assert-Equal $script:Invocations.Count 0
        Assert-Equal ([IO.File]::ReadAllText((Join-Path $Directory "vectors.exe"))) "0.7.0"
    }
    Test-Case "mismatched installed binaries fail before release downloads" {
        Reset-UpdateCalls
        $Directory = New-UpdateFixture
        [IO.File]::WriteAllText((Join-Path $Directory "vectors-server.exe"), "0.6.0")
        Assert-Fails { Invoke-UpdateIteration $Directory $true $false } "versions disagree"
        Assert-Equal $script:DownloadUris.Count 0
    }
    Test-Case "verified updates pin downloads and installation to the same tag and keep stopped servers stopped" {
        Reset-UpdateCalls
        $Directory = New-UpdateFixture
        Invoke-UpdateIteration -Directory $Directory -CheckOnly $false -SkipStart $false
        Assert-Equal $script:DownloadUris.Count 2
        foreach ($Uri in $script:DownloadUris) {
            if (-not $Uri.StartsWith("https://github.com/kamilsj/vectors/releases/download/v0.8.0/")) { throw "Release was not pinned: $Uri" }
        }
        Assert-Equal $script:Invocations.Count 1
        Assert-Equal $script:Invocations[0].Tag "v0.8.0"
        Assert-Equal $script:Invocations[0].SkipStart $true
        Assert-Equal ([IO.File]::ReadAllText((Join-Path $Directory "database.sentinel"))) "preserve my data"
    }
    Test-Case "a checksum mismatch never executes the downloaded installer" {
        Reset-UpdateCalls
        $Directory = New-UpdateFixture
        function Receive-UpdateBytes {
            param([string]$Uri, [int]$MaxBytes)
            if ($Uri.EndsWith("/SHA256SUMS")) { return ,[Text.Encoding]::UTF8.GetBytes(("0" * 64) + "  install.ps1`n") }
            return ,$script:FixtureBytes
        }
        Assert-Fails { Invoke-UpdateIteration $Directory $false $false } "SHA-256 verification failed"
        Assert-Equal $script:Invocations.Count 0
        Assert-Equal ([IO.File]::ReadAllText((Join-Path $Directory "vectors.exe"))) "0.7.0"
    }
    Test-Case "download failures leave binaries and data unchanged" {
        Reset-UpdateCalls
        $Directory = New-UpdateFixture
        function Receive-UpdateBytes { param([string]$Uri, [int]$MaxBytes) throw "fixture offline" }
        Assert-Fails { Invoke-UpdateIteration $Directory $false $false } "fixture offline"
        Assert-Equal $script:Invocations.Count 0
        Assert-Equal ([IO.File]::ReadAllText((Join-Path $Directory "vectors.exe"))) "0.7.0"
        Assert-Equal ([IO.File]::ReadAllText((Join-Path $Directory "database.sentinel"))) "preserve my data"
    }
    Test-Case "an installer failure restores both prior binaries" {
        $Directory = New-UpdateFixture
        function Invoke-UpdateInstaller {
            param([string]$ScriptPath, [string]$Tag, [string]$Directory, [bool]$SkipStart)
            [IO.File]::WriteAllText((Join-Path $Directory "vectors.exe"), "partially applied update")
            [IO.File]::WriteAllText((Join-Path $Directory "vectors-server.exe"), "partially applied update")
            throw "fixture install failed"
        }
        Assert-Fails { Invoke-UpdateIteration $Directory $false $false } "previous installed binaries were restored"
        foreach ($Name in @("vectors.exe", "vectors-server.exe")) { Assert-Equal ([IO.File]::ReadAllText((Join-Path $Directory $Name))) "0.7.0" }
        Assert-Equal ([IO.File]::ReadAllText((Join-Path $Directory "database.sentinel"))) "preserve my data"
    }
    Test-Case "the installation lock excludes simultaneous updaters" {
        $Directory = New-UpdateFixture
        $Lock = [IO.File]::Open((Join-Path $Directory ".vectors-update.lock"), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        try { Assert-Fails { Invoke-UpdateIteration $Directory $false $false } "Another updater" }
        finally { $Lock.Dispose() }
    }
    Test-Case "authenticated restarts fail before downloading when the existing token is absent" {
        Reset-UpdateCalls
        $Directory = New-UpdateFixture
        function Get-UpdateManagedServer { return [pscustomobject]@{ Running = $true; Config = [pscustomobject]@{ api_token_required = $true } } }
        Assert-Fails { Invoke-UpdateIteration $Directory $false $false } "existing VECTORS_API_TOKEN"
        Assert-Equal $script:DownloadUris.Count 0
        Invoke-UpdateIteration $Directory $false $true
        Assert-Equal $script:Invocations[0].SkipStart $true
    }
    Test-Case "running managed servers are restarted when allowed" {
        Reset-UpdateCalls
        $Directory = New-UpdateFixture
        function Get-UpdateManagedServer { return [pscustomobject]@{ Running = $true; Config = [pscustomobject]@{ api_token_required = $false } } }
        Invoke-UpdateIteration $Directory $false $false
        Assert-Equal $script:Invocations[0].SkipStart $false
    }
    Test-Case "cleanup cannot target another file" {
        $TempScript = Join-Path ([IO.Path]::GetTempPath()) "vectors-update-fixture.ps1"
        Assert-Equal (Assert-UpdateCleanupPath $TempScript $TempScript) ([IO.Path]::GetFullPath($TempScript))
        Assert-Fails { Assert-UpdateCleanupPath (Join-Path $script:TestRoot "other.ps1") $TempScript } "own temporary script"
    }
    Test-Case "pinned installations and conflicting modes are rejected" {
        $Directory = New-UpdateFixture
        Assert-Fails { Invoke-VectorsUpdate $true $true 60 $Directory $false 0 } "cannot be combined"
        Assert-Fails { Invoke-VectorsUpdate $false $true 59 $Directory $false 0 } "between 60"
        $env:VECTORS_VERSION = "v0.7.0"
        try { Assert-Fails { Invoke-VectorsUpdate $false $false 60 $Directory $false 0 } "pins this installation" }
        finally { $env:VECTORS_VERSION = $null }
    }
    Test-Case "watch retries only after the interval and honors VECTORS_NO_START" {
        $Directory = New-UpdateFixture
        $script:WatchChecks = 0
        $script:WatchSleeps = 0
        $env:OS = "Windows_NT"
        $env:VECTORS_NO_START = "1"
        function Invoke-UpdateIteration {
            param([string]$Directory, [bool]$CheckOnly, [bool]$SkipStart)
            Assert-Equal $SkipStart $true
            $script:WatchChecks += 1
            throw "fixture transient failure"
        }
        function Start-Sleep {
            param([int]$Seconds)
            Assert-Equal $Seconds 60
            $script:WatchSleeps += 1
            throw "fixture watch stopped"
        }
        Assert-Fails { Invoke-VectorsUpdate $false $true 60 $Directory $false 0 } "fixture watch stopped"
        Assert-Equal $script:WatchChecks 1
        Assert-Equal $script:WatchSleeps 1
        $env:VECTORS_NO_START = $null
    }
    Write-Host "$script:Passed network-free Windows updater tests passed."
} finally {
    $env:VECTORS_API_TOKEN = $OriginalToken
    $env:VECTORS_VERSION = $OriginalPin
    $env:VECTORS_NO_START = $OriginalNoStart
    $env:OS = $OriginalOS
    Remove-Item -LiteralPath $script:TestRoot -Recurse -Force -ErrorAction SilentlyContinue
}
