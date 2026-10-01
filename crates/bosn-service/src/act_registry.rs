//! Trusted daemon persistence commands; these are not client wire authority.

use crate::{DbCommand, Error, RegistryActor};
use bosn_registry::{
    Registry,
    act::{
        ActEngineIntent, ActEngineObservation, ActEngineRecord, ActEngineRecoveryPage,
        ActEngineRemovalProof, ActRunOutcome,
    },
};
use kernal_api::async_engine;

/// Fixed persistence operations for the trusted engine runtime only.
/// There is deliberately no corresponding protobuf/client operation.
#[derive(Debug)]
pub enum ActRegistryCommand {
    Begin(ActEngineIntent),
    Register {
        run: String,
        observed: ActEngineObservation,
        at: f64,
    },
    Recover {
        run: String,
        observed: ActEngineObservation,
        at: f64,
    },
    Execution {
        run: String,
        outcome: ActRunOutcome,
        at: f64,
    },
    Cleanup {
        run: String,
        outcome: ActRunOutcome,
        at: f64,
    },
    Authorize {
        run: String,
        observed: ActEngineObservation,
    },
    Finalize {
        run: String,
        proof: ActEngineRemovalProof,
        at: f64,
    },
    Pending {
        after_run_id: Option<String>,
        limit: usize,
    },
}
#[derive(Debug)]
pub enum ActRegistryReply {
    Committed,
    Authorized(Box<ActEngineRecord>),
    Recovery(ActEngineRecoveryPage),
}

impl RegistryActor {
    /// Await the durable transaction before making the corresponding engine call.
    /// Observations and absence receipts must come from trusted real probes,
    /// never from a client-supplied serialized command.
    pub async fn act_registry(
        &self,
        command: ActRegistryCommand,
    ) -> Result<ActRegistryReply, Error> {
        let (reply, wait) = async_engine::oneshot_channel();
        self.sender
            .send(DbCommand::ActRegistry {
                command: Box::new(command),
                reply,
            })
            .await
            .map_err(|_| Error::ActorClosed)?;
        wait.await.map_err(|_| Error::ActorClosed)?
    }
}

pub(crate) fn apply(
    registry: &mut Registry,
    command: ActRegistryCommand,
) -> Result<ActRegistryReply, bosn_registry::Error> {
    if let ActRegistryCommand::Pending {
        after_run_id,
        limit,
    } = command
    {
        return registry
            .pending_act_engines(after_run_id.as_deref(), limit)
            .map(ActRegistryReply::Recovery);
    }
    let mut transaction = registry.begin_immediate()?;
    let reply = match command {
        ActRegistryCommand::Begin(intent) => {
            transaction.begin_act_engine(&intent)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Register { run, observed, at } => {
            transaction.register_act_engine(&run, &observed, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Recover { run, observed, at } => {
            transaction.recover_act_engine(&run, &observed, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Execution { run, outcome, at } => {
            transaction.record_act_execution(&run, outcome, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Cleanup { run, outcome, at } => {
            transaction.request_act_cleanup(&run, outcome, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Authorize { run, observed } => ActRegistryReply::Authorized(Box::new(
            transaction.authorize_act_cleanup(&run, &observed)?,
        )),
        ActRegistryCommand::Finalize { run, proof, at } => {
            transaction.finalize_act_cleanup(&run, &proof, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Pending { .. } => unreachable!("read handled before transaction"),
    };
    transaction.commit()?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bosn_registry::{Registry, act::ActEngineIntent};
    use kernal_api::platform::fs::TemporaryDirectory;

    #[test]
    fn daemon_intent_reply_follows_commit_and_duplicate_rolls_back() {
        let dir = TemporaryDirectory::new().unwrap();
        let path = dir.path().join("registry.sqlite3");
        let registry =
            Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();
        let intent = ActEngineIntent {
            run_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            workspace: "/private/source".into(),
            candidate_sha: "a".repeat(40),
            payload_sha256: "b".repeat(64),
            snapshot_sha256: "c".repeat(64),
            act_version: "0.2.88".into(),
            act_image_digest: format!("sha256:{}", "d".repeat(64)),
            engine_image_digest: format!("sha256:{}", "e".repeat(64)),
            runner_image_digest: format!("sha256:{}", "f".repeat(64)),
            created_at: 1.0,
        };
        let runtime = kernal_api::async_engine::RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.run(async {
            let (sender, receiver) = async_engine::channel(16);
            let actor = RegistryActor { sender };
            let task = async_engine::launch(crate::registry_actor(registry, receiver, None));
            assert!(matches!(
                actor
                    .act_registry(ActRegistryCommand::Begin(intent.clone()))
                    .await
                    .unwrap(),
                ActRegistryReply::Committed
            ));
            let mut changed = intent.clone();
            changed.candidate_sha = "0".repeat(40);
            assert!(
                actor
                    .act_registry(ActRegistryCommand::Begin(changed))
                    .await
                    .is_err()
            );
            let page = actor
                .act_registry(ActRegistryCommand::Pending {
                    after_run_id: None,
                    limit: 1,
                })
                .await
                .unwrap();
            let ActRegistryReply::Recovery(page) = page else {
                panic!("expected recovery page")
            };
            assert_eq!(page.items[0].intent, intent);
            actor.stop().await;
            task.await.unwrap();
        });
        let registry = Registry::open_writer(&path).unwrap();
        assert_eq!(
            registry.act_engine(&intent.run_id).unwrap().unwrap().intent,
            intent
        );
    }
}
