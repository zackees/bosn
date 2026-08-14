from pathlib import Path

import pytest

from bosn.compose import ComposeError, load_compose


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
