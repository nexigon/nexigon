#!/bin/sh
#
# spellchecker:ignore libc, libexec

set -e
umask 077

export PATH="/sbin:/usr/sbin:$PATH"

# Install the latest version of the agent by default.
VERSION=${VERSION:-"latest"}
# Indicates whether to use the MUSL version of the agent.
USE_MUSL=${USE_MUSL:-"false"}
# Hub URL to use to configure the agent.
HUB_URL=${HUB_URL:-"https://eu.nexigon.cloud"}
INSTALL_ROOT=${NEXIGON_INSTALL_ROOT:-}

if [ "$(id -u)" -eq 0 ]; then
  sudo=''
else
  sudo='sudo'
fi

_temp_dir=''
_private_temp=''

cleanup() {
    if [ -n "$_private_temp" ]; then
        $sudo rm -f "$_private_temp" >/dev/null 2>&1 || true
    fi
    if [ -n "$_temp_dir" ]; then
        rm -rf "$_temp_dir"
    fi
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

# Install the agent.
install_agent() {
    assert_cmd curl
    assert_cmd systemctl
    assert_cmd uname
    assert_cmd chmod
    assert_cmd mv
    assert_cmd mktemp
    assert_cmd tr
    assert_cmd rm
    assert_cmd mkdir
    assert_cmd cp

    _cpu_type=$(uname -m)
    _os_type=$(uname -s | tr '[:upper:]' '[:lower:]')
    _temp_dir=$(run_cmd mktemp -d)

    if [ -z "$TOKEN" ]; then
        echo "Please set the TOKEN environment variable to the deployment token."
        exit 1
    fi

    case "$_os_type" in
        linux)
            _target_os="unknown-linux"
            ;;
        *)
            echo "This script only supports Linux systems."
            exit 1
            ;;
    esac

    case "$_cpu_type" in
        arm64 | aarch64)
            _target_arch="aarch64"
            ;;
        x86_64)
            _target_arch="x86_64"
            ;;
        *)
            echo "Unsupported CPU type: $_cpu_type"
            exit 1
            ;;
    esac

    if [ "$_target_os" = "unknown-linux" ]; then
        if [ "${USE_MUSL}" = "true" ]; then
            _target_suffix="-musl"
        else
            _target_suffix="-gnu"
        fi
    else
        _target_suffix=""
    fi

    _target="$_target_arch-$_target_os$_target_suffix"

    _download_url="https://downloads.nexigon.dev/nexigon-agent/$VERSION/assets/$_target/nexigon-agent"

    echo "=> Downloading Nexigon Agent from:"
    echo "$_download_url"

    _download_file="$_temp_dir/nexigon-agent"

    run_cmd curl -sSfL -H  "X-Nexigon-Download: true" "$_download_url" > "$_download_file"

    echo "=> Installing Nexigon Agent..."
    run_cmd $sudo mkdir -p "$INSTALL_ROOT/usr/bin"
    run_cmd $sudo mv "$_download_file" "$INSTALL_ROOT/usr/bin/nexigon-agent"
    run_cmd $sudo chmod +x "$INSTALL_ROOT/usr/bin/nexigon-agent"

    echo "=> Installing Systemd service..."
    
    _service_file="$_temp_dir/nexigon-agent.service"
    cat > "$_service_file" <<EOF
[Unit]
Description=Nexigon Agent
After=network.target

[Service]
Type=exec
PIDFile=/run/nexigon-agent.pid
ExecStart=/usr/bin/nexigon-agent run
ExecStop=-/sbin/start-stop-daemon --quiet --stop --retry QUIT/5 --pidfile /run/nexigon-agent.pid
TimeoutStopSec=5
KillMode=mixed
Restart=always
RestartSec=60s

[Install]
WantedBy=multi-user.target
EOF

    run_cmd $sudo mkdir -p "$INSTALL_ROOT/etc/systemd/system"
    run_cmd $sudo mv "$_service_file" "$INSTALL_ROOT/etc/systemd/system/nexigon-agent.service"
    run_cmd $sudo systemctl daemon-reload

    echo "=> Installing device fingerprint script..."
    _fingerprint_script="$_temp_dir/nexigon-device-fingerprint"
    cat > "$_fingerprint_script" <<EOF
#!/usr/bin/env bash

set -euo pipefail

# Replace the following with a hardware-specific ID.
cat "/etc/machine-id"
EOF

    if [ ! -d "$INSTALL_ROOT/usr/libexec/nexigon" ]; then
        $sudo mkdir -p "$INSTALL_ROOT/usr/libexec/nexigon"
    fi
    run_cmd $sudo mv "$_fingerprint_script" "$INSTALL_ROOT/usr/libexec/nexigon/nexigon-device-fingerprint"
    run_cmd $sudo chmod +x "$INSTALL_ROOT/usr/libexec/nexigon/nexigon-device-fingerprint"

    echo "=> Configuring Nexigon Agent..."

    _config_file="$_temp_dir/nexigon-agent.toml"
    cat > "$_config_file" <<EOF
#:schema https://raw.githubusercontent.com/nexigon/nexigon/refs/heads/main/schemas/nexigon-agent.schema.json

hub-url = "$HUB_URL"
token = "$TOKEN"

fingerprint-script = "/usr/libexec/nexigon/nexigon-device-fingerprint"

# Remote terminal access is disabled by default. To opt in, add a [terminal]
# section with enabled = true. The terminal user defaults to "root" for
# compatibility; set user explicitly to run sessions as another account.
EOF

    install_private_file "$_config_file" "$INSTALL_ROOT/etc/nexigon/agent.toml"
    echo "=> Wrote private configuration to /etc/nexigon/agent.toml"

    echo "=> Starting Nexigon Agent..."
    run_cmd $sudo systemctl enable nexigon-agent
    run_cmd $sudo systemctl start nexigon-agent

    $sudo systemctl status nexigon-agent
}

# Atomically install a secret-bearing regular file without following a target
# symlink. The destination directory and file are restricted to the service user.
install_private_file() {
    _source=$1
    _destination=$2
    _destination_parent=${_destination%/*}

    if $sudo test -L "$_destination"; then
        bail "Refusing to replace symlink '$_destination'."
    fi
    run_cmd $sudo mkdir -p "$_destination_parent"
    run_cmd $sudo chmod 700 "$_destination_parent"
    _private_temp=$($sudo mktemp "${_destination}.tmp.XXXXXX")
    run_cmd $sudo cp "$_source" "$_private_temp"
    run_cmd $sudo chmod 600 "$_private_temp"
    if $sudo test -L "$_destination"; then
        bail "Refusing to replace symlink '$_destination'."
    fi
    run_cmd $sudo mv -T "$_private_temp" "$_destination"
    _private_temp=''
}

# Check whether a command exists.
check_cmd() {
    _cmd="$1"
    command -v "$_cmd" > /dev/null 2>&1
    return $?
}

# Assert that a command exists.
assert_cmd() {
    _cmd="$1"
    if ! check_cmd "$_cmd"; then
        bail "Command '$_cmd' not found. Please install it and rerun the script."
    fi
}

# Run a command and assert that it succeeds.
run_cmd() {
    _cmd="$1"
    shift
    $_cmd "$@"
    # shellcheck disable=SC2181
    if [ $? -ne 0 ]; then
        bail "Command '$_cmd $*' failed. Please check the output and rerun the script."
    fi
}

# Fail with the given error message.
bail() {
    _msg="$1"
    echo "$_msg" >&2
    exit 1
}

install_agent
