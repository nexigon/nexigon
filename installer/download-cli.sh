#!/bin/sh
#
# spellchecker:ignore libc, libexec

set -e

# Install the latest version of the agent by default.
VERSION=${VERSION:-"latest"}
# Indicates whether to use the MUSL version of the agent.
USE_MUSL=${USE_MUSL:-"false"}

# Install the agent.
download_cli() {
    assert_cmd curl
    assert_cmd uname
    assert_cmd chmod
    assert_cmd mv
    assert_cmd tr
    assert_cmd rm
    assert_cmd mkdir

    _cpu_type=$(uname -m)
    _os_type=$(uname -s | tr '[:upper:]' '[:lower:]')

    case "$_os_type" in
        linux)
            _target_os="unknown-linux"
            ;;
        darwin)
            _target_os="apple-darwin"
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

    _download_url="https://downloads.nexigon.dev/nexigon-cli/$VERSION/assets/$_target/nexigon-cli"

    echo "=> Downloading Nexigon CLI from:"
    echo "$_download_url"

    run_cmd curl -sSfL -H  "X-Nexigon-Download: true" "$_download_url" >./nexigon-cli
    chmod +x ./nexigon-cli
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

download_cli
