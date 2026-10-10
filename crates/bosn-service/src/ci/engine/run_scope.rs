//! One run's share of an engine (#547): its cgroup with memory, CPU and
//! process limits, its network, work tree, artifact and cache server ports,
//! and the Docker proxy socket it reaches Docker through. Everything a run
//! makes inside the engine carries its run label, so closing the scope
//! removes exactly that set and leaves other runs untouched.
//!
//! Every name is derived from the run's ID and slot here, once; the scripts
//! below are the only in-engine commands that create or remove them.

use bosn_registry::act::ENGINE_SOCKET_DIR;

use super::ENGINE_WORK;
use crate::docker_api::LABEL_RUN;

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

    /// `cpu.max`: the quota per 100 ms period.
    fn cpu_max(self) -> String {
        format!("{} 100000", self.nano_cpus / 10_000)
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

    /// The environment act runs with: Docker only through the run's proxy.
    pub fn act_env(&self) -> Vec<String> {
        vec![format!("DOCKER_HOST=unix://{}", self.socket())]
    }

    /// The prefix that moves the command into the run's cgroup (its `act`
    /// leaf: a cgroup with enabled controllers holds no processes itself)
    /// before it starts.
    pub fn enter(&self) -> Vec<String> {
        vec![
            "sh".into(),
            "-ec".into(),
            format!(
                "echo $$ > /sys/fs/cgroup{}/act/cgroup.procs; exec \"$@\"",
                self.cgroup()
            ),
            "bosn-run-enter".into(),
        ]
    }

    /// Create the run's cgroup with its limits, its directories and its
    /// labelled network. Idempotent.
    pub fn open_script(&self) -> String {
        let cg = format!("/sys/fs/cgroup{}", self.cgroup());
        let work = self.work();
        let limits = self.limits;
        format!(
            "set -eu; cg={cg}; mkdir -p \"$cg/act\"; \
             echo '+cpu +memory +pids' > \"$cg/cgroup.subtree_control\"; \
             echo {memory} > \"$cg/memory.max\"; \
             if [ -e \"$cg/memory.swap.max\" ]; then echo 0 > \"$cg/memory.swap.max\"; fi; \
             echo '{cpu}' > \"$cg/cpu.max\"; echo {pids} > \"$cg/pids.max\"; \
             mkdir -p {work}/src {work}/overlay {work}/artifacts {work}/home/.cache {work}/home/.config {work}/tmp; \
             docker network inspect {net} >/dev/null 2>&1 || \
               docker network create --label {LABEL_RUN}={label} {net} >/dev/null",
            memory = limits.memory_bytes,
            cpu = limits.cpu_max(),
            pids = limits.pids,
            net = self.network(),
            label = self.label,
        )
    }

    /// Stop everything the run left: kill its act process tree, remove every
    /// container, network and volume with its label, its work tree and its
    /// cgroup; then prove none is left. Idempotent.
    pub fn close_script(&self) -> String {
        let cg = format!("/sys/fs/cgroup{}", self.cgroup());
        let filter = format!("--filter label={LABEL_RUN}={}", self.label);
        format!(
            "set -u; cg={cg}; \
             if [ -e \"$cg/act/cgroup.kill\" ]; then echo 1 > \"$cg/act/cgroup.kill\" || :; fi; \
             ids=$(docker ps -aq {filter}); [ -z \"$ids\" ] || docker rm -fv $ids >/dev/null || :; \
             nets=$(docker network ls -q {filter}); [ -z \"$nets\" ] || docker network rm $nets >/dev/null || :; \
             vols=$(docker volume ls -q {filter}); [ -z \"$vols\" ] || docker volume rm -f $vols >/dev/null || :; \
             rm -rf {work}; \
             tries=0; while [ -d \"$cg\" ]; do \
               find \"$cg\" -depth -type d -exec rmdir {{}} \\; 2>/dev/null || :; \
               [ -d \"$cg\" ] || break; tries=$((tries + 1)); \
               [ \"$tries\" -lt 100 ] || {{ echo \"run cgroup $cg is still busy\" >&2; exit 1; }}; \
               sleep 0.1; \
             done; \
             left=\"$(docker ps -aq {filter})$(docker network ls -q {filter})$(docker volume ls -q {filter})\"; \
             [ -z \"$left\" ] || {{ echo 'run objects remain after cleanup' >&2; exit 1; }}; \
             [ ! -e {work} ] || {{ echo 'run work tree remains after cleanup' >&2; exit 1; }}",
            work = self.work(),
        )
    }
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
        assert_eq!(sized.cpu_max(), "400000 100000");
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
        assert_eq!(
            scope.act_env(),
            ["DOCKER_HOST=unix:///bosn/sock/0a1b2c3d4e5f.sock"]
        );
        assert!(scope.enter()[2].contains("/sys/fs/cgroup/bosn-run-0a1b2c3d4e5f/act/cgroup.procs"));
    }

    #[test]
    fn scripts_touch_only_the_runs_own_objects() {
        let scope = RunScope::new(RUN, 0, limits()).unwrap();
        let open = scope.open_script();
        assert!(
            open.contains("echo 7516192768 > \"$cg/memory.max\""),
            "{open}"
        );
        assert!(
            open.contains("echo '400000 100000' > \"$cg/cpu.max\""),
            "{open}"
        );
        assert!(open.contains("echo 3840 > \"$cg/pids.max\""), "{open}");
        assert!(open.contains(&format!("--label {LABEL_RUN}={RUN} bosn-run-0a1b2c3d4e5f")));
        let close = scope.close_script();
        let filter = format!("--filter label={LABEL_RUN}={RUN}");
        assert_eq!(close.matches(&filter).count(), 6, "{close}");
        assert!(close.contains(&format!("rm -rf {ENGINE_WORK}/runs/0a1b2c3d4e5f;")));
        assert!(!close.contains("docker system prune"));
    }

    /// The scripts are valid POSIX shell.
    #[cfg(unix)]
    #[test]
    fn scripts_parse_as_shell() {
        let scope = RunScope::new(RUN, 0, limits()).unwrap();
        for script in [
            scope.open_script(),
            scope.close_script(),
            scope.enter()[2].clone(),
        ] {
            let status = std::process::Command::new("sh")
                .args(["-n", "-c", &script])
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
        }
    }
}
