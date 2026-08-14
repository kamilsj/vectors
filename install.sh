#!/bin/sh
set -eu

umask 077

repository="kamilsj/vectors"
default_install_dir="${HOME:?HOME is not set}/.local/bin"
install_dir="${VECTORS_INSTALL_DIR:-$default_install_dir}"
version="${VECTORS_VERSION:-}"
bind="${VECTORS_BIND:-127.0.0.1:8080}"
bind_explicit=0
storage_explicit=0
autosave_explicit=0
log_explicit=0
http_timeout_explicit=0
api_token_value="${VECTORS_API_TOKEN:-}"
api_token_enabled=0
[ "${VECTORS_BIND+x}" = "x" ] && bind_explicit=1
if [ -n "${VECTORS_DATA_DIR:-}" ] || [ -n "${VECTORS_SNAPSHOT:-}" ]; then storage_explicit=1; fi
[ "${VECTORS_AUTOSAVE_INTERVAL_SECS+x}" = "x" ] && autosave_explicit=1
[ -n "${VECTORS_LOG_FILE:-}" ] && log_explicit=1
[ "${VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS+x}" = "x" ] && http_timeout_explicit=1
[ -n "$api_token_value" ] && api_token_enabled=1
start_server=1
open_console=1
restart_server=0
print_target=0
dry_run=0

temporary=""
staged_vectors=""
staged_server=""
backup_vectors=""
backup_server=""
persistent_stage_vectors=""
persistent_stage_server=""
critical_section=0

cleanup() {
    [ -z "$staged_vectors" ] || rm -f "$staged_vectors"
    [ -z "$staged_server" ] || rm -f "$staged_server"
    [ -z "$backup_vectors" ] || rm -f "$backup_vectors"
    [ -z "$backup_server" ] || rm -f "$backup_server"
    [ -z "$persistent_stage_vectors" ] || rm -f "$persistent_stage_vectors"
    [ -z "$persistent_stage_server" ] || rm -f "$persistent_stage_server"
    [ -z "$temporary" ] || rm -rf "$temporary"
}

die() {
    echo "error: $*" >&2
    exit 1
}

begin_critical_section() {
    critical_section=1
    trap '' HUP INT TERM
}

end_critical_section() {
    critical_section=0
    trap 'exit 130' HUP INT TERM
}

usage() {
    cat <<'EOF'
Install vectors for Linux or macOS.

Usage: install.sh [options]

Options:
  --version TAG       Install a release tag such as v0.6.0
  --install-dir PATH  Install binaries here (default: ~/.local/bin)
  --bind ADDRESS      Server address (default: 127.0.0.1:8080)
  --restart           Gracefully restart a managed server after an upgrade
  --no-start          Install without starting vectors-server
  --no-open           Do not open the web console
  --print-target      Print the detected Rust target without networking
  --dry-run           Show the installation plan without changing anything
  -h, --help          Show this help

Supported release targets:
  Linux:  x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
  macOS:  x86_64-apple-darwin, aarch64-apple-darwin

Environment equivalents: VECTORS_VERSION, VECTORS_INSTALL_DIR,
VECTORS_BIND, VECTORS_NO_START=1, and VECTORS_NO_OPEN=1.
The server uses VECTORS_DATA_DIR for durable storage and VECTORS_STATE_DIR
for its PID. VECTORS_LOG_FILE overrides the server log path. Set
VECTORS_SNAPSHOT to retain legacy snapshot mode. VECTORS_API_TOKEN is passed
to the server but its value is never written to installer state. The graceful
stop wait follows VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS (default: 30).
EOF
}

resolve_target() {
    resolve_os=$1
    resolve_arch=$2
    case "$resolve_os:$resolve_arch" in
        Linux:x86_64|Linux:amd64) echo "x86_64-unknown-linux-gnu" ;;
        Linux:aarch64|Linux:arm64) echo "aarch64-unknown-linux-gnu" ;;
        Darwin:x86_64|Darwin:amd64) echo "x86_64-apple-darwin" ;;
        Darwin:arm64|Darwin:aarch64) echo "aarch64-apple-darwin" ;;
        Linux:*)
            echo "error: vectors does not publish a Linux release for architecture '$resolve_arch'" >&2
            return 1
            ;;
        Darwin:*)
            echo "error: vectors does not publish a macOS release for architecture '$resolve_arch'" >&2
            return 1
            ;;
        *)
            echo "error: unsupported operating system '$resolve_os'; use install.ps1 on Windows" >&2
            return 1
            ;;
    esac
}

parse_bind_address() {
    case "$bind" in
        \[*\]:*)
            bind_host=${bind#\[}
            bind_host=${bind_host%%\]*}
            bind_suffix=${bind#*\]}
            case "$bind_suffix" in :*) port=${bind_suffix#:} ;; *) die "--bind must use [IPv6]:PORT" ;; esac
            [ "$bind" = "[$bind_host]:$port" ] || die "--bind must use [IPv6]:PORT"
            ;;
        *:*)
            bind_host=${bind%:*}
            port=${bind##*:}
            case "$bind_host" in *:*) die "IPv6 addresses must use [IPv6]:PORT" ;; esac
            ;;
        *) die "--bind must include a host and port, for example 127.0.0.1:8080" ;;
    esac
    [ -n "$bind_host" ] || die "--bind host cannot be empty"
    case "$port" in ''|*[!0-9]*) die "--bind port must be an integer from 1 to 65535" ;; esac
    [ "$port" -ge 1 ] && [ "$port" -le 65535 ] \
        || die "--bind port must be an integer from 1 to 65535"
    probe_host=$bind_host
    case "$probe_host" in 0.0.0.0) probe_host=127.0.0.1 ;; ::) probe_host=::1 ;; esac
    case "$probe_host" in
        *:*) console_url="http://[$probe_host]:$port" ;;
        *) console_url="http://$probe_host:$port" ;;
    esac
}

download() {
    curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
        --retry 3 --connect-timeout 15 --max-time 600 --output "$2" "$1"
}

process_is_vectors_server() {
    process_command=$(ps -p "$1" -o command= 2>/dev/null | awk '{$1=$1; print; exit}')
    case "$process_command" in
        "$install_dir/vectors-server"|"$install_dir/vectors-server "*) ;;
        *) return 1 ;;
    esac
    if [ -f "$identity_file" ]; then
        identity_pid=$(sed -n '1p' "$identity_file")
        identity_started=$(sed -n '2p' "$identity_file")
        process_started=$(ps -p "$1" -o lstart= 2>/dev/null | awk '{$1=$1; print; exit}')
        [ "$identity_pid" = "$1" ] && [ -n "$identity_started" ] \
            && [ "$identity_started" = "$process_started" ] || return 1
    fi
    return 0
}

port_is_occupied() {
    if command -v nc >/dev/null 2>&1; then
        nc -z -w 1 "$probe_host" "$port" >/dev/null 2>&1 && return 0
    fi
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 && return 0
    fi
    curl --noproxy '*' --silent --output /dev/null \
        --connect-timeout 1 --max-time 2 "$console_url/" 2>/dev/null
}

show_log_tail() {
    if [ -s "$log_file" ] && command -v tail >/dev/null 2>&1; then
        echo "Last 20 server log lines ($log_file):" >&2
        tail -n 20 "$log_file" >&2 || true
    fi
}

write_server_config() {
    config_running_version=$1
    config_newline='
'
    for config_value in "$bind" "$storage_path" "$autosave" "$log_file" "$config_running_version" "$api_token_enabled" "$http_shutdown_timeout"; do
        case "$config_value" in *"$config_newline"*) return 1 ;; esac
    done
    config_temporary="$config_file.new.$$"
    rm -f "$config_temporary" || return 1
    printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
        "vectors-installer-config-v1" "$bind" "$storage_mode" "$storage_path" \
        "$autosave" "$log_file" "$config_running_version" "$api_token_enabled" "$http_shutdown_timeout" > "$config_temporary" \
        || return 1
    chmod 600 "$config_temporary" || return 1
    mv -f "$config_temporary" "$config_file" || return 1
    rm -f "$running_version_file" || true
    return 0
}

write_running_version_marker() {
    marker_version=$1
    case "$marker_version" in ''|*[!0-9A-Za-z.+-]*) return 1 ;; esac
    mkdir -p "$state_dir" || return 1
    chmod 700 "$state_dir" || return 1
    marker_temporary="$running_version_file.new.$$"
    rm -f "$marker_temporary" || return 1
    printf '%s\n' "$marker_version" > "$marker_temporary" || return 1
    chmod 600 "$marker_temporary" || return 1
    mv -f "$marker_temporary" "$running_version_file" || return 1
    return 0
}

apply_rollback_config() {
    bind=$rollback_bind
    storage_mode=$rollback_storage_mode
    storage_path=$rollback_storage_path
    autosave=$rollback_autosave
    log_file=$rollback_log_file
    api_token_enabled=$rollback_api_token_enabled
    http_shutdown_timeout=$rollback_http_shutdown_timeout
    if [ "$storage_mode" = "snapshot" ]; then
        snapshot=$storage_path
    else
        snapshot=""
        data_dir=$storage_path
    fi
    parse_bind_address
}

restore_requested_config() {
    bind=$requested_bind
    storage_mode=$requested_storage_mode
    storage_path=$requested_storage_path
    autosave=$requested_autosave
    log_file=$requested_log_file
    api_token_enabled=$requested_api_token_enabled
    http_shutdown_timeout=$requested_http_shutdown_timeout
    if [ "$storage_mode" = "snapshot" ]; then
        snapshot=$storage_path
    else
        snapshot=""
        data_dir=$storage_path
    fi
    parse_bind_address
}

ensure_runtime_directories() {
    runtime_log_dir=${log_file%/*}
    [ "$runtime_log_dir" = "$log_file" ] && runtime_log_dir="."
    mkdir -p "$state_dir" "$runtime_log_dir" || return 1
    chmod 700 "$state_dir" || return 1
    touch "$log_file" || return 1
    [ -w "$state_dir" ] || return 1
    if [ "$storage_mode" = "snapshot" ]; then
        runtime_storage_dir=${storage_path%/*}
        [ "$runtime_storage_dir" = "$storage_path" ] && runtime_storage_dir="."
        mkdir -p "$runtime_storage_dir" || return 1
    else
        mkdir -p "$storage_path" || return 1
    fi
    return 0
}

quote_for_shell() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

show_installer_command() {
    command_bind=$1
    command_restart=$2
    quoted_install_dir=$(quote_for_shell "$install_dir")
    quoted_bind=$(quote_for_shell "$command_bind")
    command_options="--install-dir $quoted_install_dir --bind $quoted_bind --no-open"
    if [ -n "${client_version:-}" ]; then
        quoted_version=$(quote_for_shell "v$client_version")
        command_options="--version $quoted_version $command_options"
    elif [ -n "$version" ]; then
        quoted_version=$(quote_for_shell "$version")
        command_options="--version $quoted_version $command_options"
    fi
    if [ "$command_restart" -eq 1 ]; then command_options="$command_options --restart"; fi
    if [ "$api_token_enabled" -eq 1 ]; then
        echo "  # Keep VECTORS_API_TOKEN exported; the installer never stores or prints its value." >&2
    fi
    quoted_release_url=$(quote_for_shell "$release_url/install.sh")
    quoted_state_dir=$(quote_for_shell "$state_dir")
    quoted_log_file=$(quote_for_shell "$log_file")
    quoted_http_timeout=$(quote_for_shell "$http_shutdown_timeout")
    if [ -n "$snapshot" ]; then
        quoted_snapshot=$(quote_for_shell "$snapshot")
        quoted_autosave=$(quote_for_shell "$autosave")
        echo "  curl --proto '=https' --tlsv1.2 -fsSL $quoted_release_url | VECTORS_NO_START=0 VECTORS_STATE_DIR=$quoted_state_dir VECTORS_LOG_FILE=$quoted_log_file VECTORS_SNAPSHOT=$quoted_snapshot VECTORS_AUTOSAVE_INTERVAL_SECS=$quoted_autosave VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS=$quoted_http_timeout sh -s -- $command_options" >&2
    else
        quoted_data_dir=$(quote_for_shell "$data_dir")
        echo "  curl --proto '=https' --tlsv1.2 -fsSL $quoted_release_url | VECTORS_NO_START=0 VECTORS_STATE_DIR=$quoted_state_dir VECTORS_LOG_FILE=$quoted_log_file VECTORS_DATA_DIR=$quoted_data_dir VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS=$quoted_http_timeout sh -s -- $command_options" >&2
    fi
}

show_direct_server_command() {
    direct_bind=$1
    if [ "$api_token_enabled" -eq 1 ]; then
        echo "  # Requires VECTORS_API_TOKEN to remain exported." >&2
    fi
    quoted_direct_bind=$(quote_for_shell "$direct_bind")
    quoted_shutdown_file=$(quote_for_shell "$shutdown_file")
    quoted_server=$(quote_for_shell "$install_dir/vectors-server")
    quoted_log_file=$(quote_for_shell "$log_file")
    quoted_http_timeout=$(quote_for_shell "$http_shutdown_timeout")
    if [ -n "$snapshot" ]; then
        quoted_snapshot=$(quote_for_shell "$snapshot")
        quoted_autosave=$(quote_for_shell "$autosave")
        echo "  VECTORS_BIND=$quoted_direct_bind VECTORS_SNAPSHOT=$quoted_snapshot VECTORS_AUTOSAVE_INTERVAL_SECS=$quoted_autosave VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS=$quoted_http_timeout VECTORS_SHUTDOWN_FILE=$quoted_shutdown_file $quoted_server >>$quoted_log_file 2>&1" >&2
    else
        quoted_data_dir=$(quote_for_shell "$data_dir")
        echo "  VECTORS_BIND=$quoted_direct_bind VECTORS_DATA_DIR=$quoted_data_dir VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS=$quoted_http_timeout VECTORS_SHUTDOWN_FILE=$quoted_shutdown_file $quoted_server >>$quoted_log_file 2>&1" >&2
    fi
}

show_port_retry() {
    if [ "$port" -lt 65535 ]; then retry_port=$((port + 1)); else retry_port=8080; fi
    case "$bind_host" in
        *:*) retry_bind="[$bind_host]:$retry_port" ;;
        *) retry_bind="$bind_host:$retry_port" ;;
    esac
    echo "Try another port:" >&2
    show_installer_command "$retry_bind" "$restarting_existing"
    echo "Or start the installed server directly:" >&2
    show_direct_server_command "$retry_bind"
}

if [ "${VECTORS_NO_START:-0}" = "1" ]; then start_server=0; fi
if [ "${VECTORS_NO_OPEN:-0}" = "1" ]; then open_console=0; fi

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || { echo "error: --version needs a value" >&2; exit 2; }
            version=$2; shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || { echo "error: --install-dir needs a value" >&2; exit 2; }
            install_dir=$2; shift 2
            ;;
        --bind)
            [ "$#" -ge 2 ] || { echo "error: --bind needs a value" >&2; exit 2; }
            bind=$2; bind_explicit=1; shift 2
            ;;
        --restart) restart_server=1; shift ;;
        --no-start) start_server=0; shift ;;
        --no-open) open_console=0; shift ;;
        --print-target) print_target=1; shift ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *)
            echo "error: unknown option '$1'" >&2
            usage >&2
            exit 2
            ;;
    esac
done

[ -n "$install_dir" ] || die "the installation directory cannot be empty"
if [ "$install_dir" = "$default_install_dir" ]; then uses_default_install_dir=1; else uses_default_install_dir=0; fi
case "$install_dir" in
    /*) ;;
    *) install_dir="$(pwd -P)/$install_dir" ;;
esac
[ "$install_dir" != "/" ] || die "refusing to install binaries in the filesystem root"
if [ "$restart_server" -eq 1 ] && [ "$start_server" -eq 0 ]; then
    die "--restart cannot be combined with --no-start or VECTORS_NO_START=1"
fi
if [ -n "$version" ]; then
    case "$version" in v*) ;; *) version="v$version" ;; esac
    case "$version" in *[!A-Za-z0-9._-]*) die "invalid release tag '$version'" ;; esac
    case "$version" in v[0-9]*) ;; *) die "release tags must start with a numeric version, for example v0.6.0" ;; esac
fi

host_os=$(uname -s 2>/dev/null) || die "could not detect the operating system"
host_arch=$(uname -m 2>/dev/null) || die "could not detect the CPU architecture"
if [ "$host_os" = "Darwin" ] && [ "$host_arch" = "x86_64" ] \
    && command -v sysctl >/dev/null 2>&1 \
    && [ "$(sysctl -in sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
    host_arch="arm64"
fi
target=$(resolve_target "$host_os" "$host_arch") || exit 1
asset="vectors-$target.tar.gz"

if [ -n "$version" ]; then
    release_url="https://github.com/$repository/releases/download/$version"
else
    release_url="https://github.com/$repository/releases/latest/download"
fi
asset_url="$release_url/$asset"

if [ "$print_target" -eq 1 ]; then echo "$target"; exit 0; fi

if [ "$host_os" = "Darwin" ]; then
    default_data_dir="$HOME/Library/Application Support/vectors/data"
    default_state_dir="$HOME/Library/Application Support/vectors/state"
    default_log_file="$HOME/Library/Logs/vectors/server.log"
    browser_opener="open"
else
    default_data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/vectors"
    default_state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/vectors"
    default_log_file="$default_state_dir/server.log"
    browser_opener="xdg-open"
fi
state_dir="${VECTORS_STATE_DIR:-$default_state_dir}"
case "$state_dir" in /*) ;; *) state_dir="$(pwd -P)/$state_dir" ;; esac
[ "$state_dir" != "/" ] || die "refusing to use the filesystem root as VECTORS_STATE_DIR"
pid_file="$state_dir/server.pid"
identity_file="$state_dir/server.identity"
config_file="$state_dir/server.config"
running_version_file="$state_dir/server.running-version"
shutdown_file="$state_dir/shutdown.request"

data_dir="${VECTORS_DATA_DIR:-$default_data_dir}"
snapshot="${VECTORS_SNAPSHOT:-}"
autosave="${VECTORS_AUTOSAVE_INTERVAL_SECS:-30}"
http_shutdown_timeout="${VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS:-30}"
if [ -n "${VECTORS_LOG_FILE:-}" ]; then
    log_file=$VECTORS_LOG_FILE
elif [ -n "${VECTORS_STATE_DIR:-}" ]; then
    log_file="$state_dir/server.log"
else
    log_file=$default_log_file
fi
if [ -n "${VECTORS_DATA_DIR:-}" ] && [ -n "$snapshot" ]; then
    die "VECTORS_DATA_DIR and VECTORS_SNAPSHOT select different storage modes; set only one"
fi

prior_config_active=0
prior_bind=""
prior_storage_mode=""
prior_storage_path=""
prior_autosave=""
prior_log_file=""
prior_running_version=""
prior_api_token_enabled=""
prior_http_shutdown_timeout=""
legacy_running_version=""
legacy_state_present=0
old_pid_hint=""
if [ -e "$config_file" ] || [ -L "$config_file" ]; then
    [ -f "$config_file" ] && [ ! -L "$config_file" ] \
        || die "managed server configuration must be a regular file: $config_file"
fi
if [ -f "$pid_file" ]; then
    old_pid_hint=$(sed -n '1p' "$pid_file" 2>/dev/null || true)
    case "$old_pid_hint" in
        ''|*[!0-9]*) old_pid_hint="" ;;
        *) kill -0 "$old_pid_hint" 2>/dev/null || old_pid_hint="" ;;
    esac
fi
if [ -f "$config_file" ]; then
    config_line_count=$(awk 'END { print NR + 0 }' "$config_file")
    config_header=$(sed -n '1p' "$config_file")
    prior_bind=$(sed -n '2p' "$config_file")
    prior_storage_mode=$(sed -n '3p' "$config_file")
    prior_storage_path=$(sed -n '4p' "$config_file")
    prior_autosave=$(sed -n '5p' "$config_file")
    prior_log_file=$(sed -n '6p' "$config_file")
    prior_running_version=$(sed -n '7p' "$config_file")
    prior_api_token_enabled=$(sed -n '8p' "$config_file")
    prior_http_shutdown_timeout=$(sed -n '9p' "$config_file")
    [ "$config_line_count" = "9" ] && [ "$config_header" = "vectors-installer-config-v1" ] \
        || die "managed server configuration is malformed: $config_file"
    case "$prior_storage_mode" in data|snapshot) ;; *) die "invalid storage mode in $config_file" ;; esac
    [ -n "$prior_bind" ] && [ -n "$prior_storage_path" ] && [ -n "$prior_log_file" ] \
        || die "managed server configuration has empty required fields"
    case "$prior_storage_path" in /*) ;; *) die "managed storage path must be absolute" ;; esac
    case "$prior_log_file" in /*) ;; *) die "managed log path must be absolute" ;; esac
    case "$prior_autosave" in ''|*[!0-9]*) die "invalid autosave interval in $config_file" ;; esac
    if [ "$prior_storage_mode" = "snapshot" ] && [ "$prior_autosave" -eq 0 ]; then
        die "snapshot autosave interval in $config_file must be positive"
    fi
    case "$prior_running_version" in ''|*[!0-9A-Za-z.+-]*) die "invalid running version in $config_file" ;; esac
    case "$prior_api_token_enabled" in 0|1) ;; *) die "invalid API-token marker in $config_file" ;; esac
    case "$prior_http_shutdown_timeout" in ''|*[!0-9]*) die "invalid HTTP shutdown timeout in $config_file" ;; esac
    [ "$prior_http_shutdown_timeout" -gt 0 ] || die "HTTP shutdown timeout in $config_file must be positive"
    prior_config_active=1
fi
if [ "$prior_config_active" -eq 0 ] \
    && [ -f "$running_version_file" ] && [ ! -L "$running_version_file" ]; then
    marker_line_count=$(awk 'END { print NR + 0 }' "$running_version_file")
    legacy_running_version=$(sed -n '1p' "$running_version_file")
    case "$legacy_running_version" in ''|*[!0-9A-Za-z.+-]*) die "invalid running-version marker" ;; esac
    [ "$marker_line_count" = "1" ] || die "running-version marker is malformed"
    legacy_state_present=1
fi

if [ "$prior_config_active" -eq 1 ]; then
    [ "$bind_explicit" -eq 1 ] || bind=$prior_bind
    if [ "$storage_explicit" -eq 0 ]; then
        if [ "$prior_storage_mode" = "snapshot" ]; then
            snapshot=$prior_storage_path
        else
            snapshot=""
            data_dir=$prior_storage_path
        fi
    fi
    [ "$autosave_explicit" -eq 1 ] || autosave=$prior_autosave
    [ "$log_explicit" -eq 1 ] || log_file=$prior_log_file
    [ "$http_timeout_explicit" -eq 1 ] || http_shutdown_timeout=$prior_http_shutdown_timeout
fi

case "$autosave" in ''|*[!0-9]*) die "VECTORS_AUTOSAVE_INTERVAL_SECS must be a non-negative integer" ;; esac
case "$http_shutdown_timeout" in ''|*[!0-9]*) die "VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS must be a positive integer" ;; esac
[ "$http_shutdown_timeout" -gt 0 ] || die "VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS must be a positive integer"
parse_bind_address

case "$data_dir" in /*) ;; *) data_dir="$(pwd -P)/$data_dir" ;; esac
case "$snapshot" in ''|/*) ;; *) snapshot="$(pwd -P)/$snapshot" ;; esac
case "$log_file" in /*) ;; *) log_file="$(pwd -P)/$log_file" ;; esac

if [ -n "$snapshot" ]; then
    storage_mode="snapshot"
    storage_path=$snapshot
    [ "$autosave" -gt 0 ] || die "VECTORS_AUTOSAVE_INTERVAL_SECS must be positive in snapshot mode"
else
    storage_mode="data"
    storage_path=$data_dir
fi

rollback_bind=$bind
rollback_storage_mode=$storage_mode
rollback_storage_path=$storage_path
rollback_autosave=$autosave
rollback_log_file=$log_file
rollback_running_version=$prior_running_version
[ -n "$rollback_running_version" ] || rollback_running_version=$legacy_running_version
rollback_api_token_enabled=$api_token_enabled
rollback_http_shutdown_timeout=$http_shutdown_timeout
if [ "$prior_config_active" -eq 1 ]; then
    rollback_bind=$prior_bind
    rollback_storage_mode=$prior_storage_mode
    rollback_storage_path=$prior_storage_path
    rollback_autosave=$prior_autosave
    rollback_log_file=$prior_log_file
    rollback_api_token_enabled=$prior_api_token_enabled
    rollback_http_shutdown_timeout=$prior_http_shutdown_timeout
fi
requested_bind=$bind
requested_storage_mode=$storage_mode
requested_storage_path=$storage_path
requested_autosave=$autosave
requested_log_file=$log_file
requested_api_token_enabled=$api_token_enabled
requested_http_shutdown_timeout=$http_shutdown_timeout

if [ "$dry_run" -eq 1 ]; then
    echo "vectors installation plan"
    echo "  host:        $host_os $host_arch"
    echo "  target:      $target"
    echo "  archive:     $asset"
    echo "  download:    $asset_url"
    echo "  install dir: $install_dir"
    echo "  storage:     $storage_mode ($storage_path)"
    echo "  state dir:   $state_dir"
    echo "  log:         $log_file"
    echo "  autosave:    $autosave seconds"
    echo "  shutdown:    $http_shutdown_timeout seconds"
    echo "  API token:   $api_token_enabled"
    echo "  bind:        $bind"
    echo "  start:       $start_server"
    echo "  restart:     $restart_server"
    echo "  open:        $open_console"
    exit 0
fi

if [ "$host_os" = "Linux" ]; then
    if [ -f /etc/alpine-release ] \
        || { command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; }; then
        die "official Linux archives require glibc; build vectors from source on musl-based systems"
    fi
fi
command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

temporary=$(mktemp -d "${TMPDIR:-/tmp}/vectors-install.XXXXXX") || die "could not create a temporary directory"
trap cleanup 0
trap 'exit 130' HUP INT TERM

echo "Downloading $asset..."
download "$asset_url" "$temporary/$asset" \
    || die "could not download $asset_url; the requested release may not publish this target"
download "$release_url/SHA256SUMS" "$temporary/SHA256SUMS" || die "could not download release checksums"

checksum_count=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { count++ } END { print count + 0 }' "$temporary/SHA256SUMS")
[ "$checksum_count" = "1" ] || die "SHA256SUMS must contain exactly one entry for $asset"
expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1; exit }' "$temporary/SHA256SUMS")
[ "${#expected}" -eq 64 ] || die "release checksum for $asset is malformed"
case "$expected" in *[!0-9A-Fa-f]*) die "release checksum for $asset is malformed" ;; esac

if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temporary/$asset" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$temporary/$asset" | awk '{print $1}')
elif command -v openssl >/dev/null 2>&1; then
    actual=$(openssl dgst -sha256 "$temporary/$asset" | awk '{print $NF}')
else
    die "sha256sum, shasum, or openssl is required to verify the download"
fi
expected=$(printf '%s' "$expected" | tr 'A-F' 'a-f')
actual=$(printf '%s' "$actual" | tr 'A-F' 'a-f')
[ "$actual" = "$expected" ] || die "archive checksum does not match SHA256SUMS"
echo "Checksum verified."

mkdir "$temporary/unpack"
unexpected_member=$(tar -tzf "$temporary/$asset" \
    | awk '$0 != "vectors" && $0 != "vectors-server" && $0 != "./vectors" && $0 != "./vectors-server" { print; exit }') \
    || die "could not inspect $asset"
[ -z "$unexpected_member" ] || die "archive contains unexpected member '$unexpected_member'"
tar -xzf "$temporary/$asset" -C "$temporary/unpack" || die "could not extract $asset"
for binary in vectors vectors-server; do
    [ -f "$temporary/unpack/$binary" ] || die "archive is missing $binary"
    [ ! -L "$temporary/unpack/$binary" ] || die "archive entry $binary must not be a symbolic link"
    chmod 755 "$temporary/unpack/$binary"
done

client_report=$("$temporary/unpack/vectors" --version) || die "downloaded vectors binary could not be executed"
server_report=$("$temporary/unpack/vectors-server" --version) || die "downloaded vectors-server binary could not be executed"
case "$client_report" in
    "vectors "*) client_version=${client_report#vectors } ;;
    *) die "unexpected vectors version output: '$client_report'" ;;
esac
case "$server_report" in
    "vectors-server "*) server_version=${server_report#vectors-server } ;;
    *) die "unexpected vectors-server version output: '$server_report'" ;;
esac
case "$client_version" in ''|*[!0-9A-Za-z.+-]*) die "release binaries reported a malformed version" ;; esac
[ -n "$client_version" ] && [ "$client_version" = "$server_version" ] \
    || die "release binaries report different versions: '$client_report' and '$server_report'"
if [ -n "$version" ]; then
    requested_version=${version#v}
    [ "$client_version" = "$requested_version" ] || die "requested $version, but the archive contains $client_version"
fi

mkdir -p "$install_dir" || die "could not create installation directory $install_dir"
install_dir=$(cd -P "$install_dir" && pwd -P) || die "could not resolve installation directory"
[ "$install_dir" != "/" ] || die "refusing to install binaries in the filesystem root"
[ -d "$install_dir" ] && [ -w "$install_dir" ] || die "installation directory is not writable: $install_dir"
[ ! -d "$install_dir/vectors" ] || die "$install_dir/vectors is a directory"
[ ! -d "$install_dir/vectors-server" ] || die "$install_dir/vectors-server is a directory"

previous_version=""
previous_server_version=""
if [ -x "$install_dir/vectors" ]; then
    previous_report=$("$install_dir/vectors" --version 2>/dev/null || true)
    case "$previous_report" in "vectors "*) previous_version=${previous_report#vectors } ;; esac
fi
if [ -x "$install_dir/vectors-server" ]; then
    previous_server_report=$("$install_dir/vectors-server" --version 2>/dev/null || true)
    case "$previous_server_report" in "vectors-server "*) previous_server_version=${previous_server_report#vectors-server } ;; esac
fi
[ -n "$rollback_running_version" ] || rollback_running_version=$previous_version

had_vectors=0
had_server=0
if [ -f "$install_dir/vectors" ]; then
    backup_vectors=$(mktemp "$install_dir/.vectors-backup.XXXXXX") || die "could not back up vectors"
    cp "$install_dir/vectors" "$backup_vectors"
    chmod 755 "$backup_vectors"
    had_vectors=1
fi
if [ -f "$install_dir/vectors-server" ]; then
    backup_server=$(mktemp "$install_dir/.vectors-server-backup.XXXXXX") || die "could not back up vectors-server"
    cp "$install_dir/vectors-server" "$backup_server"
    chmod 755 "$backup_server"
    had_server=1
fi

persistent_vectors="$install_dir/.vectors.previous"
persistent_server="$install_dir/.vectors-server.previous"
rollback_vectors=$backup_vectors
rollback_server=$backup_server
rollback_available=0
backup_pair_version=""
if [ "$had_vectors" -eq 1 ] && [ "$had_server" -eq 1 ] \
    && [ -n "$previous_version" ] && [ "$previous_version" = "$previous_server_version" ]; then
    backup_pair_version=$previous_version
fi
persistent_pair_version=""
if [ -x "$persistent_vectors" ] && [ -x "$persistent_server" ]; then
    persistent_client_report=$("$persistent_vectors" --version 2>/dev/null || true)
    persistent_server_report=$("$persistent_server" --version 2>/dev/null || true)
    case "$persistent_client_report:$persistent_server_report" in
        "vectors "*:"vectors-server "*)
            persistent_client_version=${persistent_client_report#vectors }
            persistent_server_version=${persistent_server_report#vectors-server }
            if [ "$persistent_client_version" = "$persistent_server_version" ]; then
                persistent_pair_version=$persistent_client_version
            fi
            ;;
    esac
fi
if [ -n "$rollback_running_version" ] && [ "$persistent_pair_version" = "$rollback_running_version" ]; then
    rollback_vectors=$persistent_vectors
    rollback_server=$persistent_server
    rollback_available=1
elif [ -n "$rollback_running_version" ] && [ "$backup_pair_version" = "$rollback_running_version" ]; then
    rollback_available=1
elif [ "$prior_config_active" -eq 0 ] && [ -n "$backup_pair_version" ]; then
    rollback_running_version=$backup_pair_version
    rollback_available=1
fi

# Same-filesystem renames ensure neither executable is ever partial. Backups
# allow the pair to be restored if the second rename fails.
staged_vectors=$(mktemp "$install_dir/.vectors.XXXXXX") || die "could not stage vectors"
staged_server=$(mktemp "$install_dir/.vectors-server.XXXXXX") || die "could not stage vectors-server"
cp "$temporary/unpack/vectors" "$staged_vectors"
cp "$temporary/unpack/vectors-server" "$staged_server"
chmod 755 "$staged_vectors" "$staged_server"
"$staged_vectors" --version >/dev/null
"$staged_server" --version >/dev/null
if [ "$start_server" -eq 1 ]; then
    command -v nohup >/dev/null 2>&1 || die "nohup is required to start vectors-server"
    ensure_runtime_directories || die "could not prepare vectors storage, state, or log paths"
fi
begin_critical_section
mv -f "$staged_vectors" "$install_dir/vectors" || die "could not install vectors"
staged_vectors=""
if mv -f "$staged_server" "$install_dir/vectors-server"; then
    staged_server=""
else
    pair_restore_ok=1
    if [ "$had_vectors" -eq 1 ]; then
        if ! cp "$backup_vectors" "$install_dir/.vectors-restore.$$" \
            || ! chmod 755 "$install_dir/.vectors-restore.$$" \
            || ! mv -f "$install_dir/.vectors-restore.$$" "$install_dir/vectors"; then
            pair_restore_ok=0
        fi
    else
        rm -f "$install_dir/vectors" || pair_restore_ok=0
    fi
    if [ "$pair_restore_ok" -ne 1 ]; then
        retained_vectors=$backup_vectors
        retained_server=$backup_server
        backup_vectors=""
        backup_server=""
        echo "error: binary transaction rollback failed; backups remain at $retained_vectors and $retained_server" >&2
        end_critical_section
        die "could not install vectors-server; manual binary recovery is required"
    fi
    end_critical_section
    die "could not install vectors-server; the previous binary pair was restored"
fi

preserve_previous_pair() {
    [ -n "$rollback_running_version" ] || return 0
    if [ "$persistent_pair_version" = "$rollback_running_version" ]; then return 0; fi
    [ "$backup_pair_version" = "$rollback_running_version" ] || return 1
    persistent_stage_vectors=$(mktemp "$install_dir/.vectors-previous.XXXXXX") \
        || return 1
    persistent_stage_server=$(mktemp "$install_dir/.vectors-server-previous.XXXXXX") \
        || return 1
    cp "$backup_vectors" "$persistent_stage_vectors" || return 1
    cp "$backup_server" "$persistent_stage_server" || return 1
    chmod 755 "$persistent_stage_vectors" "$persistent_stage_server" || return 1
    mv -f "$persistent_stage_vectors" "$persistent_vectors" || return 1
    persistent_stage_vectors=""
    mv -f "$persistent_stage_server" "$persistent_server" || return 1
    persistent_stage_server=""
    persistent_pair_version=$rollback_running_version
}

preserve_previous_pair_or_retain() {
    preserve_previous_pair && return 0
    if [ "$backup_pair_version" = "$rollback_running_version" ]; then
        retained_vectors=$backup_vectors
        retained_server=$backup_server
        backup_vectors=""
        backup_server=""
        echo "warning: rollback binaries could not be moved into place; retained at $retained_vectors and $retained_server" >&2
    fi
    return 1
}

record_deferred_running_version() {
    if [ "$prior_config_active" -eq 0 ]; then
        write_running_version_marker "$rollback_running_version" || return 1
    fi
    return 0
}

restore_previous_pair() {
    [ "$rollback_available" -eq 1 ] || return 1
    staged_vectors=$(mktemp "$install_dir/.vectors-rollback.XXXXXX") || return 1
    staged_server=$(mktemp "$install_dir/.vectors-server-rollback.XXXXXX") || return 1
    cp "$rollback_vectors" "$staged_vectors" || return 1
    cp "$rollback_server" "$staged_server" || return 1
    chmod 755 "$staged_vectors" "$staged_server" || return 1
    rollback_client_report=$("$staged_vectors" --version 2>/dev/null || true)
    rollback_server_report=$("$staged_server" --version 2>/dev/null || true)
    case "$rollback_client_report:$rollback_server_report" in
        "vectors "*:"vectors-server "*) ;;
        *) return 1 ;;
    esac
    rollback_client_version=${rollback_client_report#vectors }
    rollback_server_version=${rollback_server_report#vectors-server }
    [ "$rollback_client_version" = "$rollback_server_version" ] || return 1
    mv -f "$staged_vectors" "$install_dir/vectors" || return 1
    staged_vectors=""
    if ! mv -f "$staged_server" "$install_dir/vectors-server"; then
        # Preserve the sources when automatic recovery cannot finish.
        backup_vectors=""
        backup_server=""
        staged_server=""
        echo "rollback binaries were retained at $rollback_vectors and $rollback_server" >&2
        return 1
    fi
    staged_server=""
    return 0
}

case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *)
        if [ "$uses_default_install_dir" -eq 1 ]; then
            shell_name=${SHELL:-}
            shell_name=${shell_name##*/}
            case "$shell_name" in
                zsh)
                    if [ "$host_os" = "Darwin" ]; then path_profile="$HOME/.zprofile"; else path_profile="$HOME/.zshrc"; fi
                    path_line='export PATH="$HOME/.local/bin:$PATH"'
                    ;;
                bash)
                    if [ "$host_os" = "Darwin" ]; then
                        if [ -f "$HOME/.bash_profile" ]; then path_profile="$HOME/.bash_profile"; else path_profile="$HOME/.profile"; fi
                    else
                        path_profile="$HOME/.bashrc"
                    fi
                    path_line='export PATH="$HOME/.local/bin:$PATH"'
                    ;;
                fish)
                    path_profile="$HOME/.config/fish/conf.d/vectors.fish"
                    path_line='set -gx PATH "$HOME/.local/bin" $PATH'
                    if ! mkdir -p "$HOME/.config/fish/conf.d"; then
                        echo "warning: could not create Fish PATH configuration directory" >&2
                        path_profile=""
                    fi
                    ;;
                *)
                    path_profile="$HOME/.profile"
                    path_line='export PATH="$HOME/.local/bin:$PATH"'
                    ;;
            esac
            if [ -z "$path_profile" ]; then
                :
            elif [ -e "$path_profile" ] && [ ! -f "$path_profile" ]; then
                echo "warning: cannot update PATH because $path_profile is not a regular file" >&2
            elif ! grep -F "$path_line" "$path_profile" >/dev/null 2>&1; then
                if printf '\n# Added by the vectors installer.\n%s\n' "$path_line" >> "$path_profile"; then
                    echo "Added ~/.local/bin to PATH in $path_profile."
                else
                    echo "warning: could not update PATH in $path_profile" >&2
                fi
            fi
        fi
        ;;
esac

installed_report=$("$install_dir/vectors" --version)
if [ -n "$previous_version" ] && [ "$previous_version" != "$client_version" ]; then
    echo "Upgraded vectors $previous_version -> $client_version in $install_dir."
else
    echo "Installed $installed_report in $install_dir."
fi
case ":${PATH:-}:" in *":$install_dir:"*) ;; *) echo "Open a new terminal, or add '$install_dir' to PATH for this shell." ;; esac

old_pid=""
managed_server_running=0
if [ -f "$pid_file" ]; then
    old_pid=$(cat "$pid_file" 2>/dev/null || true)
    case "$old_pid" in
        ''|*[!0-9]*) old_pid="" ;;
        *)
            if kill -0 "$old_pid" 2>/dev/null; then
                if process_is_vectors_server "$old_pid"; then
                    managed_server_running=1
                else
                    if ! preserve_previous_pair_or_retain; then
                        end_critical_section
                        die "could not retain binaries matching running vectors-server $rollback_running_version"
                    fi
                    end_critical_section
                    die "$pid_file points to live PID $old_pid, which is not vectors-server; refusing to signal it"
                fi
            fi
            ;;
    esac
fi

if [ "$start_server" -eq 0 ]; then
    if [ "$managed_server_running" -eq 1 ]; then
        echo "The new binaries are ready, but vectors-server PID $old_pid remains on its already-loaded version."
        if [ "$prior_config_active" -eq 0 ]; then
            echo "Legacy server settings are not recorded; verify the bind and storage values before restarting."
        fi
        echo "Apply the upgrade with:"
        show_installer_command "$bind" 1
        if ! preserve_previous_pair_or_retain; then
            end_critical_section
            die "could not retain rollback binaries for running vectors-server $rollback_running_version"
        fi
        if ! record_deferred_running_version; then
            end_critical_section
            die "could not record running vectors-server $rollback_running_version"
        fi
    else
        rm -f "$persistent_vectors" "$persistent_server"
    fi
    echo "SQL shell: '$install_dir/vectors' (then type .tutorial)"
    echo "Server:"
    show_direct_server_command "$bind"
    echo "Uninstall: remove both binaries from $install_dir; stored data is preserved at $storage_path."
    end_critical_section
    exit 0
fi

request_server_shutdown() {
    shutdown_pid=$1
    shutdown_wait=$2
    shutdown_allow_sigterm=$3
    if [ -e "$shutdown_file" ] || [ -L "$shutdown_file" ]; then
        [ -f "$shutdown_file" ] && [ ! -L "$shutdown_file" ] || return 1
        rm -f "$shutdown_file" || return 1
    fi
    shutdown_temporary=$(mktemp "$state_dir/.shutdown-request.XXXXXX") || return 1
    if ! chmod 600 "$shutdown_temporary"; then rm -f "$shutdown_temporary"; return 1; fi
    if mv -f "$shutdown_temporary" "$shutdown_file" 2>/dev/null; then
        shutdown_attempt=0
        while kill -0 "$shutdown_pid" 2>/dev/null && [ "$shutdown_attempt" -lt "$shutdown_wait" ]; do
            sleep 1
            shutdown_attempt=$((shutdown_attempt + 1))
        done
        if ! kill -0 "$shutdown_pid" 2>/dev/null; then
            rm -f "$shutdown_file"
            return 0
        fi
    fi
    rm -f "$shutdown_temporary" "$shutdown_file"
    if [ "$shutdown_allow_sigterm" -ne 1 ]; then
        echo "Cooperative shutdown did not finish within $shutdown_wait seconds; no signal fallback was used." >&2
        return 1
    fi
    echo "Cooperative shutdown timed out; sending SIGTERM to legacy server PID $shutdown_pid..." >&2
    kill "$shutdown_pid" 2>/dev/null || return 1
    shutdown_attempt=0
    while kill -0 "$shutdown_pid" 2>/dev/null && [ "$shutdown_attempt" -lt "$shutdown_wait" ]; do
        sleep 1
        shutdown_attempt=$((shutdown_attempt + 1))
    done
    kill -0 "$shutdown_pid" 2>/dev/null && return 1
    return 0
}

restarting_existing=0
if [ "$managed_server_running" -eq 1 ]; then
    if [ "$restart_server" -eq 0 ]; then
        if ! preserve_previous_pair_or_retain; then
            end_critical_section
            die "could not retain rollback binaries for running vectors-server $rollback_running_version"
        fi
        if ! record_deferred_running_version; then
            end_critical_section
            die "could not record running vectors-server $rollback_running_version"
        fi
        echo "The new binaries are ready, but vectors-server PID $old_pid remains on its already-loaded version."
        if [ "$prior_config_active" -eq 0 ]; then
            echo "Legacy server settings are not recorded; verify the bind and storage values before restarting."
        fi
        echo "Apply the upgrade with:"
        show_installer_command "$bind" 1
        echo "No process was stopped. Stored data remains at $storage_path."
        end_critical_section
        exit 0
    fi
    if [ "$rollback_available" -ne 1 ]; then
        preserve_previous_pair_or_retain || true
        end_critical_section
        die "no rollback binary pair matches running vectors-server $rollback_running_version; server was left running"
    fi
    if [ "$prior_config_active" -eq 0 ] \
        && { [ "$bind_explicit" -ne 1 ] || [ "$storage_explicit" -ne 1 ]; }; then
        preserve_previous_pair_or_retain || true
        record_deferred_running_version || true
        end_critical_section
        die "legacy managed server settings are unknown; rerun --restart with its current --bind and either VECTORS_DATA_DIR or VECTORS_SNAPSHOT"
    fi
    if [ "$rollback_api_token_enabled" -eq 1 ] && [ -z "$api_token_value" ]; then
        preserve_previous_pair_or_retain || true
        end_critical_section
        die "running vectors-server requires VECTORS_API_TOKEN; export the token before --restart"
    fi
    apply_rollback_config
    if ! ensure_runtime_directories; then
        restore_requested_config
        preserve_previous_pair_or_retain || true
        end_critical_section
        die "rollback storage or log paths are unavailable; server was left running"
    fi
    restore_requested_config
    echo "Stopping managed vectors-server PID $old_pid..."
    cooperative_wait=$((rollback_http_shutdown_timeout + 5))
    if [ "$prior_config_active" -eq 0 ]; then old_allow_sigterm=1; else old_allow_sigterm=0; fi
    if ! request_server_shutdown "$old_pid" "$cooperative_wait" "$old_allow_sigterm"; then
        preserve_previous_pair_or_retain || true
        end_critical_section
        die "could not stop vectors-server PID $old_pid"
    fi
    rm -f "$pid_file" "$identity_file" "$shutdown_file" || true
    restarting_existing=1
elif [ -f "$pid_file" ]; then
    rm -f "$pid_file"
    rm -f "$identity_file"
fi

if [ "$prior_config_active" -eq 1 ] && [ "$prior_api_token_enabled" -eq 1 ] \
    && [ -z "$api_token_value" ]; then
    end_critical_section
    die "saved managed server requires VECTORS_API_TOKEN; export the token before starting it"
fi
if [ "$prior_config_active" -eq 0 ] && [ "$legacy_state_present" -eq 1 ] \
    && [ "$managed_server_running" -eq 0 ] \
    && { [ "$bind_explicit" -ne 1 ] || [ "$storage_explicit" -ne 1 ]; }; then
    end_critical_section
    die "stopped legacy server settings are unknown; specify its --bind and either VECTORS_DATA_DIR or VECTORS_SNAPSHOT"
fi

launch_server() {
    if [ -e "$shutdown_file" ] || [ -L "$shutdown_file" ]; then
        [ -f "$shutdown_file" ] && [ ! -L "$shutdown_file" ] || return 1
        rm -f "$shutdown_file" || return 1
    fi
    echo "Starting vectors-server on $bind..." >> "$log_file"
    if [ -n "$snapshot" ]; then
        (
            trap - 0 HUP INT TERM
            unset VECTORS_DATA_DIR
            VECTORS_BIND=$bind; VECTORS_SNAPSHOT=$snapshot; VECTORS_AUTOSAVE_INTERVAL_SECS=$autosave; VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS=$http_shutdown_timeout; VECTORS_SHUTDOWN_FILE=$shutdown_file
            export VECTORS_BIND VECTORS_SNAPSHOT VECTORS_AUTOSAVE_INTERVAL_SECS VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS VECTORS_SHUTDOWN_FILE
            if [ "$api_token_enabled" -eq 1 ]; then
                VECTORS_API_TOKEN=$api_token_value; export VECTORS_API_TOKEN
            else
                unset VECTORS_API_TOKEN
            fi
            exec nohup "$install_dir/vectors-server"
        ) </dev/null >> "$log_file" 2>&1 &
    else
        (
            trap - 0 HUP INT TERM
            unset VECTORS_SNAPSHOT VECTORS_AUTOSAVE_INTERVAL_SECS
            VECTORS_BIND=$bind; VECTORS_DATA_DIR=$data_dir; VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS=$http_shutdown_timeout; VECTORS_SHUTDOWN_FILE=$shutdown_file
            export VECTORS_BIND VECTORS_DATA_DIR VECTORS_HTTP_SHUTDOWN_TIMEOUT_SECS VECTORS_SHUTDOWN_FILE
            if [ "$api_token_enabled" -eq 1 ]; then
                VECTORS_API_TOKEN=$api_token_value; export VECTORS_API_TOKEN
            else
                unset VECTORS_API_TOKEN
            fi
            exec nohup "$install_dir/vectors-server"
        ) </dev/null >> "$log_file" 2>&1 &
    fi
    server_pid=$!

    identity_started=""
    identity_attempt=0
    while [ "$identity_attempt" -lt 5 ]; do
        kill -0 "$server_pid" 2>/dev/null || return 1
        identity_started=$(ps -p "$server_pid" -o lstart= 2>/dev/null | awk '{$1=$1; print; exit}')
        [ -z "$identity_started" ] || break
        sleep 1
        identity_attempt=$((identity_attempt + 1))
    done
    [ -n "$identity_started" ] || return 1

    identity_temporary="$identity_file.new.$$"
    pid_temporary="$pid_file.new.$$"
    printf '%s\n%s\n' "$server_pid" "$identity_started" > "$identity_temporary" || return 1
    printf '%s\n' "$server_pid" > "$pid_temporary" || return 1
    mv -f "$identity_temporary" "$identity_file" || return 1
    mv -f "$pid_temporary" "$pid_file" || return 1
    return 0
}

wait_for_server() {
    attempt=0
    while [ "$attempt" -lt 30 ]; do
        kill -0 "$server_pid" 2>/dev/null || return 1
        if curl --noproxy '*' --fail --silent --show-error --connect-timeout 1 --max-time 2 \
            "$console_url/healthz" 2>/dev/null | grep -q '"status":"ok"'; then
            return 0
        fi
        sleep 1
        attempt=$((attempt + 1))
    done
    return 2
}

stop_launched_server() {
    if [ -n "${server_pid:-}" ] && kill -0 "$server_pid" 2>/dev/null; then
        cooperative_wait=$((http_shutdown_timeout + 5))
        request_server_shutdown "$server_pid" "$cooperative_wait" "$launched_allow_sigterm" || return 1
    fi
    rm -f "$pid_file" "$identity_file" "$shutdown_file"
    return 0
}

retain_rollback_sources() {
    if [ "$rollback_vectors" = "$backup_vectors" ]; then backup_vectors=""; fi
    if [ "$rollback_server" = "$backup_server" ]; then backup_server=""; fi
    echo "Automatic rollback could not finish." >&2
    echo "Recovery binaries were retained at $rollback_vectors and $rollback_server." >&2
}

recover_previous_service() {
    if ! stop_launched_server; then
        retain_rollback_sources
        return 1
    fi
    if ! restore_previous_pair; then
        retain_rollback_sources
        return 1
    fi
    echo "The previous binary pair was restored; attempting to recover the service..." >&2
    apply_rollback_config
    if ! ensure_runtime_directories; then
        restore_requested_config
        return 1
    fi
    server_pid=""
    if [ "$prior_config_active" -eq 0 ]; then launched_allow_sigterm=1; else launched_allow_sigterm=0; fi
    if ! launch_server; then
        restore_requested_config
        return 1
    fi
    if wait_for_server; then
        recovered_url=$console_url
        if ! write_server_config "$rollback_running_version"; then
            stop_launched_server || true
            restore_requested_config
            return 1
        fi
        rm -f "$persistent_vectors" "$persistent_server"
        echo "The previous vectors-server $rollback_running_version is healthy again at $recovered_url." >&2
        restarting_existing=1
        restore_requested_config
        return 0
    fi
    stop_launched_server || true
    restore_requested_config
    return 1
}

server_pid=""
launched_allow_sigterm=0
if port_is_occupied; then
    echo "error: $bind is already accepting connections; vectors-server was not started" >&2
    if [ "$restarting_existing" -eq 1 ]; then
        recover_previous_service || true
    else
        rm -f "$persistent_vectors" "$persistent_server"
    fi
    show_log_tail
    show_port_retry
    end_critical_section
    exit 1
fi

startup_failure=0
if ! launch_server; then
    startup_failure=1
elif wait_for_server; then
    if ! write_server_config "$client_version"; then startup_failure=3; fi
else
    startup_failure=$?
fi

if [ "$startup_failure" -ne 0 ]; then
    if [ "$startup_failure" -eq 1 ]; then
        echo "error: vectors-server stopped during startup" >&2
    elif [ "$startup_failure" -eq 3 ]; then
        echo "error: vectors-server became healthy but its managed configuration could not be saved" >&2
    else
        echo "error: vectors-server did not become ready at $console_url after 30 seconds" >&2
    fi
    show_log_tail
    if [ "$rollback_available" -eq 1 ]; then
        if recover_previous_service; then
            echo "The upgrade was rolled back without leaving the managed service down." >&2
        else
            echo "error: the previous service could not be restarted; inspect $log_file" >&2
        fi
    else
        stop_launched_server || true
    fi
    show_port_retry
    end_critical_section
    exit 1
fi

rm -f "$persistent_vectors" "$persistent_server"
end_critical_section
echo "vectors-server started with PID $server_pid."
echo "Web console: $console_url"
echo "Tutorial: open 'Start here', or run '$install_dir/vectors' and type '.tutorial'."
if [ -n "$snapshot" ]; then echo "Snapshot: $snapshot"; else echo "Durable data: $data_dir"; fi
echo "Log: $log_file"
quoted_pid_file=$(quote_for_shell "$pid_file")
quoted_shutdown_template=$(quote_for_shell "$state_dir/.shutdown-request.XXXXXX")
quoted_shutdown_file=$(quote_for_shell "$shutdown_file")
echo "Stop gracefully (waits for the final checkpoint):"
echo "  pid=\$(cat $quoted_pid_file) && request=\$(mktemp $quoted_shutdown_template) && chmod 600 \"\$request\" && mv -f \"\$request\" $quoted_shutdown_file && while kill -0 \"\$pid\" 2>/dev/null; do sleep 1; done"
echo "Uninstall: stop the server, then remove both binaries from $install_dir. Data is preserved."
if [ "$open_console" -eq 1 ] && command -v "$browser_opener" >/dev/null 2>&1; then
    if [ "$host_os" = "Darwin" ]; then
        [ -n "${SSH_CONNECTION:-}" ] || "$browser_opener" "$console_url" >/dev/null 2>&1 &
    elif [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; then
        "$browser_opener" "$console_url" >/dev/null 2>&1 &
    fi
fi
exit 0
