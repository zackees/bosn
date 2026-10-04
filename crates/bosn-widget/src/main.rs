//! `bosn-widget`: desktop tray status with explicitly opened CI details.
//!
//! One user-session process owns the tray icon, a dock-adjacent fallback,
//! an explicitly toggled panel, and one full-view dashboard. Each window is an
//! isolated kernal-api webview (no IPC bridge) on a page the daemon serves;
//! clicks go page -> daemon -> this process (`WidgetPoll`). The daemon stays
//! headless and decides when the widget should exist.
//!
//! usage: bosn-widget [--state-dir DIR] [--autostart]
//!   --autostart  started by the session (systemd/XDG), not typed by the
//!                user: exits quietly when the user quit the widget earlier
//!                in this graphical session.

mod controller;
mod layout;
mod notify;
mod placement;
mod tray;
mod windows;

use std::path::PathBuf;

use kernal_api::{async_engine::RuntimeBuilder, webview::ExternalWebviewHost};

struct Args {
    state_dir: PathBuf,
    explicit: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut state_dir = None;
    let mut explicit = true;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--state-dir") => {
                state_dir = Some(PathBuf::from(
                    args.next().ok_or("--state-dir needs a value")?,
                ));
            }
            Some("--autostart") => explicit = false,
            _ => return Err("usage: bosn-widget [--state-dir DIR] [--autostart]".into()),
        }
    }
    Ok(Args {
        state_dir: state_dir.unwrap_or_else(bosn_service::mcp::default_state_dir),
        explicit,
    })
}

fn main() {
    // Release verification works without a display, daemon or instance lock.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--build-info")) {
        println!(
            "{}",
            serde_json::json!({
                "version": env!("BOSN_WIDGET_VERSION"),
                "source_sha": env!("BOSN_WIDGET_SOURCE_SHA"),
                "target": env!("BOSN_WIDGET_TARGET"),
            })
        );
        return;
    }
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("bosn-widget: {message}");
            std::process::exit(2);
        }
    };
    // One widget per user: a second start asks the running one to show its
    // details, then exits successfully.
    let Some(lock) = controller::single_instance(&args.state_dir) else {
        controller::ask_running_widget_to_show(&args.state_dir);
        return;
    };
    let runtime = match RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bosn-widget: no async runtime: {error}");
            std::process::exit(1);
        }
    };
    // The app id lets a compositor rule match the windows (KWin on Wayland).
    let host = match ExternalWebviewHost::with_app_id(runtime.handle(), windows::APP_ID) {
        Ok(host) => host,
        Err(error) => {
            eprintln!("bosn-widget: no webview (is WebKitGTK 4.1 installed?): {error:?}");
            std::process::exit(1);
        }
    };
    let client = host.client();
    let _controller = runtime.handle().launch(controller::run(
        client,
        args.state_dir.clone(),
        args.explicit,
    ));
    let code = host.run();
    drop(lock);
    std::process::exit(code);
}
