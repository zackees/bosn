"""Run the compositor geometry regression without a desktop session."""

import subprocess
import unittest
from pathlib import Path


class WidgetPlacementTests(unittest.TestCase):
    """The exact installed script handles monitor and panel geometry."""

    def test_lower_right_without_focus_or_other_window_changes(self) -> None:
        """Keep the bubble clear of panels without activating other windows."""
        subprocess.run(
            ["node", "ci/test_widget_placement.js"],
            cwd=Path(__file__).resolve().parents[1],
            check=True,
        )
