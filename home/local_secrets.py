"""Helpers for reading local FreeMDU development secrets."""

from __future__ import annotations

from pathlib import Path
import tomllib


DEFAULT_SECRETS_FILE = Path(__file__).resolve().parent / ".cargo" / "secrets.toml"


def load_env_secret(name: str, secrets_file: Path = DEFAULT_SECRETS_FILE) -> str:
    try:
        with secrets_file.open("rb") as handle:
            document = tomllib.load(handle)
    except FileNotFoundError as exc:
        raise RuntimeError(
            f"secrets file not found: {secrets_file}; "
            "copy .cargo/secrets.example.toml to .cargo/secrets.toml first"
        ) from exc

    env = document.get("env")
    if not isinstance(env, dict):
        raise RuntimeError(f"{secrets_file}: missing [env] table")

    value = env.get(name)
    if not isinstance(value, str) or not value:
        raise RuntimeError(f"{secrets_file}: [env].{name} is missing or empty")

    return value
