"""Resource enumeration and adoption.

Ownership lives in the Docker labels, so enumeration is the recovery path for a lost
registry as well as the input to every lifecycle decision. Three buckets come out of a
scan, and the distinction is the whole safety property:

- **owned**    complete label set carrying our registry id -- eligible for collection
- **foreign**  complete label set carrying someone else's registry id -- counted, never touched
- **unlabeled** incomplete or absent labels -- invisible to every decision, never touched

Automatic deletion requires complete ownership proof, which is why there is no `gc --force`.
"""

from __future__ import annotations

import datetime as dt
import json
import os
import subprocess
import sys
import uuid
from dataclasses import dataclass, field
from pathlib import Path

from bosn import labels
from bosn.clock import Clock, SystemClock
from bosn.engine import Engine, EngineError
from bosn.registry import Registry, Resource

# Docker CLI list commands per resource kind, formatted as one JSON object per line.
_LIST_COMMANDS: dict[str, list[str]] = {
    "container": ["ps", "--all", "--format", "{{json .}}"],
    "volume": ["volume", "ls", "--format", "{{json .}}"],
    # `docker images` shortens `.ID` to 12 characters unless told otherwise.  Adoption,
    # convergence, execution leases, and GC must all use the same immutable full ID.
    "image": ["images", "--no-trunc", "--format", "{{json .}}"],
    "network": ["network", "ls", "--format", "{{json .}}"],
}

_INSPECT_COMMANDS: dict[str, list[str]] = {
    "container": ["inspect", "--format", "{{json .Config.Labels}}"],
    "volume": ["volume", "inspect", "--format", "{{json .Labels}}"],
    "image": ["image", "inspect", "--format", "{{json .Config.Labels}}"],
    "network": ["network", "inspect", "--format", "{{json .Labels}}"],
}


@dataclass(frozen=True)
class DiscoveredResource:
    kind: str
    name: str
    raw_labels: dict[str, str]

    def owned_by(self, registry_id: str) -> bool:
        return labels.is_owned_by(self.raw_labels, registry_id)

    @property
    def complete(self) -> bool:
        return labels.is_complete(self.raw_labels)

    @property
    def registry(self) -> str | None:
        return self.raw_labels.get(labels.REGISTRY)

    def parsed(self) -> labels.ResourceLabels:
        return labels.ResourceLabels.from_dict(self.raw_labels)


@dataclass
class ScanResult:
    owned: list[DiscoveredResource] = field(default_factory=list)
    foreign: list[DiscoveredResource] = field(default_factory=list)
    unlabeled: list[DiscoveredResource] = field(default_factory=list)
    scanned_kinds: set[str] = field(default_factory=set)

    @property
    def foreign_registries(self) -> set[str]:
        return {r.registry for r in self.foreign if r.registry}

    def counts(self) -> dict[str, int]:
        return {
            "owned": len(self.owned),
            "foreign": len(self.foreign),
            "unlabeled": len(self.unlabeled),
        }


def _parse_labels(blob: str) -> dict[str, str]:
    """Docker renders labels either as a JSON object or as a comma-joined k=v string."""
    blob = (blob or "").strip()
    if not blob or blob in {"null", "<no value>", "map[]"}:
        return {}
    if blob.startswith("{"):
        try:
            parsed = json.loads(blob)
        except json.JSONDecodeError:
            return {}
        if not isinstance(parsed, dict):
            return {}
        return {str(k): str(v) for k, v in parsed.items() if v is not None}
    result: dict[str, str] = {}
    for pair in blob.split(","):
        key, sep, value = pair.partition("=")
        if sep:
            result[key.strip()] = value.strip()
    return result


def _name_of(kind: str, row: dict[str, object]) -> str:
    if kind == "volume":
        return str(row.get("Name", ""))
    if kind == "image":
        return str(row.get("ID", ""))
    # `container` rows carry `Names`; `network` rows carry `Name` (no plural) -- both fall
    # through to this shared branch rather than getting a dedicated one.
    return str(row.get("Names") or row.get("Name") or row.get("ID", ""))


class ResourceScanner:
    """Enumerates engine resources and sorts them by ownership proof."""

    def __init__(self, engine: Engine | None = None) -> None:
        self.engine = engine or Engine()

    def _discover(self, kind: str) -> tuple[list[DiscoveredResource], bool]:
        args = _LIST_COMMANDS.get(kind)
        if args is None:
            raise ValueError(f"cannot enumerate unknown kind {kind!r}")
        result = self.engine.run(args)
        if not result.ok:
            return [], False

        discovered: list[DiscoveredResource] = []
        for line in result.stdout.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(row, dict):
                continue
            name = _name_of(kind, row)
            if not name:
                continue
            raw = _parse_labels(str(row.get("Labels", "")))
            if not labels.is_complete(raw):
                # The list format truncates labels for some kinds; confirm via inspect
                # before concluding a resource is unlabeled.
                raw = self.inspect_labels(kind, name) or raw
            discovered.append(DiscoveredResource(kind=kind, name=name, raw_labels=raw))
        return discovered, True

    def discover(self, kind: str) -> list[DiscoveredResource]:
        """List one kind; callers needing safety metadata should use :meth:`scan`."""
        discovered, _success = self._discover(kind)
        return discovered

    def inspect_labels(self, kind: str, name: str) -> dict[str, str]:
        args = _INSPECT_COMMANDS.get(kind)
        if args is None:
            return {}
        result = self.engine.run([*args, name])
        if not result.ok:
            return {}
        return _parse_labels(result.stdout)

    def scan(self, registry_id: str, kinds: list[str] | None = None) -> ScanResult:
        scan = ScanResult()
        for kind in kinds or list(_LIST_COMMANDS):
            discovered, success = self._discover(kind)
            if success:
                scan.scanned_kinds.add(kind)
            for resource in discovered:
                if not resource.complete:
                    scan.unlabeled.append(resource)
                elif resource.owned_by(registry_id):
                    scan.owned.append(resource)
                else:
                    scan.foreign.append(resource)
        return scan


# -- run state ---------------------------------------------------------------


def running_container_names(engine: Engine) -> frozenset[str] | None:
    """Container names the engine reports as currently running, or ``None`` if unknown.

    Uses ``docker ps`` *without* ``--all`` -- unlike :data:`_LIST_COMMANDS`\\ 's
    ``container`` entry, which passes ``--all`` because enumeration needs every container
    regardless of state. Omitting it here is what makes the result "running right now"
    rather than "exists". Parsing follows the same one-JSON-object-per-line format as
    :meth:`ResourceScanner._discover`, reusing :func:`_name_of` for the row shape.

    One engine call for the whole pass: callers (GC) evaluate potentially hundreds of
    resources per run and must not turn this into one subprocess per container.

    ``None`` and an empty ``frozenset`` are deliberately NOT interchangeable, and this is
    the entire safety property this function exists for:

    - ``frozenset()`` means the engine answered and reported nothing running. It is safe
      to treat every resource as unprotected by this check.
    - ``None`` means the engine could not answer -- unreachable, non-zero exit, timeout,
      or output that cannot be parsed as the expected JSON-lines format. Callers MUST
      treat ``None`` as "protect everything", because the alternative -- silently
      returning an empty set on failure -- would make every container collectable at
      exactly the moment bosn cannot see the engine, which is the bug this function
      exists to prevent. Do not "simplify" this to always return a set.
    """
    try:
        result = engine.run(["ps", "--format", "{{json .}}"])
    except EngineError:
        return None
    if not result.ok:
        return None
    names: set[str] = set()
    for line in result.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            return None
        if not isinstance(row, dict):
            return None
        name = _name_of("container", row)
        if name:
            names.add(name)
    return frozenset(names)


# -- leases ----------------------------------------------------------------


PROCESS_START_TOLERANCE_SECONDS = 2.0


def _parse_windows_process_start(ticks_text: str) -> float | None:
    """Parse .NET ``DateTime.Ticks`` (100ns units since 0001-01-01) into an epoch float."""
    try:
        return (int(ticks_text.strip()) / 10_000_000) - 62_135_596_800
    except ValueError:
        return None


def _parse_linux_process_start(
    stat_text: str, proc_stat_text: str, clock_ticks_per_sec: float
) -> float | None:
    """Parse ``/proc/<pid>/stat`` field 22 (starttime, in clock ticks since boot).

    ``stat_text`` is the raw content of ``/proc/<pid>/stat``; the comm field can itself
    contain spaces or parens, so the split happens after the last ``)``. ``proc_stat_text``
    is the raw content of ``/proc/stat``, which supplies the boot time (``btime``, seconds
    since epoch) that the tick count is relative to.
    """
    try:
        fields = stat_text.rsplit(")", 1)[1].split()
        ticks = int(fields[19])
        boot = next(
            int(line.split()[1])
            for line in proc_stat_text.splitlines()
            if line.startswith("btime ")
        )
        return boot + (ticks / clock_ticks_per_sec)
    except (IndexError, ValueError, StopIteration, ZeroDivisionError):
        return None


def _parse_darwin_process_start(lstart_text: str) -> float | None:
    """Parse ``ps -o lstart=`` output, e.g. ``Thu Aug 13 09:41:02 2026``."""
    try:
        return dt.datetime.strptime(lstart_text.strip(), "%a %b %d %H:%M:%S %Y").timestamp()
    except ValueError:
        return None


def process_start_time(pid: int) -> float | None:
    """Return an epoch creation time from an OS-owned process identity source.

    This is a thin per-OS dispatch: it gathers the raw platform data (a subprocess call on
    Windows/macOS, `/proc` reads on Linux) and hands it to a pure parsing helper so the
    parsing logic itself can be unit-tested on any OS with captured sample input.
    """
    if pid <= 0:
        return None
    if os.name == "nt":
        # WMIC is removed on current Windows; the CIM provider (Get-CimInstance) is
        # available on every supported v1 host and returns the creation time without name
        # guessing *and* without throwing on another user's process the way
        # `(Get-Process -Id $pid).StartTime` does (access-denied there surfaces as a
        # null-valued-expression error on stderr, which is discarded below).
        # Note: this spawns a PowerShell process (~1s) per call. process_alive() only
        # invokes it once a lease's TTL has already elapsed, so it is not on the hot path
        # of a normal heartbeat -- but a maintenance sweep iterating many expired leases
        # pays this cost per lease. A cache is deliberately not added here: keying on pid
        # alone is unsafe across PID reuse in a long-lived daemon, and there is no cheap
        # cache key (pid, start) available before the very probe the cache would replace.
        try:
            result = subprocess.run(
                [
                    "powershell",
                    "-NoProfile",
                    "-Command",
                    f"(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}')"
                    ".CreationDate.ToUniversalTime().Ticks",
                ],
                capture_output=True,
                text=True,
                check=False,
                timeout=5,
            )
        except (OSError, subprocess.SubprocessError):
            # Probe unavailable (missing/wedged powershell, timeout): process_alive()
            # treats a None start time as "identity unknown" and fails open rather than
            # ever deleting a resource that might belong to a live process.
            return None
        return _parse_windows_process_start(result.stdout)
    if sys.platform == "linux":
        try:
            stat_text = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
            proc_stat_text = Path("/proc/stat").read_text(encoding="utf-8")
            clock_ticks_per_sec = os.sysconf("SC_CLK_TCK")
        except OSError:
            return None
        if clock_ticks_per_sec is None or clock_ticks_per_sec <= 0:
            return None
        return _parse_linux_process_start(stat_text, proc_stat_text, clock_ticks_per_sec)
    if sys.platform == "darwin":
        try:
            result = subprocess.run(
                ["ps", "-p", str(pid), "-o", "lstart="],
                capture_output=True,
                text=True,
                check=False,
                timeout=5,
                # `ps`'s day/month names (%a/%b) follow LC_TIME; pin to the "C" locale so
                # the fixed English format `_parse_darwin_process_start` expects can never
                # silently degrade identity checking to PID-only under a non-English locale.
                env={**os.environ, "LC_ALL": "C"},
            )
        except (OSError, subprocess.SubprocessError):
            # See the Windows branch above: probe unavailable -> fail open, never deletes.
            return None
        return _parse_darwin_process_start(result.stdout)
    return None


def process_alive(pid: int, proc_start: float | None = None) -> bool:
    """Liveness probe for a lease holder.

    A lease expires only when its TTL elapses *and* this probe fails, so a killed client
    releases within one TTL while a live 40-minute build is never collected out from
    under itself.
    """
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except SystemError:
        # Windows: os.kill(pid, 0) on another user's/SYSTEM's process raises SystemError
        # (CPython returns a result with ERROR_ACCESS_DENIED still set as an exception,
        # which is not an OSError subclass and would otherwise escape every handler here).
        # Access-denied means the process exists and we cannot see it: treat as alive, the
        # same fail-open posture as PermissionError above.
        return True
    except OSError:
        # Only ProcessLookupError above proves the process is gone. Every other OSError
        # means this probe could not answer, so ask the OS-owned identity source instead
        # of guessing -- in either direction.
        #
        # Observed on an elevated Windows CI runner (issue #68): os.kill(4, 0) against the
        # live System process raises OSError [WinError 87] "The parameter is incorrect",
        # while tasklist lists the process and process_start_time() returns a valid
        # creation time for it. Answering "dead" there called a demonstrably live process
        # dead, expiring its lease. Answering "alive" unconditionally is no better: a
        # long-gone pid also lands here on Windows, and its lease would never expire.
        #
        # A creation time is positive evidence the process exists; its absence, after
        # os.kill already failed, means two independent probes could not find it.
        return process_start_time(pid) is not None
    if proc_start is None:
        return True
    actual = process_start_time(pid)
    # An unavailable identity probe fails conservatively: it may leak a lease but never
    # deletes resources belonging to a live process.
    return actual is None or abs(actual - proc_start) <= PROCESS_START_TOLERANCE_SECONDS


def lease_is_expired(
    lease,
    now: float,
    *,
    alive_probe=process_alive,
) -> bool:
    """True only when the TTL has elapsed AND the holder is gone."""
    if not lease.expired_by_time(now):
        return False
    return not alive_probe(lease.pid, lease.proc_start)


def resource_is_leased(registry: Registry, resource_id: str, *, alive_probe=process_alive) -> bool:
    now = registry.clock.now()
    return any(
        not lease_is_expired(lease, now, alive_probe=alive_probe)
        for lease in registry.leases_for(resource_id)
    )


def prune_dead_leases(registry: Registry, *, alive_probe=process_alive) -> list[str]:
    """Delete every lease that is both expired by time and confirmed dead.

    Reuses ``lease_is_expired`` so pruning applies exactly the same liveness proof as the
    rest of the lease lifecycle -- a lease is never deleted on TTL alone. Idempotent: a
    pruned lease's row is gone, so a second pass finds nothing left to prune and returns an
    empty list without error.
    """
    now = registry.clock.now()
    pruned: list[str] = []
    for lease in registry.all_leases():
        if lease_is_expired(lease, now, alive_probe=alive_probe):
            registry.prune_lease(lease.id)
            pruned.append(lease.id)
    return pruned


# -- adoption --------------------------------------------------------------

QUIET_PERIOD_SECONDS = 24 * 3600
TRANSFER_IMAGE = "alpine:3.20"


class TransferError(RuntimeError):
    """An explicit ownership transfer cannot be performed safely."""


def transfer_volume(registry: Registry, engine: Engine, resource: DiscoveredResource) -> str:
    """Recreate one detached foreign volume with the current ownership labels.

    Thin wrapper over :func:`recreate_volume_with_labels`: this call site is explicit
    ownership transfer, so the new label set is the old one with only ``registry``
    swapped for ours -- everything else about the resource (stack, generation, scope,
    workspace, created) is preserved exactly.
    """
    if resource.kind != "volume" or not resource.complete:
        raise TransferError("only a complete labeled volume can be transferred")
    parsed = resource.parsed()
    new_labels = labels.ResourceLabels(
        registry=registry.registry_id,
        kind=parsed.kind,
        stack=parsed.stack,
        generation=parsed.generation,
        scope=parsed.scope,
        workspace=parsed.workspace,
        created=parsed.created,
    )
    return recreate_volume_with_labels(engine, resource, new_labels)


def volume_is_attached(engine: Engine, name: str) -> bool:
    """True when a container references this volume (list is not empty on success).

    Shared by explicit ownership transfer and legacy-family adoption: both recreate a
    volume in place, which is only safe once nothing has it mounted.
    """
    attached = engine.run(["ps", "--all", "--filter", f"volume={name}", "--quiet"])
    if not attached.ok:
        raise TransferError(f"could not check volume attachments for {name}")
    return bool(attached.stdout.strip())


def recreate_volume_with_labels(
    engine: Engine, resource: DiscoveredResource, new_labels: labels.ResourceLabels
) -> str:
    """Recreate one detached volume carrying ``new_labels``, staging its data first.

    Docker labels are immutable, so "relabeling" a volume means: copy its data into a
    scratch volume, remove the original, recreate it under the same name with the new
    label set, and copy the data back. The staging volume retains a recoverable copy
    until the replacement is verified, so a failed relabel never silently destroys data.

    This is the mechanical core shared by explicit ``--transfer`` (adopt.py's foreign-id
    recovery) and legacy-family adoption (``legacy.py``): both need "same object, new
    ownership labels" and neither can get there by writing a database row, because the
    labels the engine reports come from the object itself, not from bosn's registry.
    """
    if resource.kind != "volume":
        raise TransferError("only a volume can be relabeled by staged recreation")
    if volume_is_attached(engine, resource.name):
        raise TransferError(f"volume {resource.name} is attached; stop its containers first")
    staging = f"bosn-transfer-{uuid.uuid4().hex}"
    created_staging = engine.run(["volume", "create", staging])
    if not created_staging.ok:
        raise TransferError(f"could not create transfer staging volume for {resource.name}")
    copy_to_staging = engine.run(
        [
            "run",
            "--rm",
            "-v",
            f"{resource.name}:/from:ro",
            "-v",
            f"{staging}:/to",
            TRANSFER_IMAGE,
            "sh",
            "-c",
            "cp -a /from/. /to/",
        ]
    )
    if not copy_to_staging.ok:
        raise TransferError(f"copy to staging failed; preserved staging volume {staging}")
    removed = engine.run(["volume", "rm", resource.name])
    if not removed.ok:
        raise TransferError(f"could not remove old volume; preserved staging volume {staging}")
    create = engine.run(["volume", "create", *new_labels.to_docker_args(), resource.name])
    if not create.ok:
        raise TransferError(f"could not recreate volume; preserved staging volume {staging}")
    copy_back = engine.run(
        [
            "run",
            "--rm",
            "-v",
            f"{staging}:/from:ro",
            "-v",
            f"{resource.name}:/to",
            TRANSFER_IMAGE,
            "sh",
            "-c",
            "cp -a /from/. /to/",
        ]
    )
    if not copy_back.ok:
        raise TransferError(
            f"copy into recreated volume failed; preserved staging volume {staging}"
        )
    engine.run(["volume", "rm", staging])
    return resource.name


def adopt(
    registry: Registry,
    scan: ScanResult,
    *,
    clock: Clock | None = None,
) -> list[str]:
    """Rebuild registry rows from labels after the database is lost.

    Adoption time becomes last-use, and the caller applies a 24 h quiet period on top, so
    recovery is never followed by a mass age-out of caches that were merely un-tracked.
    """
    clock = clock or SystemClock()
    now = clock.now()
    adopted: list[str] = []
    known = {(r.kind, r.name) for r in registry.list_resources()}

    for resource in scan.owned:
        if (resource.kind, resource.name) in known:
            continue
        parsed = resource.parsed()
        registry.register_resource(
            kind=resource.kind,
            name=resource.name,
            stack=parsed.stack,
            generation=parsed.generation,
            scope=parsed.scope,
            workspace=parsed.workspace,
            created_at=now,
        )
        registered = next(
            r
            for r in registry.list_resources()
            if r.kind == resource.kind and r.name == resource.name
        )
        registry.set_resource_state(registered.id, "adopted")
        registry.log_event("resource.adopted", f"{resource.kind}:{resource.name}")
        adopted.append(resource.name)
    return adopted


def reconcile_owned(
    registry: Registry, scan: ScanResult, *, prior_resources: list[Resource] | None = None
) -> list[str]:
    """Make registry state agree with complete resources carrying our identity.

    This is deliberately additive: a failed or unavailable engine listing must never
    turn an empty scan into permission to forget registry rows.  Engine deletion is
    already idempotently handled by collection; startup reconciliation repairs the
    other crash boundary, where Docker accepted a create but the process died before
    SQLite recorded it.
    """
    reconciled: list[str] = []
    for resource in scan.owned:
        parsed = resource.parsed()
        existing = registry.get_resource_by_engine_identity(resource.kind, resource.name)
        if existing is not None:
            # Observing a resource is not using it. `reconcile_resource` is an
            # "I am using this now" mutation (last_used = now, state = 'active'), and the
            # daemon idle-retires and restarts constantly, so calling it for rows that
            # already exist would: revert `bosn done` back to active (done workspaces
            # never collect again), reset last_used so idle/superseded age never
            # accumulates across a restart (age-based GC becomes inert and the disk grows
            # without bound -- the exact failure this project exists to prevent), and flip
            # 'adopted' to 'active', stripping the 24h quiet period so pressure can evict
            # data that recovery just restored. Reconciliation repairs the missing-row
            # crash boundary only; it must leave lifecycle state alone.
            continue
        registered = registry.reconcile_resource(
            kind=resource.kind,
            name=resource.name,
            stack=parsed.stack,
            generation=parsed.generation,
            scope=parsed.scope,
            workspace=parsed.workspace,
        )
        registry.record_generation(parsed.generation, parsed.stack, parsed.workspace)
        registry.set_resource_state(registered.id, "adopted")
        registry.log_event("resource.recovered", f"{resource.kind}:{resource.name}")
        reconciled.append(resource.name)
    discovered = {(resource.kind, resource.name) for resource in scan.owned}
    # Only rows observed before the scan are candidates.  A concurrent converge may
    # create a new engine object after listing completes; deleting that fresh row from a
    # stale scan would reintroduce the crash boundary this function repairs.
    for resource in prior_resources or []:
        if resource.kind in scan.scanned_kinds and (resource.kind, resource.name) not in discovered:
            registry.remove_resource(resource.id)
    return reconciled


def recompute_manifest_generations(registry: Registry, scan: ScanResult) -> int:
    """Record the current local-content generation for recoverable workspaces.

    Label values describe the generation at creation time.  When the workspace still
    exists, recovery must also restore the current manifest generation so old labeled
    resources become superseded after an edit made while SQLite was unavailable.
    External-image stacks record their content closure but defer supersession to normal
    daemon convergence: pulling or building during recovery would evade job cancellation.
    """
    from bosn.manifest import ManifestError, dockerfile_external_images, generation_digest, load

    refreshed = 0
    seen: set[tuple[str, str]] = set()
    for resource in scan.owned:
        parsed = resource.parsed()
        key = (parsed.workspace, parsed.stack)
        if key in seen:
            continue
        seen.add(key)
        try:
            manifest = load(Path(parsed.workspace))
            stack = manifest.stack(parsed.stack)
        except (ManifestError, OSError):
            continue
        has_external_identity = bool(
            stack.image or dockerfile_external_images(manifest.root, stack)
        )
        digest = generation_digest(manifest, stack)
        registry.record_generation(digest, parsed.stack, parsed.workspace)
        # A base-image identity is resolved only inside a managed build job.  Recording
        # the local content closure is still useful after recovery, but treating it as
        # the final identity would incorrectly retire a resource built from the same
        # Dockerfile with a resolved external image digest.
        if not has_external_identity:
            registry.supersede_generations(parsed.stack, digest, parsed.workspace)
        refreshed += 1
    return refreshed


def within_quiet_period(adopted_at: float, now: float) -> bool:
    """Adopted resources are protected from age-out for the quiet period."""
    return (now - adopted_at) < QUIET_PERIOD_SECONDS
