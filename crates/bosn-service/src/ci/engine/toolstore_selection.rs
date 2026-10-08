//! Admission of a complete successor before any immutable payload is copied.
use super::toolstore_records::Install;

pub(super) struct Candidate {
    pub path: String,
    pub object_id: String,
    pub bytes: i64,
    pub entries: usize,
}

pub(super) fn choose(
    mut candidates: Vec<Candidate>,
    current: &[Install],
    max_bytes: i64,
) -> Result<Vec<Candidate>, String> {
    use std::collections::{BTreeMap, BTreeSet};
    let old: BTreeMap<_, _> = current
        .iter()
        .map(|i| (i.path.as_str(), i.object_id.as_str()))
        .collect();
    let mut paths = BTreeSet::new();
    if max_bytes <= 0
        || candidates
            .iter()
            .any(|c| c.bytes < 0 || c.entries > 100000 || !paths.insert(c.path.clone()))
    {
        return Err("tool successor admission has invalid or duplicate candidates".into());
    }
    // New or changed installs get room before unchanged seeded payloads.
    // Stable path order makes admission reproducible within each priority.
    candidates.sort_by(|a, b| {
        let unchanged =
            |c: &Candidate| old.get(c.path.as_str()).copied() == Some(c.object_id.as_str());
        unchanged(a)
            .cmp(&unchanged(b))
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut bytes = 0i64;
    let mut entries = 1usize;
    let mut selected = Vec::new();
    for c in candidates {
        // Count each install's tree plus its path ancestors and completion
        // marker. Shared ancestors are overcounted for conservative admission.
        let cost = c
            .entries
            .checked_add(c.path.split('/').count())
            .and_then(|n| n.checked_add(1));
        let next_entries = cost.and_then(|n| entries.checked_add(n));
        let next_bytes = bytes.checked_add(c.bytes);
        if selected.len() == 128
            || next_bytes.is_none_or(|n| n > max_bytes)
            || next_entries.is_none_or(|n| n > 100000)
        {
            continue;
        }
        bytes = next_bytes.ok_or("tool payload admission overflow")?;
        entries = next_entries.ok_or("tool entry admission overflow")?;
        selected.push(c);
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(version: usize, bytes: i64) -> Candidate {
        Candidate {
            path: format!("Go/{version}/x64"),
            object_id: format!("{version:064x}"),
            bytes,
            entries: 2,
        }
    }

    #[test]
    fn new_install_replaces_old_selection_before_payload_admission() {
        let old = candidate(1, 4);
        let current = vec![Install {
            path: old.path.clone(),
            object_id: old.object_id.clone(),
        }];
        let selected = choose(vec![old, candidate(2, 4)], &current, 4).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].path, "Go/2/x64");
    }

    #[test]
    fn install_count_is_bounded_and_new_version_gets_room() {
        let current: Vec<_> = (1..=128)
            .map(|v| {
                let c = candidate(v, 1);
                Install {
                    path: c.path,
                    object_id: c.object_id,
                }
            })
            .collect();
        let selected = choose(
            (1..=1024).map(|v| candidate(v, 1)).collect(),
            &current,
            1000,
        )
        .unwrap();
        assert_eq!(selected.len(), 128);
        assert!(selected.iter().any(|c| c.path == "Go/129/x64"));
    }
    #[test]
    fn changed_install_wins_and_entry_budget_is_conservative() {
        let current = vec![
            Install {
                path: "Go/9/x64".into(),
                object_id: format!("{:064x}", 0),
            },
            Install {
                path: "Go/2/x64".into(),
                object_id: format!("{:064x}", 2),
            },
        ];
        let mut changed = candidate(9, 1);
        changed.entries = 99994;
        let selected = choose(vec![candidate(2, 1), changed], &current, 2).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].path, "Go/9/x64");
        assert!(selected[0].entries < 100000);
    }

    #[test]
    fn invalid_counts_and_duplicate_candidates_fail_before_admission() {
        assert!(choose(vec![candidate(1, -1)], &[], 100).is_err());
        assert!(choose(vec![candidate(1, 1), candidate(1, 1)], &[], 100).is_err());
        assert!(choose(vec![candidate(1, 1)], &[], 0).is_err());
        let selected =
            choose(vec![candidate(1, i64::MAX), candidate(2, 1)], &[], i64::MAX).unwrap();
        assert_eq!(selected.len(), 1);
    }
}
