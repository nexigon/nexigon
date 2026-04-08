# Rugix Apps Integration

Remote commands for managing Rugix Apps (container-like application bundles).

## Commands

| Command | Description |
|---|---|
| `nexigon.rugix-apps.list` | List installed apps |
| `nexigon.rugix-apps.info` | Get details of an app |
| `nexigon.rugix-apps.start` | Start an app |
| `nexigon.rugix-apps.stop` | Stop an app |
| `nexigon.rugix-apps.rollback` | Rollback an app to its previous generation |
| `nexigon.rugix-apps.remove` | Remove an app |
| `nexigon.rugix-apps.deploy` | Deploy an app from a Nexigon repository package version |

Most commands are thin wrappers around `rugix-ctrl apps <subcommand>`, dispatched
by a single handler script.

The `deploy` command is more involved: it resolves a package version via the
`nexigon-agent` CLI, finds the `.rugixb` asset, issues a download URL, and passes
it to `rugix-ctrl apps install`.

## Dependencies

- rugix-ctrl
- nexigon-agent (for deploy)
- jq

## Files

- `cmds/*.toml` — install to `/etc/nexigon/agent/commands/`
- `files/nexigon-rugix-apps` — install to `/usr/libexec/nexigon/`
- `files/nexigon-rugix-apps-deploy` — install to `/usr/libexec/nexigon/`
