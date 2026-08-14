"""Typed machine policy configuration with explicit precedence and origins."""

from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any


class ConfigError(ValueError):
    """A policy value is invalid; callers must stop rather than silently use a default."""


@dataclass(frozen=True)
class Value:
    value: float
    origin: str


_DEFAULTS = {
    "container_idle_stop": 3600.0,
    "container_remove": 86400.0,
    "warm_volume_ttl": 259200.0,
    "superseded_cap": 86400.0,
    "shared_cache_ceiling": float(100 * 1024**3),
    "run_max_duration": 28800.0,
    "idle_retire_seconds": 900.0,
    "build_ttl_seconds": 3600.0,
    "max_builds": float(max(2, (os.cpu_count() or 2) // 2)),
}
_ENV = {key: f"BOSN_{key.upper()}" for key in _DEFAULTS}


def policy_keys() -> tuple[str, ...]:
    """Documented policy keys, in stable CLI/report order."""
    return tuple(_DEFAULTS)


def default_path() -> Path:
    explicit = os.environ.get("BOSN_CONFIG")
    if explicit:
        return Path(explicit)
    base = os.environ.get("XDG_CONFIG_HOME")
    if base:
        return Path(base) / "bosn" / "config.toml"
    return Path.home() / ".config" / "bosn" / "config.toml"


def _number(key: str, value: Any, origin: str) -> float:
    if isinstance(value, bool):
        raise ConfigError(f"invalid config key {key!r} from {origin}: expected a positive number")
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise ConfigError(
            f"invalid config key {key!r} from {origin}: expected a positive number"
        ) from exc
    if number <= 0:
        raise ConfigError(f"invalid config key {key!r} from {origin}: expected a positive number")
    return number


@dataclass(frozen=True)
class Config:
    values: dict[str, Value]

    def get(self, key: str) -> float:
        return self.values[key].value

    def report(self) -> dict[str, dict[str, float | str]]:
        return {
            key: {"value": value.value, "origin": value.origin}
            for key, value in self.values.items()
        }


def load(*, path: Path | None = None, flags: dict[str, float | None] | None = None) -> Config:
    """Load policy with file < environment < CLI-flag precedence."""
    target = path or default_path()
    result = {key: Value(value, "default") for key, value in _DEFAULTS.items()}
    if target.exists():
        try:
            raw = tomllib.loads(target.read_text(encoding="utf-8"))
        except tomllib.TOMLDecodeError as exc:
            raise ConfigError(f"invalid config file {target}: {exc}") from exc
        policy = raw.get("policy", raw)
        if not isinstance(policy, dict):
            raise ConfigError(f"invalid config file {target}: [policy] must be a table")
        for key, value in policy.items():
            if key not in result:
                raise ConfigError(f"invalid config key {key!r} in {target}")
            result[key] = Value(_number(key, value, str(target)), str(target))
    for key, env in _ENV.items():
        if env in os.environ:
            result[key] = Value(_number(key, os.environ[env], env), env)
    for key, value in (flags or {}).items():
        if value is not None:
            if key not in result:
                raise ConfigError(f"invalid config flag {key!r}")
            result[key] = Value(_number(key, value, "flag"), "flag")
    return Config(result)
