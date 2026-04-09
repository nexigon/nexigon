import json

import httpx
import pytest

from nexigon_hub_sdk import ActionApiError, Client
from nexigon_hub_sdk._actions import _ACTION_REGISTRY
from nexigon_hub_sdk.api_types import users


def test_action_request_encoding() -> None:
    """Actions serialize to camelCase JSON matching the server's expectations."""
    action = users.GetUserDetailsAction(
        user_id=users.UserId("u_9JTb4Drw5fbVWpoqM8fwvg")
    )
    payload = json.loads(action.model_dump_json(by_alias=True))
    assert payload == {"userId": "u_9JTb4Drw5fbVWpoqM8fwvg"}


def test_action_response_decoding() -> None:
    """Output types deserialize from camelCase JSON."""
    _, adapter = _ACTION_REGISTRY[users.GetUserDetailsAction]
    output = adapter.validate_json(
        json.dumps(
            {
                "userId": "u_9JTb4Drw5fbVWpoqM8fwvg",
                "email": "alice@example.com",
                "displayName": "Alice",
                "isAdmin": False,
            }
        )
    )
    assert isinstance(output, users.GetUserDetailsOutput)
    assert output.user_id == "u_9JTb4Drw5fbVWpoqM8fwvg"
    assert output.display_name == "Alice"


def test_execute_success() -> None:
    """Client.execute round-trips a successful action call."""
    transport = httpx.MockTransport(
        lambda req: httpx.Response(
            200,
            json={
                "userId": "u_9JTb4Drw5fbVWpoqM8fwvg",
                "email": "bob@example.com",
                "displayName": None,
                "isAdmin": True,
            },
        )
    )
    client = Client.__new__(Client)
    client._http = httpx.Client(transport=transport, base_url="http://test/api/v1/")

    result = client.execute(
        users.GetUserDetailsAction(user_id=users.UserId("u_9JTb4Drw5fbVWpoqM8fwvg"))
    )
    assert isinstance(result, users.GetUserDetailsOutput)
    assert result.email == "bob@example.com"
    assert result.is_admin is True


def test_execute_error() -> None:
    """Client.execute raises ActionApiError on error responses."""
    transport = httpx.MockTransport(
        lambda req: httpx.Response(
            403,
            json={"kind": "Forbidden", "message": "Not allowed"},
        )
    )
    client = Client.__new__(Client)
    client._http = httpx.Client(transport=transport, base_url="http://test/api/v1/")

    with pytest.raises(ActionApiError) as exc_info:
        client.execute(users.QueryUsersAction())

    assert exc_info.value.status_code == 403
    assert "Forbidden" in str(exc_info.value)
