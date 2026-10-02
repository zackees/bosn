//! The widget's control loop: register with the daemon, open the bubble,
//! then poll for commands (toggle the panel, open the full view, open an
//! allowlisted external link, quit) and watch for finished runs to notify
//! about.

use std::{path::Path, time::Duration};

use bosn_service::{
    Client,
    ci::{CiRequest, WidgetReply, widget::WidgetCommand},
};
use kernal_api::{
    async_engine,
    platform::fs::OwnedFileLock,
    webview::{ExternalWebviewClient, WebviewHandle, WebviewPermissions, WebviewUrlGrant},
};

use crate::{
    layout::{Layout, Presence, Step},
    notify::Notifier,
    windows::Window,
};

const POLL: Duration = Duration::from_secs(1);

/// The single-instance lock (`<state>/widget.lock`); `None` if one runs.
pub fn single_instance(state_dir: &Path) -> Option<OwnedFileLock> {
    std::fs::create_dir_all(state_dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state_dir.join("widget.lock"))
        .ok()?;
    kernal_api::platform::fs::try_lock_exclusive_owned(file).ok()
}

/// A second `bosn widget`: ask the running one to show its bubble.
pub fn ask_running_widget_to_show(state_dir: &Path) {
    let Ok(client) = Client::for_state(state_dir) else {
        return;
    };
    let Ok(runtime) = kernal_api::async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    let _ = runtime.run(widget(&client, CiRequest::WidgetCommand {
        command: WidgetCommand::Show,
    }));
}

async fn widget(client: &Client, request: CiRequest) -> Result<WidgetReply, bosn_service::Error> {
    client.ci_widget(request).await
}

/// The graphical session this process belongs to (dismissal is per session).
fn session_id() -> String {
    std::env::var("XDG_SESSION_ID")
        .or_else(|_| std::env::var("WAYLAND_DISPLAY"))
        .or_else(|_| std::env::var("DISPLAY"))
        .unwrap_or_else(|_| "default".into())
}

/// Whether the control loop keeps going after a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Quit,
}

/// The deliberate quit (the bubble closed, or Quit in the panel): record
/// `dismissed` for this graphical session, then end the event loop.
async fn quit(client: &Client, views: &ExternalWebviewClient, session: &str) {
    let _ = widget(client, CiRequest::WidgetDismiss {
        session: session.into(),
    })
    .await;
    let _ = views.request_exit();
}

/// The three windows this process owns, and what each is doing.
#[derive(Default)]
struct Windows {
    layout: Layout,
    bubble: Option<WebviewHandle>,
    panel: Option<WebviewHandle>,
    full: Option<WebviewHandle>,
}

impl Windows {
    fn slot(&mut self, window: Window) -> &mut Option<WebviewHandle> {
        match window {
            Window::Bubble => &mut self.bubble,
            Window::Panel => &mut self.panel,
            Window::Full => &mut self.full,
        }
    }

    /// Drop a window that is gone; the next command that needs it reopens it.
    fn forget(&mut self, window: Window) {
        *self.slot(window) = None;
        self.layout.lost(window);
    }

    /// Forget a panel or full view the user closed with its title bar.
    async fn forget_closed(&mut self) {
        for window in [Window::Panel, Window::Full] {
            if let Some(handle) = self.slot(window).as_ref()
                && closed(handle).await
            {
                self.forget(window);
            }
        }
    }

    /// Carry out one daemon command: plan it, run each step, and stop at the
    /// first failure (that window is forgotten and reopens next time). A
    /// quit step is the caller's to carry out: it ends the loop.
    async fn apply(&mut self, command: WidgetCommand, views: &ExternalWebviewClient, client: &Client) -> Flow {
        self.forget_closed().await;
        for step in self.layout.plan(command) {
            if step == Step::Quit {
                return Flow::Quit;
            }
            if self.execute(&step, views, client).await {
                self.layout.record(&step);
            } else {
                if let Some(window) = step.window() {
                    self.forget(window);
                }
                return Flow::Continue;
            }
        }
        Flow::Continue
    }

    /// Run one step; `false` when it did not take effect.
    async fn execute(&mut self, step: &Step, views: &ExternalWebviewClient, client: &Client) -> bool {
        match step {
            Step::Open { window, path } => {
                let opened = open(views, client, *window, path).await;
                let ok = opened.is_some();
                *self.slot(*window) = opened;
                ok
            }
            Step::Show(window) => match self.slot(*window) {
                Some(handle) => handle.show().await.is_ok(),
                None => false,
            },
            Step::Hide(window) => match self.slot(*window) {
                Some(handle) => handle.hide().await.is_ok(),
                None => false,
            },
            Step::Focus(window) => match self.slot(*window) {
                Some(handle) => handle.focus().await.is_ok(),
                None => false,
            },
            Step::Navigate { path } => match (&self.full, grant(client, path).await) {
                (Some(full), Some(grant)) => full.navigate(&grant).await.is_ok(),
                _ => false,
            },
            Step::External { url } => {
                crate::notify::open_external(url);
                true
            }
            // `apply` returns before executing a quit.
            Step::Quit => false,
        }
    }
}

/// A daemon page's URL with a fresh single-use sign-in token (views are
/// incognito, so each window, and each navigation, signs in on its own).
async fn page_url(client: &Client, path: &str) -> Option<String> {
    client.ci_ui_grant(Some(path.into())).await.ok().map(|grant| grant.url)
}

async fn grant(client: &Client, path: &str) -> Option<WebviewUrlGrant> {
    WebviewUrlGrant::new(&page_url(client, path).await?).ok()
}

/// Open `window` on a daemon page, presented for this display.
async fn open(views: &ExternalWebviewClient, client: &Client, window: Window, path: &str) -> Option<WebviewHandle> {
    let url = page_url(client, path).await?;
    let options = window.options(views.window_support()).ok()?;
    views
        .open_webview_with_options(&url, options, WebviewPermissions::deny_all())
        .await
        .ok()
}

/// Whether the window is gone. Races the untimed terminal wait against a
/// short timer: dropping that wait leaves the window alive, whereas a timed
/// `wait_until_terminal` revokes (closes) the window when its timeout lapses.
async fn closed(handle: &WebviewHandle) -> bool {
    async_engine::timeout(Duration::from_millis(1), handle.wait_for_terminal())
        .await
        .is_ok()
}

pub async fn run(views: ExternalWebviewClient, state_dir: std::path::PathBuf, explicit: bool) {
    let session = session_id();
    let pid = std::process::id();
    let client = loop {
        match Client::for_state(&state_dir) {
            Ok(client) => break client,
            Err(_) => async_engine::sleep(POLL).await,
        }
    };
    let hello = loop {
        let request = CiRequest::WidgetHello {
            pid,
            session: session.clone(),
            explicit,
        };
        match widget(&client, request).await {
            Ok(reply) => break reply,
            // The daemon may be restarting: keep trying, never give up.
            Err(_) => async_engine::sleep(POLL).await,
        }
    };
    if !hello.allowed {
        let _ = views.request_exit();
        return;
    }
    let mut windows = Windows::default();
    let mut notifier = Notifier::default();
    loop {
        if windows.layout.presence(Window::Bubble) == Presence::Absent {
            windows.apply(WidgetCommand::Show, &views, &client).await;
        }
        if let Some(bubble) = &windows.bubble
            && closed(bubble).await
        {
            // Closing the bubble is the deliberate "quit".
            quit(&client, &views, &session).await;
            return;
        }
        match widget(&client, CiRequest::WidgetPoll { pid }).await {
            Ok(reply) => {
                for command in reply.commands {
                    if windows.apply(command, &views, &client).await == Flow::Quit {
                        quit(&client, &views, &session).await;
                        return;
                    }
                }
            }
            // A daemon restart: re-register once it answers again.
            Err(_) => {
                let request = CiRequest::WidgetHello {
                    pid,
                    session: session.clone(),
                    explicit: true,
                };
                let _ = widget(&client, request).await;
            }
        }
        notifier.check(&client).await;
        async_engine::sleep(POLL).await;
    }
}
