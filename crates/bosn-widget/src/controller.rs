//! The widget's control loop: register with the daemon, open the bubble,
//! then poll for commands (toggle the panel, open the full view, open an
//! allowlisted external link) and watch for finished runs to notify about.

use std::{path::Path, time::Duration};

use bosn_service::{
    Client,
    ci::{CiRequest, WidgetReply, widget::WidgetCommand},
};
use kernal_api::{
    async_engine,
    platform::fs::OwnedFileLock,
    webview::{ExternalWebviewClient, WebviewHandle, WebviewPermissions, WebviewWindowOptions},
};

use crate::notify::Notifier;

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

/// The three windows this process owns.
#[derive(Default)]
struct Windows {
    bubble: Option<WebviewHandle>,
    panel: Option<WebviewHandle>,
    full: Option<WebviewHandle>,
}

/// Open a daemon page through a fresh single-use grant (views are
/// incognito, so each window signs in on its own).
async fn open(
    views: &ExternalWebviewClient,
    client: &Client,
    path: &str,
    title: &str,
    size: (u32, u32),
) -> Option<WebviewHandle> {
    let grant = client.ci_ui_grant(Some(path.into())).await.ok()?;
    let options = WebviewWindowOptions::new(title, size.0, size.1).ok()?;
    views
        .open_webview_with_options(&grant.url, options, WebviewPermissions::deny_all())
        .await
        .ok()
}

async fn closed(handle: &WebviewHandle) -> bool {
    handle.wait_until_terminal(Duration::from_millis(1)).await.is_ok()
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
        if windows.bubble.is_none() {
            windows.bubble = open(&views, &client, "/widget/bubble", "bosn", (72, 72)).await;
        }
        if let Some(bubble) = &windows.bubble
            && closed(bubble).await
        {
            // Closing the bubble is the deliberate "quit".
            let _ = widget(&client, CiRequest::WidgetDismiss { session: session.clone() }).await;
            let _ = views.request_exit();
            return;
        }
        match widget(&client, CiRequest::WidgetPoll { pid }).await {
            Ok(reply) => {
                for command in reply.commands {
                    apply(command, &views, &client, &mut windows).await;
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

async fn apply(
    command: WidgetCommand,
    views: &ExternalWebviewClient,
    client: &Client,
    windows: &mut Windows,
) {
    match command {
        WidgetCommand::Toggle => match windows.panel.take() {
            Some(panel) if !closed(&panel).await => {
                let _ = panel.close().await;
            }
            _ => {
                windows.panel = open(views, client, "/widget/panel", "bosn panel", (420, 640)).await;
            }
        },
        WidgetCommand::Show => {
            if let Some(bubble) = windows.bubble.take() {
                let _ = bubble.close().await;
            }
            windows.bubble = open(views, client, "/widget/bubble", "bosn", (72, 72)).await;
        }
        WidgetCommand::Open { path } => {
            // Exactly one full-view window: replace it on the new page.
            if let Some(full) = windows.full.take() {
                let _ = full.close().await;
            }
            windows.full = open(views, client, &path, "bosn", (1280, 800)).await;
        }
        WidgetCommand::OpenExternal { url } => crate::notify::open_external(&url),
    }
}
