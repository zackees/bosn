//! Permit compact windows only with installed compositor-owned dock placement.

use std::{future::Future, io::Read, path::PathBuf, time::Duration};

const READY: &str = "bosn-widget-corner-ready-v1";

#[derive(Clone, Copy)]
enum Query {
    Loaded,
    Pulse,
}

async fn bus(query: Query) -> Option<String> {
    let name = match query {
        Query::Loaded => "bosn-widget-corner",
        Query::Pulse => READY,
    };
    let output = kernal_api::run_bounded_command_async(
        kernal_api::SpawnSpec::new("busctl").args([
            "--user",
            "--timeout=0.2",
            "call",
            "org.kde.KWin",
            "/Scripting",
            "org.kde.kwin.Scripting",
            "isScriptLoaded",
            "s",
            name,
        ]),
        Duration::from_millis(250),
        512,
    )
    .await
    .ok()?;
    (output.exit.raw_code() == 0)
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

fn loaded(reply: Option<String>) -> Option<bool> {
    match reply.as_deref().map(str::trim) {
        Some("b true") => Some(true),
        Some("b false") => Some(false),
        _ => None,
    }
}

async fn live_ready<F, Fut>(configured: bool, mut query: F) -> bool
where
    F: FnMut(Query) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    if !configured || loaded(query(Query::Loaded).await) != Some(true) {
        return false;
    }
    let Some(initial) = loaded(query(Query::Pulse).await) else {
        return false;
    };
    // A cached marker is insufficient: require fresh execution after geometry
    // readback. Every call is bounded; at most twelve pulse probes are issued.
    for _ in 0..12 {
        kernal_api::async_engine::sleep(Duration::from_millis(100)).await;
        let Some(current) = loaded(query(Query::Pulse).await) else {
            return false;
        };
        if current != initial {
            return loaded(query(Query::Loaded).await) == Some(true);
        }
    }
    false
}

fn enabled(desktop: &str, wayland: bool, script: bool, config: &str) -> bool {
    if !wayland
        || !script
        || !desktop
            .split(':')
            .any(|part| part.eq_ignore_ascii_case("KDE"))
    {
        return false;
    }
    let mut plugins = false;
    let mut active = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            plugins = line == "[Plugins]";
        } else if plugins && let Some(value) = line.strip_prefix("bosn-widget-cornerEnabled=") {
            active = value == "true";
        }
    }
    active
}

fn installed_configuration() -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let mut text = String::new();
    let Ok(file) = std::fs::File::open(config.join("kwinrc")) else {
        return false;
    };
    if file.take(262145).read_to_string(&mut text).is_err() || text.len() > 262144 {
        return false;
    }
    enabled(
        &std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        data.join("kwin/scripts/bosn-widget-corner/contents/code/main.js")
            .is_file(),
        &text,
    )
}

pub async fn configured() -> bool {
    live_ready(installed_configuration(), bus).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_enabled_plugin_but_unloaded_fails_closed() {
        let configured = enabled(
            "KDE",
            true,
            true,
            "[Plugins]\nbosn-widget-cornerEnabled=true\n",
        );
        assert!(configured);
        let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            !runtime.run(live_ready(configured, |_| std::future::ready(Some(
                "b false\n".into()
            ))))
        );
    }

    #[test]
    fn live_execution_requires_fresh_marker_and_valid_replies() {
        let runtime = kernal_api::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(!runtime.run(live_ready(true, |_| std::future::ready(Some(
            "b true".into()
        )))));
        assert!(!runtime.run(live_ready(true, |_| std::future::ready(None))));
        assert!(!runtime.run(live_ready(true, |_| std::future::ready(Some(
            "garbage".into()
        )))));
        let mut pulse = false;
        assert!(runtime.run(live_ready(true, |query| {
            let value = match query {
                Query::Loaded => true,
                Query::Pulse => {
                    pulse = !pulse;
                    pulse
                }
            };
            std::future::ready(Some(format!("b {value}")))
        })));
    }

    #[test]
    fn compact_windows_require_enabled_kde_dock_placement() {
        let config = "[Plugins]\nbosn-widget-cornerEnabled=true\n";
        assert!(enabled("KDE:Plasma", true, true, config));
        assert!(!enabled("GNOME", true, true, config));
        assert!(!enabled("KDE", false, true, config));
        assert!(!enabled("KDE", true, false, config));
        assert!(!enabled(
            "KDE",
            true,
            true,
            "[Other]\nbosn-widget-cornerEnabled=true"
        ));
        assert!(!enabled(
            "KDE",
            true,
            true,
            "[Plugins]\nbosn-widget-cornerEnabled=true\nbosn-widget-cornerEnabled=false"
        ));
    }
}
