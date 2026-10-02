//! How big one `bosn ci` engine may grow, sized from the engine's host.
//!
//! The engine's private storage is a tmpfs, so it is RAM and counts against
//! the engine's memory limit. Neither reserves anything until it is written:
//! both only bound a runaway job, so a large host gets large bounds. Sizing
//! starts from memory: half the host's total, but no more than three
//! quarters of what is available right now, held between [`MEMORY_FLOOR`]
//! and [`MEMORY_CEILING`]. Storage takes three quarters of that (36 GiB of a
//! 48 GiB engine; a build's `target/` plus a guard's free-space margin, #392),
//! always leaving [`MEMORY_HEADROOM`] for the nested daemon and the jobs.
//!
//! Every value can be pinned in `<state>/config.toml`:
//!
//! ```toml
//! [engine]
//! memory_gib = 16   # the engine's whole memory limit
//! storage_gib = 10  # its private storage tmpfs (counts against memory)
//! cpus = 4
//! pids = 4096
//! spares = 0        # opt out of the prepared spare engine (#410; default 1)
//! ```
//!
//! Pinning only `storage_gib` grows a sized memory limit to fit it (by the
//! same three quarters). The chosen limits are frozen into each run's
//! creation profile before its intent commits, and creation and every
//! inspection verify the engine against that profile exactly; changing the
//! host or the config never changes an existing record.

use serde::Deserialize;

use crate::act_engine::ActEngineLimits;

const GIB: u64 = 1 << 30;
/// The least memory an engine is sized to, however small the host.
pub const MEMORY_FLOOR: u64 = 4 * GIB;
/// The most memory an engine is sized to, however large the host.
pub const MEMORY_CEILING: u64 = 48 * GIB;
/// Memory always left outside the storage tmpfs.
pub const MEMORY_HEADROOM: u64 = 2 * GIB;
/// The least private storage an engine is sized to.
pub const STORAGE_FLOOR: u64 = GIB;
pub const CPU_CEILING: u64 = 8;
pub const DEFAULT_PIDS: u64 = 4096;

/// What the engine's host offers, as the host engine reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostResources {
    pub total_memory: u64,
    /// Memory available now; the total when the host cannot say.
    pub available_memory: u64,
    pub cpus: u64,
}

impl HostResources {
    /// From `docker info --format '{{.MemTotal}} {{.NCPU}}'` (the engine's
    /// host, which under Docker Desktop is its VM) and, when it describes
    /// the same machine, this host's `/proc/meminfo`.
    pub fn parse(info: &str, meminfo: Option<&str>) -> Result<Self, String> {
        let mut fields = info.split_whitespace();
        let (Some(total), Some(cpus), None) = (fields.next(), fields.next(), fields.next()) else {
            return Err(format!("unexpected docker info: {info:?}"));
        };
        let total_memory: u64 = total
            .parse()
            .map_err(|_| format!("docker info MemTotal is not a byte count: {total:?}"))?;
        let cpus: u64 = cpus
            .parse()
            .map_err(|_| format!("docker info NCPU is not a count: {cpus:?}"))?;
        if total_memory == 0 || cpus == 0 {
            return Err(format!("docker info reports an empty host: {info:?}"));
        }
        let available_memory = meminfo
            .and_then(Meminfo::parse)
            .filter(|local| local.total == total_memory)
            .map_or(total_memory, |local| local.available.min(total_memory));
        Ok(Self {
            total_memory,
            available_memory,
            cpus,
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

/// `[engine]` in `<state>/config.toml`: explicit overrides of the sized
/// limits. An override is taken as written (still bounded by the engine
/// layer's validation); anything left out is sized from the host.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub memory_gib: Option<u64>,
    pub storage_gib: Option<u64>,
    pub cpus: Option<u64>,
    pub pids: Option<u64>,
    /// Prepared spare engines kept (#410); `spares = 0` opts out.
    #[serde(default)]
    pub spares: super::spare::Spares,
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
    let memory_bytes = match (config.memory_gib, pinned_storage) {
        (Some(value), _) => gib(value, "memory_gib")?,
        (None, pinned) => {
            let sized = (host.total_memory / 2)
                .min(host.available_memory / 4 * 3)
                .clamp(MEMORY_FLOOR, MEMORY_CEILING);
            // Pinned storage needs the memory it is a share of.
            pinned.map_or(sized, |storage| {
                sized.max(storage.saturating_add(storage / 3))
            })
        }
    };
    let storage_bytes = pinned_storage.unwrap_or_else(|| {
        (memory_bytes / 4 * 3)
            .min(memory_bytes.saturating_sub(MEMORY_HEADROOM))
            .max(STORAGE_FLOOR)
    });
    let cpus = config.cpus.unwrap_or(host.cpus.clamp(1, CPU_CEILING));
    let limits = ActEngineLimits {
        memory_bytes,
        storage_bytes,
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

    fn host(total_gib: u64, available_gib: u64, cpus: u64) -> HostResources {
        HostResources {
            total_memory: total_gib * GIB,
            available_memory: available_gib * GIB,
            cpus,
        }
    }

    #[test]
    fn a_large_idle_host_gets_the_ceiling() {
        let limits = size_engine(host(128, 120, 32), EngineConfig::default()).unwrap();
        assert_eq!(limits.memory_bytes, 48 * GIB);
        assert_eq!(limits.storage_bytes, 36 * GIB);
        assert_eq!(limits.nano_cpus, 8_000_000_000);
        assert_eq!(limits.pids, DEFAULT_PIDS);
    }

    #[test]
    fn memory_follows_the_host_total_and_what_is_available() {
        // Half of a 32 GiB host.
        let limits = size_engine(host(32, 30, 4), EngineConfig::default()).unwrap();
        assert_eq!(limits.memory_bytes, 16 * GIB);
        assert_eq!(limits.storage_bytes, 12 * GIB);
        assert_eq!(limits.nano_cpus, 4_000_000_000);
        // A busy host: three quarters of the 8 GiB still available.
        let busy = size_engine(host(32, 8, 4), EngineConfig::default()).unwrap();
        assert_eq!(busy.memory_bytes, 6 * GIB);
        assert!(busy.memory_bytes - busy.storage_bytes >= MEMORY_HEADROOM);
    }

    #[test]
    fn clud_build_fits_with_soldr_margin_on_a_64_gib_host_and_up() {
        // clud's build-linux-x64 keeps about 16 GiB on the engine's storage
        // and soldr refuses to build with under 5 GiB free (#392).
        let needed = 16 * GIB + 5 * GIB;
        for total in [64, 96, 128, 256] {
            let limits = size_engine(host(total, total, 16), EngineConfig::default()).unwrap();
            assert!(
                limits.storage_bytes >= needed,
                "{total} GiB host: {limits:?}"
            );
        }
    }

    #[test]
    fn pinning_only_storage_grows_the_sized_memory_to_fit_it() {
        let pinned = EngineConfig {
            storage_gib: Some(32),
            ..EngineConfig::default()
        };
        // A 32 GiB host sizes 16 GiB of memory, too little for the pin.
        let limits = size_engine(host(32, 30, 4), pinned).unwrap();
        assert_eq!(limits.storage_bytes, 32 * GIB);
        assert_eq!(limits.memory_bytes, 32 * GIB + 32 * GIB / 3);
        // Memory already large enough is left as sized.
        let roomy = size_engine(host(128, 120, 8), pinned).unwrap();
        assert_eq!(roomy.memory_bytes, 48 * GIB);
    }

    #[test]
    fn a_small_host_gets_the_floor_with_headroom() {
        let limits = size_engine(host(4, 1, 1), EngineConfig::default()).unwrap();
        assert_eq!(limits.memory_bytes, MEMORY_FLOOR);
        assert_eq!(limits.storage_bytes, MEMORY_FLOOR - MEMORY_HEADROOM);
        assert_eq!(limits.nano_cpus, 1_000_000_000);
    }

    #[test]
    fn overrides_are_taken_as_written_and_bounded_by_validation() {
        let pinned = EngineConfig {
            memory_gib: Some(12),
            storage_gib: Some(6),
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
                nano_cpus: 3_000_000_000,
                pids: 2048,
            }
        );
        // Storage that leaves no memory for the engine is refused up front.
        let starved = EngineConfig {
            memory_gib: Some(8),
            storage_gib: Some(8),
            ..EngineConfig::default()
        };
        assert!(size_engine(host(64, 64, 8), starved).is_err());
        let huge = EngineConfig {
            memory_gib: Some(u64::MAX),
            ..EngineConfig::default()
        };
        assert!(size_engine(host(64, 64, 8), huge).is_err());
    }

    #[test]
    fn sized_limits_round_trip_through_the_frozen_profile_exactly() {
        use bosn_registry::act::ActEngineIntent;
        let sized = size_engine(host(24, 20, 6), EngineConfig::default()).unwrap();
        let profile = crate::act_engine::creation_profile_with_cache(sized, None).unwrap();
        assert_eq!(
            (
                profile.memory_bytes,
                profile.storage_bytes,
                profile.nano_cpus,
                profile.pids
            ),
            (
                sized.memory_bytes,
                sized.storage_bytes,
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
        let elsewhere = size_engine(host(64, 64, 6), EngineConfig::default()).unwrap();
        assert_ne!(elsewhere, sized);
        assert!(crate::act_engine::create_arguments(&stored, owner, elsewhere).is_err());
    }

    #[test]
    fn host_resources_come_from_docker_info_and_matching_meminfo() {
        let total = 16 * GIB;
        let info = format!("{total} 12");
        let meminfo = format!(
            "MemTotal:       {} kB\nMemFree:  1 kB\nMemAvailable:   {} kB\n",
            total / 1024,
            4 * GIB / 1024
        );
        assert_eq!(
            HostResources::parse(&info, Some(&meminfo)).unwrap(),
            HostResources {
                total_memory: total,
                available_memory: 4 * GIB,
                cpus: 12,
            }
        );
        // Another machine's meminfo (a Docker Desktop VM, a remote engine)
        // says nothing about the engine's host.
        let other = meminfo.replace(&(total / 1024).to_string(), "1024");
        assert_eq!(
            HostResources::parse(&info, Some(&other))
                .unwrap()
                .available_memory,
            total
        );
        assert_eq!(
            HostResources::parse(&info, None).unwrap().available_memory,
            total
        );
        for bad in ["", "16", "x 4", "16 y", "0 4", "16 0", "16 4 2"] {
            assert!(HostResources::parse(bad, None).is_err(), "{bad:?}");
        }
    }
}
