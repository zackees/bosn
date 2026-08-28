"""The `bosn` command line entry point.

Every verb from the design is registered here. Verbs whose implementation has not landed
yet exit with a specific error naming the verb and the phase that will land it — never a
silent no-op and never a fallback to raw Docker.
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import shutil
import sqlite3
import sys
from collections.abc import Callable, Sequence
from typing import NoReturn

from bosn import __version__, ipc
from bosn.engine import CLOCK_SKEW_BUDGET_SECONDS, Engine
from bosn.options import Options, from_namespace

DAEMON_VERB = "__daemon"

# `gc` waits on a daemon-side `Collector.collect`, whose cost is nothing like the shared
# `ipc.DEFAULT_TIMEOUT` (10s) that every other verb inherits. Measured on a ~280-object
# development host: `docker system df -v` alone is 5.2s, the resource scan is 7-11s quiet
# and ~24s under load (see #99), and removals come on top of both and scale with how much
# there is to delete. 10s could not cover the *first* step, so `bosn gc` reported failure
# for collections that were proceeding normally (#110).
#
# 120s is sized to cover inventory plus a loaded scan plus a substantial removal pass with
# room to spare, rather than to the median case -- a `gc` that has real work to do is
# exactly when this budget matters, and exactly when the host is slowest.
#
# It is deliberately a per-call override rather than a bump to `ipc.DEFAULT_TIMEOUT`, for
# the same reason `compose-adopt` got its own in #99: that constant is shared by every
# verb, and widening it to fit the slowest one would silently loosen budgets for verbs
# that answer in milliseconds and should fail fast when they do not.
#
# Any fixed number here is ultimately a guess, because `gc`'s runtime scales with how much
# it deletes. #110 records the alternative -- making `gc` job-backed the way builds already
# are (`jobs.py`), so progress streams and no budget is needed at all -- as the real fix if
# this one ever proves too small.
GC_REQUEST_TIMEOUT_SECONDS = 120.0

# `adopt` always scans the engine, then processes each explicit transfer sequentially. Reserve a
# loaded-host scan/reconciliation margin here; `adopt_request_timeout_seconds()` adds both
# transfer-sized copy legs for every selected volume. Keeping this operation-specific preserves
# the shared 10-second control-plane budget used by ordinary verbs.
ADOPT_SCAN_REQUEST_TIMEOUT_SECONDS = 15 * 60.0
RECONCILE_VOLUME_REQUEST_TIMEOUT_SECONDS = ADOPT_SCAN_REQUEST_TIMEOUT_SECONDS + (2 * 30 * 60.0)


def adopt_request_timeout_seconds(transfer_count: int) -> float:
    from bosn.resources import VOLUME_TRANSFER_COPY_TIMEOUT_SECONDS

    return ADOPT_SCAN_REQUEST_TIMEOUT_SECONDS + (
        max(0, transfer_count) * 2 * VOLUME_TRANSFER_COPY_TIMEOUT_SECONDS
    )


NOT_IMPLEMENTED_EXIT = 3

# Long enough to cover a daemon draining a cancelled build (see SHUTDOWN_DRAIN_SECONDS),
# short enough not to look hung.
DAEMON_STOP_TIMEOUT = 45.0
# Execution acquire may have to remove an orphaned persistent container and recreate it.
# That bounded recovery legitimately exceeds the generic 10-second IPC control timeout on
# Docker Desktop, while the preceding image build already has its own streamed job timeout.
EXECUTION_ACQUIRE_TIMEOUT = 120.0
# `status` is a diagnostic, not an engine inventory request.  When the daemon control plane
# has lost a stream, wait only long enough to distinguish that failure, then read the durable
# registry proof rather than starting the slow Docker scan that made #119 look like a hang.
STATUS_DAEMON_TIMEOUT_SECONDS = 2.0
# `jobs` is also a read-only control-plane diagnostic. It must not start a fresh daemon
# (which cannot know about an old daemon's jobs) or inherit the generic 10-second wait.
JOBS_DAEMON_TIMEOUT_SECONDS = 2.0


def _policy_flags(opts: Options) -> dict[str, float | None]:
    """The complete CLI layer of the policy precedence stack."""
    from bosn.config import policy_keys

    return {key: getattr(opts, key) for key in policy_keys()}


_POLICY_FLAG_KEYS = (
    "container_idle_stop",
    "container_remove",
    "warm_volume_ttl",
    "superseded_cap",
    "shared_cache_ceiling",
    "run_max_duration",
    "idle_retire_seconds",
    "max_builds",
    "build_ttl_seconds",
)

_GLOBAL_VALUE_FLAGS = {
    "--engine",
    "--state-dir",
    "--manifest",
    *(f"--{key.replace('_', '-')}" for key in _POLICY_FLAG_KEYS),
}


def _add_policy_flags(parser: argparse.ArgumentParser, *, default: object) -> None:
    for key in _POLICY_FLAG_KEYS:
        parser.add_argument(f"--{key.replace('_', '-')}", type=float, default=default)


def _error(*, code: str, message: str, next_step: str, as_json: bool = False) -> int:
    """Emit the stable machine error envelope while preserving readable stderr."""
    if as_json:
        print(json.dumps({"ok": False, "code": code, "message": message, "next": next_step}))
    else:
        print(message, file=sys.stderr)
    return 1


class _JSONArgumentParser(argparse.ArgumentParser):
    """Suppress argparse prose when the caller explicitly requests JSON."""

    def error(self, message: str) -> NoReturn:
        print(
            json.dumps(
                {
                    "ok": False,
                    "code": "parse.invalid",
                    "message": message,
                    "next": "correct the command arguments and retry",
                }
            )
        )
        raise SystemExit(2)


# verb -> (help text, phase that lands it)
VERBS: dict[str, tuple[str, str]] = {
    "run": ("run an ad-hoc command in a stack", "implemented"),
    "shell": ("interactive session in the persistent container", "implemented"),
    "tasks": ("list manifest tasks, stacks, digests, registration state", "implemented"),
    "jobs": ("list daemon-owned jobs", "implemented"),
    "attach": ("attach to a daemon-owned job", "implemented"),
    "cancel": ("cancel a daemon-owned job", "implemented"),
    "status": ("tiers, leases, managed bytes vs ceiling, foreign registries", "implemented"),
    "gc": ("report or reclaim collectable resources", "implemented"),
    "done": ("mark this workspace finished; its caches become collectable", "implemented"),
    "ensure": ("pre-warm a stack without running a command", "implemented"),
    "adopt": ("recover labeled resources into this registry", "implemented"),
    "reconcile-volume": ("explicitly repair one incomplete manifest volume", "implemented"),
    "doctor": ("engine health and reachability", "implemented"),
    "daemon-stop": ("stop the running daemon (needed after upgrades)", "implemented"),
    "init": ("translate a Compose file into bosn.toml (alias: bosn-docker init)", "implemented"),
}


class VerbNotImplementedError(RuntimeError):
    def __init__(self, verb: str, phase: str) -> None:
        super().__init__(f"`bosn {verb}` is not implemented yet (lands in {phase}).")
        self.verb = verb
        self.phase = phase


def build_parser(*, json_errors: bool = False) -> argparse.ArgumentParser:
    parser_type = _JSONArgumentParser if json_errors else argparse.ArgumentParser
    parser = parser_type(prog="bosn", description="bosn - container lifecycle supervisor")
    parser.add_argument("--version", action="version", version=__version__)
    parser.add_argument(
        "--engine",
        default="docker",
        help="container engine binary to drive (default: docker)",
    )
    parser.add_argument(
        "--state-dir",
        default=None,
        help="override the bosn state directory (registry + daemon state)",
    )
    parser.add_argument(
        "--manifest", default=None, help="path to bosn.toml (default: nearest one upward)"
    )
    parser.add_argument(
        "--json", action="store_true", help="emit structured machine-readable output"
    )
    # Policy is global because every command must resolve exactly the same snapshot.
    # The daemon repeats its three operational flags after the verb for spawned argv
    # compatibility; the remaining flags belong before the verb like --engine.
    _add_policy_flags(parser, default=None)
    subparsers = parser.add_subparsers(dest="verb", metavar="VERB")
    for verb, (help_text, _) in VERBS.items():
        sub = subparsers.add_parser(verb, help=help_text)
        _add_policy_flags(sub, default=argparse.SUPPRESS)
        sub.add_argument("args", nargs=argparse.REMAINDER, help=argparse.SUPPRESS)
        if verb in {"run", "tasks", "shell", "done", "ensure", "adopt", "reconcile-volume"}:
            sub.add_argument("--stack", default=None, help="stack to use (default: the default)")
            sub.add_argument("--task", default=None, help="run a manifest task by name")
            sub.add_argument("--manifest", default=None, dest="sub_manifest")
        sub.add_argument("--json", action="store_true", default=argparse.SUPPRESS)
        if verb == "adopt":
            sub.add_argument(
                "--from-registry",
                dest="source_registry",
                default=None,
                help="recover this lost registry identity (required when several are found)",
            )
            sub.add_argument(
                "--transfer",
                action="append",
                default=[],
                metavar="KIND:NAME",
                help="explicitly transfer one detached volume by staged copy and recreation",
            )
            sub.add_argument(
                "--legacy",
                dest="legacy",
                default=None,
                metavar="FAMILY",
                help=(
                    "adopt resources from a documented pre-bosn producer contract "
                    "(clud, soldr, zccache); unknown names remain manual"
                ),
            )
            sub.add_argument(
                "--yes",
                dest="yes",
                action="store_true",
                default=False,
                help="apply the adoption; without it, only report what would be adopted",
            )
        if verb == "reconcile-volume":
            sub.add_argument(
                "--volume",
                default=None,
                metavar="LOGICAL_NAME",
                help=(
                    "declared volume name from the selected stack; engine names are never accepted"
                ),
            )
            sub.add_argument(
                "--apply",
                dest="reconcile_apply",
                action="store_true",
                default=False,
                help="apply the exact recovery after preview and --yes confirmation",
            )
            sub.add_argument(
                "--yes",
                dest="yes",
                action="store_true",
                default=False,
                help="confirm the explicit legacy recovery when used with --apply",
            )
        if verb == "init":
            # Matches `bosn-docker init`'s flag surface (see docker_cli._parse_init_args)
            # so the two verbs are interchangeable per #46.
            sub.add_argument("--compose", default="compose.yaml", help="Compose file to read")
            sub.add_argument(
                "--output",
                default="bosn.toml",
                help="manifest path to write (refuses to overwrite)",
            )
        if verb == "gc":
            group = sub.add_mutually_exclusive_group()
            group.add_argument(
                "--dry-run",
                dest="dry_run",
                action="store_true",
                default=True,
                help="report what would be reclaimed (default)",
            )
            group.add_argument(
                "--apply",
                dest="dry_run",
                action="store_false",
                help="actually reclaim. There is deliberately no --force: automatic "
                "deletion always requires complete ownership proof",
            )
        if verb == "daemon-stop":
            sub.set_defaults(stop=True)

    daemon_parser = subparsers.add_parser(DAEMON_VERB, help=argparse.SUPPRESS)
    daemon_parser.add_argument("--port", type=int, default=None)
    _add_policy_flags(daemon_parser, default=argparse.SUPPRESS)
    daemon_parser.add_argument("--stop", action="store_true", help="stop a running daemon")
    autostart = daemon_parser.add_mutually_exclusive_group()
    autostart.add_argument(
        "--autostart",
        action="store_const",
        const=True,
        dest="autostart",
        help="start the maintenance daemon automatically at login",
    )
    autostart.add_argument(
        "--no-autostart",
        action="store_const",
        const=False,
        dest="autostart",
        help="remove bosn's per-user login launcher",
    )
    # Accepted after the verb as well, so a spawned argv need not order its flags.
    daemon_parser.add_argument("--state-dir", default=None, dest="daemon_state_dir")
    return parser


def cmd_doctor(opts: Options) -> int:
    from bosn import autostart

    info = Engine(opts.engine).info()
    print(f"engine binary:  {info.binary}")
    print(f"client version: {info.client_version or '-'}")
    print(f"server version: {info.server_version or '-'}")
    print(f"reachable:      {'yes' if info.reachable else 'no'}")
    if info.clock_skew_seconds is None:
        print("clock skew:     unavailable")
        clock_unsafe = False
    else:
        print(f"clock skew:     {info.clock_skew_seconds:+.3f}s")
        clock_unsafe = abs(info.clock_skew_seconds) > CLOCK_SKEW_BUDGET_SECONDS
        if clock_unsafe:
            print(
                f"engine clock differs from the client by more than "
                f"{CLOCK_SKEW_BUDGET_SECONDS:.1f}s; synchronize the Docker host clock "
                "before relying on warm incremental builds",
                file=sys.stderr,
            )
    from bosn.registry import Registry, default_db_path

    db_path = (opts.state_dir / "registry.sqlite3") if opts.state_dir else default_db_path()
    if not db_path.exists():
        deadline = None
        registry_id = None
        integrity = "not initialized"
        local_resources = None
        local_leases = None
        local_sessions = None
    else:
        # A corrupt registry must still produce a diagnosis, never a traceback: doctor is
        # the tool the user reaches for precisely when the database is damaged, and the
        # recovery guidance below is printed from `integrity`. Opening, reading meta, and
        # reading the registry id can each fail on damage severe enough to stop the pager.
        try:
            with Registry(db_path, read_only=True) as registry:
                deadline = registry.meta("maintenance.next_deadline")
                registry_id = registry.registry_id
                integrity = registry.integrity_check()
                local_resources = len(registry.list_resources())
                local_leases = len(registry.all_leases())
                local_sessions = len(registry.execution_sessions())
        except sqlite3.DatabaseError as exc:
            deadline = None
            registry_id = None
            integrity = f"unreadable: {exc}"
            local_resources = None
            local_leases = None
            local_sessions = None
    print(f"scheduler manifest installed: {autostart.manifest_installed()}")
    print(f"scheduler next deadline: {deadline or '-'}")
    print(f"registry integrity: {integrity}")
    # A `docker` shim redirects every Docker invocation on the machine, so whether one is
    # installed -- and whether the real engine is still reachable past it -- is exactly the
    # kind of thing you want stated plainly when something is behaving strangely.
    # `shims.status()` is documented never to raise or mutate, which is what makes it safe
    # to call from a read-only diagnostic.
    from bosn import shims

    shim_status = shims.status()
    print(f"docker shims: {shim_status.detail}")
    if shim_status.conflicts:
        print(
            f"  not installed by bosn, left untouched: {', '.join(shim_status.conflicts)}",
            file=sys.stderr,
        )
    if db_path.exists() and integrity != "ok":
        backup = db_path.with_suffix(".backup.sqlite3")
        recovered = db_path.with_suffix(".recovered.sql")
        print(
            "registry integrity check failed; back up before attempting recovery, "
            "then recover into a new file -- never overwrite the original:",
            file=sys.stderr,
        )
        print(f"  sqlite3 {db_path} \"VACUUM INTO '{backup}'\"", file=sys.stderr)
        print(f"  sqlite3 {db_path} .recover > {recovered}", file=sys.stderr)
        print(
            "inspect the recovered SQL before replacing anything; the original "
            f"{db_path} is left untouched by these commands",
            file=sys.stderr,
        )
    if not info.reachable:
        print(f"diagnosis:      {info.detail}", file=sys.stderr)
        if info.failure_category == "docker_desktop_wedged":
            from bosn.accounting import configured_desktop_vhdx_allocation

            allocation = configured_desktop_vhdx_allocation()
            print("engine resource inventory: unavailable")
            if info.desktop_evidence is not None:
                desktop = "running" if info.desktop_evidence.desktop_running else "unavailable"
                wsl = "running" if info.desktop_evidence.wsl_distro_running else "unavailable"
                print(f"Docker Desktop observation: {desktop}")
                print(f"docker-desktop WSL observation: {wsl}")
            print(
                "local registered resources: "
                f"{local_resources if local_resources is not None else 'unavailable'}"
            )
            print(f"local leases: {local_leases if local_leases is not None else 'unavailable'}")
            print(
                "local execution sessions: "
                f"{local_sessions if local_sessions is not None else 'unavailable'}"
            )
            if allocation is not None:
                print(
                    "configured Docker Desktop VHDX: "
                    f"{allocation.path} ({allocation.allocated_bytes / 1024**3:.1f} GiB "
                    "allocated; allocation only)"
                )
                try:
                    volume = shutil.disk_usage(allocation.path)
                except OSError:
                    print("configured VHDX volume free space: unavailable")
                else:
                    print(f"configured VHDX volume free space: {volume.free / 1024**3:.1f} GiB")
            print(
                "Docker Desktop engine appears wedged; restart Docker Desktop manually, "
                "then rerun `bosn doctor`. "
                "Bosn will not restart Docker or alter WSL, the VHDX, registry, "
                "or engine resources.",
                file=sys.stderr,
            )
        return 1
    from bosn.resources import ResourceScanner

    scan = ResourceScanner(Engine(opts.engine)).scan(registry_id or "")
    if scan.failed_kinds:
        # #117: the engine answered the reachability probe and then took longer than the
        # 60-second command deadline to list a kind -- the signature of a host with enough
        # objects on it that diagnosis matters most. Everything printed above stands; the
        # inventory does not, and it is reported as unknown rather than as an empty
        # inventory. The foreign-registry report below is skipped for the same reason:
        # `adopt --from-registry <id>` advice derived from a listing that never finished
        # would be guidance built on absence of evidence.
        scanned = ", ".join(sorted(scan.scanned_kinds)) or "none"
        print(f"engine resource inventory: incomplete (scanned: {scanned})")
        for kind, reason in sorted(scan.failed_kinds.items()):
            print(f"  {kind}: could not be listed -- {reason}")
        failed = ", ".join(sorted(scan.failed_kinds))
        print(
            f"engine resource inventory is incomplete: the engine is reachable but "
            f"listing these kinds did not complete: {failed}. bosn cannot say what "
            "exists on the engine. "
            "Rerun `bosn doctor` once the engine is less loaded. No ownership or "
            "foreign-registry conclusion is drawn from a partial scan, and nothing "
            "was changed.",
            file=sys.stderr,
        )
        return 1
    if scan.foreign_registries:
        _report_foreign_registries(scan, opts)
    return 1 if clock_unsafe else 0


# A foreign label only proves "not this registry" -- it is not, and cannot be, proof of
# death. The same machine can host another live bosn instance (a different user, a
# different --state-dir, a CI runner) whose registry id will show up here forever, by
# design: `resources.py` refuses to ever delete or downgrade a foreign resource on its
# own say-so. So this report counts and points at the biggest holders; it never claims
# orphaned/dead/safe-to-remove, because bosn has no signal that would make that claim true.
#
# Threshold: above this many distinct foreign ids, one command per id stops being
# actionable (nobody reads, let alone runs, a 150-command line) and starts being noise
# that buries the one thing doctor still owes the reader: how big the situation is. Below
# it -- the case this feature was built for, recovering a single lost prior identity --
# the exact `adopt --from-registry <id>` command is still printed because it is the whole
# point of running doctor in the first place.
_FOREIGN_REGISTRY_COMMAND_THRESHOLD = 5
_FOREIGN_REGISTRY_TOP_N = 5


def _report_foreign_registries(scan, opts: Options) -> None:
    from collections import Counter

    per_registry = Counter(resource.registry for resource in scan.foreign if resource.registry)
    registry_ids = sorted(per_registry)
    total_resources = sum(per_registry.values())
    state = f" --state-dir {opts.state_dir}" if opts.state_dir else ""

    if len(registry_ids) <= _FOREIGN_REGISTRY_COMMAND_THRESHOLD:
        commands = "; ".join(
            f"bosn{state} adopt --from-registry {candidate}" for candidate in registry_ids
        )
        print(
            "resources labeled with a registry id other than this one were found "
            "(bosn cannot tell whether that registry is gone or still in use elsewhere -- "
            f"complete foreign resources are never touched automatically); recovery options "
            f"if one of these was this machine's own prior identity: {commands}",
            file=sys.stderr,
        )
        return

    top = ", ".join(
        f"{registry_id} ({count} resource{'' if count == 1 else 's'})"
        for registry_id, count in per_registry.most_common(_FOREIGN_REGISTRY_TOP_N)
    )
    print(
        f"{total_resources} resources belong to {len(registry_ids)} registry ids other than "
        "this one (bosn cannot tell a prior identity of this machine from a registry still "
        "in use elsewhere -- these are never touched automatically); "
        f"largest by resource count: {top}; recover an id you recognize with "
        f"bosn{state} adopt --from-registry <id>",
        file=sys.stderr,
    )


def cmd_daemon(opts: Options) -> int:
    from bosn import daemon as daemon_mod
    from bosn.config import ConfigError
    from bosn.config import load as load_config

    if opts.autostart is not None:
        from bosn import autostart

        target = autostart.enable() if opts.autostart else autostart.disable()
        print(f"autostart {'enabled' if opts.autostart else 'disabled'}: {target}")
        return 0

    state_dir = opts.state_dir

    if opts.stop:
        # Three outcomes, not two. Stopping now drains in-flight builds before it closes
        # the registry, so a daemon with a build to cancel can outlast the wait -- and
        # reporting that as "no daemon was running" would be a plainly wrong answer to the
        # question the user asked.
        was_running = daemon_mod.is_serving(state_dir)
        try:
            stopped = daemon_mod.stop(state_dir, timeout=DAEMON_STOP_TIMEOUT)
        except daemon_mod.DaemonError as exc:
            print(str(exc), file=sys.stderr)
            return 1
        if stopped:
            print("daemon stopped")
        elif was_running:
            print(
                "daemon is still shutting down; it is waiting for an in-flight build to "
                "stop cleanly. Run `bosn jobs` to see what it is finishing.",
                file=sys.stderr,
            )
            return 1
        else:
            print("no daemon was running")
        return 0

    try:
        config = load_config(flags=_policy_flags(opts))
        daemon = daemon_mod.Daemon(
            state_dir=state_dir,
            port=opts.port,
            idle_retire_seconds=config.get("idle_retire_seconds"),
            max_builds=int(config.get("max_builds")),
            build_ttl_seconds=config.get("build_ttl_seconds"),
            engine_binary=opts.engine,
            config=config,
        )
        return daemon.serve_forever()
    except ConfigError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    except daemon_mod.DaemonError as exc:
        print(str(exc), file=sys.stderr)
        return 1


def cmd_jobs(opts: Options) -> int:
    from bosn import daemon as daemon_mod

    try:
        reply = daemon_mod.request(
            "jobs",
            opts.state_dir,
            autostart=False,
            request_timeout=JOBS_DAEMON_TIMEOUT_SECONDS,
            diagnostic=True,
        )
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:  # fail closed, stay visible
        print(f"cannot reach the bosn daemon: {exc}", file=sys.stderr)
        return 1
    print(json.dumps(reply.get("jobs", []), indent=2))
    return 0


def _open_manifest_and_registry(opts: Options, *, read_only: bool = False):
    from bosn.manifest import ManifestError, find_manifest, load
    from bosn.registry import Registry, default_db_path

    manifest_path = opts.manifest or find_manifest()
    if manifest_path is None:
        raise ManifestError(
            "no bosn.toml found in this directory or any parent; create one or pass --manifest"
        )
    manifest = load(manifest_path)
    state_dir = opts.state_dir
    db_path = (state_dir / "registry.sqlite3") if state_dir else default_db_path()
    return manifest, Registry(db_path, read_only=read_only)


def cmd_tasks(opts: Options) -> int:
    from bosn.manifest import ManifestError, find_manifest, generation_digest, load
    from bosn.registry import Registry, default_db_path

    try:
        manifest_path = opts.manifest or find_manifest()
        if manifest_path is None:
            raise ManifestError(
                "no bosn.toml found in this directory or any parent; create one or pass --manifest"
            )
        manifest = load(manifest_path)
    except ManifestError as exc:
        return _error(
            code="manifest.invalid",
            message=str(exc),
            next_step="create or select a valid bosn.toml with --manifest",
            as_json=opts.json,
        )

    db_path = (opts.state_dir / "registry.sqlite3") if opts.state_dir else default_db_path()
    registered = []
    registry_reason = "registry not initialized"
    if db_path.exists():
        try:
            with Registry(db_path, read_only=True) as registry:
                registered = registry.list_resources()
        except (OSError, sqlite3.DatabaseError) as exc:
            return _error(
                code="registry.unreadable",
                message=f"cannot read registry: {exc}",
                next_step="run `bosn doctor` and follow its SQLite recovery guidance",
                as_json=opts.json,
            )
        registry_reason = "daemon job state unavailable; run `bosn jobs`"

    def readiness(stack_name: str) -> dict[str, object]:
        resources = [
            resource
            for resource in registered
            if resource.stack == stack_name and resource.workspace == str(manifest.root)
        ]
        return {
            "state": "ready" if resources else "unregistered",
            "resources": len(resources),
            "generations": sorted({resource.generation for resource in resources}),
            "jobs": {"state": "unavailable", "reason": registry_reason},
        }

    try:
        payload = {
            "manifest": str(manifest.path),
            "stacks": {
                name: {
                    "dockerfile": stack.dockerfile,
                    "image": stack.image,
                    "family": stack.family,
                    "default": stack.default,
                    # Discovery is engine-independent, so this is deliberately the local
                    # content key. Converge resolves external images into the canonical
                    # generation digest before job coalescing and registration.
                    "content_digest": generation_digest(manifest, stack),
                    "volumes": {v.name: v.scope for v in stack.volumes},
                    "readiness": readiness(name),
                }
                for name, stack in manifest.stacks.items()
            },
            "tasks": {
                name: {
                    "stack": t.stack,
                    "cmd": t.cmd,
                    "readiness": readiness(t.stack or manifest.default_stack().name),
                }
                for name, t in manifest.tasks.items()
            },
        }
    except ManifestError as exc:
        return _error(
            code="manifest.digest_failed",
            message=str(exc),
            next_step="fix the manifest or Dockerfile inputs, then retry",
            as_json=opts.json,
        )
    print(json.dumps(payload, indent=2))
    return 0


class JobFailed(RuntimeError):
    """A daemon-owned job ended without producing a usable generation."""

    def __init__(self, message: str, exit_code: int) -> None:
        super().__init__(message)
        self.exit_code = exit_code


def _drive_job(events, *, quiet: bool = False) -> dict:
    """Render a job's event stream to stderr and return its terminal event.

    Build output goes to stderr, never stdout: `bosn run`'s stdout belongs to the command
    the user asked to run, and an agent parsing that output must not find build noise in it.
    """
    final: dict = {}
    for event in events:
        kind = event.get("event")
        if kind == "log":
            if not quiet:
                print(event.get("line", ""), file=sys.stderr)
        elif kind == "submitted" and not quiet:
            note = _submission_note(event)
            if note:
                print(note, file=sys.stderr)
        elif kind == "attached" and not quiet:
            print(f"attached to {event.get('job')} ({event.get('state')})", file=sys.stderr)
        elif kind == "cancelling":
            print(f"build cancelling: {event.get('reason')}", file=sys.stderr)
        if event.get("final"):
            final = event
    if not final:
        raise JobFailed("the daemon ended the stream without a result", 1)
    return final


def _submission_note(event: dict) -> str | None:
    disposition = event.get("disposition")
    job = event.get("job")
    if event.get("joined"):
        return f"joined in-flight build {job} for the same generation"
    if disposition == "pending":
        return (
            f"queued as {job}: a build for an older generation of this stack is still "
            "running; it will start as soon as that one finishes"
        )
    if disposition == "queued":
        return None
    return None


# Distinct codes so a caller can tell "your build broke" from "your request was dropped".
BUILD_FAILED_EXIT = 1
SUPERSEDED_EXIT = 4
CANCELLED_EXIT = 5


def _result_or_raise(final: dict):
    """Turn a terminal job event into a ConvergeResult, or a specific, non-silent failure."""
    from bosn.converge import ConvergeResult

    state = final.get("state")
    if state == "succeeded":
        return ConvergeResult.from_dict(final.get("result") or {})
    reason = final.get("error") or state or "unknown failure"
    if state == "superseded":
        raise JobFailed(f"converge did not run: {reason}", SUPERSEDED_EXIT)
    if state == "cancelled":
        raise JobFailed(f"build cancelled: {reason}", CANCELLED_EXIT)
    raise JobFailed(f"build failed: {reason}", BUILD_FAILED_EXIT)


def _converge_via_daemon(opts: Options, manifest, stack_name: str | None):
    """Ask the daemon to converge, watching its build. The job outlives this process.

    Converge runs in the daemon rather than here so that killing this CLI cannot destroy a
    20-minute build, and so the registry keeps exactly one writer.
    """
    from bosn import daemon as daemon_mod
    from bosn.converge import workspace_of

    events = daemon_mod.stream(
        "converge",
        opts.state_dir,
        manifest=str(manifest.path),
        stack=stack_name,
        workspace=workspace_of(manifest),
        engine=opts.engine,
    )
    return _result_or_raise(_drive_job(events))


def _release_execution(daemon_mod, state_dir, session: object) -> str | None:
    """Release a foreground session without silently flattening cleanup failures."""
    try:
        released = daemon_mod.request("execution-release", state_dir, session=session)
    except KeyboardInterrupt:
        raise
    except Exception as exc:  # the caller decides whether another failure is authoritative
        return str(exc)
    if released.get("ok"):
        return None
    return str(released.get("error") or f"could not release execution session {session}")


def cmd_run(opts: Options) -> int:
    from bosn import daemon as daemon_mod
    from bosn.config import ConfigError
    from bosn.config import load as load_config
    from bosn.converge import workspace_of
    from bosn.engine import EngineError
    from bosn.manifest import ManifestError, find_manifest, load
    from bosn.resources import process_start_time

    command = opts.command
    if not command and not opts.task:
        print("nothing to run: pass a command after `--`, or name a task", file=sys.stderr)
        return 2

    try:
        manifest_path = opts.manifest or find_manifest()
        if manifest_path is None:
            raise ManifestError("no bosn.toml found; create one or pass --manifest")
        manifest = load(manifest_path)
    except ManifestError as exc:
        print(str(exc), file=sys.stderr)
        return 1

    try:
        config = load_config(flags=_policy_flags(opts))
        if opts.task:
            task = manifest.task(opts.task)
            command = ["sh", "-c", task.cmd]
            stack_name = task.stack or None
        else:
            stack_name = opts.stack

        converged = _converge_via_daemon(opts, manifest, stack_name)
        acquired = daemon_mod.request(
            "execution-acquire",
            opts.state_dir,
            manifest=str(manifest.path),
            result=converged.to_dict(),
            stack=stack_name,
            workspace=workspace_of(manifest),
            engine=opts.engine,
            pid=os.getpid(),
            proc_start=process_start_time(os.getpid()),
            request_timeout=EXECUTION_ACQUIRE_TIMEOUT,
        )
        if not acquired.get("ok"):
            raise EngineError(str(acquired.get("error") or "execution acquire failed"))
        code: int | None = None
        try:
            engine = Engine(opts.engine)
            exec_args = ["exec", str(acquired["container"]), *command]
            if opts.json:
                # JSON errors reserve stdout for exactly one parseable envelope. Capturing
                # is intentionally limited to this machine-facing mode; ordinary runs use
                # ``execute`` below and remain live on both native streams.
                try:
                    result = engine.execute_capture(
                        exec_args,
                        timeout=config.get("run_max_duration"),
                    )
                except KeyboardInterrupt:
                    engine._abort_container(str(acquired["container"]))
                    raise
                except EngineError as exc:
                    cleanup_error = engine._abort_container(str(acquired["container"]))
                    if cleanup_error:
                        raise EngineError(f"{exc}; {cleanup_error}") from exc
                    raise
                code = result.returncode
                if result.stdout:
                    print(result.stdout, file=sys.stderr if code else sys.stdout)
                if result.stderr:
                    print(result.stderr, file=sys.stderr)
            else:
                code = engine.execute(
                    exec_args,
                    timeout=config.get("run_max_duration"),
                    abort_container=str(acquired["container"]),
                )
        finally:
            cleanup_error = _release_execution(daemon_mod, opts.state_dir, acquired["session"])
            if cleanup_error:
                detail = (
                    f"execution cleanup failed for session {acquired['session']}: {cleanup_error}"
                )
                # Never replace the command's own exception or non-zero exit. A successful
                # command, however, must not claim success while leaving reuse blocked.
                if sys.exception() is not None or (code is not None and code != 0):
                    print(detail, file=sys.stderr)
                else:
                    raise EngineError(detail)
    except JobFailed as exc:
        print(str(exc), file=sys.stderr)
        return exc.exit_code
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:  # fail closed, stay visible
        print(f"cannot reach the bosn daemon: {exc}", file=sys.stderr)
        return 1
    except (ManifestError, EngineError, ConfigError) as exc:
        print(str(exc), file=sys.stderr)
        return 1
    assert code is not None
    return code


def cmd_attach(opts: Options) -> int:
    from bosn import daemon as daemon_mod

    job_id = next(iter(opts.command), "")
    if not job_id:
        print("attach needs a job id; see `bosn jobs`", file=sys.stderr)
        return 2
    try:
        # autostart=False: attaching asks about a job that already exists, and a daemon we
        # just started cannot have one. Spawning here would spend 30s to answer "no such
        # job" instead of the truth, which is that nothing is running.
        final = _drive_job(daemon_mod.stream("attach", opts.state_dir, autostart=False, job=job_id))
        # Same terminal-event handling as `run`, so a superseded job exits 4 here too
        # rather than being flattened into a generic failure.
        _result_or_raise(final)
    except JobFailed as exc:
        print(str(exc), file=sys.stderr)
        return exc.exit_code
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        print(f"cannot reach the bosn daemon: {exc}", file=sys.stderr)
        return 1
    print(f"job {job_id} succeeded", file=sys.stderr)
    return 0


def cmd_cancel(opts: Options) -> int:
    from bosn import daemon as daemon_mod

    job_id = next(iter(opts.command), "")
    if not job_id:
        print("cancel needs a job id; see `bosn jobs`", file=sys.stderr)
        return 2
    try:
        reply = daemon_mod.request("cancel", opts.state_dir, autostart=False, job=job_id)
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        print(f"cannot reach the bosn daemon: {exc}", file=sys.stderr)
        return 1
    if not reply.get("ok"):
        print(str(reply.get("error") or "cancel failed"), file=sys.stderr)
        return 1
    print(f"cancelled {job_id}")
    return 0


def _persisted_execution_sessions(registry, *, daemon_control_available: bool) -> list[dict]:
    """Describe durable foreground ownership without contacting the engine or daemon.

    The registry proves that a session is protected, but never authorizes removal.  In
    particular, a dead owner remains blocking until the daemon's documented recovery path
    has removed the exact immutable container and released its leases.
    """
    from bosn.resources import process_alive

    sessions = []
    for session in registry.execution_sessions():
        alive = process_alive(session.client_pid, session.client_start)
        last_reap_error = registry.latest_event(
            "execution.orphan_reap.error", detail_prefix=f"session={session.id} "
        )
        sessions.append(
            {
                "id": session.id,
                "container_id": session.container_id,
                "engine": session.engine_binary,
                "client_pid": session.client_pid,
                "client_start": session.client_start,
                "client_alive": alive,
                "lease_ids": list(session.lease_ids),
                "blocking_reason": (
                    "client is live"
                    if alive
                    else "client is dead; awaiting safe exact-container reap"
                ),
                "last_orphan_reap_error": (
                    {"at": last_reap_error["at"], "detail": last_reap_error["detail"]}
                    if last_reap_error is not None
                    else None
                ),
                "recovery": (
                    "do not interrupt the live client"
                    if alive
                    else (
                        "run `bosn daemon-stop`; it reaps only this exact container after "
                        "confirming the client is dead"
                        if daemon_control_available
                        else "the daemon control channel is unavailable; restore or restart "
                        "Bosn through its supported launcher/service first. Once `bosn "
                        "status` responds, run `bosn daemon-stop` for exact-container reap"
                    )
                ),
            }
        )
    return sessions


def cmd_status(opts: Options) -> int:
    from bosn import daemon as daemon_mod
    from bosn.registry import Registry, default_db_path

    state_dir = opts.state_dir
    db_path = (state_dir / "registry.sqlite3") if state_dir else default_db_path()
    if not db_path.exists():
        print(
            json.dumps(
                {
                    "mode": "offline",
                    "registry_id": None,
                    "registered": 0,
                    "execution_sessions": [],
                    "daemon": {"reachable": False, "error": "registry not initialized"},
                    "next": (
                        "no Bosn registry or daemon is available yet. Run the desired Bosn "
                        "command to initialize its managed state; do not create resources with "
                        "raw Docker."
                    ),
                },
                indent=2,
            )
        )
        return 0
    # Status is deliberately a bounded control-plane and registry diagnostic, never an engine
    # inventory.  That keeps it useful exactly when Docker Desktop or a foreground stream is
    # stuck; `gc --dry-run --json` remains the explicit rich engine/storage report.
    daemon_error: str | None = None
    daemon_control_lost = False
    try:
        daemon_status = daemon_mod.request(
            "status",
            state_dir,
            autostart=False,
            request_timeout=STATUS_DAEMON_TIMEOUT_SECONDS,
            diagnostic=True,
        )
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        daemon_status = None
        daemon_error = str(exc)
        daemon_control_lost = isinstance(exc, ipc.TransportError)
    if daemon_status and daemon_status.get("ok", True):
        print(
            json.dumps(
                {
                    "mode": "online",
                    "registry_id": daemon_status.get("registry_id"),
                    "registered": daemon_status.get("resources", 0),
                    "execution_sessions": daemon_status.get("execution_sessions", []),
                    "daemon": {
                        "reachable": True,
                        "pid": daemon_status.get("pid"),
                        "version": daemon_status.get("version"),
                    },
                    "next": (
                        "a live session remains protected; for a dead owner, `bosn daemon-stop` "
                        "safely reaps only its exact container"
                        if daemon_status.get("execution_sessions")
                        else "no foreground execution session is blocking the daemon"
                    ),
                },
                indent=2,
            )
        )
        return 0
    if daemon_status is not None:
        daemon_error = str(daemon_status.get("error") or "daemon did not return status")
    with Registry(db_path, read_only=True) as registry:
        persisted_sessions = _persisted_execution_sessions(registry, daemon_control_available=False)
        print(
            json.dumps(
                {
                    "mode": "degraded" if daemon_control_lost else "offline",
                    "registry_id": registry.registry_id,
                    "registered": len(registry.list_resources()),
                    "execution_sessions": persisted_sessions,
                    "daemon": {
                        "reachable": False,
                        "error": daemon_error,
                        "status_timeout_seconds": STATUS_DAEMON_TIMEOUT_SECONDS,
                    },
                    "next": (
                        "the daemon control channel is unavailable, so do not run `bosn "
                        "daemon-stop` yet: it uses that same channel. Restore or restart Bosn "
                        "through its supported launcher/service first; once `bosn status` "
                        "responds, `bosn daemon-stop` can attempt safe exact-container recovery."
                        if persisted_sessions
                        else "no execution session is persisted. Restore or restart Bosn through "
                        "its supported launcher/service before retrying a mutating command."
                    ),
                },
                indent=2,
            )
        )
    return 0


def cmd_gc(opts: Options) -> int:
    from bosn import daemon as daemon_mod
    from bosn.config import ConfigError
    from bosn.config import load as load_config
    from bosn.manifest import ManifestError, find_manifest, load

    try:
        flags = _policy_flags(opts)
        load_config(flags=flags)
        # GC stays global when no manifest is present. When one is available, load it
        # before IPC so a relative or custom filename becomes the stable source path the
        # daemon must inspect, while its manifest root remains the collision context.
        manifest_path = opts.manifest or find_manifest()
        manifest = load(manifest_path) if manifest_path is not None else None
        reply = daemon_mod.request(
            "gc",
            opts.state_dir,
            engine=opts.engine,
            dry_run=opts.dry_run,
            policy_flags=flags,
            manifest=str(manifest.path) if manifest is not None else None,
            request_timeout=GC_REQUEST_TIMEOUT_SECONDS,
        )
    except ConfigError as exc:
        return _error(
            code="policy.invalid",
            message=str(exc),
            next_step="correct the named policy value and retry",
            as_json=opts.json,
        )
    except ManifestError as exc:
        return _error(
            code="manifest.invalid",
            message=str(exc),
            next_step="create or select a valid bosn.toml with --manifest",
            as_json=opts.json,
        )
    except ipc.TransportTimeout as exc:
        # Ordered before the `TransportError` clause below, which it is a subclass of.
        # A timeout here does not mean the daemon is absent -- it means it is still
        # collecting -- so it must not inherit that clause's "start or restart the daemon"
        # remedy. Restarting is the one action that would interrupt the very work being
        # waited on, and the collection continues regardless of this client giving up.
        return _error(
            code="gc.timeout",
            message=(
                f"the daemon did not finish collecting within "
                f"{GC_REQUEST_TIMEOUT_SECONDS:g}s: {exc}"
            ),
            next_step=(
                "the daemon is still collecting -- do not restart it; check `bosn status` "
                "or the event log, and retry once it settles"
            ),
            as_json=opts.json,
        )
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        return _error(
            code="daemon.unreachable",
            message=f"cannot reach the bosn daemon: {exc}",
            next_step="start or restart the daemon, then retry",
            as_json=opts.json,
        )
    if not reply.get("ok"):
        return _error(
            code="gc.failed",
            message=str(reply.get("error") or "gc failed"),
            next_step="inspect `bosn status` and retry after resolving the reported error",
            as_json=opts.json,
        )
    if opts.json:
        print(
            json.dumps(
                {
                    "ok": not bool(reply.get("errors")),
                    "result": reply["result"],
                    "dry_run": opts.dry_run,
                    "would_stop": reply.get("would_stop", []),
                    "stopped": reply.get("stopped", []),
                    "removed": reply.get("removed", []),
                    "image_dependency_deferred": reply.get("image_dependency_deferred", []),
                    "image_decisions": reply.get("image_decisions", []),
                    "unproven_resources": reply.get("unproven_resources", []),
                    "errors": reply.get("errors", []),
                    "advisories": reply.get("advisories", []),
                },
                indent=2,
            )
        )
        return 1 if reply.get("errors") else 0
    print(json.dumps({**reply["result"], "dry_run": opts.dry_run}, indent=2))
    for name in reply.get("would_stop", []):
        print(f"would stop {name}")
    for name in reply.get("stopped", []):
        print(f"stopped {name}")
    for name in reply.get("removed", []):
        print(("would remove " if opts.dry_run else "removed ") + name)
    for name in reply.get("image_dependency_deferred", []):
        print(f"deferred image {name}")
    for message in reply.get("errors", []):
        print(f"error: {message}", file=sys.stderr)
    for advisory in reply.get("advisories", []):
        print(f"advisory: {advisory}", file=sys.stderr)
    return 1 if reply.get("errors") else 0


def cmd_done(opts: Options) -> int:
    from bosn import daemon as daemon_mod
    from bosn.converge import workspace_of
    from bosn.manifest import ManifestError, find_manifest, load

    try:
        manifest_path = opts.manifest or find_manifest()
        if manifest_path is None:
            raise ManifestError("no bosn.toml found; create one or pass --manifest")
        manifest = load(manifest_path)
        reply = daemon_mod.request("done", opts.state_dir, workspace=workspace_of(manifest))
    except ManifestError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        print(f"cannot reach the bosn daemon: {exc}", file=sys.stderr)
        return 1
    if not reply.get("ok"):
        print(str(reply.get("error") or "done failed"), file=sys.stderr)
        return 1
    print(f"marked {reply['marked']} resource(s) in {manifest.root} as done")
    return 0


def cmd_adopt_legacy(opts: Options) -> int:
    """``adopt --legacy <family>``: documented pre-bosn producer contracts, no name guessing.

    Unlike lost-registry recovery and explicit ``--transfer``, this path never mutates the
    registry directly from the CLI (writes to it always go through the daemon -- see
    ``bosn/daemon.py``'s ``compose-adopt`` verb). It only recreates qualifying volumes with
    bosn's label contract via the engine, then asks the daemon to register whatever now
    carries our identity -- the same idempotent step ``bosn/docker_cli.py`` already runs
    after ``docker compose up``.
    """
    from bosn import daemon as daemon_mod
    from bosn import legacy
    from bosn.engine import Engine
    from bosn.resources import ResourceScanner

    try:
        family = legacy.resolve_family(str(opts.legacy))
    except legacy.UnknownLegacyFamilyError as exc:
        return _error(
            code="adopt.unknown_legacy_family",
            message=str(exc),
            next_step=f"choose one of: {', '.join(legacy.known_families())}",
            as_json=opts.json,
        )

    try:
        status = daemon_mod.request("status", opts.state_dir)
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        return _error(
            code="daemon.unreachable",
            message=f"cannot reach the bosn daemon: {exc}",
            next_step="start or restart the daemon, then retry",
            as_json=opts.json,
        )
    if not status.get("ok"):
        return _error(
            code="adopt.failed",
            message=str(status.get("error") or "could not read current registry identity"),
            next_step="run `bosn doctor` and retry",
            as_json=opts.json,
        )
    registry_id = str(status["registry_id"])

    import time

    engine = Engine(opts.engine)
    plan = legacy.plan_adoption(
        ResourceScanner(engine), family, registry_id=registry_id, now=time.time()
    )
    eligible_names = [entry.resource.name for entry in plan.eligible]
    skipped_names = [resource.name for resource in plan.skipped_immutable]
    refused_report = [
        {"name": resource.name, "reason": reason} for resource, reason in plan.refused
    ]

    if not opts.yes:
        report = {
            "ok": False,
            "code": "adopt.confirmation_required",
            "message": (
                f"--legacy {family.name} would adopt {len(eligible_names)} volume(s); "
                "no changes applied without --yes"
            ),
            "next": "re-run the same command with --yes to apply",
            "family": family.name,
            "would_adopt": eligible_names,
            "skipped_immutable": skipped_names,
            "refused": refused_report,
            "applied": False,
        }
        if opts.json:
            print(json.dumps(report))
        else:
            print(report["message"], file=sys.stderr)
            for name in eligible_names:
                print(f"  would adopt: {name}", file=sys.stderr)
            for name in skipped_names:
                print(f"  skipped (not a volume, cannot relabel): {name}", file=sys.stderr)
            for item in refused_report:
                print(f"  refused: {item['name']}: {item['reason']}", file=sys.stderr)
        return 1

    from bosn.resources import TransferError

    try:
        legacy.apply_plan(engine, plan)
    except TransferError as exc:
        return _error(
            code="adopt.legacy_relabel_failed",
            message=str(exc),
            next_step="resolve the reported engine error and retry",
            as_json=opts.json,
        )

    try:
        registered = daemon_mod.request("compose-adopt", opts.state_dir)
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        return _error(
            code="daemon.unreachable",
            message=(
                f"relabeled {len(eligible_names)} volume(s) but could not reach the daemon "
                f"to register them: {exc}"
            ),
            next_step="run `bosn adopt --from-registry` or restart the daemon and retry",
            as_json=opts.json,
        )
    if not registered.get("ok"):
        return _error(
            code="adopt.failed",
            message=str(registered.get("error") or "registration after relabel failed"),
            next_step="run `bosn doctor` and retry",
            as_json=opts.json,
        )

    result = {
        "adopted": eligible_names,
        "skipped_immutable": skipped_names,
        "refused": refused_report,
        "family": family.name,
        "registry_id": registry_id,
        "applied": True,
    }
    print(json.dumps(result, indent=2))
    return 0


def cmd_adopt(opts: Options) -> int:
    """The sole ownership-transfer/recovery entry point for complete label contracts."""
    from bosn import daemon as daemon_mod

    if opts.legacy:
        return cmd_adopt_legacy(opts)

    request_timeout = adopt_request_timeout_seconds(len(opts.transfer))
    try:
        reply = daemon_mod.request(
            "adopt",
            opts.state_dir,
            engine=opts.engine,
            source_registry=opts.source_registry,
            transfer=list(opts.transfer),
            request_timeout=request_timeout,
        )
    except ipc.TransportTimeout as exc:
        return _error(
            code="adopt.timeout",
            message=(f"the daemon did not finish adoption within {request_timeout:g}s: {exc}"),
            next_step=(
                "the daemon may still be transferring data -- do not restart it or start an "
                "overlapping adoption; inspect `bosn status` and the preserved staging volume"
            ),
            as_json=opts.json,
        )
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        return _error(
            code="daemon.unreachable",
            message=f"cannot reach the bosn daemon: {exc}",
            next_step="start or restart the daemon, then retry",
            as_json=opts.json,
        )
    if not reply.get("ok"):
        return _error(
            code="adopt.failed",
            message=str(reply.get("error") or "adopt failed"),
            next_step="run `bosn doctor` and retry",
            as_json=opts.json,
        )
    adopted = list(reply.get("adopted") or [])
    transferred = list(reply.get("transferred") or [])
    if not adopted and not transferred:
        print("no complete labeled resources found")
        return 0
    print(
        json.dumps(
            {
                "adopted": adopted,
                "transferred": transferred,
                "registry_id": reply.get("registry_id"),
            },
            indent=2,
        )
    )
    return 0


def cmd_reconcile_volume(opts: Options) -> int:
    """Preview or explicitly repair one incomplete volume derived from bosn.toml."""
    from bosn import daemon as daemon_mod
    from bosn.manifest import ManifestError, find_manifest, load

    if not opts.stack or not opts.volume:
        return _error(
            code="reconcile-volume.stack_required",
            message="reconcile-volume requires --stack and --volume for an unambiguous target",
            next_step="pass the stack and logical volume declared by bosn.toml",
            as_json=opts.json,
        )
    try:
        manifest_path = opts.manifest or find_manifest()
        if manifest_path is None:
            raise ManifestError("no bosn.toml found; create one or pass --manifest")
        manifest = load(manifest_path)
        # Validate client-side too, so a typo never starts a daemon merely to be refused.
        stack = manifest.stack(opts.stack)
        if not any(volume.name == opts.volume for volume in stack.volumes):
            raise ManifestError(f"volume {opts.volume!r} is not declared by stack {stack.name!r}")
    except ManifestError as exc:
        return _error(
            code="reconcile-volume.invalid_manifest",
            message=str(exc),
            next_step="select a declared stack and logical volume",
            as_json=opts.json,
        )
    try:
        reply = daemon_mod.request(
            "reconcile-volume",
            opts.state_dir,
            manifest=str(manifest.path),
            stack=opts.stack,
            volume=opts.volume,
            apply=opts.reconcile_apply,
            yes=opts.yes,
            engine=opts.engine,
            request_timeout=RECONCILE_VOLUME_REQUEST_TIMEOUT_SECONDS,
        )
    except ipc.TransportTimeout as exc:
        return _error(
            code="reconcile-volume.timeout",
            message=f"the daemon did not finish volume reconciliation: {exc}",
            next_step=(
                "do not retry a transfer yet; inspect the preserved staging volume and status"
            ),
            as_json=opts.json,
        )
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        return _error(
            code="daemon.unreachable",
            message=f"cannot reach the bosn daemon: {exc}",
            next_step="start or restart the daemon, then retry",
            as_json=opts.json,
        )
    if opts.json:
        print(json.dumps(reply, indent=2))
    elif reply.get("plan"):
        print(json.dumps(reply["plan"], indent=2))
    elif not reply.get("ok"):
        print(str(reply.get("error") or "reconcile-volume failed"), file=sys.stderr)
    return 0 if reply.get("ok") else 1


def cmd_shell(opts: Options) -> int:
    from bosn import daemon as daemon_mod
    from bosn.converge import workspace_of
    from bosn.engine import EngineError
    from bosn.manifest import ManifestError, find_manifest, load
    from bosn.resources import process_start_time

    try:
        manifest_path = opts.manifest or find_manifest()
        if manifest_path is None:
            raise ManifestError("no bosn.toml found; create one or pass --manifest")
        manifest = load(manifest_path)
    except ManifestError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    try:
        converged = _converge_via_daemon(opts, manifest, opts.stack)
        acquired = daemon_mod.request(
            "execution-acquire",
            opts.state_dir,
            manifest=str(manifest.path),
            result=converged.to_dict(),
            stack=opts.stack,
            workspace=workspace_of(manifest),
            engine=opts.engine,
            pid=os.getpid(),
            proc_start=process_start_time(os.getpid()),
            request_timeout=EXECUTION_ACQUIRE_TIMEOUT,
        )
        if not acquired.get("ok"):
            raise EngineError(str(acquired.get("error") or "execution acquire failed"))
        code: int | None = None
        try:
            code = Engine(opts.engine).interactive(
                ["exec", "-it", str(acquired["container"]), "sh"]
            )
        finally:
            cleanup_error = _release_execution(daemon_mod, opts.state_dir, acquired["session"])
            if cleanup_error:
                detail = (
                    f"execution cleanup failed for session {acquired['session']}: {cleanup_error}"
                )
                if sys.exception() is not None or (code is not None and code != 0):
                    print(detail, file=sys.stderr)
                else:
                    raise EngineError(detail)
        assert code is not None
        return code
    except JobFailed as exc:
        print(str(exc), file=sys.stderr)
        return exc.exit_code
    except (daemon_mod.DaemonError, ipc.TransportError, EngineError) as exc:
        print(str(exc), file=sys.stderr)
        return 1


def cmd_ensure(opts: Options) -> int:
    """Build/register the requested generation without starting a command."""
    from bosn import daemon as daemon_mod
    from bosn.manifest import ManifestError, find_manifest, load

    try:
        manifest_path = opts.manifest or find_manifest()
        if manifest_path is None:
            raise ManifestError("no bosn.toml found; create one or pass --manifest")
        manifest = load(manifest_path)
    except ManifestError as exc:
        print(
            json.dumps({"ok": False, "error": str(exc), "next": "create-or-pass-manifest"}),
            file=sys.stderr,
        )
        return 1
    try:
        result = _converge_via_daemon(opts, manifest, opts.stack)
    except JobFailed as exc:
        print(json.dumps({"ok": False, "error": str(exc), "next": "inspect-jobs"}), file=sys.stderr)
        return exc.exit_code
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:
        print(json.dumps({"ok": False, "error": str(exc), "next": "start-daemon"}), file=sys.stderr)
        return 1
    print(json.dumps({"ok": True, "stack": result.stack, "digest": result.digest}))
    return 0


def cmd_init(opts: Options) -> int:
    """Translate a Compose file into a manifest; #46 moves this on-ramp under `bosn`.

    `bosn-docker init` remains a working alias (`docker_cli.main`'s own `init` branch),
    since both call the same `run_init` -- there is exactly one implementation of the
    translation, the write, and the no-clobber refusal to keep in sync.
    """
    from pathlib import Path

    from bosn.docker_cli import DockerFrontDoorError, run_init

    try:
        output = run_init(Path(opts.compose or "compose.yaml"), Path(opts.output or "bosn.toml"))
    except (OSError, DockerFrontDoorError) as exc:
        return _error(
            code="init.failed",
            message=str(exc),
            next_step="fix the Compose file, or pass --output to pick a different path, and retry",
            as_json=opts.json,
        )
    print(f"wrote {output}")
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    from bosn.paths import in_wsl

    if in_wsl():
        print(
            "bosn v1 does not support WSL: its Windows loopback daemon is unreachable "
            "from WSL; use a native Windows shell or wait for the v2 transport.",
            file=sys.stderr,
        )
        return 1
    raw_argv = list(argv if argv is not None else sys.argv[1:])
    # A manifest task is the friendly front door: `bosn unit` is equivalent to
    # `bosn run --task unit`.  Fixed verbs remain reserved, so adding a task called
    # `status` never changes the meaning of `bosn status`.
    command_index: int | None = None
    skip_value = False
    for index, token in enumerate(raw_argv):
        if skip_value:
            skip_value = False
            continue
        if token in _GLOBAL_VALUE_FLAGS:
            skip_value = True
            continue
        if token.startswith("--"):
            continue
        command_index = index
        break
    if command_index is not None and raw_argv[command_index] not in {*VERBS, DAEMON_VERB}:
        task = raw_argv[command_index]
        raw_argv[command_index : command_index + 1] = ["run", "--task", task]
    parser = build_parser(json_errors="--json" in raw_argv)
    ns = parser.parse_args(raw_argv)
    opts = from_namespace(ns)

    if opts.verb is None:
        parser.print_help()
        return 0

    handlers: dict[str, Callable[[Options], int]] = {
        DAEMON_VERB: cmd_daemon,
        "daemon-stop": cmd_daemon,
        "doctor": cmd_doctor,
        "jobs": cmd_jobs,
        "attach": cmd_attach,
        "cancel": cmd_cancel,
        "tasks": cmd_tasks,
        "run": cmd_run,
        "shell": cmd_shell,
        "ensure": cmd_ensure,
        "status": cmd_status,
        "gc": cmd_gc,
        "done": cmd_done,
        "adopt": cmd_adopt,
        "reconcile-volume": cmd_reconcile_volume,
        "init": cmd_init,
    }
    handler = handlers.get(opts.verb)
    if handler is not None:
        if opts.json and opts.verb not in {"tasks", "gc", "adopt", "init"}:
            # Older human-oriented verbs already return useful exit codes and write
            # diagnostics to stderr.  Adapt that boundary once so JSON callers never
            # need to parse prose while those commands are migrated individually.
            stdout = io.StringIO()
            stderr = io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                code = handler(opts)
            if code:
                message = (stderr.getvalue() + stdout.getvalue()).strip() or f"{opts.verb} failed"
                return _error(
                    code="command.failed",
                    message=message,
                    next_step="resolve the reported condition and retry the command",
                    as_json=True,
                )
            print(stdout.getvalue(), end="")
            print(stderr.getvalue(), end="", file=sys.stderr)
            return code
        return handler(opts)

    error = VerbNotImplementedError(opts.verb, VERBS[opts.verb][1])
    print(str(error), file=sys.stderr)
    return NOT_IMPLEMENTED_EXIT
