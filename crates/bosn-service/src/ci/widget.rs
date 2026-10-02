//! The desktop widget's daemon side: presence, dismissal, the command queue
//! the dashboard fills (toggle, open, open-external, quit) and the auto-launch
//! decision. The widget is a separate user-session process that talks to the
//! daemon only through typed CI requests; the daemon stays headless.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

/// A poll older than this means the widget is gone (crashed or quit).
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
/// At most one launch attempt per this interval.
pub const LAUNCH_DEBOUNCE: Duration = Duration::from_secs(10);
const MAX_QUEUED: usize = 64;

vocabulary!(
    /// When the daemon brings the widget up on its own.
    AutoLaunch, "auto_launch" {
        Always => "always",
        OnActivity => "on-activity",
        Never => "never",
    }
);

// The `vocabulary!` macro cannot carry `#[default]` for one enum only.
#[allow(clippy::derivable_impls)]
impl Default for AutoLaunch {
    fn default() -> Self {
        Self::Always
    }
}

vocabulary!(
    /// Whether a widget is running, gone, or deliberately quit.
    WidgetPresence, "presence" {
        Connected => "connected",
        Absent => "absent",
        Dismissed => "dismissed",
    }
);

/// What the widget process should do next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum WidgetCommand {
    /// Show or hide the panel.
    Toggle,
    /// Show the bubble (a second `bosn widget` asked for it).
    Show,
    /// Open or focus the full-view window on a dashboard path.
    Open { path: String },
    /// Open an allowlisted external URL with the OS opener.
    OpenExternal { url: String },
    /// Quit deliberately, as closing the bubble does (dismiss, then exit).
    Quit,
}

/// Why the daemon considered launching the widget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchTrigger {
    DaemonStart,
    Activity,
}

#[derive(Debug, Default)]
pub struct WidgetState {
    /// (last poll, pid, graphical session) of the live widget.
    seen: Option<(Instant, u32, String)>,
    /// A menu quit in this graphical session suppresses auto-launch.
    dismissed_session: Option<String>,
    queue: VecDeque<WidgetCommand>,
    last_launch: Option<Instant>,
}

impl WidgetState {
    pub fn presence(&self, now: Instant) -> WidgetPresence {
        match &self.seen {
            Some((at, _, _)) if now.duration_since(*at) < HEARTBEAT_TIMEOUT => {
                WidgetPresence::Connected
            }
            _ if self.dismissed_session.is_some() => WidgetPresence::Dismissed,
            _ => WidgetPresence::Absent,
        }
    }

    /// A widget process starting. `explicit` (`bosn widget` typed by the
    /// user) clears a dismissal; an auto-launched one in the dismissed
    /// session is told to exit. A new session always clears it.
    pub fn hello(&mut self, now: Instant, pid: u32, session: &str, explicit: bool) -> bool {
        let dismissed_here = self.dismissed_session.as_deref() == Some(session);
        if dismissed_here && !explicit {
            return false;
        }
        self.dismissed_session = None;
        // A quit addressed the widget that was running, never its successor.
        self.queue.retain(|command| *command != WidgetCommand::Quit);
        self.seen = Some((now, pid, session.into()));
        true
    }

    /// The heartbeat; returns (and clears) the pending commands.
    pub fn poll(&mut self, now: Instant, pid: u32) -> Vec<WidgetCommand> {
        if let Some((at, seen_pid, _)) = &mut self.seen
            && *seen_pid == pid
        {
            *at = now;
            return self.queue.drain(..).collect();
        }
        Vec::new()
    }

    /// A deliberate quit from the widget's menu (a crash is not one).
    pub fn dismiss(&mut self, session: &str) {
        self.dismissed_session = Some(session.into());
        self.seen = None;
        self.queue.clear();
    }

    /// Queue a command for the widget; the oldest are dropped when full.
    pub fn enqueue(&mut self, command: WidgetCommand) {
        if self.queue.len() == MAX_QUEUED {
            self.queue.pop_front();
        }
        self.queue.push_back(command);
    }

    /// Should the daemon try to launch the widget now? Records the attempt.
    pub fn should_launch(
        &mut self,
        now: Instant,
        policy: AutoLaunch,
        trigger: LaunchTrigger,
    ) -> bool {
        let wanted = !matches!(
            (policy, trigger),
            (AutoLaunch::Never, _) | (AutoLaunch::OnActivity, LaunchTrigger::DaemonStart)
        );
        let recently = self
            .last_launch
            .is_some_and(|at| now.duration_since(at) < LAUNCH_DEBOUNCE);
        if !wanted || recently || self.presence(now) != WidgetPresence::Absent {
            return false;
        }
        self.last_launch = Some(now);
        true
    }
}

/// External links the widget may open: `https` to an allowlisted host only.
pub fn external_allowed(url: &str, extra_hosts: &[String]) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.contains(['@', ':']) || host.is_empty() {
        return false;
    }
    ["github.com", "gitlab.com"]
        .iter()
        .copied()
        .chain(extra_hosts.iter().map(String::as_str))
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
}

/// The platform command that starts the installed widget service, if any.
pub fn launch_command() -> Option<(&'static str, Vec<String>)> {
    if cfg!(target_os = "linux") {
        Some((
            "systemctl",
            vec![
                "--user".into(),
                "start".into(),
                "bosn-widget.service".into(),
            ],
        ))
    } else if cfg!(target_os = "macos") {
        let uid = kernal_api::platform::ipc::current_user_id().ok()?;
        Some((
            "launchctl",
            vec!["kickstart".into(), format!("gui/{uid}/dev.bosn.widget")],
        ))
    } else if cfg!(windows) {
        Some((
            "schtasks",
            vec!["/Run".into(), "/TN".into(), "bosn-widget".into()],
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_follows_heartbeats_and_a_crash_is_not_a_dismissal() {
        let t0 = Instant::now();
        let mut w = WidgetState::default();
        assert_eq!(w.presence(t0), WidgetPresence::Absent);
        assert!(w.hello(t0, 7, "s1", false));
        assert_eq!(w.presence(t0), WidgetPresence::Connected);
        // No heartbeat for longer than the timeout: gone, not dismissed.
        assert_eq!(w.presence(t0 + HEARTBEAT_TIMEOUT), WidgetPresence::Absent);
    }

    #[test]
    fn a_menu_quit_holds_until_a_new_session_or_an_explicit_start() {
        let t0 = Instant::now();
        let mut w = WidgetState::default();
        w.hello(t0, 7, "s1", false);
        w.dismiss("s1");
        assert_eq!(w.presence(t0), WidgetPresence::Dismissed);
        assert!(!w.should_launch(t0, AutoLaunch::Always, LaunchTrigger::Activity));
        assert!(
            !w.hello(t0, 8, "s1", false),
            "auto-launch in the dismissed session exits"
        );
        assert!(
            w.hello(t0, 9, "s1", true),
            "`bosn widget` typed by the user clears it"
        );
        w.dismiss("s1");
        assert!(w.hello(t0, 10, "s2", false), "a new login clears it");
    }

    #[test]
    fn launches_are_debounced_and_follow_the_policy() {
        let t0 = Instant::now();
        let mut w = WidgetState::default();
        assert!(!w.should_launch(t0, AutoLaunch::Never, LaunchTrigger::Activity));
        assert!(!w.should_launch(t0, AutoLaunch::OnActivity, LaunchTrigger::DaemonStart));
        assert!(w.should_launch(t0, AutoLaunch::OnActivity, LaunchTrigger::Activity));
        // 20 concurrent submissions inside the debounce: one attempt only.
        assert!((0..20).all(|_| !w.should_launch(t0, AutoLaunch::Always, LaunchTrigger::Activity)));
        assert!(w.should_launch(
            t0 + LAUNCH_DEBOUNCE,
            AutoLaunch::Always,
            LaunchTrigger::Activity
        ));
        w.hello(t0 + LAUNCH_DEBOUNCE, 3, "s", false);
        assert!(
            !w.should_launch(
                t0 + LAUNCH_DEBOUNCE * 3,
                AutoLaunch::Always,
                LaunchTrigger::Activity
            ) || w.presence(t0 + LAUNCH_DEBOUNCE * 3) == WidgetPresence::Absent
        );
    }

    #[test]
    fn commands_reach_only_the_registered_widget_and_stay_bounded() {
        let t0 = Instant::now();
        let mut w = WidgetState::default();
        w.hello(t0, 7, "s", false);
        for _ in 0..100 {
            w.enqueue(WidgetCommand::Toggle);
        }
        assert!(w.poll(t0, 99).is_empty(), "an unknown pid gets nothing");
        assert_eq!(w.poll(t0, 7).len(), MAX_QUEUED);
        assert!(w.poll(t0, 7).is_empty(), "drained once");
    }

    #[test]
    fn a_quit_addresses_only_the_widget_that_was_running() {
        let t0 = Instant::now();
        let mut w = WidgetState::default();
        w.hello(t0, 7, "s", false);
        w.enqueue(WidgetCommand::Quit);
        w.enqueue(WidgetCommand::Open { path: "/".into() });
        // Widget 7 died before its next poll; its replacement must not quit.
        assert!(w.hello(t0, 8, "s", false));
        assert_eq!(w.poll(t0, 8), [WidgetCommand::Open { path: "/".into() }]);
    }

    #[test]
    fn only_https_allowlisted_hosts_open_externally() {
        let extra = ["git.example.org".to_string()];
        assert!(external_allowed(
            "https://github.com/zackees/bosn/pull/1",
            &extra
        ));
        assert!(external_allowed("https://GitLab.com/x", &extra));
        assert!(external_allowed("https://git.example.org/x", &extra));
        for url in [
            "http://github.com/x",
            "https://github.com.evil.example/x",
            "https://evil.example/github.com",
            "https://user@github.com/x",
            "https://github.com:444/x",
            "javascript:alert(1)",
            "file:///etc/passwd",
        ] {
            assert!(!external_allowed(url, &extra), "{url}");
        }
    }
}
