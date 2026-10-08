//! Default CI admission: agreement, leased import, durable publication, routing.
use super::{ActInvocation, DockerActBackend};
use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
    ci::cache_cohort::CacheRoute,
};
use kernal_api::async_engine;
use std::time::Duration;

impl DockerActBackend {
    pub(super) async fn admit_cache_route(
        &self,
        registry: &RegistryActor,
        engine: &str,
        invocation: &ActInvocation,
    ) -> Result<CacheRoute, String> {
        let (namespace, policy) = match &invocation.cache_route {
            CacheRoute::Legacy(namespace) => (namespace, invocation.cache_policy),
            CacheRoute::Cohort { namespace, policy } => (namespace, *policy),
        };
        let observed = self
            .published_cache_route(engine, namespace, policy)
            .await?;
        if matches!(observed, CacheRoute::Cohort { .. }) || !invocation.auto_retention {
            return Ok(observed);
        }
        self.require_coordinated_cache_writers().await?;
        self.agree_cache_policy(engine, policy).await?;
        let deadline = async_engine::Deadline::after(Duration::from_secs(150));
        let mut migration = loop {
            let observed = self
                .published_cache_route(engine, namespace, policy)
                .await?;
            if matches!(observed, CacheRoute::Cohort { .. }) {
                return Ok(observed);
            }
            let opened = async_engine::timeout_at(
                deadline,
                self.open_cache_migration(engine, namespace, policy),
            )
            .await
            .map_err(|_| "cache enrollment admission deadline exceeded")?;
            match opened {
                Ok(migration) => break migration,
                Err(error) if error == super::migration_session::BUSY => {
                    async_engine::timeout_at(
                        deadline,
                        async_engine::sleep(Duration::from_millis(100)),
                    )
                    .await
                    .map_err(|_| "cache enrollment admission deadline exceeded")?;
                }
                Err(error) => return Err(error),
            }
        };
        // Re-read under the continuous exclusive migration lease: another
        // daemon may have published while this invocation prepared its engine.
        let observed = self
            .published_cache_route(engine, namespace, policy)
            .await?;
        if matches!(observed, CacheRoute::Cohort { .. }) {
            return Ok(observed);
        }
        self.require_coordinated_cache_writers().await?;
        let reply = registry
            .act_registry(ActRegistryCommand::CacheMigrationGet {
                namespace: namespace.as_str().into(),
            })
            .await
            .map_err(|error| error.to_string())?;
        match reply {
            ActRegistryReply::CacheMigration(None) => {
                migration
                    .import_recorded(registry)
                    .await?
                    .require_warm_publication()?;
                migration.receipt().await?;
            }
            ActRegistryReply::CacheMigration(Some(_)) => {
                migration.resume_recorded(registry).await?;
            }
            _ => return Err("cache admission requires a typed migration journal reply".into()),
        }
        migration.audit().await?.require_current_inventory()?;
        migration.publish().await?;
        self.published_cache_route(engine, namespace, policy).await
    }
}
