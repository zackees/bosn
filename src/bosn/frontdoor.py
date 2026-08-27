"""The `bosn-docker` front-door category table.

`bosn-docker` advertises a drop-in Docker/Compose surface. Every verb a caller might type
against it falls into exactly one of three categories, and this module is the single place
that decides which:

- ``GOVERNED`` -- bosn implements the verb itself. It creates or manages resources (a
  container, a volume, a network, an image), and every one of those resources is labeled,
  registered, and leased the same way anything else bosn creates is (see ``labels.py`` /
  ``registry.py``). ``init`` and ``compose`` are the current members.

- ``FORWARD`` -- the verb is passed verbatim to the real Docker binary because it is
  provably incapable of creating or mutating an engine-managed resource. This is a narrow
  allowlist by design: the bar is not "read-only in the common case", it is "cannot create
  a resource in *any* case". A verb only earns this category when its manual page is read
  end to end and the reasoning is written down next to it below.

- ``REFUSE`` -- everything else, including every verb this table has never heard of.
  Refusing carries a ``remedy``: the governed bosn equivalent, or the documented escape
  hatch to the real engine, so the caller has somewhere to go next instead of a dead end.

The load-bearing property is the *default*. A verb that is absent from ``VERBS`` is not
"probably fine to pass through" -- it is unknown, and unknown means refuse. Getting this
backwards would let an unrecognized verb like ``docker run`` fall through to the real
engine and silently create an unlabeled, unregistered, unleasable resource: precisely the
ungoverned accumulation bosn exists to prevent (see the project's #1 label contract).
``resolve()`` below makes that default an explicit branch in code -- never an implicit
`dict.get(verb)` that happens to return `None` and gets treated as "okay to forward" by an
inattentive caller.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

# Bumped only if the shape of supported()'s payload changes in a way a consumer parsing it
# would need to branch on (a field renamed or removed; a field's meaning changed). Adding a
# new verb to VERBS, or a new optional field, is not a breaking change and does not bump
# this. A consuming agent that pins to a schema_version it has validated against can detect
# drift instead of silently misreading a reshaped payload.
SCHEMA_VERSION = 1


class Category(Enum):
    """Where a `bosn-docker` verb's implementation lives.

    Values are lowercase strings, not auto() ints, because they round-trip through
    supported()'s JSON payload -- an agent parsing that JSON reads "governed", not "1".
    """

    GOVERNED = "governed"
    FORWARD = "forward"
    REFUSE = "refuse"


@dataclass(frozen=True)
class VerbSpec:
    """One row of the category table.

    `remedy` is required for REFUSE (there is always something else to do or say) and
    left `None` for GOVERNED/FORWARD, where "run the verb" already is the remedy.
    """

    verb: str
    category: Category
    summary: str
    remedy: str | None = None

    def __post_init__(self) -> None:
        if self.category is Category.REFUSE and not self.remedy:
            raise ValueError(f"REFUSE entry {self.verb!r} must carry a remedy")


# ---------------------------------------------------------------------------------------
# GOVERNED -- bosn implements these. Every resource they create is labeled and registered.
# ---------------------------------------------------------------------------------------
_GOVERNED: tuple[VerbSpec, ...] = (
    VerbSpec(
        verb="init",
        category=Category.GOVERNED,
        summary="translate compose.yaml into a starting bosn.toml manifest",
    ),
    VerbSpec(
        verb="compose",
        category=Category.GOVERNED,
        summary=(
            "managed Compose subset (up/down/logs/ps/build/run/exec/config); every "
            "resource labeled and leased"
        ),
    ),
)

# ---------------------------------------------------------------------------------------
# FORWARD -- passed verbatim to the real docker binary. Kept deliberately small: each entry
# below only exists because it is provably incapable of creating or mutating an
# engine-managed resource, not merely because it "usually" is. See the module docstring.
# ---------------------------------------------------------------------------------------
_FORWARD: tuple[VerbSpec, ...] = (
    # `docker version` prints client/server version metadata. It contacts the engine but
    # neither reads nor writes any container, image, volume, or network -- there is no
    # target argument for it to act on even if it wanted to.
    VerbSpec(
        verb="version",
        category=Category.FORWARD,
        summary="print client and engine version info (forwarded, read-only)",
    ),
    # `docker info` prints engine-wide diagnostics (driver, root dir, resource counts). It
    # is a global read with no resource-scoped target and no side effect on the engine.
    VerbSpec(
        verb="info",
        category=Category.FORWARD,
        summary="print engine-wide diagnostic info (forwarded, read-only)",
    ),
    # `docker login` writes a credential to the *local* Docker config file
    # (~/.docker/config.json). That file is not part of bosn's resource model -- it is not
    # a container, image, volume, or network, and nothing about it is labeled, leased, or
    # collected. It cannot create or mutate anything the registry tracks.
    VerbSpec(
        verb="login",
        category=Category.FORWARD,
        summary="authenticate to a registry (forwarded; touches local credential store only)",
    ),
    # `docker logout` is login's exact inverse: it deletes an entry from the same local
    # credential file and touches nothing else. Same reasoning, same conclusion.
    VerbSpec(
        verb="logout",
        category=Category.FORWARD,
        summary="clear registry credentials (forwarded; touches local credential store only)",
    ),
)

# The explicit safety allowlist tests assert FORWARD against: nothing here may be a verb
# that docker documents as creating, starting, stopping, or removing a container, image,
# volume, or network. Kept separate from _FORWARD (rather than derived from it) so that
# adding a dangerous verb to _FORWARD without updating this list is exactly the mistake
# the test is designed to catch, not something the list would quietly absorb.
FORWARD_SAFE_ALLOWLIST: frozenset[str] = frozenset({"version", "info", "login", "logout"})

# ---------------------------------------------------------------------------------------
# REFUSE -- explicitly refused, each with a remedy pointing at the governed bosn equivalent
# or the documented real-engine escape hatch. This list exists for verbs common enough that
# a specific, named refusal is more useful than falling through to the generic "unknown
# verb" refusal in resolve() -- it is not, and does not need to be, exhaustive: anything
# missing from this list and from GOVERNED/FORWARD is still refused, generically, by
# resolve()'s fail-closed default.
# ---------------------------------------------------------------------------------------
_ESCAPE_HATCH = (
    "not part of the managed subset; use the real `docker` binary directly if you "
    "accept the resources it creates will be unmanaged, or add it to bosn.toml so bosn "
    "can create it tracked"
)

_REFUSE: tuple[VerbSpec, ...] = (
    VerbSpec(
        "run",
        Category.REFUSE,
        "create and start an ad-hoc container",
        "resource-creating; use `bosn run` for a manifest-declared stack, or "
        "`bosn-docker compose up` for a multi-service project",
    ),
    VerbSpec(
        "create",
        Category.REFUSE,
        "create a container without starting it",
        "resource-creating; use `bosn ensure` to pre-warm a manifest-declared stack",
    ),
    VerbSpec(
        "build",
        Category.REFUSE,
        "build an image from a Dockerfile",
        "resource-creating; declare the build in bosn.toml and use `bosn ensure`/`bosn run`, "
        "which key the built image to a spec digest and track it",
    ),
    VerbSpec(
        "exec",
        Category.REFUSE,
        "run a command in a running container",
        "targets a raw container name outside bosn's registry; use `bosn shell` or "
        "`bosn run` against a manifest-declared stack",
    ),
    VerbSpec(
        "start",
        Category.REFUSE,
        "start a stopped container",
        "mutates container lifecycle state outside the registry; use `bosn run` or "
        "`bosn ensure` to bring up a managed stack",
    ),
    VerbSpec(
        "stop",
        Category.REFUSE,
        "stop a running container",
        "mutates container lifecycle state outside lease/GC bookkeeping; use `bosn done` "
        "to mark a workspace finished, or `bosn gc` to reclaim collectable resources",
    ),
    VerbSpec(
        "restart",
        Category.REFUSE,
        "restart a container",
        "mutates container lifecycle state outside the registry; use `bosn run` or "
        "`bosn ensure` to bring up a managed stack fresh",
    ),
    VerbSpec(
        "kill",
        Category.REFUSE,
        "send a signal to a running container",
        "mutates container lifecycle state outside the registry; use `bosn cancel` for a "
        "daemon-owned job, or `bosn gc` to reclaim collectable resources",
    ),
    VerbSpec(
        "rm",
        Category.REFUSE,
        "remove a container",
        "deletes outside lease/GC bookkeeping; use `bosn gc` to reclaim managed resources safely",
    ),
    VerbSpec(
        "rmi",
        Category.REFUSE,
        "remove an image",
        "deletes outside lease/GC bookkeeping; use `bosn gc` to reclaim managed resources safely",
    ),
    VerbSpec(
        "pull",
        Category.REFUSE,
        "download an image from a registry",
        "creates a local image copy outside the registry; declare the image in bosn.toml "
        "and let `bosn ensure` pull it tracked",
    ),
    VerbSpec(
        "push",
        Category.REFUSE,
        "upload an image to a registry",
        _ESCAPE_HATCH,
    ),
    VerbSpec(
        "tag",
        Category.REFUSE,
        "create a new tag pointing at an existing image",
        "creates a new local image reference outside the registry; " + _ESCAPE_HATCH,
    ),
    VerbSpec(
        "cp",
        Category.REFUSE,
        "copy files into or out of a container",
        "targets a raw container name outside bosn's registry; use `bosn shell` to reach "
        "a managed container's filesystem",
    ),
    VerbSpec(
        "network",
        Category.REFUSE,
        "manage networks (create/rm/connect/...)",
        "networks are declared implicitly by a stack and labeled by bosn; use "
        "`bosn status --json` for bounded managed-state diagnostics, "
        "`bosn gc --dry-run --json` for a rich Bosn-owned engine/storage report, or "
        "`bosn gc` to reclaim collectable networks",
    ),
    VerbSpec(
        "volume",
        Category.REFUSE,
        "manage volumes (create/rm/prune/...)",
        "volumes are declared in bosn.toml and labeled by bosn; use `bosn status --json` "
        "for bounded managed-state diagnostics, `bosn gc --dry-run --json` for a rich "
        "Bosn-owned engine/storage report, or `bosn gc` to reclaim collectable volumes",
    ),
    VerbSpec(
        "image",
        Category.REFUSE,
        "manage images (build/rm/prune/...)",
        "images are generation-keyed and managed by bosn; use `bosn status --json` for "
        "bounded managed-state diagnostics, `bosn gc --dry-run --json` for a rich "
        "Bosn-owned engine/storage report, or `bosn gc` to reclaim collectable images",
    ),
    VerbSpec(
        "container",
        Category.REFUSE,
        "manage containers (create/rm/prune/...)",
        "containers are declared in bosn.toml and labeled by bosn; use `bosn status --json` "
        "for bounded managed-state diagnostics, `bosn gc --dry-run --json` for a rich "
        "Bosn-owned engine/storage report, or `bosn gc` to reclaim collectable containers",
    ),
    VerbSpec(
        "system",
        Category.REFUSE,
        "manage or inspect the engine (df/prune/events/...)",
        "`system prune` in particular deletes resources bosn did not choose to reclaim; "
        "use `bosn gc --dry-run --json` for a rich Bosn-owned engine/storage report, then "
        "`bosn gc` to reclaim collectable resources",
    ),
    VerbSpec(
        "save",
        Category.REFUSE,
        "export an image to a tar archive",
        _ESCAPE_HATCH,
    ),
    VerbSpec(
        "load",
        Category.REFUSE,
        "import an image from a tar archive",
        "creates a local image outside the registry; " + _ESCAPE_HATCH,
    ),
    VerbSpec(
        "export",
        Category.REFUSE,
        "export a container's filesystem to a tar archive",
        _ESCAPE_HATCH,
    ),
    VerbSpec(
        "import",
        Category.REFUSE,
        "create an image from a tarball",
        "creates a local image outside the registry; " + _ESCAPE_HATCH,
    ),
    VerbSpec(
        "commit",
        Category.REFUSE,
        "create a new image from a container's changes",
        "creates a local image outside the registry; " + _ESCAPE_HATCH,
    ),
    VerbSpec(
        "rename",
        Category.REFUSE,
        "rename a container",
        "mutates a container's identity outside the registry's naming; " + _ESCAPE_HATCH,
    ),
    VerbSpec(
        "attach",
        Category.REFUSE,
        "attach local streams to a running container",
        "targets a raw container name outside bosn's registry; use `bosn attach` for a "
        "daemon-owned job",
    ),
    VerbSpec(
        "ps",
        Category.REFUSE,
        "list containers",
        "lists raw engine state instead of Bosn's bounded managed-state diagnostics; use "
        "`bosn status --json` for those diagnostics, `bosn tasks` for manifest readiness, or "
        "`bosn gc --dry-run --json` for a rich Bosn-owned engine/storage report",
    ),
    VerbSpec(
        "logs",
        Category.REFUSE,
        "fetch a container's logs",
        "targets a raw container name outside bosn's registry; use `bosn-docker compose "
        "logs` for a managed stack, or `bosn attach` for a daemon-owned job",
    ),
    VerbSpec(
        "inspect",
        Category.REFUSE,
        "show low-level details of an object",
        "targets a raw object name outside bosn's registry; use `bosn status --json` for "
        "bounded managed-state diagnostics or `bosn gc --dry-run --json` for a rich "
        "Bosn-owned engine/storage report",
    ),
    VerbSpec(
        "top",
        Category.REFUSE,
        "list processes running in a container",
        "targets a raw container name outside bosn's registry; use `bosn jobs` for "
        "governed introspection",
    ),
    VerbSpec(
        "stats",
        Category.REFUSE,
        "stream resource usage statistics",
        "targets raw container names outside bosn's registry; Bosn does not expose a "
        "streaming resource-usage view. Use `bosn status --json` for bounded managed-state "
        "diagnostics or `bosn gc --dry-run --json` for a rich Bosn-owned engine/storage report",
    ),
    VerbSpec(
        "events",
        Category.REFUSE,
        "stream real-time engine events",
        "surfaces raw engine activity outside bosn's registry; Bosn does not expose a raw "
        "engine event stream. Use `bosn status --json` for bounded managed-state diagnostics "
        "or `bosn gc --dry-run --json` for a rich Bosn-owned engine/storage report",
    ),
    VerbSpec(
        "port",
        Category.REFUSE,
        "list a container's published ports",
        "targets a raw container name outside bosn's registry; Bosn does not expose a "
        "published-port listing. Use `bosn status --json` only for bounded managed-state "
        "diagnostics or `bosn gc --dry-run --json` for a rich Bosn-owned engine/storage report",
    ),
    VerbSpec(
        "diff",
        Category.REFUSE,
        "list changed files in a container's filesystem",
        "targets a raw container name outside bosn's registry; use `bosn shell` to "
        "inspect a managed container directly",
    ),
    VerbSpec(
        "wait",
        Category.REFUSE,
        "block until a container stops, then print its exit code",
        "targets a raw container name outside bosn's registry; use `bosn jobs`/`bosn "
        "attach` to wait on a daemon-owned job",
    ),
    VerbSpec(
        "search",
        Category.REFUSE,
        "search Docker Hub for images",
        _ESCAPE_HATCH,
    ),
)

VERBS: tuple[VerbSpec, ...] = _GOVERNED + _FORWARD + _REFUSE


def _index_by_verb(verbs: tuple[VerbSpec, ...]) -> dict[str, VerbSpec]:
    index: dict[str, VerbSpec] = {}
    for spec in verbs:
        if spec.verb in index:
            raise ValueError(f"duplicate verb in VERBS: {spec.verb!r}")
        index[spec.verb] = spec
    return index


_BY_VERB: dict[str, VerbSpec] = _index_by_verb(VERBS)


# ---------------------------------------------------------------------------------------
# Compose flag surface (#47) -- the GOVERNED `compose` verb's own sub-table.
#
# `compose` is one row above, but it is not a leaf: it has its own sub-verbs (`up`,
# `down`, ...) and each of those has its own flag surface. Before this table existed,
# `_run_compose` refused *any* token after the sub-verb -- "unsupported compose flag or
# argument" for literally anything, including `-d`. That satisfied #47's "no silent
# ignoring" half by refusing everything, but not its "decided subset" half: `up -d --wait`
# and `down -v --remove-orphans` are named in the issue as flags bosn must actually accept.
#
# Same shape as the verb table above, for the same reason: a flag a caller might type is
# either ACCEPTED (bosn understands it and passes it through) or REFUSED (named, with a
# remedy) -- there is no third "silently drop it" option, because that is exactly the
# failure mode #47's last acceptance criterion exists to prevent.
# ---------------------------------------------------------------------------------------


class FlagStatus(Enum):
    """Whether a compose sub-verb's flag is understood, mirroring `Category` above.

    There is no FORWARD equivalent here: a flag either round-trips through bosn's overlay
    machinery (ACCEPTED) or it does not exist as far as bosn is concerned (REFUSED). Values
    are lowercase strings for the same JSON-round-trip reason `Category` gives.
    """

    ACCEPTED = "accepted"
    REFUSED = "refused"


@dataclass(frozen=True)
class ComposeFlagSpec:
    """One flag a `compose` sub-verb (`up`, `down`, ...) either accepts or refuses.

    `aliases` holds the flag's other spellings (`-d` and `--detach` are one spec, not
    two) so a lookup can resolve either without the table declaring the same flag twice.
    `takes_value` says whether the flag consumes a following token as its argument
    (`--file compose.yaml`) or stands alone (`--wait`) -- callers building an argv parser
    for a sub-verb need this to know how many tokens a flag occupies.

    `remedy` is required for REFUSED (same contract `VerbSpec` enforces for REFUSE: there
    is always something else to do or say) and left `None` for ACCEPTED, where "the flag
    works" already is the remedy.
    """

    flag: str
    status: FlagStatus
    takes_value: bool = False
    summary: str = ""
    remedy: str | None = None
    aliases: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        if self.status is FlagStatus.REFUSED and not self.remedy:
            raise ValueError(f"REFUSED flag {self.flag!r} must carry a remedy")
        if self.status is FlagStatus.ACCEPTED and self.remedy is not None:
            raise ValueError(f"ACCEPTED flag {self.flag!r} must not carry a remedy")

    def names(self) -> tuple[str, ...]:
        """Every spelling this spec answers to: the primary flag plus its aliases."""
        return (self.flag, *self.aliases)


# The remedy for a flag this table has never heard of, for a `compose` sub-verb it does
# recognize. Parallel to `_UNKNOWN_VERB_REMEDY`: generic, because there is no specific
# governed alternative to point at for a flag this table doesn't know exists.
def _unknown_compose_flag_remedy(command: str) -> str:
    return (
        f"not part of bosn's managed `compose {command}` flag subset; run `bosn-docker "
        "--supported --json` to see every accepted flag per compose command, or use the "
        "real `docker compose` binary directly if you accept the resources it creates "
        "will be unmanaged"
    )


# `-f`/`--file` selects which compose file to read. It is parsed ahead of the sub-verb
# (`bosn-docker compose -f compose.yaml up`, not `... up -f compose.yaml`) and applies to
# every sub-verb identically, so it lives here once rather than being copied into all
# eight per-command tuples below -- the same reasoning `_ESCAPE_HATCH` gives for sharing
# one remedy string across REFUSE rows instead of retyping it.
COMPOSE_GLOBAL_FLAGS: tuple[ComposeFlagSpec, ...] = (
    ComposeFlagSpec(
        flag="-f",
        status=FlagStatus.ACCEPTED,
        takes_value=True,
        summary="path to the compose file (already wired; predates #47)",
        aliases=("--file",),
    ),
)

# `up`: the issue names `-d`/`--wait` explicitly ("up -d --wait"). `--build`,
# `--force-recreate`, `--no-recreate`, and `--scale` are all real `docker compose up`
# flags, but none is named in #47, and each has a bosn-shaped reason to stay out of v1:
# `--build` would rebuild an image bosn did not track through `bosn ensure`'s spec-digest
# keying (see the `build` REFUSE row above), and `--remove-orphans` is accepted on `down`
# below, not here, matching the issue's own pairing of that flag with `down -v`.
_COMPOSE_UP_FLAGS: tuple[ComposeFlagSpec, ...] = (
    ComposeFlagSpec(
        flag="-d",
        status=FlagStatus.ACCEPTED,
        summary="run containers in the background instead of attaching to their output",
        aliases=("--detach",),
    ),
    ComposeFlagSpec(
        flag="--wait",
        status=FlagStatus.ACCEPTED,
        summary="wait for services to report healthy/running before returning",
    ),
    ComposeFlagSpec(
        flag="--build",
        status=FlagStatus.REFUSED,
        summary="rebuild images before starting",
        remedy=(
            "would rebuild an image outside `bosn ensure`'s spec-digest tracking; declare "
            "the build in bosn.toml and run `bosn ensure` first, then `compose up`"
        ),
    ),
    ComposeFlagSpec(
        flag="--remove-orphans",
        status=FlagStatus.REFUSED,
        summary="remove containers for services not defined in the compose file",
        remedy=(
            "accepted on `compose down`, not `up`, in this subset; run "
            "`bosn-docker compose down --remove-orphans`"
        ),
    ),
)

# `down`: the issue names `-v`/`--remove-orphans` explicitly ("down -v --remove-orphans").
# `--rmi` is refused for the same reason the top-level `rmi` verb is refused above:
# deleting images outside lease/GC bookkeeping. `-t`/`--timeout` is real but unnamed by the
# issue; kept out per "prefer the smaller set" rather than guessed at.
_COMPOSE_DOWN_FLAGS: tuple[ComposeFlagSpec, ...] = (
    ComposeFlagSpec(
        flag="-v",
        status=FlagStatus.ACCEPTED,
        summary="remove named volumes declared in the compose file's `volumes` section",
        aliases=("--volumes",),
    ),
    ComposeFlagSpec(
        flag="--remove-orphans",
        status=FlagStatus.ACCEPTED,
        summary="remove containers for services not defined in the compose file",
    ),
    ComposeFlagSpec(
        flag="--rmi",
        status=FlagStatus.REFUSED,
        takes_value=True,
        summary="remove images used by services",
        remedy=(
            "deletes images outside lease/GC bookkeeping; use `bosn gc` to reclaim "
            "managed resources safely"
        ),
    ),
    ComposeFlagSpec(
        flag="-t",
        status=FlagStatus.REFUSED,
        takes_value=True,
        summary="shutdown timeout in seconds before a container is killed",
        remedy=_unknown_compose_flag_remedy("down"),
        aliases=("--timeout",),
    ),
)

# `logs`: the issue names no flags for it, so the accepted set is empty -- "prefer the
# smaller set" per the task brief. `-f` is called out by name (not left to the generic
# fallback) because it collides with the global `-f`/`--file` above: in real
# `docker compose logs`, `-f` means `--follow`, a different flag entirely from the file
# selector every other sub-verb reads `-f` as. Naming it here stops that collision from
# reading as bosn silently reinterpreting `-f` rather than refusing it outright.
_COMPOSE_LOGS_FLAGS: tuple[ComposeFlagSpec, ...] = (
    ComposeFlagSpec(
        flag="-f",
        status=FlagStatus.REFUSED,
        summary="follow log output (docker compose's meaning; NOT bosn's global --file)",
        remedy=(
            "`-f` is bosn's global compose-file selector, not `--follow`, in this "
            "subset; `--follow` itself is not part of the managed flag surface -- "
            + _unknown_compose_flag_remedy("logs")
        ),
        aliases=("--follow",),
    ),
)

# `ps`, `build`, `run`, `exec`, `config`: the issue names these as sub-verbs bosn must
# accept (#47: "build/run/exec/config") but names no flags for any of them. Empty accepted
# sets, same "prefer the smaller set" call as `logs` -- every flag on these four falls
# through to the generic per-command remedy until a future issue names one specifically.
_COMPOSE_PS_FLAGS: tuple[ComposeFlagSpec, ...] = ()
_COMPOSE_BUILD_FLAGS: tuple[ComposeFlagSpec, ...] = ()
_COMPOSE_RUN_FLAGS: tuple[ComposeFlagSpec, ...] = ()
_COMPOSE_EXEC_FLAGS: tuple[ComposeFlagSpec, ...] = ()
_COMPOSE_CONFIG_FLAGS: tuple[ComposeFlagSpec, ...] = ()

# The single source of truth for both the sub-verb surface (`COMPOSE_COMMANDS` below is
# derived from these keys, not hand-copied) and each sub-verb's flags. Order is
# deliberate and matches the order the `compose` row's summary lists them in.
COMPOSE_FLAGS: dict[str, tuple[ComposeFlagSpec, ...]] = {
    "up": _COMPOSE_UP_FLAGS,
    "down": _COMPOSE_DOWN_FLAGS,
    "logs": _COMPOSE_LOGS_FLAGS,
    "ps": _COMPOSE_PS_FLAGS,
    "build": _COMPOSE_BUILD_FLAGS,
    "run": _COMPOSE_RUN_FLAGS,
    "exec": _COMPOSE_EXEC_FLAGS,
    "config": _COMPOSE_CONFIG_FLAGS,
}

# `docker_cli._run_compose` checks membership in this instead of hand-maintaining a second
# list that could drift from the one above it dispatches against -- the same "one source of
# truth" argument the module docstring makes for verbs. (It is deliberately *not* wired into
# `_parse_compose_args` as an argparse `choices=`: that would refuse an unknown sub-verb with
# a bare `SystemExit(2)` and a usage dump instead of bosn's structured refusal. See that
# function's docstring.)
COMPOSE_COMMANDS: tuple[str, ...] = tuple(COMPOSE_FLAGS.keys())

# Sub-verbs whose grammar is `<verb> [OPTIONS] SERVICE [COMMAND [ARGS...]]` rather than
# `<verb> [OPTIONS]`. Docker requires a SERVICE for both -- `docker compose run` with no
# service is an error from Compose itself -- so a front door that validated every token
# against the flag table would refuse these verbs their own mandatory argument and leave
# them declared-but-unusable, which is the #47 bug in a different disguise.
#
# The split this drives (in `docker_cli._validate_compose_flags`): tokens are checked as
# flags only up to the first non-`-` token; that token is the SERVICE, and everything after
# it is the *container's* argv, forwarded untouched. `compose exec app ls -la` must reach
# the engine with `-la` intact -- it belongs to `ls`, not to `compose exec`, and running it
# through `resolve_compose_flag` would refuse a legitimate command line.
#
# Lives here rather than as a literal in `docker_cli` so the sub-verb surface and its
# grammar stay described in one place. `logs`/`ps` are deliberately absent: they accept
# optional service names from Docker, but bosn has always scoped them to the whole project
# and widening that is not part of #47.
COMPOSE_SERVICE_COMMANDS: frozenset[str] = frozenset({"run", "exec"})


def _index_compose_flags(
    flags: tuple[ComposeFlagSpec, ...],
) -> dict[str, ComposeFlagSpec]:
    index: dict[str, ComposeFlagSpec] = {}
    for spec in flags:
        for name in spec.names():
            if name in index:
                raise ValueError(f"duplicate compose flag spelling: {name!r}")
            index[name] = spec
    return index


_COMPOSE_GLOBAL_INDEX: dict[str, ComposeFlagSpec] = _index_compose_flags(COMPOSE_GLOBAL_FLAGS)


def _merge_with_globals(flags: tuple[ComposeFlagSpec, ...]) -> dict[str, ComposeFlagSpec]:
    """Merge one sub-verb's flags over the globals, letting the sub-verb win a collision.

    A plain concatenate-then-index would raise on `logs`' `-f`, which deliberately reuses
    the `-f` spelling the globals already claim for `--file` -- see `_COMPOSE_LOGS_FLAGS`'s
    comment on why that collision is named as a REFUSED row instead of silently inheriting
    the global meaning. "Sub-verb wins" is what makes that override actually take effect
    instead of the duplicate-spelling guard rejecting it as a table bug.
    """
    merged = dict(_COMPOSE_GLOBAL_INDEX)
    merged.update(_index_compose_flags(flags))
    return merged


# Per-command index, each pre-merged with the globals so a lookup never has to check two
# tables. Built once at import time from `COMPOSE_FLAGS`/`COMPOSE_GLOBAL_FLAGS` -- there is
# no second copy of this merge anywhere else for it to drift from.
_COMPOSE_FLAGS_BY_COMMAND: dict[str, dict[str, ComposeFlagSpec]] = {
    command: _merge_with_globals(flags) for command, flags in COMPOSE_FLAGS.items()
}


def compose_flag_spec_for(command: str, flag: str) -> ComposeFlagSpec | None:
    """Look up `flag`'s spec for `command` (its primary spelling or any alias).

    Raw lookup, `spec_for`'s compose-flag counterpart: `None` means the table has no
    opinion, and callers must not treat that as "safe to pass through" -- see
    `resolve_compose_flag` for the fail-closed wrapper. `command` must be a member of
    `COMPOSE_COMMANDS`; anything else is a caller bug (an unrecognized sub-verb never
    reaches flag parsing at all, since it fails sub-verb resolution first), so this raises
    rather than silently returning `None` for a command as well as a flag.

    `flag` must already be the bare flag spelling with any `=value` split off by the
    caller (`--file=compose.yaml` -> pass `"--file"`, not the raw token) -- this table
    only knows flag identity, not how a caller chose to spell a value onto it.
    """
    if command not in _COMPOSE_FLAGS_BY_COMMAND:
        raise ValueError(f"{command!r} is not a compose sub-verb; see COMPOSE_COMMANDS")
    return _COMPOSE_FLAGS_BY_COMMAND[command].get(flag)


def resolve_compose_flag(command: str, flag: str) -> ComposeFlagSpec:
    """The fail-closed caller-facing entry point for a compose sub-verb's flag.

    Always returns a `ComposeFlagSpec`, never `None`: a flag present in `command`'s table
    (or the globals) returns its row; a flag absent from both is unknown, and this
    function's job is to make "unknown" resolve to REFUSED, with a usable remedy, as an
    explicit branch here -- the same fail-closed shape `resolve()` gives verbs, for the
    same reason. Dispatch code should call this, not `compose_flag_spec_for`, so an
    un-cataloged flag can never fall through to "pass it to the real engine" by accident.
    """
    spec = compose_flag_spec_for(command, flag)
    if spec is not None:
        return spec
    return ComposeFlagSpec(
        flag=flag,
        status=FlagStatus.REFUSED,
        summary="unrecognized compose flag",
        remedy=_unknown_compose_flag_remedy(command),
    )


def spec_for(verb: str) -> VerbSpec | None:
    """Look up `verb`'s table row. `None` means the table has no opinion -- callers must
    not treat that as "safe to forward"; see `resolve()` for the fail-closed wrapper.
    """
    return _BY_VERB.get(verb)


# The remedy attached to a verb resolve() has never heard of. Generic on purpose: there is
# no specific governed equivalent to point at for a verb this table doesn't recognize.
_UNKNOWN_VERB_REMEDY = (
    "not part of the introspectable bosn-docker subset; run `bosn-docker --supported "
    "--json` to see every governed, forwarded, and refused verb, or use the real `docker` "
    "binary directly if you accept the resources it creates will be unmanaged"
)


def resolve(verb: str) -> VerbSpec:
    """The fail-closed caller-facing entry point: always returns a decision, never `None`.

    A verb present in `VERBS` returns its row. A verb absent from `VERBS` is unknown, and
    this function's whole job is to make "unknown" resolve to REFUSE as an explicit branch
    here rather than as a side effect of some other function's lookup miss. Dispatch code
    should call this, not `spec_for`, for exactly that reason.
    """
    spec = spec_for(verb)
    if spec is not None:
        return spec
    return VerbSpec(
        verb=verb,
        category=Category.REFUSE,
        summary="unrecognized docker verb",
        remedy=_UNKNOWN_VERB_REMEDY,
    )


def _compose_flag_payload(spec: ComposeFlagSpec) -> dict:
    return {
        "flag": spec.flag,
        "aliases": list(spec.aliases),
        "status": spec.status.value,
        "takes_value": spec.takes_value,
        "summary": spec.summary,
        "remedy": spec.remedy,
    }


def supported() -> dict:
    """The payload behind `bosn-docker --supported --json`.

    `VERBS` is the only source for the verb table, and `COMPOSE_FLAGS`/
    `COMPOSE_GLOBAL_FLAGS` are the only source for the `compose` verb's flag surface --
    there is no second, hand-maintained list for parser help text, generated docs, or this
    JSON to drift out of sync with. Every field here is JSON-primitive (str/bool/None/list
    of same), so `json.dumps(supported())` always round-trips for the agent this is
    written for.

    Adding the `compose` key below is an additive change to this payload's shape: existing
    consumers reading `schema_version`/`verbs` see no field renamed or removed underneath
    them, so `SCHEMA_VERSION` does not move -- see its module-level comment.
    """
    return {
        "schema_version": SCHEMA_VERSION,
        "verbs": [
            {
                "verb": spec.verb,
                "category": spec.category.value,
                "summary": spec.summary,
                "remedy": spec.remedy,
            }
            for spec in VERBS
        ],
        "compose": {
            "commands": list(COMPOSE_COMMANDS),
            "global_flags": [_compose_flag_payload(spec) for spec in COMPOSE_GLOBAL_FLAGS],
            "flags": {
                command: [_compose_flag_payload(spec) for spec in flags]
                for command, flags in COMPOSE_FLAGS.items()
            },
        },
    }
