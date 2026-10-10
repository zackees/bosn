//! One CI engine per host Docker engine (#544), whichever daemon or state
//! directory asks for it.
//!
//! **The claim.** The singleton is decided by the host Docker engine itself:
//! a created, never started container named [`CLAIM_NAME`]. Docker refuses a
//! second container of the same name atomically, across daemons, state
//! directories, users and restarts, so exactly one daemon wins the right to
//! create the shared engine. The claim's labels name that engine and the
//! daemon holding it (registry, boot, pid and its start time), so a claim a
//! dead daemon left is recognised and taken over, never waited on forever.
//!
//! **Adoption.** A daemon that loses the claim runs in the claimed engine
//! when it would make a compatible one ([`EngineIdentity`]: the same pinned
//! act, engine and runner images and the same init command); otherwise it
//! asks the engine to retire once empty and waits.
//!
//! **Slots.** Every run, from any daemon, leases a slot inside the engine
//! under one lock there (`slots.sh`): `/run/bosn-slots/<n>` names its holder.
//! The slot fixes the run's artifact and cache ports, and the slot table is
//! what decides that an engine is idle, so no daemon retires an engine
//! another daemon's run is using.
//!
//! **Legacy engines** (per-run engines and spares of earlier releases, or of
//! `[engine] shared = false`) are drained, not killed: an engine running an
//! exec session or a job container is left to finish, and one that is idle
//! and older than [`LEGACY_GRACE`] is removed with its storage volume, each
//! removal verified ([`super::shared_engine`]).

use std::{collections::BTreeMap, time::Duration};

use bosn_registry::act::ActEngineIntent;

use super::engine::BoxFuture;

/// The name of the container that is the machine's engine claim.
pub const CLAIM_NAME: &str = "bosn-ci-engine-claim";
/// Marks the claim container.
pub const CLAIM_LABEL: &str = "com.zackees.bosn.ci-engine-claim";
const CLAIM_PREFIX: &str = "com.zackees.bosn.ci-engine-claim.";
/// A claim whose engine is still not running this long after it was made
/// is abandoned (the slowest engine preparation is 30 minutes).
pub const CLAIM_PREPARE_LIMIT: Duration = Duration::from_secs(45 * 60);
/// A legacy engine younger than this may still be being prepared for a
/// run, so it is never taken for idle.
pub const LEGACY_GRACE: Duration = Duration::from_secs(10 * 60);

/// The daemon process holding a claim or a slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Daemon {
    pub boot: String,
    pub pid: u32,
    pub start: u64,
}

impl Daemon {
    /// This process.
    pub fn current() -> Self {
        let pid = std::process::id();
        Self {
            boot: boot_id(),
            pid,
            start: start_time(pid).unwrap_or(0),
        }
    }

    /// Whether the process still runs. One from another boot or machine
    /// cannot be checked and counts as alive.
    pub fn alive(&self) -> bool {
        self.boot != boot_id() || start_time(self.pid) == Some(self.start)
    }

    fn parse(boot: &str, pid: &str, start: &str) -> Result<Self, String> {
        let token = |v: &str| !v.is_empty() && v.bytes().all(|b| b.is_ascii_graphic());
        if !token(boot) {
            return Err("bad daemon boot id".into());
        }
        Ok(Self {
            boot: boot.into(),
            pid: pid.parse().map_err(|_| "bad daemon pid")?,
            start: start.parse().map_err(|_| "bad daemon start time")?,
        })
    }
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|id| id.trim().to_owned())
        .ok()
        .filter(|id| !id.is_empty() && !id.contains(char::is_whitespace))
        .unwrap_or_else(|| "unknown".into())
}

/// A process's start time in clock ticks since boot (`/proc/<pid>/stat`
/// field 22), which tells a live process from a reused pid.
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19)?.parse().ok()
}

/// What makes two engines interchangeable for a run: the pins and the init
/// command (cache, tool generation and socket layout). Sizing may differ;
/// a run is sized from the engine it gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineIdentity {
    act_version: String,
    act_image: String,
    engine_image: String,
    runner_image: String,
    init_command: String,
}

impl EngineIdentity {
    pub fn of(intent: &ActEngineIntent) -> Option<Self> {
        Some(Self {
            act_version: intent.act_version.clone(),
            act_image: intent.act_image_digest.clone(),
            engine_image: intent.engine_image_digest.clone(),
            runner_image: intent.runner_image_digest.clone(),
            init_command: intent
                .creation_profile
                .as_ref()?
                .init_command_sha256
                .clone(),
        })
    }

    /// From an engine container's ownership labels.
    pub fn from_labels(labels: &BTreeMap<String, String>) -> Option<Self> {
        let get = |key: &str| labels.get(&format!("com.zackees.bosn.act.{key}")).cloned();
        Some(Self {
            act_version: get("act-version")?,
            act_image: get("act-image")?,
            engine_image: get("engine-image")?,
            runner_image: get("runner-image")?,
            init_command: get("init-command-sha256")?,
        })
    }
}

/// The machine's engine claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineClaim {
    /// The claim container's ID (empty before it is created).
    pub id: String,
    /// The engine container's name.
    pub engine: String,
    /// The registry of the daemon that holds it.
    pub registry: String,
    pub daemon: Daemon,
    /// Unix seconds.
    pub created: u64,
}

impl EngineClaim {
    pub fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::from([(CLAIM_LABEL.to_owned(), "v1".to_owned())]);
        for (key, value) in [
            ("engine", self.engine.clone()),
            ("registry", self.registry.clone()),
            ("boot", self.daemon.boot.clone()),
            ("pid", self.daemon.pid.to_string()),
            ("start", self.daemon.start.to_string()),
            ("created", self.created.to_string()),
        ] {
            labels.insert(format!("{CLAIM_PREFIX}{key}"), value);
        }
        labels
    }

    pub fn from_labels(id: &str, labels: &BTreeMap<String, String>) -> Result<Self, String> {
        let get = |key: &str| {
            labels
                .get(&format!("{CLAIM_PREFIX}{key}"))
                .map(String::as_str)
                .ok_or_else(|| format!("the engine claim has no {key} label"))
        };
        if labels.get(CLAIM_LABEL).map(String::as_str) != Some("v1") {
            return Err(format!("{CLAIM_NAME} is not a bosn engine claim"));
        }
        let engine = get("engine")?;
        if !engine.starts_with("bosn-act-") {
            return Err("the engine claim names no act engine".into());
        }
        Ok(Self {
            id: id.into(),
            engine: engine.into(),
            registry: get("registry")?.into(),
            daemon: Daemon::parse(get("boot")?, get("pid")?, get("start")?)?,
            created: get("created")?.parse().map_err(|_| "bad claim time")?,
        })
    }

    /// Held by this registry in this process.
    pub fn ours(&self, registry: &str) -> bool {
        self.registry == registry && self.daemon == Daemon::current()
    }
}

/// An act engine container as Docker reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Foreign {
    pub id: String,
    pub name: String,
    pub running: bool,
    pub identity: Option<EngineIdentity>,
    /// The registry its ownership labels name.
    pub registry: String,
    /// The host directory bound at the engine's socket directory.
    pub socket_dir: Option<String>,
    /// Its named storage volume, removed with it.
    pub storage_volume: Option<String>,
    pub memory_bytes: u64,
    pub nano_cpus: u64,
    pub pids: u64,
    /// Unix seconds.
    pub created: u64,
}

/// A run holding a slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub run: String,
    pub registry: String,
    pub daemon: Daemon,
}

impl Holder {
    /// One line, spaces between fields (each is a token).
    pub fn line(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.run, self.registry, self.daemon.boot, self.daemon.pid, self.daemon.start
        )
    }

    fn parse(fields: &[&str]) -> Result<Self, String> {
        let [run, registry, boot, pid, start] = fields else {
            return Err("bad slot holder".into());
        };
        Ok(Self {
            run: (*run).into(),
            registry: (*registry).into(),
            daemon: Daemon::parse(boot, pid, start)?,
        })
    }
}

/// The answer to a slot lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Leased {
    Slot(u16),
    /// Its maker has not finished preparing it (act, runner image).
    Preparing,
    /// The engine is being retired; no new run.
    Retiring,
    Full,
}

/// The engine's slot table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Survey {
    pub held: Vec<(u16, Holder)>,
    /// Seconds since a slot was last leased or released.
    pub idle_secs: u64,
    /// A daemon wants it retired once empty (it needs another engine).
    pub requested: bool,
    pub retiring: bool,
}

/// The in-engine slot protocol's replies, parsed eagerly.
pub mod reply {
    use super::*;

    pub fn leased(out: &str) -> Result<Leased, String> {
        match out.split_whitespace().collect::<Vec<_>>().as_slice() {
            ["slot", n] => n
                .parse()
                .map(Leased::Slot)
                .map_err(|_| format!("bad slot reply {out:?}")),
            ["preparing"] => Ok(Leased::Preparing),
            ["retiring"] => Ok(Leased::Retiring),
            ["full"] => Ok(Leased::Full),
            _ => Err(format!("bad slot reply {out:?}")),
        }
    }

    pub fn survey(out: &str) -> Result<Survey, String> {
        let mut survey = Survey::default();
        let mut idle = None;
        for line in out.lines() {
            match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                ["held", slot, holder @ ..] => survey.held.push((
                    slot.parse().map_err(|_| format!("bad slot {slot:?}"))?,
                    Holder::parse(holder)?,
                )),
                ["idle", secs] => idle = secs.parse().ok(),
                ["requested"] => survey.requested = true,
                ["retiring"] => survey.retiring = true,
                [] => {}
                _ => return Err(format!("bad slot survey line {line:?}")),
            }
        }
        survey.idle_secs = idle.ok_or("the slot survey has no idle time")?;
        Ok(survey)
    }
}

/// The host Docker engine, as the singleton needs it.
pub trait MachineEngine: Send + Sync {
    /// The claim, if there is one.
    fn claim(&self) -> BoxFuture<'_, Result<Option<EngineClaim>, String>>;
    /// Create the claim; `false` when another exists (Docker's name conflict).
    fn create_claim<'a>(&'a self, claim: &'a EngineClaim) -> BoxFuture<'a, Result<bool, String>>;
    /// Remove exactly the claim container `id` (absent is fine).
    fn remove_claim<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// The act engine container `name`, if it exists.
    fn inspect<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Foreign>, String>>;
    /// Every running act engine container.
    fn engines(&self) -> BoxFuture<'_, Result<Vec<Foreign>, String>>;
    /// Whether an engine runs anything beyond its own daemons: an exec
    /// session (act, a preparation step) or a job container.
    fn busy<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<bool, String>>;
    /// Remove an act engine container and its storage volume, and prove
    /// both gone.
    fn remove_engine<'a>(&'a self, engine: &'a Foreign) -> BoxFuture<'a, Result<(), String>>;
    fn lease_slot<'a>(
        &'a self,
        engine: &'a str,
        holder: &'a Holder,
    ) -> BoxFuture<'a, Result<Leased, String>>;
    fn release_slot<'a>(&'a self, engine: &'a str, slot: u16) -> BoxFuture<'a, Result<(), String>>;
    fn survey<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<Survey, String>>;
    /// Open the engine to runs: its maker prepared it.
    fn mark_ready<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// Ask the engine's owner to retire it once empty.
    fn request_retire<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// Mark the engine retiring when no slot is held, atomically with
    /// leasing; whether it was marked.
    fn begin_retire<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<bool, String>>;
}

#[cfg(test)]
mod tests;
