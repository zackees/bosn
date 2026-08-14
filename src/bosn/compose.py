"""A parsed, validated model of a Compose file.

Compose files used to be parsed with indentation regexes in `docker_cli.py`. That
approach produced two real bugs before this module existed:

- The service-name regex scanned the *whole file* for anything shaped like a
  two-space-indented `name:` mapping key, so entries under a top-level `volumes:`
  or `networks:` block were labeled as services -- "ghost services" (#47).
- Label values generated for the Compose overlay were emitted as double-quoted
  YAML, where escape sequences are processed; a Windows workspace path such as
  `C:\\Users\\...` contains `\\U`, which YAML reads as the start of an 8-hex-digit
  unicode escape and rejects outright. The generated overlay could not parse its
  own output on the platform bosn is developed on.

Both bugs are instances of the same root cause: YAML is not a regular language,
and a line-oriented regex cannot reliably tell "a top-level declaration" from "a
nested reference" or reason about quoting rules. This module replaces the regex
scanner with `yaml.safe_load` plus explicit, fail-closed structure validation, so
those classes of bug cannot recur here.

This is the *first slice* of #47: a correct parsed model with strict key
validation. It intentionally does not implement new CLI verbs (build/run/exec/
config), `up -d --wait`, or `down -v`, and it does not attempt the `twp/e2e`
acceptance fixture -- those are tracked separately.
"""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml

from bosn.manifest import ManifestError, docker_context_references


class ComposeError(ValueError):
    """A Compose file could not be parsed, or uses a key this front door does not support.

    Named separately from `docker_cli.DockerFrontDoorError` because this module has no
    dependency on `docker_cli` (and must not gain one -- see the module docstring in
    `docker_cli.py` for the CLI/front-door split). `docker_cli` is expected to catch
    this alongside `DockerFrontDoorError`, or subclass/wrap it, when it is rewired to
    use this module.
    """


# -- supported key set --------------------------------------------------------
#
# bosn's front door must never silently ignore a Compose option: an unrecognized key
# either matters (and dropping it changes behavior the author asked for) or it doesn't
# (and it should be added here, deliberately). Anything not listed below is refused
# with the exact dotted path, rather than dropped -- fail closed, not fail quiet.
#
# The set below is exactly what `docker_cli.py` already reads or emits today (image,
# build, volumes, networks, labels) plus the keys #47 names as required for the
# documented v1 subset: profiles, healthcheck-style dependencies (`depends_on`),
# environment, ports, and both mount styles. Anything else -- `deploy`, `secrets`,
# `configs`, `extends`, etc. -- is out of scope for this slice and must be refused,
# not guessed at.
SUPPORTED_TOP_LEVEL_KEYS: frozenset[str] = frozenset({"version", "services", "volumes", "networks"})

# Compose reserves the `x-` prefix for user extension fields, at the top level and inside
# service/volume/network definitions. Refusing them would refuse the ordinary way anchors
# are written -- `x-common: &common` at the top level, merged into services with `<<:` --
# which #47 requires support for. They carry no meaning for bosn, so they are accepted and
# ignored rather than enumerated: a fixed allowlist like `x-bosn` only admits the one
# extension we happened to think of.
EXTENSION_PREFIX = "x-"

SUPPORTED_SERVICE_KEYS: frozenset[str] = frozenset(
    {
        "image",
        "build",
        "profiles",
        "volumes",
        "networks",
        "environment",
        "ports",
        "depends_on",
        "healthcheck",
        "labels",
        "command",
        "entrypoint",
        "restart",
        "container_name",
    }
)

SUPPORTED_TOP_LEVEL_VOLUME_KEYS: frozenset[str] = frozenset(
    {"driver", "driver_opts", "labels", "external", "name"}
)
SUPPORTED_TOP_LEVEL_NETWORK_KEYS: frozenset[str] = frozenset(
    {"driver", "driver_opts", "labels", "external", "internal", "name"}
)


def _refuse(path: str, remedy: str) -> None:
    raise ComposeError(f"unsupported compose key {path!r}; {remedy}")


def _check_keys(mapping: dict[str, Any], allowed: frozenset[str], path: str, remedy: str) -> None:
    """Fail closed: every key not in `allowed` is named by its full dotted path."""
    for key in mapping:
        if str(key).startswith(EXTENSION_PREFIX):
            continue
        if key not in allowed:
            _refuse(f"{path}.{key}" if path else str(key), remedy)


# -- model ----------------------------------------------------------------------


@dataclass(frozen=True)
class Service:
    """One entry under the top-level `services:` mapping.

    `volumes` and `networks` here are the names this service *references* -- not
    declarations. A bare short-syntax mount like `data:/data` contributes `"data"`;
    a bind mount (`./host:/data`) or an anonymous mount (`/data`) contributes nothing,
    since neither names a top-level resource. This mirrors the distinction #47 calls
    out: a service's nested `volumes:`/`networks:` reference resources declared
    elsewhere, and must never be confused with the top-level `volumes:`/`networks:`
    blocks that declare them (see `ComposeFile.volumes` / `ComposeFile.networks`).
    """

    name: str
    image: str | None
    has_build: bool
    profiles: tuple[str, ...] = ()
    referenced_volumes: tuple[str, ...] = ()
    referenced_networks: tuple[str, ...] = ()
    # Mounts Compose names at runtime; ungovernable through a label overlay.
    anonymous_volumes: tuple[str, ...] = ()

    @property
    def is_build_only(self) -> bool:
        """True for a service with `build:` and no `image:`.

        Today's regex-based `compose_to_manifest` silently drops such a service
        (#47: "build-only services are never emitted"). This model represents it
        faithfully instead -- `is_build_only` lets a caller decide what to do,
        rather than the parser deciding for them by omission.
        """
        return self.has_build and self.image is None


@dataclass(frozen=True)
class ComposeFile:
    """A parsed, validated Compose file.

    `services` is keyed by service name. `volumes` and `networks` are the names
    *declared* by the top-level `volumes:`/`networks:` blocks -- structurally
    distinct from any service's `referenced_volumes`/`referenced_networks`, which
    live on `Service` instead. There is no dict path where the two could be
    confused, which is the point: the ghost-service bug was a regex conflating two
    indentation levels that a real parser simply never conflates.
    """

    services: dict[str, Service] = field(default_factory=dict)
    volumes: tuple[str, ...] = ()
    networks: tuple[str, ...] = ()

    def image_pairs(self) -> list[tuple[str, str]]:
        """`(service_name, image)` pairs, in declaration order -- what `compose_to_manifest` needs.

        Build-only services are omitted here (they have no image to pair), matching
        today's caller-visible behavior for this specific accessor. Nothing is lost:
        `services` still holds the full `Service`, including build-only ones, for a
        caller that wants them.
        """
        return [(name, svc.image) for name, svc in self.services.items() if svc.image is not None]


# -- parsing ----------------------------------------------------------------------


def _referenced_volume_names(raw_volumes: list[Any], path: str) -> tuple[tuple[str, ...], ...]:
    """Split a service's volume list into (named references, anonymous entries).

    Anonymous entries are reported rather than dropped because they are the one mount kind
    the label overlay cannot govern: Compose names them at runtime, so there is no stable
    top-level key to attach labels to. The caller refuses them up front instead of letting
    an unlabeled volume come up -- which is precisely the accumulation bosn exists to stop.
    """
    names: list[str] = []
    anonymous: list[str] = []
    for i, entry in enumerate(raw_volumes):
        if isinstance(entry, str):
            # Short syntax: "source:target[:mode]", "./host:target" (bind, not named), or
            # a bare "target" (anonymous). Only a plain name before the first `:` that
            # isn't a path (no `/` or `.`) names a top-level volume.
            if ":" not in entry:
                anonymous.append(entry)
                continue
            source = entry.split(":", 1)[0]
            if source and "/" not in source and not source.startswith("."):
                names.append(source)
            continue
        if isinstance(entry, dict):
            _check_keys(
                entry,
                frozenset({"type", "source", "target", "read_only", "bind", "volume", "tmpfs"}),
                f"{path}[{i}]",
                "this slice supports type/source/target/read_only/bind/volume/tmpfs mount keys",
            )
            source = entry.get("source")
            if entry.get("type") == "volume":
                if isinstance(source, str):
                    names.append(source)
                else:
                    # `type: volume` with no source is an anonymous volume, same as the
                    # bare short-syntax form above.
                    anonymous.append(str(entry.get("target") or "<anonymous>"))
            continue
        _refuse(f"{path}[{i}]", "volume list entries must be a string or a mapping")
    return tuple(names), tuple(anonymous)


def _referenced_network_names(raw_networks: Any, path: str) -> tuple[str, ...]:
    if isinstance(raw_networks, list):
        return tuple(n for n in raw_networks if isinstance(n, str))
    if isinstance(raw_networks, dict):
        return tuple(raw_networks.keys())
    _refuse(path, "networks: must be a list of names or a mapping of name to config")
    raise AssertionError("unreachable")  # pragma: no cover


def _parse_service(name: str, raw: Any) -> Service:
    path = f"services.{name}"
    if not isinstance(raw, dict):
        raise ComposeError(f"{path} must be a mapping; remedy: define it as `image:`/`build:` keys")
    _check_keys(
        raw,
        SUPPORTED_SERVICE_KEYS,
        path,
        "this slice supports "
        f"{', '.join(sorted(SUPPORTED_SERVICE_KEYS))}; "
        "add support deliberately rather than dropping it",
    )
    image = raw.get("image")
    if image is not None and not isinstance(image, str):
        raise ComposeError(f"{path}.image must be a string")
    has_build = "build" in raw
    profiles_raw = raw.get("profiles", [])
    if not isinstance(profiles_raw, list) or not all(isinstance(p, str) for p in profiles_raw):
        raise ComposeError(f"{path}.profiles must be a list of strings")
    referenced_volumes: tuple[str, ...] = ()
    anonymous_volumes: tuple[str, ...] = ()
    if "volumes" in raw:
        raw_volumes = raw["volumes"]
        if not isinstance(raw_volumes, list):
            raise ComposeError(f"{path}.volumes must be a list")
        referenced_volumes, anonymous_volumes = _referenced_volume_names(
            raw_volumes, f"{path}.volumes"
        )
    referenced_networks: tuple[str, ...] = ()
    if "networks" in raw:
        referenced_networks = _referenced_network_names(raw["networks"], f"{path}.networks")
    return Service(
        name=name,
        image=image,
        has_build=has_build,
        profiles=tuple(profiles_raw),
        referenced_volumes=referenced_volumes,
        anonymous_volumes=anonymous_volumes,
        referenced_networks=referenced_networks,
    )


def _parse_top_level_resource(raw: Any, section: str, allowed: frozenset[str]) -> tuple[str, ...]:
    """Parse a top-level `volumes:`/`networks:` mapping into its declared names.

    This is the *declaration* side of the ghost-service distinction: names collected
    here come only from the top-level `section:` mapping's own keys, never from
    anything nested inside `services:`.
    """
    if raw is None:
        return ()
    if not isinstance(raw, dict):
        raise ComposeError(f"{section} must be a mapping of name to config")
    for name, config in raw.items():
        if config is None:
            continue
        if not isinstance(config, dict):
            raise ComposeError(f"{section}.{name} must be a mapping or null")
        _check_keys(
            config,
            allowed,
            f"{section}.{name}",
            f"this slice supports {', '.join(sorted(allowed))} for top-level {section} entries",
        )
    return tuple(raw.keys())


def load_compose(path_or_text: Path | str) -> ComposeFile:
    """Parse and validate a Compose file with `yaml.safe_load`.

    Accepts either a filesystem `Path` or a raw YAML string -- a `Path` whose file
    exists is read; anything else (including a `str` that isn't a valid path, and any
    `str` at all) is treated as YAML text directly, so callers/tests can pass literal
    Compose content inline.

    `yaml.safe_load` resolves anchors (`&name`) and `<<:` merge keys before this
    module ever sees the data -- #47 requires that; see
    `test_anchors_and_merge_keys_resolve` for the proof.
    """
    if isinstance(path_or_text, Path):
        text = path_or_text.read_text(encoding="utf-8")
    else:
        text = path_or_text

    try:
        raw = yaml.safe_load(text)
    except yaml.YAMLError as exc:
        raise ComposeError(f"malformed compose YAML: {exc}") from exc

    if raw is None:
        raise ComposeError("compose file is empty")
    if not isinstance(raw, dict):
        raise ComposeError("compose file must be a YAML mapping at the top level")

    _check_keys(
        raw,
        SUPPORTED_TOP_LEVEL_KEYS,
        "",
        f"this slice supports {', '.join(sorted(SUPPORTED_TOP_LEVEL_KEYS))} at the top level",
    )

    services_raw = raw.get("services")
    if not isinstance(services_raw, dict) or not services_raw:
        raise ComposeError("compose file has no services")
    services = {name: _parse_service(name, body) for name, body in services_raw.items()}

    volumes = _parse_top_level_resource(
        raw.get("volumes"), "volumes", SUPPORTED_TOP_LEVEL_VOLUME_KEYS
    )
    networks = _parse_top_level_resource(
        raw.get("networks"), "networks", SUPPORTED_TOP_LEVEL_NETWORK_KEYS
    )

    return ComposeFile(services=services, volumes=volumes, networks=networks)


# -- identity ---------------------------------------------------------------------
#
# The Compose front door (`docker_cli._compose_overlay`) used to label every resource
# it creates with a *constant* `generation="compose"` -- every Compose project on the
# machine shared one generation string, which defeats supersession entirely: editing
# a compose file rolled nothing, because the generation never changed. The two
# functions below give Compose the same identity contract `manifest.py` already gives
# the native stack path: a stable per-project name (`project_identity`) and a content
# digest that changes exactly when the resolved configuration changes
# (`content_digest`). See `manifest.generation_digest`'s docstring for the parent
# contract: two invocations are compatible iff their digests are byte-equal.


def project_identity(compose_path: Path) -> str:
    """A stable, collision-resistant project identity for a Compose file's directory.

    Compose itself derives a project name from the containing directory's basename
    alone, which collides whenever two different directories share a name -- an
    ordinary shape for sibling clones or worktrees (`~/work/app` and
    `~/scratch/app` would both compose to project `app`). This follows the same
    precedent `converge.volume_name_for` already uses for workspace scoping: keep
    the human-readable basename for debuggability, and disambiguate with a short
    stable hash of the directory's *resolved absolute path* rather than inventing a
    new scheme.

    Deterministic and stable: the same directory (via any spelling -- relative,
    symlinked, `.` segments) resolves to the same absolute path and therefore the
    same identity on every call, on the same machine. It does not attempt to be
    stable *across machines*, the way a content digest is -- an absolute path is
    inherently local, and Compose project names have never claimed to be portable.

    The basename component is lowercased and stripped to
    `[a-z0-9]([a-z0-9_.-]*[a-z0-9])?`, matching the character set Compose itself
    enforces on project names -- this identity is meant to be usable as a project
    name (or a label value derived the same way), not just a debugging label, so
    it must not carry characters Compose or a resource name would reject. The hash
    suffix still carries all the collision resistance; the sanitized basename is
    decoration.
    """
    directory = compose_path.resolve()
    if directory.is_file():
        directory = directory.parent
    key = hashlib.sha256(directory.as_posix().encode("utf-8")).hexdigest()[:12]
    slug = re.sub(r"[^a-z0-9_.-]+", "-", directory.name.lower()).strip("-._") or "project"
    return f"bosn-compose-{slug}-{key}"


def _hash_field(hasher: Any, value: bytes) -> None:
    """Append one unambiguous binary field to a digest input.

    Length-delimited rather than a plain separator-joined concatenation: a naive
    `b"\\0".join([name, content])` lets two different (name, content) splits collide
    on the same byte stream (see `manifest.test_file_records_are_length_delimited`).
    Prefixing every field with its own byte length removes that ambiguity.
    """
    hasher.update(len(value).to_bytes(8, "big"))
    hasher.update(value)


def _service_build_context(raw_build: Any, compose_dir: Path) -> tuple[Path, Path] | None:
    """Resolve a service's `build:` value to (context directory, Dockerfile path).

    Mirrors Compose's own defaults: a bare string is the context, with an implicit
    `Dockerfile` inside it; a mapping may override `context` and/or `dockerfile`.
    Anything else (no `build:`, or a shape this slice doesn't recognize) yields
    `None` and is simply not folded into the digest -- `load_compose` already
    validated the service's key set before this runs, so an unrecognized `build:`
    shape here means Compose's own schema allows something bosn hasn't modeled yet,
    not that the file is malformed.
    """
    if isinstance(raw_build, str):
        context = compose_dir / raw_build
        dockerfile_name = "Dockerfile"
    elif isinstance(raw_build, dict):
        context = compose_dir / str(raw_build.get("context", "."))
        dockerfile_name = str(raw_build.get("dockerfile", "Dockerfile"))
    else:
        return None
    return context, context / dockerfile_name


def content_digest(compose_path: Path | str) -> str:
    """Ordered content-closure digest of a Compose file, for generation identity.

    Same contract as `manifest.generation_digest`: two invocations are compatible
    iff their digests are byte-equal. `sha256:`-prefixed, and every field folded in
    is length-delimited (`_hash_field`) rather than concatenated, for the same
    collision reason `manifest.py` documents.

    **What rolls the digest.** The file is parsed with `yaml.safe_load` -- the same
    step `load_compose` uses, so `&anchor`/`<<:` merge keys are already resolved --
    and the *parsed structure* is hashed, not the source bytes. Concretely:

    - Reformatting -- indentation, mapping key order, quoting style (`'x'` vs `"x"`
      vs bare `x`), comments -- does NOT roll the digest, because none of that
      survives `yaml.safe_load`; the digest is built from canonical JSON
      (`sort_keys=True`) over the parsed value, so key order never matters either.
    - Any semantic change to the parsed structure DOES roll the digest: an image
      tag, a service's `volumes:`/`networks:`/`environment:`/`ports:`/`command:`,
      adding or removing a top-level `volumes:` or `networks:` entry, all change
      what got parsed and therefore the canonical JSON that gets hashed.
    - List order is preserved, not sorted -- a `ports:` list or `command:` argv
      reordered is a different digest, matching #48's "ordered ... digest".
      Top-level key order is not preserved (canonical JSON sorts keys), so
      reordering *services* in the file, or the keys within one service, does not
      roll the digest -- only reordering matters *within* a list value.
    - A null value and an empty mapping are NOT the same reformat, even though a
      human skimming the file might read them that way: `back:` parses to `None`
      and `back: {}` parses to `{}`, two different Python values, so switching
      between them DOES roll the digest.

    **Build contexts fold in.** For every service with a `build:` key, the local
    build closure is resolved with `manifest.docker_context_references` -- the
    same COPY/ADD-driven, `.dockerignore`-filtered walk stacks use -- and the byte
    content of every file in that closure is folded in, in
    `(service name, path)`-sorted order. Without this, editing application source
    under a `build:` context would roll nothing for Compose-managed resources,
    which is exactly the bug #48 exists to close. `build.args` is not resolved
    against the Dockerfile's `ARG`/`FROM` the way `manifest.py` does for stacks
    converted through `compose_to_manifest` -- that richer, build-arg-aware
    resolution belongs to the manifest path once a service is converted. Here, an
    unresolved `$VAR` in a COPY/ADD source falls back to the same conservative
    "hash the whole context" behavior `docker_context_references` already
    implements for stacks.

    **Path handling** mirrors `load_compose`: a real `Path` is read from disk, and
    its parent directory anchors `build:` context resolution; a `str` -- even one
    that looks like a filesystem path -- is treated as literal YAML text, exactly
    like `load_compose`, and cannot resolve `build:` contexts (there is no
    directory to resolve a relative context against); build-context folding is
    silently skipped for text input, since nothing on disk exists to hash.
    """
    load_compose(compose_path)  # validates first; fail closed on anything unsupported

    if isinstance(compose_path, Path):
        text = compose_path.read_text(encoding="utf-8")
        compose_dir: Path | None = compose_path.resolve().parent
    else:
        text = compose_path
        compose_dir = None

    raw = yaml.safe_load(text)

    hasher = hashlib.sha256()
    # `default=str`: `yaml.safe_load` turns an unquoted ISO-looking scalar (a bare
    # `2024-06-27` in a label value, say) into a `datetime.date`, which `load_compose`
    # never rejects (it doesn't type-check leaf values) but which `json.dumps` cannot
    # serialize on its own. `str()` on such a value is stable across calls, so digest
    # determinism holds; this is a fallback for values JSON has no native type for, not
    # an attempt to normalize their formatting.
    canonical = json.dumps(raw, sort_keys=True, separators=(",", ":"), default=str).encode("utf-8")
    _hash_field(hasher, canonical)

    if compose_dir is not None:
        services_raw = raw.get("services") or {}
        for name in sorted(services_raw):
            service_raw = services_raw[name]
            if not isinstance(service_raw, dict) or "build" not in service_raw:
                continue
            spec = _service_build_context(service_raw["build"], compose_dir)
            if spec is None:
                continue
            context, dockerfile = spec
            try:
                files = docker_context_references(context, dockerfile)
            except ManifestError as exc:
                raise ComposeError(f"services.{name}.build: {exc}") from exc
            for file_path in files:
                relative = file_path.relative_to(context).as_posix()
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
                    raise ComposeError(
                        f"cannot digest build context file {file_path}: {exc}"
                    ) from exc
                hasher.update(b"\0build-file-record\0")
                _hash_field(hasher, name.encode("utf-8"))
                _hash_field(hasher, kind)
                _hash_field(hasher, relative.encode("utf-8"))
                _hash_field(hasher, content)

    return f"sha256:{hasher.hexdigest()}"
