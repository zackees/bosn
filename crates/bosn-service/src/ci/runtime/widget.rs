//! The runtime's widget operations: presence, the command queue and the
//! debounced auto-launch (the decisions themselves live in `ci::widget`).

use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use super::*;
use crate::ci::widget::{self as policy, WidgetCommand};

impl CiRuntime {
    fn widget_state(&self) -> MutexGuard<'_, WidgetState> {
        self.widget.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether this is the daemon's first failed widget launch: one "the
    /// widget cannot be launched" line per daemon, not one per run.
    pub(crate) fn note_launch_failure(&self) -> bool {
        !self.launch_failure_logged.swap(true, Ordering::Relaxed)
    }

    pub(crate) fn widget_presence(&self) -> WidgetPresence {
        self.widget_state().presence(Instant::now())
    }

    /// Apply the `[widget]` configuration (daemon start).
    pub fn configure_widget(&self, config: WidgetConfig) {
        *self.widget_config.lock().unwrap_or_else(|e| e.into_inner()) = config;
    }

    fn reply(&self, allowed: bool, commands: Vec<WidgetCommand>) -> WidgetReply {
        WidgetReply {
            presence: self.widget_presence(),
            allowed,
            commands,
        }
    }

    pub(super) fn widget_hello(&self, pid: u32, session: &str, explicit: bool) -> WidgetReply {
        let allowed = self
            .widget_state()
            .hello(Instant::now(), pid, session, explicit);
        self.reply(allowed, Vec::new())
    }

    pub(super) fn widget_poll(&self, pid: u32) -> WidgetReply {
        let commands = self.widget_state().poll(Instant::now(), pid);
        self.reply(true, commands)
    }

    pub(super) fn widget_dismiss(&self, session: &str) -> WidgetReply {
        self.widget_state().dismiss(session);
        self.reply(false, Vec::new())
    }

    /// Queue a command; paths must be local, external links allowlisted, and
    /// a quit needs a running widget (a queued one would outlive it).
    pub(super) fn widget_command(&self, command: WidgetCommand) -> Result<WidgetReply, CiError> {
        match &command {
            WidgetCommand::Open { path }
                if !path.starts_with('/') || path.starts_with("//") || path.len() > 256 =>
            {
                return Err(CiError::refused("open needs a local dashboard path"));
            }
            WidgetCommand::OpenExternal { url } => {
                let hosts = self
                    .widget_config
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .external_hosts
                    .clone();
                if !policy::external_allowed(url, &hosts) {
                    eprintln!("bosn ci: refused to open a non-allowlisted external link");
                    return Err(CiError::refused(
                        "only https links to github.com, gitlab.com or a configured host open",
                    ));
                }
            }
            WidgetCommand::Quit if self.widget_presence() != WidgetPresence::Connected => {
                return Err(CiError::refused("no widget is running"));
            }
            _ => {}
        }
        self.widget_state().enqueue(command);
        Ok(self.reply(true, Vec::new()))
    }

    /// Bring the widget up when the policy allows and none is running. Only
    /// with the dashboard serving (the widget shows its pages); debounced.
    pub(crate) fn maybe_launch_widget(&self, trigger: LaunchTrigger) {
        if self.ui.get().is_none() {
            return;
        }
        let policy = self
            .widget_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .auto_launch;
        if !self
            .widget_state()
            .should_launch(Instant::now(), policy, trigger)
        {
            return;
        }
        let Some((program, args)) = policy::launch_command() else {
            return;
        };
        let runtime = self.clone();
        async_engine::launch(async move {
            let spec = args.iter().fold(
                kernal_api::SpawnSpec::new(program)
                    .stdin(kernal_api::StreamMode::Null)
                    .stdout(kernal_api::StreamMode::Null)
                    .stderr(kernal_api::StreamMode::Piped),
                |spec, arg| spec.arg(arg),
            );
            let started =
                kernal_api::run_bounded_command_async(spec, Duration::from_secs(10), 4096).await;
            let ok = started.as_ref().is_ok_and(|out| out.exit.raw_code() == 0);
            if !ok && runtime.note_launch_failure() {
                eprintln!(
                    "bosn ci: widget: not launched ({program} failed; no graphical session or the \
                     bosn-widget service is not installed)"
                );
            }
        })
        .detach();
    }
}
