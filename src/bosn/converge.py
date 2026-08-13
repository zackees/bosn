"""Converge-then-run.

Every stack verb makes registered state match the manifest -- registering, rolling a
generation, or reusing as-is -- and then runs. The same command is correct on the 1st and
the 500th invocation. Errors are reserved for states that need a human decision; an error
whose remedy is always the same mechanical command is just a forced retry loop.
"""

from __future__ import annotations

import datetime as dt
from dataclasses import dataclass
from pathlib import Path

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
    ) -> None:
        self.manifest = manifest
        self.registry = registry
        self.engine = engine or Engine()

    def converge(
        self, stack_name: str | None = None, *, workspace: str | None = None
    ) -> ConvergeResult:
        stack = self.manifest.stack(stack_name)
        digest = generation_digest(self.manifest, stack)
        workspace = workspace or str(self.manifest.root)

        known = self.registry.generation_superseded_at(digest)
        is_new_generation = known is None and not self._generation_recorded(digest)

        self.registry.record_generation(digest, stack.name)
        superseded = self.registry.supersede_generations(stack.name, keep_digest=digest)

        image_tag = image_tag_for(stack, digest)
        self._ensure_image(stack, digest, image_tag, workspace)
        volumes = self._ensure_volumes(stack, digest, workspace)

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

    def _generation_recorded(self, digest: str) -> bool:
        row = self.registry.conn.execute(
            "SELECT 1 FROM generations WHERE digest = ?", (digest,)
        ).fetchone()
        return row is not None

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
        result = self.engine.run(
            [
                "build",
                "--file",
                str(dockerfile),
                "--tag",
                tag,
                *label_args,
                str(self.manifest.root),
            ]
        )
        if not result.ok:
            raise EngineError(f"building {tag} failed: {result.stderr or result.stdout}")
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
        converged = self.converge(stack_name, workspace=workspace)
        stack = self.manifest.stack(stack_name)

        args = ["run", "--rm"]
        for volume in stack.volumes:
            name = volume_name_for(
                stack,
                volume.scope,
                volume.name,
                digest=converged.digest,
                workspace=workspace or str(self.manifest.root),
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
    return str(Path(manifest.root).resolve())
