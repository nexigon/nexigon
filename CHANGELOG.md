# Changelog

## 0.6.0 (Unreleased)

- Require an explicit TCP forwarding policy. HTTP exports permit their own
  ports, and devices can opt in to forwarding all ports.
- Increase the default multiplex capacity to 512 channels and announce usable
  forwarding capacity to compatible Hubs.
- Make multiplex channel capacity and the channel-open rate configurable.
- Reject excess channel-open bursts without disconnecting the Agent.

See the [release notes and migration instructions](releases/v0.6.0.md).
