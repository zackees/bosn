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
import posixpath
import re
import shlex
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

MANIFEST_NAME = "bosn.toml"
VALID_SCOPES = {"spec", "stack", "machine"}

# Retention tiers a declared volume may ask for.
#
# `warm` is every volume bosn has ever managed: the tiered clocks in `retention.py` decide
# when it goes, and losing one costs a rebuild. `pinned` is for state that is *not*
# rebuildable on demand -- the motivating case is a macOS guest disk whose only creation
# path is a human sitting through a 30-60 minute interactive installer (#151). A pinned
# volume is never collected by any automatic rule: not by age, not by supersession, not by
# `bosn done`, not under storage pressure. It leaves only when a human names it to
# `bosn release-volume --apply --yes`.
DEFAULT_RETENTION = "warm"
VALID_RETENTIONS = {DEFAULT_RETENTION, "pinned"}

# The one non-default stack kind. A bare stack (`kind` unset) is the Linux stack bosn has
# always run: an image, `docker create`, `docker exec`. This kind is a QEMU/KVM macOS guest
# inside a `dockurr/macos` container, where the workload runs in a VM *within* the
# container -- so it needs device passthrough at create time and ssh, not `docker exec`, as
# its execution transport.
GUEST_MACOS_X64 = "macos-x64-guest"
VALID_KINDS = {GUEST_MACOS_X64}

# bosn mounts its own objects under this prefix and keeps the daemon heartbeat beside
# it. User mounts may not land here: a collision would shadow a managed volume or the
# liveness file the container's PID 1 watches.
RESERVED_PREFIX = "/bosn/"
RESERVED_HEARTBEAT = "/bosn-daemon/heartbeat"


class ManifestError(ValueError):
    """The manifest is malformed, or names something that does not exist."""


def _validate_destination(destination: str, *, what: str, name: str) -> str:
    """A mount destination must be absolute, unique, and outside bosn's own namespace."""
    if not destination.startswith("/"):
        raise ManifestError(
            f"{what} {name!r} has destination {destination!r}; it must be an absolute path"
        )
    normalized = destination.rstrip("/") or "/"
    if normalized == RESERVED_HEARTBEAT.rstrip("/") or normalized.startswith(RESERVED_PREFIX):
        raise ManifestError(
            f"{what} {name!r} would mount at {destination!r}, inside bosn's reserved "
            f"namespace ({RESERVED_PREFIX}*, {RESERVED_HEARTBEAT}); choose another destination"
        )
    return normalized


def _validate_workdir(workdir: str, *, stack: str) -> str:
    """A stack's `workdir` must be an absolute path, same as any mount destination.

    Reuses `_validate_destination` wholesale rather than re-deriving "absolute, and not
    inside bosn's reserved namespace" a second time. The reserved-namespace half is a
    freebie, not the point (nothing stops `sh -c` from `cd`-ing there once running), but
    there is no legitimate reason a task would want its cwd to start inside `/bosn/*` or
    at the heartbeat file, so refusing it here is consistent with why mounts refuse it.
    """
    return _validate_destination(workdir, what="workdir", name=stack)


def _validate_env_key(key: str, *, stack: str) -> None:
    """`docker create -e` splits its argument on the first `=`; an empty or `=`-bearing
    key would either be silently misparsed by the engine or collide with the value it is
    supposed to precede. Catching it here, at manifest load, is a much better failure mode
    than a wrong container running and 1000 miles from the missing environment variable.
    """
    if not key:
        raise ManifestError(f"[stack.{stack}.env] has an empty key")
    if "=" in key:
        raise ManifestError(f"[stack.{stack}.env] key {key!r} must not contain '='")


@dataclass(frozen=True)
class VolumeSpec:
    name: str
    scope: str
    destination: str | None = None
    # Appended after the existing fields on purpose: `VolumeSpec(name, scope, destination)`
    # is constructed positionally in several places and in the test suite.
    retention: str = DEFAULT_RETENTION

    def __post_init__(self) -> None:
        if self.scope not in VALID_SCOPES:
            raise ManifestError(
                f"volume {self.name!r} has unknown scope {self.scope!r}; "
                f"expected one of {sorted(VALID_SCOPES)}"
            )
        if self.retention not in VALID_RETENTIONS:
            raise ManifestError(
                f"volume {self.name!r} has unknown retention {self.retention!r}; "
                f"expected one of {sorted(VALID_RETENTIONS)}"
            )
        if self.destination is not None:
            object.__setattr__(
                self,
                "destination",
                _validate_destination(self.destination, what="volume", name=self.name),
            )

    def mount_at(self) -> str:
        """Where this volume lands in the container.

        Defaults to bosn's own namespace; an explicit destination lets an existing image
        keep the paths its ENV already points at instead of rewriting its Dockerfile.
        """
        return self.destination or f"{RESERVED_PREFIX}{self.name}"


@dataclass(frozen=True)
class MountSpec:
    """A host path bind-mounted into the container.

    bosn *owns* volumes and may delete them; it only *references* a bind source and must
    never touch it. That distinction is why binds are a separate table rather than another
    volume scope: nothing here is ever labeled, registered, or collected.
    """

    name: str
    source: str
    destination: str
    readonly: bool = False

    def __post_init__(self) -> None:
        if not self.source:
            raise ManifestError(f"mount {self.name!r} must set `source`")
        if not self.destination:
            raise ManifestError(f"mount {self.name!r} must set `destination`")
        object.__setattr__(
            self,
            "destination",
            _validate_destination(self.destination, what="mount", name=self.name),
        )

    def resolve_source(self, root: Path) -> Path:
        """The absolute host path, relative to the manifest root.

        The spelling is whatever shell wrote the manifest. A path typed in Git Bash reads
        `/c/work/repo`, which `Path` on Windows would join into `C:\\c\\work\\repo` -- a
        directory that does not exist, so the check below would reject a source that is
        perfectly valid. Unlike argv, a string inside bosn.toml never passes through MSYS
        path conversion, so nothing upstream has already fixed it.

        A missing source is an error rather than a silently engine-created empty
        directory: mounting a directory that was supposed to hold your source tree, and
        getting an empty one, fails much later and much more confusingly.
        """
        from bosn.paths import to_host_path

        spelled = to_host_path(self.source)
        resolved = (spelled if spelled.is_absolute() else root / spelled).resolve()
        if not resolved.exists():
            raise ManifestError(
                f"mount {self.name!r} sources {self.source!r}, which resolves to "
                f"{resolved} and does not exist"
            )
        return resolved


@dataclass(frozen=True)
class TmpfsSpec:
    """A container-local RAM filesystem rendered through Docker's `--tmpfs` option."""

    value: str

    def __post_init__(self) -> None:
        if not self.value:
            raise ManifestError("tmpfs entry must not be empty")
        raw_destination, separator, raw_options = self.value.partition(":")
        destination = _validate_destination(raw_destination, what="tmpfs", name=raw_destination)
        if separator and (not raw_options or any(not item for item in raw_options.split(","))):
            raise ManifestError(f"tmpfs {self.value!r} has an empty mount option")
        object.__setattr__(
            self,
            "value",
            f"{destination}:{raw_options}" if separator else destination,
        )

    @property
    def destination(self) -> str:
        return self.value.split(":", 1)[0]

    @property
    def readwrite(self) -> bool:
        readwrite = True
        options = self.value.partition(":")[2]
        for option in options.split(",") if options else ():
            if option == "ro":
                readwrite = False
            elif option == "rw":
                readwrite = True
        return readwrite


def _positive_int(value: Any, *, stack: str, key: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ManifestError(f"[stack.{stack}.guest] {key} must be a positive integer")
    if value <= 0:
        raise ManifestError(f"[stack.{stack}.guest] {key} must be a positive integer")
    return value


@dataclass(frozen=True)
class GuestSpec:
    """How to reach and size the VM inside a guest stack's container.

    Every default here is the value the working `zackees/kernal-api` scripts proved against
    a real `dockurr/macos` guest, so an empty `[stack.X.guest]` table is a usable stack
    rather than a stub. `dockurr/macos` publishes its web installer on 8006 and the guest's
    sshd is reached by mapping a host port onto the container's 22.
    """

    ssh_port: int = 2222
    ssh_user: str = "runner"
    ssh_host: str = "127.0.0.1"
    web_port: int = 8006
    # macOS boots slowly, and on one core it is minutes rather than seconds. The deadline is
    # bounded so a guest that never comes up fails with its logs attached instead of hanging
    # a CI job to its own timeout.
    ready_timeout: int = 1800
    ready_poll_interval: int = 10
    version: str = "ventura"
    ram_size: str = "8G"
    disk_size: str = "128G"
    # `None` means "decide from the host CPU vendor at create time" -- see
    # `guest.effective_cpu_cores`. An explicit value is honored as written.
    cpu_cores: int | None = None
    # One repo-relative file shipped into the guest before every task, and where it lands.
    # A guest stack cannot bind-mount -- the VM does not see the container's filesystem -- so
    # this is how the prebuilt artifact a task needs actually gets there. Unset means bosn
    # ships nothing and the task is responsible for finding its own inputs.
    payload: str | None = None
    payload_destination: str = "~/bosn-payload"

    def __post_init__(self) -> None:
        for key in ("ssh_port", "web_port", "ready_timeout", "ready_poll_interval"):
            value = getattr(self, key)
            if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
                raise ManifestError(f"[stack.*.guest] {key} must be a positive integer")
        if not self.ssh_user:
            raise ManifestError("[stack.*.guest] ssh_user must not be empty")
        if not self.ssh_host:
            raise ManifestError("[stack.*.guest] ssh_host must not be empty")
        if self.ssh_port == self.web_port:
            raise ManifestError(
                f"[stack.*.guest] ssh_port and web_port must differ; both are {self.ssh_port}"
            )
        if self.cpu_cores is not None and (
            isinstance(self.cpu_cores, bool) or not isinstance(self.cpu_cores, int)
        ):
            raise ManifestError("[stack.*.guest] cpu_cores must be a positive integer")
        if self.cpu_cores is not None and self.cpu_cores <= 0:
            raise ManifestError("[stack.*.guest] cpu_cores must be a positive integer")

    def digest_fields(self) -> list[tuple[str, str]]:
        """A stable, sorted rendering for `generation_digest`.

        Every field here lands in `docker create` -- published ports, `VERSION`, `RAM_SIZE`,
        `DISK_SIZE` -- or decides how a task reaches the guest. Docker has no verb for
        changing a created container's port map or env, so a change to any of them must roll
        the generation for the same reason `env` does; see `generation_digest`.
        """
        return sorted(
            (key, "" if value is None else str(value)) for key, value in self.__dict__.items()
        )


@dataclass(frozen=True)
class StackSpec:
    name: str
    dockerfile: str | None = None
    image: str | None = None
    family: str | None = None
    default: bool = False
    volumes: tuple[VolumeSpec, ...] = ()
    mounts: tuple[MountSpec, ...] = ()
    tmpfs: tuple[TmpfsSpec, ...] = ()
    # Container-level `-e KEY=VALUE` pairs, applied at `docker create` (see converge.py).
    # A dict, not a tuple of a small dataclass like VolumeSpec/MountSpec: there is nothing
    # here to validate per-entry beyond the key shape (`_validate_env_key`), no destination,
    # no scope, no readonly flag -- the extra structure those specs carry would be empty
    # ceremony for a bare key/value pair. `field(default_factory=dict)` is safe on a frozen
    # dataclass: frozen only blocks *reassigning* `self.env`, not mutating the dict object,
    # and nothing here ever mutates it after construction.
    env: dict[str, str] = field(default_factory=dict)
    # Container-level `-w` override, applied at `docker exec` (see converge.py). Unlike
    # `env`, this is not baked into the persistent container at `docker create` time -- it
    # is supplied fresh on every `exec`, the same way a task's `cmd` is. See
    # `generation_digest`'s comment on why that puts it on the opposite side of the digest
    # boundary from `env`.
    workdir: str | None = None
    # `None` is the ordinary Linux stack. See GUEST_MACOS_X64 for the one alternative.
    kind: str | None = None
    guest: GuestSpec | None = None
    # Apple's EULA conditions macOS on the hardware it runs on, and running it under QEMU on
    # a non-Apple host is the user's call to make, not something they should back into by
    # copying somebody's bosn.toml. So a guest stack is inert until this is written out
    # explicitly -- an opt-in keyword, not a default.
    acknowledge_macos_license: bool = False

    @property
    def is_guest(self) -> bool:
        return self.kind == GUEST_MACOS_X64

    def guest_spec(self) -> GuestSpec:
        """The guest configuration, for callers that already know this is a guest stack."""
        if not self.is_guest or self.guest is None:
            raise ManifestError(f"stack {self.name!r} is not a {GUEST_MACOS_X64} stack")
        return self.guest

    def __post_init__(self) -> None:
        for key in self.env:
            _validate_env_key(key, stack=self.name)
        if self.workdir is not None:
            object.__setattr__(self, "workdir", _validate_workdir(self.workdir, stack=self.name))
        if self.kind is not None and self.kind not in VALID_KINDS:
            raise ManifestError(
                f"[stack.{self.name}] has unknown kind {self.kind!r}; "
                f"expected one of {sorted(VALID_KINDS)}"
            )
        if self.kind is None:
            if self.guest is not None:
                raise ManifestError(
                    f'[stack.{self.name}.guest] is only meaningful with kind = "{GUEST_MACOS_X64}"'
                )
            if self.acknowledge_macos_license:
                raise ManifestError(
                    f"[stack.{self.name}] sets acknowledge_macos_license without "
                    f'kind = "{GUEST_MACOS_X64}"'
                )
            return
        if not self.acknowledge_macos_license:
            raise ManifestError(
                f"[stack.{self.name}] declares kind = {self.kind!r}, which boots macOS on "
                "non-Apple hardware under QEMU. Apple's licence conditions macOS on the "
                "hardware it runs on, so bosn will not start one unless you say so in the "
                "manifest: add `acknowledge_macos_license = true` to this stack."
            )
        if self.mounts:
            # Not a policy choice -- a bind lands in the *container's* filesystem, and the
            # guest is a VM inside that container with no view of it. Accepting the
            # declaration would produce a stack whose repo mount silently is not there.
            names = sorted(mount.name for mount in self.mounts)
            raise ManifestError(
                f"[stack.{self.name}.mounts] declares {names}, but a {self.kind} guest "
                "cannot see host bind mounts: the workload runs in a VM inside the "
                "container. Ship what the task needs over the guest's ssh channel instead "
                "(see docs/macos-guest.md)."
            )
        if self.guest is None:
            object.__setattr__(self, "guest", GuestSpec())

    def referenced_files(self, root: Path) -> list[Path]:
        """Files whose byte content folds into this stack's digest."""
        if not self.dockerfile:
            return []
        return docker_context_references(root, root / self.dockerfile)


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
    # `root` is deliberately the Docker context/workspace root.  The selected manifest
    # need not be called bosn.toml, though, and daemon IPC must retain that exact source
    # path rather than reconstructing a default-named sibling from the context root.
    # Keeping this after the existing positional fields preserves Manifest(root, stacks,
    # tasks) callers.
    source_path: Path | None = None

    @property
    def path(self) -> Path:
        return self.source_path or self.root / MANIFEST_NAME

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
    # Accepts whatever spelling the caller's shell produced. Argv from Git Bash is usually
    # converted to a native path before it reaches us, but "usually" is not a contract:
    # MSYS_NO_PATHCONV, a path built inside a script, or an IPC client all bypass it.
    from bosn.paths import to_host_path

    path = to_host_path(path)
    if path.is_dir():
        path = path / MANIFEST_NAME
    # IPC crosses a daemon whose cwd is not the client's cwd.  Canonicalizing the
    # selected source here makes a relative --manifest stable on both sides while keeping
    # `root` (and therefore Docker context, digest inputs, and workspace identity) at the
    # manifest's parent directory.
    path = path.resolve()
    if not path.is_file():
        raise ManifestError(f"no manifest at {path}")
    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as exc:
        raise ManifestError(f"{path} is not valid TOML: {exc}") from exc
    return parse(raw, root=path.parent, source_path=path)


def _refuse_duplicate_destinations(
    stack: str,
    volumes: tuple[VolumeSpec, ...],
    mounts: tuple[MountSpec, ...],
    tmpfs: tuple[TmpfsSpec, ...] = (),
) -> None:
    """Two mounts at one destination is last-writer-wins in Docker; refuse it here.

    The engine accepts the container and one of the two simply does not appear, which
    surfaces as a mysteriously empty directory rather than an error.
    """
    seen: dict[str, str] = {}
    for volume in volumes:
        seen[volume.mount_at()] = f"volume {volume.name!r}"
    for mount in mounts:
        previous = seen.get(mount.destination)
        if previous is not None:
            raise ManifestError(
                f"[stack.{stack}] mounts {mount.destination!r} twice: "
                f"{previous} and mount {mount.name!r}"
            )
        seen[mount.destination] = f"mount {mount.name!r}"
    for entry in tmpfs:
        previous = seen.get(entry.destination)
        if previous is not None:
            raise ManifestError(
                f"[stack.{stack}] mounts {entry.destination!r} twice: {previous} and tmpfs"
            )
        seen[entry.destination] = "tmpfs"
    destinations = [v.mount_at() for v in volumes]
    if len(set(destinations)) != len(destinations):
        duplicated = sorted({d for d in destinations if destinations.count(d) > 1})
        raise ManifestError(f"[stack.{stack}] mounts {duplicated} more than once")


_GUEST_INT_KEYS = ("ssh_port", "web_port", "ready_timeout", "ready_poll_interval", "cpu_cores")
_GUEST_STR_KEYS = (
    "ssh_user",
    "ssh_host",
    "version",
    "ram_size",
    "disk_size",
    "payload",
    "payload_destination",
)


def _parse_guest(stack: str, body: object) -> GuestSpec | None:
    """Read `[stack.X.guest]`, refusing unknown keys rather than ignoring them.

    A silently dropped `ssh_port` would send every task to the default 2222 and fail with a
    connection error a long way from the typo that caused it.
    """
    if body is None:
        return None
    if not isinstance(body, dict):
        raise ManifestError(f"[stack.{stack}.guest] must be a table")
    known = set(_GUEST_INT_KEYS) | set(_GUEST_STR_KEYS)
    unknown = sorted(set(body) - known)
    if unknown:
        raise ManifestError(
            f"[stack.{stack}.guest] has unknown keys {unknown}; expected {sorted(known)}"
        )
    fields: dict[str, Any] = {}
    for key in _GUEST_INT_KEYS:
        if key in body:
            fields[key] = _positive_int(body[key], stack=stack, key=key)
    for key in _GUEST_STR_KEYS:
        if key in body:
            value = body[key]
            if isinstance(value, dict | list | bool):
                raise ManifestError(f"[stack.{stack}.guest] {key} must be a string")
            fields[key] = str(value)
    try:
        return GuestSpec(**fields)
    except ManifestError as exc:
        raise ManifestError(str(exc).replace("[stack.*.guest]", f"[stack.{stack}.guest]")) from exc


def parse(raw: dict, root: Path, *, source_path: Path | None = None) -> Manifest:
    manifest = Manifest(root=root, source_path=source_path)

    for name, body in (raw.get("stack") or {}).items():
        if not isinstance(body, dict):
            raise ManifestError(f"[stack.{name}] must be a table")
        volumes = tuple(
            VolumeSpec(
                name=vol_name,
                scope=str((vol_body or {}).get("scope", "spec")),
                destination=(
                    str((vol_body or {}).get("destination"))
                    if (vol_body or {}).get("destination")
                    else None
                ),
                retention=str((vol_body or {}).get("retention", DEFAULT_RETENTION)),
            )
            for vol_name, vol_body in (body.get("volumes") or {}).items()
        )
        mounts = tuple(
            MountSpec(
                name=mount_name,
                source=str((mount_body or {}).get("source", "")),
                destination=str((mount_body or {}).get("destination", "")),
                readonly=bool((mount_body or {}).get("readonly", False)),
            )
            for mount_name, mount_body in (body.get("mounts") or {}).items()
        )
        raw_tmpfs = body["tmpfs"] if "tmpfs" in body else []
        if not isinstance(raw_tmpfs, list) or not all(
            isinstance(entry, str) for entry in raw_tmpfs
        ):
            raise ManifestError(f"[stack.{name}].tmpfs must be an array of strings")
        tmpfs = tuple(TmpfsSpec(entry) for entry in raw_tmpfs)
        _refuse_duplicate_destinations(name, volumes, mounts, tmpfs)
        env: dict[str, str] = {}
        for env_key, env_value in (body.get("env") or {}).items():
            if isinstance(env_value, dict | list):
                raise ManifestError(
                    f"[stack.{name}.env] key {env_key!r} must be a scalar, not a table or array"
                )
            env[str(env_key)] = str(env_value)
        workdir = body.get("workdir")
        kind = body.get("kind")
        stack = StackSpec(
            name=name,
            dockerfile=body.get("dockerfile"),
            image=body.get("image"),
            family=body.get("family"),
            default=bool(body.get("default", False)),
            volumes=volumes,
            mounts=mounts,
            tmpfs=tmpfs,
            env=env,
            workdir=str(workdir) if workdir else None,
            kind=str(kind) if kind is not None else None,
            guest=_parse_guest(name, body.get("guest")),
            acknowledge_macos_license=bool(body.get("acknowledge_macos_license", False)),
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
        # Declarations are digested; a bind source's *contents* deliberately are not.
        # Where something is mounted is part of this stack's identity, but the live
        # working tree behind a bind is exactly the thing a bind exists to keep outside
        # content identity -- and hashing it would be both enormous and never stable.
        "volumes": sorted((v.name, v.scope, v.mount_at()) for v in stack.volumes),
        "mounts": sorted((m.name, m.source, m.destination, m.readonly) for m in stack.mounts),
        "tmpfs": sorted(entry.value for entry in stack.tmpfs),
        # `env` IS digested; `workdir` (below `referenced_files`, not in this dict at all)
        # is NOT. Both look like "just another stack attribute" but they land on opposite
        # sides of `docker create` vs `docker exec`, and that is what decides this, not
        # convenience:
        #
        # `env` is baked into the persistent container at `docker create` time, exactly
        # like the image tag or a volume mount -- Docker has no "update the env of a
        # running container" verb, so once created, a container serves its create-time env
        # forever. If `env` were excluded from the digest the way a bind's *contents* are
        # (see the comment on `mounts` above), an env edit would change nothing this
        # function hashes, `_container_stale_reasons` would see the same generation and
        # happily reuse the old container, and every task run against it would keep
        # observing the stale environment indefinitely -- silently, with no error, in
        # exactly the same shape as the `CARGO_TARGET_DIR` bug that motivated this feature
        # in the first place (issue #105). That risk -- an unrelated env tweak forcing an
        # otherwise-warm generation to roll -- is real but bounded and visible: the task
        # rebuilds once, obviously, and moves on. A stale env silently steering a build
        # into the wrong place is unbounded and invisible. Digesting it is the smaller cost.
        #
        # `workdir` is supplied fresh on every `docker exec` (see `converge.py`), never
        # baked into the container at create time -- it is closer to a task's `cmd` than to
        # `env`, and `cmd` (on `TaskSpec`, not `StackSpec`) was never part of this digest
        # either. There is no "stale workdir survives in an old container" failure mode to
        # guard against: the very next `exec` picks up whatever `workdir` currently reads.
        # Digesting it anyway would force a container replacement for a change that cannot
        # make the existing container wrong, which is exactly the kind of unrelated-cache-
        # invalidation cost `env`'s comment above accepts only because the alternative is
        # worse. For `workdir` the alternative is not worse, so it stays out.
        "env": sorted(stack.env.items()),
    }
    # Everything below is added to `section` *only when it is set*, so a manifest that uses
    # none of it hashes byte-identically to the way it hashed before these fields existed.
    # An unconditional key would roll every existing user's generation on upgrade and
    # rebuild every warm cache in the fleet, for a feature they are not using.
    #
    # When they *are* set, they belong in the digest for the same reason `env` does: `kind`
    # and every `guest` field land in `docker create` (device passthrough, published ports,
    # `VERSION`/`RAM_SIZE`/`DISK_SIZE`), and a created container serves its create-time
    # configuration until it is replaced.
    if stack.kind is not None:
        section["kind"] = stack.kind
    if stack.guest is not None:
        section["guest"] = stack.guest.digest_fields()
    # A retention change *must* roll: `pinned` is written into the volume's registry row at
    # creation, and a volume that was registered warm keeps being collectable until a fresh
    # generation re-registers it as pinned.
    pinned = sorted(v.name for v in stack.volumes if v.retention != DEFAULT_RETENTION)
    if pinned:
        section["pinned_volumes"] = pinned
    _hash_field(
        hasher,
        json.dumps(section, sort_keys=True, separators=(",", ":")).encode("utf-8"),
    )

    for file_path in stack.referenced_files(manifest.root):
        try:
            relative = file_path.relative_to(manifest.root).as_posix()
        except ValueError as exc:
            raise ManifestError(
                f"stack {stack.name!r} references {file_path} outside its build context"
            ) from exc
        try:
            if file_path.is_symlink():
                kind = b"link"
                content = file_path.readlink().as_posix().encode("utf-8")
            elif file_path.is_dir():
                kind = b"dir"
                content = b""
            else:
                kind = b"file"
                content = file_path.read_bytes()
        except OSError as exc:
            raise ManifestError(f"cannot digest referenced path {file_path}: {exc}") from exc
        hasher.update(b"\0path-record\0")
        _hash_field(hasher, kind)
        _hash_field(hasher, relative.encode("utf-8"))
        _hash_field(hasher, content)

    return f"sha256:{hasher.hexdigest()}"


def _hash_field(hasher: Any, value: bytes) -> None:
    """Append one unambiguous binary field to a digest input."""
    hasher.update(len(value).to_bytes(8, "big"))
    hasher.update(value)


@dataclass(frozen=True)
class _IgnoreRule:
    pattern: str
    negated: bool


def _dockerignore_path(root: Path, dockerfile: Path) -> Path | None:
    specific = dockerfile.with_name(f"{dockerfile.name}.dockerignore")
    if specific.is_file():
        return specific
    default = root / ".dockerignore"
    return default if default.is_file() else None


def _dockerignore_rules(path: Path | None) -> list[_IgnoreRule]:
    if path is None:
        return []
    rules: list[_IgnoreRule] = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        if not raw or raw.startswith("#"):
            continue
        text = raw.strip()
        if not text or text == ".":
            continue
        negated = text.startswith("!")
        if negated:
            text = text[1:]
        # Docker deliberately disregards leading and trailing slashes. Always use POSIX
        # separators here so the same checked-out context hashes identically on every host.
        text = posixpath.normpath(text.replace("\\", "/").strip("/"))
        if text:
            rules.append(_IgnoreRule(text, negated))
    return rules


def _docker_pattern_regex(pattern: str) -> re.Pattern[str]:
    """Compile Docker's slash-aware glob syntax, including its special `**` wildcard."""
    expression = "^"
    index = 0
    while index < len(pattern):
        char = pattern[index]
        if char == "*":
            if index + 1 < len(pattern) and pattern[index + 1] == "*":
                index += 2
                if index < len(pattern) and pattern[index] == "/":
                    index += 1
                    expression += "(?:.*/)?"
                else:
                    expression += ".*"
                continue
            expression += "[^/]*"
        elif char == "?":
            expression += "[^/]"
        elif char == "[":
            end = pattern.find("]", index + 1)
            if end == -1:
                expression += r"\["
            else:
                character_class = pattern[index + 1 : end]
                if character_class.startswith("!"):
                    character_class = "^" + character_class[1:]
                expression += "[" + character_class.replace("\\", r"\\") + "]"
                index = end
        elif char == "\\" and index + 1 < len(pattern):
            index += 1
            expression += re.escape(pattern[index])
        else:
            expression += re.escape(char)
        index += 1
    return re.compile(expression + "$")


def _glob_matches(pattern: str, candidate: str) -> bool:
    return _docker_pattern_regex(pattern).fullmatch(candidate) is not None


def _ignore_rule_matches(rule: _IgnoreRule, relative: str) -> bool:
    parts = relative.split("/")
    prefixes = ["/".join(parts[:index]) for index in range(1, len(parts))]
    if _glob_matches(rule.pattern, relative):
        return True
    # A directory match applies to everything below it. Evaluating each prefix also lets a
    # later negation re-include a more specific descendant, matching Docker's last-rule-wins
    # behavior without pruning ignored directories during traversal.
    return any(_glob_matches(rule.pattern, prefix) for prefix in prefixes)


def _is_ignored(path: Path, root: Path, rules: list[_IgnoreRule]) -> bool:
    relative = path.relative_to(root).as_posix()
    ignored = False
    for rule in rules:
        if _ignore_rule_matches(rule, relative):
            ignored = not rule.negated
    return ignored


def _logical_dockerfile_lines(text: str) -> list[str]:
    escape = "\\"
    for raw in text.splitlines():
        directive = re.match(r"^\s*#\s*escape\s*=\s*([\\`])\s*$", raw, re.IGNORECASE)
        if directive:
            escape = directive.group(1)
            break
        if raw.strip() and not raw.lstrip().startswith("#"):
            break
    lines: list[str] = []
    heredocs: list[tuple[str, bool]] = []
    pending = ""
    for raw in text.splitlines():
        if heredocs:
            delimiter, strip_tabs = heredocs[0]
            candidate = raw.lstrip("\t") if strip_tabs else raw
            if candidate == delimiter:
                heredocs.pop(0)
            continue
        stripped = raw.strip()
        if not stripped or (not pending and stripped.startswith("#")):
            continue
        pending += stripped
        if pending.endswith(escape):
            pending = pending[:-1] + " "
            continue
        lines.append(pending)
        heredocs.extend(_heredoc_delimiters(pending))
        pending = ""
    if pending:
        lines.append(pending)
    return lines


def _heredoc_delimiters(instruction: str) -> list[tuple[str, bool]]:
    delimiters: list[tuple[str, bool]] = []
    pattern = re.compile(
        r"<<(?P<tabs>-?)(?:\\)?(?P<quote>['\"]?)(?P<name>[A-Za-z0-9_.-]+)(?P=quote)"
    )
    index = 0
    quote: str | None = None
    while index < len(instruction):
        char = instruction[index]
        if quote is not None:
            if char == "\\" and quote == '"':
                index += 2
                continue
            if char == quote:
                quote = None
            index += 1
            continue
        if char in {'"', "'"}:
            quote = char
            index += 1
            continue
        if char == "\\":
            index += 2
            continue
        match = (
            pattern.match(instruction, index)
            if index == 0 or instruction[index - 1] != "<"
            else None
        )
        if match is not None:
            delimiters.append((match.group("name"), bool(match.group("tabs"))))
            index = match.end()
            continue
        index += 1
    return delimiters


def _copy_instruction(
    operation: str, payload: str, dockerfile: Path
) -> tuple[list[str], dict[str, str | None]]:
    """Parse COPY/ADD while preserving JSON form after its leading option tokens."""
    flags: dict[str, str | None] = {}
    body = payload.lstrip()
    while body.startswith("--"):
        flag, separator, body = body.partition(" ")
        if not separator:
            raise ManifestError(f"{operation} in {dockerfile} has no source or destination")
        name, equals, value = flag[2:].partition("=")
        flags[name.lower()] = value if equals else None
        body = body.lstrip()

    if body.startswith("["):
        try:
            parsed = json.loads(body)
        except json.JSONDecodeError as exc:
            raise ManifestError(f"cannot parse JSON {operation} in {dockerfile}") from exc
        if not isinstance(parsed, list) or not all(isinstance(item, str) for item in parsed):
            raise ManifestError(f"invalid JSON {operation} in {dockerfile}")
        tokens = list(parsed)
    else:
        try:
            tokens = shlex.split(body, posix=True)
        except ValueError as exc:
            raise ManifestError(f"cannot parse {operation} in {dockerfile}: {exc}") from exc
    if len(tokens) < 2:
        raise ManifestError(f"{operation} in {dockerfile} needs a source and destination")
    return tokens, flags


def _run_mounts(line: str, dockerfile: Path) -> list[dict[str, str]]:
    match = re.match(r"^RUN\s+(.*)$", line, re.IGNORECASE)
    if not match:
        return []
    body = match.group(1).lstrip()
    mounts: list[dict[str, str]] = []
    token_pattern = re.compile(r"""^(?P<token>(?:[^\s'"\\]|\\.|"[^"]*"|'[^']*')+)""")
    while body.startswith("--"):
        token_match = token_pattern.match(body)
        if token_match is None:
            break
        raw_token = token_match.group("token")
        try:
            token = shlex.split(raw_token, posix=True)[0]
        except (IndexError, ValueError) as exc:
            raise ManifestError(f"cannot parse RUN option in {dockerfile}: {exc}") from exc
        body = body[token_match.end() :].lstrip()
        if not token.startswith("--mount="):
            continue
        options: dict[str, str] = {}
        for item in token.removeprefix("--mount=").split(","):
            name, equals, value = item.partition("=")
            if name:
                options[name.lower()] = value if equals else "true"
        mounts.append(options)
    return mounts


def _remote_add_kind(source: str) -> str | None:
    lowered = source.lower()
    if lowered.startswith(("git://", "ssh://", "git@")):
        return "git"
    if lowered.startswith(("http://", "https://")):
        path = lowered.split("#", 1)[0].split("?", 1)[0]
        return "git" if path.endswith(".git") else "http"
    return None


def _git_add_is_immutable(source: str) -> bool:
    fragment = source.rsplit("#", 1)[1] if "#" in source else ""
    revision = fragment.split(":", 1)[0]
    return re.fullmatch(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})", revision) is not None


def _copy_sources(dockerfile: Path) -> tuple[list[str], bool]:
    """Return local COPY/ADD sources and whether a conservative full-context hash is needed."""
    sources: list[str] = []
    full_context = False
    for line in _logical_dockerfile_lines(dockerfile.read_text(encoding="utf-8")):
        match = re.match(r"^(COPY|ADD)\s+(.*)$", line, re.IGNORECASE)
        if not match:
            for mount in _run_mounts(line, dockerfile):
                if mount.get("type", "bind").lower() != "bind" or mount.get("from"):
                    continue
                source = mount.get("source", mount.get("src", "."))
                if "$" in source:
                    full_context = True
                else:
                    sources.append(source)
            continue
        operation, payload = match.groups()
        operation = operation.upper()
        tokens, flags = _copy_instruction(operation, payload, dockerfile)
        if "from" in flags:
            if not flags["from"]:
                raise ManifestError(f"{operation} --from in {dockerfile} needs an image or stage")
            continue
        for source in tokens[:-1]:
            if source.startswith("<<"):
                # Heredoc bodies live in the Dockerfile itself, which is already hashed.
                continue
            if "$" in source:
                full_context = True
                continue
            if operation == "ADD" and (remote_kind := _remote_add_kind(source)):
                if remote_kind == "git" and not _git_add_is_immutable(source):
                    raise ManifestError(
                        f"Git ADD source {source!r} in {dockerfile} needs a full commit "
                        "reference for deterministic generation identity"
                    )
                if remote_kind == "http" and not flags.get("checksum"):
                    raise ManifestError(
                        f"remote ADD source {source!r} in {dockerfile} needs --checksum for "
                        "deterministic generation identity"
                    )
                continue
            sources.append(source)
    return sources, full_context


def _walk_source(root: Path, source: str) -> set[Path]:
    normalized = posixpath.normpath(source.replace("\\", "/").lstrip("/")) or "."
    while normalized == ".." or normalized.startswith("../"):
        normalized = normalized.removeprefix("..").lstrip("/") or "."
    if any(marker in normalized for marker in "*?["):
        matches = [
            path
            for path in root.rglob("*")
            if _glob_matches(normalized, path.relative_to(root).as_posix())
        ]
    else:
        candidate = root / normalized
        matches = [candidate] if candidate.exists() or candidate.is_symlink() else []
    if not matches:
        raise ManifestError(f"Docker build source {source!r} does not exist under {root}")
    paths: set[Path] = set()
    for match in matches:
        paths.add(match)
        if match.is_dir():
            paths.update(match.rglob("*"))
    return paths


def docker_context_references(root: Path, dockerfile: Path) -> list[Path]:
    """Resolve the local Docker build content closure used for generation identity."""
    if not dockerfile.is_file():
        raise ManifestError(f"stack references {dockerfile}, which does not exist")
    ignore_path = _dockerignore_path(root, dockerfile)
    rules = _dockerignore_rules(ignore_path)
    sources, full_context = _copy_sources(dockerfile)
    paths: set[Path] = {dockerfile}
    if ignore_path is not None:
        paths.add(ignore_path)
    if full_context:
        paths.update(root.rglob("*"))
    else:
        for source in sources:
            paths.update(_walk_source(root, source))
    protected = {dockerfile, ignore_path}
    return sorted(
        (path for path in paths if path in protected or not _is_ignored(path, root, rules)),
        key=lambda path: path.relative_to(root).as_posix(),
    )


_AUTOMATIC_PLATFORM_ARGS = frozenset(
    {
        "BUILDPLATFORM",
        "BUILDOS",
        "BUILDARCH",
        "BUILDVARIANT",
        "TARGETPLATFORM",
        "TARGETOS",
        "TARGETARCH",
        "TARGETVARIANT",
    }
)


def _substitute_build_args(
    value: str,
    args: dict[str, str],
    dockerfile: Path,
    *,
    allow_automatic_platform_args: bool = False,
) -> str:
    variable = re.compile(r"\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([^}]+)\})")

    def replace(match: re.Match[str]) -> str:
        name = match.group(1) or match.group(2) or ""
        if allow_automatic_platform_args and name in _AUTOMATIC_PLATFORM_ARGS:
            return match.group(0)
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) or name not in args:
            raise ManifestError(
                f"image reference {value!r} in {dockerfile} uses unresolved build argument {name!r}"
            )
        return args[name]

    return variable.sub(replace, value)


def dockerfile_external_images(root: Path, stack: StackSpec) -> list[tuple[str, str | None]]:
    """Return external image references and their requested platforms in build order."""
    if not stack.dockerfile:
        return []
    dockerfile = root / stack.dockerfile
    if not dockerfile.is_file():
        raise ManifestError(f"stack {stack.name!r} references {dockerfile}, which does not exist")
    aliases: set[str] = set()
    global_args: dict[str, str] = {}
    images: list[tuple[str, str | None]] = []
    first_stage_seen = False
    for line in _logical_dockerfile_lines(dockerfile.read_text(encoding="utf-8")):
        arg_match = re.match(r"^ARG\s+([A-Za-z_][A-Za-z0-9_]*)(?:=(.*))?$", line, re.IGNORECASE)
        if arg_match and not first_stage_seen:
            name, default = arg_match.groups()
            if default is not None:
                global_args[name] = _substitute_build_args(
                    default,
                    global_args,
                    dockerfile,
                    allow_automatic_platform_args=True,
                )
            continue
        from_match = re.match(r"^FROM\s+(.*)$", line, re.IGNORECASE)
        if from_match:
            try:
                tokens = shlex.split(from_match.group(1), posix=True)
            except ValueError as exc:
                raise ManifestError(f"cannot parse FROM in {dockerfile}: {exc}") from exc
            platform: str | None = None
            while tokens and tokens[0].startswith("--"):
                option = tokens.pop(0)
                if option.startswith("--platform="):
                    platform = _substitute_build_args(
                        option.partition("=")[2],
                        global_args,
                        dockerfile,
                        allow_automatic_platform_args=True,
                    )
            if not tokens:
                raise ManifestError(f"FROM in {dockerfile} needs an image reference")
            reference = _substitute_build_args(
                tokens.pop(0),
                global_args,
                dockerfile,
                allow_automatic_platform_args=True,
            )
            alias = tokens[1] if len(tokens) == 2 and tokens[0].lower() == "as" else None
            if tokens and alias is None:
                raise ManifestError(f"cannot parse FROM in {dockerfile}")
            if reference.lower() != "scratch" and reference.lower() not in aliases:
                images.append((reference, platform))
            if alias:
                aliases.add(alias.lower())
            first_stage_seen = True
            continue
        copy_match = re.match(r"^(?:COPY|ADD)\s+(.*)$", line, re.IGNORECASE)
        if copy_match:
            _tokens, flags = _copy_instruction("COPY", copy_match.group(1), dockerfile)
            from_reference = flags.get("from")
            if from_reference:
                from_reference = _substitute_build_args(from_reference, global_args, dockerfile)
                if (
                    not from_reference.isdigit()
                    and from_reference.lower() not in aliases
                    and from_reference.lower() != "scratch"
                ):
                    images.append((from_reference, None))
            continue
        for mount in _run_mounts(line, dockerfile):
            from_reference = mount.get("from")
            if not from_reference:
                continue
            from_reference = _substitute_build_args(
                from_reference,
                global_args,
                dockerfile,
                allow_automatic_platform_args=True,
            )
            if (
                not from_reference.isdigit()
                and from_reference.lower() not in aliases
                and from_reference.lower() != "scratch"
            ):
                images.append((from_reference, None))
    return images
