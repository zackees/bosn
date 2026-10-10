//! Everything one pass may use to prove ownership, beyond an object's own labels (#545).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bosn_core::ResourceKind;

use super::{catalog, now_seconds, registered::RegisteredOwnership};

/// This registry, its records, and the abandoned registries the machine daemon may reclaim for.
pub(super) struct Authority {
    ownership: Option<RegisteredOwnership>,
    our: Option<String>,
    abandoned: BTreeSet<String>,
}

/// One observation after the registry and the catalog had their say.
pub(super) struct Proven {
    pub labels: BTreeMap<String, String>,
    pub age: f64,
    pub in_use: bool,
}

impl Authority {
    /// A fresh read. Unreadable sources prove nothing, so objects they would cover stay held.
    pub(super) fn load(state_dir: &Path) -> Self {
        let our = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3"))
            .ok()
            .and_then(|registry| registry.registry_id().ok());
        Self {
            ownership: RegisteredOwnership::load(state_dir).ok(),
            abandoned: catalog::abandoned(state_dir, our.as_deref()),
            our,
        }
    }

    pub(super) fn our(&self) -> Option<&str> {
        self.our.as_deref()
    }

    pub(super) fn abandoned(&self) -> &BTreeSet<String> {
        &self.abandoned
    }

    /// Apply every proof that applies; otherwise the engine's labels stand as they are.
    ///
    /// A setup-labelled object recorded in this registry gains its canonical labels, and only
    /// becomes *harder* to remove: its age is the shorter of the engine age and the registry's
    /// idle time, and a lease, session or creation intent marks it in use. An object whose
    /// complete labels name an abandoned registry is judged as ours, under the same gates.
    pub(super) fn prove(
        &self,
        kind: ResourceKind,
        name: &str,
        mut labels: BTreeMap<String, String>,
        mut age: f64,
        mut in_use: bool,
    ) -> Proven {
        if let Some(proof) = self
            .ownership
            .as_ref()
            .and_then(|ownership| ownership.normalize(kind, name, &labels, now_seconds()))
        {
            labels = proof.labels;
            age = age.min(proof.idle_seconds);
            in_use |= proof.protected;
        } else if let Some(ours) = self.our.as_deref()
            && labels
                .get(bosn_core::LABEL_REGISTRY)
                .is_some_and(|registry| self.abandoned.contains(registry))
        {
            labels.insert(bosn_core::LABEL_REGISTRY.to_owned(), ours.to_owned());
        }
        Proven {
            labels,
            age,
            in_use,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosn_core::retention::{HoldReason, RetentionPolicy, classify_managed};

    const OURS: &str = "11111111-2222-4333-8444-555555555555";
    const GONE: &str = "aaaaaaaa-2222-4333-8444-555555555555";
    const LIVE: &str = "bbbbbbbb-2222-4333-8444-555555555555";

    fn labels(registry: &str) -> BTreeMap<String, String> {
        bosn_core::ResourceLabels::new(
            registry,
            ResourceKind::Volume,
            "act",
            "g1",
            bosn_core::Scope::Stack,
            "/tmp/workspace",
            "1",
            None,
        )
        .unwrap()
        .to_map()
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
    }

    fn hold(authority: &Authority, registry: &str, in_use: bool) -> Option<HoldReason> {
        let proven = authority.prove(
            ResourceKind::Volume,
            "v",
            labels(registry),
            30.0 * 86_400.0,
            in_use,
        );
        let artifact = bosn_core::ObservedArtifact {
            id: "v".into(),
            kind: ResourceKind::Volume,
            labels: proven.labels,
            signals: bosn_core::Signals {
                in_use: proven.in_use,
                ..bosn_core::Signals::default()
            },
            bytes: Some(1),
            age_seconds: Some(proven.age),
        };
        classify_managed(&artifact, authority.our(), RetentionPolicy::default()).hold
    }

    #[test]
    fn only_an_abandoned_registrys_objects_are_judged_as_ours() {
        let authority = Authority {
            ownership: None,
            our: Some(OURS.into()),
            abandoned: BTreeSet::from([GONE.to_owned()]),
        };
        assert_eq!(hold(&authority, GONE, false), None);
        assert_eq!(hold(&authority, GONE, true), Some(HoldReason::InUse));
        assert_eq!(
            hold(&authority, LIVE, false),
            Some(HoldReason::ForeignRegistry)
        );
        let no_owner = Authority {
            ownership: None,
            our: None,
            abandoned: BTreeSet::from([GONE.to_owned()]),
        };
        assert_eq!(
            hold(&no_owner, GONE, false),
            Some(HoldReason::ForeignRegistry)
        );
    }
}
