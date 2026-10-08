//! Registering the daemon with the platform's user service manager.
//!
//! #147's second precondition was that something must be running to do the work, and on the
//! reference machine nothing was: `~/.local/state/bosn` did not exist and bosn had never run.
//! Default registration preserves an explicit user opt-out across later starts.
//!
//! The original defect it replaces was precise: the Python implementation wrote the
//! LaunchAgent plist and returned, never registering it with `launchd`. Dropping a file into
//! `~/Library/LaunchAgents` does not make launchd run it, so a macOS user who explicitly
//! asked for autostart got no daemon until their next login. Writing the file is therefore
//! *not* the operation; registering it is. Every mutating step here goes through
//! [`CommandRunner`], so the argv a platform would receive is testable without requiring the
//! platform in CI.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Unit name shared by both platforms' generated entries.
pub const SERVICE_NAME: &str = "com.zackees.bosn";

/// Runs a bounded, trusted argv.
///
/// This is deliberately not a general shell: callers do not interpolate user input into a
/// command line, and the platform argv is fixed by this module.
pub trait CommandRunner {
    /// # Errors
    /// Reports a failure to launch, or a non-zero exit.
    fn run(&self, argv: &[OsString]) -> Result<(), String>;
}

/// Runs commands as child processes.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, argv: &[OsString]) -> Result<(), String> {
        let Some((program, rest)) = argv.split_first() else {
            return Err("empty command".to_owned());
        };
        let status = run_bounded(
            std::process::Command::new(program).args(rest),
            std::time::Duration::from_secs(15),
        )
        .map_err(|error| format!("{}: {error}", program.to_string_lossy()))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "{} exited with {}",
                program.to_string_lossy(),
                status.code().unwrap_or(-1)
            ))
        }
    }
}

fn run_bounded(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Result<std::process::ExitStatus, String> {
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => {
                // Reap the child even when polling fails; registration must not
                // leave a hung service-manager command behind.
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
                    Err(error) => error.to_string(),
                    _ => "service-manager command timed out".to_owned(),
                });
            }
        }
    }
}

/// The platform whose service manager will own the daemon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    /// A user `systemd` unit.
    LinuxSystemd,
    /// A per-user `launchd` agent.
    MacosLaunchd,
}

impl Platform {
    /// The platform this build is running on, if autostart is supported here.
    #[must_use]
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "linux") {
            Some(Self::LinuxSystemd)
        } else if cfg!(target_os = "macos") {
            Some(Self::MacosLaunchd)
        } else {
            None
        }
    }
}

/// What one enable, disable, or status call found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutostartStatus {
    pub platform: Platform,
    /// The generated entry file.
    pub path: PathBuf,
    /// Whether the entry file exists.
    pub written: bool,
    /// Whether a service-manager query confirms registration. False also covers an unavailable manager.
    pub registered: bool,
}

/// The service-manager command that registers or unregisters an entry.
///
/// Kept separate from execution so the exact argv is asserted in tests. `enable` and
/// `disable` are deliberately symmetric: leaving an unloaded file behind, or an unlinked
/// registration behind, is the defect this module exists to fix.
#[must_use]
pub fn registration_argv(platform: Platform, enable: bool, path: &Path) -> Vec<OsString> {
    match platform {
        Platform::LinuxSystemd => {
            let action = if enable { "enable" } else { "disable" };
            argv(&["systemctl", "--user", action, "--now", SERVICE_NAME])
        }
        Platform::MacosLaunchd => {
            let action = if enable { "load" } else { "unload" };
            argv_os(&["launchctl", action, "-w"], path.as_os_str().to_owned())
        }
    }
}

/// The service-manager command that makes it re-read unit files, if the platform has one.
///
/// It must run *after* the file changes, otherwise the manager keeps serving the previous
/// state of the world.
#[must_use]
pub fn reload_argv(platform: Platform) -> Option<Vec<OsString>> {
    match platform {
        Platform::LinuxSystemd => Some(argv(&["systemctl", "--user", "daemon-reload"])),
        Platform::MacosLaunchd => None,
    }
}

/// The generated unit file for a platform.
#[must_use]
pub fn unit_contents(platform: Platform, binary: &Path, state_dir: &Path) -> String {
    let encode = |path: &Path| match platform {
        Platform::LinuxSystemd => systemd_argument(&path.to_string_lossy()),
        Platform::MacosLaunchd => xml_text(&path.to_string_lossy()),
    };
    let binary = encode(binary);
    let state_dir = encode(state_dir);
    match platform {
        Platform::LinuxSystemd => format!(
            "[Unit]\n\
             Description=Bosn Docker lifecycle daemon\n\
             After=network.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={binary} daemon serve --state-dir {state_dir}\n\
             Restart=on-failure\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        ),
        Platform::MacosLaunchd => format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \x20 <key>Label</key><string>{SERVICE_NAME}</string>\n\
             \x20 <key>ProgramArguments</key>\n\
             \x20 <array>\n\
             \x20   <string>{binary}</string>\n\
             \x20   <string>daemon</string>\n\
             \x20   <string>serve</string>\n\
             \x20   <string>--state-dir</string>\n\
             \x20   <string>{state_dir}</string>\n\
             \x20 </array>\n\
             \x20 <key>RunAtLoad</key><true/>\n\
             \x20 <key>KeepAlive</key>\n\
             \x20 <dict><key>SuccessfulExit</key><false/></dict>\n\
             </dict>\n\
             </plist>\n"
        ),
    }
}

fn systemd_argument(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Where the entry file lives for a user.
#[must_use]
pub fn unit_path(platform: Platform, home: &Path) -> PathBuf {
    match platform {
        Platform::LinuxSystemd => home
            .join(".config/systemd/user")
            .join(format!("{SERVICE_NAME}.service")),
        Platform::MacosLaunchd => home
            .join("Library/LaunchAgents")
            .join(format!("{SERVICE_NAME}.plist")),
    }
}

/// Write the entry and register it, in that order.
///
/// # Errors
/// Reports a filesystem failure or a service-manager refusal. A refusal after the file is
/// written leaves the file in place and says so, rather than pretending the pair succeeded.
pub fn enable(
    runner: &dyn CommandRunner,
    platform: Platform,
    home: &Path,
    binary: &Path,
    state_dir: &Path,
) -> Result<AutostartStatus, String> {
    let path = unit_path(platform, home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&path, unit_contents(platform, binary, state_dir))
        .map_err(|error| error.to_string())?;
    // Reload first so the manager can see the file, then register it.
    if let Some(reload) = reload_argv(platform) {
        runner.run(&reload)?;
    }
    runner.run(&registration_argv(platform, true, &path))?;
    let disabled = disabled_path(platform, home);
    if disabled.exists() {
        std::fs::remove_file(disabled).map_err(|error| error.to_string())?;
    }
    Ok(AutostartStatus {
        platform,
        path,
        written: true,
        registered: true,
    })
}

/// Unregister the entry, then remove it.
///
/// # Errors
/// Reports a service-manager refusal. The file is only removed after the manager has let go
/// of it, so a failure leaves a loadable unit rather than an orphaned registration.
pub fn disable(
    runner: &dyn CommandRunner,
    platform: Platform,
    home: &Path,
) -> Result<AutostartStatus, String> {
    let path = unit_path(platform, home);
    let disabled = disabled_path(platform, home);
    if let Some(parent) = disabled.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    // Persist the user's choice even if registration is absent or the
    // service manager is unavailable. A later automatic start must honor it.
    std::fs::write(disabled, b"disabled\n").map_err(|error| error.to_string())?;
    if !path.exists() {
        return Ok(AutostartStatus {
            platform,
            path,
            written: false,
            registered: false,
        });
    }
    runner.run(&registration_argv(platform, false, &path))?;
    std::fs::remove_file(&path).map_err(|error| error.to_string())?;
    // Reload last, so the manager forgets the unit rather than holding an unlinked one.
    if let Some(reload) = reload_argv(platform) {
        runner.run(&reload)?;
    }
    Ok(AutostartStatus {
        platform,
        path,
        written: false,
        registered: false,
    })
}

fn disabled_path(platform: Platform, home: &Path) -> PathBuf {
    unit_path(platform, home).with_extension("disabled")
}

/// Register maintenance unless the user explicitly disabled autostart.
///
/// Returns `None` for an opt-out. Registration failures remain visible to the
/// caller so a detached daemon cannot be mistaken for persistent maintenance.
///
/// # Errors
/// Reports filesystem or service-manager failures.
pub fn ensure_default(
    runner: &dyn CommandRunner,
    platform: Platform,
    home: &Path,
    binary: &Path,
    state_dir: &Path,
) -> Result<Option<AutostartStatus>, String> {
    if disabled_path(platform, home).exists() {
        return Ok(None);
    }
    let path = unit_path(platform, home);
    if platform == Platform::MacosLaunchd
        && std::fs::read_to_string(&path).ok().as_deref()
            == Some(unit_contents(platform, binary, state_dir).as_str())
        && runner
            .run(&argv(&["launchctl", "list", SERVICE_NAME]))
            .is_ok()
    {
        return Ok(Some(AutostartStatus {
            platform,
            path,
            written: true,
            registered: true,
        }));
    }
    enable(runner, platform, home, binary, state_dir).map(Some)
}

/// Report the unit file and service-manager-confirmed registration without mutation.
#[must_use]
pub fn status(platform: Platform, home: &Path) -> AutostartStatus {
    status_with_runner(&SystemRunner, platform, home)
}

fn status_with_runner(
    runner: &dyn CommandRunner,
    platform: Platform,
    home: &Path,
) -> AutostartStatus {
    let path = unit_path(platform, home);
    let written = path.is_file();
    let query = match platform {
        Platform::LinuxSystemd => {
            argv(&["systemctl", "--user", "is-enabled", "--quiet", SERVICE_NAME])
        }
        Platform::MacosLaunchd => argv(&["launchctl", "list", SERVICE_NAME]),
    };
    AutostartStatus {
        platform,
        path,
        written,
        registered: runner.run(&query).is_ok(),
    }
}

fn argv(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

fn argv_os(prefix: &[&str], tail: OsString) -> Vec<OsString> {
    prefix
        .iter()
        .map(OsString::from)
        .chain(std::iter::once(tail))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn generated_entries_escape_paths_for_their_platform_parser() {
        assert_eq!(
            super::systemd_argument("/a b/%x/$HOME/\"c\\d"),
            "\"/a b/%%x/$$HOME/\\\"c\\\\d\""
        );
        assert_eq!(super::xml_text("/a&b/<c>"), "/a&amp;b/&lt;c&gt;");
        let unit = super::unit_contents(
            super::Platform::LinuxSystemd,
            std::path::Path::new("/a b/bosn"),
            std::path::Path::new("/state dir"),
        );
        assert!(unit.contains("ExecStart=\"/a b/bosn\" daemon serve --state-dir \"/state dir\""));
    }

    #[test]
    #[cfg(unix)]
    fn service_manager_timeout_terminates_child() {
        let started = std::time::Instant::now();
        let result = super::run_bounded(
            std::process::Command::new("/bin/sleep").arg("5"),
            std::time::Duration::from_millis(20),
        );
        assert_eq!(result.unwrap_err(), "service-manager command timed out");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<Vec<OsString>>>,
        fail_on: Option<usize>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, argv: &[OsString]) -> Result<(), String> {
            let mut calls = self.calls.lock().expect("lock");
            calls.push(argv.to_vec());
            if self.fail_on == Some(calls.len() - 1) {
                return Err("refused".to_owned());
            }
            Ok(())
        }
    }

    fn home() -> kernal_api::platform::fs::TemporaryDirectory {
        kernal_api::platform::fs::TemporaryDirectory::new().expect("temporary directory")
    }

    #[test]
    fn default_registration_preserves_opt_out_until_explicit_enable() {
        let home = home();
        let runner = FakeRunner::default();
        let platform = Platform::LinuxSystemd;
        let binary = Path::new("/usr/bin/bosn");
        let state = Path::new("/state");
        assert!(
            ensure_default(&runner, platform, home.path(), binary, state)
                .expect("default registration")
                .is_some()
        );
        disable(&runner, platform, home.path()).expect("disable");
        let calls = runner.calls.lock().expect("lock").len();
        assert!(
            ensure_default(&runner, platform, home.path(), binary, state)
                .expect("honor opt-out")
                .is_none()
        );
        assert_eq!(runner.calls.lock().expect("lock").len(), calls);
        enable(&runner, platform, home.path(), binary, state).expect("explicit enable");
        assert!(!disabled_path(platform, home.path()).exists());
    }

    #[test]
    fn disabling_before_first_registration_preserves_opt_out() {
        let home = home();
        let runner = FakeRunner::default();
        disable(&runner, Platform::MacosLaunchd, home.path()).expect("disable absent unit");
        assert!(
            ensure_default(
                &runner,
                Platform::MacosLaunchd,
                home.path(),
                Path::new("/bosn"),
                Path::new("/state"),
            )
            .expect("opt-out")
            .is_none()
        );
        assert!(runner.calls.lock().expect("lock").is_empty());
    }

    #[test]
    fn a_unit_file_does_not_prove_service_manager_registration() {
        for platform in [Platform::LinuxSystemd, Platform::MacosLaunchd] {
            let home = home();
            let path = unit_path(platform, home.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                unit_contents(platform, Path::new("/bosn"), Path::new("/state")),
            )
            .unwrap();
            let unavailable = FakeRunner {
                fail_on: Some(0),
                ..Default::default()
            };
            let observed = status_with_runner(&unavailable, platform, home.path());
            assert!(observed.written);
            assert!(!observed.registered);
            let available = FakeRunner::default();
            assert!(status_with_runner(&available, platform, home.path()).registered);
            assert_eq!(
                *unavailable.calls.lock().unwrap(),
                *available.calls.lock().unwrap()
            );
        }
    }

    #[test]
    fn repeated_macos_registration_checks_the_manager_without_loading_again() {
        let home = home();
        let runner = FakeRunner::default();
        for _ in 0..2 {
            ensure_default(
                &runner,
                Platform::MacosLaunchd,
                home.path(),
                Path::new("/bosn"),
                Path::new("/state"),
            )
            .unwrap();
        }
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][1], "load");
        assert_eq!(calls[1], argv(&["launchctl", "list", SERVICE_NAME]));
    }

    #[test]
    fn unloaded_macos_file_retries_registration() {
        let home = home();
        let path = unit_path(Platform::MacosLaunchd, home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            unit_contents(
                Platform::MacosLaunchd,
                Path::new("/bosn"),
                Path::new("/state"),
            ),
        )
        .unwrap();
        let runner = FakeRunner {
            fail_on: Some(0),
            ..FakeRunner::default()
        };
        ensure_default(
            &runner,
            Platform::MacosLaunchd,
            home.path(),
            Path::new("/bosn"),
            Path::new("/state"),
        )
        .unwrap();
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][1], "list");
        assert_eq!(calls[1][1], "load");
    }

    #[test]
    fn linux_enable_writes_the_unit_and_registers_it() {
        let home = home();
        let runner = FakeRunner::default();
        let status = enable(
            &runner,
            Platform::LinuxSystemd,
            home.path(),
            Path::new("/usr/bin/bosn"),
            Path::new("/home/u/.local/state/bosn"),
        )
        .expect("enable");
        assert!(status.written && status.registered);
        assert!(status.path.exists());
        let contents = std::fs::read_to_string(&status.path).expect("read unit");
        assert!(contents.contains("ExecStart=\"/usr/bin/bosn\" daemon serve --state-dir"));
        assert!(contents.contains("WantedBy=default.target"));
        // The defect this replaces: writing the file without ever invoking the manager.
        let calls = runner.calls.lock().expect("lock").clone();
        assert_eq!(calls.len(), 2, "a reload and an enable are both required");
        assert_eq!(calls[0], argv(&["systemctl", "--user", "daemon-reload"]));
        assert_eq!(
            calls[1],
            argv(&["systemctl", "--user", "enable", "--now", SERVICE_NAME])
        );
    }

    #[test]
    fn linux_disable_unregisters_before_removing() {
        let home = home();
        let runner = FakeRunner::default();
        enable(
            &runner,
            Platform::LinuxSystemd,
            home.path(),
            Path::new("/usr/bin/bosn"),
            Path::new("/state"),
        )
        .expect("enable");
        let status = disable(&runner, Platform::LinuxSystemd, home.path()).expect("disable");
        assert!(!status.written);
        assert!(!status.path.exists());
        let calls = runner.calls.lock().expect("lock").clone();
        assert_eq!(
            calls[2],
            argv(&["systemctl", "--user", "disable", "--now", SERVICE_NAME]),
            "unregister before removing the file"
        );
        assert_eq!(calls[3], argv(&["systemctl", "--user", "daemon-reload"]));
    }

    #[test]
    fn macos_enable_loads_the_agent_and_disable_unloads_it() {
        let home = home();
        let runner = FakeRunner::default();
        let status = enable(
            &runner,
            Platform::MacosLaunchd,
            home.path(),
            Path::new("/usr/local/bin/bosn"),
            Path::new("/Users/u/.local/state/bosn"),
        )
        .expect("enable");
        assert!(
            status
                .path
                .ends_with("Library/LaunchAgents/com.zackees.bosn.plist")
        );
        let contents = std::fs::read_to_string(&status.path).expect("read plist");
        assert!(contents.contains("<key>RunAtLoad</key>"));
        assert!(contents.contains("<key>KeepAlive</key>"));
        assert!(contents.contains("<dict><key>SuccessfulExit</key><false/></dict>"));
        let calls = runner.calls.lock().expect("lock").clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "launchctl");
        assert_eq!(calls[0][1], "load");
        assert_eq!(calls[0][2], "-w");
        disable(&runner, Platform::MacosLaunchd, home.path()).expect("disable");
        let calls = runner.calls.lock().expect("lock").clone();
        assert_eq!(calls[1][1], "unload");
        assert_eq!(calls[1][2], "-w");
    }

    #[test]
    fn a_refused_registration_does_not_claim_success() {
        let home = home();
        let runner = FakeRunner {
            calls: Mutex::new(Vec::new()),
            fail_on: Some(1),
        };
        let error = enable(
            &runner,
            Platform::LinuxSystemd,
            home.path(),
            Path::new("/usr/bin/bosn"),
            Path::new("/state"),
        )
        .expect_err("enable must fail");
        assert!(error.contains("refused"));
    }

    #[test]
    fn disabling_something_that_was_never_enabled_is_a_no_op() {
        let home = home();
        let runner = FakeRunner::default();
        let status = disable(&runner, Platform::LinuxSystemd, home.path()).expect("disable");
        assert!(!status.written);
        assert!(runner.calls.lock().expect("lock").is_empty());
    }

    #[test]
    fn the_unit_paths_are_the_platform_conventional_ones() {
        let home = Path::new("/home/u");
        assert_eq!(
            unit_path(Platform::LinuxSystemd, home),
            Path::new("/home/u/.config/systemd/user/com.zackees.bosn.service")
        );
        assert_eq!(
            unit_path(Platform::MacosLaunchd, home),
            Path::new("/home/u/Library/LaunchAgents/com.zackees.bosn.plist")
        );
    }
}
