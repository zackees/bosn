//! Setup and manifest GC previews, repairs, rollovers and `setup done`.

mod common;
use common::*;

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn missing_setup_repair_is_exact_atomic_idempotent_and_protects_ambiguous_state() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let workspace = "/canonical/work";
    let id = "setup-container:missing";
    let name = "bosn-setup-missing";
    let generation = "sha256:missing";
    let mut tx = registry.begin_immediate().unwrap();
    tx.put_resource(&active_setup_container(id, name, workspace, generation))
        .unwrap();
    tx.put_resource_use(&active_setup_use(id, workspace, generation))
        .unwrap();
    tx.commit().unwrap();

    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.repair_missing_setup_container(workspace, id, name, generation, 2.0)
            .unwrap(),
        Some(SetupMissingRepair::Repaired)
    );
    tx.commit().unwrap();
    assert!(
        registry
            .resources(0, 10)
            .unwrap()
            .items
            .iter()
            .any(|r| r.id == id && r.state == ResourceState::Retired)
    );
    assert!(
        registry
            .resource_uses(0, 10)
            .unwrap()
            .items
            .iter()
            .any(|u| u.resource_id == id && u.state == ResourceState::Retired)
    );
    assert_eq!(
        registry
            .events(0, 10)
            .unwrap()
            .items
            .iter()
            .filter(|event| event.kind == "setup.reconcile.missing_repaired")
            .count(),
        1
    );
    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.repair_missing_setup_container(workspace, id, name, generation, 3.0)
            .unwrap(),
        Some(SetupMissingRepair::AlreadyRepaired)
    );
    drop(tx);
    assert_eq!(
        registry.events(0, 10).unwrap().items.len(),
        1,
        "repeat does not write"
    );

    // Foreign uses, leases, sessions, and identity/generation mismatch all
    // fail closed and dropping the transaction proves no partial state/event.
    for (suffix, foreign_use, lease, session) in [
        ("foreign", true, false, false),
        ("lease", false, true, false),
        ("session", false, false, true),
    ] {
        let candidate_id = format!("setup-container:{suffix}");
        let candidate_name = format!("bosn-setup-{suffix}");
        let mut tx = registry.begin_immediate().unwrap();
        tx.put_resource(&active_setup_container(
            &candidate_id,
            &candidate_name,
            workspace,
            generation,
        ))
        .unwrap();
        tx.put_resource_use(&active_setup_use(&candidate_id, workspace, generation))
            .unwrap();
        if foreign_use {
            tx.put_resource_use(&active_setup_use(&candidate_id, "/other/work", generation))
                .unwrap();
        }
        if lease {
            tx.put_lease(&Lease {
                id: format!("lease-{suffix}"),
                resource_id: candidate_id.clone(),
                pid: 1,
                proc_start: None,
                acquired_at: 1.0,
                heartbeat_at: 1.0,
                ttl_seconds: 1.0,
            })
            .unwrap();
        }
        if session {
            tx.put_execution_session(&ExecutionSession {
                id: format!("session-{suffix}"),
                container_id: candidate_name.clone(),
                engine_binary: "docker".into(),
                client_pid: 1,
                client_start: None,
                lease_ids: vec![],
            })
            .unwrap();
        }
        tx.commit().unwrap();
        let mut tx = registry.begin_immediate().unwrap();
        assert_eq!(
            tx.repair_missing_setup_container(
                workspace,
                &candidate_id,
                &candidate_name,
                generation,
                4.0
            )
            .unwrap(),
            None
        );
        drop(tx);
        assert!(
            registry
                .resources(0, 20)
                .unwrap()
                .items
                .iter()
                .any(|r| r.id == candidate_id && r.state == ResourceState::Active)
        );
    }
    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.repair_missing_setup_container(
            workspace,
            "setup-container:missing",
            name,
            "sha256:wrong",
            5.0
        )
        .unwrap(),
        None
    );
    drop(tx);

    let rollback_id = "setup-container:rollback";
    let rollback_name = "bosn-setup-rollback";
    let mut tx = registry.begin_immediate().unwrap();
    tx.put_resource(&active_setup_container(
        rollback_id,
        rollback_name,
        workspace,
        generation,
    ))
    .unwrap();
    tx.put_resource_use(&active_setup_use(rollback_id, workspace, generation))
        .unwrap();
    tx.commit().unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.repair_missing_setup_container(workspace, rollback_id, rollback_name, generation, 6.0)
            .unwrap(),
        Some(SetupMissingRepair::Repaired)
    );
    drop(tx);
    assert!(
        registry
            .resources(0, 32)
            .unwrap()
            .items
            .iter()
            .any(|r| r.id == rollback_id && r.state == ResourceState::Active)
    );
    assert_eq!(
        registry
            .events(0, 32)
            .unwrap()
            .items
            .iter()
            .filter(|event| event.kind == "setup.reconcile.missing_repaired")
            .count(),
        1,
        "dropped repair rolls back state and event"
    );
}

#[test]
fn setup_gc_preview_only_returns_unambiguously_retired_managed_containers() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for (id, name, state) in [
        (
            "setup-container:eligible",
            "bosn-setup-eligible",
            ResourceState::Retired,
        ),
        (
            "setup-container:active",
            "bosn-setup-active",
            ResourceState::Active,
        ),
        ("foreign", "foreign", ResourceState::Retired),
        (
            "setup-container:leased",
            "bosn-setup-leased",
            ResourceState::Retired,
        ),
        (
            "setup-container:session",
            "bosn-setup-session",
            ResourceState::Retired,
        ),
        (
            "setup-container:ambiguous",
            "bosn-setup-ambiguous",
            ResourceState::Retired,
        ),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Container,
            name: name.into(),
            stack: "setup".into(),
            generation: "g".into(),
            scope: Scope::Machine,
            workspace: "/work".into(),
            created_at: 1.0,
            last_used: 1.0,
            state,
            retention: Retention::Pinned,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: "/work".into(),
            stack: "setup".into(),
            generation: "g".into(),
            last_used: 1.0,
            state: if id == "setup-container:active" {
                ResourceState::Active
            } else {
                ResourceState::Retired
            },
        })
        .unwrap();
    }
    tx.put_resource_use(&ResourceUse {
        resource_id: "setup-container:ambiguous".into(),
        workspace: "/other".into(),
        stack: "setup".into(),
        generation: "g".into(),
        last_used: 1.0,
        state: ResourceState::Active,
    })
    .unwrap();
    tx.put_lease(&Lease {
        id: "lease".into(),
        resource_id: "setup-container:leased".into(),
        pid: 1,
        proc_start: None,
        acquired_at: 1.0,
        heartbeat_at: 1.0,
        ttl_seconds: 1.0,
    })
    .unwrap();
    tx.put_execution_session(&ExecutionSession {
        id: "session".into(),
        container_id: "bosn-setup-session".into(),
        engine_binary: "docker".into(),
        client_pid: 1,
        client_start: None,
        lease_ids: vec![],
    })
    .unwrap();
    tx.commit().unwrap();
    let preview = registry.setup_gc_preview("/work", 0, 1).unwrap();
    assert_eq!(preview.candidates.items.len(), 1);
    assert_eq!(preview.candidates.items[0].id, "setup-container:eligible");
    assert_eq!(preview.counts.protected_not_retired, 1);
    assert_eq!(preview.counts.protected_ambiguous_use, 1);
    assert_eq!(preview.counts.protected_lease, 1);
    assert_eq!(preview.counts.protected_session, 1);
    assert_eq!(preview.counts.excluded_unmanaged, 1);
}

#[test]
fn manifest_volume_gc_only_allows_retired_warm_spec_native_volumes() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for (id, name, scope, retention) in [
        (
            "manifest-volume:eligible",
            "bosn-v-spec-eligible",
            Scope::Spec,
            Retention::Warm,
        ),
        (
            "manifest-volume:pinned",
            "bosn-v-spec-pinned",
            Scope::Spec,
            Retention::Pinned,
        ),
        (
            "manifest-volume:machine",
            "bosn-v-machine-machine",
            Scope::Machine,
            Retention::Warm,
        ),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Volume,
            name: name.into(),
            stack: "app".into(),
            generation: "sha256:old".into(),
            scope,
            workspace: "/work".into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Retired,
            retention,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: "/work".into(),
            stack: "app".into(),
            generation: "sha256:old".into(),
            last_used: 1.0,
            state: ResourceState::Retired,
        })
        .unwrap();
    }
    tx.commit().unwrap();
    let preview = registry.manifest_volume_gc_preview("/work", 0, 16).unwrap();
    assert_eq!(
        preview
            .candidates
            .items
            .iter()
            .map(|v| v.id.as_str())
            .collect::<Vec<_>>(),
        vec!["manifest-volume:eligible"]
    );
    assert_eq!(preview.counts.protected_policy, 2);
    let candidate = preview.candidates.items[0].clone();
    let mut tx = registry.begin_immediate().unwrap();
    assert!(
        tx.finalize_manifest_volume_gc_candidate(
            "/work",
            &candidate.id,
            &candidate.name,
            &candidate.generation,
            2.0,
            "manifest.volume_gc.removed"
        )
        .unwrap()
    );
    tx.commit().unwrap();
    assert!(
        registry
            .resource_by_kind_name(ResourceKind::Volume, &candidate.name)
            .unwrap()
            .is_none()
    );
}

#[test]
fn manifest_volume_release_only_allows_unambiguous_active_durable_native_volumes() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for (id, name, scope, retention) in [
        (
            "manifest-volume:stack",
            "bosn-v-stack-release",
            Scope::Stack,
            Retention::Warm,
        ),
        (
            "manifest-volume:pinned",
            "bosn-v-spec-release",
            Scope::Spec,
            Retention::Pinned,
        ),
        (
            "manifest-volume:warm",
            "bosn-v-spec-warm",
            Scope::Spec,
            Retention::Warm,
        ),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Volume,
            name: name.into(),
            stack: "app".into(),
            generation: "sha256:release".into(),
            scope,
            workspace: "/work".into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: "/work".into(),
            stack: "app".into(),
            generation: "sha256:release".into(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    }
    tx.put_lease(&Lease {
        id: "lease".into(),
        resource_id: "manifest-volume:pinned".into(),
        pid: 1,
        proc_start: None,
        acquired_at: 1.0,
        heartbeat_at: 1.0,
        ttl_seconds: 30.0,
    })
    .unwrap();
    tx.commit().unwrap();
    let preview = registry
        .manifest_volume_release_preview("/work", 0, 16)
        .unwrap();
    assert_eq!(
        preview
            .items
            .iter()
            .map(|v| v.id.as_str())
            .collect::<Vec<_>>(),
        vec!["manifest-volume:stack"]
    );
    let candidate = preview.items[0].clone();
    let mut tx = registry.begin_immediate().unwrap();
    assert!(
        tx.finalize_manifest_volume_release_candidate(
            "/work",
            &candidate.id,
            &candidate.name,
            &candidate.generation,
            2.0,
            "manifest.volume_release.removed"
        )
        .unwrap()
    );
    tx.commit().unwrap();
    assert!(
        registry
            .resource_by_kind_name(ResourceKind::Volume, &candidate.name)
            .unwrap()
            .is_none()
    );
}

#[test]
fn manifest_volume_rollover_retires_only_warm_spec_data() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for (id, scope, retention) in [
        ("manifest-volume:spec", Scope::Spec, Retention::Warm),
        ("manifest-volume:pinned", Scope::Spec, Retention::Pinned),
        ("manifest-volume:stack", Scope::Stack, Retention::Warm),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Volume,
            name: format!("bosn-v-{}-{id}", scope.as_str()),
            stack: "app".into(),
            generation: "sha256:old".into(),
            scope,
            workspace: "/work".into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: "/work".into(),
            stack: "app".into(),
            generation: "sha256:old".into(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    }
    tx.retire_prior_manifest_warm_spec_volume_generations("/work", "app", &[])
        .unwrap();
    tx.commit().unwrap();
    let rows = registry.resources(0, 16).unwrap().items;
    assert_eq!(
        rows.iter()
            .find(|r| r.id == "manifest-volume:spec")
            .unwrap()
            .state,
        ResourceState::Retired
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.id == "manifest-volume:pinned")
            .unwrap()
            .state,
        ResourceState::Active
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.id == "manifest-volume:stack")
            .unwrap()
            .state,
        ResourceState::Active
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn manifest_generation_rollover_is_workspace_stack_scoped_and_keeps_sessions_protected() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let workspace_a = "/canonical/manifest-a";
    let workspace_b = "/canonical/manifest-b";
    let mut tx = registry.begin_immediate().unwrap();
    for (id, name, workspace, stack, generation) in [
        (
            "manifest-container:app:old",
            "bosn-setup-old",
            workspace_a,
            "app",
            "sha256:old",
        ),
        (
            "manifest-container:app:new",
            "bosn-setup-new",
            workspace_a,
            "app",
            "sha256:new",
        ),
        (
            "manifest-guest:app:old",
            "bosn-setup-guest-old",
            workspace_a,
            "app",
            "sha256:old-guest",
        ),
        (
            "manifest-container:app:other-workspace",
            "bosn-setup-other-workspace",
            workspace_b,
            "app",
            "sha256:other-workspace",
        ),
        (
            "manifest-container:other:other-stack",
            "bosn-setup-other-stack",
            workspace_a,
            "other",
            "sha256:other-stack",
        ),
        (
            "setup-container:setup",
            "bosn-setup-setup",
            workspace_a,
            "setup",
            "sha256:setup",
        ),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Container,
            name: name.into(),
            stack: stack.into(),
            generation: generation.into(),
            scope: Scope::Machine,
            workspace: workspace.into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: workspace.into(),
            stack: stack.into(),
            generation: generation.into(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    }
    tx.put_execution_session(&ExecutionSession {
        id: "uncertain-manifest-task".into(),
        container_id: "bosn-setup-old".into(),
        engine_binary: "docker".into(),
        client_pid: 1,
        client_start: None,
        lease_ids: vec![],
    })
    .unwrap();
    tx.put_execution_session(&ExecutionSession {
        id: "uncertain-manifest-guest".into(),
        container_id: "bosn-setup-guest-old".into(),
        engine_binary: "docker".into(),
        client_pid: 2,
        client_start: None,
        lease_ids: vec![],
    })
    .unwrap();
    tx.retire_prior_manifest_container_generations(workspace_a, "app", "sha256:new")
        .unwrap();
    tx.commit().unwrap();

    let resources = registry.resources(0, 16).unwrap().items;
    let state = |id: &str| {
        resources
            .iter()
            .find(|resource| resource.id == id)
            .unwrap()
            .state
    };
    assert_eq!(state("manifest-container:app:old"), ResourceState::Retired);
    assert_eq!(state("manifest-guest:app:old"), ResourceState::Retired);
    assert_eq!(state("manifest-container:app:new"), ResourceState::Active);
    assert_eq!(
        state("manifest-container:app:other-workspace"),
        ResourceState::Active
    );
    assert_eq!(
        state("manifest-container:other:other-stack"),
        ResourceState::Active
    );
    assert_eq!(state("setup-container:setup"), ResourceState::Active);
    let uses = registry.resource_uses(0, 16).unwrap().items;
    assert_eq!(
        uses.iter()
            .find(|use_| use_.resource_id == "manifest-container:app:old")
            .unwrap()
            .state,
        ResourceState::Retired
    );
    // A rollover is registry-only. The uncertain task session survives and
    // keeps the retired generation out of conservative GC until cleared.
    assert_eq!(registry.status().unwrap().sessions, 2);
    let protected = registry.setup_gc_preview(workspace_a, 0, 16).unwrap();
    assert!(protected.candidates.items.is_empty());
    assert_eq!(protected.counts.protected_session, 2);
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn setup_done_is_workspace_isolated_idempotent_and_preserves_shared_resources() {
    let (_directory, path) = database_path();
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    for (id, workspace) in [
        ("shared", "/canonical/a"),
        ("only-a", "/canonical/a"),
        ("only-b", "/canonical/b"),
    ] {
        tx.put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Image,
            name: format!("image-{id}"),
            stack: "setup".into(),
            generation: "g".into(),
            scope: Scope::Machine,
            workspace: workspace.into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        })
        .unwrap();
        tx.put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: workspace.into(),
            stack: "setup".into(),
            generation: "g".into(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    }
    // This foreign active use protects the machine-global resource state.
    tx.put_resource_use(&ResourceUse {
        resource_id: "shared".into(),
        workspace: "/canonical/b".into(),
        stack: "setup".into(),
        generation: "g2".into(),
        last_used: 1.0,
        state: ResourceState::Active,
    })
    .unwrap();
    // A non-setup use is never selected.
    tx.put_resource_use(&ResourceUse {
        resource_id: "only-a".into(),
        workspace: "/canonical/a".into(),
        stack: "other".into(),
        generation: "g3".into(),
        last_used: 1.0,
        state: ResourceState::Active,
    })
    .unwrap();
    tx.commit().unwrap();

    let mut rollback = registry.begin_immediate().unwrap();
    assert_eq!(
        rollback
            .complete_setup_workspace("/canonical/a", 2.0)
            .unwrap()
            .uses_completed,
        2
    );
    drop(rollback);
    assert!(
        registry
            .resource_uses(0, 20)
            .unwrap()
            .items
            .iter()
            .filter(|use_row| use_row.workspace == "/canonical/a" && use_row.stack == "setup")
            .all(|use_row| use_row.state == ResourceState::Active)
    );
    assert!(registry.events(0, 20).unwrap().items.is_empty());

    let mut tx = registry.begin_immediate().unwrap();
    let result = tx.complete_setup_workspace("/canonical/a", 2.0).unwrap();
    assert_eq!(result.uses_completed, 2);
    assert_eq!(result.resources_completed, 0); // shared + other-stack active uses protect both
    tx.commit().unwrap();
    let uses = registry.resource_uses(0, 20).unwrap().items;
    assert!(
        uses.iter()
            .any(|u| u.resource_id == "only-b" && u.state == ResourceState::Active)
    );
    assert!(uses.iter().any(|u| u.resource_id == "only-a"
        && u.stack == "setup"
        && u.state == ResourceState::Done));
    assert!(uses.iter().any(|u| u.resource_id == "only-a"
        && u.stack == "other"
        && u.state == ResourceState::Active));
    assert!(
        registry
            .resources(0, 20)
            .unwrap()
            .items
            .iter()
            .all(|r| r.state == ResourceState::Active)
    );
    // Completion is not a GC retirement transition, so it cannot make a
    // candidate collectible merely by setting a use to done.
    assert!(
        registry
            .setup_gc_preview("/canonical/a", 0, 10)
            .unwrap()
            .candidates
            .items
            .is_empty()
    );
    let events = registry.events(0, 20).unwrap().items;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == "setup.done")
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .all(|event| !event.detail.contains("/canonical"))
    );

    let mut tx = registry.begin_immediate().unwrap();
    assert_eq!(
        tx.complete_setup_workspace("/canonical/a", 3.0)
            .unwrap()
            .uses_completed,
        0
    );
    // Do not commit an idempotent no-op: no second event or timestamp write.
    drop(tx);
    assert_eq!(
        registry
            .events(0, 20)
            .unwrap()
            .items
            .iter()
            .filter(|event| event.kind == "setup.done")
            .count(),
        1
    );

    // A following ensure upsert can reactivate the previously done use/resource.
    let mut reactivated = registry
        .resources(0, 20)
        .unwrap()
        .items
        .into_iter()
        .find(|r| r.id == "shared")
        .unwrap();
    reactivated.state = ResourceState::Active;
    let mut tx = registry.begin_immediate().unwrap();
    tx.put_resource(&reactivated).unwrap();
    tx.put_resource_use(&ResourceUse {
        resource_id: "shared".into(),
        workspace: "/canonical/a".into(),
        stack: "setup".into(),
        generation: "g".into(),
        last_used: 4.0,
        state: ResourceState::Active,
    })
    .unwrap();
    tx.commit().unwrap();
    assert!(
        registry
            .resource_uses(0, 20)
            .unwrap()
            .items
            .iter()
            .any(|u| u.resource_id == "shared"
                && u.workspace == "/canonical/a"
                && u.state == ResourceState::Active)
    );
}
