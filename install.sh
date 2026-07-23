#!/bin/sh
set -eu

repository="kamilsj/vectors"
default_install_dir="${HOME:?HOME is not set}/.local/bin"
install_dir="${VECTORS_INSTALL_DIR:-$default_install_dir}"
version="${VECTORS_VERSION:-}"
bind="${VECTORS_BIND:-127.0.0.1:8080}"
start_server=1
open_console=1

if [ "${VECTORS_NO_START:-0}" = "1" ]; then
    start_server=0
fi
if [ "${VECTORS_NO_OPEN:-0}" = "1" ]; then
    open_console=0
fi

usage() {
    cat <<'EOF'
Install vectors for Linux x86-64.

Usage: install.sh [options]

Options:
  --version TAG       Install a release tag such as v0.6.0
  --install-dir PATH  Install binaries here (default: ~/.local/bin)
  --bind ADDRESS      Server address (default: 127.0.0.1:8080)
  --no-start          Install without starting vectors-server
  --no-open           Do not open the web console
  -h, --help          Show this help

Environment equivalents: VECTORS_VERSION, VECTORS_INSTALL_DIR,
VECTORS_BIND, VECTORS_NO_START=1, and VECTORS_NO_OPEN=1.
The started server uses VECTORS_DATA_DIR (platform default) for durable storage.
Set VECTORS_SNAPSHOT to retain legacy interval-based snapshot mode.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || { echo "error: --version needs a value" >&2; exit 2; }
            version=$2
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || { echo "error: --install-dir needs a value" >&2; exit 2; }
            install_dir=$2
            shift 2
            ;;
        --bind)
            [ "$#" -ge 2 ] || { echo "error: --bind needs a value" >&2; exit 2; }
            bind=$2
            shift 2
            ;;
        --no-start)
            start_server=0
            shift
            ;;
        --no-open)
            open_console=0
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown option '$1'" >&2
            usage >&2
            exit 2
            ;;
    esac
done

[ "$(uname -s)" = "Linux" ] || {
    echo "error: this installer supports Linux; use install.ps1 on Windows" >&2
    exit 1
}
case "$(uname -m)" in
    x86_64|amd64) ;;
    *)
        echo "error: no release binary is available for architecture $(uname -m)" >&2
        exit 1
        ;;
esac
command -v curl >/dev/null 2>&1 || { echo "error: curl is required" >&2; exit 1; }
command -v tar >/dev/null 2>&1 || { echo "error: tar is required" >&2; exit 1; }

asset="vectors-x86_64-unknown-linux-gnu.tar.gz"
if [ -n "$version" ]; then
    case "$version" in v*) ;; *) version="v$version" ;; esac
    release_url="https://github.com/$repository/releases/download/$version"
else
    release_url="https://github.com/$repository/releases/latest/download"
fi

temporary=$(mktemp -d "${TMPDIR:-/tmp}/vectors-install.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

echo "Downloading vectors from $release_url..."
curl --proto '=https' --tlsv1.2 -fL --retry 3 --connect-timeout 15 \
    -o "$temporary/$asset" "$release_url/$asset"
curl --proto '=https' --tlsv1.2 -fL --retry 3 --connect-timeout 15 \
    -o "$temporary/SHA256SUMS" "$release_url/SHA256SUMS"

expected=$(awk -v asset="$asset" '$2 == asset || $2 == "*" asset { print $1; exit }' \
    "$temporary/SHA256SUMS")
[ -n "$expected" ] || { echo "error: release checksum is missing" >&2; exit 1; }
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$temporary/$asset" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$temporary/$asset" | awk '{print $1}')
else
    echo "error: sha256sum or shasum is required" >&2
    exit 1
fi
[ "$actual" = "$expected" ] || { echo "error: archive checksum does not match" >&2; exit 1; }

tar -xzf "$temporary/$asset" -C "$temporary"
for binary in vectors vectors-server; do
    [ -f "$temporary/$binary" ] || { echo "error: archive is missing $binary" >&2; exit 1; }
    chmod 755 "$temporary/$binary"
done
"$temporary/vectors" --version >/dev/null
"$temporary/vectors-server" --version >/dev/null

mkdir -p "$install_dir"
for binary in vectors vectors-server; do
    cp "$temporary/$binary" "$install_dir/$binary.new"
    chmod 755 "$install_dir/$binary.new"
    mv -f "$install_dir/$binary.new" "$install_dir/$binary"
done

if [ "$install_dir" = "$default_install_dir" ]; then
    case ":${PATH:-}:" in
        *":$install_dir:"*) ;;
        *)
            profile="$HOME/.profile"
            path_line='export PATH="$HOME/.local/bin:$PATH"'
            if ! grep -F "$path_line" "$profile" >/dev/null 2>&1; then
                printf '\n# vectors installer\n%s\n' "$path_line" >> "$profile"
                echo "Added ~/.local/bin to PATH in ~/.profile."
            fi
            ;;
    esac
fi

installed_version=$("$install_dir/vectors" --version)
echo "Installed $installed_version in $install_dir."

if [ "$start_server" -eq 0 ]; then
    echo "Run '$install_dir/vectors' for the SQL shell or '$install_dir/vectors-server' for the web console."
    exit 0
fi

data_dir="${VECTORS_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/vectors}"
state_dir="${VECTORS_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/vectors}"
snapshot="${VECTORS_SNAPSHOT:-}"
pid_file="$state_dir/server.pid"
log_file="$state_dir/server.log"
mkdir -p "$data_dir" "$state_dir"

if [ -f "$pid_file" ]; then
    old_pid=$(cat "$pid_file" 2>/dev/null || true)
    if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
        echo "vectors-server is already running with PID $old_pid."
        exit 0
    fi
    rm -f "$pid_file"
fi

if [ -n "$snapshot" ]; then
    autosave="${VECTORS_AUTOSAVE_INTERVAL_SECS:-30}"
    nohup env -u VECTORS_DATA_DIR \
        VECTORS_BIND="$bind" \
        VECTORS_SNAPSHOT="$snapshot" \
        VECTORS_AUTOSAVE_INTERVAL_SECS="$autosave" \
        "$install_dir/vectors-server" </dev/null >>"$log_file" 2>&1 &
else
    nohup env -u VECTORS_SNAPSHOT -u VECTORS_AUTOSAVE_INTERVAL_SECS \
        VECTORS_BIND="$bind" \
        VECTORS_DATA_DIR="$data_dir" \
        "$install_dir/vectors-server" </dev/null >>"$log_file" 2>&1 &
fi
server_pid=$!
printf '%s\n' "$server_pid" > "$pid_file"

port=${bind##*:}
console_url="http://127.0.0.1:$port"
attempt=0
while [ "$attempt" -lt 20 ]; do
    if curl -fsS "$console_url/healthz" 2>/dev/null | grep -q '"status":"ok"'; then
        echo "vectors-server started with PID $server_pid."
        echo "Web console: $console_url"
        if [ -n "$snapshot" ]; then
            echo "Snapshot: $snapshot"
        else
            echo "Durable data: $data_dir"
        fi
        echo "Log: $log_file"
        echo "Stop: kill \$(cat '$pid_file')"
        if [ "$open_console" -eq 1 ] && command -v xdg-open >/dev/null 2>&1 \
            && { [ -n "${DISPLAY:-}" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; }; then
            xdg-open "$console_url" >/dev/null 2>&1 &
        fi
        exit 0
    fi
    if ! kill -0 "$server_pid" 2>/dev/null; then
        echo "error: vectors-server stopped during startup; see $log_file" >&2
        rm -f "$pid_file"
        exit 1
    fi
    sleep 1
    attempt=$((attempt + 1))
done

echo "error: vectors-server did not become ready; see $log_file" >&2
kill "$server_pid" 2>/dev/null || true
rm -f "$pid_file"
exit 1
