//! Default bounded expiry for the legacy per-repository cache namespaces (#544 box 5, #545).
//!
//! Production CI still routes every repository to a legacy namespace,
//! `/bosn/cache/actcache/<16 hex>`, and no machine has an enrolled cohort policy. Without
//! this pass nothing ever expired those archives: the shared `bosn-ci-cache-v1` volume reached
//! 56–60 GB on the development host. The documented budget ([`LegacyBudget::DEFAULT`],
//! `docs/ci.md` "Shared cache budget") is applied by act2's own per-namespace retention
//! (`act cache prune --apply`), the same code act2's cache server runs in-process:
//!
//! - **Liveness:** act2 takes the namespace's transfer lock and its `bolt.db` exclusively, each
//!   with a 25 ms timeout. A live cache server or transfer makes the namespace report `busy`, and
//!   nothing in it is touched. Archives used within five minutes are always protected.
//! - **Ownership:** only the Bosn-owned, measured `bosn-ci-cache-v1` volume is mounted, in the
//!   journaled finite-lived maintenance helper. Only direct 16-hex children are visited; the
//!   cohort root and routing records are never.
//! - **Pins:** the volume itself is pinned and is never removed by this pass; expiry removes
//!   archives inside it, never the volume.
//! - **Exclusion:** the pass holds the machine maintenance lock that cohort maintenance and
//!   import hold, so it never overlaps them, and an enrolled cohort policy replaces it.
//!
//! A namespace without act2's transfer coordination file (written by an act2 that predates it)
//! cannot be locked, so it is skipped and named on stderr, never deleted.
use super::{DockerActBackend, ENGINE_CACHE, ENGINE_WORK, MaintenanceAttempt, RunOptions};
use crate::ci::cache_maintenance::{CohortReport, NamespaceAudit, StoreStatus};
use std::time::Duration;

/// The shared-cache budget applied when no cohort policy is enrolled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyBudget {
    /// Completed archive bytes kept per repository namespace (least recently used go first).
    pub namespace_max_bytes: i64,
    /// Archives unused this long expire, so an idle repository's namespace empties.
    pub unused_age_secs: u64,
    /// Archives older than this expire even if still used.
    pub max_age_secs: u64,
    /// Time between passes.
    pub interval_secs: u64,
}

impl LegacyBudget {
    /// 8 GiB per namespace, 7 days unused, 30 days absolute, hourly. The two largest
    /// namespaces on the development host held 23–24 GB of 0.4–1.4 GB archives; 8 GiB keeps
    /// several complete toolchain/target caches per repository warm.
    pub const DEFAULT: Self = Self {
        namespace_max_bytes: 8 * 1024 * 1024 * 1024,
        unused_age_secs: 7 * 24 * 3600,
        max_age_secs: 30 * 24 * 3600,
        interval_secs: 3600,
    };
}

/// Namespaces visited per pass; more is reported as partial rather than walked unbounded.
pub(super) const MAX_NAMESPACES: usize = 64;

/// The root this pass reports, distinct from the cohort root.
pub fn legacy_root() -> String {
    format!("{ENGINE_CACHE}/actcache")
}

/// One pass over every coordinated legacy namespace, under the machine maintenance lock.
pub(super) fn pass_script(budget: LegacyBudget) -> String {
    format!(
        "set -u; root={root}; \
         [ -d \"$root\" ] && [ ! -L \"$root\" ] || exit 0; \
         exec 6>>\"$root/.bosn-maintenance-v1.lock\" || exit $?; \
         flock -x -n 6 || {{ echo 'machine cache maintenance busy' >&2; exit 75; }}; \
         n=0; status=0; \
         for d in \"$root\"/*; do \
           b=${{d##*/}}; [ ${{#b}} -eq 16 ] || continue; \
           case $b in *[!0-9a-f]*) continue;; esac; \
           [ -d \"$d\" ] && [ ! -L \"$d\" ] || continue; \
           if [ ! -f \"$d/transfers.bolt\" ] || [ -L \"$d/transfers.bolt\" ]; then \
             echo \"uncoordinated $b\" >&2; continue; fi; \
           n=$((n+1)); [ $n -le {max} ] || {{ echo 'namespace limit reached' >&2; exit 76; }}; \
           {act} cache prune --apply --cache-server-path \"$d\" \
             --cache-server-max-bytes {bytes} --cache-server-max-age {age}s \
             --cache-server-unused-age {unused}s --cache-server-gc-interval {interval}s \
             || status=$?; \
         done; exit $status",
        root = legacy_root(),
        max = MAX_NAMESPACES,
        act = format_args!("{ENGINE_WORK}/bin/act"),
        bytes = budget.namespace_max_bytes,
        age = budget.max_age_secs,
        unused = budget.unused_age_secs,
        interval = budget.interval_secs,
    )
}

impl DockerActBackend {
    /// Run one pass in an already verified maintenance helper (see `maintain_cache_with_helper`).
    /// The deadline stays inside the helper's 300 s lifetime.
    pub(super) async fn maintain_legacy_namespaces(
        &self,
        helper: &str,
        budget: LegacyBudget,
    ) -> Result<MaintenanceAttempt, String> {
        let output = self
            .run_bounded(
                Self::exec(helper, &pass_script(budget)),
                RunOptions::bounded(Duration::from_secs(240), 256 * 1024),
            )
            .await
            .map_err(|e| format!("legacy cache maintenance outcome is unknown: {e}"))?;
        let diagnostic = String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(256)
            .collect::<String>();
        let report = parse_pass(&output.stdout, output.exit_code, budget).map_err(|e| {
            format!("legacy cache maintenance has no valid outcome evidence: {e}; {diagnostic}")
        })?;
        Ok(MaintenanceAttempt {
            exit_code: output.exit_code,
            report,
            diagnostic,
        })
    }
}

/// Parse one pass's stdout (one act2 store audit per line) into the report shape the
/// maintenance snapshot persists. `budget_bytes` is the per-namespace budget; remaining and
/// protected bytes are totals across the namespaces visited.
pub(super) fn parse_pass(
    stdout: &[u8],
    exit_code: i32,
    budget: LegacyBudget,
) -> Result<CohortReport, String> {
    if stdout.len() > 256 * 1024 {
        return Err("legacy maintenance output exceeds its bound".into());
    }
    let text = std::str::from_utf8(stdout).map_err(|_| "legacy maintenance output is not UTF-8")?;
    let mut namespaces = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let audit: NamespaceAudit = serde_json::from_str(line).map_err(|e| e.to_string())?;
        crate::ci::cache_cohort::Namespace::parse(&audit.namespace)?;
        if audit.schema_version != 1 || !seen.insert(audit.namespace.clone()) {
            return Err("legacy namespace identity mismatch".into());
        }
        if let Some(retention) = &audit.retention {
            retention.validate(budget.namespace_max_bytes)?;
        }
        namespaces.push(audit);
    }
    if namespaces.len() > MAX_NAMESPACES {
        return Err("legacy maintenance reported too many namespaces".into());
    }
    // A busy namespace is a live server holding it: protected, not a failure, but the pass
    // cannot prove its total, so the report is partial (and retried next interval).
    let partial = exit_code != 0
        || namespaces.iter().any(|ns| {
            ns.partial
                || ns.status != StoreStatus::Ready
                || ns.retention.as_ref().is_none_or(|r| {
                    r.remaining_completed_bytes.is_none()
                        || r.protected_bytes.is_none()
                        || r.budget_met.is_none()
                })
        });
    let sum = |field: fn(&NamespaceAudit) -> Option<u64>| -> Option<u64> {
        namespaces
            .iter()
            .try_fold(0u64, |total, ns| total.checked_add(field(ns)?))
    };
    let (remaining, protected, met) = if partial {
        (None, None, None)
    } else {
        (
            sum(|ns| ns.retention.as_ref()?.remaining_completed_bytes),
            sum(|ns| ns.retention.as_ref()?.protected_bytes),
            Some(
                namespaces
                    .iter()
                    .all(|ns| ns.retention.as_ref().and_then(|r| r.budget_met) == Some(true)),
            ),
        )
    };
    Ok(CohortReport {
        schema_version: 1,
        root: legacy_root(),
        partial,
        errors: None,
        budget_bytes: budget.namespace_max_bytes,
        remaining_completed_bytes: remaining,
        protected_bytes: protected,
        budget_met: met,
        namespaces: Some(namespaces),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audit(namespace: &str, status: &str, remaining: u64, reclaimed: u64) -> String {
        format!(
            "{{\"schema_version\":1,\"namespace\":\"{namespace}\",\"status\":\"{status}\",\
             \"partial\":false,\"errors\":null,\"entry_count\":1,\"archive_bytes\":{remaining},\
             \"temporary_bytes\":0,\"untracked_bytes\":0,\"fingerprint\":\"f\",\"entries\":null,\
             \"next_cursor\":null,\"retention\":{{\"deleted_count\":{n},\
             \"reclaimed_archive_bytes\":{reclaimed},\"receipts\":{receipts},\
             \"receipts_omitted\":0,\"budget_bytes\":{budget},\
             \"remaining_completed_bytes\":{remaining},\"protected_bytes\":0,\
             \"budget_met\":{met}}}}}",
            n = u64::from(reclaimed > 0),
            receipts = if reclaimed > 0 {
                format!("[{{\"id\":7,\"reason\":\"byte_budget\",\"archive_bytes\":{reclaimed}}}]")
            } else {
                "null".into()
            },
            budget = LegacyBudget::DEFAULT.namespace_max_bytes,
            met = remaining <= LegacyBudget::DEFAULT.namespace_max_bytes as u64,
        )
    }

    #[test]
    fn complete_pass_totals_every_namespace_against_the_per_namespace_budget() {
        let out = format!(
            "{}\n{}\n",
            audit("5c15c12850dcf5b1", "ready", 8_000_000_000, 15_000_000_000),
            audit("4f135ead70a0b283", "ready", 1_310_000_000, 0)
        );
        let report = parse_pass(out.as_bytes(), 0, LegacyBudget::DEFAULT).unwrap();
        assert!(!report.partial);
        assert_eq!(report.root, "/bosn/cache/actcache");
        assert_eq!(report.remaining_completed_bytes, Some(9_310_000_000));
        assert_eq!(report.budget_met, Some(true));
        assert_eq!(report.budget_bytes, 8 * 1024 * 1024 * 1024);
    }

    #[test]
    fn a_busy_namespace_or_failed_exit_makes_the_pass_partial_and_claims_no_totals() {
        let busy = audit("5c15c12850dcf5b1", "busy", 1, 0);
        let report = parse_pass(busy.as_bytes(), 0, LegacyBudget::DEFAULT).unwrap();
        assert!(report.partial);
        assert_eq!(report.remaining_completed_bytes, None);
        let ready = audit("5c15c12850dcf5b1", "ready", 1, 0);
        assert!(
            parse_pass(ready.as_bytes(), 1, LegacyBudget::DEFAULT)
                .unwrap()
                .partial
        );
    }

    #[test]
    fn foreign_or_duplicate_namespaces_and_wrong_budgets_are_refused() {
        let good = audit("5c15c12850dcf5b1", "ready", 1, 0);
        for bad in [
            good.replace("5c15c12850dcf5b1", "cohort-v1"),
            format!("{good}\n{good}"),
            good.replace("\"budget_bytes\":8589934592", "\"budget_bytes\":1"),
            "not json".into(),
        ] {
            assert!(
                parse_pass(bad.as_bytes(), 0, LegacyBudget::DEFAULT).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn script_visits_only_coordinated_hex_namespaces_under_the_machine_lock() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let root = dir.path().join("actcache");
        for (name, coordinated) in [
            ("5c15c12850dcf5b1", true),
            ("330e48ea825a774c", false),
            ("cohort-v1", true),
            ("5C15C12850DCF5B1", true),
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            if coordinated {
                std::fs::write(root.join(name).join("transfers.bolt"), "").unwrap();
            }
        }
        let fake = dir.path().join("act");
        std::fs::write(&fake, "#!/bin/sh\necho \"$@\"\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let script = pass_script(LegacyBudget::DEFAULT)
            .replace(&format!("{ENGINE_WORK}/bin/act"), fake.to_str().unwrap())
            .replace(&legacy_root(), root.to_str().unwrap());
        let output = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.lines().count(), 1, "{stdout}");
        assert!(stdout.contains("5c15c12850dcf5b1 --cache-server-max-bytes 8589934592"));
        assert!(stdout.contains("--cache-server-unused-age 604800s"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("uncoordinated 330e48ea825a774c"));
        // flock(1) holds the machine maintenance lock while the script runs: busy, no prune.
        let lock = root.join(".bosn-maintenance-v1.lock");
        let held = std::process::Command::new("flock")
            .args(["-x", lock.to_str().unwrap(), "sh", "-c", &script])
            .output()
            .unwrap();
        assert_eq!(held.status.code(), Some(75), "{held:?}");
        assert!(held.stdout.is_empty());
    }
}
