#!/usr/bin/env bash
# Verify bootstrap argument forwarding, credential handling, and cleanup.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
installer="$repo_root/installer/install-hub.sh"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/nexigon-hub-bootstrap-test.XXXXXX")"
trap 'rm -rf -- "$scratch"' EXIT HUP INT TERM

release="$scratch/release"
fake_bin="$scratch/bin"
record="$scratch/hubctl-record"
curl_log="$scratch/curl.log"
mkdir -p "$release" "$fake_bin"

cat >"$release/nexigon-hubctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
{
    printf 'token=%s\n' "${NEXIGON_DOWNLOAD_TOKEN:-}"
    printf 'bootstrap=%s\n' "${0%/*}"
    printf 'base=%s\n' "${NEXIGON_HUB_DOWNLOADS_BASE_URL:-}"
    printf 'selector=%s\n' "${NEXIGON_HUB_RELEASE:-}"
    printf 'argument=%s\n' "$@"
} >"${FAKE_HUBCTL_RECORD:?}"
EOF
chmod 755 "$release/nexigon-hubctl"
sha256sum "$release/nexigon-hubctl" \
    | sed 's#  .*/#  #' \
    >"$release/nexigon-hubctl.sha256"
jq -S -n \
    --arg checksum "$(sha256sum "$release/nexigon-hubctl" | awk '{print $1}')" \
    --argjson size "$(stat --format '%s' "$release/nexigon-hubctl")" '
    {
        formatVersion: 1,
        releaseId: "build-2026.2.0-20260802120347-83b301b1",
        version: "2026.2.0",
        architecture: "x86_64",
        hubctl: {path: "nexigon-hubctl", sha256: $checksum, size: $size}
    }
' >"$release/release.json"

cat >"$fake_bin/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
for argument in "$@"; do
    [[ "$argument" != *test-download-token* ]]
done
config="$(cat)"
[[ "$config" = *'Authorization: Bearer test-download-token'* ]]
destination=''
url="${*: -1}"
printf '%s\n' "$url" >>"${FAKE_CURL_LOG:?}"
while [ "$#" -gt 0 ]; do
    if [ "$1" = --output ]; then
        destination="$2"
        break
    fi
    shift
done
cp "${FAKE_RELEASE:?}/${url##*/}" "$destination"
EOF
chmod 755 "$fake_bin/curl"

output="$scratch/output"
PATH="$fake_bin:$PATH" \
NEXIGON_DOWNLOAD_TOKEN=test-download-token \
FAKE_CURL_LOG="$curl_log" \
FAKE_RELEASE="$release" \
FAKE_HUBCTL_RECORD="$record" \
    "$installer" --install-dir "$scratch/install" --restore "$scratch/old.sql.gz" \
    >"$output" 2>&1

if grep -Fq test-download-token "$output"; then
    echo 'ERROR: Hub installer printed the download token.' >&2
    exit 1
fi
grep -qx 'token=test-download-token' "$record"
grep -qx 'selector=' "$record"
grep -qx 'base=https://nexigon.silitics.com/api/v1/repositories/nexigon-enterprise/nexigon-hub/build-2026.2.0-20260802120347-83b301b1/assets/x86_64' "$record"
grep -qx 'argument=install' "$record"
grep -qx 'selector=' "$record"
grep -qx 'argument=--install-dir' "$record"
grep -qx "argument=$scratch/install" "$record"
grep -qx 'argument=--restore' "$record"
grep -qx "argument=$scratch/old.sql.gz" "$record"
bootstrap="$(sed -n 's/^bootstrap=//p' "$record")"
[[ "$bootstrap" = "${TMPDIR:-/tmp}/nexigon-hub-installer."* ]]
[ ! -e "$bootstrap" ]
grep -Fxq 'https://nexigon.silitics.com/api/v1/repositories/nexigon-enterprise/nexigon-hub/stable/assets/x86_64/release.json' "$curl_log"
grep -Fxq 'https://nexigon.silitics.com/api/v1/repositories/nexigon-enterprise/nexigon-hub/build-2026.2.0-20260802120347-83b301b1/assets/x86_64/release.json' "$curl_log"

# An exact product-version selector is resolved once and then pinned to the
# same immutable release ID before hubctl starts.
: >"$curl_log"
NEXIGON_DOWNLOAD_TOKEN=test-download-token \
NEXIGON_HUB_RELEASE=2026.2.0 \
FAKE_CURL_LOG="$curl_log" \
FAKE_RELEASE="$release" \
FAKE_HUBCTL_RECORD="$record" \
PATH="$fake_bin:$PATH" \
    "$installer" --install-dir "$scratch/exact-install" >/dev/null 2>&1
grep -Fxq 'https://nexigon.silitics.com/api/v1/repositories/nexigon-enterprise/nexigon-hub/v2026.2.0/assets/x86_64/release.json' "$curl_log"
grep -qx 'argument=install' "$record"
grep -qx "argument=$scratch/exact-install" "$record"

echo 'Hub bootstrap contract tests passed'
