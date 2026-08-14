"""Converge-then-run.

Every stack verb makes registered state match the manifest -- registering, rolling a
generation, or reusing as-is -- and then runs. The same command is correct on the 1st and
the 500th invocation. Errors are reserved for states that need a human decision; an error
whose remedy is always the same mechanical command is just a forced retry loop.
"""

from __future__ import annotations

import datetime as dt
import threading
from collections.abc import Callable
from dataclasses import dataclass

from bosn import labels
from bosn.engine import Engine, EngineError
from bosn.manifest import Manifest, StackSpec, generation_digest
from bosn.registry import Registry

# What converge did, in the order of increasing work.
REUSED = "reused"
REGISTERED = "registered"
ROLLED = "rolled"


@dataclass(frozen=True)
class ConvergeResult:
    stack: str
    digest: str
    action: str
    image_tag: str
    volumes: tuple[str, ...] = ()
    superseded: int = 0

    def to_dict(self) -> dict[str, object]:
        """Converge runs in the daemon and its result is used by the CLI, so it travels."""
        return {
            "stack": self.stack,
            "digest": self.digest,
            "action": self.action,
            "image_tag": self.image_tag,
            "volumes": list(self.volumes),
            "superseded": self.superseded,
        }

    @classmethod
    def from_dict(cls, raw: dict[str, object]) -> ConvergeResult:
        volumes = raw.get("volumes") or []
        if not isinstance(volumes, (list, tuple)):
            volumes = []
        return cls(
            stack=str(raw.get("stack", "")),
            digest=str(raw.get("digest", "")),
            action=str(raw.get("action", "")),
            image_tag=str(raw.get("image_tag", "")),
            volumes=tuple(str(v) for v in volumes),
            superseded=int(raw.get("superseded", 0) or 0),  # type: ignore[arg-type]
        )


def _now_iso() -> str:
    return dt.datetime.now(dt.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def image_tag_for(stack: StackSpec, digest: str) -> str:
    """Images are keyed by digest, so a spec edit cannot reuse a stale image."""
    return f"bosn/{stack.name}:{digest.removeprefix('sha256:')[:16]}"


def volume_name_for(
    stack: StackSpec,
    volume_scope: str,
    volume_name: str,
    *,
    digest: str,
    workspace: str,
    family: str | None,
) -> str:
    """Volume identity follows its scope.

    - `spec`    keyed by the generation digest, so a spec edit gets a fresh volume
    - `stack`   keyed by workspace + stack, surviving spec edits in this workspace
    - `machine` keyed by family only -- one per machine, shared across every repo and
                worktree. This is what kills the incident's dominant multiplier.
    """
    short_digest = digest.removeprefix("sha256:")[:12]
    workspace_key = _stable_key(workspace)
    if volume_scope == "machine":
        return f"bosn-m-{family or stack.name}-{volume_name}"
    if volume_scope == "stack":
        return f"bosn-s-{stack.name}-{workspace_key}-{volume_name}"
    return f"bosn-g-{stack.name}-{short_digest}-{volume_name}"


def _stable_key(value: str) -> str:
    import hashlib

    return hashlib.sha256(value.encode("utf-8")).hexdigest()[:10]


class Converger:
    """Brings engine and registry state in line with the manifest."""

    def __init__(
        self,
        manifest: Manifest,
        registry: Registry,
        engine: Engine | None = None,
        *,
        progress: Callable[[str], None] | None = None,
        cancelled: threading.Event | None = None,
    ) -> None:
        self.manifest = manifest
        self.registry = registry
        self.engine = engine or Engine()
        # Set when a daemon job owns this converge: output goes to whoever is attached, and
        # the build stops when the job is cancelled.
        self.progress = progress
        self.cancelled = cancelled

    def converge(
        self, stack_name: str | None = None, *, workspace: str | None = None
    ) -> ConvergeResult:
        stack = self.manifest.stack(stack_name)
        digest = generation_digest(self.manifest, stack)
        workspace = workspace or workspace_of(self.manifest)

        known = self.registry.generation_superseded_at(digest)
        is_new_generation = known is None and not self._generation_recorded(digest)

        # Build first, register second. The ordering is load-bearing, not stylistic: a
        # build can now be cancelled (by `bosn cancel`, daemon shutdown, or the job TTL),
        # and a cancelled build must leave nothing behind that implies a usable image.
        #
        # Superseding is the sharp edge. Recording this generation retires every sibling,
        # and retention puts superseded generations on a 24-hour collection clock -- so
        # doing it before the build means one cancelled build marks the *previous, working*
        # image for early collection in favor of an image that never got built. Nothing
        # about this generation is written until `docker build` has exited 0.
        self._abort_if_cancelled()
        image_tag = image_tag_for(stack, digest)
        self._ensure_image(stack, digest, image_tag, workspace)

        # The last point at which stopping is free. Past here the registry gets written and
        # volumes get created, and `docker build` is no longer the only slow step -- so
        # without this checkpoint a cancel arriving during a *warm* converge (image already
        # present, nothing to build, nothing that consults the cancel flag) would be
        # reported as "cancelled" only after this generation had already superseded its
        # predecessor. Telling a user nothing happened while their previous generation went
        # on retention's 24-hour clock is worse than either outcome on its own.
        self._abort_if_cancelled()

        self.registry.record_generation(digest, stack.name)
        volumes = self._ensure_volumes(stack, digest, workspace)
        superseded = self.registry.supersede_generations(stack.name, keep_digest=digest)

        if superseded:
            action = ROLLED
        elif is_new_generation:
            action = REGISTERED
        else:
            action = REUSED

        self.registry.log_event("converge", f"{stack.name} {action} {digest[:19]}")
        return ConvergeResult(
            stack=stack.name,
            digest=digest,
            action=action,
            image_tag=image_tag,
            volumes=tuple(volumes),
            superseded=superseded,
        )

    def _abort_if_cancelled(self) -> None:
        if self.cancelled is not None and self.cancelled.is_set():
            raise EngineError("converge was cancelled")

    def _generation_recorded(self, digest: str) -> bool:
        # Through the registry's own accessor, not `registry.conn` directly: that accessor
        # holds the lock keeping this one sqlite connection single-writer, and converge now
        # runs on up to `max_builds` daemon threads at once rather than alone in the CLI.
        return self.registry.generation_recorded(digest)

    def _resource_labels(
        self, stack: StackSpec, kind: str, digest: str, scope: str, workspace: str
    ) -> labels.ResourceLabels:
        return labels.ResourceLabels(
            registry=self.registry.registry_id,
            kind=kind,
            stack=stack.name,
            generation=digest,
            scope=scope,
            workspace=workspace,
            created=_now_iso(),
        )

    def _ensure_image(self, stack: StackSpec, digest: str, tag: str, workspace: str) -> None:
        if stack.image:
            return  # a pulled image is not ours to label or collect
        if self.engine.run(["image", "inspect", tag]).ok:
            return

        dockerfile = self.manifest.root / str(stack.dockerfile)
        label_args = self._resource_labels(
            stack, "image", digest, "spec", workspace
        ).to_docker_args()
        result = self.engine.stream(
            [
                "build",
                "--file",
                str(dockerfile),
                "--tag",
                tag,
                *label_args,
                str(self.manifest.root),
            ],
            on_line=self.progress,
            cancelled=self.cancelled,
        )
        if not result.ok:
            if self.cancelled is not None and self.cancelled.is_set():
                raise EngineError(f"building {tag} was cancelled")
            raise EngineError(f"building {tag} failed: {result.stderr or result.stdout}")
        # Registration happens only after the build exits 0, which is what makes a
        # cancelled or failed build safe: it can never leave a generation row implying a
        # usable image exists. Do not hoist this above the build.
        self._register(stack, "image", tag, digest, "spec", workspace)

    def _ensure_volumes(self, stack: StackSpec, digest: str, workspace: str) -> list[str]:
        created: list[str] = []
        for volume in stack.volumes:
            name = volume_name_for(
                stack,
                volume.scope,
                volume.name,
                digest=digest,
                workspace=workspace,
                family=stack.family,
            )
            created.append(name)
            if self.engine.run(["volume", "inspect", name]).ok:
                continue
            label_args = self._resource_labels(
                stack, "volume", digest, volume.scope, workspace
            ).to_docker_args()
            result = self.engine.run(["volume", "create", *label_args, name])
            if not result.ok:
                raise EngineError(f"creating volume {name} failed: {result.stderr}")
            self._register(stack, "volume", name, digest, volume.scope, workspace)
        return created

    def _register(
        self, stack: StackSpec, kind: str, name: str, digest: str, scope: str, workspace: str
    ) -> None:
        self.registry.register_resource(
            kind=kind,
            name=name,
            stack=stack.name,
            generation=digest,
            scope=scope,
            workspace=workspace,
        )

    # -- running -----------------------------------------------------------

    def run(
        self,
        command: list[str],
        *,
        stack_name: str | None = None,
        workspace: str | None = None,
    ) -> tuple[ConvergeResult, int, str]:
        """Converge silently, then run the command in the stack. Returns (result, rc, output)."""
        workspace = workspace or workspace_of(self.manifest)
        converged = self.converge(stack_name, workspace=workspace)
        return self.run_converged(converged, command, stack_name=stack_name, workspace=workspace)

    def run_converged(
        self,
        converged: ConvergeResult,
        command: list[str],
        *,
        stack_name: str | None = None,
        workspace: str | None = None,
    ) -> tuple[ConvergeResult, int, str]:
        """Run a command against an already-converged generation.

        Split out from `run` because the two halves live in different processes once builds
        are daemon-owned: the daemon converges (it is the registry's writer and the owner
        of the long build), and the CLI runs the container itself, so the command keeps the
        caller's terminal and exit status.
        """
        workspace = workspace or workspace_of(self.manifest)
        stack = self.manifest.stack(stack_name)

        args = ["run", "--rm"]
        for volume in stack.volumes:
            name = volume_name_for(
                stack,
                volume.scope,
                volume.name,
                digest=converged.digest,
                workspace=workspace,
                family=stack.family,
            )
            args += ["--volume", f"{name}:/bosn/{volume.name}"]
        args += [converged.image_tag, *command]

        result = self.engine.run(args)
        self.registry.log_event("run", f"{converged.stack} rc={result.returncode}")
        return converged, result.returncode, result.stdout or result.stderr


def run_task(
    manifest: Manifest,
    registry: Registry,
    task_name: str,
    *,
    engine: Engine | None = None,
    workspace: str | None = None,
) -> tuple[ConvergeResult, int, str]:
    task = manifest.task(task_name)
    converger = Converger(manifest, registry, engine)
    return converger.run(["sh", "-c", task.cmd], stack_name=task.stack or None, workspace=workspace)


def workspace_of(manifest: Manifest) -> str:
    """The canonical workspace identity: the resolved manifest root, never the cwd.

    Everything that must serialize or run in parallel keys off this string -- the job
    table's `(workspace-id, stack)` key, stack-scoped volume names, and `bosn done`. Two
    agents in different subdirectories of one worktree find the same `bosn.toml`, so they
    get the same id and correctly serialize; two worktrees get different ids and correctly
    build in parallel.

    It has to be canonical, not merely convenient. `resolve()` collapses symlinks and `..`
    so the same worktree reached by two paths is one workspace, and normcase folds Windows
    drive-letter and case differences that would otherwise split it in two. Keying on the
    cwd instead is the per-worktree path-hashing that produced the volume explosion in #1.
    """
    from bosn.paths import normalize_workspace_path

    return normalize_workspace_path(manifest.root)
