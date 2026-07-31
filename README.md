<h1 align="center">
    Nexigon
</h1>
<h4 align="center">
    Fleet Management for Connected Devices
</h4>
<p align="center">
  <a href="https://github.com/nexigon/nexigon/actions"><img alt="Pipeline Status Badge" src="https://img.shields.io/github/actions/workflow/status/nexigon/nexigon/pipeline.yml"></a>
</p>

[Nexigon](https://nexigon.dev) is a fleet management platform for connected devices. This repository contains its open-source components:

- [**Nexigon Agent**](crates/apps/nexigon-agent): On-device agent that connects devices to the Nexigon platform.
- [**Nexigon CLI**](crates/apps/nexigon-cli): Command-line interface for managing devices, projects, and deployments.
- [**Rust SDK**](crates/libs): Client libraries and API bindings for integrating with Nexigon.

## Getting Started

For documentation and guides, visit [docs.nexigon.dev](https://docs.nexigon.dev).

## CLI Authentication

The Nexigon CLI accepts a personal user token or a fine-grained organization API
token in the same `token` configuration field:

```toml
hub-url = "https://nexigon.example.com"
token = "org_sk_..."
```

Prefer an organization API token for automation. An artifact publisher needs two
policy statements scoped to its target repository: repository `view` and
`manage_assets`, plus package `view` and `manage_versions`. It does not need
organization, project, device, repository-settings, or package-management
permissions when the destination package already exists.

## Security

For information about reporting security vulnerabilities, see [SECURITY.md](SECURITY.md).

## Licensing

This project is licensed under either [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project by you, as defined in the Apache 2.0 license, shall be dual licensed as above, without any additional terms or conditions.

---

Made with ❤️ by [Silitics](https://www.silitics.com)
