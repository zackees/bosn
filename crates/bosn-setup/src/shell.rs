//! The one shell shape every declared setup command runs under.
//!
//! A declared command runs in a login shell (`sh -l`), so `/etc/profile`,
//! `/etc/profile.d/*`, and `~/.profile` apply exactly as they would for an
//! interactive user. On Debian- and Alpine-based images that profile assigns
//! `PATH` outright, which silently drops every `ENV PATH` entry the image
//! declared (`/usr/local/cargo/bin` in `rust:*`, `/opt/<tool>/bin`, ...).
//!
//! The container's own process environment still holds the image's `PATH`
//! before any profile runs, so a non-login launcher shell records it, then
//! `exec`s the login shell, and the declared script re-prepends it after the
//! profile: image entries come first, followed by whatever the profile set.
//! The declared command text itself is unchanged and never quoted or parsed.

/// Non-login launcher: records the pre-profile `PATH` in `BOSN_IMAGE_PATH`;
/// `$1` is the full login-shell script.
const LAUNCHER: &str = r#"BOSN_IMAGE_PATH="$PATH"; export BOSN_IMAGE_PATH; exec sh -lc "$1""#;

/// First line of the login-shell script, run after the profile. An empty image
/// `PATH` adds nothing (never an empty entry, which would mean the cwd).
const RESTORE_IMAGE_PATH: &str =
    r#"PATH="${BOSN_IMAGE_PATH:+$BOSN_IMAGE_PATH:}$PATH"; export PATH; unset BOSN_IMAGE_PATH"#;

/// The exact argv tail (after the image or container name) for one declared
/// command: `sh -c LAUNCHER sh SCRIPT`, where the launcher runs
/// `sh -lc SCRIPT` and `SCRIPT` is the `PATH` restore line followed by the
/// declared command.
pub fn login_shell_args(command: &str) -> [String; 5] {
    [
        "sh".into(),
        "-c".into(),
        LAUNCHER.into(),
        "sh".into(),
        format!("{RESTORE_IMAGE_PATH}\n{command}"),
    ]
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Run the production argv with the host's `/bin/sh` as a login shell
    /// whose profile clobbers `PATH`, as Debian's and Alpine's do.
    fn run_under_clobbering_profile(initial_path: &str, command: &str) -> String {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".profile"),
            "PATH=/profile/bin:/usr/bin:/bin\nexport PATH\n",
        )
        .unwrap();
        let args = login_shell_args(command);
        let output = std::process::Command::new("/bin/sh")
            .args(&args[1..])
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", initial_path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn host_shell_dirs() -> String {
        // `sh` must stay resolvable for the launcher's `exec sh -lc`.
        let sh = std::fs::canonicalize("/bin/sh").unwrap();
        sh.parent().unwrap().display().to_string()
    }

    #[test]
    fn image_env_path_survives_a_profile_that_assigns_path() {
        let image_path = format!("/image/cargo/bin:/opt/tool/bin:{}", host_shell_dirs());
        let path = run_under_clobbering_profile(&image_path, r#"printf '%s' "$PATH""#);
        assert!(
            path.starts_with(&format!("{image_path}:")),
            "image PATH must lead: {path}"
        );
        assert!(
            path.contains("/profile/bin"),
            "profile PATH must be kept: {path}"
        );
        assert!(!path.contains("::"), "no empty entry: {path}");
    }

    #[test]
    fn the_carrier_variable_does_not_leak_into_the_declared_command() {
        let out = run_under_clobbering_profile(
            &host_shell_dirs(),
            r#"printf '%s' "${BOSN_IMAGE_PATH-unset}""#,
        );
        assert_eq!(out, "unset");
    }

    #[test]
    fn declared_command_text_runs_verbatim_with_login_semantics() {
        // Multi-line scripts, quotes, and `$0` behave as under plain `sh -lc`.
        let out = run_under_clobbering_profile(
            &host_shell_dirs(),
            "set -e\ncase x in x) printf 'a\"b' ;; esac\nprintf ' %s' \"$0\"",
        );
        assert_eq!(out, "a\"b sh");
    }

    #[test]
    fn argv_shape_is_fixed() {
        let args = login_shell_args("cargo test --locked");
        assert_eq!(&args[..2], ["sh", "-c"]);
        assert!(args[2].contains("exec sh -lc \"$1\""));
        assert_eq!(args[3], "sh");
        assert!(args[4].ends_with("\ncargo test --locked"));
        assert!(args[4].starts_with("PATH=\"${BOSN_IMAGE_PATH:+"));
    }
}
