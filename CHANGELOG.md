# Changelog

## 0.6.0 (Unreleased)

- Require explicitly permitted TCP forwarding ports. HTTP exports permit their
  own ports.
- Increase the default multiplex capacity to 512 channels and announce usable
  forwarding capacity to compatible Hubs.
- Make multiplex channel capacity and the channel-open rate configurable.
- Reject excess channel-open bursts without disconnecting the Agent.

See the [release notes and migration instructions](releases/v0.6.0.md).
