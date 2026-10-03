use super::*;
#[test]
fn context_records_are_ordered_and_typed() {
    let a = ContextObservation {
        materialization_root: "build".into(),
        entries: vec![
            ContextEntry::Directory {
                path: "empty".into(),
            },
            ContextEntry::Symlink {
                path: "link".into(),
                target: "a".into(),
            },
            ContextEntry::File {
                path: "a".into(),
                bytes: b"x".to_vec(),
                executable: false,
            },
        ],
    };
    let mut b = a.clone();
    b.entries.reverse();
    assert_eq!(
        content_digest(&manifest(), manifest().stack("s").unwrap(), &a),
        content_digest(&manifest(), manifest().stack("s").unwrap(), &b)
    );
}
#[test]
fn external_platform_is_identity_significant() {
    let a = final_generation(
        &format!("sha256:{}", "c".repeat(64)),
        &[ExternalImageIdentity {
            reference: "alpine".into(),
            platform: Some("linux/amd64".into()),
            identity: None,
        }],
        &[ExternalImageIdentity {
            reference: "alpine".into(),
            platform: Some("linux/amd64".into()),
            identity: Some(format!("sha256:{}", "a".repeat(64))),
        }],
    )
    .unwrap();
    let b = final_generation(
        &format!("sha256:{}", "c".repeat(64)),
        &[ExternalImageIdentity {
            reference: "alpine".into(),
            platform: Some("linux/arm64".into()),
            identity: None,
        }],
        &[ExternalImageIdentity {
            reference: "alpine".into(),
            platform: Some("linux/arm64".into()),
            identity: Some(format!("sha256:{}", "a".repeat(64))),
        }],
    )
    .unwrap();
    assert_ne!(a, b);
}
#[test]
fn resolver_requires_exact_complete_unique_immutable_receipts() {
    let required = vec![ExternalImageIdentity {
        reference: "base".into(),
        platform: Some("linux/amd64".into()),
        identity: None,
    }];
    let valid = ExternalImageIdentity {
        reference: "base".into(),
        platform: Some("linux/amd64".into()),
        identity: Some(format!("sha256:{}", "b".repeat(64))),
    };
    assert_eq!(
        validate_resolved_images(&required, std::slice::from_ref(&valid)).unwrap(),
        vec![valid.clone()]
    );
    assert!(matches!(
        validate_resolved_images(&required, &[]),
        Err(GenerationError::MissingExternalImage { .. })
    ));
    assert!(matches!(
        validate_resolved_images(
            &required,
            &[ExternalImageIdentity {
                identity: Some("sha256:short".into()),
                ..valid.clone()
            }]
        ),
        Err(GenerationError::InvalidExternalIdentity { .. })
    ));
    assert!(matches!(
        validate_resolved_images(&required, &[valid.clone(), valid]),
        Err(GenerationError::DuplicateExternalImage { .. })
    ));
}
#[test]
fn stack_entry_point_binds_real_root_and_requires_immutable_image_receipt() {
    let root = tempfile::tempdir().unwrap();
    let root_name = root.path().to_str().unwrap();
    let manifest = bosn_core::parse_manifest_toml(
        "[stack.s]\nimage='alpine'",
        bosn_core::ManifestRoots::new("m", root_name, "workspace"),
    )
    .unwrap();
    let stack = manifest.stack("s").unwrap();
    let limits = collector::CollectorLimits::default();
    let observed = ExternalImageIdentity {
        reference: "alpine".into(),
        platform: None,
        identity: Some(format!("sha256:{}", "a".repeat(64))),
    };
    assert!(
        stack_generation(
            &manifest,
            stack,
            root.path(),
            &limits,
            std::slice::from_ref(&observed)
        )
        .is_ok()
    );
    assert!(stack_generation(&manifest, stack, root.path(), &limits, &[]).is_err());
    assert!(
        stack_generation(
            &manifest,
            stack,
            root.path(),
            &limits,
            &[observed.clone(), observed]
        )
        .is_err()
    );
}
#[cfg(unix)]
#[test]
fn stack_entry_normalizes_a_manifest_symlink_root_before_digesting() {
    let root = tempfile::tempdir().unwrap();
    let holder = tempfile::tempdir().unwrap();
    let link = holder.path().join("materialized");
    std::os::unix::fs::symlink(root.path(), &link).unwrap();
    let manifest = bosn_core::parse_manifest_toml(
        "[stack.s]\nimage='alpine'",
        bosn_core::ManifestRoots::new("m", link.to_str().unwrap(), "workspace"),
    )
    .unwrap();
    let observed = ExternalImageIdentity {
        reference: "alpine".into(),
        platform: None,
        identity: Some(format!("sha256:{}", "a".repeat(64))),
    };
    assert!(
        stack_generation(
            &manifest,
            manifest.stack("s").unwrap(),
            root.path(),
            &collector::CollectorLimits::default(),
            &[observed]
        )
        .is_ok()
    );
}
#[test]
fn stack_generation_real_roots_selected_and_resolver_receipts() {
    fn setup() -> (tempfile::TempDir, Manifest) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Dockerfile"),
            "FROM busybox\nCOPY keep /x\n",
        )
        .unwrap();
        std::fs::write(root.path().join("keep"), "a").unwrap();
        std::fs::write(root.path().join("skip"), "z").unwrap();
        let manifest = bosn_core::parse_manifest_toml(
            "[stack.s]\ndockerfile='Dockerfile'",
            bosn_core::ManifestRoots::new("m", root.path().to_str().unwrap(), "workspace"),
        )
        .unwrap();
        (root, manifest)
    }
    let (one, first) = setup();
    let (two, second) = setup();
    let receipt = ExternalImageIdentity {
        reference: "busybox".into(),
        platform: None,
        identity: Some(format!("sha256:{}", "b".repeat(64))),
    };
    let limits = collector::CollectorLimits::default();
    let a = stack_generation(
        &first,
        first.stack("s").unwrap(),
        one.path(),
        &limits,
        std::slice::from_ref(&receipt),
    )
    .unwrap();
    let b = stack_generation(
        &second,
        second.stack("s").unwrap(),
        two.path(),
        &limits,
        std::slice::from_ref(&receipt),
    )
    .unwrap();
    assert_eq!(a, b);
    std::fs::write(one.path().join("skip"), "changed").unwrap();
    assert_eq!(
        a,
        stack_generation(
            &first,
            first.stack("s").unwrap(),
            one.path(),
            &limits,
            std::slice::from_ref(&receipt)
        )
        .unwrap()
    );
    assert!(matches!(
        stack_generation(&first, first.stack("s").unwrap(), one.path(), &limits, &[]),
        Err(StackGenerationError::Generation(
            GenerationError::MissingExternalImage { .. }
        ))
    ));
    assert!(matches!(
        stack_generation(
            &first,
            first.stack("s").unwrap(),
            one.path(),
            &limits,
            &[ExternalImageIdentity {
                reference: "busybox".into(),
                platform: None,
                identity: Some("sha256:x".into())
            }]
        ),
        Err(StackGenerationError::Generation(
            GenerationError::InvalidExternalIdentity { .. }
        ))
    ));
    std::fs::write(one.path().join("keep"), "b").unwrap();
    assert_ne!(
        a,
        stack_generation(
            &first,
            first.stack("s").unwrap(),
            one.path(),
            &limits,
            &[receipt]
        )
        .unwrap()
    );
}
#[test]
fn stack_generation_create_time_fields_roll_but_workdir_and_tasks_do_not() {
    let root = tempfile::tempdir().unwrap();
    let root_name = root.path().to_str().unwrap();
    let manifest = bosn_core::parse_manifest_toml("[stack.s]\nimage='alpine'\nworkdir='/one'\n[stack.s.env]\nA='one'\n[stack.s.volumes.v]\n[task.t]\nstack='s'\ncmd='one'", bosn_core::ManifestRoots::new("m", root_name, "workspace-one")).unwrap();
    let receipt = ExternalImageIdentity {
        reference: "alpine".into(),
        platform: None,
        identity: Some(format!("sha256:{}", "a".repeat(64))),
    };
    let limits = collector::CollectorLimits::default();
    let base = stack_generation(
        &manifest,
        manifest.stack("s").unwrap(),
        root.path(),
        &limits,
        std::slice::from_ref(&receipt),
    )
    .unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("bind"), "one").unwrap();
    let material = tempfile::tempdir().unwrap();
    std::fs::write(
        material.path().join("Dockerfile"),
        "FROM alpine\nCOPY keep /x\n",
    )
    .unwrap();
    std::fs::write(material.path().join("keep"), "x").unwrap();
    let bound = bosn_core::parse_manifest_toml(&format!("[stack.s]\ndockerfile='Dockerfile'\n[stack.s.mounts.bind]\nsource='{}'\ndestination='/bind'", workspace.path().display()), bosn_core::ManifestRoots::new("m", material.path().to_str().unwrap(), workspace.path().to_str().unwrap())).unwrap();
    let alpine = ExternalImageIdentity {
        reference: "alpine".into(),
        platform: None,
        identity: Some(format!("sha256:{}", "a".repeat(64))),
    };
    let before_bind = stack_generation(
        &bound,
        bound.stack("s").unwrap(),
        material.path(),
        &limits,
        std::slice::from_ref(&alpine),
    )
    .unwrap();
    std::fs::write(workspace.path().join("bind"), "two").unwrap();
    assert_eq!(
        before_bind,
        stack_generation(
            &bound,
            bound.stack("s").unwrap(),
            material.path(),
            &limits,
            &[alpine]
        )
        .unwrap()
    );
    let mut task = manifest.clone();
    task.tasks.get_mut("t").unwrap().cmd = "two".into();
    assert_eq!(
        base,
        stack_generation(
            &task,
            task.stack("s").unwrap(),
            root.path(),
            &limits,
            std::slice::from_ref(&receipt)
        )
        .unwrap()
    );
    let mut workdir = manifest.stack("s").unwrap().clone();
    workdir.workdir = Some("/two".into());
    assert_eq!(
        base,
        stack_generation(
            &manifest,
            &workdir,
            root.path(),
            &limits,
            std::slice::from_ref(&receipt)
        )
        .unwrap()
    );
    let mut env = manifest.stack("s").unwrap().clone();
    env.env.insert("A".into(), "two".into());
    assert_ne!(
        base,
        stack_generation(
            &manifest,
            &env,
            root.path(),
            &limits,
            std::slice::from_ref(&receipt)
        )
        .unwrap()
    );
    let mut retention = manifest.stack("s").unwrap().clone();
    retention.volumes[0].retention = bosn_core::Retention::Pinned;
    assert_ne!(
        base,
        stack_generation(
            &manifest,
            &retention,
            root.path(),
            &limits,
            std::slice::from_ref(&receipt)
        )
        .unwrap()
    );
    let mut guest = manifest.stack("s").unwrap().clone();
    guest.guest = Some(bosn_core::manifest::Guest {
        ssh_port: 22,
        ssh_user: "u".into(),
        ssh_host: "h".into(),
        web_port: 80,
        ready_timeout: 1,
        ready_poll_interval: 1,
        version: "v".into(),
        ram_size: "1G".into(),
        disk_size: "1G".into(),
        cpu_cores: None,
        payload: None,
        payload_destination: "/x".into(),
    });
    assert_ne!(
        base,
        stack_generation(
            &manifest,
            &guest,
            root.path(),
            &limits,
            std::slice::from_ref(&receipt)
        )
        .unwrap()
    );
}
#[test]
fn unresolved_is_only_valid_for_read_only_coalescing() {
    let image = ExternalImageIdentity {
        reference: "mutable".into(),
        platform: None,
        identity: None,
    };
    assert!(
        final_generation(
            &format!("sha256:{}", "c".repeat(64)),
            std::slice::from_ref(&image),
            std::slice::from_ref(&image)
        )
        .is_err()
    );
    assert_ne!(
        coalescing_generation("sha256:c", &[image]),
        coalescing_generation("sha256:c", &[])
    );
}
#[test]
fn docker_references_include_multistage_and_copy_from() {
    let refs = dockerfile::external_images(
        "FROM --platform=linux/amd64 base AS build\nFROM build\nCOPY --from=busybox /x /x\n",
    )
    .unwrap();
    assert_eq!(
        refs.iter()
            .map(|x| x.reference.as_str())
            .collect::<Vec<_>>(),
        ["base", "busybox"]
    );
}
fn manifest() -> Manifest {
    bosn_core::parse_manifest_toml(
        "[stack.s]\nimage='x'\n[stack.s.env]\nA='b'",
        bosn_core::ManifestRoots::new("m", "build", "workspace"),
    )
    .unwrap()
}

#[test]
fn a_files_execute_bit_is_identity_significant() {
    let observation = |executable| ContextObservation {
        materialization_root: "build".into(),
        entries: vec![ContextEntry::File {
            path: "tool".into(),
            bytes: b"#!/bin/sh\n".to_vec(),
            executable,
        }],
    };
    let digest =
        |o: &ContextObservation| content_digest(&manifest(), manifest().stack("s").unwrap(), o);
    assert_ne!(digest(&observation(false)), digest(&observation(true)));
}
