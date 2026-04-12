# Quick Start

## Creating a client

Both [`Client`][nexigon_hub_sdk.Client] and
[`AsyncClient`][nexigon_hub_sdk.AsyncClient] accept a base URL and an API token.
They work as context managers that close the underlying HTTP connection on exit.

=== "Sync"

    ```python
    from nexigon_hub_sdk import Client

    with Client("https://hub.example.com", token="your-api-token") as client:
        ...
    ```

=== "Async"

    ```python
    from nexigon_hub_sdk import AsyncClient

    async with AsyncClient("https://hub.example.com", token="your-api-token") as client:
        ...
    ```

## Executing actions

All API operations are represented as **action** objects. Import the action
class, construct it with the required parameters, and pass it to
`client.execute()`:

```python
from nexigon_hub_sdk.api_types import devices

output = client.execute(devices.GetDeviceDetailsAction(device_id="d_..."))
print(output.name)
print(output.is_connected)
```

Every `execute()` call is fully typed -- your IDE knows the exact return type
based on the action you pass in.

## Listing resources

Many actions follow a query/list pattern and return a wrapper with a list field:

```python
from nexigon_hub_sdk.api_types import projects

output = client.execute(projects.QueryProjectDevicesAction(project_id="p_..."))
for device in output.devices:
    print(f"{device.device_id}: connected={device.is_connected}")
```

## Working with device properties

```python
from nexigon_hub_sdk.api_types import devices

# Set a property on a device
client.execute(devices.SetDevicePropertyAction(
    device_id="d_...",
    key="firmware-version",
    value="2.1.0",
))

# Read it back
output = client.execute(devices.GetDevicePropertyAction(
    device_id="d_...",
    key="firmware-version",
))
print(output.value)
```

## Issuing remote access URLs

Generate a short-lived HTTP proxy token for direct access to a connected device:

```python
from nexigon_hub_sdk.api_types import devices

output = client.execute(devices.IssueDeviceHttpProxyTokenAction(
    device_id="d_...",
))
print(output.token)
```

## Async usage

[`AsyncClient`][nexigon_hub_sdk.AsyncClient] has the same API surface -- just
`await` the calls:

```python
from nexigon_hub_sdk import AsyncClient
from nexigon_hub_sdk.api_types import projects

async with AsyncClient("https://hub.example.com", token="your-api-token") as client:
    output = await client.execute(
        projects.QueryProjectDevicesAction(project_id="p_...")
    )
    for device in output.devices:
        print(f"{device.device_id}: connected={device.is_connected}")
```
