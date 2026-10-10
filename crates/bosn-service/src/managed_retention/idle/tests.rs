//! #545 / #536: only a provably idle, registry-proven keepalive is ever stopped.

use std::collections::BTreeMap;
use std::time::Duration;

use bosn_core::{ResourceKind, ResourceState, Retention, Scope};
use bosn_engine::DockerEngine;
use bosn_registry::{Registry, Resource};

use super::{only_keepalive_processes, retire_idle_keepalives};

const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const GATE: Duration = Duration::from_secs(6 * 3600);

fn digest() -> String {
    "a".repeat(64)
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// One fixture container: its name, registry idle time, command and liveness.
struct Fixture {
    name: &'static str,
    idle_hours: f64,
    keepalive: bool,
    running: bool,
    recorded: bool,
}

/// A state directory, its registry, and a fake Docker that inspects each fixture by id and
/// records every `stop`.
fn world(
    fixtures: &[Fixture],
    top: &str,
) -> (kernal_api::platform::fs::TemporaryDirectory, DockerEngine) {
    let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry =
        Registry::create_writer(root.path().join("registry.sqlite3"), OWNER).unwrap();
    let mut transaction = registry.begin_immediate().unwrap();
    let mut ids = String::new();
    for fixture in fixtures {
        if fixture.recorded {
            transaction
                .put_resource(&Resource {
                    id: format!("manifest-container:app:{}", fixture.name),
                    kind: ResourceKind::Container,
                    name: fixture.name.into(),
                    stack: "app".into(),
                    generation: format!("sha256:{}", digest()),
                    scope: Scope::Machine,
                    workspace: "/workspace".into(),
                    created_at: 1.0,
                    last_used: now() - fixture.idle_hours * 3600.0,
                    state: ResourceState::Active,
                    retention: Retention::Pinned,
                })
                .unwrap();
        }
        let labels = BTreeMap::from([
            ("com.zackees.bosn.setup-managed", "v1".to_owned()),
            ("com.zackees.bosn.setup-container", fixture.name.to_owned()),
            ("com.zackees.bosn.setup-content-sha256", digest()),
        ]);
        let cmd: Vec<String> = if fixture.keepalive {
            bosn_setup::login_shell_args(crate::MANIFEST_LINUX_IDLE_COMMAND).to_vec()
        } else {
            bosn_setup::login_shell_args("run-app").to_vec()
        };
        let document = serde_json::json!([{
            "Id": fixture.name, "Name": format!("/{}", fixture.name),
            "Created": "2020-01-01T00:00:00Z",
            "State": {"Running": fixture.running},
            "Config": {"Labels": labels, "Cmd": cmd},
        }]);
        std::fs::write(
            root.path().join(format!("{}.json", fixture.name)),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        ids.push_str(fixture.name);
        ids.push('\n');
    }
    transaction.commit().unwrap();
    std::fs::write(root.path().join("ids"), ids).unwrap();
    std::fs::write(root.path().join("top"), top).unwrap();
    let engine = DockerEngine::synthetic_for_test(
        "/bin/sh",
        [
            "-c",
            r#"
case "$1" in
  ps) cat "$IDLE_FIXTURE/ids" ;;
  inspect)
    shift; printf '['; sep=''
    for id in "$@"; do
      case "$id" in -*) continue ;; esac
      [ -f "$IDLE_FIXTURE/$id.json" ] || continue
      printf '%s' "$sep"; sed -e 's/^\[//' -e 's/\]$//' "$IDLE_FIXTURE/$id.json"; sep=','
    done
    printf ']' ;;
  top) cat "$IDLE_FIXTURE/top" ;;
  stop) printf '%s\n' "$4" >> "$IDLE_FIXTURE/stopped" ;;
  *) exit 99 ;;
esac
"#,
            "fake-docker",
        ],
    )
    .env("IDLE_FIXTURE", root.path().as_os_str());
    (root, engine)
}

fn stopped(root: &kernal_api::platform::fs::TemporaryDirectory) -> String {
    std::fs::read_to_string(root.path().join("stopped")).unwrap_or_default()
}

const KEEPALIVE_TOP: &str = "PID COMMAND\n1 sh\n2 sleep\n";

#[test]
fn only_the_proven_idle_keepalive_is_stopped() {
    let fixtures = [
        Fixture {
            name: "idle",
            idle_hours: 7.0,
            keepalive: true,
            running: true,
            recorded: true,
        },
        Fixture {
            name: "recent",
            idle_hours: 1.0,
            keepalive: true,
            running: true,
            recorded: true,
        },
        Fixture {
            name: "app",
            idle_hours: 7.0,
            keepalive: false,
            running: true,
            recorded: true,
        },
        Fixture {
            name: "unrecorded",
            idle_hours: 7.0,
            keepalive: true,
            running: true,
            recorded: false,
        },
        Fixture {
            name: "exited",
            idle_hours: 7.0,
            keepalive: true,
            running: false,
            recorded: true,
        },
    ];
    let (root, engine) = world(&fixtures, KEEPALIVE_TOP);
    let outcome = retire_idle_keepalives(&engine, root.path(), GATE, 16);
    assert_eq!(outcome.failures, Vec::<String>::new());
    assert_eq!(outcome.stopped, vec!["idle".to_owned()]);
    assert_eq!(stopped(&root), "idle\n");
}

#[test]
fn a_busy_process_table_keeps_the_container() {
    let fixtures = [Fixture {
        name: "idle",
        idle_hours: 7.0,
        keepalive: true,
        running: true,
        recorded: true,
    }];
    let (root, engine) = world(&fixtures, "PID COMMAND\n1 sh\n2 sleep\n3 cargo\n");
    let outcome = retire_idle_keepalives(&engine, root.path(), GATE, 16);
    assert!(outcome.stopped.is_empty());
    assert_eq!(stopped(&root), "");
}

#[test]
fn a_session_protects_the_container() {
    let fixtures = [Fixture {
        name: "idle",
        idle_hours: 7.0,
        keepalive: true,
        running: true,
        recorded: true,
    }];
    let (root, engine) = world(&fixtures, KEEPALIVE_TOP);
    {
        let mut registry = Registry::open_writer(root.path().join("registry.sqlite3")).unwrap();
        let mut transaction = registry.begin_immediate().unwrap();
        transaction
            .put_execution_session(&bosn_registry::ExecutionSession {
                id: "manifest-app-task:1".into(),
                container_id: "idle".into(),
                engine_binary: "docker".into(),
                client_pid: std::process::id(),
                client_start: None,
                lease_ids: Vec::new(),
            })
            .unwrap();
        transaction.commit().unwrap();
    }
    let outcome = retire_idle_keepalives(&engine, root.path(), GATE, 16);
    assert!(outcome.stopped.is_empty(), "{outcome:?}");
}

#[test]
fn the_per_pass_limit_is_honoured() {
    let fixtures = [Fixture {
        name: "idle",
        idle_hours: 7.0,
        keepalive: true,
        running: true,
        recorded: true,
    }];
    let (root, engine) = world(&fixtures, KEEPALIVE_TOP);
    assert!(
        retire_idle_keepalives(&engine, root.path(), GATE, 0)
            .stopped
            .is_empty()
    );
    assert_eq!(stopped(&root), "");
}

#[test]
fn process_tables_other_than_the_bare_keepalive_are_protected() {
    assert!(only_keepalive_processes(b"PID COMMAND\n1 sh\n2 sleep\n"));
    assert!(!only_keepalive_processes(b"PID COMMAND\n1 sh\n2 cargo\n"));
    assert!(!only_keepalive_processes(
        b"PID COMMAND\n1 sh\n2 sleep\n3 sh\n4 sleep\n"
    ));
    assert!(!only_keepalive_processes(b"PID COMMAND\n1 sh\n"));
    assert!(!only_keepalive_processes(b"PID COMMAND\n"));
    assert!(!only_keepalive_processes(b"COMMAND\nsh\n"));
}
