"""The label contract.

Ownership is machine-readable: every managed resource carries a complete label set
including the owning daemon's `registry` UUID. A resource is *ours* only when every
required label is present and the registry id matches. Anything else is foreign or
unlabeled — counted and reported, never deleted. Name prefixes are never ownership proof.
"""

from __future__ import annotations

from dataclasses import dataclass

NAMESPACE = "com.zackees.bosn"

REGISTRY = f"{NAMESPACE}.registry"
KIND = f"{NAMESPACE}.kind"
STACK = f"{NAMESPACE}.stack"
GENERATION = f"{NAMESPACE}.generation"
SCOPE = f"{NAMESPACE}.scope"
WORKSPACE = f"{NAMESPACE}.workspace"
CREATED = f"{NAMESPACE}.created"

REQUIRED_LABELS: tuple[str, ...] = (REGISTRY, KIND, STACK, GENERATION, SCOPE, WORKSPACE, CREATED)

KINDS: frozenset[str] = frozenset({"container", "volume", "image", "builder", "network"})
SCOPES: frozenset[str] = frozenset({"spec", "stack", "machine"})


class LabelError(ValueError):
    """A label set is malformed."""


@dataclass(frozen=True)
class ResourceLabels:
    registry: str
    kind: str
    stack: str
    generation: str
    scope: str
    workspace: str
    created: str

    def __post_init__(self) -> None:
        if self.kind not in KINDS:
            raise LabelError(f"unknown kind {self.kind!r}; expected one of {sorted(KINDS)}")
        if self.scope not in SCOPES:
            raise LabelError(f"unknown scope {self.scope!r}; expected one of {sorted(SCOPES)}")
        if not self.registry:
            raise LabelError("registry id must not be empty")

    def to_dict(self) -> dict[str, str]:
        return {
            REGISTRY: self.registry,
            KIND: self.kind,
            STACK: self.stack,
            GENERATION: self.generation,
            SCOPE: self.scope,
            WORKSPACE: self.workspace,
            CREATED: self.created,
        }

    def to_docker_args(self) -> list[str]:
        """Render as repeated `--label k=v` arguments for the engine CLI."""
        args: list[str] = []
        for key, value in self.to_dict().items():
            args += ["--label", f"{key}={value}"]
        return args

    @classmethod
    def from_dict(cls, raw: dict[str, str]) -> ResourceLabels:
        missing = [key for key in REQUIRED_LABELS if not raw.get(key)]
        if missing:
            raise LabelError(f"incomplete label set; missing {missing}")
        return cls(
            registry=raw[REGISTRY],
            kind=raw[KIND],
            stack=raw[STACK],
            generation=raw[GENERATION],
            scope=raw[SCOPE],
            workspace=raw[WORKSPACE],
            created=raw[CREATED],
        )


def is_complete(raw: dict[str, str]) -> bool:
    """True when every required label is present and non-empty."""
    return all(raw.get(key) for key in REQUIRED_LABELS)


def is_owned_by(raw: dict[str, str], registry_id: str) -> bool:
    """Ownership proof: a complete label set whose registry id is ours.

    Incomplete labels are never ownership proof, even when the registry id matches --
    a partially labeled resource may have been created by a version we cannot reason about.
    """
    return is_complete(raw) and raw.get(REGISTRY) == registry_id
