//! Typed async handles onto the registry actor.

use super::*;

impl RegistryActor {
    pub(crate) async fn put_manifest_volume_intents(
        &self,
        volumes: Vec<ManifestVolumeResource>,
    ) -> Result<(), Error> {
        if volumes.is_empty() {
            return Ok(());
        }
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::PutManifestVolumeIntents { volumes, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn begin_setup_app_task_session(
        &self,
        job_id: u64,
        managed_container_identity: String,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::BeginSetupAppTaskSession {
                job_id,
                container_id: managed_container_identity,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn finish_setup_app_task_session(
        &self,
        job_id: u64,
        outcome: &'static str,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::FinishSetupAppTaskSession {
                job_id,
                outcome,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn begin_manifest_app_task_session(
        &self,
        job_id: u64,
        managed_container_identity: String,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::BeginManifestAppTaskSession {
                job_id,
                container_id: managed_container_identity,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn finish_manifest_app_task_session(
        &self,
        job_id: u64,
        outcome: &'static str,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::FinishManifestAppTaskSession {
                job_id,
                outcome,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn status(&self) -> Result<Status, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::Status(reply))
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn doctor_integrity(&self) -> &'static str {
        let (reply, wait) = async_engine::oneshot_channel();
        if self
            .sender
            .send(DbCommand::DoctorIntegrity(reply))
            .await
            .is_err()
        {
            return "unavailable";
        }
        match async_engine::timeout(DOCTOR_REGISTRY_DEADLINE, wait).await {
            Ok(Ok(state)) => state,
            Ok(Err(_)) => "unavailable",
            Err(_) => "deadline",
        }
    }
    pub(crate) async fn resources(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<RegistryResourcePage, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::Resources {
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn setup_ensure_events(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<SetupEnsureEventPage, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupEnsureEvents {
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn setup_gc_preview(
        &self,
        workspace: String,
        after: u64,
        limit: u32,
    ) -> Result<SetupGcPreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupGcPreview {
                workspace,
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn manifest_volume_gc_preview(
        &self,
        workspace: String,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ManifestVolumeGcPreview {
                workspace,
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn manifest_volume_release_preview(
        &self,
        workspace: String,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ManifestVolumeReleasePreview {
                workspace,
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn setup_reconcile_preview(
        &self,
        workspace: String,
        after: u64,
        limit: u32,
    ) -> Result<SetupReconcileCandidates, Error> {
        validate_registry_page(after, limit)?;
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupReconcilePreview {
                workspace,
                after,
                limit,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn repair_missing_setup_container(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<Option<bosn_registry::SetupMissingRepair>, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::RepairMissingSetupContainer {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn setup_missing_repair_candidate(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupMissingRepairCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn setup_gc_candidate(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<Option<bosn_registry::SetupGcCandidate>, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::SetupGcCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn manifest_volume_gc_candidate(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<Option<bosn_registry::ManifestVolumeGcCandidate>, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ManifestVolumeGcCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn manifest_volume_release_candidate(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<Option<bosn_registry::ManifestVolumeGcCandidate>, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ManifestVolumeReleaseCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn finalize_manifest_volume_gc(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
        missing: bool,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::FinalizeManifestVolumeGc {
                workspace,
                id,
                name,
                generation,
                missing,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn finalize_manifest_volume_release(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
        missing: bool,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::FinalizeManifestVolumeRelease {
                workspace,
                id,
                name,
                generation,
                missing,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn finalize_setup_gc(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
        missing: bool,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::FinalizeSetupGc {
                workspace,
                id,
                name,
                generation,
                missing,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn confirm_setup_retired_stopped(
        &self,
        workspace: String,
        id: String,
        name: String,
        generation: String,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ConfirmSetupRetiredStopped {
                workspace,
                id,
                name,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn complete_setup_workspace(
        &self,
        workspace: String,
    ) -> Result<SetupDoneResult, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::CompleteSetupWorkspace { workspace, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn append_setup_ensure_events(
        &self,
        events: Vec<SetupEnsureEvent>,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::AppendSetupEnsureEvents { events, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn append_manifest_recovery_events(
        &self,
        events: Vec<(String, String)>,
    ) -> Result<(), Error> {
        if events.is_empty() {
            return Ok(());
        }
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::AppendManifestRecoveryEvents { events, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn record_setup_ensure(
        &self,
        job_id: u64,
        execution: SetupEnsureExecution,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::RecordSetupEnsure {
                job_id,
                execution: Box::new(execution),
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn record_manifest_ensure(
        &self,
        job_id: u64,
        execution: SetupEnsureExecution,
        contract: ManifestRecoveryContract,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::RecordManifestEnsure {
                job_id,
                execution: Box::new(execution),
                contract,
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn manifest_recovery_contracts(&self) -> Result<Vec<String>, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ManifestRecoveryContracts { reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn manifest_autostart_intent_disabled(
        &self,
        contract: &ManifestRecoveryContract,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ManifestAutostartIntentDisabled {
                detail: manifest_autostart_intent_detail(contract),
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn manifest_recovery_authorized(
        &self,
        contract: ManifestRecoveryContract,
    ) -> Result<bool, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ManifestRecoveryAuthorized { contract, reply })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn record_setup_adoption(
        &self,
        execution: SetupEnsureExecution,
    ) -> Result<(), Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::RecordSetupAdoption {
                execution: Box::new(execution),
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
    pub(crate) async fn stop(&self) {
        let (reply, wait) = async_engine::oneshot_channel();
        if self.sender.send(DbCommand::Stop(reply)).await.is_ok() {
            let _ = wait.await;
        }
    }
}

#[cfg(test)]
pub(crate) struct SetupEnsureRecordGate {
    pub(crate) entered: async_engine::Sender<()>,
    pub(crate) release: async_engine::Receiver<()>,
}
#[derive(Clone)]
pub struct RegistryActor {
    pub(crate) sender: async_engine::Sender<DbCommand>,
}
pub(crate) enum DbCommand {
    ActRegistry {
        command: Box<act_registry::ActRegistryCommand>,
        reply: async_engine::OneshotSender<Result<act_registry::ActRegistryReply, Error>>,
    },
    Status(async_engine::OneshotSender<Result<Status, Error>>),
    DoctorIntegrity(async_engine::OneshotSender<&'static str>),
    Resources {
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<RegistryResourcePage, Error>>,
    },
    SetupEnsureEvents {
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<SetupEnsureEventPage, Error>>,
    },
    SetupGcPreview {
        workspace: String,
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<SetupGcPreviewPage, Error>>,
    },
    ManifestVolumeGcPreview {
        workspace: String,
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<ManifestVolumeGcPreviewPage, Error>>,
    },
    ManifestVolumeReleasePreview {
        workspace: String,
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<ManifestVolumeGcPreviewPage, Error>>,
    },
    SetupReconcilePreview {
        workspace: String,
        after: u64,
        limit: u32,
        reply: async_engine::OneshotSender<Result<SetupReconcileCandidates, Error>>,
    },
    SetupMissingRepairCandidate {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    RepairMissingSetupContainer {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply:
            async_engine::OneshotSender<Result<Option<bosn_registry::SetupMissingRepair>, Error>>,
    },
    SetupGcCandidate {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply: async_engine::OneshotSender<Result<Option<bosn_registry::SetupGcCandidate>, Error>>,
    },
    FinalizeSetupGc {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        missing: bool,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    ManifestVolumeGcCandidate {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply: async_engine::OneshotSender<
            Result<Option<bosn_registry::ManifestVolumeGcCandidate>, Error>,
        >,
    },
    ManifestVolumeReleaseCandidate {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply: async_engine::OneshotSender<
            Result<Option<bosn_registry::ManifestVolumeGcCandidate>, Error>,
        >,
    },
    FinalizeManifestVolumeGc {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        missing: bool,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    FinalizeManifestVolumeRelease {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        missing: bool,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    ConfirmSetupRetiredStopped {
        workspace: String,
        id: String,
        name: String,
        generation: String,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    CompleteSetupWorkspace {
        workspace: String,
        reply: async_engine::OneshotSender<Result<SetupDoneResult, Error>>,
    },
    AppendSetupEnsureEvents {
        events: Vec<SetupEnsureEvent>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    AppendManifestRecoveryEvents {
        events: Vec<(String, String)>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    RecordSetupEnsure {
        job_id: u64,
        execution: Box<SetupEnsureExecution>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    RecordManifestEnsure {
        job_id: u64,
        execution: Box<SetupEnsureExecution>,
        contract: ManifestRecoveryContract,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    ManifestRecoveryContracts {
        reply: async_engine::OneshotSender<Result<Vec<String>, Error>>,
    },
    ManifestAutostartIntentDisabled {
        detail: String,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    ManifestRecoveryAuthorized {
        contract: ManifestRecoveryContract,
        reply: async_engine::OneshotSender<Result<bool, Error>>,
    },
    PutManifestVolumeIntents {
        volumes: Vec<ManifestVolumeResource>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    RecordSetupAdoption {
        execution: Box<SetupEnsureExecution>,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    BeginSetupAppTaskSession {
        job_id: u64,
        container_id: String,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    BeginManifestAppTaskSession {
        job_id: u64,
        container_id: String,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    FinishSetupAppTaskSession {
        job_id: u64,
        outcome: &'static str,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    FinishManifestAppTaskSession {
        job_id: u64,
        outcome: &'static str,
        reply: async_engine::OneshotSender<Result<(), Error>>,
    },
    Stop(async_engine::OneshotSender<()>),
}
