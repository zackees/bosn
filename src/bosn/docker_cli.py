"""The `bosn-docker` managed Docker/Compose front door."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Sequence
from pathlib import Path

from bosn import __version__, daemon, labels
from bosn.compose import ComposeError, content_digest, load_compose

# The category table (#46): every verb bosn-docker knows about, and whether it is
# implemented here (GOVERNED), passed verbatim to the real engine (FORWARD), or
# explicitly refused with a remedy (REFUSE). `resolve()` is the fail-closed entry point --
# it always returns a VerbSpec, turning a verb the table has never heard of into a REFUSE
# with a generic remedy rather than a `None` a dispatcher could mistake for "safe to
# forward". Dispatch below calls `resolve()`, never `spec_for()`, for that reason.
from bosn.frontdoor import Category, resolve, supported
from bosn.registry import Registry
from bosn.resources import process_start_time


class DockerFrontDoorError(ValueError):
    pass


# Environment marker set on the child's environment for exactly the duration bosn-docker
# spawns a forwarded or compose-invoked command. Its purpose is narrow: catch a `docker`
# shim (a future slice) that turns out to *be* bosn-docker resolving itself off PATH and
# calling back in here -- without it, that resolution loops forever. See
# `_resolve_forwarding_engine` for the full guard and its blind spots.
_RECURSION_GUARD_ENV = "BOSN_DOCKER_FORWARDING"


def _this_program() -> Path:
    """Best-effort absolute path to the file backing the running `bosn-docker` process."""
    return Path(sys.argv[0]).resolve()


def _resolve_real_engine(binary: str = "docker") -> Path | None:
    """Resolve `binary` to an absolute path via PATH, or None if nothing is found."""
    found = shutil.which(binary)
    if found is None:
        return None
    return Path(found).resolve()


def _is_this_program(candidate: Path) -> bool:
    """True when `candidate` is the same file as the running `bosn-docker` program.

    `Path.samefile` is preferred (works across symlinks/hardlinks, the way a shim would
    likely be installed) and falls back to a plain path comparison when the candidate
    does not exist or the filesystem does not support the identity check (e.g. across
    drives on Windows).
    """
    this = _this_program()
    try:
        return candidate.samefile(this)
    except OSError:
        return candidate == this


class _EngineResolutionError(DockerFrontDoorError):
    """Raised by `_resolve_forwarding_engine` when spawning the real engine is refused.

    Carries the structured `code`/`next_step` pair `_forward` needs for its JSON envelope,
    while still being a plain `DockerFrontDoorError` -- so a caller with no `--json` surface
    of its own (`_run_compose`) can simply let it propagate and rely on `str(exc)` reading
    as ordinary prose, the same as any other refusal `main()` already catches.
    """

    def __init__(self, *, code: str, message: str, next_step: str) -> None:
        super().__init__(message)
        self.code = code
        self.next_step = next_step


def _resolve_forwarding_engine() -> tuple[Path, dict[str, str]]:
    """Resolve the real engine binary and prepare its child env, refusing on recursion.

    Shared by `_forward` (FORWARD verbs, e.g. `bosn-docker version`) and `_run_compose`
    (the GOVERNED `compose` verb) -- both ultimately spawn "the real `docker`", and both
    must refuse identically once a `docker` shim that *is* bosn-docker exists on PATH.
    Before this function existed, `_run_compose` spawned bare `["docker", "compose", ...]`
    with neither check: a shim resolving back to this program would re-enter
    `bosn-docker compose`, which would spawn bare `docker compose` again -- an unbounded
    fork bomb. Factored once here rather than copied into both call sites, since two
    guards that can drift independently is how the second one quietly stops matching the
    first.

    Two layers, in order:

    1. `_RECURSION_GUARD_ENV` in the environment: set on every child this function's
       caller spawns. A shim invoked as that child inherits it and refuses immediately,
       before touching PATH resolution at all -- this is what stops a chain of
       self-invocations once one has already started.
    2. `_is_this_program()`: resolves "docker" via PATH and compares it, by file identity,
       against this program's own path. This is what stops the *first* hop -- the case
       where PATH's "docker" already is (a copy or symlink of) bosn-docker before any
       forwarding has happened yet, so there is no inherited env var to catch it.

    What this cannot catch: a shim that (a) is a distinct file from bosn-docker's own
    script -- so `samefile` does not match -- *and* (b) clears its inherited environment
    before re-invoking bosn-docker, dropping the marker. Neither mechanism alone covers
    that combination; it would need the shim itself to cooperate (e.g. by also checking
    its own resolved identity), which is out of scope for this slice.
    """
    if os.environ.get(_RECURSION_GUARD_ENV):
        raise _EngineResolutionError(
            code="docker.recursion",
            message=(
                "refusing to forward -- this process is already inside a bosn-docker "
                "forward, so the `docker` on PATH would call back into itself"
            ),
            next_step=(
                "run `bosn doctor` to check the docker shim, or invoke the real engine directly"
            ),
        )
    real = _resolve_real_engine("docker")
    if real is None:
        raise _EngineResolutionError(
            code="docker.no-engine",
            message="no `docker` found on PATH",
            next_step="install Docker (or podman) and ensure `docker` is on PATH",
        )
    if _is_this_program(real):
        raise _EngineResolutionError(
            code="docker.recursion",
            message=(
                f"the `docker` resolved from PATH is bosn-docker itself ({real}); "
                "forwarding would call back into this program"
            ),
            next_step=(
                "run `bosn doctor` to check the docker shim, or repair PATH to reach the "
                "real engine"
            ),
        )
    env = dict(os.environ)
    env[_RECURSION_GUARD_ENV] = str(os.getpid())
    return real, env


def _envelope(*, code: str, message: str, next_step: str, as_json: bool) -> int:
    """Emit the repo's stable refusal envelope, or readable prose when not asked for JSON.

    Deliberately not imported from `bosn.cli`: `bosn-docker` is a separate console entry
    point (see pyproject.toml), and reaching into another module's private helper would
    couple two independently-invoked programs for a four-line dict. The shape is copied,
    not the function.
    """
    if as_json:
        print(json.dumps({"ok": False, "code": code, "message": message, "next": next_step}))
    else:
        print(message, file=sys.stderr)
    return 1


def compose_to_manifest(source: Path) -> str:
    """Translate the portable image/build subset without silently accepting YAML we ignore.

    Reads through the parsed model rather than indentation regexes. The regexes could not
    see YAML: they resolved no anchors, so a service whose `image` arrived through a `<<:`
    merge looked like it had none, and `profiles:` -- the key the acceptance workload in
    #47 opens with -- was reported as an unsupported key rather than parsed.
    """
    try:
        services = load_compose(source).image_pairs()
    except ComposeError as exc:
        raise DockerFrontDoorError(str(exc)) from exc
    if not services:
        raise DockerFrontDoorError("compose file has no supported service image")
    stacks = []
    for name, image in services:
        default = str(len(stacks) == 0).lower()
        stacks.append(f'[stack.{name}]\nimage = "{image}"\ndefault = {default}\n')
    return "# Generated by bosn-docker init; review before committing.\n\n" + "\n".join(stacks)


def run_init(compose: Path, output: Path) -> Path:
    """Translate `compose` and write the manifest to `output`; the whole `init` verb.

    `compose_to_manifest` above already takes a plain `Path` and returns text, with no
    argparse or `bosn-docker`-specific handling in its way. This wraps the file-write and
    the no-clobber refusal around it so `bosn init` (#46: "Move Compose migration to `bosn
    init`") is a matter of `cli.py` calling this one function and catching
    `(OSError, DockerFrontDoorError)` -- not reimplementing `main()`'s `init` block.
    """
    text = compose_to_manifest(compose)
    if output.exists():
        raise DockerFrontDoorError(f"refusing to overwrite {output}; choose --output")
    output.write_text(text, encoding="utf-8")
    return output


def build_parser() -> argparse.ArgumentParser:
    """The top-level parser deliberately does not hard-code the verb surface.

    It used to: `sub.add_parser("init", ...)` / `sub.add_parser("compose", ...)` via
    `add_subparsers`, which makes argparse itself reject anything it didn't register --
    including verbs that should reach the REFUSE path with a structured remedy, and
    unknown verbs, which must refuse rather than bounce off argparse with a bare exit 2.
    So the verb is a plain optional positional and everything after it is REMAINDER;
    `bosn.frontdoor.VERBS` is consulted in `main()`, and is the only place a verb is
    declared. `init`/`compose` still get their own dedicated sub-parsing once the verb is
    known (see `_parse_init_args`/`_parse_compose_args`), so their flag surfaces and error
    messages are unchanged.
    """
    parser = argparse.ArgumentParser(prog="bosn-docker", description=__doc__)
    parser.add_argument("--version", action="version", version=__version__)
    parser.add_argument(
        "--supported",
        action="store_true",
        help="print the supported verb table (see bosn.frontdoor) and exit",
    )
    parser.add_argument(
        "--json", action="store_true", help="emit structured machine-readable output"
    )
    parser.add_argument("verb", nargs="?", help="docker verb; see --supported")
    parser.add_argument("args", nargs=argparse.REMAINDER, help="verb-specific arguments")
    return parser


def _parse_init_args(args: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="bosn-docker init")
    parser.add_argument("--compose", default="compose.yaml")
    parser.add_argument("--output", default="bosn.toml")
    return parser.parse_args(args)


def _parse_compose_args(args: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="bosn-docker compose")
    parser.add_argument("command", choices=["up", "down", "logs", "ps"])
    parser.add_argument("args", nargs=argparse.REMAINDER)
    parser.add_argument("-f", "--file", default="compose.yaml")
    return parser.parse_args(args)


def _yaml_scalar(value: str) -> str:
    """Quote a label value so a YAML parser reads it back byte-for-byte.

    Double quotes were the obvious choice and were wrong on Windows: YAML processes escape
    sequences inside them, so a workspace of `C:\\Users\\me` contains `\\U`, which YAML reads
    as the start of an 8-hex-digit unicode escape and rejects outright. Compose therefore
    could not parse its own generated overlay on the platform bosn is developed on.

    Single-quoted YAML performs no escape processing at all; the only special character is
    the quote itself, escaped by doubling.
    """
    return "'" + value.replace("'", "''") + "'"


def _compose_overlay(registry_id: str | Registry, compose: Path) -> Path:
    """Generate an ephemeral Compose overlay that labels every resource Compose creates.

    Services, top-level volumes, and top-level networks all get a full label
    contract. Compose's implicit `default` network is labeled too, since it is
    created whenever a service doesn't opt out -- even when the file declares no
    `networks:` section at all. Anonymous service-level volumes have no top-level
    key to attach labels to, so they cannot be governed through this overlay; the
    front door refuses to run rather than let them come up unlabeled.
    """
    if isinstance(registry_id, Registry):
        registry_id = registry_id.registry_id
    try:
        parsed = load_compose(compose)
    except ComposeError as exc:
        raise DockerFrontDoorError(str(exc)) from exc
    names = list(parsed.services)
    if not names:
        raise DockerFrontDoorError("compose file has no services")
    volume_names = list(parsed.volumes)
    network_names = list(parsed.networks)
    anonymous = [
        f"{service.name}:{mount}"
        for service in parsed.services.values()
        for mount in service.anonymous_volumes
    ]
    if anonymous:
        raise DockerFrontDoorError(
            "compose file declares anonymous service volumes, which Compose names at "
            "runtime -- there is no stable key an overlay can attach labels to, so they "
            f"would come up ungoverned: {', '.join(anonymous)}; declare them under a "
            "top-level `volumes:` entry using short `name:/path` syntax instead"
        )
    created = dt.datetime.now(dt.UTC).isoformat().replace("+00:00", "Z")
    workspace = str(compose.parent.resolve())
    # Every Compose resource used to carry the constant generation "compose", so every
    # project on the machine shared one generation string and nothing could ever be
    # superseded: editing the compose file rolled nothing, and the old resources stayed
    # current forever. A content digest gives Compose the same identity the manifest path
    # already has -- two invocations are compatible iff their digests are byte-equal.
    generation = content_digest(compose)

    def label_block(resource_names: list[str], kind: str) -> list[str]:
        block: list[str] = []
        for name in resource_names:
            contract = labels.ResourceLabels(
                registry=registry_id,
                kind=kind,
                stack=name,
                generation=generation,
                scope="stack",
                workspace=workspace,
                created=created,
            )
            block += [f"  {name}:", "    labels:"]
            block += [
                f"      {key}: {_yaml_scalar(value)}" for key, value in contract.to_dict().items()
            ]
        return block

    lines = ["services:"]
    lines += label_block(names, "container")
    if volume_names:
        lines.append("volumes:")
        lines += label_block(volume_names, "volume")
    lines.append("networks:")
    governed_networks = network_names if "default" in network_names else [*network_names, "default"]
    lines += label_block(governed_networks, "network")

    handle = tempfile.NamedTemporaryFile(
        mode="w", suffix=".bosn-compose.yaml", delete=False, encoding="utf-8"
    )
    try:
        handle.write("\n".join(lines) + "\n")
        return Path(handle.name)
    finally:
        handle.close()


def _reconcile_after_compose(command: str) -> None:
    """Adopt any labeled-but-unregistered resource left behind by a Compose invocation.

    Runs unconditionally -- on a clean exit, a non-zero exit, and on the way out of a
    KeyboardInterrupt -- because Compose labels (and creates) containers, networks, and
    volumes incrementally as it works through the file. A run that fails partway through
    pulling images, or is killed by Ctrl-C during a foreground `up`, can still leave fully
    labeled resources on the engine that the registry has never heard of: exactly the
    ungoverned accumulation bosn exists to prevent. Gating this on `command == "up" and
    returncode == 0` is precisely how those resources went unregistered.

    Paying for the scan on `down`/`logs`/`ps` too is a non-issue: those commands don't
    label new resources, adoption only registers what is not already known, so the scan
    finds nothing to do and returns immediately.

    A failure here is reported to stderr but never raised. Raising would surface as a
    `DockerFrontDoorError` in `main()`, which maps to exit code 1 regardless of what the
    compose command itself returned -- masking `up`'s real exit status behind a
    bookkeeping error the caller didn't ask about.
    """
    try:
        adopted = daemon.request("compose-adopt")
    except KeyboardInterrupt:
        raise
    except Exception as exc:  # daemon unreachable, IPC failure, etc.
        print(
            f"bosn-docker compose: reconcile after {command} failed: {exc}; "
            "resources created by this run may be unregistered",
            file=sys.stderr,
        )
        return
    if not adopted.get("ok"):
        print(
            f"bosn-docker compose: reconcile after {command} failed: "
            f"{adopted.get('error') or 'compose adoption failed'}; "
            "resources created by this run may be unregistered",
            file=sys.stderr,
        )


def _acquire_compose_lease(workspace: str) -> str | None:
    """Lease every resource already registered for this Compose project before it runs.

    Held under bosn-docker's own pid/start-time, sent explicitly in the request, rather
    than the daemon acquiring with its own `os.getpid()` the way `execution-acquire` does.
    `bosn-docker` runs the (possibly long, foreground) compose command itself; if it is
    SIGKILLed mid-run, no `finally` here ever executes and no release request is ever sent.
    A daemon-held lease would then sit pinned until the daemon itself restarts -- a
    permanent leak, exactly what leases exist to prevent. Held under the client's identity
    instead, the ordinary TTL-plus-liveness rule (`lease_is_expired`) reclaims it within one
    TTL once this pid (and start time, when available) no longer match a live process.

    Never fatal: a project with no resources registered yet (its first `up`, before this
    invocation's own reconcile above has anything to find) leases nothing and that is fine
    -- the orphan-recovery half of #48 is what protects a run's *own* newly created
    resources; this lease only protects what a concurrent GC pass could otherwise already
    see and evict out from under an in-progress run.
    """
    try:
        acquired = daemon.request(
            "compose-acquire",
            workspace=workspace,
            pid=os.getpid(),
            proc_start=process_start_time(os.getpid()),
        )
    except KeyboardInterrupt:
        raise
    except Exception as exc:  # noqa: BLE001 - fail open, same posture as reconcile below
        print(
            f"bosn-docker compose: could not lease project resources: {exc}; "
            "they are unprotected against pressure eviction for this run",
            file=sys.stderr,
        )
        return None
    if not acquired.get("ok"):
        print(
            f"bosn-docker compose: could not lease project resources: "
            f"{acquired.get('error') or 'lease acquire failed'}; "
            "they are unprotected against pressure eviction for this run",
            file=sys.stderr,
        )
        return None
    return str(acquired["session"])


def _release_compose_lease(session: str | None) -> None:
    """Release a lease acquired above. A `None` session (acquire failed or leased nothing
    worth tracking) is a normal, silent no-op -- there is nothing on the daemon side to
    release, and it must never be reported as a failure of the run itself.
    """
    if session is None:
        return
    try:
        daemon.request("compose-release", session=session)
    except KeyboardInterrupt:
        raise
    except Exception as exc:  # noqa: BLE001 - never mask compose's own exit status
        print(f"bosn-docker compose: could not release project lease: {exc}", file=sys.stderr)


def _run_compose(command: str, compose: Path, args: list[str]) -> int:
    if args:
        raise DockerFrontDoorError(f"unsupported compose flag or argument {args[0]!r}")
    # Resolved before touching the daemon at all: once a `docker` shim that is
    # bosn-docker itself exists on PATH (a later slice of #46), spawning bare
    # `["docker", "compose", ...]` below would resolve to that shim, which re-enters
    # `bosn-docker compose`, which spawns bare `docker compose` again -- an unbounded
    # fork bomb. `_EngineResolutionError` is a `DockerFrontDoorError`, so a refusal here
    # propagates straight through to `main()`'s existing except clause with no `--json`
    # handling of its own to add.
    real, engine_env = _resolve_forwarding_engine()
    reply = daemon.request("status")
    if not reply.get("ok"):
        raise DockerFrontDoorError(str(reply.get("error") or "cannot reach bosn daemon"))
    overlay = _compose_overlay(str(reply["registry_id"]), compose)
    workspace = str(compose.parent.resolve())
    try:
        # Reconcile before acquiring: a lease can only protect a resource the registry
        # already knows about, and a project that is already up -- this is a second
        # `compose up`, or a `logs`/`ps` against a live stack -- has resources sitting on
        # the engine from a prior invocation that this process never registered itself.
        _reconcile_after_compose(command)
        session = _acquire_compose_lease(workspace)
        try:
            completed = subprocess.run(
                [str(real), "compose", "-f", str(compose), "-f", str(overlay), command],
                check=False,
                env=engine_env,
            )
        finally:
            # Release and reconcile both have to happen -- whether the subprocess returned
            # normally, raised, or is unwinding from a Ctrl-C KeyboardInterrupt -- and
            # neither may swallow a failure in the other. Nesting the two `finally` blocks
            # guarantees the inner one (reconcile) still runs even if releasing the lease
            # raises, and the exception from whichever ran second is what actually
            # propagates -- same ordering guarantee `try/finally` always gives, just applied
            # twice. Release goes first only because it can't discover anything reconcile's
            # scan doesn't need: releasing does not add or remove resource rows.
            try:
                _release_compose_lease(session)
            finally:
                _reconcile_after_compose(command)
        return completed.returncode
    finally:
        overlay.unlink(missing_ok=True)


def _forward(verb: str, args: list[str], *, as_json: bool) -> int:
    """Pass a FORWARD verb's argv verbatim to the real engine.

    The recursion guard itself lives in `_resolve_forwarding_engine`, shared with
    `_run_compose` -- see that function's docstring for why the two must not drift apart.
    This wrapper only translates a refusal into `_forward`'s own JSON-or-prose envelope,
    prefixed with the verb the caller typed.
    """
    try:
        real, env = _resolve_forwarding_engine()
    except _EngineResolutionError as exc:
        return _envelope(
            code=exc.code,
            message=f"bosn-docker {verb}: {exc}",
            next_step=exc.next_step,
            as_json=as_json,
        )
    completed = subprocess.run([str(real), verb, *args], check=False, env=env)
    return completed.returncode


def _dispatch_from_table(verb: str, args: list[str], *, as_json: bool) -> int:
    """Route every verb that is not one of the two GOVERNED verbs implemented directly
    in `main()`. Uses `frontdoor.resolve()`, not `spec_for()`: `resolve()` is the
    fail-closed wrapper that turns a verb the table has never heard of into an explicit
    REFUSE row with a generic remedy, so there is no `None` case here to get wrong --
    unknown never means forward.
    """
    spec = resolve(verb)
    if spec.category is Category.FORWARD:
        return _forward(verb, args, as_json=as_json)
    if spec.category is Category.REFUSE:
        return _envelope(
            code="docker.refused",
            message=f"bosn-docker {verb}: {spec.summary}",
            next_step=spec.remedy or "run `bosn-docker --supported --json` for supported verbs",
            as_json=as_json,
        )
    # spec.category is Category.GOVERNED here: declared in the table but not one of the
    # verbs main() implements directly (init/compose). Refuse rather than silently no-op.
    return _envelope(
        code="docker.not-implemented",
        message=f"bosn-docker {verb}: declared governed but not yet wired in this build",
        next_step=spec.remedy or "see the project tracker for this verb's landing phase",
        as_json=as_json,
    )


def main(argv: Sequence[str] | None = None) -> int:
    raw_argv = list(argv if argv is not None else sys.argv[1:])
    # Scanned from the raw tokens, the same way `bosn.cli` decides `json_errors` -- not
    # read off the parsed namespace. `args` is `argparse.REMAINDER`, so a `--json` typed
    # after the verb (`bosn-docker rm --json`, the natural place to put it) is swallowed
    # into the verb's own argv rather than bound to the top-level flag; forwarded verbs
    # need that swallowing intact so the flag reaches the real engine verbatim, but
    # refusals still need to know the caller asked for JSON however it was spelled.
    as_json = "--json" in raw_argv
    parser = build_parser()
    ns = parser.parse_args(raw_argv)
    if ns.supported:
        print(json.dumps(supported()))
        return 0
    if ns.verb is None:
        parser.print_help()
        return 0
    if ns.verb == "init":
        init_ns = _parse_init_args(ns.args)
        try:
            output = run_init(Path(init_ns.compose), Path(init_ns.output))
        except (OSError, DockerFrontDoorError) as exc:
            print(f"bosn-docker init: {exc}", file=sys.stderr)
            return 1
        print(f"wrote {output}")
        return 0
    if ns.verb == "compose":
        compose_ns = _parse_compose_args(ns.args)
        try:
            return _run_compose(compose_ns.command, Path(compose_ns.file), list(compose_ns.args))
        except (OSError, DockerFrontDoorError) as exc:
            print(f"bosn-docker compose: {exc}", file=sys.stderr)
            return 1
    return _dispatch_from_table(ns.verb, ns.args, as_json=as_json)


def compose_main(argv: Sequence[str] | None = None) -> int:
    """Entry point for the `bosn-compose` console script (#46).

    `bosn-compose up` is exactly `bosn-docker compose up` -- reuses `main()`'s own verb
    dispatch by prepending the `compose` verb, rather than forking a second implementation
    of overlay generation, leasing, and reconcile that could drift from the one above.
    """
    raw_argv = list(argv if argv is not None else sys.argv[1:])
    return main(["compose", *raw_argv])
