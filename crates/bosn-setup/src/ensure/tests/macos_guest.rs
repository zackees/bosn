//! Typed macOS guest plans and the documented example image.

use super::*;

/// Extracts the first ```toml fenced block from docs/macos-guest.md. Read at
/// run time (not include_str!) so builds from a package without docs still compile.
fn macos_guest_doc_example() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/macos-guest.md");
    let doc = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let fence = "```toml\n";
    let (_, after) = doc.split_once(fence).expect("toml example");
    let (example, _) = after.split_once("```").expect("closed fence");
    example.to_owned()
}

/// Replaces a documentation placeholder such as `<64 lowercase hex>` in the
/// digest position with a real-shaped digest, leaving a literal digest alone.
fn substitute_doc_digest_placeholder(example: &str) -> String {
    let mut out = String::new();
    let mut rest = example;
    while let Some((before, after)) = rest.split_once("@sha256:<") {
        let (_, tail) = after.split_once('>').expect("placeholder is closed");
        out.push_str(before);
        out.push_str("@sha256:");
        out.push_str(HASH);
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[test]
fn macos_guest_doc_example_image_is_accepted_by_runtime() {
    let raw = macos_guest_doc_example();
    let example = substitute_doc_digest_placeholder(&raw);
    let manifest = bosn_core::parse_manifest_toml(
        &example,
        bosn_core::ManifestRoots::new("docs/macos-guest.md", "assets", "workspace"),
    )
    .unwrap_or_else(|error| panic!("docs/macos-guest.md example must parse: {error:?}"));
    let stack = manifest.stack("macos-x64").expect("stack macos-x64");
    assert_eq!(stack.kind.as_deref(), Some("macos-x64-guest"));
    assert!(stack.acknowledge_macos_license);
    let image = stack.image.as_deref().expect("example declares an image");
    assert!(
        valid_macos_guest_image(image),
        "docs/macos-guest.md example image {image:?} is refused by valid_macos_guest_image"
    );
    let storage = stack
        .volumes
        .iter()
        .find(|volume| volume.name == "storage")
        .expect("example declares the storage volume");
    assert_eq!(storage.scope, Scope::Machine);
    assert_eq!(storage.destination.as_deref(), Some("/storage"));
    assert_eq!(storage.retention, Retention::Pinned);
}

#[test]
fn macos_guest_doc_digest_substitution_only_fills_placeholders() {
    let placeholder = "image = \"dockurr/macos@sha256:<64 lowercase hex>\"";
    assert_eq!(
        substitute_doc_digest_placeholder(placeholder),
        format!("image = \"dockurr/macos@sha256:{HASH}\"")
    );
    let literal = format!("image = \"dockurr/macos@sha256:{HASH}\"");
    assert_eq!(substitute_doc_digest_placeholder(&literal), literal);
    // The substitution must not make a foreign registry acceptable.
    assert!(!valid_macos_guest_image(
        "ghcr.io/o/r/macos-x64-guest:ventura"
    ));
    assert!(!valid_macos_guest_image(&format!(
        "ghcr.io/o/r/macos-x64-guest@sha256:{HASH}"
    )));
}

#[test]
fn typed_macos_guest_emits_only_its_fixed_privileged_runtime_shape() {
    let command = SetupEnsureCommand::Create {
        container_name: "bosn-setup-test".into(),
        image_identity: IDENTITY.into(),
        mounts: Vec::new(),
        volumes: Vec::new(),
        tmpfs: Vec::new(),
        host_docker_socket: None,
        environment: BTreeMap::new(),
        workdir: None,
        command: Some("must-not-be-emitted".into()),
        labels: BTreeMap::new(),
        macos_guest: Box::new(Some(SetupEnsureMacosGuest {
            ssh_port: 2222,
            web_port: 8006,
            version: "ventura".into(),
            ram_size: "8G".into(),
            disk_size: "128G".into(),
            cpu_cores: 1,
        })),
    };
    let args = command.docker_args();
    for expected in [
        "/dev/kvm",
        "/dev/net/tun",
        "NET_ADMIN",
        "127.0.0.1:2222:22",
        "127.0.0.1:8006:8006",
        "VERSION=ventura",
        "RAM_SIZE=8G",
        "DISK_SIZE=128G",
        "CPU_CORES=1",
    ] {
        assert!(
            args.iter().any(|value| value == expected),
            "missing {expected}"
        );
    }
    assert!(!args.iter().any(|value| value == "must-not-be-emitted"));
}

#[test]
fn macos_guest_plan_requires_trusted_image_and_exact_durable_storage_receipt() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let plan = macos_guest_plan(&workspace);
    assert!(validate_plan_shape(&plan).is_ok());

    let mut untrusted_image = plan.clone();
    let image = format!("registry.example/dockurr/macos@sha256:{HASH}");
    untrusted_image.app.source = bosn_core::SetupSource::PinnedImage(image.clone());
    untrusted_image.app_source = SetupPlanAppSource::PinnedImage { image };
    assert!(matches!(
        validate_plan_shape(&untrusted_image),
        Err(SetupEnsureError::InvalidRequest(
            "macOS guest receipt was modified"
        ))
    ));

    let mut missing_storage = plan.clone();
    missing_storage.named_volumes.clear();
    assert!(matches!(
        validate_plan_shape(&missing_storage),
        Err(SetupEnsureError::InvalidRequest(
            "macOS guest storage volume receipt was modified"
        ))
    ));

    let mut unsafe_storage = plan;
    unsafe_storage.macos_guest.as_mut().unwrap().storage_scope = Scope::Stack;
    assert!(matches!(
        validate_plan_shape(&unsafe_storage),
        Err(SetupEnsureError::InvalidRequest(
            "macOS guest storage volume receipt was modified"
        ))
    ));
}
