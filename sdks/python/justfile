[private]
_default:
    @just --list

# Run all checks (lint, format, typecheck, test).
check: lint typecheck test

# Run tests.
[positional-arguments]
test *args:
    uv run --group dev pytest tests/ "$@"

# Run type checking.
typecheck:
    uv run --group dev mypy src/

# Run linting and format checking.
lint:
    uv run --group dev ruff check src/ tests/
    uv run --group dev ruff format --check src/ tests/

# Auto-fix lint errors and format code.
fmt:
    uv run --group dev ruff check --fix src/ tests/
    uv run --group dev ruff format src/ tests/
