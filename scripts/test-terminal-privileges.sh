#!/usr/bin/env bash
set -euo pipefail

workspace_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$workspace_root"

test_name="terminal::child::tests::root_switches_to_exact_non_root_credentials"

if [[ "$(id -u)" == "0" ]]; then
  exec cargo test -p nexigon-agent "$test_name" -- --ignored --exact
fi

username="$(id -un)"
uid="$(id -u)"
gid="$(id -g)"
if [[ ! -r /etc/subuid || ! -r /etc/subgid ]] || ! command -v unshare >/dev/null 2>&1; then
  echo "error: root terminal test requires root or Linux user-namespace tooling" >&2
  exit 1
fi
subuid_entry="$(awk -F: -v user="$username" '$1 == user { print; exit }' /etc/subuid)"
subgid_entry="$(awk -F: -v user="$username" '$1 == user { print; exit }' /etc/subgid)"
IFS=: read -r _ subuid subuid_count <<<"$subuid_entry"
IFS=: read -r _ subgid subgid_count <<<"$subgid_entry"

if [[ -z "${subuid:-}" || -z "${subgid:-}" || "${subuid_count:-0}" -lt 2 || "${subgid_count:-0}" -lt 2 ]]; then
  echo "error: root terminal test requires root or subordinate UID/GID ranges" >&2
  exit 1
fi

exec unshare --user \
  --map-users="0:${uid}:1" \
  --map-users="1:${subuid}:${subuid_count}" \
  --map-groups="0:${gid}:1" \
  --map-groups="1:${subgid}:${subgid_count}" \
  --setgroups allow \
  cargo test -p nexigon-agent "$test_name" -- --ignored --exact
