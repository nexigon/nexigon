# Rugix OTA Integration

Automated over-the-air system updates using Rugix's A/B partition scheme.

## How It Works

A systemd timer periodically runs a reconciliation script that:

1. **Commits** the system if booted into a non-default partition group (post-update reboot).
2. **Detects stale state** if a previous update was marked active but the version didn't change.
3. **Checks for updates** by comparing the current version against the target version in a Nexigon repository.
4. **Installs the update** by downloading the bundle via a signed URL and calling `rugix-ctrl update install`.
5. **Reboots** into the updated partition group.

The script derives state from the system on each run rather than trusting persisted
state. The OTA status property (`dev.nexigon.ota.status`) is published for
observability but is not used as the source of truth.

On unexpected failure, the script exits and the next timer tick retries.
The [rugix-watchdog](../rugix-watchdog/) provides the hard timeout for rollback.

## Configuration

The default configuration is in `/etc/nexigon-rugix-ota.json`:

```json
{"path": "your/repository/path"}
```

Override per-device by setting the `dev.nexigon.ota.config` property to a JSON
object with the same shape. The device property is merged on top of the file-based
defaults.

## Commands

| Command | Description |
|---|---|
| `nexigon.rugix-ota.check` | Trigger an immediate OTA update check |

The check command triggers the systemd service directly via `systemctl start`.
It returns immediately — the actual update runs in the background as a systemd unit.

## Dependencies

- rugix-ctrl
- nexigon-agent
- jq

## Files

- `cmds/check.toml` — install to `/etc/nexigon/agent/commands/`
- `files/nexigon-rugix-ota` — install to `/usr/bin/`
- `files/nexigon-rugix-ota.json` — install to `/etc/`
- `files/nexigon-rugix-ota.service` — install to `/usr/lib/systemd/system/`
- `files/nexigon-rugix-ota.timer` — install to `/usr/lib/systemd/system/`

## See Also

- [rugix-watchdog](../rugix-watchdog/) — auto-rollback safety net for failed updates
