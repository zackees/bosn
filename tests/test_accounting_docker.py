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
        # Assert `measured` first (#106): on a busy host `system df -v` can fail or return
        # unusable output, and the resulting empty `sizes` reads identically to "the volume
        # is missing" unless the failure is checked separately. This turns that failure mode
        # from a confusing `None is not None` into an explicit "measurement failed".
        assert inventory.measured, "system df -v failed or returned unusable output"
        assert inventory.sizes.get(("volume", name)) is not None
    finally:
        engine.run(["volume", "rm", "--force", name])
