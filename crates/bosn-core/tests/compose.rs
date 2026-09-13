use bosn_core::{
    ComposeErrorCode, MountSpec, SetupSource, parse_and_plan_compose_yaml,
    parse_and_translate_compose_yaml, parse_compose_yaml,
};

const PINNED_IMAGE: &str = "registry.example/team/demo@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

// Characterization of src/bosn/compose.py::test_realistic_multi_service_file_parses_end_to_end.
// The Rust foundation deliberately gives the accepted values a typed, inert plan instead of
// forwarding YAML to Docker.
const REPRESENTED_SUBSET: &str = r#"
name: demo
version: "3.9"
x-service: &service
  image: alpine:3.20
  environment:
    ANSWER: 42
services:
  db:
    image: postgres:16
    volumes:
      - data:/var/lib/postgresql/data
    networks: [back]
    healthcheck:
      test: [CMD, pg_isready]
      interval: 5s
  app:
    <<: *service
    build:
      context: ./app/.
      dockerfile: ./Dockerfile
    profiles: [default]
    environment: [DATABASE_URL=postgres://db/app]
    ports: ["8080:8080"]
    depends_on:
      db:
        condition: service_healthy
    volumes:
      - ./config/.:/app/config:ro
      - type: tmpfs
        target: /var/cache/app
    networks:
      back: {}
    command: [sh, -lc, echo hello]
    entrypoint: /entrypoint.sh
    restart: unless-stopped
    labels: [com.example.role=app]
volumes:
  data:
networks:
  back:
    internal: true
"#;

#[test]
fn parses_anchors_supported_fields_and_normalizes_inert_plan() {
    let plan = parse_and_plan_compose_yaml(REPRESENTED_SUBSET).unwrap();
    assert_eq!(plan.version, 1);
    assert_eq!(plan.document.name.as_deref(), Some("demo"));
    assert_eq!(plan.document.version.as_deref(), Some("3.9"));
    let app = &plan.document.services["app"];
    assert_eq!(app.image.as_deref(), Some("alpine:3.20"));
    assert_eq!(app.build.as_ref().unwrap().context.as_str(), "app");
    assert_eq!(
        app.build.as_ref().unwrap().dockerfile.as_str(),
        "Dockerfile"
    );
    assert_eq!(app.environment["DATABASE_URL"], "postgres://db/app");
    assert!(matches!(
        app.mounts.first(),
        Some(MountSpec::Bind { source, target, read_only: true })
            if source.as_str() == "config" && target == "/app/config"
    ));
    assert_eq!(plan.document.networks["back"].internal, Some(true));
    assert!(plan.normalized_json.contains("service_healthy"));
    assert!(plan.digest.starts_with("sha256:"));
}

#[test]
fn map_order_and_yaml_formatting_do_not_change_plan_digest() {
    let first = r#"
services:
  app:
    image: alpine:3.20
networks:
  back:
volumes:
  data:
"#;
    let second = r#"
volumes: {data: {}}
networks: {back: {}}
services: {app: {image: 'alpine:3.20'}}
"#;
    let first = parse_and_plan_compose_yaml(first).unwrap();
    let second = parse_and_plan_compose_yaml(second).unwrap();
    assert_eq!(first.normalized_json, second.normalized_json);
    assert_eq!(first.digest, second.digest);
}

#[test]
fn ordered_semantics_still_change_digest() {
    let first = parse_and_plan_compose_yaml(
        "services:\n  app:\n    image: alpine\n    command: [one, two]\n",
    )
    .unwrap();
    let second = parse_and_plan_compose_yaml(
        "services:\n  app:\n    image: alpine\n    command: [two, one]\n",
    )
    .unwrap();
    assert_ne!(first.digest, second.digest);
}

#[test]
fn rejects_path_escape_and_ambiguous_mounts_before_any_effect() {
    for source in [
        "services:\n  app:\n    build: ../escape\n",
        "services:\n  app:\n    image: alpine\n    volumes: [../host:/target]\n",
        "services:\n  app:\n    image: alpine\n    volumes: [./host:/target/../escape]\n",
    ] {
        let error = parse_compose_yaml(source).unwrap_err();
        assert_eq!(error.code, ComposeErrorCode::UnsafePath, "{source}");
    }
    let error = parse_compose_yaml("services:\n  app:\n    image: alpine\n    volumes: [/data]\n")
        .unwrap_err();
    assert_eq!(error.code, ComposeErrorCode::Ambiguous);
}

#[test]
fn refuses_unrepresented_or_ambiguous_compose_features_with_paths() {
    let unsupported = parse_compose_yaml(
        "services:\n  app:\n    image: alpine\n    deploy:\n      replicas: 2\n",
    )
    .unwrap_err();
    assert_eq!(unsupported.code, ComposeErrorCode::Unsupported);
    assert_eq!(unsupported.path, "services.app.deploy");

    let build_args =
        parse_compose_yaml("services:\n  app:\n    build:\n      context: .\n      args: {A: B}\n")
            .unwrap_err();
    assert_eq!(build_args.code, ComposeErrorCode::Unsupported);
    assert_eq!(build_args.path, "services.app.build.args");

    let missing_volume = parse_compose_yaml(
        "services:\n  app:\n    image: alpine\n    volumes: [not-declared:/data]\n",
    )
    .unwrap_err();
    assert_eq!(missing_volume.code, ComposeErrorCode::Ambiguous);
    assert_eq!(missing_volume.path, "services.app.volumes");
}

#[test]
fn parser_is_pure_and_has_no_source_path_api() {
    // The parser deliberately receives only YAML text.  This makes it impossible for this
    // planning layer to read a build context, call Docker, or resolve against CWD.
    let document = parse_compose_yaml("services:\n  app:\n    image: alpine\n").unwrap();
    assert_eq!(document.services.len(), 1);
}

#[test]
fn translates_the_lossless_single_service_setup_subset() {
    let source = format!(
        r#"
services:
  app:
    image: {PINNED_IMAGE}
    environment:
      LOG_LEVEL: info
      FEATURE_X: enabled
    volumes:
      - .:/workspace
      - ./src:/workspace/src:ro
    working_dir: /workspace/src
    command: [sh, -lc, 'exec ./serve --port 8080']
"#
    );
    let plan = parse_and_translate_compose_yaml(&source).unwrap();
    assert_eq!(plan.version, 1);
    assert_eq!(plan.service, "app");
    assert_eq!(
        plan.setup.app.source,
        SetupSource::PinnedImage(PINNED_IMAGE.into())
    );
    assert_eq!(plan.setup.app.environment["LOG_LEVEL"], "info");
    assert_eq!(plan.setup.app.workdir.as_deref(), Some("src"));
    assert_eq!(
        plan.setup.app.command.as_deref(),
        Some("exec ./serve --port 8080")
    );
    assert_eq!(plan.setup.app.mounts.len(), 2);
    assert_eq!(plan.setup.app.mounts[0].source, ".");
    assert_eq!(plan.setup.app.mounts[0].target, "/workspace");
    assert!(plan.setup.app.mounts[1].readonly);
    assert!(plan.setup.tasks.is_empty());
    assert!(plan.setup.files.is_empty());
    assert!(plan.normalized_json.contains("workspace"));
    assert!(plan.digest.starts_with("sha256:"));
}

#[test]
fn setup_translation_digest_is_canonical_for_equivalent_documents() {
    let first = format!(
        "services:\n  app:\n    image: {PINNED_IMAGE}\n    environment: {{B: two, A: one}}\n    volumes: [./src:/workspace/src:ro]\n    working_dir: /workspace/src\n"
    );
    let second = format!(
        "services:\n  app:\n    working_dir: /workspace/src\n    volumes:\n      - ./src:/workspace/src:ro\n    environment:\n      A: one\n      B: two\n    image: {PINNED_IMAGE}\n"
    );
    let first = parse_and_translate_compose_yaml(&first).unwrap();
    let second = parse_and_translate_compose_yaml(&second).unwrap();
    assert_eq!(first.normalized_json, second.normalized_json);
    assert_eq!(first.digest, second.digest);
}

#[test]
fn setup_translation_refuses_non_lossless_compose_semantics_with_paths() {
    let cases = [
        (
            format!(
                "services:\n  app:\n    image: {PINNED_IMAGE}\n  db:\n    image: {PINNED_IMAGE}\n"
            ),
            "services",
        ),
        (
            "services:\n  app:\n    build: .\n".into(),
            "services.app.build",
        ),
        (
            "services:\n  app:\n    image: alpine:3.21\n".into(),
            "services.app.image",
        ),
        (
            format!("services:\n  app:\n    image: {PINNED_IMAGE}\n    ports: ['8080:8080']\n"),
            "services.app.ports",
        ),
        (
            format!(
                "services:\n  app:\n    image: {PINNED_IMAGE}\n    entrypoint: [/entrypoint.sh]\n"
            ),
            "services.app.entrypoint",
        ),
        (
            format!(
                "services:\n  app:\n    image: {PINNED_IMAGE}\n    command: [./serve, --port, '8080']\n"
            ),
            "services.app.command",
        ),
        (
            format!(
                "services:\n  app:\n    image: {PINNED_IMAGE}\n    volumes: [cache:/cache]\nvolumes:\n  cache:\n"
            ),
            "volumes",
        ),
        (
            format!(
                "services:\n  app:\n    image: {PINNED_IMAGE}\n    volumes: [./src:/workspace/src]\n    working_dir: /not-mounted\n"
            ),
            "services.app.working_dir",
        ),
    ];
    for (source, path) in cases {
        let error = parse_and_translate_compose_yaml(&source).unwrap_err();
        assert_eq!(error.code, ComposeErrorCode::Unsupported, "{source}");
        assert_eq!(error.path, path, "{source}");
    }
}
