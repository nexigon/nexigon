# Device Commands

The SDK can invoke **on-demand commands** on connected devices over WebSocket.
This is useful for operations like triggering a firmware update, running a
diagnostic, or fetching device state in real time.

## Invoking a command

The simplest way is
[`invoke_device_command()`][nexigon_hub_sdk.Client.invoke_device_command], which
sends the command and waits for the device to finish:

=== "Sync"

    ```python
    output = client.invoke_device_command(
        "d_...",
        "reboot",
        timeout_secs=30,
    )
    print(output.status)
    ```

=== "Async"

    ```python
    output = await client.invoke_device_command(
        "d_...",
        "reboot",
        timeout_secs=30,
    )
    print(output.status)
    ```

## Passing input

Commands can accept structured JSON input:

```python
output = client.invoke_device_command(
    "d_...",
    "set-config",
    input={"wifi": {"ssid": "MyNetwork", "password": "secret"}},
    timeout_secs=10,
)
```

## Streaming logs

Set `stream_log=True` and provide an `on_log` callback to receive real-time log
output from the device as the command runs:

=== "Sync"

    ```python
    def handle_log(log_frame):
        print(f"[device] {log_frame.message}")

    output = client.invoke_device_command(
        "d_...",
        "update-firmware",
        stream_log=True,
        timeout_secs=120,
        on_log=handle_log,
    )
    ```

=== "Async"

    ```python
    async def handle_log(log_frame):
        print(f"[device] {log_frame.message}")

    output = await client.invoke_device_command(
        "d_...",
        "update-firmware",
        stream_log=True,
        timeout_secs=120,
        on_log=handle_log,
    )
    ```

## Low-level frame streaming

For full control, use
[`stream_device_command()`][nexigon_hub_sdk.Client.stream_device_command] to
iterate over raw WebSocket frames:

```python
with client.stream_device_command(
    "d_...",
    "diagnostics",
    stream_log=True,
    timeout_secs=60,
) as frames:
    for frame in frames:
        print(frame)
```

Each frame is either a `DeviceCommandDeviceFrame_Log` (intermediate log output)
or a `DeviceCommandDeviceFrame_Done` (terminal frame with status and optional
output).
