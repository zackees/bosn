//! Install KDE's compositor-owned lower-right placement for the bubble.

use std::{path::Path, process::Command};

const ID: &str = "bosn-widget-corner";
const PLACEMENT: &str = include_str!("placement.js");
const METADATA: &str = r#"{
  "KPlugin": {
    "Id": "bosn-widget-corner",
    "Name": "Bosn widget corner",
    "Description": "Keep the Bosn bubble above the lower-right usable corner",
    "Version": "1.0",
    "License": "MIT"
  },
  "X-Plasma-API": "javascript",
  "X-Plasma-MainScript": "code/main.js",
  "KPackageStructure": "KWin/Script"
}"#;

fn checked(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("{program}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with {status}"))
    }
}

fn install_focus_rule() -> Result<(), String> {
    // An exact-title rule keeps automatic bubble creation from taking focus.
    // Preserve the user's rule registry; panel and full-view windows still focus.
    let output = Command::new("kreadconfig6")
        .args([
            "--file",
            "kwinrulesrc",
            "--group",
            "General",
            "--key",
            "rules",
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err("cannot read existing KDE window rules".into());
    }
    let registry = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    let mut rules: Vec<&str> = registry
        .trim()
        .split(',')
        .filter(|id| !id.is_empty())
        .collect();
    let id = "bosn-widget-focus";
    if !rules.contains(&id) {
        rules.push(id);
    }
    for (key, value) in [
        ("Description", "Bosn bubble never takes focus"),
        ("wmclass", "dev.bosn.widget"),
        ("wmclassmatch", "1"),
        ("title", "bosn bubble"),
        ("titlematch", "1"),
        ("acceptfocus", "false"),
        ("acceptfocusrule", "2"),
    ] {
        checked(
            "kwriteconfig6",
            &["--file", "kwinrulesrc", "--group", id, "--key", key, value],
        )?;
    }
    checked(
        "kwriteconfig6",
        &[
            "--file",
            "kwinrulesrc",
            "--group",
            "General",
            "--key",
            "rules",
            &rules.join(","),
        ],
    )?;
    checked(
        "kwriteconfig6",
        &[
            "--file",
            "kwinrulesrc",
            "--group",
            "General",
            "--key",
            "count",
            &rules.len().to_string(),
        ],
    )
}

pub fn install(home: &Path) -> Result<(), String> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if !desktop
        .split(':')
        .any(|value| value.eq_ignore_ascii_case("KDE"))
        || std::env::var_os("WAYLAND_DISPLAY").is_none()
    {
        return Ok(());
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));
    let package = data.join("kwin/scripts").join(ID);
    let code = package.join("contents/code");
    std::fs::create_dir_all(&code).map_err(|error| error.to_string())?;
    std::fs::write(package.join("metadata.json"), METADATA).map_err(|error| error.to_string())?;
    std::fs::write(code.join("main.js"), PLACEMENT).map_err(|error| error.to_string())?;
    checked(
        "kwriteconfig6",
        &[
            "--file",
            "kwinrc",
            "--group",
            "Plugins",
            "--key",
            "bosn-widget-cornerEnabled",
            "true",
        ],
    )?;
    install_focus_rule()?;
    // Reload our script when installation updates its bytes.
    checked("qdbus", &["org.kde.KWin", "/Scripting", "unloadScript", ID])?;
    checked("qdbus", &["org.kde.KWin", "/KWin", "reconfigure"])?;
    checked("qdbus", &["org.kde.KWin", "/Scripting", "start"])?;
    println!(
        "installed KDE lower-right widget placement: {}",
        package.display()
    );
    Ok(())
}
