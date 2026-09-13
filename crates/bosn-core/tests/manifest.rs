use bosn_core::{ManifestRoots, Retention, Scope, parse_manifest_toml};

fn roots() -> ManifestRoots {
    ManifestRoots::new("source.toml", "assets/root", "workspace/root")
}

#[test]
fn roots_are_three_distinct_opaque_inputs() {
    let manifest = parse_manifest_toml("[stack.a]\nimage='x'", roots()).unwrap();
    assert_eq!(manifest.roots.source_provenance, "source.toml");
    assert_eq!(manifest.roots.materialization_root, "assets/root");
    assert_eq!(manifest.roots.workspace_root, "workspace/root");
}

#[test]
fn repository_and_soldr_fixtures_parse_without_path_observation() {
    let bosn = parse_manifest_toml(include_str!("../../../bosn.toml"), roots()).unwrap();
    assert_eq!(bosn.default_stack().unwrap().name, "test");
    let soldr = parse_manifest_toml(include_str!("../../../examples/soldr.toml"), roots()).unwrap();
    assert_eq!(
        soldr.stack("perf").unwrap().workdir.as_deref(),
        Some("/repo")
    );
}

#[test]
fn schema_selection_and_volume_defaults_are_strict() {
    let manifest = parse_manifest_toml("[stack.a]\nimage='x'\ndefault=true\n[stack.a.volumes]\ncache={}\npin={scope='machine',retention='pinned'}\n[task.fallback]\ncmd='x'", roots()).unwrap();
    assert_eq!(manifest.default_stack().unwrap().name, "a");
    assert_eq!(manifest.task("fallback").unwrap().stack, "a");
    assert_eq!(manifest.stack("a").unwrap().volumes[0].scope, Scope::Spec);
    assert_eq!(
        manifest.stack("a").unwrap().volumes[0].mount_at(),
        "/bosn/cache"
    );
    assert_eq!(
        manifest.stack("a").unwrap().volumes[1].retention,
        Retention::Pinned
    );
    assert!(
        parse_manifest_toml(
            "[stack.a]\nimage='x'\ndefault=true\n[stack.b]\nimage='y'\ndefault=true",
            roots()
        )
        .unwrap()
        .default_stack()
        .is_err()
    );
    for invalid in [
        "stakc={}\n[stack.a]\nimage='x'",
        "[stack.a]\nimage='x'\nunknown=1",
        "[stack.a]\nimage='x'\n[task.t]\ncmd='x'\nunknown=1",
        "[stack.a]\nimage=true",
        "[stack.a]\nimage='x'\ndefault='false'",
        "[stack.a]\nimage='x'\n[task.t]\ncmd=true",
    ] {
        assert!(parse_manifest_toml(invalid, roots()).is_err(), "{invalid}");
    }
}

#[test]
fn destinations_guests_and_image_dockerfile_contracts_are_explicit() {
    for destination in [
        "/",
        "/bosn",
        "//bosn/cache",
        "/x/../bosn/cache",
        "/bosn-daemon",
        "/bosn-daemon/heartbeat/x",
    ] {
        let source = format!(
            "[stack.a]\nimage='x'\n[stack.a.mounts.x]\nsource='.'\ndestination='{destination}'"
        );
        assert!(
            parse_manifest_toml(&source, roots()).is_err(),
            "{destination}"
        );
    }
    assert!(parse_manifest_toml("[stack.a]\nimage='x'\nworkdir='/'", roots()).is_ok());
    assert!(parse_manifest_toml("[stack.a]\nimage='x'\ndockerfile='Dockerfile'", roots()).is_ok());
    let guest = parse_manifest_toml(
        "[stack.m]\nimage='x'\nkind='macos-x64-guest'\nacknowledge_macos_license=true",
        roots(),
    )
    .unwrap();
    assert_eq!(
        guest.stack("m").unwrap().guest.as_ref().unwrap().ssh_port,
        2222
    );
    for invalid in [
        "[stack.m]\nimage='x'\nkind='macos-x64-guest'",
        "[stack.m]\nimage='x'\nkind='macos-x64-guest'\nacknowledge_macos_license=true\n[stack.m.mounts.a]\nsource='.'\ndestination='/a'",
        "[stack.m]\nimage='x'\nkind='macos-x64-guest'\nacknowledge_macos_license=true\n[stack.m.guest]\nssh_port=65536",
    ] {
        assert!(parse_manifest_toml(invalid, roots()).is_err());
    }
}
