"""Load tracked FreeMDU defaults plus ignored local overrides."""

from __future__ import annotations

from pathlib import Path
import tomllib


ROOT = Path(__file__).resolve().parent
DEFAULT_CONFIG = ROOT / ".cargo" / "config.toml"
LOCAL_CONFIG = ROOT / ".cargo" / "local.toml"


def _read_env(path: Path, *, optional: bool) -> dict[str, str]:
    try:
        with path.open("rb") as handle:
            document = tomllib.load(handle)
    except FileNotFoundError:
        if optional:
            return {}
        raise RuntimeError(f"configuration file not found: {path}") from None

    env = document.get("env", {})
    if not isinstance(env, dict):
        raise RuntimeError(f"{path}: [env] must be a TOML table")

    result: dict[str, str] = {}
    for name, value in env.items():
        if not isinstance(value, str):
            raise RuntimeError(f"{path}: [env].{name} must be a string")
        result[name] = value

    return result


def load_default_env() -> dict[str, str]:
    return _read_env(DEFAULT_CONFIG, optional=False)


def load_local_env() -> dict[str, str]:
    return _read_env(LOCAL_CONFIG, optional=True)


def load_config_env() -> dict[str, str]:
    """Return config.toml defaults merged with local.toml overrides."""
    merged = load_default_env()
    merged.update(load_local_env())
    return merged


def load_config_value(name: str) -> str:
    env = load_config_env()
    try:
        return env[name]
    except KeyError:
        raise RuntimeError(
            f"{name} is not defined in {DEFAULT_CONFIG} or {LOCAL_CONFIG}"
        ) from None
