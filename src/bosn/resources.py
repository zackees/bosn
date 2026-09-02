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

import ctypes
import datetime as dt
import json
import os
import subprocess
import sys
import uuid
from collections.abc import Iterator
from dataclasses import dataclass, field
from pathlib import Path

from bosn import labels
from bosn.clock import Clock, SystemClock
from bosn.engine import Engine, EngineError
from bosn.registry import Registry, Resource

# A staged ownership transfer copies the complete cache twice: foreign volume -> staging,
# then staging -> same-named volume with the current registry labels. Warm compiler caches
# routinely exceed the generic 60-second engine-command budget; one real Rust target cache
# reproduced that cutoff in #113. Keep metadata operations fail-fast and give only the two
# data-plane copies a size-appropriate ceiling.
VOLUME_TRANSFER_COPY_TIMEOUT_SECONDS = 30 * 60.0

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

# Batch inspect commands, one per kind, that accept N names/ids on the command line and
# emit one JSON line per *found* object -- see `_batch_inspect_labels`. Each format string
# hand-builds a JSON object (`{"<id-field>": {{json .X}}, "Labels": {{json .Y}}}`) rather
# than relying on line position to recover which name a line belongs to. That is not
# cosmetic: verified against real Docker, a batch inspect where one of N names does not
# exist prints output for the ones that *were* found, writes an error for the missing one
# to stderr, and exits 1 -- and the missing name produces no line at all. With a plain
# `{{json .Labels}}` format (the single-name shape `_INSPECT_COMMANDS` still uses, since a
# single-name call has nothing to misalign), N names in and fewer than N lines out would
# make "which line is whose" a guess. Embedding the identity turns each line into a
# self-describing (name, labels) pair, so mapping is a dict build, not a zip().
#
# The identity field differs per kind because Docker's inspect JSON does: containers and
# volumes expose `.Name` (containers with a leading "/", stripped in
# `_batch_inspect_labels`),
# images expose only `.Id` -- there is no name at the image-inspect layer, mirroring why
# `_LIST_COMMANDS["image"]` passes `--no-trunc` and `_name_of` reads `.ID` for images --
# and networks expose `.Name`. Each was checked against a real `docker <kind> inspect`
# invocation rather than assumed from the single-name formats above, which is what caught
# the container leading-slash and the image having no `.Name` at all.
_BATCH_INSPECT_COMMANDS: dict[str, tuple[list[str], str]] = {
    "container": (
        ["inspect"],
        '{"Name":{{json .Name}},"Labels":{{json .Config.Labels}}}',
    ),
    "volume": (
        ["volume", "inspect"],
        '{"Name":{{json .Name}},"Labels":{{json .Labels}}}',
    ),
    "image": (
        ["image", "inspect"],
        '{"Id":{{json .Id}},"Labels":{{json .Config.Labels}}}',
    ),
    "network": (
        ["network", "inspect"],
        '{"Name":{{json .Name}},"Labels":{{json .Labels}}}',
    ),
}

# The JSON key `_BATCH_INSPECT_COMMANDS`' format string uses for each kind's identity,
# needed to read the right field back out of the parsed line in `_batch_inspect_labels`.
_BATCH_IDENTITY_KEY: dict[str, str] = {
    "container": "Name",
    "volume": "Name",
    "image": "Id",
    "network": "Name",
}

# Names per `docker <kind> inspect` invocation. Chunking exists purely so a host with
# thousands of unlabeled objects (this is exactly the population `_discover` is scanning
# when it falls back to inspect at all -- see the module docstring) cannot build a command
# line past the OS argv-length limit; it is not a batching-effectiveness knob. A few hundred
# is comfortably under every platform's limit (Windows CreateProcess ~32K chars is the
# tightest of the three) even for the longest identifiers this code handles (a 64-hex-char
# `sha256:` image id), and it still collapses a 255-object host from ~255 subprocesses down
# to a small constant per kind (issue #99).
INSPECT_BATCH_SIZE = 200


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
    # kind -> why its listing did not complete. The complement of `scanned_kinds`, carrying
    # the reason rather than only the absence, because an enumeration failure is the one
    # case where an empty bucket means "unknown" and not "none exist" (#117). Every
    # consumer that acts on absence already gates on `scanned_kinds`; this exists so the
    # ones that *report* -- doctor, gc's dry run -- can say which kind failed and why
    # instead of silently showing a short inventory.
    failed_kinds: dict[str, str] = field(default_factory=dict)

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


def _chunked(items: list[str], size: int) -> Iterator[list[str]]:
    """Split `items` into consecutive slices of at most `size`, preserving order.

    Extracted purely so `_batch_inspect_labels` reads as "for each chunk" rather than
    hand-rolled range/slice arithmetic at the call site -- there is no other behavior here.
    """
    for start in range(0, len(items), size):
        yield items[start : start + size]


def _labels_from_row(value: object) -> dict[str, str]:
    """Normalize a `Labels` field already parsed out of a batch-inspect JSON line.

    Unlike `_parse_labels`, the input here is a Python object (dict or `None`), not a raw
    string blob -- `_BATCH_INSPECT_COMMANDS`' hand-built format strings emit `{{json
    .Labels}}` as a nested JSON value inside the already-parsed outer object, so by the time
    this runs `json.loads` has already turned it into `dict | None` (Docker renders an
    unlabeled object's `.Labels` as JSON `null`, confirmed against real `docker image
    inspect`/`docker network inspect` output). Re-serializing back to a string just to hand
    it to `_parse_labels` would work but adds a pointless round trip for no shared logic
    beyond "stringify values, drop Nones", which is inlined here instead.
    """
    if not isinstance(value, dict):
        return {}
    return {str(k): str(v) for k, v in value.items() if v is not None}


def _name_of(kind: str, row: dict[str, object]) -> str:
    if kind == "volume":
        return str(row.get("Name", ""))
    if kind == "image":
        return str(row.get("ID", ""))
    # `container` rows carry `Names`; `network` rows carry `Name` (no plural) -- both fall
    # through to this shared branch rather than getting a dedicated one.
    return str(row.get("Names") or row.get("Name") or row.get("ID", ""))


def _listing_failure(args: list[str], outcome: str, stderr: str) -> str:
    """One line naming the command that failed and, when Docker said why, its first line."""
    detail = next((line.strip() for line in (stderr or "").splitlines() if line.strip()), "")
    rendered = f"docker {' '.join(args)} {outcome}"
    return f"{rendered}: {detail}" if detail else rendered


class ResourceScanner:
    """Enumerates engine resources and sorts them by ownership proof."""

    def __init__(self, engine: Engine | None = None) -> None:
        self.engine = engine or Engine()

    def _discover(self, kind: str) -> tuple[list[DiscoveredResource], str | None]:
        """List one kind. The second element is ``None`` on success, else why it failed.

        Only a *non-zero exit* is reported that way. A listing that never produced a
        result at all -- a blown deadline, a spawn failure -- raises `EngineError` out of
        `Engine.run` and keeps doing so here, because `discover()` has nowhere to put a
        reason; `scan()` is the caller that turns it into a failed kind (#117).
        """
        args = _LIST_COMMANDS.get(kind)
        if args is None:
            raise ValueError(f"cannot enumerate unknown kind {kind!r}")
        result = self.engine.run(args)
        if not result.ok:
            return [], _listing_failure(args, f"exited {result.returncode}", result.stderr)

        # Two passes rather than one: the list format truncates labels for some kinds, and
        # confirming a truncated-looking row used to mean "call `inspect_labels` right here,
        # one subprocess per such row" (issue #99). On a host with hundreds of foreign or
        # unlabeled objects -- not bounded by anything bosn owns, since it is whatever else
        # happens to exist on the machine -- that made a single `scan()` cost tens of
        # seconds and blow the IPC client's timeout. Collecting every name that needs
        # confirming first and resolving them together in one batched (and internally
        # chunked) call per kind is what turns that into O(kinds) subprocesses instead of
        # O(resources).
        rows: list[tuple[str, dict[str, str]]] = []
        incomplete_names: list[str] = []
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
            rows.append((name, raw))
            if not labels.is_complete(raw):
                incomplete_names.append(name)

        confirmed = self._batch_inspect_labels(kind, incomplete_names) if incomplete_names else {}
        discovered: list[DiscoveredResource] = []
        for name, raw in rows:
            if not labels.is_complete(raw):
                raw = confirmed.get(name) or raw
            discovered.append(DiscoveredResource(kind=kind, name=name, raw_labels=raw))
        return discovered, None

    def discover(self, kind: str) -> list[DiscoveredResource]:
        """List one kind; callers needing safety metadata should use :meth:`scan`."""
        discovered, _failure = self._discover(kind)
        return discovered

    def inspect_labels(self, kind: str, name: str) -> dict[str, str]:
        """Single-name inspect: the fallback path, not the hot path (see `_discover`)."""
        args = _INSPECT_COMMANDS.get(kind)
        if args is None:
            return {}
        result = self.engine.run([*args, name])
        if not result.ok:
            return {}
        return _parse_labels(result.stdout)

    def _batch_inspect_labels(self, kind: str, names: list[str]) -> dict[str, dict[str, str]]:
        """Resolve labels for many names in a handful of `docker <kind> inspect` calls.

        Chunked per `INSPECT_BATCH_SIZE` so this stays a small constant number of
        subprocesses regardless of host size, rather than one per name (the O(resources)
        cost issue #99 is about) or one unbounded command line (a host with thousands of
        objects could otherwise exceed the OS argv-length limit).

        Every name in `names` was just observed to exist by `_discover`'s `ls`, but Docker
        objects are not frozen between that listing and this inspect -- something else on
        the host (or a concurrent bosn operation) can remove one in between. Verified
        against real Docker: when that happens the batch command still prints a JSON line
        for every name it *did* find, writes an error to stderr for the one it didn't, and
        exits 1 for the whole invocation. So a nonzero exit does not mean "discard this
        chunk's output" -- `_BATCH_INSPECT_COMMANDS`' format embeds each row's identity
        precisely so the lines that did come back can still be trusted and mapped correctly
        even when the process itself reports failure.

        For whatever names a chunk's output did *not* account for -- true removal, or some
        other reason the whole invocation failed (permissions, a transient engine hiccup) --
        this falls back to `inspect_labels` one name at a time, i.e. exactly today's
        pre-#99 per-resource behavior. That fallback is scoped to the unresolved names only,
        not the whole chunk: names the batch call did resolve are not re-fetched just
        because a sibling name in the same chunk failed.
        """
        command = _BATCH_INSPECT_COMMANDS.get(kind)
        if command is None:
            return {}
        args_prefix, fmt = command
        identity_key = _BATCH_IDENTITY_KEY[kind]
        recovered: dict[str, dict[str, str]] = {}
        for chunk in _chunked(names, INSPECT_BATCH_SIZE):
            result = self.engine.run([*args_prefix, "--format", fmt, *chunk])
            found_in_chunk: set[str] = set()
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
                identity = row.get(identity_key)
                if not isinstance(identity, str):
                    continue
                if kind == "container":
                    # `docker inspect`'s `.Name` carries a leading "/" that `docker ps`'s
                    # `.Names` does not (confirmed against real Docker) -- without this the
                    # identity would never match what `_name_of` put in `names`.
                    identity = identity.lstrip("/")
                recovered[identity] = _labels_from_row(row.get("Labels"))
                found_in_chunk.add(identity)
            if not result.ok:
                for name in chunk:
                    if name in found_in_chunk:
                        continue
                    single = self.inspect_labels(kind, name)
                    if single:
                        recovered[name] = single
        return recovered

    def scan(self, registry_id: str, kinds: list[str] | None = None) -> ScanResult:
        """Enumerate every kind, bucketed by ownership proof, plus what could not be read.

        One kind failing does not abandon the rest. A resource-heavy host is exactly where
        `docker images` blows the 60-second engine deadline (#117) and also exactly where
        the remaining diagnosis is worth the most, so the `EngineError` becomes a recorded
        failed kind instead of an exception that discards three good listings. The kind is
        deliberately kept out of `scanned_kinds`: every decision that reads absence as
        removal gates on that set, so a failed kind stays invisible to it rather than
        turning "could not list" into "no longer exists".
        """
        scan = ScanResult()
        for kind in kinds or list(_LIST_COMMANDS):
            try:
                discovered, failure = self._discover(kind)
            except EngineError as exc:
                scan.failed_kinds[kind] = str(exc)
                continue
            if failure is None:
                scan.scanned_kinds.add(kind)
            else:
                scan.failed_kinds[kind] = failure
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


def container_image_references(engine: Engine) -> dict[str, str] | None:
    """Map every container name to its immutable image ID, or fail closed with ``None``.

    Stopped containers pin images just as running ones do, so this deliberately lists all
    containers. The list and every batched inspect must be complete and parseable; GC uses
    an unknown result to defer image removal rather than guessing that no dependency exists.
    """
    try:
        listed = engine.run(["ps", "--all", "--quiet", "--no-trunc"])
    except EngineError:
        return None
    if not listed.ok:
        return None
    identities = [line.strip() for line in listed.stdout.splitlines() if line.strip()]
    if len(identities) != len(set(identities)):
        return None

    references: dict[str, str] = {}
    fmt = '{"Name":{{json .Name}},"Image":{{json .Image}}}'
    for chunk in _chunked(identities, INSPECT_BATCH_SIZE):
        try:
            inspected = engine.run(["container", "inspect", "--format", fmt, *chunk])
        except EngineError:
            return None
        if not inspected.ok:
            return None
        rows = [line.strip() for line in inspected.stdout.splitlines() if line.strip()]
        if len(rows) != len(chunk):
            return None
        for line in rows:
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                return None
            if not isinstance(row, dict):
                return None
            name = row.get("Name")
            image_id = row.get("Image")
            if not isinstance(name, str) or not isinstance(image_id, str):
                return None
            name = name.lstrip("/")
            if not name or not image_id or name in references:
                return None
            references[name] = image_id
    return references


# -- leases ----------------------------------------------------------------


PROCESS_START_TOLERANCE_SECONDS = 2.0


def _windows_process_exists(pid: int) -> bool | None:
    """Read-only Windows liveness probe: True/False, or None when unavailable.

    `os.kill(pid, 0)` is a POSIX idiom, not a safe Windows probe: CPython may route a
    non-console signal through TerminateProcess, so asking whether another process exists
    can terminate it with exit code zero. OpenProcess with query-only rights cannot mutate
    the target and distinguishes a missing pid from a protected live process.
    """
    try:
        win_dll = getattr(ctypes, "WinDLL", None)
        get_last_error = getattr(ctypes, "get_last_error", None)
        if win_dll is None or get_last_error is None:
            return None
        kernel32 = win_dll("kernel32", use_last_error=True)
        open_process = kernel32.OpenProcess
        open_process.argtypes = [ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
        open_process.restype = ctypes.c_void_p
        handle = open_process(0x1000, 0, pid)  # PROCESS_QUERY_LIMITED_INFORMATION
        if handle:
            exit_code = ctypes.c_ulong()
            get_exit_code = kernel32.GetExitCodeProcess
            get_exit_code.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong)]
            get_exit_code.restype = ctypes.c_int
            queried = bool(get_exit_code(handle, ctypes.byref(exit_code)))
            close_handle = kernel32.CloseHandle
            close_handle.argtypes = [ctypes.c_void_p]
            close_handle.restype = ctypes.c_int
            close_handle(handle)
            if not queried:
                return None
            return exit_code.value == 259  # STILL_ACTIVE
        error = get_last_error()
    except (AttributeError, OSError):
        return None
    if error == 87:  # ERROR_INVALID_PARAMETER: no such process
        return False
    if error == 5:  # ERROR_ACCESS_DENIED: protected, but demonstrably present
        return True
    return None


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
    if os.name == "nt":
        exists = _windows_process_exists(pid)
        if exists is False:
            return False
        if exists is None:
            return True  # the read-only probe was unavailable: fail open
        if proc_start is None:
            return True
        actual = process_start_time(pid)
        return actual is None or abs(actual - proc_start) <= PROCESS_START_TOLERANCE_SECONDS
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except (SystemError, OSError):
        # A non-Windows runtime can still report ambiguous permission/platform errors.
        # Ask the OS-owned identity source rather than guessing: a creation time is positive
        # evidence of life; no process from two independent probes is enough to reclaim.
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
        # Preserved with everything else: a transfer changes who owns the volume, never how
        # expensive it is to recreate.
        retention=parsed.retention,
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
        ],
        timeout=VOLUME_TRANSFER_COPY_TIMEOUT_SECONDS,
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
        ],
        timeout=VOLUME_TRANSFER_COPY_TIMEOUT_SECONDS,
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
            # The label is the authority here -- rebuilding a lost registry is exactly the
            # case the label exists for. Absent means warm, never an invented pin.
            retention=parsed.retention or "warm",
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
            retention=parsed.retention or "warm",
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
