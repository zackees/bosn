//! Permit compact windows only with installed compositor-owned dock placement.

use std::{io::Read, path::PathBuf};

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

pub fn configured() -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

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
