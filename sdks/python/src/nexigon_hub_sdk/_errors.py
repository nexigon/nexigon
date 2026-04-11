from __future__ import annotations

from .api_types.errors import ActionError


class ActionApiError(Exception):
    """Raised when the Nexigon Hub API returns an error response."""

    def __init__(self, error: ActionError, status_code: int) -> None:
        self.error = error
        self.status_code = status_code
        super().__init__(f"{error.kind.root}: {error.message}")


class CommandInvocationError(Exception):
    """Raised when a device command invocation fails at the transport or protocol level.

    This is distinct from a command that completes with `DeviceCommandStatus.Error`: a
    failed command still yields a `Done` frame and is returned to the caller. This
    exception signals that the WebSocket connection was lost, a malformed frame was
    received, or the server closed the stream before sending a terminal `Done` frame.
    """
