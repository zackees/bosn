from bosn.paths import normalize_workspace_path


def test_windows_native_msys_wsl_and_extended_spellings_share_an_identity() -> None:
    expected = normalize_workspace_path(r"C:\Users\Me\work")
    assert normalize_workspace_path("/c/Users/Me/work") == expected
    assert normalize_workspace_path("/mnt/c/Users/Me/work") == expected
    assert normalize_workspace_path(r"\\?\C:\Users\Me\work") == expected


def test_unc_case_and_separator_spellings_share_an_identity() -> None:
    assert normalize_workspace_path(r"\\Server\Share\Work") == normalize_workspace_path(
        "//server/share/work"
    )


def test_wsl_is_rejected_with_the_transport_explanation(monkeypatch, capsys) -> None:
    import bosn.cli

    monkeypatch.setattr("bosn.paths.in_wsl", lambda: True)
    assert bosn.cli.main(["--version"]) == 1
    assert "does not support WSL" in capsys.readouterr().err
