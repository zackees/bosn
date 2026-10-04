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
//! storage_gib = 32  # default: sized from memory (grows it when pinned alone)
//! spares = 0        # default 1: keep one prepared spare engine (#410)
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
    /// Explicit archive retention; activation requires a coordinated runner.
    #[serde(default)]
    pub cache: Option<super::cache_policy::CachePolicy>,
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

/// Opt-in desktop installation enables the dashboard without rewriting user settings.
/// Returns whether configuration changed. A running daemon still needs an idle restart.
pub fn enable_desktop_ui(state_dir: &Path) -> Result<bool, String> {
    use std::io::Write as _;
    if load(state_dir)?.ui.enabled {
        return Ok(false);
    }
    std::fs::create_dir_all(state_dir).map_err(|e| e.to_string())?;
    let path = state_dir.join("config.toml");
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.to_string()),
    };
    let mut document = original
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| e.to_string())?;
    document["ui"]["enabled"] = toml_edit::value(true);
    let changed = document.to_string();
    toml::from_str::<CiConfig>(&changed).map_err(|e| e.to_string())?;
    let staging =
        kernal_api::platform::fs::TemporaryDirectory::in_directory(state_dir, "widget-config-")
            .map_err(|e| e.to_string())?;
    let staged = staging.path().join("config.toml");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|e| e.to_string())?;
    if let Ok(metadata) = std::fs::metadata(&path) {
        file.set_permissions(metadata.permissions())
            .map_err(|e| e.to_string())?;
    }
    file.write_all(changed.as_bytes())
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    if std::fs::read_to_string(&path).unwrap_or_default() != original {
        return Err("configuration changed during desktop installation; retry".into());
    }
    std::fs::rename(&staged, &path).map_err(|e| e.to_string())?;
    Ok(true)
/// Configuration usable by both run and spare engine planning.
pub(crate) fn load_engine(state_dir: &Path) -> Result<super::limits::EngineConfig, String> {
    let settings = load(state_dir)?;
    if settings.cache.is_some() {
        return Err("configured cache retention requires verified act2 retention pin and warm cohort migration; this runner is not enrolled".into());
    }
    Ok(settings.engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_install_enables_ui_and_preserves_existing_policy_and_comments() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# my limits\n[engine]\ncpus = 3\n[widget]\nauto_launch = \"never\"\n",
        )
        .unwrap();
        assert!(enable_desktop_ui(dir.path()).unwrap());
        let enabled = load(dir.path()).unwrap();
        assert!(enabled.ui.enabled);
        assert_eq!(enabled.engine.cpus, Some(3));
        assert_eq!(
            enabled.widget.auto_launch,
            super::super::widget::AutoLaunch::Never
        );
        let original = std::fs::read_to_string(&path).unwrap();
        assert!(original.contains("# my limits"));
        assert!(!enable_desktop_ui(dir.path()).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::write(&path, "[ui]\nenabeld = false\n").unwrap();
        assert!(enable_desktop_ui(dir.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[ui]\nenabeld = false\n"
        );
    }

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
    #[test]
    fn engine_configuration_refuses_unenrolled_retention() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        assert!(load_engine(dir.path()).is_ok());
        std::fs::write(dir.path().join("config.toml"),
            "[cache]\nrepository_max_bytes=100\naggregate_max_bytes=200\nmax_age_secs=3600\nunused_age_secs=1800\nmaintenance_interval_secs=60\n").unwrap();
        assert!(load(dir.path()).unwrap().cache.is_some());
        assert!(
            load_engine(dir.path())
                .unwrap_err()
                .contains("not enrolled")
        );
    }
}
