"""Desktop release archives bind executable contents to version and source."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ci"))
import widget_artifact as artifact


class WidgetArtifactTests(unittest.TestCase):
    def test_stage_and_verify_bind_source_version_and_binary(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "widget"
            header = bytearray(64)
            header[:6] = b"\x7fELF\x02\x01"
            header[18:20] = (62).to_bytes(2, "little")
            binary.write_bytes(header)
            info = artifact.BuildInfo("0.1.12", "a" * 40, artifact.TARGET)
            with patch.object(artifact, "build_info", return_value=info):
                archive = artifact.stage(binary, root / "dist", info)
            self.assertEqual(artifact.verify(archive, info), info)
            original = archive.read_bytes()
            with patch.object(artifact, "build_info", return_value=info):
                artifact.stage(binary, root / "dist", info)
            self.assertEqual(archive.read_bytes(), original)
            wrong = artifact.BuildInfo("0.1.12", "b" * 40, artifact.TARGET)
            with self.assertRaisesRegex(ValueError, "source"):
                artifact.verify(archive, wrong)
            sidecar = archive.with_suffix(archive.suffix + ".sha256")
            sidecar.write_text("0" * 64 + "  " + archive.name + "\n")
            with self.assertRaisesRegex(ValueError, "checksum"):
                artifact.verify(archive, info)

    def test_metadata_and_non_native_binary_fail_closed(self):
        with self.assertRaises(ValueError):
            artifact.BuildInfo.from_json(json.dumps({"version": "0.1.12"}))
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "widget"
            binary.write_bytes(b"#!/bin/sh\nexit 0\n")
            with self.assertRaisesRegex(ValueError, "ELF"):
                artifact.stage(
                    binary,
                    Path(temporary) / "dist",
                    artifact.BuildInfo("0.1.12", "a" * 40, artifact.TARGET),
                )


if __name__ == "__main__":
    unittest.main()
