//! Complete Docker census and registry-backed ownership reconciliation.

use super::*;

/// Read every Bosn-labeled container, volume and image, with ages and liveness.
///
/// Returns the observations and, separately, a refusal reason when any read was incomplete.
/// A partial read yields no observations at all rather than a partial set, because an
/// incomplete candidate list is indistinguishable from an empty one.
pub(super) fn observe_owned(
    engine: &DockerEngine,
    state_dir: &Path,
) -> (
    Vec<bosn_core::ObservedArtifact>,
    Vec<StoppedSetupContainer>,
    Option<String>,
) {
    if let Err(reason) = budget::check() {
        return (Vec::new(), Vec::new(), Some(reason));
    }
    let options = RunOptions::bounded(RETENTION_READ_DEADLINE, RETENTION_OUTPUT_LIMIT);
    let mut artifacts = Vec::new();
    let mut stopped_containers = Vec::new();
    let mut unreadable = details::ReadFailures::default();

    // The label key used as the entry filter. Any object carrying it is a candidate for
    // inspection; `classify_managed` then proves or rejects each one individually, so a loose
    // filter here costs a read but can never widen what is removed.
    let probe = bosn_core::LABEL_KIND;

    let registered = match registered::RegisteredOwnership::load(state_dir) {
        Ok(registered) => registered,
        Err(error) => return (Vec::new(), Vec::new(), Some(error)),
    };
    for probe in [probe, "com.zackees.bosn.setup-managed"] {
        observe_containers(
            engine,
            options,
            probe,
            &registered,
            &mut artifacts,
            &mut stopped_containers,
            &mut unreadable,
        );
        observe_volumes(engine, options, probe, &mut artifacts, &mut unreadable);
    }
    observe_images(engine, options, probe, &mut artifacts, &mut unreadable);
    images::observe_registered(
        engine,
        &registered,
        options,
        &mut artifacts,
        &mut unreadable,
    );
    artifacts.sort_by(|left, right| (left.kind, &left.id).cmp(&(right.kind, &right.id)));
    artifacts.dedup_by(|left, right| left.kind == right.kind && left.id == right.id);
    stopped_containers.sort_by(|left, right| left.id.cmp(&right.id));
    stopped_containers.dedup_by(|left, right| left.id == right.id);
    for artifact in &mut artifacts {
        if artifact.kind == ResourceKind::Image
            && let Some((labels, last_used, protected)) =
                registered.normalize_image(&artifact.id, &artifact.labels)
        {
            artifact.labels = labels;
            artifact.signals.in_use |= protected;
            artifact.age_seconds = artifact
                .age_seconds
                .map(|age| age.min((now_seconds() - last_used).max(0.0)));
        }
        // Legacy container identities were verified against Docker's Name below.
        let name = if artifact.kind == ResourceKind::Container {
            artifact
                .labels
                .get("com.zackees.bosn.setup-container")
                .map_or("", String::as_str)
        } else {
            &artifact.id
        }
        .to_owned();
        if let Some((labels, last_used)) =
            registered.normalize(artifact.kind, &name, &artifact.labels)
        {
            artifact.signals.in_use |= registered.protected_name(&name);
            artifact.labels = labels;
            artifact.age_seconds = artifact
                .age_seconds
                .map(|age| age.min((now_seconds() - last_used).max(0.0)));
        }
        registered.apply_usage(
            artifact.kind,
            &name,
            &mut artifact.labels,
            &mut artifact.age_seconds,
            &mut artifact.signals.in_use,
        );
    }

    if unreadable.is_empty() {
        // Oldest first, so the report's head is the worst offender.
        stopped_containers.sort_by(|left, right| {
            right
                .age_seconds
                .total_cmp(&left.age_seconds)
                .then_with(|| left.id.cmp(&right.id))
        });
        (artifacts, stopped_containers, None)
    } else {
        (
            Vec::new(),
            Vec::new(),
            Some(format!(
                "{} engine read(s) failed, so this pass cannot prove what is safe to remove: {}",
                unreadable.len(),
                unreadable.describe()
            )),
        )
    }
}

/// Observe every Bosn-labeled container, running or stopped.
///
/// `State.Running` comes from Docker rather than being inferred, so a container that started
/// between two reads is still seen as live. The same read feeds the #518 report, so a stopped
/// container's pinned volumes come from the mount table Docker gives us here rather than from a
/// second, raceable query.
fn observe_containers(
    engine: &DockerEngine,
    options: RunOptions,
    probe: &str,
    registered: &registered::RegisteredOwnership,
    artifacts: &mut Vec<bosn_core::ObservedArtifact>,
    stopped_containers: &mut Vec<StoppedSetupContainer>,
    unreadable: &mut details::ReadFailures,
) {
    if !budget::observe(unreadable) {
        return;
    }
    let Some(ids) = labeled_ids(
        engine.container_ids_with_label(probe, budget::options(options)),
        "docker ps -a",
        unreadable,
    ) else {
        return;
    };
    for chunk in ids.chunks(INSPECT_CHUNK) {
        if !budget::observe(unreadable) {
            return;
        }
        let Some(entries) = parse_inspect::<ContainerDetail>(
            engine.inspect_containers(chunk, budget::options(options)),
            "docker inspect",
            unreadable,
        ) else {
            continue;
        };
        for entry in entries {
            if !budget::observe(unreadable) {
                return;
            }
            let Some(age) = entry.created_age(now_seconds()) else {
                unreadable.push(format!(
                    "container {} has no usable creation time",
                    entry.id()
                ));
                continue;
            };
            let mut labels = verified_container_labels(&entry);
            let mut age = Some(age);
            let mut in_use = entry.running();
            registered.apply_usage(
                ResourceKind::Container,
                entry.name.trim_start_matches('/'),
                &mut labels,
                &mut age,
                &mut in_use,
            );
            if !entry.running() {
                stopped_containers.push(StoppedSetupContainer {
                    id: entry.id().to_owned(),
                    age_seconds: age.unwrap_or(0.0),
                    pinned_volumes: entry.pinned_volume_names(),
                });
            }
            artifacts.push(bosn_core::ObservedArtifact {
                id: entry.id().to_owned(),
                kind: ResourceKind::Container,
                labels,
                signals: signals(in_use),
                bytes: entry.size_bytes(),
                age_seconds: age,
            });
        }
    }
}

/// Observe every Bosn-labeled volume.
///
/// Liveness is Docker's own verdict on whether any container still mounts it, because that is
/// exactly the question the volume age gate must not answer wrongly.
fn observe_volumes(
    engine: &DockerEngine,
    options: RunOptions,
    probe: &str,
    artifacts: &mut Vec<bosn_core::ObservedArtifact>,
    unreadable: &mut details::ReadFailures,
) {
    if !budget::observe(unreadable) {
        return;
    }
    let Some(names) = labeled_ids(
        engine.volume_names_with_label(probe, budget::options(options)),
        "docker volume ls",
        unreadable,
    ) else {
        return;
    };
    for chunk in names.chunks(INSPECT_CHUNK) {
        if !budget::observe(unreadable) {
            return;
        }
        let Some(entries) = parse_inspect::<VolumeDetail>(
            engine.inspect_volumes(chunk, budget::options(options)),
            "docker volume inspect",
            unreadable,
        ) else {
            continue;
        };
        for entry in entries {
            if !budget::observe(unreadable) {
                return;
            }
            let Some(age) = entry.created_age(now_seconds()) else {
                unreadable.push(format!("volume {} has no usable creation time", entry.name));
                continue;
            };
            let in_use = volume_is_unused(engine, &entry.name, budget::options(options));
            let bytes = entry.size_bytes();
            artifacts.push(bosn_core::ObservedArtifact {
                id: entry.name,
                kind: ResourceKind::Volume,
                labels: entry.labels,
                signals: signals(!in_use),
                bytes,
                age_seconds: Some(age),
            });
        }
    }
}

/// Observe every Bosn-labeled image.
///
/// An image any container was created from is still in use, so it is held. `bosn-setup:*`
/// images are content-addressed by tag, so an unreferenced one is rebuilt rather than lost.
fn observe_images(
    engine: &DockerEngine,
    options: RunOptions,
    probe: &str,
    artifacts: &mut Vec<bosn_core::ObservedArtifact>,
    unreadable: &mut details::ReadFailures,
) {
    if !budget::observe(unreadable) {
        return;
    }
    let Some(ids) = labeled_ids(
        engine.image_ids_with_label(probe, budget::options(options)),
        "docker image ls",
        unreadable,
    ) else {
        return;
    };
    for chunk in ids.chunks(INSPECT_CHUNK) {
        if !budget::observe(unreadable) {
            return;
        }
        let Some(entries) = parse_inspect::<ImageDetail>(
            engine.inspect_images(chunk, budget::options(options)),
            "docker image inspect",
            unreadable,
        ) else {
            continue;
        };
        for entry in entries {
            if !budget::observe(unreadable) {
                return;
            }
            artifacts.push(bosn_core::ObservedArtifact {
                id: entry.id.clone(),
                kind: ResourceKind::Image,
                labels: entry.labels(),
                signals: signals(!image_is_unused(
                    engine,
                    &entry.id,
                    budget::options(options),
                )),
                bytes: entry.size_bytes(),
                age_seconds: entry.created_age(now_seconds()),
            });
        }
    }
}
