#!/bin/sh
# Embedded in `vectors update`; can also be run directly with POSIX sh.
set -eu
umask 077

repository_url=https://github.com/kamilsj/vectors
install_dir=${VECTORS_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}
check_only=0
watch=0
interval=21600
no_start=0
[ "${VECTORS_NO_START:-0}" != 1 ] || no_start=1

die() { echo "error: $*" >&2; exit 1; }

usage() {
    cat <<'EOF'
Update vectors from the latest stable GitHub release.

Usage: vectors update [options]
       sh update.sh [options]

  --check             Report availability without changing files or restarting
  --watch             Check now, then automatically apply newer releases
  --interval SECONDS  Time between checks in watch mode (60–604800; default 21600)
  --install-dir PATH  Directory containing vectors and vectors-server
  --no-start          Update binaries without restarting a managed server
  -h, --help          Show this help

Updates verify the installer and archive checksums and reuse the installer's
graceful restart and rollback. Stopped servers remain stopped. Watch mode runs
in the foreground until interrupted; use your service manager for persistence.
Keep authentication, provider keys, and runtime settings in that environment.
Pinned VECTORS_VERSION installations must be unpinned before using this updater.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --check) check_only=1; shift ;;
        --watch) watch=1; shift ;;
        --interval)
            [ "$#" -ge 2 ] || die "--interval needs seconds"
            interval=$2; shift 2 ;;
        --install-dir)
            [ "$#" -ge 2 ] && [ -n "$2" ] || die "--install-dir needs a path"
            install_dir=$2; shift 2 ;;
        --no-start) no_start=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown update option '$1'; run 'vectors update --help'" ;;
    esac
done
[ "$check_only" -eq 0 ] || [ "$watch" -eq 0 ] || die "--check and --watch cannot be combined"
case "$interval" in ''|*[!0-9]*) die "--interval must be an integer from 60 to 604800" ;; esac
[ "${#interval}" -le 6 ] && [ "$interval" -ge 60 ] && [ "$interval" -le 604800 ] \
    || die "--interval must be an integer from 60 to 604800"
[ -z "${VECTORS_VERSION:-}" ] || die "VECTORS_VERSION pins a release; unset it before automatic updates"
case "$(uname -s)" in
    Darwin) default_state_dir="$HOME/Library/Application Support/vectors/state" ;;
    Linux) default_state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/vectors" ;;
    *) die "use update.ps1 on Windows; this updater supports Linux and macOS" ;;
esac
[ -d "$install_dir" ] || die "installation directory does not exist: $install_dir"
install_dir=$(cd -P "$install_dir" && pwd -P) || die "cannot resolve installation directory"
[ "$install_dir" != / ] || die "the installation directory cannot be the filesystem root"
state_dir=${VECTORS_STATE_DIR:-$default_state_dir}
command -v curl >/dev/null 2>&1 || die "curl is required"

valid_version() {
    # Stable numeric releases only; bounded components avoid numeric overflow.
    printf '%s\n' "$1" | awk -F . '
        NR != 1 || NF != 3 { exit 1 }
        { for (i = 1; i <= 3; i++)
            if ($i !~ /^(0|[1-9][0-9]*)$/ || length($i) > 10 || $i + 0 > 4294967295) exit 1 }
    '
}

is_newer() {
    awk -v next_version="$1" -v current_version="$2" 'BEGIN {
        split(next_version, n, "."); split(current_version, c, ".")
        for (i = 1; i <= 3; i++) {
            if (n[i] + 0 > c[i] + 0) exit 0
            if (n[i] + 0 < c[i] + 0) exit 1
        }
        exit 1
    }'
}

read_installed_version() {
    [ -f "$install_dir/vectors" ] && [ -x "$install_dir/vectors" ] \
        || die "vectors is missing from $install_dir"
    [ ! -L "$install_dir/vectors" ] && [ ! -L "$install_dir/vectors-server" ] \
        || die "update the owning package manager instead of replacing symbolic links"
    installed_report=$("$install_dir/vectors" --version) || die "cannot read installed vectors version"
    case "$installed_report" in
        'vectors '*) installed_version=${installed_report#vectors } ;;
        *) die "unexpected installed vectors version" ;;
    esac
    valid_version "$installed_version" || die "automatic updates require an installed stable X.Y.Z version"
    server_report=$("$install_dir/vectors-server" --version) || die "cannot read installed vectors-server version"
    [ "$server_report" = "vectors-server $installed_version" ] \
        || die "installed binaries have different versions; repair using the release installer"
}

download() {
    curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location \
        --silent --show-error --connect-timeout 15 --max-time 120 \
        --max-filesize 1048576 --output "$2" "$1" \
        || die "could not download $1"
    [ "$(wc -c < "$2" | tr -d ' ')" -le 1048576 ] || die "release metadata exceeds 1 MiB"
}

update_once() (
    temporary=""
    backup_dir=""
    lock_owned=0
    update_lock="$install_dir/.vectors-update.lock"
    cleanup() {
        [ -z "$temporary" ] || rm -rf "$temporary"
        [ -z "$backup_dir" ] || rm -rf "$backup_dir"
        if [ "$lock_owned" -eq 1 ]; then
            rm -f "$update_lock/owner"
            rmdir "$update_lock" 2>/dev/null || true
        fi
    }
    trap cleanup 0
    trap 'exit 130' HUP INT TERM
    # A directory lock is portable to macOS's POSIX tools. Never guess whether
    # a surviving lock belongs to a different process after PID reuse.
    if [ "$check_only" -eq 0 ]; then
        mkdir "$update_lock" 2>/dev/null \
            || die "another updater owns $update_lock (after a crash, verify no updater is running before removing this directory)"
        lock_owned=1
        printf 'pid=%s\n' "$$" > "$update_lock/owner" || die "cannot record updater lock"
    fi
    read_installed_version
    latest_url=$(curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail \
        --location --silent --show-error --head --connect-timeout 15 --max-time 60 \
        --output /dev/null --write-out '%{url_effective}' "$repository_url/releases/latest") \
        || die "could not resolve the latest release"
    case "$latest_url" in
        "$repository_url/releases/tag/v"*) latest_version=${latest_url#"$repository_url/releases/tag/v"} ;;
        *) die "latest release did not resolve to an official stable tag" ;;
    esac
    valid_version "$latest_version" || die "latest release tag is not a stable X.Y.Z version"
    if ! is_newer "$latest_version" "$installed_version"; then
        echo "vectors $installed_version is up to date (latest stable: $latest_version)."
        return 0
    fi
    echo "Update available: vectors $installed_version → $latest_version."
    [ "$check_only" -eq 0 ] || return 0

    restart=0
    if [ "$no_start" -eq 0 ] && [ -f "$state_dir/server.pid" ]; then
        managed_pid=$(sed -n '1p' "$state_dir/server.pid")
        case "$managed_pid" in
            ''|*[!0-9]*) die "managed server PID is malformed" ;;
        esac
        if kill -0 "$managed_pid" 2>/dev/null; then
            [ -f "$state_dir/server.config" ] && [ ! -L "$state_dir/server.config" ] \
                || die "running server configuration is unknown; use the installer for this legacy upgrade"
            [ "$(sed -n '1p' "$state_dir/server.config")" = vectors-installer-config-v1 ] \
                || die "running server configuration is unsupported"
            [ "$(awk 'END {print NR}' "$state_dir/server.config")" = 9 ] \
                || die "running server configuration is incomplete"
            process_command=$(ps -p "$managed_pid" -o command= 2>/dev/null | awk '{$1=$1; print; exit}')
            case "$process_command" in
                "$install_dir/vectors-server"|"$install_dir/vectors-server "*) ;;
                *) die "managed PID does not belong to this installation; no update was applied" ;;
            esac
            [ -f "$state_dir/server.identity" ] && [ ! -L "$state_dir/server.identity" ] \
                || die "managed process identity is missing; use the installer to repair its state"
            recorded_pid=$(sed -n '1p' "$state_dir/server.identity")
            recorded_start=$(sed -n '2p' "$state_dir/server.identity")
            actual_start=$(ps -p "$managed_pid" -o lstart= 2>/dev/null | awk '{$1=$1; print; exit}')
            [ "$recorded_pid" = "$managed_pid" ] && [ -n "$recorded_start" ] && [ "$recorded_start" = "$actual_start" ] \
                || die "managed process identity changed; no update was applied"
            auth_enabled=$(sed -n '8p' "$state_dir/server.config")
            case "$auth_enabled" in
                0) ;;
                1) [ -n "${VECTORS_API_TOKEN:-}" ] || die "export the existing VECTORS_API_TOKEN before updating this authenticated server" ;;
                *) die "managed authentication configuration is invalid" ;;
            esac
            restart=1
        fi
    fi

    temporary=$(mktemp -d "${TMPDIR:-/tmp}/vectors-update.XXXXXX") || die "cannot create updater staging directory"
    release_url="$repository_url/releases/download/v$latest_version"
    download "$release_url/SHA256SUMS" "$temporary/SHA256SUMS"
    download "$release_url/install.sh" "$temporary/install.sh"
    count=$(awk '$2 == "install.sh" || $2 == "*install.sh" { count++ } END { print count + 0 }' "$temporary/SHA256SUMS")
    [ "$count" = 1 ] || die "SHA256SUMS must contain exactly one install.sh entry"
    expected=$(awk '$2 == "install.sh" || $2 == "*install.sh" { print $1 }' "$temporary/SHA256SUMS")
    [ "${#expected}" -eq 64 ] || die "installer checksum is malformed"
    case "$expected" in *[!0-9a-fA-F]*) die "installer checksum is malformed" ;; esac
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$temporary/install.sh" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$temporary/install.sh" | awk '{print $1}')
    else
        die "sha256sum or shasum is required"
    fi
    expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
    [ "$actual" = "$expected" ] || die "installer checksum does not match SHA256SUMS; nothing was installed"
    echo "Verified installer for v$latest_version."
    backup_dir=$(mktemp -d "$install_dir/.vectors-update-backup.XXXXXX") || die "cannot stage update rollback"
    cp -p "$install_dir/vectors" "$backup_dir/vectors" \
        && cp -p "$install_dir/vectors-server" "$backup_dir/vectors-server" \
        || die "cannot back up the installed binary pair"
    # Do not interrupt a binary replacement or race a still-running installer
    # by rolling it back from a signal handler. Its own transaction must finish.
    trap '' HUP INT TERM
    installed_ok=0
    # The pinned installer validates its archive and owns replacement, process
    # identity checks, graceful shutdown, health verification, and rollback.
    if [ "$restart" -eq 1 ]; then
        VECTORS_NO_START=0 sh "$temporary/install.sh" --version "v$latest_version" \
            --install-dir "$install_dir" --restart --no-open </dev/null \
            && installed_ok=1
    else
        sh "$temporary/install.sh" --version "v$latest_version" \
            --install-dir "$install_dir" --no-start --no-open </dev/null \
            && installed_ok=1
    fi
    if [ "$installed_ok" -eq 1 ]; then
        checked_client=$("$install_dir/vectors" --version 2>/dev/null) || installed_ok=0
        checked_server=$("$install_dir/vectors-server" --version 2>/dev/null) || installed_ok=0
        [ "$checked_client" = "vectors $latest_version" ] \
            && [ "$checked_server" = "vectors-server $latest_version" ] || installed_ok=0
    fi
    if [ "$installed_ok" -ne 1 ]; then
        if cp -p "$backup_dir/vectors" "$backup_dir/vectors.restore" \
            && cp -p "$backup_dir/vectors-server" "$backup_dir/vectors-server.restore" \
            && mv -f "$backup_dir/vectors.restore" "$install_dir/vectors" \
            && mv -f "$backup_dir/vectors-server.restore" "$install_dir/vectors-server"; then
            die "upgrade failed; previous installed binaries restored. Inspect the installer's service recovery message before retrying"
        fi
        retained_backup=$backup_dir
        backup_dir=""
        die "upgrade rollback needs manual recovery; previous binaries retained at $retained_backup"
    fi
    trap 'exit 130' HUP INT TERM
    echo "Updated vectors to $latest_version."
)

if [ "$watch" -eq 0 ]; then
    update_once
else
    echo "Automatic updates enabled for this process; checking every $interval seconds. Press Ctrl+C to stop."
    sleeper=""
    stop_watch() {
        if [ -n "$sleeper" ]; then
            kill "$sleeper" 2>/dev/null || true
            wait "$sleeper" 2>/dev/null || true
        fi
        exit 130
    }
    trap stop_watch HUP INT TERM
    while :; do
        if update_once; then :; else echo "Update check failed; the next attempt is in $interval seconds." >&2; fi
        sleep "$interval" &
        sleeper=$!
        wait "$sleeper" || exit 130
        sleeper=""
    done
fi
