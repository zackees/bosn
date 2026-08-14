from __future__ import annotations

import pytest

from bosn.config import ConfigError, load


def test_config_precedence_and_origins(monkeypatch, tmp_path) -> None:
    path = tmp_path / "config.toml"
    path.write_text("[policy]\nwarm_volume_ttl = 12\n", encoding="utf-8")
    monkeypatch.setenv("BOSN_WARM_VOLUME_TTL", "24")
    config = load(path=path, flags={"warm_volume_ttl": 48})
    assert config.get("warm_volume_ttl") == 48
    assert config.report()["warm_volume_ttl"]["origin"] == "flag"


def test_invalid_config_fails_closed_naming_the_key(tmp_path) -> None:
    path = tmp_path / "config.toml"
    path.write_text("[policy]\nwarm_volume_ttl = 0\n", encoding="utf-8")
    with pytest.raises(ConfigError, match="warm_volume_ttl"):
        load(path=path)
