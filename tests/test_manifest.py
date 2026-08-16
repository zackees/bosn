"""Phase 5: manifest parsing and generation digests. No Docker needed."""

from __future__ import annotations

from pathlib import Path

import pytest

from bosn import manifest as manifest_mod
from bosn.manifest import (
    ManifestError,
    StackSpec,
    dockerfile_external_images,
    generation_digest,
    load,
)

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


# -- bind mounts and explicit volume destinations (#76) ----------------------


BIND_SAMPLE = """
[stack.test]
dockerfile = "docker/test.Dockerfile"
default = true

[stack.test.volumes]
target = { scope = "stack", destination = "/target" }
cache  = { scope = "machine" }

[stack.test.mounts]
repo = { source = ".", destination = "/repo" }
conf = { source = "docker", destination = "/etc/app", readonly = true }
"""


def _write(root: Path, body: str) -> Path:
    (root / "docker").mkdir(exist_ok=True)
    (root / "docker" / "test.Dockerfile").write_text("FROM alpine\n", encoding="utf-8")
    (root / "bosn.toml").write_text(body, encoding="utf-8")
    return root


def test_mounts_and_explicit_volume_destinations_parse(tmp_path: Path) -> None:
    stack = load(_write(tmp_path, BIND_SAMPLE)).stacks["test"]

    assert {v.name: v.mount_at() for v in stack.volumes} == {
        "target": "/target",  # explicit destination honored
        "cache": "/bosn/cache",  # default when omitted
    }
    assert [(m.name, m.destination, m.readonly) for m in stack.mounts] == [
        ("repo", "/repo", False),
        ("conf", "/etc/app", True),
    ]
    assert stack.mounts[0].resolve_source(tmp_path) == tmp_path.resolve()


def test_a_missing_bind_source_is_an_error_not_an_empty_directory(tmp_path: Path) -> None:
    """Docker would create the source silently; an empty /repo fails much later."""
    body = BIND_SAMPLE.replace('source = "."', 'source = "does-not-exist"')
    stack = load(_write(tmp_path, body)).stacks["test"]

    with pytest.raises(ManifestError, match="does not exist"):
        stack.mounts[0].resolve_source(tmp_path)


@pytest.mark.parametrize(
    "destination",
    ["/bosn/target", "/bosn-daemon/heartbeat"],
)
def test_a_mount_inside_bosns_own_namespace_is_refused(tmp_path: Path, destination: str) -> None:
    """Shadowing a managed volume or the heartbeat the container's PID 1 watches."""
    body = BIND_SAMPLE.replace('destination = "/repo"', f'destination = "{destination}"')

    with pytest.raises(ManifestError, match="reserved"):
        load(_write(tmp_path, body))


def test_a_relative_destination_is_refused(tmp_path: Path) -> None:
    body = BIND_SAMPLE.replace('destination = "/repo"', 'destination = "repo"')

    with pytest.raises(ManifestError, match="absolute"):
        load(_write(tmp_path, body))


def test_two_mounts_at_one_destination_are_refused(tmp_path: Path) -> None:
    """Docker takes the last one silently, leaving the other mysteriously absent."""
    body = BIND_SAMPLE.replace('destination = "/etc/app"', 'destination = "/repo"')

    with pytest.raises(ManifestError, match="twice"):
        load(_write(tmp_path, body))


def test_a_bind_colliding_with_a_volume_destination_is_refused(tmp_path: Path) -> None:
    body = BIND_SAMPLE.replace('destination = "/repo"', 'destination = "/target"')

    with pytest.raises(ManifestError, match="twice"):
        load(_write(tmp_path, body))


def test_moving_a_mount_rolls_the_generation(tmp_path: Path) -> None:
    """Where something is mounted is part of the stack's identity."""
    before = load(_write(tmp_path, BIND_SAMPLE))
    after = load(_write(tmp_path, BIND_SAMPLE.replace('"/repo"', '"/src"')))

    assert generation_digest(before, before.stacks["test"]) != generation_digest(
        after, after.stacks["test"]
    )


def test_the_digest_splits_copied_files_from_bind_source_contents(tmp_path: Path) -> None:
    """The deliberate narrowing: a bind exists to keep its contents OUT of identity.

    Proven both ways against one Dockerfile that COPYs a single file. Editing the copied
    file rolls the generation; editing a sibling visible only through the bind does not.
    Without that second half a live working tree would rebuild on every keystroke, which
    is the reason for bind-mounting it in the first place.
    """
    root = _write(tmp_path, BIND_SAMPLE)
    (root / "docker" / "test.Dockerfile").write_text(
        "FROM alpine\nCOPY copied.py /copied.py\n", encoding="utf-8"
    )
    (root / "copied.py").write_text("print('one')\n", encoding="utf-8")
    (root / "only_bind_mounted.py").write_text("print('one')\n", encoding="utf-8")

    def digest() -> str:
        loaded = load(root)
        return generation_digest(loaded, loaded.stacks["test"])

    baseline = digest()

    (root / "only_bind_mounted.py").write_text("print('edited')\n", encoding="utf-8")
    assert digest() == baseline, "a bind source's contents must not roll the generation"

    (root / "copied.py").write_text("print('edited')\n", encoding="utf-8")
    assert digest() != baseline, "a COPYed file must roll the generation"


def test_a_git_bash_spelled_bind_source_resolves(tmp_path: Path) -> None:
    r"""A manifest written in one shell must work when read in another.

    Argv from Git Bash is usually MSYS-converted to a native path before bosn sees it, but
    a string inside bosn.toml never passes through that conversion. `/c/work/repo` would
    otherwise be joined into `C:\c\work\repo` and rejected as missing.
    """
    import os

    root = _write(tmp_path, BIND_SAMPLE)
    (root / "src").mkdir()
    native = str((root / "src").resolve())
    if os.name != "nt":
        pytest.skip("drive-letter spellings only differ on Windows")

    drive = native[0].lower()
    tail = native[2:].replace("\\", "/").lstrip("/")
    body = BIND_SAMPLE.replace('source = "."', f'source = "/{drive}/{tail}"')

    stack = load(_write(tmp_path, body)).stacks["test"]

    assert stack.mounts[0].resolve_source(root) == (root / "src").resolve()


# -- examples/soldr.toml expresses soldr's actual perf-local mount table (#75) ----------
#
# `Runner.volumes`/`create_command()` in soldr's `ci/perf_local.py` mount six paths: one
# bind (the checkout itself, at `/repo`) and five named volumes (`target`, `cargo_home`,
# `soldr_home`, `uv_cache`, `venv`). These tests load the checked-in example through the
# real loader -- the same `load()` a `bosn ensure`/`bosn run` invocation uses -- and pin
# every destination and scope this deliverable claims to soldr's real ones, so a drift
# between the example and soldr's actual harness fails a test instead of going unnoticed.

EXAMPLES_ROOT = Path(__file__).resolve().parent.parent / "examples"

# Exactly soldr's six container-side paths, read from `create_command()`'s `-v` flags in
# ci/perf_local.py (bind: /repo; volumes: /root/.cargo, /root/.soldr, /root/.cache/uv,
# /venv, /target).
SOLDR_MOUNT_DESTINATIONS = {
    "repo": "/repo",
    "cargo-home": "/root/.cargo",
    "soldr-home": "/root/.soldr",
    "uv-cache": "/root/.cache/uv",
    "target": "/target",
    "venv": "/venv",
}


def _soldr_stack() -> StackSpec:
    return load(EXAMPLES_ROOT / "soldr.toml").stack("perf")


def test_soldr_example_mounts_all_six_soldr_paths_at_soldr_destinations() -> None:
    stack = _soldr_stack()

    by_destination = {v.name: v.mount_at() for v in stack.volumes}
    by_destination.update({m.name: m.destination for m in stack.mounts})

    assert by_destination == SOLDR_MOUNT_DESTINATIONS


def test_soldr_example_repo_is_a_bind_never_a_volume() -> None:
    """bosn *references* the checkout; it must never own, label, or delete it (MountSpec).

    Proven both directions: `repo` is absent from `volumes` (so it is never registered,
    labeled, or GC'd as bosn's own), and present in `mounts` (so it is a bind).
    """
    stack = _soldr_stack()

    assert "repo" not in {v.name for v in stack.volumes}
    assert {m.name for m in stack.mounts} == {"repo"}
    mount = stack.mounts[0]
    assert mount.source == "."
    assert mount.destination == "/repo"


def test_soldr_example_scopes_match_the_readme_sharing_claim() -> None:
    """README.md ("How the disk savings actually work" -> "1. Sharing") claims applying
    bosn's scopes to soldr's own five volumes drops five per-checkout volumes to two,
    with cargo-home/soldr-home/uv-cache machine-scoped and target/venv stack-scoped. This
    pins the claim against the checked-in example so an edit to either one that breaks the
    correspondence fails a test rather than silently diverging.
    """
    stack = _soldr_stack()
    scopes = {v.name: v.scope for v in stack.volumes}

    assert scopes == {
        "cargo-home": "machine",
        "soldr-home": "machine",
        "uv-cache": "machine",
        "target": "stack",
        "venv": "stack",
    }
    machine_scoped = {name for name, scope in scopes.items() if scope == "machine"}
    stack_scoped = {name for name, scope in scopes.items() if scope == "stack"}
    assert len(machine_scoped) == 3
    assert len(stack_scoped) == 2


def test_soldr_example_destinations_outside_bosns_namespace_are_all_accepted() -> None:
    """None of soldr's real destinations collide with /bosn/* or the heartbeat file --
    loading the example at all is already partial proof of this, but the point of this
    test is to pin it against the exact set rather than "it didn't raise."""
    stack = _soldr_stack()
    destinations = {v.mount_at() for v in stack.volumes} | {m.destination for m in stack.mounts}

    for destination in destinations:
        assert not destination.startswith(manifest_mod.RESERVED_PREFIX)
        assert destination != manifest_mod.RESERVED_HEARTBEAT

    assert destinations == set(SOLDR_MOUNT_DESTINATIONS.values())


def test_soldr_example_reserved_namespace_guard_still_rejects_a_bosn_destination(
    tmp_path: Path,
) -> None:
    """The guard soldr's manifest relies on staying silent is still live: redirect one of
    its own volumes into bosn's reserved namespace and confirm the loader still refuses.
    """
    body = (
        (EXAMPLES_ROOT / "soldr.toml")
        .read_text(encoding="utf-8")
        .replace('"/root/.cargo"', '"/bosn/cargo-home"')
    )
    (tmp_path / "bosn.toml").write_text(body, encoding="utf-8")

    with pytest.raises(ManifestError, match="reserved"):
        load(tmp_path)
