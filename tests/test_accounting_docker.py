"""Real Docker coverage for #37 inventory accounting."""

from __future__ import annotations

import uuid

import pytest

from bosn.accounting import StorageInventory
from bosn.engine import Engine


@pytest.mark.docker
def test_system_df_attributes_a_real_named_volume(engine: Engine) -> None:
    name = f"bosn-accounting-{uuid.uuid4().hex[:12]}"
    engine.run(["volume", "create", name], check=True)
    try:
        inventory = StorageInventory.collect(engine)
        assert inventory.sizes.get(("volume", name)) is not None
    finally:
        engine.run(["volume", "rm", "--force", name])
