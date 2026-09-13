use bosn_core::{
    SetupConfigLocator, SetupSource, parse_setup_config_locator, parse_setup_document_toml,
};

const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn parses_self_contained_pinned_image_application() {
    let setup = parse_setup_document_toml(&format!(
        r##"
version = 1
[app]
image = "registry.example/team/demo@sha256:{DIGEST}"
workdir = "app"
[app.environment]
RUST_LOG = "info"
[[app.mount]]
source = "src"
target = "/workspace/src"
readonly = true
[task.test]
command = "cargo test"
workdir = "."
[task.test.environment]
CI = "1"
[[file]]
path = "scripts/check.sh"
content = "#!/bin/sh\necho ok\n"
"##
    ))
    .unwrap();
    assert_eq!(setup.version, 1);
    assert_eq!(
        setup.app.source,
        SetupSource::PinnedImage(format!("registry.example/team/demo@sha256:{DIGEST}"))
    );
    assert_eq!(setup.app.workdir.as_deref(), Some("app"));
    assert_eq!(setup.app.mounts[0].source, "src");
    assert!(setup.app.mounts[0].readonly);
    assert_eq!(setup.tasks["test"].workdir.as_deref(), Some("."));
    assert_eq!(setup.files[0].path, "scripts/check.sh");
}

#[test]
fn parses_inline_dockerfile_and_companion_files() {
    let setup = parse_setup_document_toml(
        r#"
version = 1
[app]
dockerfile = """
FROM alpine:3.22
COPY hello.txt /hello.txt
"""
[[file]]
path = "hello.txt"
content = "hello\n"
"#,
    )
    .unwrap();
    assert!(matches!(setup.app.source, SetupSource::InlineDockerfile(_)));
    assert_eq!(setup.files.len(), 1);
}

#[test]
fn rejects_invalid_source_versions_shape_and_bounds() {
    let cases = [
        "version = [",
        "version = 1\n[app]\n",
        &format!("version = 1\n[app]\nimage = 'x@sha256:{DIGEST}'\ndockerfile = 'FROM alpine'"),
        "version = 2\n[app]\ndockerfile = 'FROM alpine'",
        "version = 1\n[app]\nimage = 'alpine:latest'",
        "version = 1\n[app]\ndockerfile = ''",
        "version = 1\n[app]\ndockerfile = 'FROM alpine'\nunknown = true",
    ];
    for source in cases {
        assert!(parse_setup_document_toml(source).is_err(), "{source}");
    }
    let oversized = format!(
        "version = 1\n[app]\ndockerfile = {:?}",
        "x".repeat(512 * 1024 + 1)
    );
    assert!(parse_setup_document_toml(&oversized).is_err());
    let oversized_document = format!(
        "version = 1\n[app]\ndockerfile = 'FROM alpine'\n#{}",
        "x".repeat(1024 * 1024)
    );
    assert!(parse_setup_document_toml(&oversized_document).is_err());
}

#[test]
fn rejects_path_escapes_and_absolute_workspace_values() {
    let base = format!("version = 1\n[app]\nimage = 'demo@sha256:{DIGEST}'");
    for suffix in [
        "\nworkdir = '/host'",
        "\nworkdir = '../host'",
        "\n[[app.mount]]\nsource = '/host'\ntarget = '/workspace'",
        "\n[[app.mount]]\nsource = 'C:/host'\ntarget = '/workspace'",
        "\n[[app.mount]]\nsource = 'src/../secret'\ntarget = '/workspace'",
        "\n[[file]]\npath = '../Dockerfile'\ncontent = 'x'",
        "\n[[file]]\npath = '/Dockerfile'\ncontent = 'x'",
        "\n[[file]]\npath = '.'\ncontent = 'x'",
        "\n[[file]]\npath = 'dir\\file'\ncontent = 'x'",
        "\n[[app.mount]]\nsource = 'src'\ntarget = '/workspace/../host'",
    ] {
        let source = format!("{base}{suffix}");
        assert!(parse_setup_document_toml(&source).is_err(), "{source}");
    }
}

#[test]
fn only_https_remote_config_locators_are_accepted() {
    assert_eq!(
        parse_setup_config_locator("https://configs.example/bosn.toml?revision=1").unwrap(),
        SetupConfigLocator::HttpsUrl("https://configs.example/bosn.toml?revision=1".into())
    );
    assert_eq!(
        parse_setup_config_locator("configs/bosn.toml").unwrap(),
        SetupConfigLocator::LocalPath("configs/bosn.toml".into())
    );
    assert_eq!(
        parse_setup_config_locator("C:/configs/bosn.toml").unwrap(),
        SetupConfigLocator::LocalPath("C:/configs/bosn.toml".into())
    );
    for locator in [
        "http://configs.example/bosn.toml",
        "file:///tmp/bosn.toml",
        "https://user:password@configs.example/bosn.toml",
        "https://configs.example/bosn.toml#fragment",
    ] {
        assert!(parse_setup_config_locator(locator).is_err(), "{locator}");
    }
}
