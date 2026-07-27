#!/usr/bin/env bash
set -euo pipefail

workspace_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
installer="$workspace_root/installer/install-agent.sh"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/nexigon-installer-security.XXXXXX")"
trap 'rm -rf -- "$scratch"' EXIT HUP INT TERM

fake_bin="$scratch/bin"
install_root="$scratch/root"
installer_tmp="$scratch/tmp"
mkdir -p "$fake_bin" "$install_root" "$installer_tmp"

cat >"$fake_bin/id" <<'EOF'
#!/usr/bin/env sh
if [ "${1:-}" = -u ]; then
    echo 0
else
    exec /usr/bin/id "$@"
fi
EOF
cat >"$fake_bin/curl" <<'EOF'
#!/usr/bin/env sh
printf '%s\n' '#!/usr/bin/env sh' 'exit 0'
EOF
cat >"$fake_bin/systemctl" <<'EOF'
#!/usr/bin/env sh
exit 0
EOF
chmod +x "$fake_bin/id" "$fake_bin/curl" "$fake_bin/systemctl"

token='deployment-token-must-never-be-printed' # NOT_A_SECRET: isolated test fixture

run_installer() {
    local output="$1"
    (
        umask 022
        PATH="$fake_bin:$PATH" \
        TMPDIR="$installer_tmp" \
        NEXIGON_INSTALL_ROOT="$install_root" \
        TOKEN="$token" \
        HUB_URL='https://hub.example.test' \
        sh "$installer"
    ) >"$output" 2>&1
}

assert_silent_and_clean() {
    local output="$1"
    if grep -Fq "$token" "$output"; then
        echo 'ERROR: agent installer printed its deployment token.' >&2
        exit 1
    fi
    if find "$installer_tmp" -mindepth 1 -print -quit | grep -q .; then
        echo 'ERROR: agent installer left a temporary credential file behind.' >&2
        exit 1
    fi
}

output="$scratch/install.out"
run_installer "$output"
config="$install_root/etc/nexigon/agent.toml"
test "$(stat -c '%a' "$config")" = 600
test "$(stat -c '%a' "$(dirname "$config")")" = 700
grep -Fq "token = \"$token\"" "$config"
assert_silent_and_clean "$output"

# An existing regular file is replaced atomically and repaired to 0600.
chmod 644 "$config"
run_installer "$scratch/reinstall.out"
test "$(stat -c '%a' "$config")" = 600
assert_silent_and_clean "$scratch/reinstall.out"

# A target symlink is rejected without changing the file it references.
rm -f "$config"
victim="$scratch/victim"
printf 'unchanged\n' >"$victim"
ln -s "$victim" "$config"
if run_installer "$scratch/symlink.out"; then
    echo 'ERROR: agent installer accepted a symlinked config destination.' >&2
    exit 1
fi
test "$(cat "$victim")" = unchanged
test -L "$config"
assert_silent_and_clean "$scratch/symlink.out"

echo 'agent installer credential tests passed'
