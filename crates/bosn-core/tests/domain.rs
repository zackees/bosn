use bosn_core::*;

#[test]
fn finite_timestamp_subtraction_overflow_never_authorizes_cleanup() {
    let mut container = resource(ResourceKind::Container);
    container.last_used = -f64::MAX;
    let config = RetentionConfig::default();
    assert!(!container_should_stop(&container, f64::MAX, config));
    let input = EvaluationInput {
        now: f64::MAX,
        resource: container,
        leases: vec![],
        signals: RetentionSignals {
            workspace_done: true,
            superseded: true,
        },
        pressure: Pressure::default(),
        config,
        running_containers: Some(vec![]),
    };
    assert!(!evaluate(&input).collect);
    let lease = LeaseSnapshot {
        id: "lease".into(),
        resource_id: "r".into(),
        pid: 1,
        proc_start: None,
        heartbeat_at: -f64::MAX,
        ttl_seconds: 1.0,
    };
    assert!(!lease_expired(&lease, f64::MAX, Liveness::ConfirmedDead));
}

#[test]
fn changing_legacy_labels_to_pinned_cannot_drop_durable_pin() {
    let mut labels = ResourceLabels::new(
        "ours",
        ResourceKind::Volume,
        "stack",
        "generation",
        Scope::Spec,
        "workspace",
        "created",
        None,
    )
    .unwrap();
    labels.retention = Retention::Pinned;
    let rendered = labels.to_map();
    assert_eq!(
        rendered.get(LABEL_RETENTION).map(String::as_str),
        Some("pinned")
    );
    assert_eq!(
        ResourceLabels::parse(rendered).unwrap().retention,
        Retention::Pinned
    );
}

#[test]
fn container_idle_stop_uses_only_explicit_time_and_config() {
    let config = RetentionConfig {
        container_idle_stop: 10.0,
        ..RetentionConfig::default()
    };
    let container = resource(ResourceKind::Container);
    assert!(!container_should_stop(&container, 9.0, config));
    assert!(container_should_stop(&container, 10.0, config));
    assert!(!container_should_stop(
        &resource(ResourceKind::Volume),
        99.0,
        config
    ));
    assert!(!container_should_stop(&container, f64::NAN, config));
    let mut future = container;
    future.last_used = 11.0;
    assert!(!container_should_stop(&future, 10.0, config));
}

fn resource(kind: ResourceKind) -> ResourceSnapshot {
    ResourceSnapshot {
        id: "r".into(),
        name: "n".into(),
        kind,
        stack: "s".into(),
        generation: "g".into(),
        scope: Scope::Spec,
        workspace: "w".into(),
        created_at: 0.0,
        last_used: 0.0,
        state: ResourceState::Active,
        retention: Retention::Warm,
    }
}

#[test]
fn complete_matching_labels_are_the_only_ownership_proof() {
    let labels = ResourceLabels::new(
        "ours",
        ResourceKind::Volume,
        "s",
        "g",
        Scope::Spec,
        "w",
        "created",
        None,
    )
    .unwrap();
    assert!(labels.is_owned_by("ours"));
    assert!(!labels.is_owned_by("theirs"));
    assert!(ResourceLabels::parse(labels.to_map()).is_ok());
    assert!(ResourceLabels::parse([(LABEL_REGISTRY, "ours")]).is_err());
    assert!(!ownership_from_labels(
        [("com.zackees.bosn.name", "bosn-cache")],
        "ours"
    ));
}

#[test]
fn absent_legacy_retention_is_warm_but_unknown_is_conservative() {
    let warm = ResourceLabels::new(
        "ours",
        ResourceKind::Volume,
        "s",
        "g",
        Scope::Spec,
        "w",
        "c",
        None,
    )
    .unwrap();
    assert_eq!(warm.retention, Retention::Warm);
    assert!(!warm.retention_explicit);
    let explicit = ResourceLabels::new(
        "ours",
        ResourceKind::Volume,
        "s",
        "g",
        Scope::Spec,
        "w",
        "c",
        Some(Retention::Warm),
    )
    .unwrap();
    assert_eq!(
        ResourceLabels::parse(explicit.to_map()).unwrap().to_map(),
        explicit.to_map()
    );
    let mut bad = warm.to_map();
    bad.insert(LABEL_RETENTION, "mystery".into());
    assert!(ResourceLabels::parse(bad).is_err());
}

#[test]
fn lease_requires_expired_ttl_and_confirmed_dead_identity() {
    let lease = LeaseSnapshot {
        id: "l".into(),
        resource_id: "r".into(),
        pid: 7,
        proc_start: Some(1.0),
        heartbeat_at: 0.0,
        ttl_seconds: 10.0,
    };
    assert!(!lease_expired(&lease, 10.0, Liveness::ConfirmedDead));
    assert!(!lease_expired(
        &lease,
        11.0,
        Liveness::Alive {
            observed_start: Some(1.0)
        }
    ));
    assert!(!lease_expired(&lease, 11.0, Liveness::Unknown));
    assert!(!lease_expired(
        &lease,
        11.0,
        Liveness::Alive {
            observed_start: Some(2.0)
        }
    ));
    assert!(lease_expired(&lease, 11.0, Liveness::ConfirmedDead));
}

#[test]
fn retention_safety_precedence_and_pure_inputs() {
    let mut r = resource(ResourceKind::Volume);
    r.retention = Retention::Pinned;
    let input = EvaluationInput {
        now: 999999.0,
        resource: r,
        leases: vec![],
        signals: RetentionSignals {
            workspace_done: true,
            ..Default::default()
        },
        pressure: Pressure::default(),
        config: RetentionConfig::default(),
        running_containers: Some(vec![]),
    };
    assert_eq!(evaluate(&input).reason, VerdictReason::KeptPinned);
    let mut leased = input.clone();
    leased.resource.retention = Retention::Warm;
    leased.leases.push(ObservedLease {
        lease: LeaseSnapshot {
            id: "l".into(),
            resource_id: "r".into(),
            pid: 1,
            proc_start: None,
            heartbeat_at: 999999.0,
            ttl_seconds: 10.0,
        },
        liveness: Liveness::Unknown,
    });
    assert_eq!(evaluate(&leased).reason, VerdictReason::KeptLeased);
}

#[test]
fn retention_handles_all_collection_paths_and_unknown_observations() {
    let config = RetentionConfig::default();
    let idle = EvaluationInput {
        now: config.warm_volume_ttl + 1.0,
        resource: resource(ResourceKind::Volume),
        leases: vec![],
        signals: RetentionSignals::default(),
        pressure: Pressure::default(),
        config,
        running_containers: Some(vec![]),
    };
    assert_eq!(evaluate(&idle).reason, VerdictReason::CollectIdle);
    let mut image = idle.clone();
    image.resource.kind = ResourceKind::Image;
    image.signals.superseded = true;
    assert_eq!(
        evaluate(&image).reason,
        VerdictReason::CollectSupersededImage
    );
    let mut current = image.clone();
    current.signals.superseded = false;
    current.pressure.under_pressure = true;
    assert_eq!(evaluate(&current).reason, VerdictReason::KeptCurrentImage);
    let mut running = idle.clone();
    running.resource.kind = ResourceKind::Container;
    running.resource.name = "run".into();
    running.running_containers = None;
    assert_eq!(evaluate(&running).reason, VerdictReason::KeptRunning);
}

#[test]
fn pressure_and_order_are_deterministic_and_machine_last() {
    let p = Pressure::assess(5, 200, 9, 4, 100, 10, false);
    assert!(
        p.count_exceeded
            && p.free_space_exceeded
            && p.under_pressure
            && p.bytes_unknown
            && !p.bytes_exceeded
    );
    let mut a = resource(ResourceKind::Volume);
    a.name = "z".into();
    a.last_used = 5.0;
    let mut b = a.clone();
    b.name = "a".into();
    b.scope = Scope::Machine;
    let ordered = collectable_ordered(vec![
        Verdict::collect(a, VerdictReason::CollectPressure),
        Verdict::collect(b, VerdictReason::CollectSuperseded),
    ]);
    assert_eq!(
        ordered
            .iter()
            .map(|v| v.resource.name.as_str())
            .collect::<Vec<_>>(),
        vec!["z", "a"]
    );
    let mut first = resource(ResourceKind::Volume);
    first.name = "first".into();
    first.last_used = f64::NAN;
    let mut second = first.clone();
    second.name = "second".into();
    let ties = collectable_ordered(vec![
        Verdict::collect(first, VerdictReason::CollectPressure),
        Verdict::collect(second, VerdictReason::CollectPressure),
    ]);
    assert_eq!(
        ties.iter()
            .map(|v| v.resource.name.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
}

#[test]
fn shared_consumer_signal_requires_every_consumer_to_be_inactive() {
    assert_eq!(retention_signals([]), RetentionSignals::default());
    assert_eq!(
        retention_signals([
            ConsumerUse {
                done: true,
                superseded: false
            },
            ConsumerUse {
                done: false,
                superseded: false
            }
        ]),
        RetentionSignals::default()
    );
    assert_eq!(
        retention_signals([
            ConsumerUse {
                done: true,
                superseded: false
            },
            ConsumerUse {
                done: false,
                superseded: true
            }
        ]),
        RetentionSignals {
            superseded: true,
            workspace_done: false
        }
    );
    assert_eq!(
        retention_signals([ConsumerUse {
            done: true,
            superseded: false
        }]),
        RetentionSignals {
            superseded: false,
            workspace_done: true
        }
    );
}

#[test]
fn invalid_times_and_missing_lease_observations_never_collect() {
    let config = RetentionConfig::default();
    let mut input = EvaluationInput {
        now: f64::NAN,
        resource: resource(ResourceKind::Volume),
        leases: vec![],
        signals: RetentionSignals {
            workspace_done: true,
            ..Default::default()
        },
        pressure: Pressure::default(),
        config,
        running_containers: Some(vec![]),
    };
    assert!(!evaluate(&input).collect);
    input.now = 100.0;
    input.resource.last_used = f64::INFINITY;
    assert!(!evaluate(&input).collect);
    input.resource.last_used = 0.0;
    input.leases.push(ObservedLease {
        lease: LeaseSnapshot {
            id: "l".into(),
            resource_id: "r".into(),
            pid: 1,
            proc_start: None,
            heartbeat_at: 0.0,
            ttl_seconds: 1.0,
        },
        liveness: Liveness::Unknown,
    });
    assert_eq!(evaluate(&input).reason, VerdictReason::KeptLeased);
}

#[test]
fn retention_thresholds_and_per_lease_mixed_liveness_are_safe() {
    let config = RetentionConfig::default();
    let base = EvaluationInput {
        now: 0.0,
        resource: resource(ResourceKind::Container),
        leases: vec![],
        signals: RetentionSignals::default(),
        pressure: Pressure::default(),
        config,
        running_containers: Some(vec![]),
    };
    let mut container = base.clone();
    container.now = config.container_remove - 1.0;
    assert_eq!(evaluate(&container).reason, VerdictReason::KeptWarm);
    container.now += 1.0;
    assert_eq!(evaluate(&container).reason, VerdictReason::CollectIdle);
    let mut network = container.clone();
    network.resource.kind = ResourceKind::Network;
    assert_eq!(evaluate(&network).reason, VerdictReason::CollectIdle);
    let mut volume = container.clone();
    volume.resource.kind = ResourceKind::Volume;
    assert_eq!(evaluate(&volume).reason, VerdictReason::KeptWarm);
    let mut superseded = volume.clone();
    superseded.signals.superseded = true;
    superseded.now = config.superseded_cap - 1.0;
    assert_eq!(evaluate(&superseded).reason, VerdictReason::KeptWarm);
    superseded.now += 1.0;
    assert_eq!(
        evaluate(&superseded).reason,
        VerdictReason::CollectSuperseded
    );
    let mut machine_done = superseded.clone();
    machine_done.resource.scope = Scope::Machine;
    machine_done.signals = RetentionSignals {
        workspace_done: true,
        superseded: false,
    };
    machine_done.now = config.warm_volume_ttl + 1.0;
    assert_eq!(
        evaluate(&machine_done).reason,
        VerdictReason::KeptMachineScope
    );
    machine_done.pressure.under_pressure = true;
    assert_eq!(evaluate(&machine_done).reason, VerdictReason::CollectIdle);
    let mut leases = base;
    leases.now = 99.0;
    leases.leases = vec![
        ObservedLease {
            lease: LeaseSnapshot {
                id: "dead".into(),
                resource_id: "r".into(),
                pid: 1,
                proc_start: None,
                heartbeat_at: 0.0,
                ttl_seconds: 1.0,
            },
            liveness: Liveness::ConfirmedDead,
        },
        ObservedLease {
            lease: LeaseSnapshot {
                id: "unknown".into(),
                resource_id: "r".into(),
                pid: 2,
                proc_start: None,
                heartbeat_at: 0.0,
                ttl_seconds: 1.0,
            },
            liveness: Liveness::Unknown,
        },
    ];
    assert_eq!(evaluate(&leases).reason, VerdictReason::KeptLeased);
}

#[test]
fn adoption_quiet_period_is_exact_and_running_precedes_it() {
    let config = RetentionConfig::default();
    let mut input = EvaluationInput {
        now: config.quiet_period - 1.0,
        resource: resource(ResourceKind::Volume),
        leases: vec![],
        signals: RetentionSignals::default(),
        pressure: Pressure::default(),
        config,
        running_containers: Some(vec![]),
    };
    input.resource.state = ResourceState::Adopted;
    assert_eq!(evaluate(&input).reason, VerdictReason::KeptQuietPeriod);
    input.now += 1.0;
    assert_ne!(evaluate(&input).reason, VerdictReason::KeptQuietPeriod);
    input.resource.kind = ResourceKind::Container;
    input.now = 1.0;
    input.running_containers = Some(vec![input.resource.name.clone()]);
    assert_eq!(evaluate(&input).reason, VerdictReason::KeptRunning);
}
