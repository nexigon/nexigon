# Error Handling

The SDK raises two exception types depending on where the error originates.

## API errors

[`ActionApiError`][nexigon_hub_sdk.ActionApiError] is raised when the Nexigon
Hub API returns an error response (4xx or 5xx). It carries the structured error
body and the HTTP status code:

```python
from nexigon_hub_sdk import ActionApiError
from nexigon_hub_sdk.api_types import devices

try:
    client.execute(devices.GetDeviceDetailsAction(device_id="d_nonexistent"))
except ActionApiError as exc:
    print(exc.status_code)   # e.g. 404
    print(exc.error.kind)    # error kind identifier
    print(exc.error.message) # human-readable message
```

## Command transport errors

[`CommandInvocationError`][nexigon_hub_sdk.CommandInvocationError] is raised when
a device command fails at the **transport or protocol level** -- for example, if
the WebSocket connection drops or the server sends a malformed frame.

This is distinct from a command that the device executes but reports as failed.
A failed command still returns normally via the `Done` frame with an error
status; `CommandInvocationError` means the response never arrived.

```python
from nexigon_hub_sdk import CommandInvocationError

try:
    output = client.invoke_device_command("d_...", "reboot")
except CommandInvocationError as exc:
    print(f"Transport failure: {exc}")
```

## Summary

| Exception | When |
|-----------|------|
| [`ActionApiError`][nexigon_hub_sdk.ActionApiError] | API returned an error response |
| [`CommandInvocationError`][nexigon_hub_sdk.CommandInvocationError] | WebSocket/protocol failure during a device command |
