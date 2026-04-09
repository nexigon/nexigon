from __future__ import annotations

from typing import NoReturn, cast

import httpx
import pydantic

from ._actions import _ACTION_REGISTRY, _AsyncExecuteMixin, _SyncExecuteMixin
from ._errors import ActionApiError
from .api_types.errors import ActionError

_ACTION_ERROR_ADAPTER = pydantic.TypeAdapter(ActionError)


def _raise_api_error(response: httpx.Response) -> NoReturn:
    error = _ACTION_ERROR_ADAPTER.validate_json(response.content)
    raise ActionApiError(error, response.status_code)


class Client(_SyncExecuteMixin):
    """Synchronous Nexigon Hub API client."""

    def __init__(self, base_url: str, *, token: str) -> None:
        self._http = httpx.Client(
            base_url=base_url.rstrip("/") + "/api/v1/",
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
            },
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
        self._http = httpx.AsyncClient(
            base_url=base_url.rstrip("/") + "/api/v1/",
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
            },
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
