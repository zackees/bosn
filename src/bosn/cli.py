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
import sqlite3
import sys
from collections.abc import Callable, Sequence
from typing import NoReturn

from bosn import __version__, ipc
from bosn.engine import Engine
from bosn.options import Options, from_namespace

DAEMON_VERB = "__daemon"

NOT_IMPLEMENTED_EXIT = 3

# Long enough to cover a daemon draining a cancelled build (see SHUTDOWN_DRAIN_SECONDS),
# short enough not to look hung.
DAEMON_STOP_TIMEOUT = 45.0


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
    "doctor": ("engine health and reachability", "implemented"),
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
        if verb in {"run", "tasks", "shell", "done", "ensure", "adopt"}:
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
    from bosn.registry import Registry, default_db_path

    db_path = (opts.state_dir / "registry.sqlite3") if opts.state_dir else default_db_path()
    if not db_path.exists():
        deadline = None
        registry_id = None
        integrity = "not initialized"
    else:
        with Registry(db_path, read_only=True) as registry:
            deadline = registry.meta("maintenance.next_deadline")
            registry_id = registry.registry_id
            integrity = registry.integrity_check()
    print(f"scheduler manifest installed: {autostart.manifest_installed()}")
    print(f"scheduler next deadline: {deadline or '-'}")
    print(f"registry integrity: {integrity}")
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
        return 1
    from bosn.resources import ResourceScanner

    scan = ResourceScanner(Engine(opts.engine)).scan(registry_id or "")
    if scan.foreign_registries:
        state = f" --state-dir {opts.state_dir}" if opts.state_dir else ""
        commands = "; ".join(
            f"bosn{state} adopt --from-registry {candidate}"
            for candidate in sorted(scan.foreign_registries)
        )
        print(
            "complete resources from foreign registry ids found; "
            f"choose recovery source: {commands}",
            file=sys.stderr,
        )
    return 0


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
        stopped = daemon_mod.stop(state_dir, timeout=DAEMON_STOP_TIMEOUT)
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
        reply = daemon_mod.request("jobs", opts.state_dir)
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


def cmd_run(opts: Options) -> int:
    from bosn import daemon as daemon_mod
    from bosn.config import ConfigError
    from bosn.config import load as load_config
    from bosn.converge import workspace_of
    from bosn.engine import EngineError
    from bosn.manifest import ManifestError, find_manifest, load

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
        )
        if not acquired.get("ok"):
            raise EngineError(str(acquired.get("error") or "execution acquire failed"))
        try:
            result = Engine(opts.engine).run(
                ["exec", str(acquired["container"]), *command],
                timeout=config.get("run_max_duration"),
            )
        finally:
            daemon_mod.request("execution-release", opts.state_dir, session=acquired["session"])
        code, output = result.returncode, result.stdout or result.stderr
    except JobFailed as exc:
        print(str(exc), file=sys.stderr)
        return exc.exit_code
    except (daemon_mod.DaemonError, ipc.TransportError) as exc:  # fail closed, stay visible
        print(f"cannot reach the bosn daemon: {exc}", file=sys.stderr)
        return 1
    except (ManifestError, EngineError, ConfigError) as exc:
        print(str(exc), file=sys.stderr)
        return 1
    if output:
        print(output)
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


def cmd_status(opts: Options) -> int:
    from bosn.config import ConfigError
    from bosn.config import load as load_config
    from bosn.gc import status
    from bosn.registry import Registry, default_db_path

    state_dir = opts.state_dir
    db_path = (state_dir / "registry.sqlite3") if state_dir else default_db_path()
    if not db_path.exists():
        print(json.dumps({"registered": 0, "storage": "not initialized"}, indent=2))
        return 0
    try:
        with Registry(db_path, read_only=True) as registry:
            print(
                json.dumps(
                    status(
                        registry, Engine(opts.engine), config=load_config(flags=_policy_flags(opts))
                    ),
                    indent=2,
                )
            )
        return 0
    except ConfigError as exc:
        print(str(exc), file=sys.stderr)
        return 1


def cmd_gc(opts: Options) -> int:
    from bosn import daemon as daemon_mod
    from bosn.config import ConfigError
    from bosn.config import load as load_config

    try:
        flags = _policy_flags(opts)
        load_config(flags=flags)
        reply = daemon_mod.request(
            "gc", opts.state_dir, engine=opts.engine, dry_run=opts.dry_run, policy_flags=flags
        )
    except ConfigError as exc:
        return _error(
            code="policy.invalid",
            message=str(exc),
            next_step="correct the named policy value and retry",
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

    try:
        reply = daemon_mod.request(
            "adopt",
            opts.state_dir,
            engine=opts.engine,
            source_registry=opts.source_registry,
            transfer=list(opts.transfer),
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


def cmd_shell(opts: Options) -> int:
    from bosn import daemon as daemon_mod
    from bosn.converge import workspace_of
    from bosn.engine import EngineError
    from bosn.manifest import ManifestError, find_manifest, load

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
        )
        if not acquired.get("ok"):
            raise EngineError(str(acquired.get("error") or "execution acquire failed"))
        try:
            return Engine(opts.engine).interactive(
                ["exec", "-it", str(acquired["container"]), "sh"]
            )
        finally:
            daemon_mod.request("execution-release", opts.state_dir, session=acquired["session"])
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
    }
    handler = handlers.get(opts.verb)
    if handler is not None:
        if opts.json and opts.verb not in {"tasks", "gc", "adopt"}:
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
