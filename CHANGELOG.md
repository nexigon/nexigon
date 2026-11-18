# Changelog

## 0.7.0 (unreleased)

See the [release notes](releases/v0.7.0.md).

### Agent

- Add system updates with Rugix Ctrl and RAUC, enabled with
  `[integrations.rugix.system-updates]` or `[integrations.rauc.system-updates]`.
  The Agent follows the desired state in `dev.nexigon.ota.config`, reports
  progress in `dev.nexigon.ota.status`, and supports holds, maintenance
  windows, spreads, retries, and checkpoint releases. `asset` in the
  integration configuration or in `dev.nexigon.ota.config` chooses the asset
  that a device installs from each version. It defaults to the bundle of the
  running system, such as `<system>.rugixb`. A version without the asset
  blocks the update with the code `no-asset`.
- Add the Rugix Apps integration, enabled with `[integrations.rugix.apps]`.
  The Agent installs, updates, starts, stops, and removes the apps listed in
  `dev.nexigon.rugix-apps.config` and reports the status of all apps in
  `dev.nexigon.rugix-apps.status`. It installs the bundle for the device's
  architecture, `<app>-<arch>.rugixb` by default, and supports holds,
  maintenance windows, spreads, and retries. Outside a maintenance window, it
  installs new versions without activating them. An app that should run
  starts again when its workload stopped, for example after a reboot. After an
  update, the Agent removes old versions except for the number of previously
  running versions that the `gc` policy or `gc.keep` in the integration
  configuration keeps, 1 by default. An app with an invalid configuration
  stays as it is, while the other apps reach their desired states. The
  integration requires Rugix Ctrl 1.4 or later.
- Both integrations publish their status with compare-and-swap, so that a
  concurrent change, such as a failure count cleared in Nexigon Hub, starts a
  new check instead of being overwritten. A failure count resets when the
  version or the effective configuration changes. The status reports the
  reason of a failed attempt as `failureCode`, such as `rolled-back`, and the
  optional features that the integration supports as `capabilities`. System
  updates and app changes do not run at the same time.
- Truncate the error text that the Agent attaches to its own system update
  and app events to 2 KiB, since Nexigon Hub limits the size of events.
- The `nexigon.ota.check` and `nexigon.rugix-apps.check` commands start an
  immediate check whenever their integration is enabled. `[commands]` enables
  only custom commands.
- The Rugix Apps integration offers the `nexigon.rugix-apps.start`, `stop`,
  `rollback`, and `remove` commands for apps that the configuration does not
  manage. They take the same input as the commands of the earlier
  `nexigon-rugix-apps` recipes and reject managed apps, whose state the
  configuration determines.
- The property formats of both integrations are stable. Later versions only add
  optional fields, states, capabilities, and codes, which readers accept as
  unknown values, so that an Agent can read a status that a newer Agent wrote,
  for example after a rollback to an older system. Each property has a
  `formatVersion`, 1 when absent, which a change increments if earlier readers
  must not interpret it. The Agent does not apply a configuration with a later
  format version and reports the blocked code `unsupported-config-format`.
- Stopping the Agent interrupts a system update or app installation in
  progress, and the next check reports it as a failed attempt. RAUC finishes
  an interrupted installation in the background, so the next check waits for
  it and keeps the running system active. A lost connection or a hold lets an
  installation finish. The Agent handles SIGTERM,
  so service managers stop it gracefully. Calls of `rugix-ctrl`, `rauc`, and
  `systemctl` fail after 10 minutes, except installations, which include the
  download.
- The systemd unit of the installer gives the Agent 30 seconds to stop and no
  longer refers to a PID file.
- The NixOS module puts Rugix Ctrl and RAUC on the service path when the
  configuration enables their integrations.
- Hosts that run the Agent in-process can provide their own update system
  through the `SystemUpdater` trait. `run_with_connection` takes `RunOptions`
  instead of the readiness channel.
- Reconnect to Nexigon Hub when the connection is lost or cannot be
  established, instead of exiting. Before each attempt after the first, the
  Agent waits for a random delay of up to 30 seconds, doubling after each
  failed attempt up to 10 minutes. A `Retry-After` header of Nexigon Hub
  extends the delay by up to 10 minutes. Configure the delays and the connect
  timeout in the new `[connection]` section. Terminals, TCP forwarding, and
  command handler channels end with the connection. The system update and app
  integrations, operation polling, and the local API keep running, and a
  command step of an operation keeps running through a lost connection.
- Add `nexigon-agent status`, which prints the connection state and device ID
  of the running Agent as JSON, and the `status` endpoint of the local API.
  While the Agent is not connected, the local API rejects the `executor`
  endpoint with the new `HubUnavailable` error code.
- The Agent's `device`, `events`, and `repositories` commands use the
  connection of a running Agent through the local API and fail while it is not
  connected to Nexigon Hub. Without a running Agent, they connect on their own
  as before. They use the socket path from `[local-api]` in the configuration.
  The local `repositories create` command requires `--organization <org-id>`.
- Authenticate the Agent with a device private key held by hardware, such as a
  TPM, a PKCS#11 token, or the Raspberry Pi firmware, instead of a key file.
  Configure a signer script for the key in the new `[signer]` section. The
  Agent obtains the public key from the script and has it sign the device
  certificate and each TLS handshake. With a signer, the Agent does not
  generate a key file.
- The Agent invokes only Device API actions, except in its `repositories`
  commands.

### CLI

- Add `nexigon-cli auth status` to verify authentication and print the
  authenticated user or organization and token ID as JSON.
- Require `--organization <org-id>` for resource commands with a user token.
  Organization API tokens infer their organization and reject a conflicting
  option. `projects list` lists projects in the selected organization.
  Resource lookups outside that organization return `NotFound`.
- Replace the positional organization in `projects create <org-id> <name>` and
  `repositories create <org-id> <display-name>` with `--organization <org-id>`.
  With an organization API token, omit the option.
- Set a repository's path name with `repositories create --path-name` and
  `repositories set-path-name`. Within an organization, repository names in
  paths are path names.
- Define checkpoint releases with `repositories versions create --base` and
  `repositories versions set-base`.
- `projects devices list` lists one page of devices, newest first. Use
  `--limit` and `--cursor` to page through large projects. This requires Hub
  2026.4.0 or later, and CLI 0.6.1 and earlier cannot list devices on that Hub.
- `repositories packages versions list` lists one page of versions, newest
  first. Use `--limit` and `--cursor` to page through them and `--search` to
  find versions by name, ID, or tag. The new `repositories packages tags list`
  lists the tags of a package and their versions with the same options. Both
  require Hub 2026.4.0 or later.
- Fix a stack overflow of `actions execute` in debug builds.

### API

- API clients can select an organization with the `X-Nexigon-Organization`
  request header.
- Mark the actions of the Device API, which remain backward compatible for at
  least five years, and of the Admin API, which require instance administrator
  permissions and change more often. The Rust, TypeScript, and Python API
  types document the API they belong to.
- Add the Admin API actions `users_Deactivate`, `users_Reactivate`,
  `organizations_Deactivate`, and `organizations_Reactivate`. User and
  organization queries return a `deactivation` field for deactivated accounts
  and organizations.
- Assign device profiles in the modes `latest`, `pinned`, or `rollout`, and roll
  out profile revisions in stages with `profiles_StartRollout` and related
  actions. `devices_SetProfiles`, `devices_UpdateProfiles`, and
  `devices_GetDetails` take or return profile assignments instead of profile
  IDs.
- Redesign fleet operations. Selectors, step conditions, and waits take
  expressions, and targets are released in stages with guards, spreads, and
  soak times. `fleet_HaltOperation`, `fleet_ResumeOperation`,
  `fleet_ReleaseOperationStage`, and `fleet_CloseOperation` replace
  `fleet_PauseOperation` and `fleet_AdvanceOperationStage`. The `DeviceTask`
  step is removed. Agent 0.6 keeps running the commands of fleet operations.
- Remove the unstable `vulnerabilities_*` actions. Nexigon Hub indexes SBOMs
  linked through `relations.sbom` automatically and manages vulnerabilities in
  its web interface. `relations.vdr` and `relations.vex` are no longer indexed;
  import VEX documents for a package instead.
- Document the size limits of Nexigon Hub on the affected API fields. By
  default, Nexigon Hub rejects property values, profile values, step values,
  command inputs, metadata, and property schemas above 64 KiB, events above
  4 KiB, batches of more than 1000 events, and device token claims above 4 KiB.
  Add `omittedOutputBytes` to the state of operation steps, which holds the size
  of a command output that exceeded the limit and was not stored.
- Write 64-bit integers beyond ±(2^53 − 1) as decimal strings and non-finite
  floats as `NaN`, `+Infinity`, or `-Infinity` in JSON. Readers accept both
  forms.
- The Python API types represent variant cases without a payload as strings,
  such as `"Forbidden"`, instead of classes, such as
  `ActionErrorKind_Forbidden`.

### Dependencies

- Update Sidex to 0.6.1.

- Update Tokio to 1.54.1 for RUSTSEC-2026-0386, which reports that
  `AsyncFd::new` violates I/O safety.

## 0.6.1 (2026-10-21)

- Verify all HTTPS connections against the operating system's trust store.
  Pairing-key provisioning, repository asset uploads, and OTLP export previously
  also trusted roots compiled into the binary, including
  `emSign ECC Root CA - G3`, to which the compromised eMudhra intermediates
  chain.
- Reject certificate chains containing the compromised eMudhra intermediate keys
  by updating `rustls-webpki` to 0.103.16.
- Update Rustls to 0.23.46.

See the [release notes](releases/v0.6.1.md).

## 0.6.0 (2026-09-18)

- Add optional pairing-key provisioning on port 6947 and persist provisioned
  credentials.
- Add a Nix package and NixOS service module for the Agent.
- Harden remote terminal setup by dropping and verifying all credentials before
  execution and by supplying a minimal environment. Fix terminal startup when
  the Agent runs as the target non-root user without supplementary groups.
- Require an explicit TCP forwarding policy. HTTP exports permit their own
  ports, and devices can opt in to forwarding all ports.
- Increase the default multiplex capacity to 512 channels and announce usable
  forwarding capacity to compatible Hubs, addressing proxy 502s under bursty
  workloads when used with Hub 2026.3.2.
- Make multiplex channel capacity and the channel-open rate configurable.
- Reject excess channel-open bursts without disconnecting the Agent.
- Replace `dangerous-disable-tls` with separate opt-ins for plaintext transport
  and invalid HTTPS certificates.
- Allow the CLI to authenticate with organization API tokens and add the Hub
  installer with exact, stable, and rolling release selection.
- Publish checksums and CycloneDX SBOMs with GitHub release assets.

See the [release notes and migration instructions](releases/v0.6.0.md).
