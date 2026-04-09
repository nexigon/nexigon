from __future__ import annotations

from .api_types.errors import ActionError


class ActionApiError(Exception):
    """Raised when the Nexigon Hub API returns an error response."""

    def __init__(self, error: ActionError, status_code: int) -> None:
        self.error = error
        self.status_code = status_code
        super().__init__(f"{error.kind.root}: {error.message}")
