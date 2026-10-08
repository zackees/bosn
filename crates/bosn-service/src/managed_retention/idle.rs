//! Retirement of the fixed manifest keepalive while job admission is held.

use super::*;

pub(super) fn run(
    engine: &DockerEngine,
    state: &Path,
    policy: RetentionPolicy,
    apply: bool,
    limit: usize,
) -> ManagedRetentionOutcome {
    if apply && let Err(error) = retire_idle_manifests(engine, state, policy.container_ttl, limit) {
        let mut outcome = staged::pass_limited(engine, state, policy, false, limit);
        outcome.summary.refused = Some(error);
        return outcome;
    }
    staged::pass_limited(engine, state, policy, apply, limit)
}

/// Stop only the exact registered manifest keepalive, never a declared app or guest.
/// Any lease/session/intent, unknown observation, or extra process protects it.
pub(super) fn retire_idle_manifests(
    engine: &DockerEngine,
    state_dir: &Path,
    ttl: Duration,
    limit: usize,
) -> Result<(), String> {
    if limit == 0 {
        return Ok(());
    }
    let ownership = registered::RegisteredOwnership::load(state_dir)?;
    let names = ownership.idle_manifests(now_seconds(), ttl.as_secs_f64());
    let options = RunOptions::bounded(RETENTION_READ_DEADLINE, RETENTION_OUTPUT_LIMIT);
    let mut stopped_count = 0;
    for name in names {
        if stopped_count >= limit {
            break;
        }
        budget::check()?;
        let entries = parse_read::<ContainerDetail>(
            engine.inspect_containers(std::slice::from_ref(&name), budget::options(options)),
        );
        let entries = match entries {
            Some(entries) => entries,
            None if absent_container(engine, &name, budget::options(options)) => continue,
            None => return Err(format!("idle container {name} could not be inspected")),
        };
        let Some(entry) = entries.into_iter().next() else {
            continue;
        };
        if !entry.running()
            || !is_manifest_keepalive(&entry)
            || verified_container_labels(&entry)
                .get(bosn_core::LABEL_RETENTION)
                .map(String::as_str)
                == Some("pinned")
        {
            continue;
        }
        let labels = verified_container_labels(&entry);
        let top = engine
            .with_args(["top", &entry.id, "-eo", "pid,comm"])
            .capture(budget::options(options))
            .map_err(|error| error.to_string())?;
        if !top.ok() || !only_keepalive_processes(&top.stdout) {
            continue;
        }
        let fresh = registered::RegisteredOwnership::load(state_dir)?;
        if !fresh
            .idle_manifests(now_seconds(), ttl.as_secs_f64())
            .contains(&name)
            || fresh
                .normalize(ResourceKind::Container, &name, &labels)
                .is_none()
        {
            continue;
        }
        // Admission remains held. Durable sessions were rechecked immediately above.
        // Use graceful stop; failure preserves the object and ends this maintenance pass.
        budget::check()?;
        let stopped = engine
            .with_args(["stop", "--time", "10", &entry.id])
            .capture(budget::options(RunOptions::bounded(
                RETENTION_REMOVAL_DEADLINE,
                RETENTION_REMOVAL_OUTPUT_LIMIT,
            )))
            .map_err(|error| error.to_string())?;
        if !stopped.ok() {
            return Err(format!("idle container {name} could not be stopped"));
        }
        stopped_count += 1;
    }
    Ok(())
}

fn absent_container(engine: &DockerEngine, name: &str, options: RunOptions) -> bool {
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        return false;
    }
    let filter = format!("name=^/{}$", name.replace('.', "\\."));
    engine
        .with_args([
            "container",
            "ls",
            "--all",
            "--no-trunc",
            "--filter",
            &filter,
            "--format",
            "{{.ID}}",
        ])
        .capture(budget::options(options))
        .is_ok_and(|result| result.ok() && result.stdout.iter().all(u8::is_ascii_whitespace))
}

fn is_manifest_keepalive(entry: &ContainerDetail) -> bool {
    entry.config.as_ref().is_some_and(|config| {
        bosn_setup::is_login_shell_command(&config.command, crate::MANIFEST_LINUX_IDLE_COMMAND)
    })
}

fn only_keepalive_processes(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let mut lines = text.lines();
    if lines
        .next()
        .is_none_or(|header| header.split_whitespace().collect::<Vec<_>>() != ["PID", "COMMAND"])
    {
        return false;
    }
    let mut shells = 0;
    let mut sleeps = 0;
    for line in lines {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2
            || fields[0].parse::<u32>().is_err()
            || !matches!(fields[1], "sh" | "sleep")
        {
            return false;
        }
        match fields[1] {
            "sh" => shells += 1,
            "sleep" => sleeps += 1,
            _ => return false,
        }
    }
    shells == 1 && sleeps == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_object_allowance_stops_only_the_oldest_verified_keepalive() {
        use bosn_core::{ResourceState, Retention, Scope};
        use bosn_registry::{Registry, Resource};
        let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let owner = "11111111-2222-4333-8444-555555555555";
        let digest = "a".repeat(64);
        let mut registry =
            Registry::create_writer(root.path().join("registry.sqlite3"), owner).unwrap();
        let mut transaction = registry.begin_immediate().unwrap();
        for (name, last_used) in [("recent", 20.0), ("oldest", 10.0)] {
            transaction
                .put_resource(&Resource {
                    id: format!("manifest-container:app:{name}"),
                    kind: ResourceKind::Container,
                    name: name.into(),
                    stack: "app".into(),
                    generation: format!("sha256:{digest}"),
                    scope: Scope::Machine,
                    workspace: "/workspace".into(),
                    created_at: 1.0,
                    last_used,
                    state: ResourceState::Active,
                    retention: Retention::Warm,
                })
                .unwrap();
            let labels = BTreeMap::from([
                ("com.zackees.bosn.setup-managed", "v1"),
                ("com.zackees.bosn.setup-container", name),
                ("com.zackees.bosn.setup-content-sha256", digest.as_str()),
            ]);
            let document = serde_json::json!([{
                "Id": name, "Name": format!("/{name}"), "Created": "2020-01-01T00:00:00Z",
                "State": {"Running": true},
                "Config": {"Labels": labels, "Cmd": bosn_setup::login_shell_args(crate::MANIFEST_LINUX_IDLE_COMMAND)},
            }]);
            std::fs::write(
                root.path().join(format!("{name}.json")),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
        }
        transaction.commit().unwrap();
        let engine = DockerEngine::synthetic_for_test(
            "/bin/sh",
            [
                "-c",
                r#"
case "$1" in
  inspect) cat "$IDLE_FIXTURE/$4.json" ;;
  top) printf 'PID COMMAND\n1 sh\n2 sleep\n' ;;
  stop) printf '%s\n' "$4" >> "$IDLE_FIXTURE/stopped" ;;
  *) exit 99 ;;
esac
"#,
                "fake-docker",
            ],
        )
        .env("IDLE_FIXTURE", root.path().as_os_str());
        retire_idle_manifests(&engine, root.path(), Duration::ZERO, 0).unwrap();
        assert!(!root.path().join("stopped").exists());
        retire_idle_manifests(&engine, root.path(), Duration::ZERO, 1).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("stopped")).unwrap(),
            "oldest\n"
        );
    }

    #[test]
    fn caller_script_ending_in_keepalive_is_not_the_fixed_launcher() {
        let command = format!("run-app; {}", crate::MANIFEST_LINUX_IDLE_COMMAND);
        let arguments = vec!["sh".into(), "-c".into(), command];
        assert!(!bosn_setup::is_login_shell_command(
            &arguments,
            crate::MANIFEST_LINUX_IDLE_COMMAND
        ));
    }

    #[test]
    fn running_work_and_unreadable_process_tables_are_protected() {
        assert!(only_keepalive_processes(b"PID COMMAND\n1 sh\n2 sleep\n"));
        assert!(!only_keepalive_processes(b"PID COMMAND\n1 sh\n2 cargo\n"));
        assert!(!only_keepalive_processes(
            b"PID COMMAND\n1 sh\n2 sleep\n3 sh\n4 sleep\n"
        ));
        assert!(!only_keepalive_processes(
            b"PID COMMAND\n1 sh\n2 sleep\n3 sleep\n"
        ));
        assert!(!only_keepalive_processes(b"PID COMMAND\n1 sh\n"));
        assert!(!only_keepalive_processes(b"PID COMMAND\n"));
        assert!(!only_keepalive_processes(b"COMMAND\nsh\n"));
    }
}
