//! The daemon's opt-in UI configuration, `<state>/config.toml`:
//!
//! ```toml
//! [ui]
//! enabled = true   # default false: no port is bound
//! port = 7428      # default 0: an ephemeral loopback port
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonConfig {
    #[serde(default)]
    ui: UiConfig,
}

/// Read `[ui]`; a missing file means disabled. A malformed file is an error
/// rather than a silent default, so a typo cannot quietly open a port or
/// quietly fail to.
pub fn load(state_dir: &Path) -> Result<UiConfig, String> {
    let path = state_dir.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str::<DaemonConfig>(&text)
            .map(|config| config.ui)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(UiConfig::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_disabled_and_typos_are_errors() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        assert_eq!(load(dir.path()).unwrap(), UiConfig::default());
        std::fs::write(
            dir.path().join("config.toml"),
            "[ui]\nenabled = true\nport = 7428\n",
        )
        .unwrap();
        assert_eq!(
            load(dir.path()).unwrap(),
            UiConfig {
                enabled: true,
                port: 7428
            }
        );
        std::fs::write(dir.path().join("config.toml"), "[ui]\nenabeld = true\n").unwrap();
        assert!(load(dir.path()).unwrap_err().contains("enabeld"));
    }
}
