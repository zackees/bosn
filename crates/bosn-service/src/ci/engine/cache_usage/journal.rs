//! Registry snapshots and in-process liveness for accounting helpers.
use super::DockerActBackend;
use crate::{
    RegistryActor,
    act_registry::{ActRegistryCommand, ActRegistryReply},
};

pub(in crate::ci::engine) struct ActiveHelper<'a> {
    backend: &'a DockerActBackend,
    nonce: String,
}
impl<'a> ActiveHelper<'a> {
    pub fn claim(backend: &'a DockerActBackend, nonce: &str) -> Self {
        backend
            .active_helpers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(nonce.into());
        Self {
            backend,
            nonce: nonce.into(),
        }
    }
}
impl Drop for ActiveHelper<'_> {
    fn drop(&mut self) {
        self.backend
            .active_helpers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.nonce);
    }
}

pub(in crate::ci::engine) struct Tracker<'a> {
    registry: &'a RegistryActor,
    nonce: &'a str,
}
impl<'a> Tracker<'a> {
    pub fn new(registry: &'a RegistryActor, nonce: &'a str) -> Self {
        Self { registry, nonce }
    }
    async fn commit(&self, command: ActRegistryCommand) -> Result<(), String> {
        match self
            .registry
            .act_registry(command)
            .await
            .map_err(|error| error.to_string())?
        {
            ActRegistryReply::Committed => Ok(()),
            _ => Err("cache helper registry reply mismatch".into()),
        }
    }
    pub async fn begin(
        &self,
        intent: bosn_registry::cache_helper::CacheHelperIntent,
    ) -> Result<(), String> {
        self.commit(ActRegistryCommand::HelperBegin(intent)).await
    }
    pub async fn register(&self, id: &str) -> Result<(), String> {
        self.commit(ActRegistryCommand::HelperRegister {
            nonce: self.nonce.into(),
            id: id.into(),
            at: crate::ci::lifecycle::now_seconds(),
        })
        .await
    }
    pub async fn finish(&self, id: &str) -> Result<(), String> {
        self.commit(ActRegistryCommand::HelperFinish {
            nonce: self.nonce.into(),
            id: id.into(),
            at: crate::ci::lifecycle::now_seconds(),
        })
        .await
    }
}
