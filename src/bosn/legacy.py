"""Legacy-producer adoption: ``bosn adopt --legacy <family>``.

Issue #1's design promised that "legacy `com.clud.docker-build.*`, `io.soldr.perf-local.*`,
and `io.zccache.perf-local.*` resources migrate through the same `adopt` machinery. Unknown
names remain manual." Issue #18 restated it as "`adopt --legacy` for resources from clud
docker-build / soldr / zccache predating bosn," and #41's acceptance criterion is that this
path "handles documented clud/soldr/zccache contracts without guessing from names."

That last clause is the whole point of this module. ``labels.py`` already says it for bosn's
own contract: "Name prefixes are never ownership proof." The same rule applies to a legacy
producer's resources: a volume named ``clud-docker-build-soldr-...`` is not evidence of
anything. What counts as evidence is carrying that producer's own ``.managed=true`` label
under its own namespace -- the same proof standard bosn holds itself to. So selection here is
strictly: (1) the caller names a known family, (2) a resource qualifies only if it carries
that family's managed label. Substring/prefix matching on names never enters the decision.

Ground truth for the label keys below was read directly from the producer source on this
machine, not inferred or guessed:

- **clud** (`com.clud.docker-build`) --
  ``clud/crates/clud-bin/assets/tools/docker/docker_build_soldr.py``. ``LABEL_NS =
  "com.clud.docker-build"``. Every managed object gets ``.managed=true``, ``.stack``,
  ``.project-key``, ``.project-root``; cache volumes additionally get ``.role`` (the
  per-mount purpose, e.g. ``target``/``cargo-home``/``rustup-home``/``cargo-chef``) via
  ``.cache-role``, applied through ``--label {LABEL_NS}.cache-role={role}`` at volume
  creation. There is no schema/version label -- clud has never needed to change this
  contract's shape.
- **soldr** (`io.soldr.perf-local`) -- ``soldr/ci/perf_local.py``. ``LABEL_PREFIX =
  "io.soldr.perf-local"``. ``RUNNER_SCHEMA`` was ``"2"`` when this module was written
  ("Bumped from '1': schema 1 runners are the pre-per-root shared containers" -- a real
  breaking change in what the labels mean) and is ``"7"`` as of soldr 75d7cc80.
  The *runner container* gets ``.managed``, ``.schema``, ``.image-id``, ``.source-root``,
  ``.ptrace``, ``.dockerfile-sha256``. Its *volumes* (target/cargo-home/soldr-home) get only
  ``.managed`` and ``.source-root`` -- notably **not** ``.schema``. Since volumes are the only
  kind adopted here, ``.schema`` is validated when present and not required; see ``SOLDR``.
- **zccache** (`io.zccache.perf-local`) -- named in the design, but a repo search
  (`dev/zccache`) found no producer that emits this namespace: only vendored dependencies
  and build artifacts, no source that writes ``io.zccache.perf-local.*`` labels. The family
  is implemented from the documented namespace and the ``.managed=true`` convention the other
  two families share, per the design text quoted above. Nothing beyond that is fabricated:
  the workspace-bearing label this family requires (``.workspace``) is a documented
  assumption, called out where it is used, and a resource missing it is refused rather than
  guessed at.

Mapping onto bosn's contract (``labels.py``: registry, kind, stack, generation, scope,
workspace, created) is per-family and deliberately conservative about two fields that have no
legacy source at all:

- ``generation`` -- a legacy *volume's* labels never carry a content digest (soldr's
  ``.dockerfile-sha256`` lives on its container, which cannot be relabeled -- see below --
  and clud/zccache have no digest concept at all). Guessing one would let a stale legacy
  cache masquerade as current. Every adopted resource instead gets
  ``LEGACY_GENERATION_SENTINEL``, a value no real ``generation_digest()`` output can ever
  equal, so the first ordinary converge treats it as superseded rather than reusable --
  fail toward "rebuild once," never toward silently trusting unknown content.
- ``created`` -- none of the three producers stamp a creation label. The adopted-resource
  registry row already gets its ``created_at`` from adoption time (see
  ``resources.adopt``, which passes ``now`` regardless of the label), so the label value
  written back onto the object only needs to be a well-formed timestamp for humans reading
  `docker inspect`; it is set to the adoption instant for the same reason.

Only **volumes** are ever adopted. Docker labels are immutable, so relabeling a container or
image means destroying and rebuilding it under this module -- unlike a volume, there is no
safe staged-copy equivalent, and doing so would silently discard a running legacy build
environment. Containers and images carrying a known family's managed label are reported as
skipped, exactly like the existing ``--transfer`` path already refuses non-volume kinds
(see ``resources.transfer_volume``).
"""

from __future__ import annotations

import datetime as dt
from dataclasses import dataclass, field

from bosn import labels
from bosn.engine import Engine
from bosn.resources import DiscoveredResource, ResourceScanner

# No legacy producer's volume-level labels carry a content digest (see module docstring).
# The sentinel is deliberately not a valid `generation_digest()` output (those are always
# `sha256:<hex>`), so an adopted resource is guaranteed to be superseded by the first real
# converge rather than mistaken for a fresh generation.
LEGACY_GENERATION_SENTINEL = "legacy:no-generation-source"


class LegacyAdoptionError(ValueError):
    """A legacy resource cannot be safely mapped onto the bosn label contract."""


class UnknownLegacyFamilyError(ValueError):
    """``--legacy <family>`` did not name one of the documented families."""


def _now_iso(now: float) -> str:
    return dt.datetime.fromtimestamp(now, dt.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


@dataclass(frozen=True)
class LegacyFamily:
    """One versioned, documented legacy producer contract.

    ``version`` is *our* migration-rule version for this family, bumped whenever the
    mapping below changes in a way that could alter which resources qualify or how they
    map. ``schema_label``/``recognized_schema_versions`` are separate: they validate the
    *producer's own* schema label where one exists (currently only soldr's ``.schema``).
    """

    name: str
    namespace: str
    version: str
    workspace_label: str
    stack_label: str | None
    stack_sentinel: str
    schema_label: str | None = None
    recognized_schema_versions: frozenset[str] = field(default_factory=frozenset)

    @property
    def managed_label(self) -> str:
        return f"{self.namespace}.managed"

    def qualifies(self, raw: dict[str, str]) -> bool:
        """Ownership proof for this family: exactly its own managed label, nothing else.

        Never inspects ``name`` -- a resource named as if it belonged to this family but
        missing the label is invisible here, by design (the anti-name-guessing rule).
        """
        return raw.get(self.managed_label) == "true"

    def map_labels(
        self, raw: dict[str, str], *, registry_id: str, now: float
    ) -> labels.ResourceLabels:
        """Map one qualifying resource's raw labels onto the full bosn contract.

        Raises :class:`LegacyAdoptionError` rather than substituting a guess whenever a
        required field is absent or a producer schema version is unrecognized.
        """
        if not self.qualifies(raw):
            raise LegacyAdoptionError(
                f"resource does not carry the {self.name} managed label {self.managed_label!r}"
            )
        if self.schema_label is not None:
            schema_value = raw.get(self.schema_label)
            # Validated only when the producer actually stamped it. soldr writes .schema
            # onto its runner *container* but not onto its volumes (verified in
            # soldr/ci/perf_local.py: `docker volume create` passes only .managed and
            # .source-root), and volumes are the only kind this path can relabel. Requiring
            # a label the producer never writes would make every soldr adoption fail closed
            # -- a feature that refuses its own documented contract 100% of the time. An
            # absent label is therefore "no version claim", and the family's required
            # fields below carry the safety; a *present* but unrecognized value still
            # refuses, which is the case that signals a real producer schema change.
            if schema_value is not None and schema_value not in self.recognized_schema_versions:
                raise LegacyAdoptionError(
                    f"{self.name} resource has unrecognized schema "
                    f"{schema_value!r} for label {self.schema_label!r}; expected one of "
                    f"{sorted(self.recognized_schema_versions)} -- refusing to migrate "
                    "blindly across a producer schema change"
                )
        workspace = raw.get(self.workspace_label)
        if not workspace:
            raise LegacyAdoptionError(
                f"{self.name} resource is missing its workspace label "
                f"{self.workspace_label!r}; cannot adopt without a workspace"
            )
        stack = (raw.get(self.stack_label) if self.stack_label else None) or self.stack_sentinel
        return labels.ResourceLabels(
            registry=registry_id,
            kind="volume",
            stack=stack,
            generation=LEGACY_GENERATION_SENTINEL,
            # Legacy caches are keyed to one project/checkout directory (clud's
            # project-root, soldr's source-root), not shared machine-wide -- "stack"
            # is the closest of bosn's three SCOPES to what these actually are.
            scope="stack",
            workspace=workspace,
            created=_now_iso(now),
        )


CLUD = LegacyFamily(
    name="clud",
    namespace="com.clud.docker-build",
    version="1",
    workspace_label="com.clud.docker-build.project-root",
    stack_label="com.clud.docker-build.stack",
    stack_sentinel="clud-docker-build",  # unused: clud always sets .stack directly.
)

SOLDR = LegacyFamily(
    name="soldr",
    namespace="io.soldr.perf-local",
    version="1",
    workspace_label="io.soldr.perf-local.source-root",
    stack_label=None,  # soldr has no stack concept of its own.
    stack_sentinel="soldr-perf-local",
    schema_label="io.soldr.perf-local.schema",
    # Only RUNNER_SCHEMA "2" is recognized. "1" is documented in the producer as "the
    # pre-per-root shared containers" -- a real structural change in what .source-root
    # means -- so it is deliberately excluded rather than assumed compatible.
    #
    # soldr has since jumped straight to "7" (commit 75d7cc80, one bump, not five), and
    # this set has deliberately NOT been widened to follow: nothing was audited about what
    # 3..7 changed, and inventing compatibility is exactly the assumption excluding "1"
    # exists to prevent. That is safe *today* only because of the paragraph below -- soldr
    # volumes still emit only .managed and .source-root, verified against perf_local.py at
    # 75d7cc80, so this allowlist is unreachable for every object bosn actually adopts.
    # If soldr ever stamps .schema on its volumes, adoption starts refusing them with the
    # "unrecognized schema" error until someone audits 3..7 and widens this set. That
    # refusal is the intended failure mode, not a bug -- but it is the thing to look up
    # when a soldr adoption suddenly starts refusing.
    #
    # soldr's *volumes* (the only object this module ever relabels) never carry .schema --
    # only the runner *container* does, and containers are never adopted (see module
    # docstring). So an absent .schema is normal here and is treated as "no version
    # claim": the volume still has to satisfy .managed and .source-root. Requiring the
    # label outright would reject every soldr volume that exists, i.e. refuse the very
    # contract this family is here to handle. A *present* but unrecognized value still
    # refuses -- that is the signal of a real producer schema change.
    #
    # A schema-1 volume cannot slip through that relaxation, because no such labeled
    # volume exists: soldr commit 1f7066d5 ("give each checkout root its own perf_local
    # runner and volumes", #1835) is the same commit that bumped RUNNER_SCHEMA to "2",
    # and `git show 1f7066d5^:ci/perf_local.py` creates volumes as a bare
    # `docker volume create <name>` with no --label arguments at all. Volume labels and
    # schema 2 arrived together, so anything carrying .managed=true is post-bump by
    # construction; the two populations are disjoint by label presence, not by version.
    recognized_schema_versions=frozenset({"2"}),
)

ZCCACHE = LegacyFamily(
    name="zccache",
    namespace="io.zccache.perf-local",
    version="1",
    # No producer was found (see module docstring); `.workspace` is the one convention
    # assumed beyond `.managed`, mirroring bosn's own field name since there is no real
    # producer to read a workspace-bearing key from. A resource missing it fails closed.
    workspace_label="io.zccache.perf-local.workspace",
    stack_label=None,
    stack_sentinel="zccache-perf-local",
)

FAMILIES: dict[str, LegacyFamily] = {family.name: family for family in (CLUD, SOLDR, ZCCACHE)}


@dataclass(frozen=True)
class LegacyPlanEntry:
    resource: DiscoveredResource
    new_labels: labels.ResourceLabels


@dataclass(frozen=True)
class LegacyAdoptionPlan:
    family: LegacyFamily
    eligible: tuple[LegacyPlanEntry, ...] = ()
    # Managed-labeled but not a volume: containers/images cannot be safely relabeled.
    skipped_immutable: tuple[DiscoveredResource, ...] = ()
    # Managed-labeled volumes that failed schema/required-field validation.
    refused: tuple[tuple[DiscoveredResource, str], ...] = ()

    def is_empty(self) -> bool:
        return not (self.eligible or self.skipped_immutable or self.refused)


def known_families() -> list[str]:
    return sorted(FAMILIES)


def resolve_family(name: str) -> LegacyFamily:
    family = FAMILIES.get(name)
    if family is None:
        raise UnknownLegacyFamilyError(
            f"unknown --legacy family {name!r}; known families: {', '.join(known_families())}"
        )
    return family


def plan_adoption(
    scanner: ResourceScanner, family: LegacyFamily, *, registry_id: str, now: float
) -> LegacyAdoptionPlan:
    """Scan every kind for this family's managed resources and classify each one.

    Only membership in ``family`` gates inclusion here (via ``qualifies``, label-only).
    Every resource this returns already carries the family's managed label; nothing is
    matched, included, or excluded by name.
    """
    eligible: list[LegacyPlanEntry] = []
    skipped_immutable: list[DiscoveredResource] = []
    refused: list[tuple[DiscoveredResource, str]] = []
    for kind in ("volume", "container", "image"):
        for resource in scanner.discover(kind):
            if not family.qualifies(resource.raw_labels):
                continue
            if resource.kind != "volume":
                skipped_immutable.append(resource)
                continue
            try:
                new_labels = family.map_labels(
                    resource.raw_labels, registry_id=registry_id, now=now
                )
            except LegacyAdoptionError as exc:
                refused.append((resource, str(exc)))
                continue
            eligible.append(LegacyPlanEntry(resource=resource, new_labels=new_labels))
    return LegacyAdoptionPlan(
        family=family,
        eligible=tuple(eligible),
        skipped_immutable=tuple(skipped_immutable),
        refused=tuple(refused),
    )


def apply_plan(engine: Engine, plan: LegacyAdoptionPlan) -> list[str]:
    """Relabel every eligible volume in place. Registration is a separate, later step.

    Relabeling only changes engine-side ownership proof; it deliberately does not touch
    the registry database (writes to it always go through the daemon -- see
    ``resources.adopt`` / the daemon's ``compose-adopt`` verb, which the caller runs next
    so the newly-owned volumes get adoption-time aging and the quiet period like any
    other adopted resource).
    """
    from bosn.resources import recreate_volume_with_labels

    adopted: list[str] = []
    for entry in plan.eligible:
        adopted.append(recreate_volume_with_labels(engine, entry.resource, entry.new_labels))
    return adopted
