from __future__ import annotations

from contextlib import asynccontextmanager, contextmanager
from typing import TYPE_CHECKING, AsyncIterator, Iterator
from urllib.parse import urlsplit, urlunsplit

import pydantic
import websockets
import websockets.asyncio.client
import websockets.asyncio.connection
import websockets.exceptions
import websockets.sync.client
import websockets.sync.connection

from ._errors import CommandInvocationError
from .api_types.devices import (
    DeviceCommandDeviceFrame,
    DeviceCommandDeviceFrame_Done,
    DeviceCommandDeviceFrame_Log,
    DeviceCommandHubFrame_Invoke,
    DeviceCommandInvokeData,
)
from .api_types.json import JsonValue

if TYPE_CHECKING:
    from .api_types.devices import DeviceId

_DEVICE_FRAME_ADAPTER: pydantic.TypeAdapter[DeviceCommandDeviceFrame] = (
    pydantic.TypeAdapter(DeviceCommandDeviceFrame)
)


def build_command_ws_url(base_url: str, device_id: DeviceId | str) -> str:
    """Return the WebSocket URL for invoking commands on `device_id`."""
    parts = urlsplit(base_url)
    if parts.scheme == "https":
        ws_scheme = "wss"
    elif parts.scheme == "http":
        ws_scheme = "ws"
    else:
        raise ValueError(f"unsupported base URL scheme: {parts.scheme!r}")
    path = f"/api/v1/connect/command/{device_id}"
    return urlunsplit((ws_scheme, parts.netloc, path, "", ""))


def build_invoke_frame(
    command: str,
    *,
    input: JsonValue = None,
    stream_log: bool = False,
    timeout_secs: int | None = None,
) -> str:
    """Serialize a `DeviceCommandHubFrame_Invoke` to a JSON string."""
    frame = DeviceCommandHubFrame_Invoke(
        command=command,
        input=input,
        stream_log=stream_log,
        timeout_secs=timeout_secs,
    )
    return frame.model_dump_json(by_alias=True)


def parse_device_frame(text: str) -> DeviceCommandDeviceFrame:
    """Parse a JSON message from the server into a `DeviceCommandDeviceFrame`."""
    try:
        return _DEVICE_FRAME_ADAPTER.validate_json(text)
    except pydantic.ValidationError as error:
        raise CommandInvocationError(
            f"received malformed device command frame: {error}"
        ) from error


def auth_headers(token: str) -> dict[str, str]:
    """Build the HTTP headers used for authenticating WebSocket upgrade requests."""
    return {"Authorization": f"Bearer {token}"}


@contextmanager
def stream_command_sync(
    base_url: str,
    token: str,
    device_id: DeviceId | str,
    command: str,
    *,
    input: JsonValue = None,
    stream_log: bool = False,
    timeout_secs: int | None = None,
) -> Iterator[Iterator[DeviceCommandDeviceFrame]]:
    """Open a synchronous WebSocket and yield device command frames until `Done`."""
    url = build_command_ws_url(base_url, device_id)
    invoke = build_invoke_frame(
        command,
        input=input,
        stream_log=stream_log,
        timeout_secs=timeout_secs,
    )
    try:
        with websockets.sync.client.connect(
            url, additional_headers=auth_headers(token)
        ) as ws:
            ws.send(invoke)
            yield _iter_sync_frames(ws)
    except websockets.exceptions.WebSocketException as error:
        raise CommandInvocationError(f"websocket error: {error}") from error


@asynccontextmanager
async def stream_command_async(
    base_url: str,
    token: str,
    device_id: DeviceId | str,
    command: str,
    *,
    input: JsonValue = None,
    stream_log: bool = False,
    timeout_secs: int | None = None,
) -> AsyncIterator[AsyncIterator[DeviceCommandDeviceFrame]]:
    """Open an async WebSocket and yield device command frames until `Done`."""
    url = build_command_ws_url(base_url, device_id)
    invoke = build_invoke_frame(
        command,
        input=input,
        stream_log=stream_log,
        timeout_secs=timeout_secs,
    )
    try:
        async with websockets.asyncio.client.connect(
            url, additional_headers=auth_headers(token)
        ) as ws:
            await ws.send(invoke)
            yield _iter_async_frames(ws)
    except websockets.exceptions.WebSocketException as error:
        raise CommandInvocationError(f"websocket error: {error}") from error


def _iter_sync_frames(
    ws: websockets.sync.connection.Connection,
) -> Iterator[DeviceCommandDeviceFrame]:
    """Yield frames from a sync WebSocket until `Done` or close."""
    try:
        for message in ws:
            if not isinstance(message, str):
                continue
            frame = parse_device_frame(message)
            yield frame
            if isinstance(frame, DeviceCommandDeviceFrame_Done):
                return
    except websockets.exceptions.WebSocketException as error:
        raise CommandInvocationError(f"websocket error: {error}") from error


async def _iter_async_frames(
    ws: websockets.asyncio.connection.Connection,
) -> AsyncIterator[DeviceCommandDeviceFrame]:
    """Yield frames from an async WebSocket until `Done` or close."""
    try:
        async for message in ws:
            if not isinstance(message, str):
                continue
            frame = parse_device_frame(message)
            yield frame
            if isinstance(frame, DeviceCommandDeviceFrame_Done):
                return
    except websockets.exceptions.WebSocketException as error:
        raise CommandInvocationError(f"websocket error: {error}") from error


__all__ = [
    "DeviceCommandDeviceFrame_Done",
    "DeviceCommandDeviceFrame_Log",
    "DeviceCommandInvokeData",
    "build_command_ws_url",
    "build_invoke_frame",
    "parse_device_frame",
    "stream_command_async",
    "stream_command_sync",
]
