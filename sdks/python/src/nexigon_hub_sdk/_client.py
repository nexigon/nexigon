from __future__ import annotations

import inspect
from contextlib import AbstractAsyncContextManager, AbstractContextManager
from typing import (
    AsyncIterator,
    Awaitable,
    Callable,
    Iterator,
    NoReturn,
    cast,
)

import httpx
import pydantic

from ._actions import _ACTION_REGISTRY, _AsyncExecuteMixin, _SyncExecuteMixin
from ._commands import stream_command_async, stream_command_sync
from ._errors import ActionApiError, CommandInvocationError
from .api_types.devices import (
    DeviceCommandDeviceFrame,
    DeviceCommandDeviceFrame_Done,
    DeviceCommandDeviceFrame_Log,
    DeviceCommandDoneData,
    DeviceId,
)
from .api_types.errors import ActionError
from .api_types.json import JsonValue

_ACTION_ERROR_ADAPTER = pydantic.TypeAdapter(ActionError)

_SyncLogHandler = Callable[[DeviceCommandDeviceFrame_Log], None]
_AsyncLogHandler = Callable[[DeviceCommandDeviceFrame_Log], None | Awaitable[None]]


def _raise_api_error(response: httpx.Response) -> NoReturn:
    error = _ACTION_ERROR_ADAPTER.validate_json(response.content)
    raise ActionApiError(error, response.status_code)


class Client(_SyncExecuteMixin):
    """Synchronous Nexigon Hub API client."""

    def __init__(self, base_url: str, *, token: str) -> None:
        self._base_url = base_url.rstrip("/")
        self._token = token
        self._http = httpx.Client(
            base_url=self._base_url + "/api/v1/",
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
            },
        )

    def invoke_device_command(
        self,
        device_id: DeviceId | str,
        command: str,
        *,
        input: JsonValue = None,
        stream_log: bool = False,
        timeout_secs: int | None = None,
        on_log: _SyncLogHandler | None = None,
    ) -> DeviceCommandDoneData:
        """Invoke a command on a device and return its completion data."""
        with self.stream_device_command(
            device_id,
            command,
            input=input,
            stream_log=stream_log,
            timeout_secs=timeout_secs,
        ) as frames:
            return _drain_sync(frames, on_log)

    def stream_device_command(
        self,
        device_id: DeviceId | str,
        command: str,
        *,
        input: JsonValue = None,
        stream_log: bool = False,
        timeout_secs: int | None = None,
    ) -> AbstractContextManager[Iterator[DeviceCommandDeviceFrame]]:
        """Stream raw device command frames as a context-managed iterator."""
        return stream_command_sync(
            self._base_url,
            self._token,
            device_id,
            command,
            input=input,
            stream_log=stream_log,
            timeout_secs=timeout_secs,
        )

    def _execute_action(self, action: pydantic.BaseModel) -> pydantic.BaseModel:
        action_name, output_adapter = _ACTION_REGISTRY[type(action)]
        response = self._http.post(
            f"actions/invoke/{action_name}",
            content=action.model_dump_json(by_alias=True),
        )
        if response.is_success:
            return cast(
                pydantic.BaseModel, output_adapter.validate_json(response.content)
            )
        _raise_api_error(response)

    def close(self) -> None:
        """Close the underlying HTTP connection."""
        self._http.close()

    def __enter__(self) -> Client:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()


class AsyncClient(_AsyncExecuteMixin):
    """Asynchronous Nexigon Hub API client."""

    def __init__(self, base_url: str, *, token: str) -> None:
        self._base_url = base_url.rstrip("/")
        self._token = token
        self._http = httpx.AsyncClient(
            base_url=self._base_url + "/api/v1/",
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
            },
        )

    async def invoke_device_command(
        self,
        device_id: DeviceId | str,
        command: str,
        *,
        input: JsonValue = None,
        stream_log: bool = False,
        timeout_secs: int | None = None,
        on_log: _AsyncLogHandler | None = None,
    ) -> DeviceCommandDoneData:
        """Invoke a command on a device and return its completion data."""
        async with self.stream_device_command(
            device_id,
            command,
            input=input,
            stream_log=stream_log,
            timeout_secs=timeout_secs,
        ) as frames:
            return await _drain_async(frames, on_log)

    def stream_device_command(
        self,
        device_id: DeviceId | str,
        command: str,
        *,
        input: JsonValue = None,
        stream_log: bool = False,
        timeout_secs: int | None = None,
    ) -> AbstractAsyncContextManager[AsyncIterator[DeviceCommandDeviceFrame]]:
        """Stream raw device command frames as an async context-managed iterator."""
        return stream_command_async(
            self._base_url,
            self._token,
            device_id,
            command,
            input=input,
            stream_log=stream_log,
            timeout_secs=timeout_secs,
        )

    async def _execute_action(self, action: pydantic.BaseModel) -> pydantic.BaseModel:
        action_name, output_adapter = _ACTION_REGISTRY[type(action)]
        response = await self._http.post(
            f"actions/invoke/{action_name}",
            content=action.model_dump_json(by_alias=True),
        )
        if response.is_success:
            return cast(
                pydantic.BaseModel, output_adapter.validate_json(response.content)
            )
        _raise_api_error(response)

    async def aclose(self) -> None:
        """Close the underlying HTTP connection."""
        await self._http.aclose()

    async def __aenter__(self) -> AsyncClient:
        return self

    async def __aexit__(self, *args: object) -> None:
        await self.aclose()


def _drain_sync(
    frames: Iterator[DeviceCommandDeviceFrame],
    on_log: _SyncLogHandler | None,
) -> DeviceCommandDoneData:
    for frame in frames:
        if isinstance(frame, DeviceCommandDeviceFrame_Done):
            return frame
        if isinstance(frame, DeviceCommandDeviceFrame_Log) and on_log is not None:
            on_log(frame)
    raise CommandInvocationError("connection closed before receiving a Done frame")


async def _drain_async(
    frames: AsyncIterator[DeviceCommandDeviceFrame],
    on_log: _AsyncLogHandler | None,
) -> DeviceCommandDoneData:
    async for frame in frames:
        if isinstance(frame, DeviceCommandDeviceFrame_Done):
            return frame
        if isinstance(frame, DeviceCommandDeviceFrame_Log) and on_log is not None:
            result = on_log(frame)
            if inspect.isawaitable(result):
                await result
    raise CommandInvocationError("connection closed before receiving a Done frame")
