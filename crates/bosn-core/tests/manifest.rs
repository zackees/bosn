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
fn repository_and_workspace_mount_fixtures_parse_without_path_observation() {
    let bosn = parse_manifest_toml(include_str!("../../../bosn.toml"), roots()).unwrap();
    assert_eq!(bosn.default_stack().unwrap().name, "test");
    let mounted = parse_manifest_toml(
        "[stack.perf]\nimage='example.invalid/perf@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\nworkdir='/repo'\n[stack.perf.mounts.repo]\nsource='.'\ndestination='/repo'\nreadonly=true",
        roots(),
    )
    .unwrap();
    assert_eq!(
        mounted.stack("perf").unwrap().workdir.as_deref(),
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

#[test]
fn task_secrets_are_declared_by_name_only() {
    let manifest = parse_manifest_toml(
        "[stack.a]\nimage='x'\n[task.ci]\ncmd='true'\nsecrets=['github_token']\n[task.plain]\ncmd='true'\n",
        roots(),
    )
    .unwrap();
    assert_eq!(manifest.task("ci").unwrap().secrets, vec!["github_token"]);
    assert!(manifest.task("plain").unwrap().secrets.is_empty());
    for bad in [
        "secrets='github_token'",
        "secrets=['unknown']",
        "secrets=['github_token','github_token']",
        "secrets=[1]",
        "secrets=['/home/me/token']",
    ] {
        let source = format!("[stack.a]\nimage='x'\n[task.ci]\ncmd='true'\n{bad}\n");
        assert!(parse_manifest_toml(&source, roots()).is_err(), "{bad}");
    }
}

#[test]
fn task_github_api_proxy_is_opt_in_and_carries_no_value() {
    let manifest = parse_manifest_toml(
        "[stack.a]\nimage='x'\n[task.ci]\ncmd='true'\ngithub_api='proxy'\n[task.plain]\ncmd='true'\n",
        roots(),
    )
    .unwrap();
    assert!(manifest.task("ci").unwrap().github_api_proxy);
    assert!(!manifest.task("plain").unwrap().github_api_proxy);
    for bad in [
        "github_api=true",
        "github_api='token'",
        "github_api='http://127.0.0.1:1'",
        "github_api=['proxy']",
    ] {
        let source = format!("[stack.a]\nimage='x'\n[task.ci]\ncmd='true'\n{bad}\n");
        assert!(parse_manifest_toml(&source, roots()).is_err(), "{bad}");
    }
}

#[test]
fn task_fresh_container_is_an_opt_in_boolean() {
    let manifest = parse_manifest_toml(
        "[stack.a]\nimage='x'\n[task.clean]\ncmd='true'\nfresh=true\n[task.plain]\ncmd='true'\n",
        roots(),
    )
    .unwrap();
    assert!(manifest.task("clean").unwrap().fresh);
    assert!(!manifest.task("plain").unwrap().fresh);
    for bad in ["fresh='yes'", "fresh=1", "fresh=['true']"] {
        let source = format!("[stack.a]\nimage='x'\n[task.ci]\ncmd='true'\n{bad}\n");
        assert!(parse_manifest_toml(&source, roots()).is_err(), "{bad}");
    }
}

#[test]
fn job_caches_are_validated_and_default_to_exclusive_repo_scope() {
    let manifest = parse_manifest_toml(
        r#"
[stack.ci]
image = "docker@sha256:0000000000000000000000000000000000000000000000000000000000000000"
[stack.ci.job_caches.toolcache]
volume = "act-toolcache"
scope = "machine"
replicas = 2
[stack.ci.job_caches.cargo]
destination = "/root/.cargo/registry/"
mode = "shared"
"#,
        roots(),
    )
    .unwrap();
    let caches = &manifest.stack("ci").unwrap().job_caches;
    assert_eq!(caches.len(), 2);
    let cargo = caches.iter().find(|c| c.name == "cargo").unwrap();
    assert_eq!(cargo.destination.as_deref(), Some("/root/.cargo/registry"));
    assert_eq!(
        (cargo.scope.as_str(), cargo.mode.as_str()),
        ("repo", "shared")
    );
    let toolcache = caches.iter().find(|c| c.name == "toolcache").unwrap();
    assert_eq!(toolcache.volume.as_deref(), Some("act-toolcache"));
    assert_eq!(toolcache.replicas, 2);
    assert_eq!(toolcache.mode, "exclusive");

    for (body, needle) in [
        (
            "[stack.ci.job_caches.x]\nscope = \"machine\"",
            "must set `volume`",
        ),
        (
            "[stack.ci.job_caches.x]\nvolume = \"a/b\"",
            "Docker volume name",
        ),
        (
            "[stack.ci.job_caches.x]\ndestination = \"rel\"",
            "absolute path",
        ),
        (
            "[stack.ci.job_caches.x]\nvolume = \"v\"\nscope = \"galaxy\"",
            "`scope`",
        ),
        (
            "[stack.ci.job_caches.x]\nvolume = \"v\"\nmode = \"rw\"",
            "`mode`",
        ),
        (
            "[stack.ci.job_caches.x]\nvolume = \"v\"\nreplicas = 0",
            "1..=64",
        ),
        (
            "[stack.ci.job_caches.x]\nvolume = \"v\"\nmode = \"shared\"\nreplicas = 2",
            "only to exclusive",
        ),
        (
            "[stack.ci.job_caches.x]\nvolume = \"v\"\nbogus = 1",
            "unknown key",
        ),
        (
            "[stack.ci.job_caches.x]\nvolume = \"v\"\n[stack.ci.job_caches.y]\nvolume = \"v\"",
            "same volume",
        ),
        (
            "[stack.ci.job_caches.\"bad name\"]\nvolume = \"v\"",
            "cache name",
        ),
    ] {
        let text = format!(
            "[stack.ci]\nimage = \"docker@sha256:0000000000000000000000000000000000000000000000000000000000000000\"\n{body}\n"
        );
        let error = parse_manifest_toml(&text, roots()).unwrap_err().to_string();
        assert!(error.contains(needle), "{body}: {error}");
    }
}
