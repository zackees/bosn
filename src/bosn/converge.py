"""Converge-then-run.

Every stack verb makes registered state match the manifest -- registering, rolling a
generation, or reusing as-is -- and then runs. The same command is correct on the 1st and
the 500th invocation. Errors are reserved for states that need a human decision; an error
whose remedy is always the same mechanical command is just a forced retry loop.
"""

from __future__ import annotations

import datetime as dt
import hashlib
import json
import os
import re
import threading
from collections.abc import Callable
from dataclasses import dataclass

from bosn import labels
from bosn.engine import Engine, EngineError
from bosn.manifest import Manifest, StackSpec, dockerfile_external_images, generation_digest
from bosn.registry import Registry

# What converge did, in the order of increasing work.
REUSED = "reused"
REGISTERED = "registered"
ROLLED = "rolled"
CONTAINER_HEARTBEAT_TIMEOUT_SECONDS = 600
RUN_MAX_DURATION_SECONDS = 8 * 60 * 60


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


def resolved_generation(
    manifest: Manifest,
    stack: StackSpec,
    engine: Engine,
    *,
    progress: Callable[[str], None] | None = None,
    cancelled: threading.Event | None = None,
) -> tuple[str, str | None]:
    """Resolve one canonical generation identity, including external image content."""
    content_digest = generation_digest(manifest, stack)
    references = _expanded_external_images(manifest, stack, engine)
    if not references:
        return content_digest, None

    identities: list[dict[str, str | None]] = []
    resolved_image: str | None = None
    for reference, platform in references:
        _abort_if_cancelled(cancelled)
        identity = _resolve_image_identity(
            engine,
            reference,
            platform=platform,
            progress=progress,
            cancelled=cancelled,
        )
        identities.append({"reference": reference, "platform": platform, "identity": identity})
        if stack.image:
            resolved_image = identity

    return _generation_digest_from_images(content_digest, identities), resolved_image


def generation_coalescing_key(manifest: Manifest, stack: StackSpec, engine: Engine) -> str:
    """Compute a read-only key that separates locally resolved mutable image identities.

    Missing images use a stable sentinel. Pulling them belongs to the managed build job,
    where cancellation, TTL, max-build limits, and output streaming all apply.
    """
    content_digest = generation_digest(manifest, stack)
    references = _expanded_external_images(manifest, stack, engine)
    if not references:
        return content_digest
    identities = [
        {
            "reference": reference,
            "platform": platform,
            "identity": _inspect_image_identity(engine, reference, platform=platform),
        }
        for reference, platform in references
    ]
    return _generation_digest_from_images(content_digest, identities)


def _generation_digest_from_images(
    content_digest: str, identities: list[dict[str, str | None]]
) -> str:
    payload = {"content_digest": content_digest, "external_images": identities}
    encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return f"sha256:{hashlib.sha256(encoded).hexdigest()}"


def _expanded_external_images(
    manifest: Manifest, stack: StackSpec, engine: Engine
) -> list[tuple[str, str | None]]:
    references = (
        [(stack.image, None)] if stack.image else dockerfile_external_images(manifest.root, stack)
    )
    references = [(reference, platform) for reference, platform in references if reference]
    platform_values: dict[str, str] | None = None

    def expand_automatic(value: str | None) -> str | None:
        nonlocal platform_values
        if value is None or "$" not in value:
            return value
        if platform_values is None:
            platform_values = _engine_platform_values(engine)
        pattern = re.compile(r"\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([^}]+)\})")

        def replace(match: re.Match[str]) -> str:
            name = match.group(1) or match.group(2) or ""
            assert platform_values is not None
            try:
                return platform_values[name]
            except KeyError:
                raise EngineError(f"cannot resolve automatic platform argument {name!r}") from None

        return pattern.sub(replace, value)

    return [
        (expand_automatic(reference) or "", expand_automatic(platform))
        for reference, platform in references
    ]


def _engine_platform_values(engine: Engine) -> dict[str, str]:
    inspected = engine.run(["version", "--format", "{{.Server.Os}}/{{.Server.Arch}}"])
    platform = inspected.stdout.strip()
    if not inspected.ok or not platform or "/" not in platform:
        raise EngineError(
            "cannot resolve Dockerfile automatic platform arguments: "
            f"{inspected.stderr or inspected.stdout or 'engine returned no platform'}"
        )
    os_name, architecture, *variant_parts = platform.split("/")
    variant = "/".join(variant_parts)
    return {
        "BUILDPLATFORM": platform,
        "BUILDOS": os_name,
        "BUILDARCH": architecture,
        "BUILDVARIANT": variant,
        "TARGETPLATFORM": platform,
        "TARGETOS": os_name,
        "TARGETARCH": architecture,
        "TARGETVARIANT": variant,
    }


def _resolve_image_identity(
    engine: Engine,
    reference: str,
    *,
    platform: str | None,
    progress: Callable[[str], None] | None,
    cancelled: threading.Event | None,
) -> str:
    identity = _inspect_image_identity(engine, reference, platform=platform)
    if identity is None:
        platform_args = ["--platform", platform] if platform else []
        pulled = engine.stream(
            ["pull", *platform_args, reference], on_line=progress, cancelled=cancelled
        )
        if not pulled.ok:
            suffix = f" for platform {platform!r}" if platform else ""
            raise EngineError(
                f"resolving image {reference!r}{suffix} failed: {pulled.stderr or pulled.stdout}"
            )
        identity = _inspect_image_identity(engine, reference, platform=platform)
    if not identity:
        raise EngineError(f"image {reference!r} has no immutable engine identity after resolution")
    return identity


def _inspect_image_identity(engine: Engine, reference: str, *, platform: str | None) -> str | None:
    platform_args = ["--platform", platform] if platform else []
    inspected = engine.run(["image", "inspect", *platform_args, "--format", "{{.Id}}", reference])
    identity = inspected.stdout.strip()
    if not inspected.ok or not identity:
        return None
    return identity


def _abort_if_cancelled(cancelled: threading.Event | None) -> None:
    if cancelled is not None and cancelled.is_set():
        raise EngineError("converge was cancelled")


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
        digest, resolved_image = self._resolved_generation(stack)
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
        image_tag = resolved_image or image_tag_for(stack, digest)
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

    def _resolved_generation(self, stack: StackSpec) -> tuple[str, str | None]:
        return resolved_generation(
            self.manifest,
            stack,
            self.engine,
            progress=self.progress,
            cancelled=self.cancelled,
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

    @staticmethod
    def container_name(workspace: str, stack: str) -> str:
        """Stable, Docker-safe container identity for one workspace and stack."""
        digest = hashlib.sha256(f"{workspace}\0{stack}".encode()).hexdigest()[:16]
        return f"bosn-{stack}-{digest}"

    def ensure_container(
        self, converged: ConvergeResult, *, stack_name: str | None, workspace: str
    ) -> str:
        """Create/start the persistent execution container, or reuse its existing one."""
        stack = self.manifest.stack(stack_name)
        name = self.container_name(workspace, stack.name)
        inspected = self.engine.run(["container", "inspect", name])
        if not inspected.ok:
            labels = self._resource_labels(stack, "container", converged.digest, "stack", workspace)
            # Docker removes an orphan automatically once PID 1's watchdog exits.
            args = ["create", "--rm", "--name", name, *labels.to_docker_args()]
            for volume in stack.volumes:
                volume_name = volume_name_for(
                    stack,
                    volume.scope,
                    volume.name,
                    digest=converged.digest,
                    workspace=workspace,
                    family=stack.family,
                )
                args += ["--volume", f"{volume_name}:/bosn/{volume.name}"]
            heartbeat = self.registry.path.parent / "daemon.heartbeat"
            heartbeat.touch(exist_ok=True)
            args += ["--volume", f"{heartbeat.resolve()}:/bosn-daemon/heartbeat:ro"]
            # PID 1 exits when the daemon heartbeat goes stale or a run is orphaned for
            # too long.  `--rm` above then removes the stopped container automatically.
            watchdog = (
                "started=$(date +%s); while :; do now=$(date +%s); "
                "beat=$(stat -c %Y /bosn-daemon/heartbeat 2>/dev/null || echo 0); "
                f"[ $((now-beat)) -gt {CONTAINER_HEARTBEAT_TIMEOUT_SECONDS} ] && exit 0; "
                f"[ $((now-started)) -gt {RUN_MAX_DURATION_SECONDS} ] && exit 0; "
                "sleep 30; done"
            )
            args += [converged.image_tag, "sh", "-c", watchdog]
            created = self.engine.run(args)
            if not created.ok:
                raise EngineError(f"creating persistent container {name} failed: {created.stderr}")
            self._register(stack, "container", name, converged.digest, "stack", workspace)
        started = self.engine.run(["start", name])
        if not started.ok and "already started" not in (started.stderr or started.stdout).lower():
            raise EngineError(f"starting persistent container {name} failed: {started.stderr}")
        return name

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
        name = self.ensure_container(converged, stack_name=stack_name, workspace=workspace)
        args = ["exec", name, *command]
        resource = next(
            r for r in self.registry.list_resources() if r.kind == "container" and r.name == name
        )
        lease = self.registry.acquire_lease(
            resource.id, pid=os.getpid(), proc_start=self.registry.clock.now()
        )
        try:
            result = self.engine.run(args)
        finally:
            self.registry.release_lease(lease.id)
        self.registry.log_event("run", f"{converged.stack} rc={result.returncode}")
        return converged, result.returncode, result.stdout or result.stderr

    def shell_converged(
        self, converged: ConvergeResult, *, stack_name: str | None, workspace: str
    ) -> int:
        """Attach a real interactive shell to the persistent container."""
        name = self.ensure_container(converged, stack_name=stack_name, workspace=workspace)
        resource = next(
            r for r in self.registry.list_resources() if r.kind == "container" and r.name == name
        )
        lease = self.registry.acquire_lease(
            resource.id, pid=os.getpid(), proc_start=self.registry.clock.now()
        )
        try:
            return self.engine.interactive(["exec", "-it", name, "sh"])
        finally:
            self.registry.release_lease(lease.id)


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
