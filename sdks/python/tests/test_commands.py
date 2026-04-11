import json
import threading
from collections.abc import Awaitable, Callable

import pytest
import websockets
import websockets.asyncio.server
import websockets.sync.server

from nexigon_hub_sdk import AsyncClient, Client, CommandInvocationError
from nexigon_hub_sdk._commands import (
    build_command_ws_url,
    build_invoke_frame,
    parse_device_frame,
)
from nexigon_hub_sdk.api_types.devices import (
    DeviceCommandDeviceFrame_Done,
    DeviceCommandDeviceFrame_Log,
    DeviceCommandStatus_Ok,
)

_TOKEN = "test-token"
_DEVICE_ID = "d_9JTb4Drw5fbVWpoqM8fwvg"


def test_build_command_ws_url_http() -> None:
    """HTTP base URLs map to ws:// and include the device id."""
    assert (
        build_command_ws_url("http://host:8180", _DEVICE_ID)
        == f"ws://host:8180/api/v1/connect/command/{_DEVICE_ID}"
    )


def test_build_command_ws_url_https_strips_existing_path() -> None:
    """HTTPS base URLs map to wss:// and existing paths are replaced."""
    assert (
        build_command_ws_url("https://hub.example.com/some/prefix", _DEVICE_ID)
        == f"wss://hub.example.com/api/v1/connect/command/{_DEVICE_ID}"
    )


def test_build_invoke_frame_uses_camel_case() -> None:
    """The serialized Invoke frame matches the server's camelCase schema."""
    text = build_invoke_frame(
        "reboot",
        input={"force": True},
        stream_log=True,
        timeout_secs=30,
    )
    payload = json.loads(text)
    assert payload == {
        "type": "Invoke",
        "command": "reboot",
        "input": {"force": True},
        "streamLog": True,
        "timeoutSecs": 30,
    }


def test_parse_device_frame_discriminates_log_and_done() -> None:
    """The device frame type adapter discriminates on the `type` tag."""
    log = parse_device_frame(json.dumps({"type": "Log", "lines": ["hello"]}))
    assert isinstance(log, DeviceCommandDeviceFrame_Log)
    assert log.lines == ["hello"]

    done = parse_device_frame(
        json.dumps(
            {
                "type": "Done",
                "status": "Ok",
                "output": {"exit_code": 0},
                "logTail": ["tail"],
                "durationMs": 42,
            }
        )
    )
    assert isinstance(done, DeviceCommandDeviceFrame_Done)
    assert isinstance(done.status, DeviceCommandStatus_Ok)
    assert done.duration_ms == 42
    assert done.log_tail == ["tail"]


def test_parse_device_frame_rejects_malformed() -> None:
    """Malformed frames raise CommandInvocationError."""
    with pytest.raises(CommandInvocationError):
        parse_device_frame(json.dumps({"type": "Nonsense"}))


def test_sync_invoke_device_command_end_to_end() -> None:
    """Client.invoke_device_command exchanges frames with a real WebSocket server."""
    received_invoke: list[dict[str, object]] = []

    def handler(ws: websockets.sync.server.ServerConnection) -> None:
        assert ws.request is not None
        assert ws.request.path == f"/api/v1/connect/command/{_DEVICE_ID}"
        assert ws.request.headers["Authorization"] == f"Bearer {_TOKEN}"
        message = ws.recv()
        assert isinstance(message, str)
        received_invoke.append(json.loads(message))
        ws.send(json.dumps({"type": "Log", "lines": ["starting"]}))
        ws.send(json.dumps({"type": "Log", "lines": ["working"]}))
        ws.send(
            json.dumps(
                {
                    "type": "Done",
                    "status": "Ok",
                    "output": {"result": "done"},
                    "logTail": ["starting", "working"],
                    "durationMs": 123,
                }
            )
        )

    with _sync_ws_server(handler) as base_url:
        logs: list[DeviceCommandDeviceFrame_Log] = []
        with Client(base_url, token=_TOKEN) as client:
            done = client.invoke_device_command(
                _DEVICE_ID,
                "reboot",
                input={"force": True},
                stream_log=True,
                on_log=logs.append,
            )

    assert received_invoke == [
        {
            "type": "Invoke",
            "command": "reboot",
            "input": {"force": True},
            "streamLog": True,
            "timeoutSecs": None,
        }
    ]
    assert [log.lines for log in logs] == [["starting"], ["working"]]
    assert isinstance(done, DeviceCommandDeviceFrame_Done)
    assert isinstance(done.status, DeviceCommandStatus_Ok)
    assert done.output == {"result": "done"}
    assert done.duration_ms == 123


def test_sync_stream_device_command_yields_all_frames() -> None:
    """stream_device_command surfaces Log frames and terminates on Done."""

    def handler(ws: websockets.sync.server.ServerConnection) -> None:
        ws.recv()
        ws.send(json.dumps({"type": "Log", "lines": ["one"]}))
        ws.send(
            json.dumps(
                {
                    "type": "Done",
                    "status": "Error",
                    "error": "boom",
                    "logTail": ["one"],
                    "durationMs": 5,
                }
            )
        )

    with _sync_ws_server(handler) as base_url:
        with Client(base_url, token=_TOKEN) as client:
            with client.stream_device_command(_DEVICE_ID, "noop") as frames:
                collected = list(frames)

    assert len(collected) == 2
    assert isinstance(collected[0], DeviceCommandDeviceFrame_Log)
    assert isinstance(collected[1], DeviceCommandDeviceFrame_Done)


def test_sync_invoke_raises_when_closed_before_done() -> None:
    """A connection closing before Done raises CommandInvocationError."""

    def handler(ws: websockets.sync.server.ServerConnection) -> None:
        ws.recv()
        ws.send(json.dumps({"type": "Log", "lines": ["partial"]}))

    with _sync_ws_server(handler) as base_url:
        with Client(base_url, token=_TOKEN) as client:
            with pytest.raises(CommandInvocationError):
                client.invoke_device_command(_DEVICE_ID, "noop")


async def test_async_invoke_device_command_end_to_end() -> None:
    """AsyncClient.invoke_device_command exchanges frames with an async server."""
    received_invoke: list[dict[str, object]] = []

    async def handler(ws: websockets.asyncio.server.ServerConnection) -> None:
        assert ws.request is not None
        assert ws.request.headers["Authorization"] == f"Bearer {_TOKEN}"
        message = await ws.recv()
        assert isinstance(message, str)
        received_invoke.append(json.loads(message))
        await ws.send(json.dumps({"type": "Log", "lines": ["async-log"]}))
        await ws.send(
            json.dumps(
                {
                    "type": "Done",
                    "status": "Ok",
                    "logTail": ["async-log"],
                    "durationMs": 7,
                }
            )
        )

    async with _async_ws_server(handler) as base_url:
        logs: list[DeviceCommandDeviceFrame_Log] = []

        async def on_log(frame: DeviceCommandDeviceFrame_Log) -> None:
            logs.append(frame)

        async with AsyncClient(base_url, token=_TOKEN) as client:
            done = await client.invoke_device_command(
                _DEVICE_ID,
                "diagnose",
                on_log=on_log,
            )

    assert received_invoke[0]["command"] == "diagnose"
    assert [log.lines for log in logs] == [["async-log"]]
    assert isinstance(done.status, DeviceCommandStatus_Ok)


async def test_async_stream_device_command_yields_all_frames() -> None:
    """The async streaming iterator surfaces all frames up to Done."""

    async def handler(ws: websockets.asyncio.server.ServerConnection) -> None:
        await ws.recv()
        await ws.send(json.dumps({"type": "Log", "lines": ["a"]}))
        await ws.send(json.dumps({"type": "Log", "lines": ["b"]}))
        await ws.send(
            json.dumps(
                {
                    "type": "Done",
                    "status": "Ok",
                    "logTail": ["a", "b"],
                    "durationMs": 1,
                }
            )
        )

    async with _async_ws_server(handler) as base_url:
        async with AsyncClient(base_url, token=_TOKEN) as client:
            collected: list[object] = []
            async with client.stream_device_command(_DEVICE_ID, "noop") as frames:
                async for frame in frames:
                    collected.append(frame)

    assert len(collected) == 3
    assert isinstance(collected[-1], DeviceCommandDeviceFrame_Done)


_SyncHandler = Callable[[websockets.sync.server.ServerConnection], None]
_AsyncHandler = Callable[[websockets.asyncio.server.ServerConnection], Awaitable[None]]


class _SyncServerContext:
    """Context manager for a temporary sync WebSocket server running in a thread."""

    def __init__(self, handler: _SyncHandler) -> None:
        self._handler = handler

    def __enter__(self) -> str:
        self._server = websockets.sync.server.serve(self._handler, "127.0.0.1", 0)
        host, port = self._server.socket.getsockname()[:2]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()
        return f"http://{host}:{port}"

    def __exit__(self, *args: object) -> None:
        self._server.shutdown()
        self._thread.join(timeout=5)


def _sync_ws_server(handler: _SyncHandler) -> _SyncServerContext:
    return _SyncServerContext(handler)


class _AsyncServerContext:
    """Async context manager for a temporary asyncio WebSocket server."""

    def __init__(self, handler: _AsyncHandler) -> None:
        self._handler = handler

    async def __aenter__(self) -> str:
        self._server = await websockets.asyncio.server.serve(
            self._handler, "127.0.0.1", 0
        )
        host, port = self._server.sockets[0].getsockname()[:2]
        return f"http://{host}:{port}"

    async def __aexit__(self, *args: object) -> None:
        self._server.close()
        await self._server.wait_closed()


def _async_ws_server(handler: _AsyncHandler) -> _AsyncServerContext:
    return _AsyncServerContext(handler)
