//! Bind the desktop executable to the root release version and source tree.

fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-env-changed=BOSN_WIDGET_SOURCE_SHA");
    let document: toml::Value = std::fs::read_to_string(root)
        .expect("read release manifest")
        .parse()
        .expect("parse release manifest");
    let version = document["workspace"]["package"]["version"]
        .as_str()
        .expect("workspace release version");
    let source = std::env::var("BOSN_WIDGET_SOURCE_SHA").unwrap_or_else(|_| "development".into());
    assert!(
        source == "development"
            || (source.len() == 40
                && source
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
        "invalid widget source SHA"
    );
    println!("cargo:rustc-env=BOSN_WIDGET_VERSION={version}");
    println!("cargo:rustc-env=BOSN_WIDGET_SOURCE_SHA={source}");
    println!(
        "cargo:rustc-env=BOSN_WIDGET_TARGET={}",
        std::env::var("TARGET").expect("target")
    );
}
