import os
from pathlib import Path

from bosn.paths import normalize_workspace_path


def test_windows_native_msys_wsl_and_extended_spellings_share_an_identity() -> None:
    expected = normalize_workspace_path(r"C:\Users\Me\work")
    assert normalize_workspace_path("/c/Users/Me/work") == expected
    assert normalize_workspace_path("/mnt/c/Users/Me/work") == expected
    assert normalize_workspace_path(r"\\?\C:\Users\Me\work") == expected


def test_cygdrive_spelling_shares_the_same_identity() -> None:
    # Cygwin is not one of the supported shells (see the module docstring), but it is
    # accepted defensively so a stray /cygdrive/ spelling roots at the real drive instead of
    # a bogus c:\cygdrive\c\... identity that would silently split the workspace in two.
    expected = normalize_workspace_path(r"C:\Users\Me\work")
    assert normalize_workspace_path("/cygdrive/c/Users/Me/work") == expected


def test_unc_case_and_separator_spellings_share_an_identity() -> None:
    assert normalize_workspace_path(r"\\Server\Share\Work") == normalize_workspace_path(
        "//server/share/work"
    )


def test_wsl_is_rejected_with_the_transport_explanation(monkeypatch, capsys) -> None:
    import bosn.cli

    monkeypatch.setattr("bosn.paths.in_wsl", lambda: True)
    assert bosn.cli.main(["--version"]) == 1
    assert "does not support WSL" in capsys.readouterr().err


# -- shell spellings must be interchangeable, not just comparable ------------


def test_to_host_path_accepts_every_supported_shells_spelling(tmp_path) -> None:
    """`normalize_workspace_path` answers "same workspace?"; this answers "what do I open?".

    The identity function normcases, so its output is not a usable path. A bind mount
    source and a manifest location are handed to the filesystem and the engine, so they
    need the real spelling with case intact.
    """
    from bosn.paths import to_host_path

    native = str(tmp_path.resolve())
    if os.name != "nt":
        assert to_host_path(native) == Path(native)
        return

    drive = native[0].lower()
    tail = native[2:].replace("\\", "/").lstrip("/")
    for spelling in (
        native,
        f"/{drive}/{tail}",
        f"/mnt/{drive}/{tail}",
        f"/cygdrive/{drive}/{tail}",
    ):
        assert to_host_path(spelling).resolve() == tmp_path.resolve(), spelling

    # Case is preserved -- unlike the identity function, which lowercases.
    assert str(to_host_path(native)) == native


def test_to_host_path_leaves_a_real_posix_path_alone() -> None:
    """`/c/x` is a legitimate directory on Linux; rewriting it would invent `C:/x`."""
    from bosn.paths import to_host_path

    result = to_host_path("/c/projects/thing")
    if os.name == "nt":
        assert result == Path("C:/projects/thing")
    else:
        assert result == Path("/c/projects/thing")
