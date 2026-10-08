//! Online retry of durable engine cleanup; never interrupts active execution.
use super::*;
use crate::act_registry::{ActRegistryCommand, ActRegistryReply};
use bosn_registry::act::ActEngineState;

pub(crate) struct CleanupRetry {
    pub next_cursor: Option<String>,
    pub retired: Option<String>,
    pub deferred: Option<String>,
}

impl CiRuntime {
    pub(crate) async fn maintain_existing_cohort(
        &self,
        owner: &str,
        stop: &async_engine::CancellationToken,
    ) {
        self.maintain_cache_with_policy(owner, stop, Duration::from_secs(60))
            .await;
    }

    pub(crate) async fn retry_cache_helper_cleanup(
        &self,
        owner: &str,
        cursor: Option<String>,
    ) -> Result<super::super::engine::HelperCleanupRetry, String> {
        self.backend
            .retry_cache_helpers(&self.registry, owner, cursor)
            .await
    }

    /// Inspect at most 512 pending records and retire at most one engine.
    /// The persistent caller cursor prevents active or repeatedly failing
    /// records at the front from starving later cleanup. Registry ownership
    /// and exact removal authorization remain the backend's hard gates.
    pub(crate) async fn retry_cleanup(
        &self,
        owner: &str,
        mut cursor: Option<String>,
    ) -> Result<CleanupRetry, String> {
        for _ in 0..8 {
            let page_start = cursor.clone();
            let reply = self
                .registry
                .act_registry(ActRegistryCommand::Pending {
                    after_run_id: cursor.clone(),
                    limit: 64,
                })
                .await
                .map_err(|e| e.to_string())?;
            let ActRegistryReply::Recovery(page) = reply else {
                return Err("cleanup retry page reply mismatch".into());
            };
            for record in page.items {
                cursor = Some(record.intent.run_id.clone());
                let tracked_run = record
                    .binding
                    .as_ref()
                    .map_or(record.intent.run_id.as_str(), |binding| {
                        binding.run_id.as_str()
                    });
                if record.state != ActEngineState::CleanupRequired
                    || self
                        .lock()
                        .runs
                        .get(tracked_run)
                        .is_some_and(|slot| slot.record.state != RunState::Done)
                {
                    continue;
                }
                if record.registry_id != owner {
                    return Err("cleanup retry registry owner mismatch".into());
                }
                let budget = crate::act_engine::CLEANUP_BUDGET;
                let result = async_engine::timeout(
                    budget,
                    self.backend.retire(&self.registry, owner, &record, budget),
                )
                .await
                .map_err(|_| "cleanup retry deadline exceeded".to_string())
                .and_then(|r| r);
                let retired = result.is_ok().then(|| record.intent.run_id.clone());
                if retired.is_some() {
                    self.update(tracked_run, |slot| {
                        slot.record.cleanup = Some("removed".into());
                    });
                }
                return Ok(CleanupRetry {
                    next_cursor: cursor,
                    retired,
                    deferred: result.err(),
                });
            }
            match page.next_run_id {
                Some(next)
                    if page_start.as_ref().is_none_or(|old| old < &next)
                        && cursor.as_ref().is_none_or(|old| old <= &next) =>
                {
                    cursor = Some(next)
                }
                Some(_) => return Err("cleanup retry cursor did not advance".into()),
                None => {
                    return Ok(CleanupRetry {
                        next_cursor: None,
                        retired: None,
                        deferred: None,
                    });
                }
            }
        }
        Ok(CleanupRetry {
            next_cursor: cursor,
            retired: None,
            deferred: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_opt_out_never_starts_cache_maintenance() {
        crate::ci::lifecycle::tests::with_registry(|registry, directory| async move {
            std::fs::write(directory.join("retention.toml"), "auto_retention = false\n").unwrap();
            let backend = Arc::new(crate::ci::lifecycle::tests::FakeBackend::default());
            let runtime = CiRuntime::start(&directory, registry, backend.clone(), 1);
            let stop = CancellationSource::new();
            let token = stop.token();
            let task = async_engine::launch(async move {
                runtime
                    .maintain_existing_cohort(crate::ci::lifecycle::tests::OWNER, &token)
                    .await;
            });
            async_engine::sleep(Duration::from_millis(40)).await;
            stop.cancel();
            task.await.unwrap();
            assert_eq!(*backend.cohort_maintenances.lock().unwrap(), 0);
        });
    }
}
