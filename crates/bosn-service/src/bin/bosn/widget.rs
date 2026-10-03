//! `bosn widget [--detach]` and `bosn widget install`: start the desktop
//! widget (the separate `bosn-widget` binary, which links the webview
//! toolkit so this CLI never does) or install its systemd user unit.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[path = "widget/desktop.rs"]
mod desktop;

pub const USAGE: &str = "usage: bosn widget [--detach] [--state-dir STATE_DIR]
   or: bosn widget install [--state-dir STATE_DIR]   (systemd user unit; Linux)";

fn fail(message: &str) -> ! {
    eprintln!("bosn widget: {message}");
    std::process::exit(3)
}

/// `bosn-widget` beside this binary, else on `PATH`.
pub fn widget_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "bosn-widget.exe"
    } else {
        "bosn-widget"
    };
    let beside = std::env::current_exe().ok()?.parent()?.join(name);
    if beside.is_file() {
        return Some(beside);
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Is there a desktop to show a widget on?
pub fn graphical_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some()
        || cfg!(any(windows, target_os = "macos"))
}

/// Start the widget detached (used by `--detach` and the CLI auto-launch).
pub fn spawn_detached(binary: &Path, state_dir: &Path, autostart: bool) -> std::io::Result<()> {
    let mut command = Command::new(binary);
    command.arg("--state-dir").arg(state_dir);
    if autostart {
        command.arg("--autostart");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command.spawn().map(|_| ())
}

/// Where `bosn ui` shows the dashboard.
#[derive(Debug, PartialEq, Eq)]
pub enum UiTarget {
    /// Queue the page for the running widget's full-view window.
    Widget,
    /// Start the widget, then queue the page once it registers.
    LaunchWidget,
    Browser,
    /// Print the single-use link (asked for, or no desktop to open it on).
    Print,
}

/// What `bosn ui` decides from.
#[derive(Clone, Copy)]
pub struct UiContext {
    pub print: bool,
    pub browser: bool,
    pub display: bool,
    pub policy: bosn_service::ci::widget::AutoLaunch,
    pub presence: bosn_service::ci::widget::WidgetPresence,
    pub installed: bool,
}

/// The widget when it is running or may be started (an explicit `bosn ui`
/// overrides an earlier quit); otherwise the browser.
pub fn ui_target(context: &UiContext) -> UiTarget {
    use bosn_service::ci::widget::{AutoLaunch, WidgetPresence};
    if context.print || !context.display {
        return UiTarget::Print;
    }
    if context.browser {
        return UiTarget::Browser;
    }
    match context.presence {
        WidgetPresence::Connected => UiTarget::Widget,
        _ if context.policy == AutoLaunch::Never || !context.installed => UiTarget::Browser,
        WidgetPresence::Absent | WidgetPresence::Dismissed => UiTarget::LaunchWidget,
    }
}

/// The systemd user unit: started with the graphical session (so it gets
/// WAYLAND_DISPLAY/DISPLAY/DBUS_SESSION_BUS_ADDRESS from systemd's user
/// environment), restarted after a crash with backoff, never after a quit.
pub fn systemd_unit(binary: &Path, state_dir: &Path) -> String {
    format!(
        "[Unit]
Description=bosn desktop widget
PartOf=graphical-session.target
After=graphical-session.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart={binary} --autostart --state-dir {state}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
",
        binary = binary.display(),
        state = state_dir.display(),
    )
}

pub fn run(mut arguments: impl Iterator<Item = OsString>) {
    let mut state_dir = None;
    let mut detach = false;
    let mut install = false;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("install") if !install => install = true,
            Some("--detach") if !detach => detach = true,
            Some("--state-dir") => {
                state_dir = Some(PathBuf::from(
                    arguments
                        .next()
                        .unwrap_or_else(|| fail("--state-dir needs a value")),
                ));
            }
            _ => fail(USAGE),
        }
    }
    let state_dir = state_dir.unwrap_or_else(bosn_service::mcp::default_state_dir);
    let binary = widget_binary().unwrap_or_else(|| {
        fail("bosn-widget is not installed (it ships with desktop builds; see docs/ci.md)")
    });
    if install {
        return install_unit(&binary, &state_dir);
    }
    if detach {
        spawn_detached(&binary, &state_dir, false).unwrap_or_else(|e| fail(&e.to_string()));
        return;
    }
    let status = Command::new(&binary)
        .arg("--state-dir")
        .arg(&state_dir)
        .status()
        .unwrap_or_else(|e| fail(&e.to_string()));
    std::process::exit(status.code().unwrap_or(1));
}

fn install_unit(binary: &Path, state_dir: &Path) {
    bosn_service::ci::config::enable_desktop_ui(state_dir).unwrap_or_else(|error| fail(&error));
    let home = std::env::var_os("HOME").unwrap_or_else(|| fail("HOME is not set"));
    let dir = Path::new(&home).join(".config/systemd/user");
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| fail(&e.to_string()));
    let unit = dir.join("bosn-widget.service");
    std::fs::write(&unit, systemd_unit(binary, state_dir)).unwrap_or_else(|e| fail(&e.to_string()));
    for args in [
        &["--user", "daemon-reload"][..],
        &["--user", "enable", "bosn-widget.service"],
    ] {
        let _ = Command::new("systemctl").args(args).status();
    }
    desktop::install(Path::new(&home)).unwrap_or_else(|error| fail(&error));
    println!("installed {}", unit.display());
    println!(
        "dashboard enabled in {}",
        state_dir.join("config.toml").display()
    );
    println!("{}", dashboard_readiness(state_dir));
}

/// Inspect the existing daemon without starting or restarting it or exposing a grant.
fn dashboard_readiness(state_dir: &Path) -> &'static str {
    let Ok(client) = bosn_service::Client::for_state(state_dir) else {
        return "dashboard readiness could not be checked; configuration is ready for the next daemon start";
    };
    let Ok(runtime) = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
    else {
        return "dashboard readiness could not be checked; configuration is ready for the next daemon start";
    };
    let ready = runtime.run(kernal_api::async_engine::timeout(
        std::time::Duration::from_secs(2),
        client.ci_ui_grant(Some("/".into())),
    ));
    match ready {
        Ok(Ok(_)) => "running daemon dashboard is ready",
        _ => {
            "dashboard is not ready on the running daemon; start it, or restart it only after all active jobs finish"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_follows_the_graphical_session_and_backs_off_on_crashes() {
        let unit = systemd_unit(Path::new("/opt/bosn-widget"), Path::new("/state"));
        assert!(unit.contains("PartOf=graphical-session.target"));
        assert!(unit.contains("WantedBy=graphical-session.target"));
        assert!(unit.contains("ExecStart=/opt/bosn-widget --autostart --state-dir /state"));
        assert!(
            unit.contains("Restart=on-failure"),
            "a quit (exit 0) is not restarted"
        );
        assert!(unit.contains("StartLimitBurst=5"));
    }

    #[test]
    fn bosn_ui_prefers_the_widget_and_falls_back_to_the_browser() {
        use bosn_service::ci::widget::{AutoLaunch, WidgetPresence::*};
        let desktop = UiContext {
            print: false,
            browser: false,
            display: true,
            policy: AutoLaunch::OnActivity,
            presence: Absent,
            installed: true,
        };
        assert_eq!(
            ui_target(&UiContext {
                presence: Connected,
                ..desktop
            }),
            UiTarget::Widget,
            "a running widget shows the page"
        );
        assert_eq!(ui_target(&desktop), UiTarget::LaunchWidget);
        assert_eq!(
            ui_target(&UiContext {
                print: true,
                presence: Connected,
                ..desktop
            }),
            UiTarget::Print
        );
        assert_eq!(
            ui_target(&UiContext {
                browser: true,
                presence: Connected,
                ..desktop
            }),
            UiTarget::Browser
        );
        assert_eq!(
            ui_target(&UiContext {
                display: false,
                ..desktop
            }),
            UiTarget::Print,
            "no desktop: print the link instead of failing to open anything"
        );
        assert_eq!(
            ui_target(&UiContext {
                policy: AutoLaunch::Never,
                ..desktop
            }),
            UiTarget::Browser
        );
        assert_eq!(
            ui_target(&UiContext {
                installed: false,
                ..desktop
            }),
            UiTarget::Browser
        );
    }
}
