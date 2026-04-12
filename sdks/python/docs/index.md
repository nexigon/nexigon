# Nexigon Hub Python SDK

The `nexigon-hub-sdk` package provides a typed Python client for the
[Nexigon Hub](https://github.com/nexigon/nexigon) IoT device management API.

## Installation

```bash
pip install nexigon-hub-sdk
```

Or with [uv](https://docs.astral.sh/uv/):

```bash
uv add nexigon-hub-sdk
```

## Features

- **Typed actions** -- every API operation is a Pydantic model with full type annotations,
  giving you IDE autocompletion and static type checking out of the box.
- **Sync and async** -- [`Client`][nexigon_hub_sdk.Client] for synchronous code,
  [`AsyncClient`][nexigon_hub_sdk.AsyncClient] for `asyncio`.
- **Device commands** -- invoke on-demand commands on connected devices over WebSocket,
  with optional real-time log streaming.

## Quick example

```python
from nexigon_hub_sdk import Client
from nexigon_hub_sdk.api_types import projects

with Client("https://hub.example.com", token="your-api-token") as client:
    output = client.execute(projects.QueryProjectDevicesAction(project_id="p_..."))
    for device in output.devices:
        print(f"{device.device_id}: connected={device.is_connected}")
```

See the [Quick Start](guide/quickstart.md) guide for a more complete walkthrough.
