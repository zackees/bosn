//! How full an engine's private storage got during a run, and what a run
//! that failed with it nearly full says about it (#392).
//!
//! The storage is the engine's `/var/lib/docker` tmpfs. While act runs, the
//! lifecycle samples it every [`SAMPLE_INTERVAL`] and keeps the peak, so a
//! step that a tool refused for lack of space (soldr will not build with
//! under 5 GiB free) or that hit ENOSPC is explained in the run's log and
//! report instead of failing silently.

use std::time::Duration;

const GIB: u64 = 1 << 30;
/// Free space below which a mostly used engine counts as low: what common
/// build guards (soldr's block threshold) require.
pub const LOW_FREE: u64 = 5 * GIB;
/// How often the storage is sampled while act runs.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);
/// Where the storage is mounted in the engine.
pub const STORAGE_PATH: &str = "/var/lib/docker";

/// One sample of the storage, in bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageUsage {
    pub size: u64,
    pub used: u64,
    pub available: u64,
}

impl StorageUsage {
    /// `df -Pk PATH`: the POSIX header, then one row of 1024-byte blocks
    /// (`Filesystem 1024-blocks Used Available Capacity Mounted-on`).
    pub fn parse_df(text: &str) -> Result<Self, String> {
        let unexpected = || format!("unexpected df output: {text:?}");
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let (Some(header), Some(row), None) = (lines.next(), lines.next(), lines.next()) else {
            return Err(unexpected());
        };
        if !header.starts_with("Filesystem") {
            return Err(unexpected());
        }
        let fields: Vec<&str> = row.split_whitespace().collect();
        if fields.len() < 6 {
            return Err(unexpected());
        }
        let kib = |field: &str| {
            field
                .parse::<u64>()
                .ok()
                .and_then(|kib| kib.checked_mul(1024))
                .ok_or_else(unexpected)
        };
        let usage = Self {
            size: kib(fields[1])?,
            used: kib(fields[2])?,
            available: kib(fields[3])?,
        };
        if usage.size == 0 || usage.used > usage.size || usage.available > usage.size {
            return Err(unexpected());
        }
        Ok(usage)
    }

    /// Less than [`LOW_FREE`] left with more than half of it used: a tiny
    /// engine that never filled up is not low.
    pub fn is_low(self) -> bool {
        self.available < LOW_FREE && self.used.saturating_mul(2) > self.size
    }

    /// `16.0 of 20.0 GiB used, 4.0 GiB free`.
    pub fn describe(self) -> String {
        let gib = |bytes: u64| bytes as f64 / GIB as f64;
        format!(
            "{:.1} of {:.1} GiB used, {:.1} GiB free",
            gib(self.used),
            gib(self.size),
            gib(self.available)
        )
    }
}

/// The fullest sample of a run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoragePeak {
    peak: Option<StorageUsage>,
}

impl StoragePeak {
    /// Keep `usage` if it is the fullest yet. True when it is the first
    /// sample to find the storage low, the moment to warn.
    pub fn record(&mut self, usage: StorageUsage) -> bool {
        let was_low = self.peak.is_some_and(StorageUsage::is_low);
        if self.peak.is_none_or(|peak| usage.used > peak.used) {
            self.peak = Some(usage);
        }
        !was_low && usage.is_low()
    }

    pub fn peak(self) -> Option<StorageUsage> {
        self.peak
    }
}

/// The log line for a sample that found the storage low.
pub fn low_warning(usage: StorageUsage) -> String {
    format!(
        "warning: the engine's storage is low ({}); a step that needs free space may fail",
        usage.describe()
    )
}

/// The log line closing a run's sampling.
pub fn peak_note(peak: StorageUsage) -> String {
    format!("engine storage peaked at {}", peak.describe())
}

/// Why a failed run may have failed, when its storage ran low.
pub fn failure_reason(peak: StorageUsage) -> Option<String> {
    peak.is_low().then(|| {
        format!(
            "the engine's storage ran low ({} at its peak): a step that needs free space \
             (soldr refuses to build under 5 GiB) or hit \"no space left on device\" may have \
             failed for it; raise storage_gib under [engine] in <state>/config.toml",
            peak.describe()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(size_gib: f64, used_gib: f64) -> StorageUsage {
        let size = (size_gib * GIB as f64) as u64;
        let used = (used_gib * GIB as f64) as u64;
        StorageUsage {
            size,
            used,
            available: size - used,
        }
    }

    #[test]
    fn df_output_parses_into_bytes() {
        let df = "Filesystem           1024-blocks    Used Available Capacity Mounted on\n\
                  tmpfs                 20971520 16777216   4194304  80% /var/lib/docker\n";
        assert_eq!(
            StorageUsage::parse_df(df).unwrap(),
            StorageUsage {
                size: 20 * GIB,
                used: 16 * GIB,
                available: 4 * GIB,
            }
        );
        for bad in [
            "",
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n",
            "tmpfs 1 1 0 100% /x\ntmpfs 1 1 0 100% /x\n",
            "Filesystem 1024-blocks Used Available Capacity Mounted on\ntmpfs x 1 0 1% /x\n",
            "Filesystem 1024-blocks Used Available Capacity Mounted on\ntmpfs 1 2 0 1% /x\n",
            "Filesystem 1024-blocks Used Available Capacity Mounted on\ntmpfs 0 0 0 0% /x\n",
            "Filesystem 1024-blocks Used Available Capacity Mounted on\ntmpfs 1 1\n",
        ] {
            assert!(StorageUsage::parse_df(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn low_means_little_free_on_a_mostly_used_engine() {
        // clud's build-linux-x64 on the old 20 GiB engine (#392).
        assert!(usage(20.0, 16.0).is_low());
        assert!(!usage(36.0, 16.0).is_low());
        // A small engine is not low just for being small.
        assert!(!usage(2.0, 0.1).is_low());
        assert!(usage(2.0, 1.9).is_low());
        assert_eq!(
            usage(20.0, 16.0).describe(),
            "16.0 of 20.0 GiB used, 4.0 GiB free"
        );
    }

    #[test]
    fn the_peak_keeps_the_fullest_sample_and_warns_once() {
        let mut peak = StoragePeak::default();
        assert_eq!(peak.peak(), None);
        assert!(!peak.record(usage(20.0, 4.0)));
        assert!(peak.record(usage(20.0, 16.0)), "first low sample warns");
        assert!(!peak.record(usage(20.0, 17.0)), "only once");
        assert!(!peak.record(usage(20.0, 2.0)), "emptying is not a new peak");
        assert_eq!(peak.peak(), Some(usage(20.0, 17.0)));
    }

    #[test]
    fn a_failed_run_on_low_storage_says_so_and_names_the_setting() {
        let reason = failure_reason(usage(20.0, 16.0)).unwrap();
        assert!(reason.contains("storage ran low"), "{reason}");
        assert!(reason.contains("16.0 of 20.0 GiB used, 4.0 GiB free"));
        assert!(reason.contains("storage_gib"));
        assert_eq!(failure_reason(usage(36.0, 16.0)), None);
        assert!(peak_note(usage(36.0, 16.0)).contains("16.0 of 36.0 GiB"));
        assert!(low_warning(usage(20.0, 16.0)).starts_with("warning:"));
    }
}
