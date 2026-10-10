//! One run's share of an engine (#547): its cgroup with memory, CPU and
//! process limits, its network, work tree, artifact and cache server ports,
//! and the Docker proxy socket it reaches Docker through. Everything a run
//! makes inside the engine carries its run label, so closing the scope
//! removes exactly that set and leaves other runs untouched.
//!
//! act2's serve mode owns the scope inside the engine (zackees/act2#62):
//! admission, the cgroup and its limits, the network, the work tree, act's
//! process, and the label cleanup that proves nothing is left. Bosn only
//! forwards: it starts `act serve` once per engine, then admits, executes in
//! and closes each run through `act serve` client commands. The names here
//! are what serve derives from the same run ID and slot (`--scope-prefix
//! bosn-run-`, `--work-root {ENGINE_WORK}/runs`); an admission that answers
//! with any other scope is refused.

use bosn_registry::act::ENGINE_SOCKET_DIR;

use serde::Deserialize;

use super::ENGINE_WORK;
use crate::docker_api::LABEL_RUN;

/// The act2 serve socket and log, on the engine's writable storage.
const SERVE_DIR: &str = "/var/lib/docker/bosn-ci/serve";
/// Each run's cgroup and network name prefix, as serve is told.
const SCOPE_PREFIX: &str = "bosn-run-";

/// The first port of the per-run pairs: slot `n` serves artifacts on
/// `BASE + 2n` and act's cache on `BASE + 2n + 1`.
const PORT_BASE: u16 = 40000;
/// How many runs one engine can hold at once (ports and slots).
pub const MAX_SLOTS: u16 = 256;
/// Memory, process and CPU room left to the engine's own daemons when a run
/// is sized from the whole engine.
const ENGINE_MEMORY_RESERVE: u64 = 1 << 30;
const ENGINE_PIDS_RESERVE: u64 = 256;
/// The process count a run never goes under.
const PIDS_FLOOR: u64 = 256;
/// The memory a run never goes under.
const MEMORY_FLOOR: u64 = 512 << 20;

/// The limits written to a run's cgroup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunLimits {
    pub memory_bytes: u64,
    pub nano_cpus: u64,
    pub pids: u64,
}

impl RunLimits {
    /// A run sized from the whole engine (`memory`, `nano_cpus`, `pids`),
    /// leaving the engine's own daemons room.
    pub fn within_engine(memory: u64, nano_cpus: u64, pids: u64) -> Self {
        Self {
            memory_bytes: memory
                .saturating_sub(ENGINE_MEMORY_RESERVE)
                .max(MEMORY_FLOOR),
            nano_cpus: nano_cpus.max(10_000_000),
            pids: pids.saturating_sub(ENGINE_PIDS_RESERVE).max(PIDS_FLOOR),
        }
    }
}

/// One run's names and limits inside an engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunScope {
    /// Twelve hex digits of the run ID: short, for socket paths.
    key: String,
    /// The run ID itself, the value of every object's run label.
    label: String,
    slot: u16,
    limits: RunLimits,
}

impl RunScope {
    pub fn new(run_id: &str, slot: u16, limits: RunLimits) -> Result<Self, String> {
        let key: String = run_id.chars().filter(|c| *c != '-').take(12).collect();
        let canonical = run_id.len() == 36
            && run_id
                .bytes()
                .all(|b| b == b'-' || b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !canonical || key.len() != 12 {
            return Err(format!("run ID {run_id:?} is not a canonical UUID"));
        }
        if slot >= MAX_SLOTS {
            return Err(format!("run slot {slot} is out of range"));
        }
        Ok(Self {
            key,
            label: run_id.into(),
            slot,
            limits,
        })
    }

    pub fn key(&self) -> &str {
        &self.key
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn limits(&self) -> RunLimits {
        self.limits
    }
    /// The run's cgroup, as the engine's (namespaced) cgroup tree names it,
    /// and the Docker `--cgroup-parent` of every container the run makes.
    pub fn cgroup(&self) -> String {
        format!("/bosn-run-{}", self.key)
    }
    pub fn network(&self) -> String {
        format!("bosn-run-{}", self.key)
    }
    /// The run's work tree, under the engine's work root.
    pub fn work(&self) -> String {
        format!("{ENGINE_WORK}/{}", self.work_relative())
    }
    /// The work tree relative to [`ENGINE_WORK`].
    pub fn work_relative(&self) -> String {
        format!("runs/{}", self.key)
    }
    /// The run's Docker proxy, as the engine sees it.
    pub fn socket(&self) -> String {
        format!("{ENGINE_SOCKET_DIR}/{}", self.socket_name())
    }
    /// The proxy socket's file name in the engine's socket directory.
    pub fn socket_name(&self) -> String {
        format!("{}.sock", self.key)
    }
    pub fn artifact_port(&self) -> u16 {
        PORT_BASE + 2 * self.slot
    }
    pub fn cache_port(&self) -> u16 {
        PORT_BASE + 2 * self.slot + 1
    }

    /// act's arguments for the scope: its network, ports and Docker socket.
    pub fn act_args(&self) -> Vec<String> {
        vec![
            "--network".into(),
            self.network(),
            "--artifact-server-port".into(),
            self.artifact_port().to_string(),
            "--cache-server-port".into(),
            self.cache_port().to_string(),
            "--container-daemon-socket".into(),
            format!("unix://{}", self.socket()),
        ]
    }

    /// The `act serve` client arguments every serve call starts with.
    fn serve(&self) -> String {
        format!("{ENGINE_WORK}/bin/act serve --socket {SERVE_DIR}/serve.sock")
    }

    /// The arguments, after the act binary, that run act through serve in
    /// this scope: Docker only through the run's proxy, and each secret
    /// passed by name from the client's environment (never argv).
    pub fn exec_args<'a>(&self, secrets: impl Iterator<Item = &'a str>) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "serve".into(),
            "--socket".into(),
            format!("{SERVE_DIR}/serve.sock"),
            "exec".into(),
            "--run".into(),
            self.label.clone(),
            "--env".into(),
            format!("DOCKER_HOST=unix://{}", self.socket()),
        ];
        for name in secrets {
            args.extend(["--env-from".into(), name.to_owned()]);
        }
        args.push("--".into());
        args
    }

    /// Start `act serve` in the engine unless it answers (one at a time:
    /// a second serve would reap the first's runs), then admit the run
    /// into this scope's slot with its limits and print serve's scope.
    pub fn admit_script(&self) -> String {
        let serve = self.serve();
        let limits = self.limits;
        format!(
            "set -eu; mkdir -p {SERVE_DIR}; exec 7>{SERVE_DIR}/start.lock; flock -x 7; \
             if ! {serve} status >/dev/null 2>&1; then \
               setsid {serve} --run-label {LABEL_RUN} --scope-prefix {SCOPE_PREFIX} \
                 --work-root {ENGINE_WORK}/runs --port-base {PORT_BASE} --max-runs {MAX_SLOTS} \
                 </dev/null >>{SERVE_DIR}/serve.log 2>&1 7>&- & \
               tries=0; until {serve} status >/dev/null 2>&1; do \
                 tries=$((tries + 1)); \
                 [ \"$tries\" -lt 300 ] || {{ tail -n 20 {SERVE_DIR}/serve.log >&2; echo 'act serve did not start' >&2; exit 1; }}; \
                 sleep 0.1; \
               done; \
             fi; exec 7>&-; \
             printf '%s' '{{\"run_id\":\"{label}\",\"slot\":{slot},\"limits\":{{\"memory_bytes\":{memory},\"nano_cpus\":{cpus},\"pids\":{pids}}}}}' \
               | {serve} admit",
            label = self.label,
            slot = self.slot,
            memory = limits.memory_bytes,
            cpus = limits.nano_cpus,
            pids = limits.pids,
        )
    }

    /// Refuse an admission whose scope is not the one Bosn's proxy and act
    /// arguments were built for.
    pub fn verify_admitted(&self, reply: &str) -> Result<(), String> {
        let admitted: Admitted = serde_json::from_str(reply.trim())
            .map_err(|error| format!("act serve admit: {error}"))?;
        let expected = Admitted {
            run_id: self.label.clone(),
            slot: self.slot,
            cgroup: self.cgroup(),
            network: self.network(),
            work: self.work(),
            artifact_port: self.artifact_port(),
            cache_port: self.cache_port(),
        };
        if admitted == expected {
            Ok(())
        } else {
            Err(format!(
                "act serve admitted {admitted:?}, expected {expected:?}"
            ))
        }
    }

    /// Kill the run's processes and remove everything it left, proven gone
    /// by serve. Idempotent.
    pub fn close_script(&self) -> String {
        format!("{} close --run {}", self.serve(), self.label)
    }
}

/// The fields of serve's scope reply that Bosn depends on.
#[derive(Debug, Deserialize, PartialEq, Eq)]
struct Admitted {
    run_id: String,
    slot: u16,
    cgroup: String,
    network: String,
    work: String,
    artifact_port: u16,
    cache_port: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: &str = "0a1b2c3d-4e5f-4a6b-8c7d-000000000001";

    fn limits() -> RunLimits {
        RunLimits::within_engine(8 << 30, 4_000_000_000, 4096)
    }

    #[test]
    fn names_derive_from_the_run_and_fit_a_unix_socket() {
        let scope = RunScope::new(RUN, 3, limits()).unwrap();
        assert_eq!(scope.key(), "0a1b2c3d4e5f");
        assert_eq!(scope.cgroup(), "/bosn-run-0a1b2c3d4e5f");
        assert_eq!(scope.network(), "bosn-run-0a1b2c3d4e5f");
        assert_eq!(scope.work(), format!("{ENGINE_WORK}/runs/0a1b2c3d4e5f"));
        assert_eq!(scope.socket(), "/bosn/sock/0a1b2c3d4e5f.sock");
        assert!(scope.socket().len() < 108);
        assert_eq!((scope.artifact_port(), scope.cache_port()), (40006, 40007));
        let last = RunScope::new(RUN, MAX_SLOTS - 1, limits()).unwrap();
        assert!(last.cache_port() < u16::MAX);
    }

    #[test]
    fn concurrent_slots_never_share_a_port() {
        let ports: std::collections::BTreeSet<u16> = (0..MAX_SLOTS)
            .flat_map(|slot| {
                let scope = RunScope::new(RUN, slot, limits()).unwrap();
                [scope.artifact_port(), scope.cache_port()]
            })
            .collect();
        assert_eq!(ports.len(), 2 * usize::from(MAX_SLOTS));
    }

    #[test]
    fn only_canonical_runs_and_slots_in_range_are_scoped() {
        assert!(RunScope::new("not-a-run", 0, limits()).is_err());
        assert!(RunScope::new(&RUN.to_uppercase(), 0, limits()).is_err());
        assert!(RunScope::new("0a1b2c3d-4e5f-4a6b-8c7d-00000000000;", 0, limits()).is_err());
        assert!(RunScope::new(RUN, MAX_SLOTS, limits()).is_err());
    }

    #[test]
    fn a_run_sized_from_the_engine_leaves_its_daemons_room() {
        let sized = RunLimits::within_engine(8 << 30, 4_000_000_000, 4096);
        assert_eq!(sized.memory_bytes, 7 << 30);
        assert_eq!(sized.pids, 4096 - 256);
        assert_eq!(sized.nano_cpus, 4_000_000_000);
        let tiny = RunLimits::within_engine(1 << 20, 0, 10);
        assert_eq!((tiny.memory_bytes, tiny.pids), (512 << 20, 256));
    }

    #[test]
    fn act_reaches_docker_only_through_the_run_proxy() {
        let scope = RunScope::new(RUN, 0, limits()).unwrap();
        let args = scope.act_args();
        let after = |flag: &str| {
            let at = args.iter().position(|a| a == flag).unwrap();
            args[at + 1].clone()
        };
        assert_eq!(after("--network"), "bosn-run-0a1b2c3d4e5f");
        assert_eq!(after("--artifact-server-port"), "40000");
        assert_eq!(after("--cache-server-port"), "40001");
        assert_eq!(
            after("--container-daemon-socket"),
            "unix:///bosn/sock/0a1b2c3d4e5f.sock"
        );
        let exec = scope.exec_args(["TOKEN"].into_iter());
        assert_eq!(
            exec,
            [
                "serve",
                "--socket",
                "/var/lib/docker/bosn-ci/serve/serve.sock",
                "exec",
                "--run",
                RUN,
                "--env",
                "DOCKER_HOST=unix:///bosn/sock/0a1b2c3d4e5f.sock",
                "--env-from",
                "TOKEN",
                "--",
            ]
        );
    }

    #[test]
    fn serve_is_started_once_and_admits_this_scope() {
        let scope = RunScope::new(RUN, 3, limits()).unwrap();
        let admit = scope.admit_script();
        assert!(admit.contains("flock -x 7"), "{admit}");
        assert!(admit.contains(&format!("--run-label {LABEL_RUN} --scope-prefix bosn-run-")));
        assert!(
            admit.contains(
                "--work-root /var/lib/docker/bosn-ci/runs --port-base 40000 --max-runs 256"
            )
        );
        assert!(admit.contains(&format!(
            "{{\"run_id\":\"{RUN}\",\"slot\":3,\"limits\":{{\"memory_bytes\":7516192768,\"nano_cpus\":4000000000,\"pids\":3840}}}}"
        )), "{admit}");
        assert_eq!(
            scope.close_script(),
            format!(
                "/var/lib/docker/bosn-ci/bin/act serve --socket /var/lib/docker/bosn-ci/serve/serve.sock close --run {RUN}"
            )
        );
    }

    #[test]
    fn only_the_expected_admission_is_accepted() {
        let scope = RunScope::new(RUN, 3, limits()).unwrap();
        let reply = format!(
            r#"{{"run_id":"{RUN}","key":"0a1b2c3d4e5f","slot":3,"cgroup":"/bosn-run-0a1b2c3d4e5f","network":"bosn-run-0a1b2c3d4e5f","work":"/var/lib/docker/bosn-ci/runs/0a1b2c3d4e5f","artifact_port":40006,"cache_port":40007,"limits":{{"memory_bytes":1,"nano_cpus":1,"pids":1}}}}"#
        );
        scope.verify_admitted(&reply).unwrap();
        let moved = reply.replace("\"slot\":3", "\"slot\":4");
        assert!(scope.verify_admitted(&moved).is_err());
        assert!(scope.verify_admitted("not json").is_err());
    }

    /// The scripts are valid POSIX shell.
    #[cfg(unix)]
    #[test]
    fn scripts_parse_as_shell() {
        let scope = RunScope::new(RUN, 0, limits()).unwrap();
        for script in [scope.admit_script(), scope.close_script()] {
            let status = std::process::Command::new("sh")
                .args(["-n", "-c", &script])
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
        }
    }
}
