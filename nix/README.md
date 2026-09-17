# NixOS Agent Integration

The Nexigon flake exports `nixosModules.nexigon-agent` and
`packages.<system>.nexigon-agent`. The module configures the agent service,
persistent credentials, and optional pairing-key provisioning. It leaves device
storage and fleet policy to the consuming configuration.

## Enable the Agent

In a flake with `nixpkgs` and `nexigon` inputs:

```nix
nixosConfigurations.device = nixpkgs.lib.nixosSystem {
  modules = [
    nexigon.nixosModules.nexigon-agent
    {
      services.nexigon-agent = {
        enable = true;
        package = nexigon.packages.x86_64-linux.nexigon-agent;
        provisioning = {
          enable = true;
          openFirewall = true;
        };
      };
    }
    ./configuration.nix
  ];
};
```

Select the agent package for the device's architecture. `configuration.nix`
supplies the device platform and hardware settings. Set
`nexigon.inputs.nixpkgs.follows = "nixpkgs"` in the flake inputs to share the
package set. `nixosModules.default` aliases the agent module, and
`overlays.default` provides `pkgs.nexigon-agent` as an alternative to supplying
the package directly.

## Service Options

All options are under `services.nexigon-agent`.

| Option                       | Default                      | Purpose                                           |
| ---------------------------- | ---------------------------- | ------------------------------------------------- |
| `enable`                     | `false`                      | Start the agent.                                  |
| `package`                    | `pkgs.nexigon-agent`         | Agent package to run.                             |
| `dataPath`                   | `/var/lib/nexigon/agent`     | Persistent credentials and agent state.           |
| `settings`                   | `{}`                         | Agent configuration in the existing TOML format.  |
| `provisioning.enable`        | `false`                      | Accept pairing keys over HTTP.                    |
| `provisioning.listenAddress` | `0.0.0.0`                    | Provisioning bind address.                        |
| `provisioning.port`          | `6947`                       | Provisioning HTTP port.                           |
| `provisioning.openFirewall`  | `false`                      | Open the provisioning port in the firewall.       |
| `provisioning.endpoints`     | EU and US Nexigon Cloud Hubs | Hubs tried for pairing keys without a Hub domain. |
| `provisioning.deviceName`    | `null`                       | Optional name assigned during pairing.            |

The module uses `/etc/machine-id` as the default device fingerprint and creates
the data directory with mode `0700`. It copies the initial configuration into a
private runtime file on service startup. Use pairing-key provisioning for
credentials: values in `settings`, including tokens, are stored in the readable
Nix store.

## TCP Forwarding

Agent 0.6 disables TCP forwarding by default. To allow SSH and a local HTTP
service, configure their ports explicitly:

```nix
services.nexigon-agent.settings.forwarding = {
  enabled = true;
  allowed-tcp-ports = [ 22 80 ];
};
```

Targets are restricted to `127.0.0.1`. HTTP exports automatically authorize
their own ports, so an exported service does not also need an allowlist entry.
Both forms permit raw TCP access. `forwarding.enabled = false` disables only
additional ports; exports remain accessible. Upgrade the agent package and
its settings together; older agents do not enforce this policy.

## Test the Service

```console
nix build .#checks.x86_64-linux.nixos
```

The check boots a NixOS VM and verifies the provisioning endpoint, generated
configuration, private file and directory permissions, and state retention
after an agent service restart.
