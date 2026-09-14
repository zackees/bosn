"""Hermetic schema coverage for the native performance runner."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
from types import ModuleType

import pytest


def load_runner() -> ModuleType:
    path = Path(__file__).parents[1] / "ci" / "native_performance_baseline.py"
    spec = importlib.util.spec_from_file_location("native_performance_baseline", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


runner = load_runner()


class FixtureMetrics:
    """No processes, Docker, paths, or credentials: just the runner seam."""

    def __init__(self) -> None:
        self.calls: list[tuple[str, int]] = []

    def latency(self, name: str, samples: int) -> dict[str, object]:
        self.calls.append((name, samples))
        return runner.latency_metric([float(index + 1) for index in range(samples)])

    def cli_startup(self, samples: int) -> dict[str, object]:
        return self.latency("cli", samples)

    def daemon_idle_rss(self) -> dict[str, object]:
        self.calls.append(("rss", 0))
        return {"status": "measured", "unit": "KiB", "value": 1234}

    def daemon_status(self, samples: int) -> dict[str, object]:
        return self.latency("status", samples)

    def setup_ensure_reuse(self, samples: int) -> dict[str, object]:
        return self.latency("ensure", samples)


def test_fixture_emits_each_required_metric_with_bounded_samples() -> None:
    fixture = FixtureMetrics()
    metrics = runner.collect_metrics(fixture, runner.MAX_SAMPLES, docker=True)

    assert set(metrics) == {
        "native_cli_startup",
        "idle_daemon_rss",
        "daemon_status_latency",
        "setup_ensure_reuse_latency",
    }
    for name in ("native_cli_startup", "daemon_status_latency", "setup_ensure_reuse_latency"):
        metric = metrics[name]
        assert metric["status"] == "measured"
        assert len(metric["samples"]) == runner.MAX_SAMPLES
        assert metric["median"] == pytest.approx((runner.MAX_SAMPLES + 1) / 2)
    assert metrics["idle_daemon_rss"] == {"status": "measured", "unit": "KiB", "value": 1234}
    assert fixture.calls == [
        ("cli", runner.MAX_SAMPLES),
        ("rss", 0),
        ("status", runner.MAX_SAMPLES),
        ("ensure", runner.MAX_SAMPLES),
    ]


def test_fixture_keeps_docker_metric_explicit_when_not_opted_in() -> None:
    fixture = FixtureMetrics()
    metrics = runner.collect_metrics(fixture, 2, docker=False)

    assert metrics["setup_ensure_reuse_latency"] == {
        "status": "unsupported",
        "reason": "requires_docker_opt_in",
    }
    assert ("ensure", 2) not in fixture.calls


def test_report_is_stable_and_does_not_serialize_fixture_paths_or_secrets() -> None:
    fixture = FixtureMetrics()
    rendered = json.dumps(
        runner.report(runner.collect_metrics(fixture, 1, docker=True), 1, True, "0.1.0"),
        sort_keys=True,
    )

    assert "native_cli_startup" in rendered
    assert "setup_ensure_reuse_latency" in rendered
    assert "/tmp" not in rendered
    assert "secret" not in rendered


@pytest.mark.parametrize("samples", [0, runner.MAX_SAMPLES + 1])
def test_sample_bound_is_rejected_before_fixture_work(samples: int) -> None:
    with pytest.raises(ValueError, match="samples must be between"):
        runner.collect_metrics(FixtureMetrics(), samples, docker=True)
