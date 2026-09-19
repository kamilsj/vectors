# Install and manage vectors

The release installers put the SQL shell and HTTP server in your user account,
verify the downloaded archive with SHA-256, start a durable local server, and
open the web console. Administrator access is not required for the default
installation.

## Supported release binaries

| Platform | Processor | Release target | Archive |
| --- | --- | --- | --- |
| Linux | x86-64 | `x86_64-unknown-linux-gnu` | `vectors-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | ARM64 | `aarch64-unknown-linux-gnu` | `vectors-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | Intel | `x86_64-apple-darwin` | `vectors-x86_64-apple-darwin.tar.gz` |
| macOS | Apple silicon | `aarch64-apple-darwin` | `vectors-aarch64-apple-darwin.tar.gz` |
| Windows | x86-64 | `x86_64-pc-windows-msvc` | `vectors-x86_64-pc-windows-msvc.zip` |

Windows on ARM64 does not yet have a native release binary. The PowerShell
installer reports this before downloading anything. Linux release archives are
built on Ubuntu 22.04 for compatibility with contemporary glibc-based
distributions; users of older distributions or musl-based systems should
[build from source](#build-from-source).

## Linux and macOS

Install and launch with one command:

```sh
curl -fsSL https://github.com/kamilsj/vectors/releases/latest/download/install.sh | sh
```

Installer download: [install.sh](https://github.com/kamilsj/vectors/releases/latest/download/install.sh).

The script needs `curl`, `tar`, and either `sha256sum` or `shasum`. These are
already present on macOS and most Linux distributions. It recognizes Intel and
ARM64 processors and selects the matching archive.

For an install-only run that does not start a server or open a browser:

```sh
curl -fsSL https://github.com/kamilsj/vectors/releases/latest/download/install.sh | sh -s -- --no-start --no-open
```

Append options after `sh -s --`. For example, use a different port:

```sh
curl -fsSL https://github.com/kamilsj/vectors/releases/latest/download/install.sh | sh -s -- --bind 127.0.0.1:8081
```

## Windows

Run this in a 64-bit PowerShell session:

```powershell
& ([scriptblock]::Create((irm https://github.com/kamilsj/vectors/releases/latest/download/install.ps1)))
```

Installer download: [install.ps1](https://github.com/kamilsj/vectors/releases/latest/download/install.ps1).

The installer supports Windows x86-64, including a 64-bit PowerShell process
started from a 32-bit parent. It refuses unsupported processors and 32-bit-only
Windows before downloading an archive.

The script block preserves PowerShell's installer options, including `-WhatIf`.
Append switches to the command for an install-only run:

```powershell
& ([scriptblock]::Create((irm https://github.com/kamilsj/vectors/releases/latest/download/install.ps1))) -NoStart -NoOpen
```

Use `-BindAddress 127.0.0.1:8081` instead to change the port, or `-WhatIf` to
download the script and preview its plan without installing anything. A
locally saved script can run `-PrintTarget` or `-WhatIf` without networking.

## Review before running

The commands above download and execute the release installer directly. To
inspect it first, save a local copy instead.

Linux or macOS:

```sh
installer="$(mktemp "${TMPDIR:-/tmp}/vectors-install.XXXXXX")"
curl --proto '=https' --tlsv1.2 -fL \
  https://github.com/kamilsj/vectors/releases/latest/download/install.sh \
  -o "$installer"
less "$installer"                    # press q to close
```

After review, run it with any desired options, then remove the temporary file:

```sh
sh "$installer"
rm -f "$installer"
```

Windows PowerShell:

```powershell
$Installer = Join-Path ([IO.Path]::GetTempPath()) "vectors-install-$([guid]::NewGuid()).ps1"
Invoke-WebRequest `
    -Uri 'https://github.com/kamilsj/vectors/releases/latest/download/install.ps1' `
    -OutFile $Installer
Get-Content -LiteralPath $Installer
```

After review, run it with any desired switches, then remove the temporary file:

```powershell
Unblock-File -LiteralPath $Installer
& $Installer
Remove-Item -LiteralPath $Installer -Force
```

## What the installer changes

Both installers:

1. detect the operating system and processor;
2. resolve the requested release without requiring a package manager;
3. download the archive and `SHA256SUMS` over HTTPS;
4. reject a missing or mismatched archive checksum;
5. verify that both binaries can report their version;
6. stage each executable on the destination filesystem before replacing it, so
   an installed binary is never partial;
7. add that directory to the current user's future `PATH` when necessary;
8. unless disabled, start `vectors-server` with durable storage and wait for
   `/healthz` before reporting success;
9. open the local web console when a desktop browser is available.

The installer starts a user-owned background process, not an operating-system
service. For unattended hosts, install with `--no-start` or `-NoStart`, then
manage `vectors-server --data-dir PATH` with your existing service manager.

## Installer options

| Purpose | Linux/macOS | Windows PowerShell |
| --- | --- | --- |
| Show help | `--help` | `Get-Help .\install.ps1 -Full` |
| Inspect the resolved target | `--print-target` | `-PrintTarget` |
| Preview changes | `--dry-run` | `-WhatIf` |
| Install a fixed release | `--version v0.7.0` | `-Version v0.7.0` |
| Choose the binary directory | `--install-dir PATH` | `-InstallDir PATH` |
| Choose the server address | `--bind 127.0.0.1:8081` | `-BindAddress 127.0.0.1:8081` |
| Do not start the server | `--no-start` | `-NoStart` |
| Do not open a browser | `--no-open` | `-NoOpen` |
| Restart an installer-managed server | `--restart` | automatic unless `-NoStart` is set |

The matching environment variables are useful for automated installations:

| Variable | Purpose |
| --- | --- |
| `VECTORS_VERSION` | Release tag, with or without the leading `v`. |
| `VECTORS_INSTALL_DIR` | Destination for both executables. |
| `VECTORS_BIND` | Server listen address; the default is `127.0.0.1:8080`. |
| `VECTORS_NO_START=1` | Install without starting the server. |
| `VECTORS_NO_OPEN=1` | Do not open the web console. |
| `VECTORS_DATA_DIR` | Durable database directory used by the server. |
| `VECTORS_STATE_DIR` | PID, managed configuration, and log directory used by the installer. |
| `VECTORS_LOG_FILE` | POSIX server log path; the default is platform-specific. |
| `VECTORS_SNAPSHOT` | Use legacy snapshot-only storage instead of the durable directory. |
| `VECTORS_AUTOSAVE_INTERVAL_SECS` | Checkpoint interval for legacy snapshot mode. |
| `VECTORS_SHUTDOWN_FILE` | Absolute private request-file path for cooperative server shutdown; installers manage this automatically. |

Command-line settings take precedence over their environment equivalents.
`VECTORS_SNAPSHOT` and `VECTORS_DATA_DIR` are mutually exclusive server modes.

Examples:

```sh
# Pin a release and leave the server stopped.
VECTORS_INSTALL_DIR="$HOME/bin" \
  sh ./install.sh --version v0.7.0 --no-start --no-open

# Start on a different local port with an explicit durable directory.
VECTORS_DATA_DIR="$HOME/vectors-data" \
  sh ./install.sh --bind 127.0.0.1:8081 --no-open
```

```powershell
# Pin a release and leave the server stopped.
& .\install.ps1 -Version v0.7.0 -InstallDir "$HOME\bin" -NoStart -NoOpen

# Start on another local port with an explicit durable directory.
$env:VECTORS_DATA_DIR = "$HOME\vectors-data"
& .\install.ps1 -BindAddress '127.0.0.1:8081' -NoOpen
```

## Default locations

| Item | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Executables | `~/.local/bin` | `~/.local/bin` | `%LOCALAPPDATA%\Programs\vectors` |
| Durable data | `${XDG_DATA_HOME:-$HOME/.local/share}/vectors` | `~/Library/Application Support/vectors/data` | `%LOCALAPPDATA%\vectors\data` |
| PID and state | `${XDG_STATE_HOME:-$HOME/.local/state}/vectors` | `~/Library/Application Support/vectors/state` | `%LOCALAPPDATA%\vectors` |
| Server log | state directory, `server.log` | `~/Library/Logs/vectors/server.log` | state directory, `server.stdout.log` and `server.stderr.log` |

Set `VECTORS_DATA_DIR` and `VECTORS_STATE_DIR` when a predictable location
matters to deployment tooling. `--dry-run` and `-PrintTarget` show the resolved
locations without changing the machine.

## Verify the installation

Open a new terminal after the first install, then run:

```sh
vectors --version
vectors-server --version
curl -fsS http://127.0.0.1:8080/healthz
```

PowerShell can check health without `curl`:

```powershell
vectors --version
vectors-server --version
Invoke-RestMethod http://127.0.0.1:8080/healthz
```

A healthy server returns JSON with `"status":"ok"`. Open
[http://127.0.0.1:8080](http://127.0.0.1:8080) for the web console, or run
`vectors` and enter `.tutorial` for the interactive SQL lesson.

## Automatic updates

Builds with the updater provide the same commands on Linux, macOS, and Windows:

```sh
vectors update --check
vectors update
vectors update --watch
```

`--check` reports the installed and latest stable versions without installing
or restarting anything. `vectors update` installs only a strictly newer stable
release. `--watch` checks immediately, then every six hours. Use
`--watch --interval 86400` for daily checks; supported intervals are 60 seconds
through seven days. Watch mode is opt-in and must remain running. It does not
register a login task or operating-system service; run it through your service
manager if it should survive logout or reboot. Stop it to disable automatic
updates. Restart a long-running watcher after upgrades to load the newest
updater logic.

On Windows, an update opens a separate PowerShell window and the calling
`vectors.exe` exits so Windows can replace it. The message in the calling
terminal means the updater started; the separate window reports the final
result and remains open for inspection. `--check` runs in the current terminal.
No machine-wide execution-policy setting is changed.

The updater targets the directory containing the invoked `vectors` binary.
Use `--install-dir PATH` or `VECTORS_INSTALL_DIR` to select another installation.
Both installed binaries must report the same stable `X.Y.Z` version. It will
not downgrade, reinstall the same version, follow prerelease tags, replace
package-manager symlinks, or ignore a `VECTORS_VERSION` pin. Unset that variable
to opt a pinned installation into latest-stable updates. Older binaries that
do not recognize `update` need one upgrade using the installer below first.

Each update resolves one exact release tag, verifies its installer against
that release's `SHA256SUMS`, then invokes the pinned installer. The installer
separately verifies its binary archive. The updater keeps a temporary copy of
the installed executable pair and restores it if installation or final version
verification fails. Existing installer recovery restores a managed service
that cannot start. Check the recovery message if a filesystem error prevents
rollback; retained backups are reported rather than silently discarded.
Database backups remain the operator's responsibility, especially across
pre-1.0 releases with potentially changing SQL or storage behavior.

A running installer-managed server uses its recorded bind and storage settings
and restarts cooperatively. A stopped server remains stopped. Pass `--no-start`
to replace binaries without restarting a managed server. External service
managers should use this mode and handle their own restart and health policy.
The updater never guesses how to stop an unrecognized process.

Provide the existing `VECTORS_API_TOKEN` in the updater environment before an
authenticated managed restart. Also supply `OPENAI_API_KEY`, `VOYAGE_API_KEY`,
and any advanced runtime settings needed by the restarted server. These secrets
are inherited in memory, not written into updater state. Provider keys entered
only in the web interface disappear when the server restarts; use environment
keys for unattended operation. Custom installations must also provide their
existing `VECTORS_STATE_DIR`. Non-secret embedding settings in the durable data
directory survive the update.

Concurrent updater processes are serialized per installation. On POSIX, a
forced kill can leave `.vectors-update.lock` in the binary directory. If the
updater reports this, first verify no updater or installer is running, then
remove only that lock directory. Normal completion and ordinary failures clean
it up automatically. Watch mode reports failed checks and waits the configured
interval before trying again, rather than repeatedly restarting a failed build.

For source-based validation without GitHub downloads:

```sh
python3 tests/test_updater.py
```

Windows uses `powershell -NoProfile -File tests/update_windows.ps1`. These
fixtures exercise version selection, checksum rejection, failure cleanup, and
upgrade decisions without touching an existing installation.

## Upgrade or reinstall

The installer is idempotent: run the current installer again to replace the
binaries. Durable data is stored outside the binary directory and is not
deleted during an upgrade.

For a predictable POSIX upgrade:

1. record `vectors --version` and back up the durable directory;
2. run the installer with `--restart` and, when pinning, `--version TAG`;
3. verify the binary version and `/healthz` response.

Without `--restart`, an already-running POSIX server keeps serving the previous
version until it is restarted. The Windows installer detects its own managed
runtime, updates it safely, and starts the new version unless `-NoStart` is set.
Both installers remember the managed bind address and storage mode in their
private state directory, reuse that configuration on the next upgrade, and ask
the server to shut down cooperatively so its final checkpoint or snapshot can
finish. If a new runtime cannot become healthy, the prior binary and its prior
configuration are restored.
The installer records only whether authentication was enabled, not the secret.
Set the existing `VECTORS_API_TOKEN` again when upgrading an authenticated
managed server; the installer refuses to restart it without the token rather
than silently removing authentication.
Other advanced `VECTORS_*` capacity and compute settings are not copied into
installer state; provide them on each managed restart or run the server through
your service manager.
On the first Windows upgrade from an older installer that has no managed
configuration record, the new installer leaves a running server untouched.
Run once with `-NoStart`, stop the legacy process using the method that launched
it after protecting its data, then rerun with its existing `VECTORS_BIND` and
storage setting. Later upgrades use cooperative shutdown automatically.
Because the project is pre-1.0, read the release notes before downgrading a data
directory to an older version.

## Uninstall

First stop the installer-managed server. The POSIX installer prints a
`Stop gracefully` command after startup; it creates a private shutdown request
and waits while the server completes its final checkpoint. On Windows, use the
cooperative sequence below. Neither example sends a blind signal to a PID from
a possibly stale file.

Linux and macOS default:

```sh
case "$(uname -s)" in
  Darwin) state_dir="$HOME/Library/Application Support/vectors/state" ;;
  *) state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/vectors" ;;
esac

if [ -f "$state_dir/server.pid" ]; then
  pid="$(head -n 1 "$state_dir/server.pid")"
  if kill -0 "$pid" 2>/dev/null; then
    server_command="$(ps -p "$pid" -o comm= | awk '{$1=$1; print; exit}')"
    case "$server_command" in
      vectors-server|*/vectors-server) ;;
      *) echo "Refusing to stop unrelated PID $pid" >&2; exit 1 ;;
    esac
    request="$(mktemp "$state_dir/.shutdown-request.XXXXXX")"
    chmod 600 "$request"
    mv -f "$request" "$state_dir/shutdown.request"
    while kill -0 "$pid" 2>/dev/null; do sleep 1; done
  fi
fi

rm -f "$HOME/.local/bin/vectors" "$HOME/.local/bin/vectors-server" \
  "$HOME/.local/bin/.vectors.previous" \
  "$HOME/.local/bin/.vectors-server.previous"
```

Windows default (the executable-path check deliberately refuses an unrelated
process):

```powershell
$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\vectors'
$StateDir = Join-Path $env:LOCALAPPDATA 'vectors'
$RuntimeServer = Join-Path $StateDir 'vectors-server.exe'
$PidFile = Join-Path $StateDir 'server.pid'
$ShutdownFile = Join-Path $StateDir 'shutdown.request'
$ConfigFile = Join-Path $StateDir 'server.config.json'

if (Test-Path -LiteralPath $PidFile) {
    $PidLines = @(Get-Content -LiteralPath $PidFile)
    $ServerProcessId = $PidLines | Select-Object -First 1
    $ServerProcess = Get-Process -Id $ServerProcessId -ErrorAction SilentlyContinue
    if ($ServerProcess) {
        if ([string]::IsNullOrWhiteSpace($ServerProcess.Path) -or
            [IO.Path]::GetFullPath($ServerProcess.Path) -ine
            [IO.Path]::GetFullPath($RuntimeServer)) {
            throw "Refusing to stop PID $ServerProcessId because it is not the managed vectors server."
        }
        if ($PidLines.Count -ge 2 -and
            $ServerProcess.StartTime.ToUniversalTime().Ticks -ne [long]$PidLines[1]) {
            throw "Refusing to stop PID $ServerProcessId because its identity changed."
        }
        [IO.File]::WriteAllBytes($ShutdownFile, [byte[]]@())
        if (-not $ServerProcess.WaitForExit(60000)) {
            throw "The server did not stop cooperatively; no files were removed."
        }
    }
}

$OwnedFiles = @(
    (Join-Path $InstallDir 'vectors.exe'),
    (Join-Path $InstallDir 'vectors-server.exe'),
    $RuntimeServer,
    $PidFile,
    $ShutdownFile,
    $ConfigFile
)
Remove-Item -LiteralPath $OwnedFiles -Force -ErrorAction SilentlyContinue
```

Removing the executables intentionally leaves database files and logs intact.
After confirming that no server is using them and that no backup is needed,
remove the data and state paths shown by `--dry-run` or `-PrintTarget`.
You may also remove the installer-added PATH entry. The POSIX installer writes
to `~/.zprofile` for zsh on macOS, `~/.zshrc` for zsh on Linux, an existing
`~/.bash_profile` for bash on macOS, `~/.bashrc` for bash on Linux, a Fish
`conf.d/vectors.fish` file for Fish, or `~/.profile` as the fallback. Windows
adds the vectors directory to the user PATH.

## Verify a release archive manually

Every release includes `SHA256SUMS`, and the release workflow publishes GitHub
build-provenance attestations for archives, installers, and the checksum file.
The installers perform the checksum verification automatically. For a manual
download on Linux:

```sh
asset=vectors-x86_64-unknown-linux-gnu.tar.gz
curl -fLO "https://github.com/kamilsj/vectors/releases/latest/download/$asset"
curl -fLO https://github.com/kamilsj/vectors/releases/latest/download/SHA256SUMS
grep " $asset$" SHA256SUMS | sha256sum -c -
```

With a recent GitHub CLI, provenance can also be checked before extraction:

```sh
gh attestation verify "$asset" --repo kamilsj/vectors
```

## Troubleshooting

### `vectors: command not found`

Open a new terminal so the updated user PATH is loaded. On Linux or macOS, you
can also run `export PATH="$HOME/.local/bin:$PATH"` in the current shell. A
custom `VECTORS_INSTALL_DIR` must be added to PATH manually.

### The installer reports an unsupported platform or processor

On Linux or macOS, run `uname -s` and `uname -m`; supported pairs are listed at
the top of this guide. On Windows, use a 64-bit PowerShell session on x86-64.
Windows ARM64, 32-bit Windows, musl Linux, and other operating systems currently
require a source build.

The message `this installer supports Linux; use install.ps1 on Windows` comes
from the old v0.6.0 installer, which has no macOS binaries. macOS and Linux ARM64
release support starts with v0.7.0. Use the latest-release command above and
remove any `VECTORS_VERSION=v0.6.0` override or older `--version` argument.
Changing only the script URL cannot add missing binaries to an older release.

### PowerShell refuses to run the script

Review the downloaded file, then run `Unblock-File -LiteralPath $Installer` as
shown above. Do not lower the machine-wide execution policy. If an organization
enforces a stricter policy, ask its administrator to approve the script or
build the project from source.

### macOS blocks an executable

The archive checksum is verified before installation. If Gatekeeper still
quarantines a binary, confirm it came from the expected GitHub release, then
remove quarantine only from the installed vectors binaries:

```sh
xattr -d com.apple.quarantine "$HOME/.local/bin/vectors" 2>/dev/null || true
xattr -d com.apple.quarantine "$HOME/.local/bin/vectors-server" 2>/dev/null || true
```

Do not disable Gatekeeper globally.

### Port 8080 is already in use

Choose another loopback port:

```sh
sh ./install.sh --bind 127.0.0.1:8081
```

```powershell
& .\install.ps1 -BindAddress '127.0.0.1:8081'
```

### The server did not become ready

The installer prints the log and PID locations. Inspect the error log first,
then check that the chosen port is free and the data directory is writable.
Only one vectors process may own a durable directory at a time.

### Download or checksum verification fails

Retry after checking the GitHub status page and your proxy or TLS inspection
configuration. Do not bypass checksum verification. A fixed `--version` or
`-Version` avoids moving `latest` references in reproducible deployments.

## Build from source

Install Rust 1.89 or newer, then build the two binaries:

```sh
git clone https://github.com/kamilsj/vectors.git
cd vectors
cargo build --release --locked
```

The executables are written to `target/release`. Add `--features gpu` to match
the optional GPU support included in official release archives.
