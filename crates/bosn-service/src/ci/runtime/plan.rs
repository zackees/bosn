//! What a run's engine is made from, and the run's plan: the same sizing,
//! pins and cache volume for a run's own engine and for a spare (#410), so a
//! spare is only claimed when it is exactly the engine the run would create.

use bosn_registry::act::{ActEngineBinding, ActEngineCreationProfile, ActEngineIntent};

use super::super::{
    engine::{ACT_VERSION, ActArtifact, ActInvocation, CacheVolume, act_artifact},
    limits::EngineConfig,
    spare::Spares,
};
use super::*;

/// The engine every intent made now would describe.
pub(super) struct EngineSpec {
    pub(super) act: ActArtifact,
    pub(super) cache: CacheVolume,
    profile: ActEngineCreationProfile,
}

impl EngineSpec {
    /// The intent of an engine bound to `binding` (a run, or a spare).
    pub(super) fn intent(&self, binding: ActEngineBinding, spare: bool) -> ActEngineIntent {
        ActEngineIntent {
            run_id: binding.run_id,
            workspace: binding.workspace,
            candidate_sha: binding.candidate_sha,
            payload_sha256: binding.payload_sha256,
            snapshot_sha256: binding.snapshot_sha256,
            act_version: ACT_VERSION.into(),
            act_image_digest: format!("sha256:{}", self.act.sha256),
            engine_image_digest: crate::act_engine::ENGINE_MANIFEST.into(),
            runner_image_digest: super::super::pins::runner_manifest().into(),
            created_at: lifecycle::now_seconds(),
            creation_profile: Some(self.profile.clone()),
            spare,
        }
    }
}

impl CiRuntime {
    /// The engine spec under `config`: the pinned act build for this host,
    /// limits sized from the host engine, and the machine-wide cache volume.
    pub(super) async fn engine_spec(&self, config: EngineConfig) -> Result<EngineSpec, String> {
        let act = act_artifact(std::env::consts::ARCH)
            .ok_or("no pinned act build or engine image for this host architecture")?;
        let registry_id = self
            .registry
            .status()
            .await
            .map_err(|e| format!("registry: {e}"))?
            .registry_id;
        self.backend.ensure_engine_image().await?;
        let limits =
            super::super::limits::size_engine(self.backend.host_resources().await?, config)?;
        let cache = CacheVolume::machine(&registry_id, lifecycle::now_seconds())?;
        let profile = crate::act_engine::creation_profile_with_cache(limits, Some(cache.mount()))
            .map_err(|e| e.to_string())?;
        Ok(EngineSpec {
            act,
            cache,
            profile,
        })
    }

    /// The immutable engine intent and act invocation for a record, with the
    /// spare engine to claim when one is kept for exactly this engine.
    pub(super) async fn plan(
        &self,
        record: &RunRecord,
        deadline: async_engine::Deadline,
        cancellation: &async_engine::CancellationToken,
    ) -> Result<EnginePlan, String> {
        super::super::pins::require_execution_pins(record.execution_pins.as_ref())?;
        let config = super::super::config::load_engine(&self.state_dir)?;
        let spec = self.engine_spec(config).await?;
        let intent = spec.intent(
            ActEngineBinding {
                run_id: record.id.clone(),
                workspace: record.workspace.clone(),
                candidate_sha: record.sha.clone(),
                payload_sha256: record.payload_sha256.clone(),
                snapshot_sha256: record.tree_digest.clone(),
            },
            false,
        );
        let spare = match config.spares {
            Spares::One => {
                let spare = self.spares.take(&intent, deadline, cancellation).await;
                // Replenish in the background while this run executes.
                self.kick();
                spare
            }
            Spares::None => None,
        };
        Ok(EnginePlan {
            act: spec.act,
            intent,
            source: self.store.source(&record.id),
            event: self.store.event(&record.id),
            invocation: ActInvocation {
                event: record.event.clone(),
                workflow: record.workflow.clone(),
                workflow_overlaid: self
                    .store
                    .overlay(&record.id)
                    .join(&record.workflow)
                    .is_file(),
                job: record.job.clone(),
                cache_policy: super::super::config::load(&self.state_dir)?
                    .cache
                    .unwrap_or_default(),
                auto_retention: crate::managed_retention::automatic_retention_enabled(
                    &self.state_dir,
                ),
                cache_route: super::super::cache_cohort::CacheRoute::Legacy(
                    super::super::cache_cohort::Namespace::parse(&record.cache_namespace())?,
                ),
                secrets: self.secrets(record)?,
                params: record.params.clone(),
            },
            cache: spec.cache,
            deadline,
            spare,
        })
    }
}
