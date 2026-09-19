"""bosn - a machine-wide lifecycle supervisor for container development resources."""

# The extension is always present: installed wheels ship it, and `uv sync` builds it for the
# source tree.
from ._native import (
    Client,
    ComposePlan,
    DoctorReport,
    JobLogPage,
    JobLogRecord,
    JobStatus,
    RegistryResource,
    RegistryResourcePage,
    SetupEnsureEvent,
    SetupEnsureEventPage,
    SetupPlan,
    Status,
    native_version,
    plan_compose_yaml,
    protocol_version,
    run_mcp,
)

# Derived, never written: the version is [workspace.package].version in Cargo.toml.
__version__: str = native_version()

__all__ = [
    "__version__",
    "Client",
    "ComposePlan",
    "DoctorReport",
    "JobLogPage",
    "JobLogRecord",
    "JobStatus",
    "RegistryResource",
    "RegistryResourcePage",
    "SetupEnsureEvent",
    "SetupEnsureEventPage",
    "SetupPlan",
    "Status",
    "native_version",
    "plan_compose_yaml",
    "protocol_version",
    "run_mcp",
]
