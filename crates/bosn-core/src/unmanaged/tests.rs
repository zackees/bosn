use super::*;

/// `docker system df -v --format json` from a small engine, written once.
const SYSTEM_DF_MINIMAL: &str = include_str!("../../tests/fixtures/system_df_minimal.json");

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn complete(registry: &str) -> BTreeMap<String, String> {
    labels(&[
        (LABEL_REGISTRY, registry),
        (crate::LABEL_KIND, "image"),
        (crate::LABEL_STACK, "app"),
        (crate::LABEL_GENERATION, "sha256:abc"),
        (crate::LABEL_SCOPE, "stack"),
        (crate::LABEL_WORKSPACE, "/w"),
        (crate::LABEL_CREATED, "1"),
    ])
}

#[test]
fn ownership_requires_the_complete_label_set() {
    assert_eq!(
        classify_ownership(&complete("r1"), Some("r1")),
        OwnershipClass::Ours
    );
    assert_eq!(
        classify_ownership(&complete("r1"), Some("r2")),
        OwnershipClass::ForeignRegistry
    );
    assert_eq!(
        classify_ownership(&labels(&[(crate::LABEL_KIND, "image")]), Some("r1")),
        OwnershipClass::IncompleteLabels
    );
    assert_eq!(
        classify_ownership(&BTreeMap::new(), Some("r1")),
        OwnershipClass::Unlabeled
    );
}

#[test]
fn one_missing_required_label_is_incomplete_not_unlabeled() {
    let mut partial = complete("r1");
    partial.remove(crate::LABEL_CREATED);
    assert_eq!(
        classify_ownership(&partial, Some("r1")),
        OwnershipClass::IncompleteLabels
    );
}

#[test]
fn names_never_prove_ownership() {
    // A bosn-looking name with no labels is still unlabeled.
    assert_eq!(
        classify_ownership(&BTreeMap::new(), Some("r1")),
        OwnershipClass::Unlabeled
    );
}

#[test]
fn classifier_selects_the_expected_class_per_kind() {
    let config = CensusConfig::default();
    let old = Some(DEFAULT_TTL_SECONDS + 1.0);

    let dangling = classify(
        ResourceKind::Image,
        &BTreeMap::new(),
        None,
        Signals {
            dangling: true,
            ..Signals::default()
        },
        old,
        config,
    );
    assert_eq!(dangling.class, Some(UnmanagedClass::DanglingImage));
    assert!(dangling.age_eligible);

    // A tagged, unreferenced image is reviewable but never sweepable: its remote
    // existence cannot be proven from the engine's accounting data.
    let tagged = classify(
        ResourceKind::Image,
        &BTreeMap::new(),
        None,
        Signals::default(),
        old,
        config,
    );
    assert_eq!(tagged.class, Some(UnmanagedClass::UnreferencedImage));
    assert_eq!(tagged.class.unwrap().tier(), Tier::Review);
    // Past its gate, but a review-tier class is still never swept on its own.
    assert!(tagged.age_eligible);

    let container = classify(
        ResourceKind::Container,
        &BTreeMap::new(),
        None,
        Signals::default(),
        old,
        config,
    );
    assert_eq!(container.class, Some(UnmanagedClass::StoppedContainer));

    let anon_volume = classify(
        ResourceKind::Volume,
        &BTreeMap::new(),
        None,
        Signals {
            anonymous: true,
            ..Signals::default()
        },
        old,
        config,
    );
    assert_eq!(anon_volume.class, Some(UnmanagedClass::AnonymousVolume));

    let named_volume = classify(
        ResourceKind::Volume,
        &BTreeMap::new(),
        None,
        Signals::default(),
        old,
        config,
    );
    assert_eq!(named_volume.class, Some(UnmanagedClass::NamedVolume));

    let cache = classify(
        ResourceKind::Builder,
        &BTreeMap::new(),
        None,
        Signals::default(),
        old,
        config,
    );
    assert_eq!(cache.class, Some(UnmanagedClass::BuildCache));
}

#[test]
fn protection_wins_over_reclaimability() {
    let config = CensusConfig::default();
    let old = Some(DEFAULT_TTL_SECONDS * 10.0);
    let ours = classify(
        ResourceKind::Image,
        &complete("r1"),
        Some("r1"),
        Signals {
            dangling: true,
            ..Signals::default()
        },
        old,
        config,
    );
    // #519 changed exactly this case: an owned image Docker itself calls dangling is now
    // reclaimable, because `bosn-setup:<sha256>` is content-addressed and therefore rebuildable.
    // Everything this test goes on to assert is unchanged. The narrowing that keeps that safe
    // is pinned by `owned_footprint_is_only_opened_by_dockers_own_verdict`.
    assert_eq!(ours.class, Some(UnmanagedClass::DanglingImage));
    assert_eq!(ours.protected, None);

    let foreign = classify(
        ResourceKind::Image,
        &complete("other"),
        Some("r1"),
        Signals::default(),
        old,
        config,
    );
    assert_eq!(foreign.protected, Some(ProtectedReason::ForeignRegistry));

    let incomplete = classify(
        ResourceKind::Image,
        &labels(&[(crate::LABEL_SCOPE, "stack")]),
        Some("r1"),
        Signals::default(),
        old,
        config,
    );
    assert_eq!(
        incomplete.protected,
        Some(ProtectedReason::IncompleteLabels)
    );

    // A pin is a label, so it cannot appear on an unlabeled artifact. A half-written
    // label set containing only a pin is therefore incomplete, and protected as such.
    // Pinning is honoured transitively: every pinned artifact lands in a protected
    // ownership class before any reclaim rule runs.
    let pinned_but_incomplete = classify(
        ResourceKind::Volume,
        &labels(&[(crate::LABEL_RETENTION, "pinned")]),
        None,
        Signals {
            anonymous: true,
            ..Signals::default()
        },
        old,
        config,
    );
    assert_eq!(
        pinned_but_incomplete.protected,
        Some(ProtectedReason::IncompleteLabels)
    );

    let in_use = classify(
        ResourceKind::Container,
        &BTreeMap::new(),
        None,
        Signals {
            in_use: true,
            ..Signals::default()
        },
        old,
        config,
    );
    assert_eq!(in_use.protected, Some(ProtectedReason::InUse));
}

fn artifact(id: &str, kind: ResourceKind, bytes: i128, age: f64) -> ObservedArtifact {
    ObservedArtifact {
        id: id.to_owned(),
        kind,
        labels: BTreeMap::new(),
        signals: Signals::default(),
        bytes: Some(bytes),
        age_seconds: Some(age),
    }
}

#[test]
fn foreign_bytes_cannot_drive_eviction_of_owned_caches() {
    // G7, the RED case from #147 and from #273: the disk is 50 GiB short of its floor,
    // foreign artifacts hold 80 GiB Bosn may not touch, and Bosn owns 1 GiB of warm
    // cache. Evicting the cache cannot close a 50 GiB gap.
    let decision = pressure_decision(true, true, 10, 60, 1, true);
    assert!(!decision.may_evict_owned);
    assert_eq!(
        decision.attribution,
        PressureAttribution::ForeignBytesDominate
    );
    assert_eq!(decision.shortfall_bytes, 50);
}

#[test]
fn a_partial_census_never_authorises_eviction() {
    let decision = pressure_decision(true, true, 0, 60, 1_000, false);
    assert!(!decision.may_evict_owned);
    assert_eq!(decision.attribution, PressureAttribution::CensusIncomplete);
}

#[test]
fn eviction_behaviour_is_unchanged_when_owned_bytes_can_close_the_gap() {
    // The no-regression case: the shortfall is small and Bosn owns enough to close it.
    let decision = pressure_decision(true, true, 50, 60, 40, true);
    assert!(decision.may_evict_owned);
    assert_eq!(
        decision.attribution,
        PressureAttribution::OwnedBytesCanClose
    );
    // Exactly enough is enough.
    assert!(pressure_decision(true, true, 50, 60, 10, true).may_evict_owned);
}

#[test]
fn pressure_without_a_free_space_shortfall_is_untouched() {
    // A count or byte-ceiling pressure has no shortfall to attribute, and keeping the
    // existing behaviour here is what stops this from silently disabling retention.
    let decision = pressure_decision(true, false, 0, 60, 0, true);
    assert!(decision.may_evict_owned);
    assert_eq!(decision.shortfall_bytes, 0);
    let idle = pressure_decision(false, false, 0, 60, 0, true);
    assert!(idle.may_evict_owned);
    assert_eq!(idle.attribution, PressureAttribution::NotUnderPressure);
}

#[test]
fn a_healthy_machine_is_silent() {
    let artifacts = [artifact(
        "c",
        ResourceKind::Container,
        1024,
        DEFAULT_TTL_SECONDS * 2.0,
    )];
    let census = census(&artifacts, None, CensusConfig::default());
    assert_eq!(warning(&census, WarningThreshold::default()), None);
}

#[test]
fn the_threshold_decides_by_bytes_or_objects() {
    let artifacts = [artifact(
        "c",
        ResourceKind::Container,
        DEFAULT_WARN_BYTES + 1,
        DEFAULT_TTL_SECONDS * 2.0,
    )];
    let census = census(&artifacts, None, CensusConfig::default());
    let warning = warning(&census, WarningThreshold::default()).expect("over the byte gate");
    assert!(!warning.partial);
    assert_eq!(warning.reclaimable_objects, 1);
}

#[test]
fn a_partial_census_always_warns() {
    let census = Census {
        partial: true,
        ..Census::default()
    };
    let warning = warning(&census, WarningThreshold::default()).expect("partial warns");
    assert!(warning.partial);
    assert_eq!(warning.reclaimable_bytes, 0);
}

#[test]
fn the_reclaimable_total_never_promises_bytes_no_command_can_free() {
    let artifacts = [
        artifact("c", ResourceKind::Container, 100, DEFAULT_TTL_SECONDS * 2.0),
        artifact(
            "cache",
            ResourceKind::Builder,
            900,
            DEFAULT_TTL_SECONDS * 2.0,
        ),
    ];
    let census = census(&artifacts, None, CensusConfig::default());
    assert_eq!(census.reclaimable_objects, 1);
    assert_eq!(census.reclaimable_bytes, 100);
    // The build cache is still reported, and the warning says so separately.
    let summary = census
        .classes
        .iter()
        .find(|summary| summary.class == UnmanagedClass::BuildCache)
        .expect("build cache reported");
    assert_eq!(summary.eligible_bytes, 900);
    assert!(
        warning(&census, WarningThreshold::default()).is_none(),
        "below threshold, so no warning at all"
    );
    let loud = warning(
        &census,
        WarningThreshold {
            bytes: 1,
            objects: 1,
        },
    )
    .expect("over threshold");
    assert_eq!(loud.report_only_bytes, 900);
    assert_eq!(loud.reclaimable_bytes, 100);
}

#[test]
fn build_cache_is_reported_but_never_removed_by_id() {
    let artifacts = [artifact(
        "cache",
        ResourceKind::Builder,
        4096,
        DEFAULT_TTL_SECONDS * 2.0,
    )];
    let selected = plan(&artifacts, None, CensusConfig::default(), &[]);
    assert!(
        selected.candidates.is_empty(),
        "build cache is never a candidate"
    );
    assert_eq!(selected.report_only.len(), 1);
    assert_eq!(selected.report_only[0].class, UnmanagedClass::BuildCache);
    assert_eq!(selected.report_only[0].eligible_bytes, 4096);
}

#[test]
fn a_plan_selects_tier_one_only_unless_named() {
    let mut dangling_image = artifact("image", ResourceKind::Image, 200, DEFAULT_TTL_SECONDS * 2.0);
    dangling_image.signals.dangling = true;
    let mut unmeasured = artifact(
        "unmeasured",
        ResourceKind::Container,
        0,
        DEFAULT_TTL_SECONDS * 2.0,
    );
    unmeasured.bytes = None;
    let artifacts = [
        artifact(
            "container",
            ResourceKind::Container,
            100,
            DEFAULT_TTL_SECONDS * 2.0,
        ),
        dangling_image,
        // Tagged and unreferenced: reviewable, never swept.
        artifact("local", ResourceKind::Image, 300, DEFAULT_TTL_SECONDS * 2.0),
        artifact("young", ResourceKind::Container, 400, 60.0),
        unmeasured,
    ];
    let selected = plan(&artifacts, None, CensusConfig::default(), &[]);
    let ids: Vec<&str> = selected.candidates.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["container", "image"], "containers before images");
    assert_eq!(selected.bytes, 300);
    assert!(selected.review.iter().any(|c| c.id == "local"));

    // Naming a Tier-2 artifact opts exactly that one in.
    let included = plan(
        &artifacts,
        None,
        CensusConfig::default(),
        &["local".to_owned()],
    );
    let ids: Vec<&str> = included.candidates.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, vec!["container", "image", "local"]);
    assert_eq!(included.bytes, 600);
    assert!(included.review.iter().all(|c| c.id != "local"));
}

#[test]
fn an_unmeasured_artifact_never_enters_a_plan() {
    let mut unmeasured = artifact("x", ResourceKind::Container, 0, DEFAULT_TTL_SECONDS * 2.0);
    unmeasured.bytes = None;
    let selected = plan(&[unmeasured], None, CensusConfig::default(), &[]);
    assert!(selected.candidates.is_empty());
    assert_eq!(selected.bytes, 0);
}

#[test]
fn an_acknowledgement_suppresses_until_the_footprint_grows() {
    let artifacts = [artifact(
        "c",
        ResourceKind::Container,
        DEFAULT_WARN_BYTES + 1,
        DEFAULT_TTL_SECONDS * 2.0,
    )];
    let census = census(&artifacts, None, CensusConfig::default());
    let now = 1_000_000.0;
    let ack = Acknowledgement {
        at: now,
        objects: census.reclaimable_objects,
        bytes: census.reclaimable_bytes,
    };
    assert!(acknowledgement_suppresses(Some(ack), &census, now));
    // Material growth re-arms it.
    let grown = Census {
        reclaimable_bytes: (ack.bytes as f64 * 1.5) as i128,
        ..census.clone()
    };
    assert!(!acknowledgement_suppresses(Some(ack), &grown, now));
    // So does age.
    assert!(!acknowledgement_suppresses(
        Some(ack),
        &census,
        now + ACK_MAX_AGE_SECONDS
    ));
    // An unreadable clock keeps the user's acknowledgement.
    assert!(acknowledgement_suppresses(Some(ack), &census, f64::NAN));
    assert!(!acknowledgement_suppresses(None, &census, now));
}

#[test]
fn networks_are_protected_because_the_census_cannot_measure_them() {
    // `docker system df -v` has no network section, so a network has no size and no age.
    // It is never reclaimable, whatever its apparent age.
    let verdict = classify(
        ResourceKind::Network,
        &BTreeMap::new(),
        None,
        Signals::default(),
        Some(DEFAULT_TTL_SECONDS * 100.0),
        CensusConfig::default(),
    );
    assert_eq!(verdict.class, None);
    assert_eq!(verdict.protected, Some(ProtectedReason::Unclassified));
}

#[test]
fn unknown_age_fails_closed() {
    let verdict = classify(
        ResourceKind::Volume,
        &BTreeMap::new(),
        None,
        Signals {
            anonymous: true,
            ..Signals::default()
        },
        None,
        CensusConfig::default(),
    );
    assert_eq!(verdict.class, None);
    assert_eq!(verdict.protected, Some(ProtectedReason::Unmeasured));
}

#[test]
fn below_the_age_gate_is_reported_but_not_eligible() {
    let verdict = classify(
        ResourceKind::Container,
        &BTreeMap::new(),
        None,
        Signals::default(),
        Some(60.0),
        CensusConfig::default(),
    );
    assert_eq!(verdict.class, Some(UnmanagedClass::StoppedContainer));
    assert!(!verdict.age_eligible);
}

#[test]
fn census_totals_only_count_eligible_tier_one() {
    let artifacts = vec![
        ObservedArtifact {
            id: "old".into(),
            kind: ResourceKind::Container,
            labels: BTreeMap::new(),
            signals: Signals::default(),
            bytes: Some(100),
            age_seconds: Some(DEFAULT_TTL_SECONDS * 2.0),
        },
        ObservedArtifact {
            id: "new".into(),
            kind: ResourceKind::Container,
            labels: BTreeMap::new(),
            signals: Signals::default(),
            bytes: Some(500),
            age_seconds: Some(1.0),
        },
        ObservedArtifact {
            id: "local".into(),
            kind: ResourceKind::Image,
            labels: BTreeMap::new(),
            signals: Signals::default(),
            bytes: Some(900),
            age_seconds: Some(DEFAULT_TTL_SECONDS * 2.0),
        },
    ];
    let census = census(&artifacts, None, CensusConfig::default());
    assert_eq!(census.reclaimable_objects, 1);
    assert_eq!(census.reclaimable_bytes, 100);
    assert!(census.bytes_approximate);
    assert!(!census.partial);
    let stopped = census
        .classes
        .iter()
        .find(|summary| summary.class == UnmanagedClass::StoppedContainer)
        .expect("stopped container summary");
    assert_eq!(stopped.objects, 2);
    assert_eq!(stopped.eligible_objects, 1);
    // Tier 2 is reported but never counted as reclaimable.
    let local_only = census
        .classes
        .iter()
        .find(|summary| summary.class == UnmanagedClass::UnreferencedImage)
        .expect("local-only summary");
    assert_eq!(local_only.tier, Tier::Review);
    assert_eq!(local_only.eligible_objects, 0);
}

#[test]
fn an_unmeasurable_size_makes_the_census_partial() {
    let artifacts = vec![ObservedArtifact {
        id: "x".into(),
        kind: ResourceKind::Container,
        labels: BTreeMap::new(),
        signals: Signals::default(),
        bytes: None,
        age_seconds: Some(DEFAULT_TTL_SECONDS * 2.0),
    }];
    assert!(census(&artifacts, None, CensusConfig::default()).partial);
}

#[test]
fn docker_sizes_parse_and_unknown_units_refuse() {
    assert_eq!(parse_docker_size("32B"), Some(32));
    assert_eq!(parse_docker_size("15.92kB"), Some(15_920));
    assert_eq!(parse_docker_size("195.6MB"), Some(195_600_000));
    assert_eq!(parse_docker_size("1.67GB"), Some(1_670_000_000));
    assert_eq!(parse_docker_size("117MB*"), Some(117_000_000));
    assert_eq!(parse_docker_size("2GiB"), Some(2 * 1024 * 1024 * 1024));
    assert_eq!(parse_docker_size("N/A"), None);
    assert_eq!(parse_docker_size(""), None);
    assert_eq!(parse_docker_size("12 furlongs"), None);
    assert_eq!(parse_docker_size("-5MB"), None);
}

#[test]
fn docker_timestamps_parse_with_numeric_offsets() {
    // 1970-01-01T00:00:00Z
    assert_eq!(
        parse_docker_timestamp("1970-01-01 00:00:00 +0000 UTC"),
        Some(0.0)
    );
    // A negative offset shifts the instant later in UTC.
    assert_eq!(
        parse_docker_timestamp("1970-01-01 00:00:00 -0700 PDT"),
        Some(7.0 * 3600.0)
    );
    // Fractional seconds are truncated to nanosecond precision.
    let fraction = parse_docker_timestamp("1970-01-01 00:00:01.5 +0000 UTC").unwrap();
    assert!((fraction - 1.5).abs() < 1e-9);
    assert_eq!(
        parse_docker_timestamp("2026-09-16 12:13:31 -0700 PDT"),
        Some(1_789_586_011.0)
    );
    assert_eq!(parse_docker_timestamp("not a timestamp"), None);
    assert_eq!(
        parse_docker_timestamp("2026-13-16 12:13:31 -0700 PDT"),
        None
    );
    assert_eq!(parse_docker_timestamp("2026-09-16 12:13:31"), None);
}

#[test]
fn label_lists_parse_and_a_bare_key_counts_as_present() {
    let parsed = parse_label_list("a=1,b=2");
    assert_eq!(parsed.get("a"), Some(&"1".to_owned()));
    assert_eq!(parsed.get("b"), Some(&"2".to_owned()));
    assert!(
        parse_label_list("com.docker.volume.anonymous=")
            .contains_key("com.docker.volume.anonymous")
    );
    assert!(parse_label_list("bare").contains_key("bare"));
    assert!(parse_label_list("").is_empty());
}

#[test]
fn observe_maps_the_df_sections_onto_artifacts() {
    let json = SYSTEM_DF_MINIMAL;
    let report: SystemDfReport = serde_json::from_str(json).expect("fixture parses");
    let observed = observe(EngineObservation {
        report: &report,
        dangling_image_ids: &["sha256:danglingimage".to_owned()],
        bosn_labeled_image_ids: &["sha256:bosnimage".to_owned()],
        inspected_volumes: &[],
        now: 1_789_595_611.0,
    });
    assert_eq!(observed.len(), 4);
    let image = &observed[0];
    assert_eq!(image.kind, ResourceKind::Image);
    assert!(image.signals.dangling);
    assert!(!image.signals.in_use);
    let labeled = observed
        .iter()
        .find(|artifact| artifact.id == "sha256:bosnimage")
        .expect("labeled image present");
    assert_eq!(
        classify_ownership(&labeled.labels, Some("r1")),
        OwnershipClass::IncompleteLabels
    );
    let volume = observed
        .iter()
        .find(|artifact| artifact.kind == ResourceKind::Volume)
        .expect("volume present");
    assert!(
        volume.signals.anonymous,
        "anonymous volume detected from its label"
    );
    assert_eq!(
        volume.age_seconds, None,
        "a volume without inspect detail has no age, and is therefore protected"
    );
}

#[test]
fn only_docker_reported_dangling_images_are_dangling() {
    // An untagged image that is still the parent of a tagged one is not dangling. If the
    // caller's dangling set omits it, the heuristic must not override that.
    let json = SYSTEM_DF_MINIMAL;
    let report: SystemDfReport = serde_json::from_str(json).expect("fixture parses");
    let observed = observe(EngineObservation {
        report: &report,
        dangling_image_ids: &[],
        bosn_labeled_image_ids: &[],
        inspected_volumes: &[],
        now: 1_789_595_611.0,
    });
    let image = &observed[0];
    assert!(!image.signals.dangling);
    // It is still untagged and local-only, so it lands in the review tier rather than
    // disappearing.
    let verdict = classify(
        image.kind,
        &image.labels,
        None,
        image.signals,
        image.age_seconds,
        CensusConfig::default(),
    );
    assert_eq!(verdict.class, Some(UnmanagedClass::UnreferencedImage));
}

#[test]
fn inspected_volumes_supply_the_age_the_accounting_document_lacks() {
    let json = SYSTEM_DF_MINIMAL;
    let report: SystemDfReport = serde_json::from_str(json).expect("fixture parses");
    let now = 1_789_595_611.0;
    let observed = observe(EngineObservation {
        report: &report,
        dangling_image_ids: &[],
        bosn_labeled_image_ids: &[],
        inspected_volumes: &[InspectedVolume {
            name: "f1c1cdf1f2ba212d6d08336115a83821aa80e1971ce9aa2513dd65bfce07f7ca".to_owned(),
            created_at: "2026-08-01T00:00:00-07:00".to_owned(),
            labels: BTreeMap::from([("com.docker.volume.anonymous".to_owned(), String::new())]),
        }],
        now,
    });
    let volume = observed
        .iter()
        .find(|artifact| artifact.kind == ResourceKind::Volume)
        .expect("volume present");
    let age = volume.age_seconds.expect("inspected age");
    // 2026-08-01T00:00:00-07:00 is 2026-08-01T07:00:00Z.
    assert!((age - (now - 1_785_567_600.0)).abs() < 1.0, "age was {age}");
    let verdict = classify(
        volume.kind,
        &volume.labels,
        None,
        volume.signals,
        volume.age_seconds,
        CensusConfig::default(),
    );
    assert_eq!(verdict.class, Some(UnmanagedClass::AnonymousVolume));
    assert!(
        verdict.age_eligible,
        "a month-old anonymous volume is eligible"
    );
}

#[test]
fn rfc3339_and_accounting_timestamps_agree() {
    assert_eq!(
        parse_rfc3339_timestamp("2026-09-13T18:41:43-07:00"),
        parse_docker_timestamp("2026-09-13 18:41:43 -0700 PDT")
    );
    let fraction = parse_rfc3339_timestamp("2026-09-16T08:56:39.263133667-07:00").unwrap();
    assert!((fraction - 1_789_574_199.263_133_7).abs() < 1e-6);
    assert_eq!(
        parse_rfc3339_timestamp("2026-09-13 18:41:43 +0000 UTC"),
        parse_docker_timestamp("2026-09-13 18:41:43 +0000 UTC")
    );
    assert_eq!(parse_rfc3339_timestamp("nonsense"), None);
    // A UTC designator is accepted, and a date without a zone is not.
    assert_eq!(parse_rfc3339_timestamp("1970-01-01T00:00:00Z"), Some(0.0));
    assert_eq!(parse_rfc3339_timestamp("1970-01-01T00:00:00"), None);
}

#[test]
fn owned_footprint_is_only_opened_by_dockers_own_verdict() {
    // The narrowing that makes #519 safe. An owned image enters the reclaimable set only when
    // Docker itself calls it dangling; owned containers and volumes stay protected whatever
    // their age, because widening those would create a second, unattended path to removing a
    // volume. Pins and incompleteness still resolve before any of this.
    let config = CensusConfig::default();
    let old = Some(DEFAULT_TTL_SECONDS * 10.0);
    let open = |kind, signals| classify(kind, &complete("r1"), Some("r1"), signals, old, config);

    // The narrowing that matters: an owned image Docker does *not* call dangling, and one that
    // is still in use, both stay protected. Only Docker's own verdict opens the door.
    let owned_tagged = open(ResourceKind::Image, Signals::default());
    assert_eq!(
        owned_tagged.protected,
        Some(ProtectedReason::OwnedByThisRegistry)
    );
    let owned_in_use = open(
        ResourceKind::Image,
        Signals {
            dangling: true,
            in_use: true,
            ..Signals::default()
        },
    );
    assert_eq!(
        owned_in_use.protected,
        Some(ProtectedReason::OwnedByThisRegistry)
    );

    // An owned *container* stays protected whatever its age. Widening images must never create
    // a second unattended path to removing an owned volume's owner.
    let owned_container = open(
        ResourceKind::Container,
        Signals {
            dangling: true,
            ..Signals::default()
        },
    );
    assert_eq!(
        owned_container.protected,
        Some(ProtectedReason::OwnedByThisRegistry)
    );

    // An owned dangling image with no measurable age cannot clear a gate it has no value for.
    let owned_unmeasured = classify(
        ResourceKind::Image,
        &complete("r1"),
        Some("r1"),
        Signals {
            dangling: true,
            ..Signals::default()
        },
        None,
        config,
    );
    assert_eq!(
        owned_unmeasured.protected,
        Some(ProtectedReason::Unmeasured)
    );
}

// ---------------------------------------------------------------------------
// #516: protected bytes are measured, so they must be reported.
// ---------------------------------------------------------------------------

fn owned_protected_artifact(bytes: i128) -> ObservedArtifact {
    // A volume labelled for a registry this machine no longer recognises — the exact shape a
    // reset state directory leaves behind. It is protected, and it is the largest thing on the
    // disk, and until #516 nothing said so.
    ObservedArtifact {
        id: "v-stale".to_owned(),
        kind: ResourceKind::Volume,
        labels: complete("some-other-registry"),
        signals: Signals::default(),
        bytes: Some(bytes),
        age_seconds: Some(DEFAULT_TTL_SECONDS * 100.0),
    }
}

#[test]
fn a_protected_footprint_alone_can_raise_a_warning() {
    // RED case for #516: 200 GiB of foreign-registry volumes, nothing reclaimable. Before the
    // fix `warning()` returned None and a machine in this state was silent.
    let census = census(
        &[owned_protected_artifact(200 << 30)],
        Some("r1"),
        CensusConfig::default(),
    );
    assert_eq!(census.reclaimable_bytes, 0, "nothing here is reclaimable");
    let warned = warning(&census, WarningThreshold::default());
    assert!(
        warned.is_some(),
        "a 200 GiB protected footprint must not be silent"
    );
    let warning = warned.expect("warning");
    assert_eq!(warning.protected.len(), 1);
    assert_eq!(warning.protected[0].objects, 1);
    assert_eq!(warning.protected[0].bytes, 200 << 30);
    assert_eq!(
        warning.protected[0].reason,
        ProtectedReason::ForeignRegistry
    );
}

#[test]
fn a_healthy_machine_stays_silent_with_the_protected_terms_added() {
    // #516 must not make every machine noisy. One small unlabeled object is under both the
    // byte and object thresholds, so there is still nothing to say.
    let census = census(
        &[artifact(
            "u",
            ResourceKind::Volume,
            1024,
            DEFAULT_TTL_SECONDS * 2.0,
        )],
        Some("r1"),
        CensusConfig::default(),
    );
    assert!(
        warning(&census, WarningThreshold::default()).is_none(),
        "a small tidy machine must stay silent"
    );
}

#[test]
fn protected_objects_alone_can_trip_the_object_threshold() {
    // The byte threshold is 5 GiB; 26 tiny protected objects clear the count threshold instead,
    // which proves the object term is live and not just a restatement of the byte term.
    let artifacts: Vec<ObservedArtifact> = (0..26)
        .map(|index| {
            let mut item = owned_protected_artifact(1024);
            item.id = format!("v-{index}");
            item
        })
        .collect();
    let census = census(&artifacts, Some("r1"), CensusConfig::default());
    assert!(warning(&census, WarningThreshold::default()).is_some());
}

// ---------------------------------------------------------------------------
// #520: an unmeasured size is not a zero-byte size.
// ---------------------------------------------------------------------------

fn unmeasured_artifact(id: &str) -> ObservedArtifact {
    ObservedArtifact {
        id: id.to_owned(),
        kind: ResourceKind::Volume,
        labels: BTreeMap::new(),
        signals: Signals::default(),
        bytes: None,
        age_seconds: Some(DEFAULT_TTL_SECONDS * 2.0),
    }
}

#[test]
fn unmeasured_objects_are_counted_rather_than_folded_into_zero_bytes() {
    let artifacts: Vec<ObservedArtifact> = (0..30)
        .map(|i| unmeasured_artifact(&format!("u-{i}")))
        .collect();
    let census = census(&artifacts, Some("r1"), CensusConfig::default());
    assert_eq!(
        census.unmeasured_objects, 30,
        "30 objects with no reported size must not read as 0 bytes"
    );
    assert!(
        census.partial,
        "an unmeasured input is still a partial census"
    );
}

#[test]
fn unmeasured_objects_alone_can_raise_a_warning() {
    // #520's user-visible half: 30 unmeasured objects and nothing else. Their byte total is
    // genuinely unknown, so the object count is what speaks.
    let artifacts: Vec<ObservedArtifact> = (0..30)
        .map(|i| unmeasured_artifact(&format!("u-{i}")))
        .collect();
    let census = census(&artifacts, Some("r1"), CensusConfig::default());
    assert_eq!(census.reclaimable_bytes, 0);
    let warned = warning(&census, WarningThreshold::default());
    assert!(warned.is_some(), "unmeasured objects must be able to speak");
    assert_eq!(warned.expect("warning").unmeasured_objects, 30);
}

#[test]
fn a_measured_zero_is_not_counted_as_unmeasured() {
    // The distinction the fix exists to preserve: 0 B measured is a fact; 0 B because the
    // engine said nothing is an admission of ignorance.
    let census = census(
        &[artifact(
            "z",
            ResourceKind::Volume,
            0,
            DEFAULT_TTL_SECONDS * 2.0,
        )],
        Some("r1"),
        CensusConfig::default(),
    );
    assert_eq!(census.unmeasured_objects, 0);
    assert!(!census.partial);
}
