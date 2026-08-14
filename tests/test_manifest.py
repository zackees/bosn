"""Phase 5: manifest parsing and generation digests. No Docker needed."""

from __future__ import annotations

from pathlib import Path

import pytest

from bosn import manifest as manifest_mod
from bosn.manifest import ManifestError, dockerfile_external_images, generation_digest, load

SAMPLE = """
[stack.test]
dockerfile = "docker/test.Dockerfile"
family = "rust"
default = true

[stack.test.volumes]
target    = { scope = "spec" }
chef      = { scope = "stack" }
cargo-reg = { scope = "machine" }

[stack.lint]
image = "python:3.12-slim"

[task.unit]
stack = "test"
cmd = "bash test"
"""


@pytest.fixture
def project(tmp_path: Path) -> Path:
    (tmp_path / "docker").mkdir()
    (tmp_path / "docker" / "test.Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (tmp_path / "bosn.toml").write_text(SAMPLE, encoding="utf-8")
    return tmp_path


def test_parses_stacks_tasks_and_volume_scopes(project: Path) -> None:
    manifest = load(project)
    assert sorted(manifest.stacks) == ["lint", "test"]
    test = manifest.stacks["test"]
    assert test.family == "rust"
    assert {v.name: v.scope for v in test.volumes} == {
        "target": "spec",
        "chef": "stack",
        "cargo-reg": "machine",
    }
    assert manifest.tasks["unit"].cmd == "bash test"
    assert manifest.default_stack().name == "test"


def test_find_manifest_walks_upward(project: Path) -> None:
    nested = project / "a" / "b"
    nested.mkdir(parents=True)
    assert manifest_mod.find_manifest(nested) == project / "bosn.toml"


def test_find_manifest_returns_none_when_absent(tmp_path: Path) -> None:
    assert manifest_mod.find_manifest(tmp_path) is None


def test_unknown_stack_and_task_names_are_specific_errors(project: Path) -> None:
    manifest = load(project)
    with pytest.raises(ManifestError, match="no stack named 'nope'"):
        manifest.stack("nope")
    with pytest.raises(ManifestError, match="no task named 'nope'"):
        manifest.task("nope")


def test_task_referencing_an_unknown_stack_is_rejected(tmp_path: Path) -> None:
    (tmp_path / "bosn.toml").write_text(
        '[stack.a]\nimage = "x"\n\n[task.t]\nstack = "ghost"\ncmd = "echo"\n', encoding="utf-8"
    )
    with pytest.raises(ManifestError, match="unknown stack 'ghost'"):
        load(tmp_path)


def test_stack_without_dockerfile_or_image_is_rejected(tmp_path: Path) -> None:
    (tmp_path / "bosn.toml").write_text('[stack.a]\nfamily = "x"\n', encoding="utf-8")
    with pytest.raises(ManifestError, match="`dockerfile` or `image`"):
        load(tmp_path)


def test_unknown_volume_scope_is_rejected(tmp_path: Path) -> None:
    (tmp_path / "bosn.toml").write_text(
        '[stack.a]\nimage = "x"\n\n[stack.a.volumes]\nv = { scope = "galaxy" }\n', encoding="utf-8"
    )
    with pytest.raises(ManifestError, match="unknown scope 'galaxy'"):
        load(tmp_path)


def test_two_default_stacks_is_an_error(tmp_path: Path) -> None:
    (tmp_path / "bosn.toml").write_text(
        '[stack.a]\nimage = "x"\ndefault = true\n\n[stack.b]\nimage = "y"\ndefault = true\n',
        encoding="utf-8",
    )
    with pytest.raises(ManifestError, match="more than one stack is marked default"):
        load(tmp_path).default_stack()


def test_invalid_toml_names_the_file(tmp_path: Path) -> None:
    (tmp_path / "bosn.toml").write_text("this is not = = toml", encoding="utf-8")
    with pytest.raises(ManifestError, match="not valid TOML"):
        load(tmp_path)


# -- digests ---------------------------------------------------------------


def test_digest_is_stable_across_loads(project: Path) -> None:
    assert load(project).digest("test") == load(project).digest("test")


def test_editing_the_dockerfile_rolls_the_digest(project: Path) -> None:
    before = load(project).digest("test")
    (project / "docker" / "test.Dockerfile").write_text("FROM alpine:3.20\n", encoding="utf-8")
    assert load(project).digest("test") != before


def test_editing_the_manifest_section_rolls_the_digest(project: Path) -> None:
    before = load(project).digest("test")
    (project / "bosn.toml").write_text(SAMPLE.replace('family = "rust"', 'family = "go"'), "utf-8")
    assert load(project).digest("test") != before


def test_touching_an_unrelated_file_does_not_roll_the_digest(project: Path) -> None:
    before = load(project).digest("test")
    (project / "README.md").write_text("hello", encoding="utf-8")
    assert load(project).digest("test") == before


def test_editing_a_copy_input_rolls_the_digest(project: Path) -> None:
    dockerfile = project / "docker" / "test.Dockerfile"
    dockerfile.write_text("FROM alpine\nCOPY payload /payload\n", encoding="utf-8")
    (project / "payload").write_text("one", encoding="utf-8")
    before = load(project).digest("test")

    (project / "payload").write_text("two", encoding="utf-8")

    assert load(project).digest("test") != before


def test_file_records_are_length_delimited(tmp_path: Path) -> None:
    roots = [tmp_path / "one", tmp_path / "two"]
    for root in roots:
        (root / "docker").mkdir(parents=True)
        (root / "docker" / "test.Dockerfile").write_text(
            "FROM alpine\nCOPY payload-* /payload/\n", encoding="utf-8"
        )
        (root / "bosn.toml").write_text(SAMPLE, encoding="utf-8")
    (roots[0] / "payload-a").write_bytes(b"X\0file\0payload-b\0Y")
    (roots[1] / "payload-a").write_bytes(b"X")
    (roots[1] / "payload-b").write_bytes(b"Y")

    assert load(roots[0]).digest("test") != load(roots[1]).digest("test")


def test_json_copy_with_flags_tracks_its_input(project: Path) -> None:
    dockerfile = project / "docker" / "test.Dockerfile"
    dockerfile.write_text(
        'FROM alpine\nCOPY --chown=1000:1000 ["payload file", "/payload"]\n',
        encoding="utf-8",
    )
    payload = project / "payload file"
    payload.write_text("one", encoding="utf-8")
    before = load(project).digest("test")

    payload.write_text("two", encoding="utf-8")

    assert load(project).digest("test") != before


@pytest.mark.parametrize(
    "mount",
    [
        "--mount=source=payload,target=/src",
        "--mount=source=payload,target=/src,type=bind",
    ],
)
def test_run_bind_mount_tracks_its_local_source(project: Path, mount: str) -> None:
    dockerfile = project / "docker" / "test.Dockerfile"
    dockerfile.write_text(f"FROM alpine\nRUN {mount} cat /src\n", encoding="utf-8")
    payload = project / "payload"
    payload.write_text("one", encoding="utf-8")
    before = load(project).digest("test")

    payload.write_text("two", encoding="utf-8")

    assert load(project).digest("test") != before


def test_heredoc_body_is_not_parsed_as_dockerfile_instructions(project: Path) -> None:
    (project / "docker" / "test.Dockerfile").write_text(
        "FROM alpine\nRUN <<'EOF'\nCOPY missing /x\nEOF\n", encoding="utf-8"
    )

    assert load(project).digest("test").startswith("sha256:")


def test_quoted_heredoc_text_does_not_hide_a_later_copy(project: Path) -> None:
    dockerfile = project / "docker" / "test.Dockerfile"
    dockerfile.write_text(
        'FROM alpine\nRUN echo "<<EOF"\nCOPY payload /payload\n', encoding="utf-8"
    )
    payload = project / "payload"
    payload.write_text("one", encoding="utf-8")
    before = load(project).digest("test")

    payload.write_text("two", encoding="utf-8")

    assert load(project).digest("test") != before


def test_bash_here_string_does_not_hide_a_later_copy(project: Path) -> None:
    dockerfile = project / "docker" / "test.Dockerfile"
    dockerfile.write_text(
        'FROM alpine\nSHELL ["/bin/bash", "-c"]\nRUN cat <<<EOF\nCOPY payload /payload\n',
        encoding="utf-8",
    )
    payload = project / "payload"
    payload.write_text("one", encoding="utf-8")
    before = load(project).digest("test")

    payload.write_text("two", encoding="utf-8")

    assert load(project).digest("test") != before


def test_dockerignore_excluded_copy_input_does_not_roll(project: Path) -> None:
    dockerfile = project / "docker" / "test.Dockerfile"
    dockerfile.write_text("FROM alpine\nCOPY . /work\n", encoding="utf-8")
    (project / ".dockerignore").write_text("ignored.txt\n", encoding="utf-8")
    (project / "included.txt").write_text("one", encoding="utf-8")
    (project / "ignored.txt").write_text("one", encoding="utf-8")
    before = load(project).digest("test")

    (project / "ignored.txt").write_text("two", encoding="utf-8")
    assert load(project).digest("test") == before

    (project / "included.txt").write_text("two", encoding="utf-8")
    assert load(project).digest("test") != before


def test_dockerignore_globs_are_root_relative_and_support_negation(project: Path) -> None:
    dockerfile = project / "docker" / "test.Dockerfile"
    dockerfile.write_text("FROM alpine\nCOPY . /work\n", encoding="utf-8")
    (project / ".dockerignore").write_text("*.md\n!keep.md\n", encoding="utf-8")
    (project / "ignored.md").write_text("one", encoding="utf-8")
    (project / "keep.md").write_text("one", encoding="utf-8")
    nested = project / "nested" / "included.md"
    nested.parent.mkdir()
    nested.write_text("one", encoding="utf-8")
    before = load(project).digest("test")

    (project / "ignored.md").write_text("two", encoding="utf-8")
    assert load(project).digest("test") == before

    nested.write_text("two", encoding="utf-8")
    assert load(project).digest("test") != before


def test_dockerfile_external_images_resolve_args_stages_and_copy_from(project: Path) -> None:
    dockerfile = project / "docker" / "test.Dockerfile"
    dockerfile.write_text(
        "\n".join(
            [
                "ARG BASE=alpine:3.20",
                "FROM --platform=linux/amd64 ${BASE} AS build",
                "FROM scratch",
                "COPY --from=build /local /local",
                "COPY --from=busybox:1.36 /bin/busybox /busybox",
                "RUN --mount=target=/tool,from=debian:bookworm cat /tool",
            ]
        ),
        encoding="utf-8",
    )
    manifest = load(project)

    assert dockerfile_external_images(project, manifest.stack("test")) == [
        ("alpine:3.20", "linux/amd64"),
        ("busybox:1.36", None),
        ("debian:bookworm", None),
    ]


def test_unpinned_remote_add_is_rejected(project: Path) -> None:
    (project / "docker" / "test.Dockerfile").write_text(
        "FROM alpine\nADD https://example.invalid/tool /tool\n", encoding="utf-8"
    )
    with pytest.raises(ManifestError, match="needs --checksum"):
        load(project).digest("test")


def test_git_add_accepts_only_a_full_commit_reference(project: Path) -> None:
    dockerfile = project / "docker" / "test.Dockerfile"
    dockerfile.write_text(
        "FROM alpine\nADD git@github.com:example/repo.git#main /repo\n", encoding="utf-8"
    )
    with pytest.raises(ManifestError, match="full commit"):
        load(project).digest("test")

    dockerfile.write_text(
        f"FROM alpine\nADD https://github.com/example/repo.git#{'a' * 40} /repo\n",
        encoding="utf-8",
    )
    assert load(project).digest("test").startswith("sha256:")


def test_different_stacks_have_different_digests(project: Path) -> None:
    manifest = load(project)
    assert manifest.digest("test") != manifest.digest("lint")


def test_a_missing_referenced_file_is_an_error_not_a_zero(project: Path) -> None:
    """Silently digesting an absent Dockerfile would make two different specs compare equal."""
    (project / "docker" / "test.Dockerfile").unlink()
    manifest = load(project)
    with pytest.raises(ManifestError, match="does not exist"):
        generation_digest(manifest, manifest.stacks["test"])
