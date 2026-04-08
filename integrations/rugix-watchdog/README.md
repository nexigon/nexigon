# Rugix Watchdog Integration

Safety net that triggers a rollback if a system update fails to commit within a
timeout period after boot.

## How It Works

A systemd service starts on boot. The watchdog script waits until the configured
timeout has elapsed since boot, then checks whether the active partition group
matches the default. If not, the OTA script has not committed the update. The
watchdog assumes the update failed and reboots the device, which causes Rugix to
fall back to the previous known-good partition group.

This catches failures that the OTA script itself cannot handle, such as the OTA
service crashing, the agent being unavailable, or the system being in a degraded
state that prevents the commit.

## Configuration

The rollback timeout is configured in `/etc/nexigon-rugix-watchdog.conf`:

```bash
WATCHDOG_TIMEOUT=1800
```

The default is 1800 seconds (30 minutes). Adjust based on how long your system
needs to boot, run health checks, and commit.

## Dependencies

- rugix-ctrl
- nexigon-agent (optional, for emitting a warning event)
- jq

## Files

- `files/nexigon-rugix-watchdog` — install to `/usr/libexec/nexigon/`
- `files/nexigon-rugix-watchdog.conf` — install to `/etc/`
- `files/nexigon-rugix-watchdog.service` — install to `/usr/lib/systemd/system/`

## See Also

- [rugix-ota](../rugix-ota/) — the OTA update service this watchdog protects
