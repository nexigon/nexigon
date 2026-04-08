# System Integration

Remote commands for system power management.

## Commands

| Command                   | Description                                   |
| ------------------------- | --------------------------------------------- |
| `nexigon.system.reboot`   | Reboot the system via `systemctl reboot`      |
| `nexigon.system.shutdown` | Shut down the system via `systemctl poweroff` |

## Dependencies

- systemd

## Files

- `cmds/*.toml` — install to `/etc/nexigon/agent/commands/`
