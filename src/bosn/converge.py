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
from bosn.registry import Lease, Registry, Resource

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
        """Validate and create, replace, or reuse the persistent execution container."""
        with self.registry.lifecycle_guard():
            name, _container_id, _resource, _created = self._ensure_container_locked(
                converged, stack_name=stack_name, workspace=workspace
            )
        return name

    def _ensure_container_locked(
        self, converged: ConvergeResult, *, stack_name: str | None, workspace: str
    ) -> tuple[str, str, Resource, bool]:
        stack = self.manifest.stack(stack_name)
        self._validate_converged_contract(stack, converged, workspace)
        name = self.container_name(workspace, stack.name)
        expected_mounts = self._expected_container_mounts(stack, converged.volumes)
        expected_image = self._expected_image_id(converged.image_tag)
        existing = self._inspect_container(name)
        container_id = self._container_id(name, existing) if existing is not None else ""
        created_container_id: str | None = None
        if existing is not None:
            stale = self._container_stale_reasons(
                name,
                existing,
                stack=stack,
                workspace=workspace,
                digest=converged.digest,
                image_id=expected_image,
                mounts=expected_mounts,
            )
            if stale:
                self._refuse_if_container_leased(name)
                # Mutate the exact object whose ownership was proved. A name could be
                # externally reused between inspect and remove; deleting by name would
                # then destroy a foreign replacement we never validated.
                removed = self.engine.run(["container", "rm", "--force", container_id])
                if not removed.ok and self._inspect_container(name) is not None:
                    raise EngineError(
                        f"replacing persistent container {name} failed: "
                        f"{removed.stderr or removed.stdout}"
                    )
                self.registry.log_event("container.replaced", f"{name}: {', '.join(stale)}")
                existing = None
                container_id = ""

        if existing is None:
            resource_labels = self._resource_labels(
                stack, "container", converged.digest, "stack", workspace
            )
            # Docker removes an orphan automatically once PID 1's watchdog exits.
            args = ["create", "--rm", "--name", name, *resource_labels.to_docker_args()]
            for mount in expected_mounts.values():
                suffix = ":ro" if not mount["rw"] else ""
                args += ["--volume", f"{mount['source']}:{mount['destination']}{suffix}"]
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
            created_container_id = created.stdout.strip()
            if not created_container_id:
                raise EngineError(
                    f"creating persistent container {name} succeeded without returning its "
                    "immutable object id; refusing to continue"
                )
            try:
                existing = self._inspect_container(name)
                if existing is None:
                    raise EngineError(
                        f"created persistent container {name} disappeared before validation"
                    )
                container_id = self._container_id(name, existing)
                if container_id != created_container_id:
                    raise EngineError(
                        f"persistent container name collision: {name} was replaced between "
                        "creation and validation; refusing to touch the replacement"
                    )
                stale = self._container_stale_reasons(
                    name,
                    existing,
                    stack=stack,
                    workspace=workspace,
                    digest=converged.digest,
                    image_id=expected_image,
                    mounts=expected_mounts,
                )
                if stale:
                    raise EngineError(
                        f"created persistent container {name} does not match its specification: "
                        f"{', '.join(stale)}"
                    )
            except KeyboardInterrupt:
                self._cleanup_created_container(name, created_container_id)
                raise
            except Exception as exc:
                cleanup_error = self._cleanup_created_container(name, created_container_id)
                if cleanup_error:
                    raise EngineError(f"{exc}; {cleanup_error}") from exc
                raise
        try:
            started = self.engine.run(["start", container_id])
            if (
                not started.ok
                and "already started" not in (started.stderr or started.stdout).lower()
            ):
                raise EngineError(f"starting persistent container {name} failed: {started.stderr}")
            resource = self.registry.reconcile_resource(
                kind="container",
                name=name,
                stack=stack.name,
                generation=converged.digest,
                scope="stack",
                workspace=workspace,
            )
        except KeyboardInterrupt:
            if created_container_id is not None:
                self._cleanup_created_container(name, created_container_id)
            raise
        except Exception as exc:
            if created_container_id is not None:
                cleanup_error = self._cleanup_created_container(name, created_container_id)
                if cleanup_error:
                    raise EngineError(f"{exc}; {cleanup_error}") from exc
            raise
        return name, container_id, resource, created_container_id is not None

    def _cleanup_created_container(self, name: str, container_id: str) -> str | None:
        """Remove only the object this call created after validation/start fails."""
        try:
            removed = self.engine.run(["container", "rm", "--force", container_id])
        except KeyboardInterrupt:
            raise
        except Exception as exc:  # noqa: BLE001 - report cleanup without masking root failure
            return f"cleanup of newly created container {name} failed: {exc}"
        if removed.ok:
            return None
        return (
            f"cleanup of newly created container {name} failed: {removed.stderr or removed.stdout}"
        )

    def _expected_image_id(self, image: str) -> str:
        inspected = self.engine.run(["image", "inspect", "--format", "{{.Id}}", image])
        identity = inspected.stdout.strip()
        if not inspected.ok or not identity:
            raise EngineError(
                f"cannot validate execution image {image!r}: {inspected.stderr or inspected.stdout}"
            )
        return identity

    def _expected_container_mounts(
        self, stack: StackSpec, volume_names: tuple[str, ...]
    ) -> dict[str, dict[str, str | bool]]:
        mounts: dict[str, dict[str, str | bool]] = {}
        for volume, volume_name in zip(stack.volumes, volume_names, strict=True):
            destination = f"/bosn/{volume.name}"
            mounts[destination] = {
                "type": "volume",
                "source": volume_name,
                "destination": destination,
                "rw": True,
            }
        heartbeat = (self.registry.path.parent / "daemon.heartbeat").resolve()
        heartbeat.touch(exist_ok=True)
        mounts["/bosn-daemon/heartbeat"] = {
            "type": "bind",
            "source": str(heartbeat),
            "destination": "/bosn-daemon/heartbeat",
            "rw": False,
        }
        return mounts

    @staticmethod
    def _validate_converged_contract(
        stack: StackSpec, converged: ConvergeResult, workspace: str
    ) -> None:
        if converged.stack != stack.name:
            raise EngineError(
                f"daemon converged stack {converged.stack!r}, but the client selected "
                f"{stack.name!r}; reload the manifest and retry"
            )
        expected_volumes = tuple(
            volume_name_for(
                stack,
                volume.scope,
                volume.name,
                digest=converged.digest,
                workspace=workspace,
                family=stack.family,
            )
            for volume in stack.volumes
        )
        if expected_volumes != converged.volumes:
            raise EngineError(
                "the manifest's volume contract changed while the daemon converged it; "
                "reload the manifest and retry"
            )

    def _inspect_container(self, name: str) -> dict[str, object] | None:
        inspected = self.engine.run(["container", "inspect", "--format", "{{json .}}", name])
        if not inspected.ok:
            detail = (inspected.stderr or inspected.stdout).lower()
            if not detail or "no such" in detail or "not found" in detail:
                return None
            raise EngineError(f"inspecting persistent container {name} failed: {detail}")
        try:
            parsed = json.loads(inspected.stdout)
        except json.JSONDecodeError as exc:
            raise EngineError(
                f"persistent container {name} returned invalid inspect data; refusing to touch it"
            ) from exc
        if not isinstance(parsed, dict):
            raise EngineError(
                f"persistent container {name} returned unexpected inspect data; "
                "refusing to touch it"
            )
        return parsed

    @staticmethod
    def _container_id(name: str, inspected: dict[str, object]) -> str:
        container_id = str(inspected.get("Id") or "")
        if not container_id:
            raise EngineError(
                f"persistent container {name} inspect data has no object id; refusing to touch it"
            )
        return container_id

    def _container_stale_reasons(
        self,
        name: str,
        inspected: dict[str, object],
        *,
        stack: StackSpec,
        workspace: str,
        digest: str,
        image_id: str,
        mounts: dict[str, dict[str, str | bool]],
    ) -> list[str]:
        config = inspected.get("Config")
        raw_labels = config.get("Labels") if isinstance(config, dict) else None
        engine_labels = (
            {str(key): str(value) for key, value in raw_labels.items() if value is not None}
            if isinstance(raw_labels, dict)
            else {}
        )
        if not labels.is_owned_by(engine_labels, self.registry.registry_id):
            kind = "foreign" if labels.is_complete(engine_labels) else "incompletely labeled"
            raise EngineError(
                f"persistent container name collision: {name} is {kind}; "
                "refusing to start, remove, or replace it"
            )
        try:
            parsed = labels.ResourceLabels.from_dict(engine_labels)
        except labels.LabelError as exc:
            raise EngineError(
                f"persistent container name collision: {name} has invalid labels; "
                "refusing to touch it"
            ) from exc
        if (
            parsed.kind != "container"
            or parsed.scope != "stack"
            or parsed.stack != stack.name
            or parsed.workspace != workspace
        ):
            raise EngineError(
                f"persistent container name collision: {name} belongs to another bosn resource; "
                "refusing to start, remove, or replace it"
            )

        stale: list[str] = []
        if parsed.generation != digest:
            stale.append("generation changed")
        if str(inspected.get("Image") or "") != image_id:
            stale.append("image changed")
        if not self._container_mounts_match(inspected.get("Mounts"), mounts):
            stale.append("mounts changed")
        return stale

    @staticmethod
    def _container_mounts_match(
        raw_mounts: object, expected: dict[str, dict[str, str | bool]]
    ) -> bool:
        if not isinstance(raw_mounts, list):
            return False
        actual = {
            str(mount.get("Destination") or ""): mount
            for mount in raw_mounts
            if isinstance(mount, dict) and mount.get("Destination")
        }
        managed_destinations = {
            destination
            for destination in actual
            if destination.startswith("/bosn/") or destination == "/bosn-daemon/heartbeat"
        }
        if managed_destinations != set(expected):
            return False
        for destination, wanted in expected.items():
            found = actual.get(destination)
            if found is None or str(found.get("Type") or "") != wanted["type"]:
                return False
            if bool(found.get("RW")) != wanted["rw"]:
                return False
            if wanted["type"] == "volume":
                if str(found.get("Name") or "") != wanted["source"]:
                    return False
            elif not Converger._bind_sources_match(
                str(found.get("Source") or ""), str(wanted["source"])
            ):
                return False
        return True

    @staticmethod
    def _bind_sources_match(actual: str, expected: str) -> bool:
        if os.path.normcase(os.path.normpath(actual)) == os.path.normcase(
            os.path.normpath(expected)
        ):
            return True
        normalized = expected.replace("\\", "/")
        drive = re.match(r"^([A-Za-z]):/(.*)$", normalized)
        if drive is None:
            return False
        letter, tail = drive.groups()
        translated = actual.replace("\\", "/").rstrip("/").casefold()
        candidates = {
            f"/run/desktop/mnt/host/{letter}/{tail}",
            f"/host_mnt/{letter}/{tail}",
        }
        return translated in {candidate.rstrip("/").casefold() for candidate in candidates}

    def _refuse_if_container_leased(self, name: str) -> None:
        from bosn.resources import resource_is_leased

        rows = [
            resource
            for resource in self.registry.list_resources()
            if resource.kind == "container" and resource.name == name
        ]
        if any(resource_is_leased(self.registry, resource.id) for resource in rows):
            raise EngineError(
                f"persistent container {name} is from an old generation but has an active "
                "execution lease; retry after that command exits"
            )

    def _acquire_execution_container(
        self, converged: ConvergeResult, *, stack_name: str | None, workspace: str
    ) -> tuple[str, Lease]:
        with self.registry.lifecycle_guard():
            name, container_id, resource, created = self._ensure_container_locked(
                converged, stack_name=stack_name, workspace=workspace
            )
            try:
                lease = self.registry.acquire_lease(
                    resource.id, pid=os.getpid(), proc_start=self.registry.clock.now()
                )
            except KeyboardInterrupt:
                if created:
                    self._cleanup_created_container(name, container_id)
                raise
            except Exception as exc:
                if created:
                    cleanup_error = self._cleanup_created_container(name, container_id)
                    if cleanup_error:
                        raise EngineError(f"{exc}; {cleanup_error}") from exc
                raise
        return container_id, lease

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
        name, lease = self._acquire_execution_container(
            converged, stack_name=stack_name, workspace=workspace
        )
        args = ["exec", name, *command]
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
        name, lease = self._acquire_execution_container(
            converged, stack_name=stack_name, workspace=workspace
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
