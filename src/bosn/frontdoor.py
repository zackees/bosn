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
        summary="managed Compose subset (up/down/logs/ps); every resource labeled and leased",
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
        "`bosn status` to inspect them, `bosn gc` to reclaim them",
    ),
    VerbSpec(
        "volume",
        Category.REFUSE,
        "manage volumes (create/rm/prune/...)",
        "volumes are declared in bosn.toml and labeled by bosn; use `bosn status` to "
        "inspect them, `bosn gc` to reclaim them",
    ),
    VerbSpec(
        "image",
        Category.REFUSE,
        "manage images (build/rm/prune/...)",
        "images are generation-keyed and managed by bosn; use `bosn status`/`bosn gc` "
        "instead of the raw image subcommands",
    ),
    VerbSpec(
        "container",
        Category.REFUSE,
        "manage containers (create/rm/prune/...)",
        "containers are declared in bosn.toml and labeled by bosn; use "
        "`bosn status`/`bosn gc` instead of the raw container subcommands",
    ),
    VerbSpec(
        "system",
        Category.REFUSE,
        "manage or inspect the engine (df/prune/events/...)",
        "`system prune` in particular deletes resources bosn did not choose to reclaim; "
        "use `bosn gc` for a governed equivalent",
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
        "lists raw engine state instead of bosn's managed view; use `bosn status` or "
        "`bosn tasks` for governed introspection",
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
        "targets a raw object name outside bosn's registry; use `bosn status` for "
        "governed introspection",
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
        "targets raw container names outside bosn's registry; use `bosn status` for "
        "governed introspection",
    ),
    VerbSpec(
        "events",
        Category.REFUSE,
        "stream real-time engine events",
        "surfaces raw engine activity outside bosn's registry; use `bosn status` for "
        "governed introspection",
    ),
    VerbSpec(
        "port",
        Category.REFUSE,
        "list a container's published ports",
        "targets a raw container name outside bosn's registry; use `bosn status` for "
        "governed introspection",
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


def supported() -> dict:
    """The payload behind `bosn-docker --supported --json`.

    `VERBS` is the only source for it -- there is no second, hand-maintained list for
    parser help text, generated docs, or this JSON to drift out of sync with. Every field
    here is JSON-primitive (str/None), so `json.dumps(supported())` always round-trips for
    the agent this is written for.
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
    }
