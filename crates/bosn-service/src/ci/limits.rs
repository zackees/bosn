//! How big one `bosn ci` engine may grow, sized from the engine's host.
//!
//! Memory: half the host's total, but no more than three quarters of what is
//! available right now, rounded down to [`MEMORY_STEP`] and held between
//! [`MEMORY_FLOOR`] and [`MEMORY_CEILING`]. It reserves nothing until it is
//! written; it only bounds a runaway job, so a large host gets a large bound.
//!
//! Storage (the engine's `/var/lib/docker`) is disk by default (#425): an
//! anonymous volume on the host engine's disk, removed with the engine, so a
//! build's `target/` dirs never compete with the host for RAM. Its size is
//! the host's free disk under Docker's root, less [`DISK_MARGIN`], rounded
//! down to [`DISK_STEP`] and capped at [`DISK_STORAGE_CEILING`]; it is a
//! budget checked before the run, not a quota. When free disk cannot be read
//! (Docker Desktop's VM, a remote engine) the budget is
//! [`DISK_STORAGE_UNKNOWN`]. When the budget is under [`DISK_STORAGE_FLOOR`],
//! storage falls back to a RAM tmpfs inside the memory limit: three quarters
//! of memory, always leaving [`MEMORY_HEADROOM`].
//!
//! Every value can be pinned in `<state>/config.toml`:
//!
//! ```toml
//! [engine]
//! memory_gib = 16   # the engine's whole memory limit
//! storage_gib = 10  # its storage (a disk budget, or a tmpfs size in memory)
//! storage = "disk"  # or "memory"; default: disk when the host has room
//! cpus = 4
//! pids = 4096
//! spares = 0        # opt out of the prepared spare engine (#410; default 1)
//! shared = false     # one engine per run, not the shared engine (#547)
//! idle_retire_secs = 600  # how long an idle shared engine is kept
//! ```
//!
//! Pinned memory-backed storage grows a sized memory limit to fit it (by the
//! same three quarters); a pinned disk budget must fit the free disk. The
//! chosen limits are frozen into each run's creation profile before its
//! intent commits, and creation and every inspection verify the engine
//! against that profile exactly; changing the host or the config never
//! changes an existing record.

use serde::Deserialize;

use crate::act_engine::{ActEngineLimits, EngineStorage};

const GIB: u64 = 1 << 30;
/// The least memory an engine is sized to, however small the host.
pub const MEMORY_FLOOR: u64 = 4 * GIB;
/// The most memory an engine is sized to, however large the host.
pub const MEMORY_CEILING: u64 = 48 * GIB;
/// Sized memory is a whole multiple of this. Available memory changes from one
/// sample to the next, and a spare engine is claimed only by a run whose
/// limits match it exactly, so unrounded memory retired the spare on almost
/// every run (#553).
pub const MEMORY_STEP: u64 = 4 * GIB;
/// Memory always left outside a storage tmpfs.
pub const MEMORY_HEADROOM: u64 = 2 * GIB;
/// The least private storage an engine is sized to.
pub const STORAGE_FLOOR: u64 = GIB;
/// Free disk always left to the host beyond a disk-backed engine's budget.
pub const DISK_MARGIN: u64 = 16 * GIB;
/// The most a disk-backed engine is sized to, however much disk is free.
pub const DISK_STORAGE_CEILING: u64 = 128 * GIB;
/// The least disk budget worth choosing over a RAM tmpfs.
pub const DISK_STORAGE_FLOOR: u64 = 24 * GIB;
/// The budget when the host's free disk cannot be read.
pub const DISK_STORAGE_UNKNOWN: u64 = 64 * GIB;
/// Disk budgets are whole multiples of this, so small changes in free disk
/// do not change the engine (and retire a prepared spare).
pub const DISK_STEP: u64 = 8 * GIB;
pub const CPU_CEILING: u64 = 8;
pub const DEFAULT_PIDS: u64 = 4096;

/// What the engine's host offers, as the host engine reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostResources {
    pub total_memory: u64,
    /// Memory available now; the total when the host cannot say.
    pub available_memory: u64,
    pub cpus: u64,
    /// Free disk under Docker's root directory, when this host can read it.
    pub available_disk: Option<u64>,
}

impl HostResources {
    /// From `docker info --format '{{.MemTotal}} {{.NCPU}} {{.DockerRootDir}}'`
    /// (the engine's host, which under Docker Desktop is its VM) and, when it
    /// describes the same machine, this host's `/proc/meminfo` and
    /// `free_disk` of Docker's root directory.
    pub fn parse(
        info: &str,
        meminfo: Option<&str>,
        free_disk: impl FnOnce(&str) -> Option<u64>,
    ) -> Result<Self, String> {
        let mut fields = info.trim().splitn(3, ' ');
        let (Some(total), Some(cpus)) = (fields.next(), fields.next()) else {
            return Err(format!("unexpected docker info: {info:?}"));
        };
        let root = fields.next().map(str::trim).filter(|root| !root.is_empty());
        if root.is_some_and(|root| !root.starts_with('/')) {
            return Err(format!("unexpected docker info: {info:?}"));
        }
        let total_memory: u64 = total
            .parse()
            .map_err(|_| format!("docker info MemTotal is not a byte count: {total:?}"))?;
        let cpus: u64 = cpus
            .parse()
            .map_err(|_| format!("docker info NCPU is not a count: {cpus:?}"))?;
        if total_memory == 0 || cpus == 0 {
            return Err(format!("docker info reports an empty host: {info:?}"));
        }
        let local = meminfo
            .and_then(Meminfo::parse)
            .filter(|local| local.total == total_memory);
        let available_memory = local
            .as_ref()
            .map_or(total_memory, |local| local.available.min(total_memory));
        // Only this machine's filesystem says anything about the engine's.
        let available_disk = local.and(root).and_then(free_disk);
        Ok(Self {
            total_memory,
            available_memory,
            cpus,
            available_disk,
        })
    }
}

/// The two `/proc/meminfo` lines sizing reads, in bytes.
struct Meminfo {
    total: u64,
    available: u64,
}

impl Meminfo {
    fn parse(text: &str) -> Option<Self> {
        let field = |name: &str| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix(name)?.strip_prefix(':')?;
                let kib = rest.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
                kib.checked_mul(1024)
            })
        };
        Some(Self {
            total: field("MemTotal")?,
            available: field("MemAvailable")?,
        })
    }
}

/// `[engine] storage`: what backs the engine's storage.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum StorageBacking {
    Disk,
    Memory,
}

/// `[engine]` in `<state>/config.toml`: explicit overrides of the sized
/// limits. An override is taken as written (still bounded by the engine
/// layer's validation); anything left out is sized from the host.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub memory_gib: Option<u64>,
    pub storage_gib: Option<u64>,
    /// Disk or memory; unset chooses disk when the host has room for it.
    pub storage: Option<StorageBacking>,
    pub cpus: Option<u64>,
    pub pids: Option<u64>,
    /// Prepared spare engines kept (#410); `spares = 0` opts out.
    #[serde(default)]
    pub spares: super::spare::Spares,
    /// One long-lived engine shared by concurrent runs (#547); `false`
    /// gives each run its own engine. Default: on, where the daemon can
    /// share a Docker socket with its engines (Linux).
    pub shared: Option<bool>,
    /// Seconds a shared engine with no run is kept before it is retired;
    /// default 600.
    pub idle_retire_secs: Option<u64>,
}

/// The disk budget for `host`, or why disk-backed storage cannot be used.
/// `pinned` is a `storage_gib` override.
fn disk_budget(host: HostResources, pinned: Option<u64>) -> Result<u64, String> {
    let room = host
        .available_disk
        .map(|free| free.saturating_sub(DISK_MARGIN));
    match (pinned, room) {
        (Some(pinned), Some(room)) if pinned > room => Err(format!(
            "[engine] storage_gib = {} needs {} GiB of free disk on the Docker host \
             (with a {} GiB margin), which has {} GiB",
            pinned / GIB,
            (pinned + DISK_MARGIN) / GIB,
            DISK_MARGIN / GIB,
            host.available_disk.unwrap_or(0) / GIB
        )),
        (Some(pinned), _) => Ok(pinned),
        (None, None) => Ok(DISK_STORAGE_UNKNOWN),
        (None, Some(room)) => {
            let budget = room.min(DISK_STORAGE_CEILING) / DISK_STEP * DISK_STEP;
            if budget < DISK_STORAGE_FLOOR {
                return Err(format!(
                    "the Docker host has {} GiB of free disk, under the {} GiB a disk-backed \
                     engine needs with a {} GiB margin",
                    host.available_disk.unwrap_or(0) / GIB,
                    (DISK_STORAGE_FLOOR + DISK_MARGIN) / GIB,
                    DISK_MARGIN / GIB
                ));
            }
            Ok(budget)
        }
    }
}

/// The limits for one engine on `host` under `config`. Pure: the same host
/// and config always give the same limits. An override that the engine
/// layer would refuse is an error here, before any intent is written.
pub fn size_engine(host: HostResources, config: EngineConfig) -> Result<ActEngineLimits, String> {
    let gib = |value: u64, what: &str| {
        value
            .checked_mul(GIB)
            .ok_or_else(|| format!("[engine] {what} is out of range"))
    };
    let pinned_storage = config
        .storage_gib
        .map(|value| gib(value, "storage_gib"))
        .transpose()?;
    // Disk unless memory is asked for; unset falls back to memory when the
    // host has no room on disk, while an explicit "disk" says why not.
    let disk = match config.storage {
        Some(StorageBacking::Memory) => None,
        Some(StorageBacking::Disk) => Some(disk_budget(host, pinned_storage)?),
        None => disk_budget(host, pinned_storage).ok(),
    };
    let sized_memory = ((host.total_memory / 2).min(host.available_memory / 4 * 3) / MEMORY_STEP
        * MEMORY_STEP)
        .clamp(MEMORY_FLOOR, MEMORY_CEILING);
    let memory_bytes = match (config.memory_gib, pinned_storage, disk) {
        (Some(value), _, _) => gib(value, "memory_gib")?,
        // Pinned memory-backed storage needs the memory it is a share of.
        (None, Some(storage), None) => sized_memory.max(storage.saturating_add(storage / 3)),
        (None, _, _) => sized_memory,
    };
    let (storage, storage_bytes) = match disk {
        Some(budget) => (EngineStorage::Disk, budget),
        None => (
            EngineStorage::Memory,
            pinned_storage.unwrap_or_else(|| {
                (memory_bytes / 4 * 3)
                    .min(memory_bytes.saturating_sub(MEMORY_HEADROOM))
                    .max(STORAGE_FLOOR)
            }),
        ),
    };
    let cpus = config.cpus.unwrap_or(host.cpus.clamp(1, CPU_CEILING));
    let limits = ActEngineLimits {
        memory_bytes,
        storage_bytes,
        storage,
        nano_cpus: cpus.saturating_mul(1_000_000_000),
        pids: config.pids.unwrap_or(DEFAULT_PIDS),
    };
    limits
        .validate()
        .map_err(|error| format!("[engine] limits {limits:?}: {error}"))?;
    Ok(limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host with plenty of free disk.
    fn host(total_gib: u64, available_gib: u64, cpus: u64) -> HostResources {
        HostResources {
            total_memory: total_gib * GIB,
            available_memory: available_gib * GIB,
            cpus,
            available_disk: Some(1024 * GIB),
        }
    }

    fn with_disk(host: HostResources, free_gib: Option<u64>) -> HostResources {
        HostResources {
            available_disk: free_gib.map(|free| free * GIB),
            ..host
        }
    }

    fn backed(storage: StorageBacking) -> EngineConfig {
        EngineConfig {
            storage: Some(storage),
            ..EngineConfig::default()
        }
    }

    #[test]
    fn a_large_idle_host_gets_the_memory_ceiling_and_disk_storage() {
        let limits = size_engine(host(128, 120, 32), EngineConfig::default()).unwrap();
        assert_eq!(limits.memory_bytes, 48 * GIB);
        assert_eq!(limits.storage, EngineStorage::Disk);
        assert_eq!(limits.storage_bytes, DISK_STORAGE_CEILING);
        assert_eq!(limits.nano_cpus, 8_000_000_000);
        assert_eq!(limits.pids, DEFAULT_PIDS);
    }

    #[test]
    fn clud_minimal_pr_run_fits_on_a_96_gib_host_with_no_override() {
        // zackees/clud's minimal PR run (#425, run 53ad0357) peaked at
        // 32.1 GiB with three Rust jobs at once, and soldr refuses to build
        // under 5 GiB free.
        let needed = 33 * GIB + 5 * GIB;
        // Plenty of disk, the 89 GiB free of the machine that hit #425, and
        // a Docker Desktop VM whose disk cannot be read.
        for free in [Some(1024), Some(89), None] {
            let limits =
                size_engine(with_disk(host(96, 90, 16), free), EngineConfig::default()).unwrap();
            assert!(limits.storage_bytes >= needed, "{free:?}: {limits:?}");
            // On disk, so the host keeps its RAM: memory is still half of it.
            assert_eq!(limits.storage, EngineStorage::Disk);
            assert_eq!(limits.memory_bytes, 48 * GIB);
        }
    }

    #[test]
    fn the_disk_budget_follows_free_disk_less_a_margin_in_whole_steps() {
        let budget = |free: Option<u64>| {
            let limits =
                size_engine(with_disk(host(64, 60, 8), free), EngineConfig::default()).unwrap();
            (limits.storage, limits.storage_bytes / GIB)
        };
        assert_eq!(budget(Some(1024)), (EngineStorage::Disk, 128));
        assert_eq!(budget(Some(89)), (EngineStorage::Disk, 72));
        assert_eq!(budget(Some(84)), (EngineStorage::Disk, 64));
        assert_eq!(budget(Some(40)), (EngineStorage::Disk, 24));
        assert_eq!(budget(None), (EngineStorage::Disk, 64));
        // Too little disk: storage falls back to a RAM tmpfs, sized as before.
        assert_eq!(budget(Some(39)), (EngineStorage::Memory, 24));
        // Asking for disk says why it cannot be had instead.
        let refused = size_engine(
            with_disk(host(64, 60, 8), Some(39)),
            backed(StorageBacking::Disk),
        )
        .unwrap_err();
        assert!(refused.contains("39 GiB of free disk"), "{refused}");
    }

    #[test]
    fn memory_follows_the_host_total_and_what_is_available() {
        let memory = backed(StorageBacking::Memory);
        // Half of a 32 GiB host, with three quarters of it as a tmpfs.
        let limits = size_engine(host(32, 30, 4), memory).unwrap();
        assert_eq!(limits.memory_bytes, 16 * GIB);
        assert_eq!(limits.storage, EngineStorage::Memory);
        assert_eq!(limits.storage_bytes, 12 * GIB);
        assert_eq!(limits.nano_cpus, 4_000_000_000);
        // A busy host: three quarters of the 8 GiB still available, in whole steps.
        let busy = size_engine(host(32, 8, 4), memory).unwrap();
        assert_eq!(busy.memory_bytes, 4 * GIB);
        assert!(busy.memory_bytes - busy.storage_bytes >= MEMORY_HEADROOM);
        // A large host's tmpfs is the old 36 GiB.
        let large = size_engine(host(128, 120, 32), memory).unwrap();
        assert_eq!(large.storage_bytes, 36 * GIB);
    }

    /// #553: a run claims the prepared spare only when its limits match exactly, so samples of
    /// available memory a few GiB apart must size the same engine.
    #[test]
    fn small_changes_in_available_memory_size_the_same_engine() {
        let config = EngineConfig::default();
        let sized = |available_mib: u64| {
            let mut sample = host(64, 0, 8);
            sample.available_memory = available_mib << 20;
            size_engine(sample, config).unwrap()
        };
        assert_eq!(
            sized(30_000),
            sized(30_700),
            "a 700 MiB swing keeps the spare"
        );
        assert_eq!(sized(24 * 1024 + 100).memory_bytes % MEMORY_STEP, 0);
    }

    #[test]
    fn clud_build_fits_with_soldr_margin_on_a_64_gib_host_and_up() {
        // clud's build-linux-x64 keeps about 16 GiB on the engine's storage
        // and soldr refuses to build with under 5 GiB free (#392).
        let needed = 16 * GIB + 5 * GIB;
        for total in [64, 96, 128, 256] {
            for config in [EngineConfig::default(), backed(StorageBacking::Memory)] {
                let limits = size_engine(host(total, total, 16), config).unwrap();
                assert!(
                    limits.storage_bytes >= needed,
                    "{total} GiB host: {limits:?}"
                );
            }
        }
    }

    #[test]
    fn pinned_storage_is_a_disk_budget_or_a_tmpfs_that_grows_memory() {
        let pinned = |storage| EngineConfig {
            storage_gib: Some(32),
            storage,
            ..EngineConfig::default()
        };
        // On disk the pin is the budget, and memory is left as sized.
        let disk = size_engine(host(32, 30, 4), pinned(None)).unwrap();
        assert_eq!(
            (disk.storage, disk.storage_bytes, disk.memory_bytes),
            (EngineStorage::Disk, 32 * GIB, 16 * GIB)
        );
        // A pin the free disk cannot hold falls back to memory, or is
        // refused when disk was asked for.
        let tight = with_disk(host(32, 30, 4), Some(40));
        assert_eq!(
            size_engine(tight, pinned(None)).unwrap().storage,
            EngineStorage::Memory
        );
        assert!(size_engine(tight, pinned(Some(StorageBacking::Disk))).is_err());
        // In memory, a 32 GiB host sizes 16 GiB, too little for the pin.
        let memory = pinned(Some(StorageBacking::Memory));
        let limits = size_engine(host(32, 30, 4), memory).unwrap();
        assert_eq!(limits.storage_bytes, 32 * GIB);
        assert_eq!(limits.memory_bytes, 32 * GIB + 32 * GIB / 3);
        // Memory already large enough is left as sized.
        let roomy = size_engine(host(128, 120, 8), memory).unwrap();
        assert_eq!(roomy.memory_bytes, 48 * GIB);
    }

    #[test]
    fn a_small_host_gets_the_floor_with_headroom() {
        let small = with_disk(host(4, 1, 1), Some(20));
        let limits = size_engine(small, EngineConfig::default()).unwrap();
        assert_eq!(limits.memory_bytes, MEMORY_FLOOR);
        assert_eq!(limits.storage, EngineStorage::Memory);
        assert_eq!(limits.storage_bytes, MEMORY_FLOOR - MEMORY_HEADROOM);
        assert_eq!(limits.nano_cpus, 1_000_000_000);
    }

    #[test]
    fn overrides_are_taken_as_written_and_bounded_by_validation() {
        let pinned = EngineConfig {
            memory_gib: Some(12),
            storage_gib: Some(6),
            storage: Some(StorageBacking::Memory),
            cpus: Some(3),
            pids: Some(2048),
            ..EngineConfig::default()
        };
        let limits = size_engine(host(4, 1, 1), pinned).unwrap();
        assert_eq!(
            limits,
            ActEngineLimits {
                memory_bytes: 12 * GIB,
                storage_bytes: 6 * GIB,
                storage: EngineStorage::Memory,
                nano_cpus: 3_000_000_000,
                pids: 2048,
            }
        );
        // A tmpfs that leaves no memory for the engine is refused up front.
        let starved = EngineConfig {
            memory_gib: Some(8),
            storage_gib: Some(8),
            storage: Some(StorageBacking::Memory),
            ..EngineConfig::default()
        };
        assert!(size_engine(host(64, 64, 8), starved).is_err());
        // The same numbers on disk leave all 8 GiB for the jobs.
        let on_disk = EngineConfig {
            storage: None,
            ..starved
        };
        assert_eq!(
            size_engine(host(64, 64, 8), on_disk).unwrap().storage,
            EngineStorage::Disk
        );
        let huge = EngineConfig {
            memory_gib: Some(u64::MAX),
            ..EngineConfig::default()
        };
        assert!(size_engine(host(64, 64, 8), huge).is_err());
    }

    #[test]
    fn sized_limits_round_trip_through_the_frozen_profile_exactly() {
        use bosn_registry::act::ActEngineIntent;
        for config in [EngineConfig::default(), backed(StorageBacking::Memory)] {
            let sized = size_engine(host(24, 20, 6), config).unwrap();
            let profile = crate::act_engine::creation_profile_with_cache(sized, None).unwrap();
            assert_eq!(
                (
                    profile.memory_bytes,
                    profile.storage_bytes,
                    profile.tmpfs_policy,
                    profile.nano_cpus,
                    profile.pids
                ),
                (
                    sized.memory_bytes,
                    sized.storage_bytes,
                    sized.storage.policy(),
                    sized.nano_cpus,
                    sized.pids
                )
            );
            let intent = ActEngineIntent {
                run_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
                workspace: "/private/source".into(),
                candidate_sha: "a".repeat(40),
                payload_sha256: "b".repeat(64),
                snapshot_sha256: "c".repeat(64),
                act_version: "0.2.88".into(),
                act_image_digest: format!("sha256:{}", "d".repeat(64)),
                engine_image_digest: crate::act_engine::ENGINE_MANIFEST.into(),
                runner_image_digest: format!("sha256:{}", "f".repeat(64)),
                created_at: 1.0,
                spare: false,
                creation_profile: Some(profile),
            };
            // What the registry stores is what recovery reads back.
            let stored: ActEngineIntent =
                serde_json::from_slice(&serde_json::to_vec(&intent).unwrap()).unwrap();
            assert_eq!(stored, intent);
            assert_eq!(crate::act_engine::frozen_limits(&stored).unwrap(), sized);
            let owner = "11111111-2222-4333-8444-555555555555";
            assert!(crate::act_engine::create_arguments(&stored, owner, sized).is_ok());
            // The same run sized on another host is a different engine.
            let elsewhere = size_engine(host(64, 64, 6), config).unwrap();
            assert_ne!(elsewhere, sized);
            assert!(crate::act_engine::create_arguments(&stored, owner, elsewhere).is_err());
        }
    }

    #[test]
    fn host_resources_come_from_docker_info_and_matching_meminfo() {
        let total = 16 * GIB;
        let info = format!("{total} 12 /var/lib/docker");
        let meminfo = format!(
            "MemTotal:       {} kB\nMemFree:  1 kB\nMemAvailable:   {} kB\n",
            total / 1024,
            4 * GIB / 1024
        );
        let disk = |root: &str| (root == "/var/lib/docker").then_some(90 * GIB);
        assert_eq!(
            HostResources::parse(&info, Some(&meminfo), disk).unwrap(),
            HostResources {
                total_memory: total,
                available_memory: 4 * GIB,
                cpus: 12,
                available_disk: Some(90 * GIB),
            }
        );
        // Another machine's meminfo (a Docker Desktop VM, a remote engine)
        // says nothing about the engine's host, its memory or its disk.
        let other = meminfo.replace(&(total / 1024).to_string(), "1024");
        let elsewhere = HostResources::parse(&info, Some(&other), disk).unwrap();
        assert_eq!(
            (elsewhere.available_memory, elsewhere.available_disk),
            (total, None)
        );
        let unknown = HostResources::parse(&format!("{total} 12"), None, disk).unwrap();
        assert_eq!(
            (unknown.available_memory, unknown.available_disk),
            (total, None)
        );
        for bad in ["", "16", "x 4", "16 y", "0 4", "16 0", "16 4 relative"] {
            assert!(
                HostResources::parse(bad, None, |_| None).is_err(),
                "{bad:?}"
            );
        }
    }
}
