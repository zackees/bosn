//! Runner capacity (#358): how many jobs the daemon runs at once and what
//! each one may use.
//!
//! * **Runner slots** run task jobs (`bosn run --task`, manifest/setup app
//!   tasks). The default is `max(1, ncpu * 2)`, where `ncpu` is
//!   [`std::thread::available_parallelism`], which already honours cgroup
//!   CPU quotas and affinity masks; when it cannot be read, `ncpu` is 1.
//! * **CPUs per slot** (default 4) is a CFS quota (`docker --cpus`) applied to
//!   the task's setup container and to every container the task starts
//!   through Bosn's Docker proxy (for example act's job containers).
//!
//! The two are independent on purpose. `slots * cpus` may exceed the host's
//! CPU count, and by default it does (16 logical CPUs give 32 slots of 4 CPUs,
//! 128 CPUs of quota). That oversubscription is permitted: a quota is a
//! ceiling, not a reservation, so idle slots cost nothing and a busy machine
//! shares its CPUs fairly between running jobs instead of queueing them. The
//! per-slot quota is what stops one job from taking every core.
//! * **Control slots** run short daemon operations (ensure, prepare,
//!   converge) in their own lane, so a stack ensure never waits behind
//!   long-running tasks. The default is `max(2, ncpu / 2)`, #12's build cap.
//! * **Stall timeout**: a running task with no output and no Docker activity
//!   for this long is torn down (default 30 minutes, 0 disables).
//!
//! Precedence, highest first: `bosn daemon serve` flags, `BOSN_RUNNER_*`
//! environment variables, `<state-dir>/runners.toml`, then the defaults.

use std::{path::Path, time::Duration};

pub const DEFAULT_CPUS_PER_SLOT: f64 = 4.0;
pub const DEFAULT_STALL_SECONDS: u64 = 30 * 60;
/// Upper bounds keep a typo from asking Docker for something absurd.
pub const MAX_SLOTS: usize = 1024;
pub const MAX_CPUS_PER_SLOT: f64 = 1024.0;
pub const CONFIG_FILE: &str = "runners.toml";

/// The machine's logical CPU count as the scheduler sees it, or `None` when
/// it cannot be determined.
pub fn host_cpus() -> Option<usize> {
    std::thread::available_parallelism().ok().map(usize::from)
}

/// `max(1, ncpu * 2)`; an unknown CPU count counts as one CPU.
pub fn default_runner_slots(ncpu: Option<usize>) -> usize {
    ncpu.unwrap_or(1).saturating_mul(2).clamp(1, MAX_SLOTS)
}

/// `max(2, ncpu / 2)`, #12's machine-wide build cap.
pub fn default_control_slots(ncpu: Option<usize>) -> usize {
    (ncpu.unwrap_or(1) / 2).clamp(2, MAX_SLOTS)
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunnerCapacity {
    pub runner_slots: usize,
    pub control_slots: usize,
    /// CPU quota per runner slot; `0.0` means no limit.
    pub cpus_per_slot: f64,
    /// Memory limit per runner slot in bytes; `None` means no limit.
    pub memory_per_slot: Option<u64>,
    /// `None` disables stall teardown.
    pub stall_after: Option<Duration>,
    /// Route Docker API calls of tasks that mount the host socket through
    /// Bosn's accounting proxy (Linux only).
    pub docker_proxy: bool,
}

impl RunnerCapacity {
    pub fn defaults(ncpu: Option<usize>) -> Self {
        Self {
            runner_slots: default_runner_slots(ncpu),
            control_slots: default_control_slots(ncpu),
            cpus_per_slot: DEFAULT_CPUS_PER_SLOT,
            memory_per_slot: None,
            stall_after: Some(Duration::from_secs(DEFAULT_STALL_SECONDS)),
            docker_proxy: cfg!(target_os = "linux"),
        }
    }

    /// Defaults, then `<state>/runners.toml`, then the environment.
    pub fn load(state_dir: &Path) -> Result<Self, String> {
        let mut capacity = Self::defaults(host_cpus());
        match std::fs::read_to_string(state_dir.join(CONFIG_FILE)) {
            Ok(text) => capacity.apply_file(&text)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{CONFIG_FILE}: {error}")),
        }
        capacity.apply_env(|name| std::env::var(name).ok())?;
        Ok(capacity)
    }

    /// `runners.toml` is a flat list of `key = value` lines (TOML's scalar
    /// subset: bare numbers and booleans, or quoted strings; `#` comments).
    pub fn apply_file(&mut self, text: &str) -> Result<(), String> {
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split_once('#').map_or(raw, |(code, _)| code).trim();
            if line.is_empty() {
                continue;
            }
            let at = || format!("{CONFIG_FILE} line {}", index + 1);
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("{}: expected `key = value`", at()))?;
            let value = value.trim();
            let value = match value.strip_prefix('"') {
                Some(rest) => rest
                    .strip_suffix('"')
                    .ok_or_else(|| format!("{}: unterminated string", at()))?,
                None if value.starts_with(['[', '{', '\'']) => {
                    return Err(format!("{}: `{}` must be a scalar", at(), key.trim()));
                }
                None => value,
            };
            self.set(key.trim(), value)
                .map_err(|error| format!("{}: {error}", at()))?;
        }
        Ok(())
    }

    pub fn apply_env(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        for (variable, key) in [
            ("BOSN_RUNNER_SLOTS", "slots"),
            ("BOSN_CONTROL_SLOTS", "control_slots"),
            ("BOSN_RUNNER_CPUS", "cpus"),
            ("BOSN_RUNNER_MEMORY", "memory"),
            ("BOSN_STALL_SECONDS", "stall_seconds"),
            ("BOSN_DOCKER_PROXY", "docker_proxy"),
        ] {
            if let Some(value) = get(variable).filter(|v| !v.trim().is_empty()) {
                self.set(key, &value)
                    .map_err(|error| format!("{variable}: {error}"))?;
            }
        }
        Ok(())
    }

    /// Set one setting by its `runners.toml` key (also the flag name without
    /// `--runner-`/`--`).
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let value = value.trim();
        match key {
            "slots" => self.runner_slots = parse_count(key, value)?,
            "control_slots" => self.control_slots = parse_count(key, value)?,
            "cpus" => {
                let cpus: f64 = value
                    .parse()
                    .map_err(|_| format!("`{key}` must be a number of CPUs, not {value:?}"))?;
                if !cpus.is_finite() || !(0.0..=MAX_CPUS_PER_SLOT).contains(&cpus) {
                    return Err(format!(
                        "`{key}` must be within 0..={MAX_CPUS_PER_SLOT} (0 = no limit)"
                    ));
                }
                self.cpus_per_slot = cpus;
            }
            "memory" => {
                self.memory_per_slot = match parse_bytes(value)? {
                    0 => None,
                    bytes => Some(bytes),
                }
            }
            "stall_seconds" => {
                let seconds: u64 = value
                    .parse()
                    .map_err(|_| format!("`{key}` must be whole seconds, not {value:?}"))?;
                self.stall_after = (seconds > 0).then(|| Duration::from_secs(seconds));
            }
            "docker_proxy" => {
                self.docker_proxy = match value {
                    "1" | "true" | "on" | "yes" => true,
                    "0" | "false" | "off" | "no" => false,
                    _ => return Err(format!("`{key}` must be true or false")),
                } && cfg!(target_os = "linux");
            }
            _ => return Err(format!("unknown setting `{key}`")),
        }
        Ok(())
    }

    /// Docker's `NanoCpus` for one slot; `0` means unlimited.
    pub fn nano_cpus(&self) -> i64 {
        (self.cpus_per_slot * 1e9).round() as i64
    }

    pub fn describe(&self) -> String {
        let cpus = if self.cpus_per_slot > 0.0 {
            format!("{} CPUs", trim_float(self.cpus_per_slot))
        } else {
            "no CPU limit".into()
        };
        let memory = self
            .memory_per_slot
            .map_or_else(|| "no memory limit".into(), |m| format!("{m} bytes"));
        let stall = self
            .stall_after
            .map_or_else(|| "off".into(), |d| format!("{}s", d.as_secs()));
        format!(
            "{} runner slots x {cpus}, {memory}; {} control slots; stall teardown {stall}; docker proxy {}",
            self.runner_slots,
            self.control_slots,
            if self.docker_proxy { "on" } else { "off" }
        )
    }
}

fn parse_count(key: &str, value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(n) if (1..=MAX_SLOTS).contains(&n) => Ok(n),
        _ => Err(format!("`{key}` must be a whole number in 1..={MAX_SLOTS}")),
    }
}

/// `8g`, `512m`, `1024k` or plain bytes; binary multiples, like Docker.
pub fn parse_bytes(value: &str) -> Result<u64, String> {
    let lower = value.to_ascii_lowercase();
    let lower = lower.trim_end_matches('b');
    let (digits, multiplier) = match lower.chars().last() {
        Some('k') => (&lower[..lower.len() - 1], 1u64 << 10),
        Some('m') => (&lower[..lower.len() - 1], 1u64 << 20),
        Some('g') => (&lower[..lower.len() - 1], 1u64 << 30),
        Some('t') => (&lower[..lower.len() - 1], 1u64 << 40),
        _ => (lower, 1),
    };
    digits
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .ok_or_else(|| format!("`memory` must be a size like 8g or 512m, not {value:?}"))
}

fn trim_float(value: f64) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_slots_are_twice_the_cpu_count_and_never_zero() {
        assert_eq!(default_runner_slots(Some(16)), 32);
        assert_eq!(default_runner_slots(Some(1)), 2);
        assert_eq!(default_runner_slots(Some(0)), 1, "a zero count still runs");
        assert_eq!(
            default_runner_slots(None),
            2,
            "unknown ncpu falls back to 1"
        );
        assert_eq!(default_runner_slots(Some(usize::MAX)), MAX_SLOTS);
        assert_eq!(default_control_slots(Some(16)), 8);
        assert_eq!(default_control_slots(Some(1)), 2);
        assert_eq!(default_control_slots(None), 2);
        // This host's real value goes through the same formula.
        let ncpu = host_cpus();
        assert_eq!(
            RunnerCapacity::defaults(ncpu).runner_slots,
            default_runner_slots(ncpu)
        );
    }

    #[test]
    fn defaults_oversubscribe_on_purpose() {
        let capacity = RunnerCapacity::defaults(Some(16));
        assert_eq!(capacity.runner_slots, 32);
        assert_eq!(capacity.cpus_per_slot, 4.0);
        assert_eq!(capacity.nano_cpus(), 4_000_000_000);
        assert!(capacity.runner_slots as f64 * capacity.cpus_per_slot > 16.0);
    }

    #[test]
    fn file_then_environment_override_the_defaults() {
        let mut capacity = RunnerCapacity::defaults(Some(16));
        capacity
            .apply_file("slots = 4\ncpus = 2.5\nmemory = \"8g\"\nstall_seconds = 0\n")
            .unwrap();
        assert_eq!(capacity.runner_slots, 4);
        assert_eq!(capacity.cpus_per_slot, 2.5);
        assert_eq!(capacity.memory_per_slot, Some(8 << 30));
        assert_eq!(capacity.stall_after, None);
        capacity
            .apply_env(|name| match name {
                "BOSN_RUNNER_SLOTS" => Some("6".into()),
                "BOSN_RUNNER_CPUS" => Some("0".into()),
                "BOSN_STALL_SECONDS" => Some("90".into()),
                _ => None,
            })
            .unwrap();
        assert_eq!(capacity.runner_slots, 6);
        assert_eq!(capacity.nano_cpus(), 0, "0 CPUs means no limit");
        assert_eq!(capacity.stall_after, Some(Duration::from_secs(90)));
    }

    #[test]
    fn nonsense_settings_are_refused_with_their_name() {
        let mut capacity = RunnerCapacity::defaults(Some(4));
        for (key, value) in [
            ("slots", "0"),
            ("slots", "-1"),
            ("slots", "lots"),
            ("cpus", "NaN"),
            ("cpus", "-2"),
            ("memory", "eight"),
            ("stall_seconds", "-5"),
            ("docker_proxy", "maybe"),
            ("bogus", "1"),
        ] {
            let error = capacity.set(key, value).unwrap_err();
            assert!(error.contains(key), "{key}={value}: {error}");
        }
        assert!(capacity.apply_file("slots = [1]").is_err());
        assert!(capacity.apply_file("not toml ===").is_err());
        assert_eq!(capacity, RunnerCapacity::defaults(Some(4)));
    }

    #[test]
    fn byte_sizes_use_binary_multiples() {
        assert_eq!(parse_bytes("512m").unwrap(), 512 << 20);
        assert_eq!(parse_bytes("8G").unwrap(), 8 << 30);
        assert_eq!(parse_bytes("8gb").unwrap(), 8 << 30);
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
        assert!(parse_bytes("99999999999t").is_err());
    }
}
