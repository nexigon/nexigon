# Actions

Nexigon Hub uses an **action-based API**: every operation -- querying devices,
setting properties, issuing tokens -- is represented by a dedicated Pydantic
model. You construct the action, pass it to `client.execute()`, and get back a
typed response.

## How it works

```python
from nexigon_hub_sdk.api_types import devices

# 1. Build the action with its parameters
action = devices.GetDeviceDetailsAction(device_id="d_...")

# 2. Execute it
output = client.execute(action)

# 3. Use the typed output
print(output.name)
print(output.is_connected)
```

Each action class documents the operation it performs and the fields it accepts.
The return type is statically known -- `execute(GetDeviceDetailsAction(...))` returns
`GetDeviceDetailsOutput`, not a generic dict.

## Action categories

Actions are organized by API domain:

| Module | Domain |
|--------|--------|
| `api_types.users` | User accounts, tokens, sessions, registration |
| `api_types.organizations` | Organizations, members, invitations |
| `api_types.projects` | Projects, deployment tokens, audit log |
| `api_types.devices` | Devices, certificates, connections, events, properties |
| `api_types.repositories` | Repositories, packages, versions, assets |
| `api_types.fleet` | Fleet-wide properties |
| `api_types.audit` | Global audit log |
| `api_types.jobs` | Background jobs |
| `api_types.instance` | Instance settings and statistics |
| `api_types.cluster` | Cluster node management |

## Naming conventions

Actions follow a consistent naming pattern:

- `Query*Action` -- list resources (returns a wrapper with a list field)
- `Get*DetailsAction` -- get a single resource by ID
- `Create*Action` -- create a new resource
- `Delete*Action` -- delete a resource
- `Set*Action` -- update a specific field on a resource

## Serialization

Action models use `camelCase` aliases for JSON serialization (matching the HTTP
API) while exposing `snake_case` attributes in Python:

```python
action = devices.GetDeviceDetailsAction(device_id="d_...")

# Python attribute
action.device_id  # "d_..."

# JSON sent to the API
action.model_dump_json(by_alias=True)  # '{"deviceId": "d_..."}'
```

You always work with `snake_case` in Python -- the SDK handles the conversion.
