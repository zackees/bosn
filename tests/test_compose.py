import re
from pathlib import Path

import pytest

from bosn.compose import ComposeError, content_digest, load_compose, project_identity


def test_anchors_and_merge_keys_resolve() -> None:
    """#47 requires anchors/`<<:` merge keys to work; a regex parser cannot do this at all."""
    text = "services:\n  base: &base\n    image: alpine:3.20\n  app:\n    <<: *base\n"
    compose = load_compose(text)
    assert compose.services["app"].image == "alpine:3.20"


def test_profiles_parse_instead_of_raising() -> None:
    """#47's first line: `profiles:` currently causes a hard `unsupported compose key` error."""
    text = "services:\n  app:\n    image: alpine\n    profiles:\n      - debug\n      - test\n"
    compose = load_compose(text)
    assert compose.services["app"].profiles == ("debug", "test")


def test_top_level_volumes_and_networks_are_not_service_references() -> None:
    """The ghost-service bug: a service's nested references must never populate the
    top-level declaration list, and vice versa."""
    text = (
        "services:\n"
        "  app:\n"
        "    image: alpine\n"
        "    volumes:\n"
        "      - data:/data\n"
        "    networks:\n"
        "      - backend\n"
        "\n"
        "volumes:\n"
        "  data:\n"
        "\n"
        "networks:\n"
        "  backend:\n"
    )
    compose = load_compose(text)

    assert compose.volumes == ("data",)
    assert compose.networks == ("backend",)
    assert list(compose.services.keys()) == ["app"]
    assert compose.services["app"].referenced_volumes == ("data",)
    assert compose.services["app"].referenced_networks == ("backend",)


def test_unsupported_service_key_names_exact_dotted_path() -> None:
    text = "services:\n  web:\n    image: alpine\n    deploy:\n      replicas: 2\n"
    with pytest.raises(ComposeError, match=r"services\.web\.deploy"):
        load_compose(text)


def test_unsupported_top_level_key_names_exact_path() -> None:
    text = "services:\n  web:\n    image: alpine\nsecrets:\n  api_key:\n    file: ./key\n"
    with pytest.raises(ComposeError, match="secrets"):
        load_compose(text)


def test_unsupported_key_error_includes_a_remedy() -> None:
    text = "services:\n  web:\n    image: alpine\n    deploy:\n      replicas: 2\n"
    with pytest.raises(ComposeError, match="this slice supports"):
        load_compose(text)


def test_build_only_service_is_represented_not_dropped() -> None:
    """#47: build-only services are silently dropped today. This model must keep them."""
    text = "services:\n  worker:\n    build: .\n"
    compose = load_compose(text)

    assert "worker" in compose.services
    service = compose.services["worker"]
    assert service.has_build is True
    assert service.image is None
    assert service.is_build_only is True
    # image_pairs() is what compose_to_manifest's existing (service, image) extraction
    # needs; a build-only service correctly contributes nothing there.
    assert compose.image_pairs() == []


def test_malformed_yaml_raises_compose_error_not_a_traceback() -> None:
    text = "services:\n  app:\n    image: alpine\n  bad indent\n"
    with pytest.raises(ComposeError, match="malformed"):
        load_compose(text)


def test_empty_compose_file_raises_clear_error() -> None:
    with pytest.raises(ComposeError):
        load_compose("")


def test_compose_file_with_no_services_raises() -> None:
    with pytest.raises(ComposeError, match="services"):
        load_compose("volumes:\n  data:\n")


def test_load_compose_accepts_a_path(tmp_path: Path) -> None:
    compose_path = tmp_path / "compose.yaml"
    compose_path.write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")

    compose = load_compose(compose_path)

    assert compose.image_pairs() == [("app", "alpine")]


def test_image_pairs_matches_compose_to_manifest_extraction() -> None:
    text = "services:\n  web:\n    image: nginx:1.27\n  api:\n    image: myorg/api:latest\n"
    compose = load_compose(text)

    assert compose.image_pairs() == [("web", "nginx:1.27"), ("api", "myorg/api:latest")]


def test_realistic_multi_service_file_parses_end_to_end() -> None:
    """Anchors, profiles, dependency ordering, healthcheck, and both mount styles together."""
    text = (
        "services:\n"
        "  db: &db-defaults\n"
        "    image: postgres:16\n"
        "    volumes:\n"
        "      - dbdata:/var/lib/postgresql/data\n"
        "    networks:\n"
        "      - backend\n"
        "    healthcheck:\n"
        '      test: ["CMD", "pg_isready"]\n'
        "      interval: 5s\n"
        "\n"
        "  app:\n"
        "    image: myorg/app:latest\n"
        "    profiles:\n"
        "      - default\n"
        "    depends_on:\n"
        "      db:\n"
        "        condition: service_healthy\n"
        "    environment:\n"
        "      - DATABASE_URL=postgres://db/app\n"
        "    ports:\n"
        '      - "8080:8080"\n'
        "    volumes:\n"
        "      - ./config:/app/config\n"
        "      - cache:/app/cache\n"
        "    networks:\n"
        "      - backend\n"
        "      - frontend\n"
        "\n"
        "  worker:\n"
        "    build: ./worker\n"
        "    profiles:\n"
        "      - jobs\n"
        "\n"
        "volumes:\n"
        "  dbdata:\n"
        "  cache:\n"
        "\n"
        "networks:\n"
        "  backend:\n"
        "    internal: true\n"
        "  frontend:\n"
    )
    compose = load_compose(text)

    assert set(compose.services.keys()) == {"db", "app", "worker"}
    assert compose.volumes == ("dbdata", "cache")
    assert compose.networks == ("backend", "frontend")

    db = compose.services["db"]
    assert db.referenced_volumes == ("dbdata",)
    assert db.referenced_networks == ("backend",)

    app = compose.services["app"]
    assert app.profiles == ("default",)
    # ./config:/app/config is a bind mount (not a named top-level volume); only
    # the named volume `cache` should be picked up as a reference.
    assert app.referenced_volumes == ("cache",)
    assert app.referenced_networks == ("backend", "frontend")

    worker = compose.services["worker"]
    assert worker.is_build_only is True
    assert worker.profiles == ("jobs",)

    assert compose.image_pairs() == [("db", "postgres:16"), ("app", "myorg/app:latest")]


def test_extension_fields_are_accepted_wherever_compose_allows_them() -> None:
    """`x-` keys are Compose's extension convention, and where anchors normally live.

    Refusing them refuses the ordinary way `<<:` merge keys are written -- an anchor is
    defined under a top-level `x-common:` and merged into services -- so a fixed allowlist
    naming one known extension would reject most real files that use anchors at all.
    """
    doc = load_compose(
        "x-common: &common\n"
        "  image: alpine:3.20\n"
        "\n"
        "services:\n"
        "  app:\n"
        "    <<: *common\n"
        "    x-notes: anything\n"
        "\n"
        "volumes:\n"
        "  data:\n"
        "    x-owner: team\n"
    )

    assert doc.services["app"].image == "alpine:3.20"
    assert doc.volumes == ("data",)


# -- project_identity ---------------------------------------------------------------


def test_project_identity_is_stable_across_repeated_calls(tmp_path: Path) -> None:
    compose_path = tmp_path / "compose.yaml"
    compose_path.write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")

    assert project_identity(compose_path) == project_identity(compose_path)


def test_project_identity_differs_for_same_basename_different_directory(tmp_path: Path) -> None:
    """Compose itself would give both of these the same project name (the directory
    basename); bosn's identity must not collide the way Compose's own does."""
    first = tmp_path / "one" / "myapp"
    second = tmp_path / "two" / "myapp"
    first.mkdir(parents=True)
    second.mkdir(parents=True)
    (first / "compose.yaml").write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")
    (second / "compose.yaml").write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")

    assert project_identity(first / "compose.yaml") != project_identity(second / "compose.yaml")


def test_project_identity_same_for_same_directory_regardless_of_spelling(tmp_path: Path) -> None:
    directory = tmp_path / "proj"
    directory.mkdir()
    compose_path = directory / "compose.yaml"
    compose_path.write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")

    indirect = tmp_path / "proj" / ".." / "proj" / "compose.yaml"

    assert project_identity(compose_path) == project_identity(indirect)


def test_project_identity_sanitizes_an_uppercase_basename(tmp_path: Path) -> None:
    """The identity is meant to double as a project name/label value, so it must stay
    inside the character set Compose itself enforces on project names."""
    directory = tmp_path / "MyApp"
    directory.mkdir()
    compose_path = directory / "compose.yaml"
    compose_path.write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")

    identity = project_identity(compose_path)

    assert identity == identity.lower()
    assert re.fullmatch(r"[a-z0-9][a-z0-9_.-]*", identity)


# -- content_digest -------------------------------------------------------------------


def test_content_digest_is_stable_for_the_same_file(tmp_path: Path) -> None:
    compose_path = tmp_path / "compose.yaml"
    compose_path.write_text("services:\n  app:\n    image: alpine:3.20\n", encoding="utf-8")

    assert content_digest(compose_path) == content_digest(compose_path)


def test_content_digest_handles_an_unquoted_date_like_scalar(tmp_path: Path) -> None:
    """`yaml.safe_load` turns an unquoted `2024-06-27` into a `datetime.date`, which
    `load_compose` never rejects (it doesn't type-check leaf values) but which
    `json.dumps` cannot serialize without a `default=` fallback -- must not raise."""
    compose_path = tmp_path / "compose.yaml"
    compose_path.write_text(
        "services:\n  app:\n    image: alpine\n    labels:\n      build-date: 2024-06-27\n",
        encoding="utf-8",
    )

    assert content_digest(compose_path) == content_digest(compose_path)


def test_content_digest_ignores_reformatting(tmp_path: Path) -> None:
    original = tmp_path / "a.yaml"
    original.write_text(
        "services:\n  app:\n    image: alpine:3.20\n    networks:\n      - back\n"
        "networks:\n  back:\n",
        encoding="utf-8",
    )
    reformatted = tmp_path / "b.yaml"
    reformatted.write_text(
        "networks:\n  back:\nservices:\n  app:\n"
        '    networks: ["back"]\n'
        "    image: 'alpine:3.20'\n",
        encoding="utf-8",
    )

    assert content_digest(original) == content_digest(reformatted)


def test_content_digest_changes_with_image_tag(tmp_path: Path) -> None:
    original = tmp_path / "compose.yaml"
    original.write_text("services:\n  app:\n    image: alpine:3.20\n", encoding="utf-8")
    changed = tmp_path / "compose2.yaml"
    changed.write_text("services:\n  app:\n    image: alpine:3.21\n", encoding="utf-8")

    assert content_digest(original) != content_digest(changed)


def test_content_digest_changes_when_top_level_volume_added(tmp_path: Path) -> None:
    without = tmp_path / "without.yaml"
    without.write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")
    with_volume = tmp_path / "with.yaml"
    with_volume.write_text(
        "services:\n  app:\n    image: alpine\n    volumes:\n      - data:/data\n"
        "volumes:\n  data:\n",
        encoding="utf-8",
    )

    assert content_digest(without) != content_digest(with_volume)


def test_content_digest_changes_when_top_level_network_removed(tmp_path: Path) -> None:
    with_network = tmp_path / "with.yaml"
    with_network.write_text(
        "services:\n  app:\n    image: alpine\n    networks:\n      - back\nnetworks:\n  back:\n",
        encoding="utf-8",
    )
    without_network = tmp_path / "without.yaml"
    without_network.write_text("services:\n  app:\n    image: alpine\n", encoding="utf-8")

    assert content_digest(with_network) != content_digest(without_network)


def test_content_digest_reorders_top_level_services_without_rolling(tmp_path: Path) -> None:
    """Canonical JSON sorts mapping keys, so reordering *service declarations* -- a pure
    reformat -- must not roll the digest, even though it does change list-valued fields."""
    first = tmp_path / "first.yaml"
    first.write_text(
        "services:\n  web:\n    image: nginx\n  api:\n    image: alpine\n", encoding="utf-8"
    )
    second = tmp_path / "second.yaml"
    second.write_text(
        "services:\n  api:\n    image: alpine\n  web:\n    image: nginx\n", encoding="utf-8"
    )

    assert content_digest(first) == content_digest(second)


def test_content_digest_rolls_when_list_order_changes(tmp_path: Path) -> None:
    """Unlike mapping-key reordering, list order is part of identity (#48: "ordered ...
    digest")."""
    first = tmp_path / "first.yaml"
    first.write_text(
        'services:\n  app:\n    image: alpine\n    command: ["a", "b"]\n', encoding="utf-8"
    )
    second = tmp_path / "second.yaml"
    second.write_text(
        'services:\n  app:\n    image: alpine\n    command: ["b", "a"]\n', encoding="utf-8"
    )

    assert content_digest(first) != content_digest(second)


def test_content_digest_folds_in_build_context_file_content(tmp_path: Path) -> None:
    """Editing application source under a service's `build:` context must roll the
    generation -- otherwise a Compose-managed build silently never supersedes (#48)."""
    (tmp_path / "worker").mkdir()
    dockerfile = tmp_path / "worker" / "Dockerfile"
    dockerfile.write_text("FROM alpine\nCOPY app.py /app.py\n", encoding="utf-8")
    source = tmp_path / "worker" / "app.py"
    source.write_text("print('v1')\n", encoding="utf-8")
    compose_path = tmp_path / "compose.yaml"
    compose_path.write_text("services:\n  worker:\n    build: ./worker\n", encoding="utf-8")

    before = content_digest(compose_path)
    source.write_text("print('v2')\n", encoding="utf-8")
    after = content_digest(compose_path)

    assert before != after


def test_content_digest_build_context_folding_is_skipped_for_literal_text() -> None:
    """A `str` is treated as literal YAML text (matching `load_compose`'s own contract),
    so there is no directory to resolve a relative `build:` context against; this must not
    raise, and must simply omit build-context content from the digest."""
    text = "services:\n  worker:\n    build: ./worker\n"

    assert content_digest(text) == content_digest(text)
