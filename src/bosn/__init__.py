"""bosn - a machine-wide lifecycle supervisor for container development resources."""

__version__ = "0.1.3"

__all__ = ["__version__"]

# Source-tree Python tests intentionally remain runnable before the extension is
# compiled.  Installed maturin wheels always contain this module; any loader
# failure inside an installed extension is surfaced rather than hidden.
try:
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
except ModuleNotFoundError as error:
    if error.name != "bosn._native":
        raise
else:
    __all__ += [
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
