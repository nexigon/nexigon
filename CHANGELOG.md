# Changelog

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
