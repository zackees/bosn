"""`bosn.toml` parsing and generation digests.

The checked-in manifest is the spec sheet, the discovery surface, and the digest root.

**Identity is a content digest.** A generation is the hash over a stack's manifest section
plus the byte content of every file it references. Two invocations are compatible iff their
digests are byte-equal. Edit the Dockerfile and a new generation rolls forward; the old one
keeps serving its live leases, then ages out as superseded.
"""

from __future__ import annotations

import hashlib
import json
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

MANIFEST_NAME = "bosn.toml"
VALID_SCOPES = {"spec", "stack", "machine"}


class ManifestError(ValueError):
    """The manifest is malformed, or names something that does not exist."""


@dataclass(frozen=True)
class VolumeSpec:
    name: str
    scope: str

    def __post_init__(self) -> None:
        if self.scope not in VALID_SCOPES:
            raise ManifestError(
                f"volume {self.name!r} has unknown scope {self.scope!r}; "
                f"expected one of {sorted(VALID_SCOPES)}"
            )


@dataclass(frozen=True)
class StackSpec:
    name: str
    dockerfile: str | None = None
    image: str | None = None
    family: str | None = None
    default: bool = False
    volumes: tuple[VolumeSpec, ...] = ()

    def referenced_files(self, root: Path) -> list[Path]:
        """Files whose byte content folds into this stack's digest."""
        return [root / self.dockerfile] if self.dockerfile else []


@dataclass(frozen=True)
class TaskSpec:
    name: str
    stack: str
    cmd: str


@dataclass
class Manifest:
    root: Path
    stacks: dict[str, StackSpec] = field(default_factory=dict)
    tasks: dict[str, TaskSpec] = field(default_factory=dict)

    @property
    def path(self) -> Path:
        return self.root / MANIFEST_NAME

    def default_stack(self) -> StackSpec:
        explicit = [s for s in self.stacks.values() if s.default]
        if len(explicit) > 1:
            names = sorted(s.name for s in explicit)
            raise ManifestError(f"more than one stack is marked default: {names}")
        if explicit:
            return explicit[0]
        if len(self.stacks) == 1:
            return next(iter(self.stacks.values()))
        raise ManifestError(
            "no default stack; mark one with `default = true` or name a stack explicitly"
        )

    def stack(self, name: str | None) -> StackSpec:
        if name is None:
            return self.default_stack()
        try:
            return self.stacks[name]
        except KeyError:
            raise ManifestError(
                f"no stack named {name!r}; known stacks: {sorted(self.stacks)}"
            ) from None

    def task(self, name: str) -> TaskSpec:
        try:
            return self.tasks[name]
        except KeyError:
            raise ManifestError(
                f"no task named {name!r}; known tasks: {sorted(self.tasks)}"
            ) from None

    def digest(self, stack_name: str | None = None) -> str:
        return generation_digest(self, self.stack(stack_name))


def find_manifest(start: Path | None = None) -> Path | None:
    """Walk upward looking for bosn.toml, the way git finds its root."""
    current = (start or Path.cwd()).resolve()
    for candidate in [current, *current.parents]:
        manifest = candidate / MANIFEST_NAME
        if manifest.is_file():
            return manifest
    return None


def load(path: Path | str) -> Manifest:
    path = Path(path)
    if path.is_dir():
        path = path / MANIFEST_NAME
    if not path.is_file():
        raise ManifestError(f"no manifest at {path}")
    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as exc:
        raise ManifestError(f"{path} is not valid TOML: {exc}") from exc
    return parse(raw, root=path.parent)


def parse(raw: dict, root: Path) -> Manifest:
    manifest = Manifest(root=root)

    for name, body in (raw.get("stack") or {}).items():
        if not isinstance(body, dict):
            raise ManifestError(f"[stack.{name}] must be a table")
        volumes = tuple(
            VolumeSpec(name=vol_name, scope=str((vol_body or {}).get("scope", "spec")))
            for vol_name, vol_body in (body.get("volumes") or {}).items()
        )
        stack = StackSpec(
            name=name,
            dockerfile=body.get("dockerfile"),
            image=body.get("image"),
            family=body.get("family"),
            default=bool(body.get("default", False)),
            volumes=volumes,
        )
        if stack.dockerfile is None and stack.image is None:
            raise ManifestError(f"[stack.{name}] must set either `dockerfile` or `image`")
        manifest.stacks[name] = stack

    for name, body in (raw.get("task") or {}).items():
        if not isinstance(body, dict):
            raise ManifestError(f"[task.{name}] must be a table")
        stack_name = body.get("stack")
        cmd = body.get("cmd")
        if not cmd:
            raise ManifestError(f"[task.{name}] must set `cmd`")
        if stack_name and stack_name not in manifest.stacks:
            raise ManifestError(
                f"[task.{name}] references unknown stack {stack_name!r}; "
                f"known stacks: {sorted(manifest.stacks)}"
            )
        manifest.tasks[name] = TaskSpec(
            name=name,
            stack=stack_name or (manifest.default_stack().name if manifest.stacks else ""),
            cmd=str(cmd),
        )

    if not manifest.stacks:
        raise ManifestError("manifest declares no stacks")
    return manifest


def generation_digest(manifest: Manifest, stack: StackSpec) -> str:
    """Hash the stack's manifest section plus the byte content of referenced files.

    A missing referenced file is an error, not a zero -- silently digesting an absent
    Dockerfile would make two different specs compare equal.
    """
    hasher = hashlib.sha256()
    section = {
        "name": stack.name,
        "dockerfile": stack.dockerfile,
        "image": stack.image,
        "family": stack.family,
        "volumes": sorted((v.name, v.scope) for v in stack.volumes),
    }
    hasher.update(json.dumps(section, sort_keys=True, separators=(",", ":")).encode("utf-8"))

    for file_path in stack.referenced_files(manifest.root):
        if not file_path.is_file():
            raise ManifestError(
                f"stack {stack.name!r} references {file_path}, which does not exist"
            )
        hasher.update(b"\0file\0")
        hasher.update(file_path.name.encode("utf-8"))
        hasher.update(b"\0")
        hasher.update(file_path.read_bytes())

    return f"sha256:{hasher.hexdigest()}"
