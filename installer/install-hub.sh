#!/usr/bin/env bash

set -euo pipefail
umask 077

repository_url="https://nexigon.silitics.com/api/v1/repositories/nexigon-enterprise/nexigon-hub"
base_url="${NEXIGON_HUB_DOWNLOADS_BASE_URL:-}"
requested_release="${NEXIGON_HUB_RELEASE:-stable}"
max_product_version_length=128
max_release_id_length=200
token="${NEXIGON_DOWNLOAD_TOKEN:-}"
temporary_directory=""

cleanup() {
    if [ -n "$temporary_directory" ] && [ -d "$temporary_directory" ]; then
        rm -rf -- "$temporary_directory"
    fi
}
trap cleanup EXIT HUP INT TERM

fail() {
    echo "ERROR: $*" >&2
    exit 1
}

for command in awk chmod cmp curl jq mktemp rm sha256sum uname wc; do
    command -v "$command" >/dev/null 2>&1 \
        || fail "Required command '$command' is not available."
done

[ "$(uname -s)" = Linux ] || fail "Nexigon Hub supports Linux hosts only."
[ "$(uname -m)" = x86_64 ] \
    || fail "Nexigon Hub is currently distributed only for x86_64 hosts."

if [ -z "$token" ]; then
    [ -r /dev/tty ] \
        || fail "No terminal is available; set NEXIGON_DOWNLOAD_TOKEN for this command."
    printf 'Nexigon download token: ' >/dev/tty
    IFS= read -r -s token </dev/tty
    printf '\n' >/dev/tty
fi
[[ "$token" =~ ^[A-Za-z0-9_-]+$ ]] \
    || fail "The Nexigon download token has an invalid format."

temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/nexigon-hub-installer.XXXXXX")"
hubctl="$temporary_directory/nexigon-hubctl"
selected_manifest="$temporary_directory/selected-release.json"
release_manifest="$temporary_directory/release.json"

download() {
    local source_base="$1" filename="$2" destination="$3"
    printf 'header = "Authorization: Bearer %s"\n' "$token" \
        | curl \
            --fail \
            --silent \
            --show-error \
            --location \
            --proto '=https' \
            --proto-redir '=https' \
            --config - \
            --output "$destination" \
            "${source_base%/}/$filename"
}

validate_manifest() {
    local manifest="$1" release_id version
    release_id="$(jq -er '.releaseId | select(type == "string" and length > 0)' "$manifest")" \
        || fail "The release manifest has no release ID."
    version="$(jq -er '.version | select(type == "string" and length > 0)' "$manifest")" \
        || fail "The release manifest has no product version."
    [[ "$release_id" =~ ^build-[0-9A-Za-z][0-9A-Za-z.+-]*$ ]] \
        && [[ "$release_id" = "build-$version-"* ]] \
        && [ "${#release_id}" -le "$max_release_id_length" ] \
        || fail "The release manifest has an invalid release ID."
    [[ "$version" =~ ^[0-9A-Za-z][0-9A-Za-z.+-]*$ ]] \
        && [ "${#version}" -le "$max_product_version_length" ] \
        || fail "The release manifest has an invalid product version."
    [ "$(jq -er '.formatVersion' "$manifest")" -eq 1 ] \
        && [ "$(jq -er '.architecture' "$manifest")" = x86_64 ] \
        && [ "$(jq -er '.hubctl.path' "$manifest")" = nexigon-hubctl ] \
        || fail "The release manifest is not compatible with this installer."
}

if [ -n "$base_url" ]; then
    download "$base_url" release.json "$release_manifest"
    validate_manifest "$release_manifest"
else
    if [ "$requested_release" = stable ]; then
        selector=stable
    elif [[ "$requested_release" =~ ^[0-9A-Za-z][0-9A-Za-z.+-]*$ ]] \
        && [ "${#requested_release}" -le "$max_product_version_length" ]; then
        selector="v$requested_release"
    else
        fail "NEXIGON_HUB_RELEASE must be 'stable' or a product version."
    fi
    selector_base="$repository_url/$selector/assets/x86_64"
    download "$selector_base" release.json "$selected_manifest"
    validate_manifest "$selected_manifest"
    selected_version="$(jq -er '.version' "$selected_manifest")"
    if [ "$requested_release" != stable ] \
        && [ "$selected_version" != "$requested_release" ]; then
        fail "The selected release does not match NEXIGON_HUB_RELEASE."
    fi
    release_id="$(jq -er '.releaseId' "$selected_manifest")"
    base_url="$repository_url/$release_id/assets/x86_64"
    download "$base_url" release.json "$release_manifest"
    cmp --silent "$selected_manifest" "$release_manifest" \
        || fail "The release selector changed while it was being resolved."
fi

expected_checksum="$(jq -er '.hubctl.sha256 | select(test("^[0-9a-f]{64}$"))' "$release_manifest")" \
    || fail "The release manifest has an invalid nexigon-hubctl checksum."
expected_size="$(jq -er '.hubctl.size | select(type == "number" and . > 0 and floor == .)' "$release_manifest")" \
    || fail "The release manifest has an invalid nexigon-hubctl size."
download "$base_url" nexigon-hubctl "$hubctl"
actual_checksum="$(sha256sum "$hubctl" | awk '{print $1}')"
[ "$actual_checksum" = "$expected_checksum" ] \
    || fail "The downloaded nexigon-hubctl checksum does not match."
[ "$(wc -c <"$hubctl")" -eq "$expected_size" ] \
    || fail "The downloaded nexigon-hubctl size does not match."
chmod 700 "$hubctl"

unset NEXIGON_HUB_RELEASE
export NEXIGON_DOWNLOAD_TOKEN="$token"
export NEXIGON_HUB_DOWNLOADS_BASE_URL="$base_url"
"$hubctl" install "$@"
