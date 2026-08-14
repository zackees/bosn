"""`bosn.toml` parsing and generation digests.

The checked-in manifest is the spec sheet, the discovery surface, and the digest root.

**Identity is a content digest.** A generation is the hash over a stack's manifest section
plus the byte content of every file it references. Two invocations are compatible iff their
digests are byte-equal. Edit the Dockerfile and a new generation rolls forward; the old one
keeps serving its live leases, then ages out as superseded.
"""

from __future__ import annotations

import hashlib
import json
import posixpath
import re
import shlex
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

MANIFEST_NAME = "bosn.toml"
VALID_SCOPES = {"spec", "stack", "machine"}


class ManifestError(ValueError):
    """The manifest is malformed, or names something that does not exist."""


@dataclass(frozen=True)
class VolumeSpec:
    name: str
    scope: str

    def __post_init__(self) -> None:
        if self.scope not in VALID_SCOPES:
            raise ManifestError(
                f"volume {self.name!r} has unknown scope {self.scope!r}; "
                f"expected one of {sorted(VALID_SCOPES)}"
            )


@dataclass(frozen=True)
class StackSpec:
    name: str
    dockerfile: str | None = None
    image: str | None = None
    family: str | None = None
    default: bool = False
    volumes: tuple[VolumeSpec, ...] = ()

    def referenced_files(self, root: Path) -> list[Path]:
        """Files whose byte content folds into this stack's digest."""
        if not self.dockerfile:
            return []
        return docker_context_references(root, root / self.dockerfile)


@dataclass(frozen=True)
class TaskSpec:
    name: str
    stack: str
    cmd: str


@dataclass
class Manifest:
    root: Path
    stacks: dict[str, StackSpec] = field(default_factory=dict)
    tasks: dict[str, TaskSpec] = field(default_factory=dict)

    @property
    def path(self) -> Path:
        return self.root / MANIFEST_NAME

    def default_stack(self) -> StackSpec:
        explicit = [s for s in self.stacks.values() if s.default]
        if len(explicit) > 1:
            names = sorted(s.name for s in explicit)
            raise ManifestError(f"more than one stack is marked default: {names}")
        if explicit:
            return explicit[0]
        if len(self.stacks) == 1:
            return next(iter(self.stacks.values()))
        raise ManifestError(
            "no default stack; mark one with `default = true` or name a stack explicitly"
        )

    def stack(self, name: str | None) -> StackSpec:
        if name is None:
            return self.default_stack()
        try:
            return self.stacks[name]
        except KeyError:
            raise ManifestError(
                f"no stack named {name!r}; known stacks: {sorted(self.stacks)}"
            ) from None

    def task(self, name: str) -> TaskSpec:
        try:
            return self.tasks[name]
        except KeyError:
            raise ManifestError(
                f"no task named {name!r}; known tasks: {sorted(self.tasks)}"
            ) from None

    def digest(self, stack_name: str | None = None) -> str:
        return generation_digest(self, self.stack(stack_name))


def find_manifest(start: Path | None = None) -> Path | None:
    """Walk upward looking for bosn.toml, the way git finds its root."""
    current = (start or Path.cwd()).resolve()
    for candidate in [current, *current.parents]:
        manifest = candidate / MANIFEST_NAME
        if manifest.is_file():
            return manifest
    return None


def load(path: Path | str) -> Manifest:
    path = Path(path)
    if path.is_dir():
        path = path / MANIFEST_NAME
    if not path.is_file():
        raise ManifestError(f"no manifest at {path}")
    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as exc:
        raise ManifestError(f"{path} is not valid TOML: {exc}") from exc
    return parse(raw, root=path.parent)


def parse(raw: dict, root: Path) -> Manifest:
    manifest = Manifest(root=root)

    for name, body in (raw.get("stack") or {}).items():
        if not isinstance(body, dict):
            raise ManifestError(f"[stack.{name}] must be a table")
        volumes = tuple(
            VolumeSpec(name=vol_name, scope=str((vol_body or {}).get("scope", "spec")))
            for vol_name, vol_body in (body.get("volumes") or {}).items()
        )
        stack = StackSpec(
            name=name,
            dockerfile=body.get("dockerfile"),
            image=body.get("image"),
            family=body.get("family"),
            default=bool(body.get("default", False)),
            volumes=volumes,
        )
        if stack.dockerfile is None and stack.image is None:
            raise ManifestError(f"[stack.{name}] must set either `dockerfile` or `image`")
        manifest.stacks[name] = stack

    for name, body in (raw.get("task") or {}).items():
        if not isinstance(body, dict):
            raise ManifestError(f"[task.{name}] must be a table")
        stack_name = body.get("stack")
        cmd = body.get("cmd")
        if not cmd:
            raise ManifestError(f"[task.{name}] must set `cmd`")
        if stack_name and stack_name not in manifest.stacks:
            raise ManifestError(
                f"[task.{name}] references unknown stack {stack_name!r}; "
                f"known stacks: {sorted(manifest.stacks)}"
            )
        manifest.tasks[name] = TaskSpec(
            name=name,
            stack=stack_name or (manifest.default_stack().name if manifest.stacks else ""),
            cmd=str(cmd),
        )

    if not manifest.stacks:
        raise ManifestError("manifest declares no stacks")
    return manifest


def generation_digest(manifest: Manifest, stack: StackSpec) -> str:
    """Hash the stack's manifest section plus the byte content of referenced files.

    A missing referenced file is an error, not a zero -- silently digesting an absent
    Dockerfile would make two different specs compare equal.
    """
    hasher = hashlib.sha256()
    section = {
        "name": stack.name,
        "dockerfile": stack.dockerfile,
        "image": stack.image,
        "family": stack.family,
        "volumes": sorted((v.name, v.scope) for v in stack.volumes),
    }
    _hash_field(
        hasher,
        json.dumps(section, sort_keys=True, separators=(",", ":")).encode("utf-8"),
    )

    for file_path in stack.referenced_files(manifest.root):
        try:
            relative = file_path.relative_to(manifest.root).as_posix()
        except ValueError as exc:
            raise ManifestError(
                f"stack {stack.name!r} references {file_path} outside its build context"
            ) from exc
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
            raise ManifestError(f"cannot digest referenced path {file_path}: {exc}") from exc
        hasher.update(b"\0path-record\0")
        _hash_field(hasher, kind)
        _hash_field(hasher, relative.encode("utf-8"))
        _hash_field(hasher, content)

    return f"sha256:{hasher.hexdigest()}"


def _hash_field(hasher: Any, value: bytes) -> None:
    """Append one unambiguous binary field to a digest input."""
    hasher.update(len(value).to_bytes(8, "big"))
    hasher.update(value)


@dataclass(frozen=True)
class _IgnoreRule:
    pattern: str
    negated: bool


def _dockerignore_path(root: Path, dockerfile: Path) -> Path | None:
    specific = dockerfile.with_name(f"{dockerfile.name}.dockerignore")
    if specific.is_file():
        return specific
    default = root / ".dockerignore"
    return default if default.is_file() else None


def _dockerignore_rules(path: Path | None) -> list[_IgnoreRule]:
    if path is None:
        return []
    rules: list[_IgnoreRule] = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        if not raw or raw.startswith("#"):
            continue
        text = raw.strip()
        if not text or text == ".":
            continue
        negated = text.startswith("!")
        if negated:
            text = text[1:]
        # Docker deliberately disregards leading and trailing slashes. Always use POSIX
        # separators here so the same checked-out context hashes identically on every host.
        text = posixpath.normpath(text.replace("\\", "/").strip("/"))
        if text:
            rules.append(_IgnoreRule(text, negated))
    return rules


def _docker_pattern_regex(pattern: str) -> re.Pattern[str]:
    """Compile Docker's slash-aware glob syntax, including its special `**` wildcard."""
    expression = "^"
    index = 0
    while index < len(pattern):
        char = pattern[index]
        if char == "*":
            if index + 1 < len(pattern) and pattern[index + 1] == "*":
                index += 2
                if index < len(pattern) and pattern[index] == "/":
                    index += 1
                    expression += "(?:.*/)?"
                else:
                    expression += ".*"
                continue
            expression += "[^/]*"
        elif char == "?":
            expression += "[^/]"
        elif char == "[":
            end = pattern.find("]", index + 1)
            if end == -1:
                expression += r"\["
            else:
                character_class = pattern[index + 1 : end]
                if character_class.startswith("!"):
                    character_class = "^" + character_class[1:]
                expression += "[" + character_class.replace("\\", r"\\") + "]"
                index = end
        elif char == "\\" and index + 1 < len(pattern):
            index += 1
            expression += re.escape(pattern[index])
        else:
            expression += re.escape(char)
        index += 1
    return re.compile(expression + "$")


def _glob_matches(pattern: str, candidate: str) -> bool:
    return _docker_pattern_regex(pattern).fullmatch(candidate) is not None


def _ignore_rule_matches(rule: _IgnoreRule, relative: str) -> bool:
    parts = relative.split("/")
    prefixes = ["/".join(parts[:index]) for index in range(1, len(parts))]
    if _glob_matches(rule.pattern, relative):
        return True
    # A directory match applies to everything below it. Evaluating each prefix also lets a
    # later negation re-include a more specific descendant, matching Docker's last-rule-wins
    # behavior without pruning ignored directories during traversal.
    return any(_glob_matches(rule.pattern, prefix) for prefix in prefixes)


def _is_ignored(path: Path, root: Path, rules: list[_IgnoreRule]) -> bool:
    relative = path.relative_to(root).as_posix()
    ignored = False
    for rule in rules:
        if _ignore_rule_matches(rule, relative):
            ignored = not rule.negated
    return ignored


def _logical_dockerfile_lines(text: str) -> list[str]:
    escape = "\\"
    for raw in text.splitlines():
        directive = re.match(r"^\s*#\s*escape\s*=\s*([\\`])\s*$", raw, re.IGNORECASE)
        if directive:
            escape = directive.group(1)
            break
        if raw.strip() and not raw.lstrip().startswith("#"):
            break
    lines: list[str] = []
    heredocs: list[tuple[str, bool]] = []
    pending = ""
    for raw in text.splitlines():
        if heredocs:
            delimiter, strip_tabs = heredocs[0]
            candidate = raw.lstrip("\t") if strip_tabs else raw
            if candidate == delimiter:
                heredocs.pop(0)
            continue
        stripped = raw.strip()
        if not stripped or (not pending and stripped.startswith("#")):
            continue
        pending += stripped
        if pending.endswith(escape):
            pending = pending[:-1] + " "
            continue
        lines.append(pending)
        heredocs.extend(_heredoc_delimiters(pending))
        pending = ""
    if pending:
        lines.append(pending)
    return lines


def _heredoc_delimiters(instruction: str) -> list[tuple[str, bool]]:
    delimiters: list[tuple[str, bool]] = []
    pattern = re.compile(
        r"<<(?P<tabs>-?)(?:\\)?(?P<quote>['\"]?)(?P<name>[A-Za-z0-9_.-]+)(?P=quote)"
    )
    index = 0
    quote: str | None = None
    while index < len(instruction):
        char = instruction[index]
        if quote is not None:
            if char == "\\" and quote == '"':
                index += 2
                continue
            if char == quote:
                quote = None
            index += 1
            continue
        if char in {'"', "'"}:
            quote = char
            index += 1
            continue
        if char == "\\":
            index += 2
            continue
        match = (
            pattern.match(instruction, index)
            if index == 0 or instruction[index - 1] != "<"
            else None
        )
        if match is not None:
            delimiters.append((match.group("name"), bool(match.group("tabs"))))
            index = match.end()
            continue
        index += 1
    return delimiters


def _copy_instruction(
    operation: str, payload: str, dockerfile: Path
) -> tuple[list[str], dict[str, str | None]]:
    """Parse COPY/ADD while preserving JSON form after its leading option tokens."""
    flags: dict[str, str | None] = {}
    body = payload.lstrip()
    while body.startswith("--"):
        flag, separator, body = body.partition(" ")
        if not separator:
            raise ManifestError(f"{operation} in {dockerfile} has no source or destination")
        name, equals, value = flag[2:].partition("=")
        flags[name.lower()] = value if equals else None
        body = body.lstrip()

    if body.startswith("["):
        try:
            parsed = json.loads(body)
        except json.JSONDecodeError as exc:
            raise ManifestError(f"cannot parse JSON {operation} in {dockerfile}") from exc
        if not isinstance(parsed, list) or not all(isinstance(item, str) for item in parsed):
            raise ManifestError(f"invalid JSON {operation} in {dockerfile}")
        tokens = list(parsed)
    else:
        try:
            tokens = shlex.split(body, posix=True)
        except ValueError as exc:
            raise ManifestError(f"cannot parse {operation} in {dockerfile}: {exc}") from exc
    if len(tokens) < 2:
        raise ManifestError(f"{operation} in {dockerfile} needs a source and destination")
    return tokens, flags


def _run_mounts(line: str, dockerfile: Path) -> list[dict[str, str]]:
    match = re.match(r"^RUN\s+(.*)$", line, re.IGNORECASE)
    if not match:
        return []
    body = match.group(1).lstrip()
    mounts: list[dict[str, str]] = []
    token_pattern = re.compile(r"""^(?P<token>(?:[^\s'"\\]|\\.|"[^"]*"|'[^']*')+)""")
    while body.startswith("--"):
        token_match = token_pattern.match(body)
        if token_match is None:
            break
        raw_token = token_match.group("token")
        try:
            token = shlex.split(raw_token, posix=True)[0]
        except (IndexError, ValueError) as exc:
            raise ManifestError(f"cannot parse RUN option in {dockerfile}: {exc}") from exc
        body = body[token_match.end() :].lstrip()
        if not token.startswith("--mount="):
            continue
        options: dict[str, str] = {}
        for item in token.removeprefix("--mount=").split(","):
            name, equals, value = item.partition("=")
            if name:
                options[name.lower()] = value if equals else "true"
        mounts.append(options)
    return mounts


def _remote_add_kind(source: str) -> str | None:
    lowered = source.lower()
    if lowered.startswith(("git://", "ssh://", "git@")):
        return "git"
    if lowered.startswith(("http://", "https://")):
        path = lowered.split("#", 1)[0].split("?", 1)[0]
        return "git" if path.endswith(".git") else "http"
    return None


def _git_add_is_immutable(source: str) -> bool:
    fragment = source.rsplit("#", 1)[1] if "#" in source else ""
    revision = fragment.split(":", 1)[0]
    return re.fullmatch(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})", revision) is not None


def _copy_sources(dockerfile: Path) -> tuple[list[str], bool]:
    """Return local COPY/ADD sources and whether a conservative full-context hash is needed."""
    sources: list[str] = []
    full_context = False
    for line in _logical_dockerfile_lines(dockerfile.read_text(encoding="utf-8")):
        match = re.match(r"^(COPY|ADD)\s+(.*)$", line, re.IGNORECASE)
        if not match:
            for mount in _run_mounts(line, dockerfile):
                if mount.get("type", "bind").lower() != "bind" or mount.get("from"):
                    continue
                source = mount.get("source", mount.get("src", "."))
                if "$" in source:
                    full_context = True
                else:
                    sources.append(source)
            continue
        operation, payload = match.groups()
        operation = operation.upper()
        tokens, flags = _copy_instruction(operation, payload, dockerfile)
        if "from" in flags:
            if not flags["from"]:
                raise ManifestError(f"{operation} --from in {dockerfile} needs an image or stage")
            continue
        for source in tokens[:-1]:
            if source.startswith("<<"):
                # Heredoc bodies live in the Dockerfile itself, which is already hashed.
                continue
            if "$" in source:
                full_context = True
                continue
            if operation == "ADD" and (remote_kind := _remote_add_kind(source)):
                if remote_kind == "git" and not _git_add_is_immutable(source):
                    raise ManifestError(
                        f"Git ADD source {source!r} in {dockerfile} needs a full commit "
                        "reference for deterministic generation identity"
                    )
                if remote_kind == "http" and not flags.get("checksum"):
                    raise ManifestError(
                        f"remote ADD source {source!r} in {dockerfile} needs --checksum for "
                        "deterministic generation identity"
                    )
                continue
            sources.append(source)
    return sources, full_context


def _walk_source(root: Path, source: str) -> set[Path]:
    normalized = posixpath.normpath(source.replace("\\", "/").lstrip("/")) or "."
    while normalized == ".." or normalized.startswith("../"):
        normalized = normalized.removeprefix("..").lstrip("/") or "."
    if any(marker in normalized for marker in "*?["):
        matches = [
            path
            for path in root.rglob("*")
            if _glob_matches(normalized, path.relative_to(root).as_posix())
        ]
    else:
        candidate = root / normalized
        matches = [candidate] if candidate.exists() or candidate.is_symlink() else []
    if not matches:
        raise ManifestError(f"Docker build source {source!r} does not exist under {root}")
    paths: set[Path] = set()
    for match in matches:
        paths.add(match)
        if match.is_dir():
            paths.update(match.rglob("*"))
    return paths


def docker_context_references(root: Path, dockerfile: Path) -> list[Path]:
    """Resolve the local Docker build content closure used for generation identity."""
    if not dockerfile.is_file():
        raise ManifestError(f"stack references {dockerfile}, which does not exist")
    ignore_path = _dockerignore_path(root, dockerfile)
    rules = _dockerignore_rules(ignore_path)
    sources, full_context = _copy_sources(dockerfile)
    paths: set[Path] = {dockerfile}
    if ignore_path is not None:
        paths.add(ignore_path)
    if full_context:
        paths.update(root.rglob("*"))
    else:
        for source in sources:
            paths.update(_walk_source(root, source))
    protected = {dockerfile, ignore_path}
    return sorted(
        (path for path in paths if path in protected or not _is_ignored(path, root, rules)),
        key=lambda path: path.relative_to(root).as_posix(),
    )


_AUTOMATIC_PLATFORM_ARGS = frozenset(
    {
        "BUILDPLATFORM",
        "BUILDOS",
        "BUILDARCH",
        "BUILDVARIANT",
        "TARGETPLATFORM",
        "TARGETOS",
        "TARGETARCH",
        "TARGETVARIANT",
    }
)


def _substitute_build_args(
    value: str,
    args: dict[str, str],
    dockerfile: Path,
    *,
    allow_automatic_platform_args: bool = False,
) -> str:
    variable = re.compile(r"\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([^}]+)\})")

    def replace(match: re.Match[str]) -> str:
        name = match.group(1) or match.group(2) or ""
        if allow_automatic_platform_args and name in _AUTOMATIC_PLATFORM_ARGS:
            return match.group(0)
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) or name not in args:
            raise ManifestError(
                f"image reference {value!r} in {dockerfile} uses unresolved build argument {name!r}"
            )
        return args[name]

    return variable.sub(replace, value)


def dockerfile_external_images(root: Path, stack: StackSpec) -> list[tuple[str, str | None]]:
    """Return external image references and their requested platforms in build order."""
    if not stack.dockerfile:
        return []
    dockerfile = root / stack.dockerfile
    if not dockerfile.is_file():
        raise ManifestError(f"stack {stack.name!r} references {dockerfile}, which does not exist")
    aliases: set[str] = set()
    global_args: dict[str, str] = {}
    images: list[tuple[str, str | None]] = []
    first_stage_seen = False
    for line in _logical_dockerfile_lines(dockerfile.read_text(encoding="utf-8")):
        arg_match = re.match(r"^ARG\s+([A-Za-z_][A-Za-z0-9_]*)(?:=(.*))?$", line, re.IGNORECASE)
        if arg_match and not first_stage_seen:
            name, default = arg_match.groups()
            if default is not None:
                global_args[name] = _substitute_build_args(
                    default,
                    global_args,
                    dockerfile,
                    allow_automatic_platform_args=True,
                )
            continue
        from_match = re.match(r"^FROM\s+(.*)$", line, re.IGNORECASE)
        if from_match:
            try:
                tokens = shlex.split(from_match.group(1), posix=True)
            except ValueError as exc:
                raise ManifestError(f"cannot parse FROM in {dockerfile}: {exc}") from exc
            platform: str | None = None
            while tokens and tokens[0].startswith("--"):
                option = tokens.pop(0)
                if option.startswith("--platform="):
                    platform = _substitute_build_args(
                        option.partition("=")[2],
                        global_args,
                        dockerfile,
                        allow_automatic_platform_args=True,
                    )
            if not tokens:
                raise ManifestError(f"FROM in {dockerfile} needs an image reference")
            reference = _substitute_build_args(
                tokens.pop(0),
                global_args,
                dockerfile,
                allow_automatic_platform_args=True,
            )
            alias = tokens[1] if len(tokens) == 2 and tokens[0].lower() == "as" else None
            if tokens and alias is None:
                raise ManifestError(f"cannot parse FROM in {dockerfile}")
            if reference.lower() != "scratch" and reference.lower() not in aliases:
                images.append((reference, platform))
            if alias:
                aliases.add(alias.lower())
            first_stage_seen = True
            continue
        copy_match = re.match(r"^(?:COPY|ADD)\s+(.*)$", line, re.IGNORECASE)
        if copy_match:
            _tokens, flags = _copy_instruction("COPY", copy_match.group(1), dockerfile)
            from_reference = flags.get("from")
            if from_reference:
                from_reference = _substitute_build_args(from_reference, global_args, dockerfile)
                if (
                    not from_reference.isdigit()
                    and from_reference.lower() not in aliases
                    and from_reference.lower() != "scratch"
                ):
                    images.append((from_reference, None))
            continue
        for mount in _run_mounts(line, dockerfile):
            from_reference = mount.get("from")
            if not from_reference:
                continue
            from_reference = _substitute_build_args(
                from_reference,
                global_args,
                dockerfile,
                allow_automatic_platform_args=True,
            )
            if (
                not from_reference.isdigit()
                and from_reference.lower() not in aliases
                and from_reference.lower() != "scratch"
            ):
                images.append((from_reference, None))
    return images
