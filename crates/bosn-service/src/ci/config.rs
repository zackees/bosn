//! The daemon's CI configuration, `<state>/config.toml`:
//!
//! ```toml
//! [ui]
//! enabled = true   # default false: no port is bound
//! port = 7428      # default 0: an ephemeral loopback port
//!
//! [widget]
//! auto_launch = "always"              # "always" | "on-activity" | "never"
//! external_hosts = ["git.example.org"] # beyond github.com and gitlab.com
//!
//! [engine]          # each run's engine limits; see [`super::limits`]
//! memory_gib = 16   # default: sized from the host
//! ```

use std::path::Path;

use serde::Deserialize;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub port: u16,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WidgetConfig {
    #[serde(default)]
    pub auto_launch: super::widget::AutoLaunch,
    #[serde(default)]
    pub external_hosts: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CiConfig {
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub widget: WidgetConfig,
    #[serde(default)]
    pub engine: super::limits::EngineConfig,
}

/// Read the config; a missing file means the defaults (no listener). A
/// malformed file is an error rather than a silent default, so a typo cannot
/// quietly open a port or quietly fail to.
pub fn load(state_dir: &Path) -> Result<CiConfig, String> {
    let path = state_dir.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str::<CiConfig>(&text).map_err(|e| format!("{}: {e}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(CiConfig::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_disabled_and_typos_are_errors() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        assert_eq!(load(dir.path()).unwrap(), CiConfig::default());
        std::fs::write(
            dir.path().join("config.toml"),
            "[ui]\nenabled = true\nport = 7428\n",
        )
        .unwrap();
        assert_eq!(
            load(dir.path()).unwrap().ui,
            UiConfig {
                enabled: true,
                port: 7428
            }
        );
        std::fs::write(
            dir.path().join("config.toml"),
            "[widget]\nauto_launch = \"on-activity\"\n",
        )
        .unwrap();
        assert_eq!(
            load(dir.path()).unwrap().widget.auto_launch,
            super::super::widget::AutoLaunch::OnActivity
        );
        std::fs::write(
            dir.path().join("config.toml"),
            "[engine]\nmemory_gib = 12\ncpus = 2\n",
        )
        .unwrap();
        assert_eq!(
            load(dir.path()).unwrap().engine,
            super::super::limits::EngineConfig {
                memory_gib: Some(12),
                cpus: Some(2),
                ..Default::default()
            }
        );
        std::fs::write(dir.path().join("config.toml"), "[engine]\nmemory = 12\n").unwrap();
        assert!(load(dir.path()).unwrap_err().contains("memory"));
        std::fs::write(dir.path().join("config.toml"), "[ui]\nenabeld = true\n").unwrap();
        assert!(load(dir.path()).unwrap_err().contains("enabeld"));
    }
}
