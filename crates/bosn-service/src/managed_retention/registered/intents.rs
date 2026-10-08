//! Verify planned volume ownership before releasing abandoned-intent protection.

use super::*;
use bosn_registry::VolumeCreationIntent;

pub(super) struct PendingVolume {
    intent: VolumeCreationIntent,
    recorded: bool,
}

impl PendingVolume {
    pub(super) fn protects(&self, name: &str, labels: &BTreeMap<String, String>) -> bool {
        self.intent.name == name
            && (!self.recorded
                || self.intent.labels.is_empty()
                || !self
                    .intent
                    .labels
                    .iter()
                    .all(|(key, value)| labels.get(key) == Some(value)))
    }
}

pub(super) fn load(
    registry: &ReadOnlyRegistry,
    resources: &[Resource],
) -> Result<Vec<PendingVolume>, String> {
    let mut pending = Vec::new();
    let mut offset = 0;
    loop {
        crate::managed_retention::budget::check()?;
        let page = registry
            .volume_creation_intents(offset, 64)
            .map_err(|error| error.to_string())?;
        for intent in page.items {
            let recorded = resources.iter().any(|resource| {
                resource.kind == ResourceKind::Volume
                    && resource.id == format!("manifest-volume:{}", intent.name)
                    && resource.name == intent.name
                    && resource.stack == intent.stack
                    && resource.generation == intent.generation
                    && resource.scope == intent.scope
                    && resource.workspace == intent.workspace
                    && resource.last_used.is_finite()
            });
            pending.push(PendingVolume { intent, recorded });
            check_record_count(pending.len())?;
        }
        let Some(next) = page.next_offset else { break };
        offset = next;
    }
    Ok(pending)
}
