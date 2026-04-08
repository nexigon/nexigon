# Systemd Integration

Remote commands for managing systemd services.

## Commands

| Command                      | Description                                  |
| ---------------------------- | -------------------------------------------- |
| `nexigon.systemd.list-units` | List all systemd units as JSON               |
| `nexigon.systemd.restart`    | Restart a systemd unit by name               |
| `nexigon.systemd.status`     | Get `systemctl show` output as a JSON object |

## Dependencies

- systemd
- jq

## Files

- `cmds/*.toml` — install to `/etc/nexigon/agent/commands/`
- `scripts/*` — install to `/usr/libexec/nexigon/`
