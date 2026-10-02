//! Which job of the run tree an act record belongs to (#404).
//!
//! act names a matrix leg `<workflow>/<name>-<n>` and numbers the legs in the
//! order Go's map iteration produced their combinations, so `n` is no
//! identity, and the legs' records arrive interleaved in any order. A leg is
//! therefore keyed by its own matrix values ([`RunTree::leg_key`]) and placed
//! in its job's stage beside its siblings, ordered by key
//! ([`RunTree::job_for`]): the same legs give the same tree in every run.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{ItemStatus, Job, RunTree, job_name_from_key};

/// `<name>` for act's `<name>-<n>`.
fn strip_leg_number(key: &str) -> &str {
    match key.rsplit_once('-') {
        Some((name, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => key,
    }
}

/// A matrix value as a job name shows it.
fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The caller a key is namespaced under: the workflow's name for its own
/// jobs (`CI/…`), the calling job's name for a reusable workflow's jobs.
fn namespace(key: &str) -> Option<&str> {
    key.split_once('/').map(|(namespace, _)| namespace)
}

/// A job act reported; a declared placeholder is keyed by its bare job ID.
fn materialized(job: &Job) -> bool {
    job.key.contains('/')
}

impl RunTree {
    /// The key of the job an act record names: act's own key, or for a
    /// matrix leg its name without act's number, plus (in key order) the
    /// matrix values that name does not show, as GitHub shows a leg whose
    /// name does not use the matrix. Two legs that would still share a key
    /// are told apart by their whole matrix.
    pub(super) fn leg_key(&self, act_key: &str, matrix: Option<&Value>) -> String {
        let Some(values) = matrix
            .and_then(Value::as_object)
            .filter(|values| !values.is_empty())
        else {
            return act_key.to_string();
        };
        let name = strip_leg_number(act_key);
        let ordered: BTreeMap<&String, &Value> = values.iter().collect();
        let missing: Vec<String> = ordered
            .values()
            .map(|value| display(value))
            .filter(|value| !name.contains(value.as_str()))
            .collect();
        let key = if missing.is_empty() {
            name.to_string()
        } else {
            format!("{name} ({})", missing.join(", "))
        };
        match self.find(&key) {
            Some((g, j)) if self.groups[g].jobs[j].matrix.as_ref() != matrix => {
                format!(
                    "{key} {}",
                    serde_json::to_string(&ordered).unwrap_or_default()
                )
            }
            _ => key,
        }
    }

    /// Find the job for an act record, materializing it the first time it
    /// appears. A declared placeholder (key == job ID) is replaced by its
    /// first leg. A job that calls a reusable workflow never runs under its
    /// own key: act runs the called jobs as `<caller name>/<workflow>/<job>`,
    /// and they stand in for the caller the same way, in the caller's stage.
    /// Later legs (and called jobs) join their siblings, ordered by key.
    pub(super) fn job_for(&mut self, key: &str, job_id: &str, matrix: Option<&Value>) -> &mut Job {
        if let Some((g, j)) = self.find(key) {
            return &mut self.groups[g].jobs[j];
        }
        let job = Job {
            key: key.into(),
            job_id: job_id.into(),
            name: job_name_from_key(key),
            matrix: matrix.filter(|m| !m.is_null()).cloned(),
            status: ItemStatus::Queued,
            conclusion: None,
            reason: None,
            sections: Vec::new(),
        };
        if let Some((g, j)) = self.placeholder(key, job_id) {
            let declared = &self.groups[g].jobs[j];
            // A matrix leg or a called job is named by its own key; the
            // declared job itself keeps its declared name.
            let keeps_declared = job.matrix.is_none() && declared.job_id == job.job_id;
            let name = if keeps_declared {
                declared.name.clone()
            } else {
                job.name.clone()
            };
            self.groups[g].jobs[j] = Job { name, ..job };
            return &mut self.groups[g].jobs[j];
        }
        let own = namespace(key);
        let same_job = |other: &Job| {
            materialized(other) && other.job_id == job_id && namespace(&other.key) == own
        };
        let same_call = |other: &Job| materialized(other) && namespace(&other.key) == own;
        let (g, at) = match self
            .family_slot(key, same_job)
            .or_else(|| self.family_slot(key, same_call))
        {
            Some(slot) => slot,
            None => {
                self.group_mut("0");
                let g = self.groups.iter().position(|x| x.name == "0").unwrap_or(0);
                (g, self.groups[g].jobs.len())
            }
        };
        self.groups[g].jobs.insert(at, job);
        &mut self.groups[g].jobs[at]
    }

    /// The declared, not yet reported job a record replaces: its own job ID,
    /// or the reusable-workflow caller whose name prefixes its key.
    fn placeholder(&self, key: &str, job_id: &str) -> Option<(usize, usize)> {
        let called_by = |job: &Job| {
            key.strip_prefix(job.name.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        };
        self.groups.iter().enumerate().find_map(|(g, group)| {
            group
                .jobs
                .iter()
                .position(|job| {
                    job.key == job.job_id
                        && job.matrix.is_none()
                        && (job.job_id == job_id || called_by(job))
                })
                .map(|j| (g, j))
        })
    }

    /// The group of the first job in `family`, and the index there that
    /// keeps the family ordered by key.
    fn family_slot(&self, key: &str, family: impl Fn(&Job) -> bool) -> Option<(usize, usize)> {
        let g = self
            .groups
            .iter()
            .position(|group| group.jobs.iter().any(&family))?;
        let jobs = &self.groups[g].jobs;
        let members: Vec<usize> = (0..jobs.len()).filter(|&j| family(&jobs[j])).collect();
        let at = members
            .iter()
            .copied()
            .find(|&j| jobs[j].key.as_str() > key)
            .unwrap_or(members.last()? + 1);
        Some((g, at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_legs_key_is_its_name_and_the_matrix_values_it_does_not_show() {
        let tree = RunTree::default();
        let key = |act: &str, matrix: Value| tree.leg_key(act, Some(&matrix));
        assert_eq!(
            key(
                "CI/Native wheel (ubuntu-latest)-1",
                json!({"os": "ubuntu-latest", "python": "3.11"})
            ),
            "CI/Native wheel (ubuntu-latest) (3.11)"
        );
        assert_eq!(
            key("w/lint-2", json!({"tool": "ruff", "n": 2})),
            "w/lint (2, ruff)",
            "values in matrix-key order, whatever act numbered the leg"
        );
        assert_eq!(key("w/m (a)", json!({"os": "a"})), "w/m (a)");
        assert_eq!(key("w/first", json!({})), "w/first", "no matrix, act's key");
        assert_eq!(tree.leg_key("w/first", None), "w/first");
    }
}
