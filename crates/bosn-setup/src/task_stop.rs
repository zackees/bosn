//! Stop one app-task execution's processes inside its setup container.
//!
//! Cancelling or timing out an app task kills and reaps only the local
//! `docker exec` client; the command it started keeps running in the
//! container (bosn#357: an `act` CI job and its sibling job containers kept
//! running after their job was cancelled). Every app-task exec therefore
//! carries a random per-execution marker, [`TASK_TOKEN_ENV`], in its
//! environment, which all of its descendants inherit. An exec that ended
//! without the task's own exit status is followed by one bounded
//! `docker exec` in the same container that signals exactly the processes
//! carrying that marker: SIGINT (what Ctrl-C would send; `act` cancels and
//! removes its job containers), then SIGTERM (background `&` children of a
//! non-interactive shell ignore SIGINT), then SIGKILL.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bosn_engine::{EngineEvent, RunOptions};
use kernal_api::async_engine::{CancellationSource, Sender};

use crate::task::{SetupAppTaskCommand, SetupAppTaskEngine};

/// Environment variable naming one app-task execution inside its container.
pub const TASK_TOKEN_ENV: &str = "BOSN_TASK_TOKEN";
/// Seconds each of SIGINT and SIGTERM is given before the next signal.
pub(crate) const STOP_GRACE_SECONDS: u32 = 10;
/// Budget for the stop exec: two grace periods, the SIGKILL check, and slack
/// for a busy engine.
pub(crate) const STOP_DEADLINE: Duration = Duration::from_secs(45);
const STOP_OUTPUT_LIMIT: usize = 16 * 1024;

/// `sh -c` program run in the container as `$0 TOKEN GRACE`. It exits 0
/// only when no process carrying the marker remains. It reads
/// `/proc/<pid>/environ`, so it needs only `sh`, `tr` and `grep`; the stop
/// exec itself never carries the marker, so it cannot match itself.
pub(crate) const STOP_SCRIPT: &str = r#"marker="BOSN_TASK_TOKEN=$1"
grace="$2"
signal() {
  hit=1
  for d in /proc/[0-9]*; do
    p="${d#/proc/}"
    [ "$p" = "$$" ] && continue
    if { tr '\000' '\n' < "$d/environ"; } 2>/dev/null | grep -qxF "$marker"; then
      kill "-$1" "$p" 2>/dev/null && hit=0
    fi
  done
  return "$hit"
}
gone_within() {
  i=0
  while [ "$i" -lt "$1" ]; do
    signal 0 || return 0
    sleep 1
    i=$((i + 1))
  done
  ! signal 0
}
if ! signal INT; then echo "bosn: no task processes were left in the container"; exit 0; fi
if gone_within "$grace"; then echo "bosn: task processes exited after SIGINT"; exit 0; fi
signal TERM
if gone_within "$grace"; then echo "bosn: task processes exited after SIGTERM"; exit 0; fi
signal KILL
if gone_within 2; then echo "bosn: task processes were killed after SIGKILL"; exit 0; fi
echo "bosn: task processes survived SIGKILL" >&2
exit 1
"#;

/// A marker unique to one execution. It names processes; it is not a secret.
pub(crate) fn new_task_token(container_name: &str, task_name: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let material = format!(
        "bosn-task-token-v1\0{container_name}\0{task_name}\0{}\0{nanos}\0{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let mut token = kernal_api::hash::sha256_bytes(material.as_bytes()).to_hex();
    token.truncate(32);
    token
}

/// Stop every process of one execution in its container. Returns true only
/// when the stop exec reports that none remain. It runs under its own budget:
/// the job's cancellation token has already fired by the time it is needed.
pub(crate) async fn stop_task_processes<E: SetupAppTaskEngine>(
    engine: &E,
    container_name: &str,
    task_token: &str,
    events: &Sender<EngineEvent>,
) -> bool {
    let independent = CancellationSource::new();
    let cancellation = independent.token();
    matches!(
        engine
            .stream(
                SetupAppTaskCommand::Stop {
                    container_name: container_name.to_owned(),
                    task_token: task_token.to_owned(),
                },
                RunOptions::streaming(STOP_DEADLINE, STOP_OUTPUT_LIMIT),
                &cancellation,
                events,
            )
            .await,
        Ok(result) if result.ok()
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    fn alive(pid: u32) -> bool {
        // A reaped or zombie child has no readable command line.
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|line| !line.is_empty())
    }

    fn spawn_tree(token: &str) -> Child {
        // A shell that ignores SIGINT and SIGTERM, plus a background child,
        // so only the last stage (SIGKILL) can end the whole tree.
        Command::new("sh")
            .arg("-c")
            .arg("trap '' INT TERM; sleep 300 & sleep 300; wait")
            .env(TASK_TOKEN_ENV, token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// (pid, ppid) for every process, from `/proc/<pid>/stat`.
    fn process_parents() -> Vec<(u32, u32)> {
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                // The command name may contain spaces; fields resume after ')'.
                let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?;
                Some((pid, ppid.parse().ok()?))
            })
            .collect()
    }

    fn descendants(pid: u32) -> Vec<u32> {
        let parents = process_parents();
        let mut found = Vec::new();
        let mut frontier = vec![pid];
        while let Some(parent) = frontier.pop() {
            for (child, _) in parents.iter().filter(|(_, ppid)| *ppid == parent) {
                found.push(*child);
                frontier.push(*child);
            }
        }
        found
    }

    #[test]
    fn the_stop_script_ends_only_the_marked_process_tree() {
        let mine = new_task_token("bosn-setup-v2-test", "check");
        let other = new_task_token("bosn-setup-v2-test", "check");
        assert_ne!(mine, other, "each execution gets its own marker");
        assert_eq!(mine.len(), 32);
        let mut target = spawn_tree(&mine);
        let mut bystander = spawn_tree(&other);
        // Wait until both shells have started their children.
        for _ in 0..100 {
            if descendants(target.id()).len() >= 2 && descendants(bystander.id()).len() >= 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let tree: Vec<u32> = std::iter::once(target.id())
            .chain(descendants(target.id()))
            .collect();
        assert!(tree.len() >= 3, "shell and both sleeps: {tree:?}");

        let output = Command::new("sh")
            .arg("-c")
            .arg(STOP_SCRIPT)
            .arg("bosn-stop")
            .arg(&mine)
            .arg("1")
            .env_remove(TASK_TOKEN_ENV)
            .output()
            .unwrap();
        let _ = target.wait();
        assert!(
            output.status.success(),
            "stop script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("SIGKILL"),
            "a tree that ignores INT and TERM ends only at SIGKILL: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        for pid in &tree[1..] {
            for _ in 0..50 {
                if !alive(*pid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(!alive(*pid), "marked process {pid} survived");
        }
        assert!(
            alive(bystander.id()),
            "a process with another marker was signalled"
        );

        // Nothing left: the stop is a successful no-op.
        let again = Command::new("sh")
            .arg("-c")
            .arg(STOP_SCRIPT)
            .arg("bosn-stop")
            .arg(&mine)
            .arg("1")
            .output()
            .unwrap();
        assert!(again.status.success());
        assert!(String::from_utf8_lossy(&again.stdout).contains("no task processes"));

        let bystander_children = descendants(bystander.id());
        let _ = bystander.kill();
        let _ = bystander.wait();
        for pid in bystander_children {
            let _ = Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .status();
        }
    }
}
