# Changelog

## Unreleased

- Add `nexigon auth status` to verify authentication and print the authenticated
  user or organization and token ID as JSON.
- Require `--organization <org-id>` for resource commands with a user token.
  Organization API tokens infer their organization and reject a conflicting
  option. `projects list` lists projects in the selected organization. Resource
  lookups outside that organization return `NotFound`. API clients can select
  an organization with the `X-Nexigon-Organization` request header.
- Replace the positional organization in `projects create <org-id> <name>` and
  `repositories create <org-id> <display-name>` with `--organization <org-id>`.
  With an organization API token, omit the option. For the Agent's local
  `repositories create` command, supply `--organization <org-id>` explicitly.
- Set a repository's path name with `repositories create --path-name` and
  `repositories set-path-name`. Within an organization, repository names in
  paths are path names.
- Add system updates with Rugix Ctrl and RAUC, enabled with
  `[integrations.rugix.system-updates]` or `[integrations.rauc.system-updates]`.
  The Agent follows the desired state in `dev.nexigon.ota.config`, reports
  progress in `dev.nexigon.ota.status`, and supports holds, maintenance
  windows, spreads, retries, and checkpoint releases. With commands enabled,
  the `nexigon.ota.check` command starts an immediate check. The Agent
  publishes the status with compare-and-swap. If the status changed during a
  check, for example because the failure count was cleared in Nexigon Hub, the
  check starts over from the changed status.
- Add app management with Rugix Apps, enabled with `[integrations.rugix.apps]`.
  The Agent installs, updates, stops, and removes the apps listed in
  `dev.nexigon.apps.config` and reports the status of all apps in
  `dev.nexigon.apps.status`. It installs the bundle for the device's
  architecture, `<app>-<arch>.rugixb` by default, and supports holds,
  maintenance windows, spreads, and retries. Outside a maintenance window, it
  installs new versions without activating them. After an update, it removes
  old versions except for the number of previously running versions that the
  `gc` policy or `gc.keep` in the integration configuration keeps, 1 by
  default. With commands enabled, the `nexigon.apps.check` command starts an
  immediate check. The integration requires a Rugix Ctrl version that supports
  `rugix-ctrl apps install --origin`. System updates and app changes do not run
  at the same time.
- Choose the asset that a device installs from each version with `asset` in
  the integration configuration or in `dev.nexigon.ota.config`. It defaults to
  the bundle of the running system, such as `<system>.rugixb`. A version
  without the asset blocks the update with the `no-asset` code.
- Define checkpoint releases with the CLI commands
  `repositories versions create --base` and `repositories versions set-base`.
- Let hosts that run the Agent in-process provide their own update system
  through the `SystemUpdater` trait. `run_with_connection` takes `RunOptions`
  instead of the readiness channel.
- Fix a stack overflow of update checks in debug builds.
- Fix a stack overflow of `actions execute` in debug builds of the CLI.
- Write 64-bit integers beyond ±(2^53 − 1) as decimal strings and non-finite
  floats as `NaN`, `+Infinity`, or `-Infinity` in JSON. Readers accept both
  forms.
- `projects devices list` lists one page of devices, newest first. Use
  `--limit` and `--cursor` to page through large projects. This requires Hub
  2026.4.0 or later, and CLI 0.6.1 and earlier cannot list devices on that Hub.
- `repositories packages versions list` lists one page of versions, newest
  first. Use `--limit` and `--cursor` to page through them and `--search` to
  find versions by name, ID, or tag. The new `repositories packages tags list`
  lists the tags of a package and their versions with the same options. Both
  require Hub 2026.4.0 or later.
- Mark the actions of the Device API, which remain backward compatible for at
  least five years, and of the Admin API, which require instance administrator
  permissions and change more often. The Rust, TypeScript, and Python API types
  document the API they belong to. The Agent invokes only Device API actions,
  except in its `repositories` commands.
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
