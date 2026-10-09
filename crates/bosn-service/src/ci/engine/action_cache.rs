//! Pressure eviction of disposable action checkouts, with original lease custody.
use super::action_cache_records::{Observation, Retirement, require_no_submounts};
use super::{DockerActBackend, ENGINE_CACHE, owned, process_control::ProcessControl};
use crate::ci::cache_policy::CachePolicy;
use std::time::Duration;

const RECIPE: &str = include_str!("action_cache_session.sh");

pub use bosn_registry::cache_maintenance::ActionMaintenanceStats;

#[derive(Clone, Copy)]
enum DisposableClass {
    Actions,
    Images,
}
impl DisposableClass {
    fn name(self) -> &'static str {
        match self {
            Self::Actions => "actions",
            Self::Images => "images",
        }
    }
    fn lease(self) -> (&'static str, &'static str) {
        match self {
            Self::Actions => (
                "/bosn/cache/actcache",
                "/bosn/cache/actcache/.legacy-migration.lock",
            ),
            Self::Images => ("/bosn/cache", "/bosn/cache/.artifact-cache.lock"),
        }
    }
}

impl DockerActBackend {
    pub(super) async fn maintain_actions(
        &self,
        engine: &str,
        policy: CachePolicy,
    ) -> Result<ActionMaintenanceStats, String> {
        self.maintain_disposable_class(engine, policy, DisposableClass::Actions)
            .await
    }
    pub(super) async fn maintain_image_archives(
        &self,
        engine: &str,
        policy: CachePolicy,
    ) -> Result<ActionMaintenanceStats, String> {
        self.maintain_disposable_class(engine, policy, DisposableClass::Images)
            .await
    }
    async fn maintain_disposable_class(
        &self,
        engine: &str,
        policy: CachePolicy,
        class: DisposableClass,
    ) -> Result<ActionMaintenanceStats, String> {
        self.verify_measured_volume(super::CACHE_VOLUME).await?;
        let mut session = Session::open_class(self, engine, class).await?;
        let result = async {
            // Recheck all attachments while the original exclusive lease is held.
            match class {
                DisposableClass::Actions => self.require_coordinated_cache_readers().await?,
                DisposableClass::Images => self.require_coordinated_artifact_readers().await?,
            }
            session
                .maintain(u64::try_from(policy.repository_max_bytes).map_err(|e| e.to_string())?)
                .await
        }
        .await;
        let _ = session.io.send(b"abort\n").await;
        result.map_err(|error| format!("{} cache: {error}", class.name()))
    }
}

struct Session {
    class: DisposableClass,
    io: ProcessControl,
}
impl Session {
    async fn open_class(
        backend: &DockerActBackend,
        engine: &str,
        class: DisposableClass,
    ) -> Result<Self, String> {
        let mut args = owned(&["exec", "-i", engine, "timeout", "180", "sh", "-c"]);
        let (directory, lease) = class.lease();
        args.push(
            RECIPE
                .replace("@CACHE@", ENGINE_CACHE)
                .replace("@CLASS@", class.name())
                .replace("@LEASE_DIR@", directory)
                .replace("@LEASE@", lease),
        );
        let process = backend
            .docker
            .with_args(args)
            .spawn_interactive(Duration::from_secs(15))
            .await
            .map_err(|e| e.to_string())?;
        let mut session = Self {
            class,
            io: ProcessControl::new(
                process,
                "cache retention",
                b"bosn-actions-end:",
                Duration::from_secs(110),
            ),
        };
        if session.io.line().await? != b"bosn-actions-ready" {
            return Err("cache held by a live reader or unavailable original lease".into());
        }
        Ok(session)
    }

    async fn raw(&mut self, operation: &str, argument: Option<&str>) -> Result<Vec<u8>, String> {
        let command = match argument {
            Some(value) if !value.contains(['\n', '\r']) => format!("{operation}\n{value}\n"),
            Some(_) => return Err("cache command has invalid framing".into()),
            None => format!("{operation}\n"),
        };
        let (code, bytes) = self.io.command(command.as_bytes()).await?;
        if code != 0 {
            return Err(format!(
                "cache {operation} failed ({code}): {}",
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(256)
                    .collect::<String>()
            ));
        }
        Ok(bytes)
    }

    async fn mounts(&mut self, proof: Option<&Retirement>) -> Result<(), String> {
        let table = self.raw("mounts", None).await?;
        // The shell frame contributes one blank line after command output.
        let table = table.strip_suffix(b"\n").unwrap_or(&table);
        let stage = proof.map(|proof| proof.stage(self.class.name()));
        require_no_submounts(
            table,
            &format!("{ENGINE_CACHE}/{}", self.class.name()),
            stage.as_deref(),
        )
    }

    async fn maintain(&mut self, budget: u64) -> Result<ActionMaintenanceStats, String> {
        self.mounts(None).await?;
        let initial = Observation::parse(&self.raw("observe", None).await?)?;
        let mut before = initial.source.map_or(0, |(_, bytes)| bytes);
        let mut retired = 0;
        let ledger = self.raw("ledger", None).await?;
        if ledger != b"absent\n" && ledger != b"absent\n\n" {
            let proof: Retirement = serde_json::from_slice(&ledger).map_err(|e| e.to_string())?;
            proof.validate(initial.cache)?;
            let (bytes, removed) = self.retire(&proof).await?;
            if initial
                .source
                .is_none_or(|(identity, _)| identity != proof.source)
            {
                before = before
                    .checked_add(bytes)
                    .ok_or("cache recovery allocation overflows")?;
            }
            retired += u32::from(removed);
        }
        let current = Observation::parse(&self.raw("observe", None).await?)?;
        if current.cache != initial.cache {
            return Err("cache changed during maintenance".into());
        }
        if let Some((source, bytes)) = current.source
            && bytes > budget
        {
            let proof = Retirement {
                schema_version: 1,
                nonce: crate::ci::new_uuid().await.map_err(|e| e.to_string())?,
                cache: current.cache,
                source,
            };
            proof.validate(current.cache)?;
            self.mounts(Some(&proof)).await?;
            self.raw("prepare", Some(&proof.argument())).await?;
            self.raw(
                "record",
                Some(&serde_json::to_string(&proof).map_err(|e| e.to_string())?),
            )
            .await?;
            retired += u32::from(self.retire(&proof).await?.1);
        }
        let final_state = Observation::parse(&self.raw("observe", None).await?)?;
        if final_state.cache != initial.cache {
            return Err("cache changed before acknowledgement".into());
        }
        let after = final_state.source.map_or(0, |(_, bytes)| bytes);
        if after > budget {
            return Err("cache class remains over budget".into());
        }
        Ok(ActionMaintenanceStats {
            allocated_before: before,
            allocated_after: after,
            budget_bytes: budget,
            retired_classes: retired,
        })
    }

    async fn retire(&mut self, proof: &Retirement) -> Result<(u64, bool), String> {
        self.mounts(Some(proof)).await?;
        let stage = Observation::parse(&self.raw("stage", Some(&proof.argument())).await?)?;
        proof.validate(stage.cache)?;
        let bytes = if let Some((identity, bytes)) = stage.source {
            if identity != proof.source {
                return Err("cache retirement stage changed inode".into());
            }
            self.mounts(Some(proof)).await?;
            self.raw("remove", Some(&proof.argument())).await?;
            bytes
        } else {
            0
        };
        self.raw("acknowledge", Some(&proof.argument())).await?;
        Ok((bytes, stage.source.is_some()))
    }
}

#[cfg(test)]
#[path = "action_cache_live_tests.rs"]
mod live_tests;
