//! Bounded online retry. Active in-process measurements are never interrupted.
use super::{CONTROL_DEADLINE, DockerActBackend, helper::Identity, journal::Tracker, owned};
use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};
use bosn_registry::cache_helper::{CacheHelperRecord, CacheHelperState};

#[derive(Default)]
pub struct HelperCleanupRetry {
    pub next_nonce: Option<String>,
    pub removed: Option<String>,
    pub deferred: Option<String>,
}

impl DockerActBackend {
    pub(in crate::ci::engine) async fn retry_measurements(
        &self,
        registry: &RegistryActor,
        owner: &str,
        mut cursor: Option<String>,
    ) -> Result<HelperCleanupRetry, String> {
        for _ in 0..8 {
            let page_start = cursor.clone();
            let reply = registry
                .act_registry(ActRegistryCommand::HelperPending {
                    after_nonce: cursor.clone(),
                    limit: 64,
                })
                .await
                .map_err(|error| error.to_string())?;
            let ActRegistryReply::Helpers(page) = reply else {
                return Err("cache helper page reply mismatch".into());
            };
            for record in page.items {
                cursor = Some(record.intent.nonce.clone());
                if record.intent.registry_id != owner {
                    return Err("cache helper registry owner mismatch".into());
                }
                if self
                    .active_helpers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(&record.intent.nonce)
                {
                    continue;
                }
                let result = self.reconcile_measurement(registry, &record).await;
                return Ok(HelperCleanupRetry {
                    next_nonce: cursor,
                    removed: result.is_ok().then(|| record.intent.name()),
                    deferred: result.err(),
                });
            }
            match page.next_nonce {
                Some(next) if page_start.as_ref().is_none_or(|old| old < &next) => {
                    cursor = Some(next)
                }
                Some(_) => return Err("cache helper cursor did not advance".into()),
                None => return Ok(HelperCleanupRetry::default()),
            }
        }
        Ok(HelperCleanupRetry {
            next_nonce: cursor,
            ..Default::default()
        })
    }

    async fn reconcile_measurement(
        &self,
        registry: &RegistryActor,
        record: &CacheHelperRecord,
    ) -> Result<(), String> {
        let identity = Identity::from_intent(&record.intent)?;
        let tracker = Tracker::new(registry, &record.intent.nonce);
        if record.state == CacheHelperState::Pending {
            return self
                .recover_measurement(&identity, &record.intent.volume, Some(&tracker))
                .await;
        }
        let id = record
            .container_id
            .as_deref()
            .ok_or("created helper has no immutable ID")?;
        let result = self
            .run(owned(&["container", "inspect", id]), CONTROL_DEADLINE)
            .await?;
        if !result.ok() {
            // Known create acknowledgement was committed before start. ID
            // absence now can safely finish an already auto-removed helper.
            self.confirm_measurement_absent(id).await?;
            return tracker.finish(id).await;
        }
        let observed = identity.verify(
            &String::from_utf8_lossy(&result.stdout),
            &record.intent.volume,
        )?;
        if observed != id {
            return Err("cache helper immutable ID mismatch".into());
        }
        self.remove_measurement(id).await?;
        tracker.finish(id).await
    }
}
