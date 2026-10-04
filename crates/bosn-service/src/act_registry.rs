//! Trusted daemon persistence commands; these are not client wire authority.

use crate::{DbCommand, Error, RegistryActor};
use bosn_registry::{
    Registry,
    act::{
        ActEngineBinding, ActEngineIntent, ActEngineObservation, ActEngineRecord,
        ActEngineRecoveryPage, ActEngineRemovalProof, ActRunOutcome,
    },
};
use kernal_api::async_engine;

/// Fixed persistence operations for the trusted engine runtime only.
/// There is deliberately no corresponding protobuf/client operation.
#[derive(Debug)]
pub enum ActRegistryCommand {
    /// Only before this writer actor admits any new creation/execution.
    StartupInterrupt {
        run: String,
        at: f64,
    },
    /// Irreversibly withdraw startup recovery authority for this actor.
    SealStartup,
    Begin(ActEngineIntent),
    ToolRecoveryBegin {
        run: String,
        token: String,
        created: i64,
        expires: i64,
    },
    ToolRecoveryReserved {
        run: String,
        token: String,
        intent: bosn_registry::act::ActToolRecoveryIntent,
        at: f64,
    },
    /// Trusted runtime receipt that the engine holding the last source reader
    /// is absent and its private source volume is still retained and ours.
    ToolRecoverySourceStopped {
        run: String,
        proof: bosn_registry::act::ActToolSourceStopProof,
        at: f64,
    },
    Register {
        run: String,
        observed: ActEngineObservation,
        at: f64,
    },
    Verify {
        run: String,
        observed: ActEngineObservation,
    },
    Claim {
        intent: ActEngineIntent,
        observed: ActEngineObservation,
        token: String,
        at: f64,
    },
    /// Hand a prepared spare (#410) from the daemon's claim `from` to one
    /// run's claim `token`, recording the run it now serves.
    ClaimSpare {
        spare: String,
        observed: ActEngineObservation,
        from: String,
        token: String,
        binding: ActEngineBinding,
        at: f64,
    },
    VerifyClaimed {
        run: String,
        observed: ActEngineObservation,
        token: String,
    },
    CleanupClaimed {
        run: String,
        token: String,
        outcome: ActRunOutcome,
        at: f64,
    },
    Recover {
        run: String,
        observed: ActEngineObservation,
        at: f64,
    },
    Execution {
        run: String,
        token: String,
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
    MaintenanceRecord(bosn_registry::cache_maintenance::MaintenanceSnapshot),
    MaintenanceLatest,
    HelperBegin(bosn_registry::cache_helper::CacheHelperIntent),
    HelperRegister {
        nonce: String,
        id: String,
        at: f64,
    },
    HelperFinish {
        nonce: String,
        id: String,
        at: f64,
    },
    HelperPending {
        after_nonce: Option<String>,
        limit: usize,
    },
    /// Latest state snapshot for one run (read-only).
    Get {
        run: String,
    },
}
#[derive(Debug)]
pub enum ActRegistryReply {
    Committed,
    Authorized(Box<ActEngineRecord>),
    Verified(Box<ActEngineRecord>),
    Claimed(Box<ActEngineRecord>),
    Recovery(ActEngineRecoveryPage),
    Helpers(bosn_registry::cache_helper::CacheHelperPage),
    Record(Option<Box<ActEngineRecord>>),
    Maintenance(Option<bosn_registry::cache_maintenance::MaintenanceSnapshot>),
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

#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
pub(crate) fn apply(
    registry: &mut Registry,
    command: ActRegistryCommand,
    startup_open: &mut bool,
) -> Result<ActRegistryReply, bosn_registry::Error> {
    // Admission closes the window even if its transaction is later refused or
    // its caller loses the reply. Actor queue order is the authority boundary.
    if matches!(
        &command,
        ActRegistryCommand::Begin(_)
            | ActRegistryCommand::ToolRecoveryBegin { .. }
            | ActRegistryCommand::ToolRecoveryReserved { .. }
            | ActRegistryCommand::HelperBegin(_)
            | ActRegistryCommand::Claim { .. }
            | ActRegistryCommand::ClaimSpare { .. }
            | ActRegistryCommand::SealStartup
    ) {
        *startup_open = false;
    }
    if matches!(&command, ActRegistryCommand::SealStartup) {
        return Ok(ActRegistryReply::Committed);
    }
    if let ActRegistryCommand::StartupInterrupt { run, at } = &command {
        if !*startup_open {
            return Err(bosn_registry::Error::BadRow(
                "act startup recovery window sealed",
            ));
        }
        let record = registry
            .act_engine(run)?
            .ok_or(bosn_registry::Error::BadRow("act startup intent missing"))?;
        if !at.is_finite() || *at < record.updated_at {
            return Err(bosn_registry::Error::BadRow("act startup time"));
        }
        if record.state == bosn_registry::act::ActEngineState::Terminal {
            return Err(bosn_registry::Error::BadRow("act startup terminal intent"));
        }
        if record.state == bosn_registry::act::ActEngineState::CleanupRequired {
            return Ok(ActRegistryReply::Committed);
        }
        let mut transaction = registry.begin_immediate()?;
        if let Some(token) = record.execution_claim {
            transaction.request_act_execution_cleanup(
                run,
                &token,
                ActRunOutcome::Interrupted,
                *at,
            )?;
        } else {
            transaction.request_act_cleanup(run, ActRunOutcome::Interrupted, *at)?;
        }
        transaction.commit()?;
        return Ok(ActRegistryReply::Committed);
    }
    if let ActRegistryCommand::Pending {
        after_run_id,
        limit,
    } = command
    {
        return registry
            .pending_act_engines(after_run_id.as_deref(), limit)
            .map(ActRegistryReply::Recovery);
    }
    if let ActRegistryCommand::HelperPending { after_nonce, limit } = command {
        return registry
            .pending_cache_helpers(after_nonce.as_deref(), limit)
            .map(ActRegistryReply::Helpers);
    }
    if let ActRegistryCommand::Get { run } = command {
        return registry
            .act_engine(&run)
            .map(|record| ActRegistryReply::Record(record.map(Box::new)));
    }
    if matches!(command, ActRegistryCommand::MaintenanceLatest) {
        return registry
            .latest_cache_maintenance()
            .map(ActRegistryReply::Maintenance);
    }
    let mut transaction = registry.begin_immediate()?;
    let reply = match command {
        ActRegistryCommand::MaintenanceRecord(snapshot) => {
            transaction.record_cache_maintenance(&snapshot)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Begin(intent) => {
            transaction.begin_act_engine(&intent)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::ToolRecoveryBegin {
            run,
            token,
            created,
            expires,
        } => {
            transaction.begin_act_tool_recovery(&run, &token, created, expires)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::ToolRecoveryReserved {
            run,
            token,
            intent,
            at,
        } => {
            transaction.acknowledge_act_tool_recovery(&run, &token, &intent, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::ToolRecoverySourceStopped { run, proof, at } => {
            transaction.record_act_tool_source_stopped(&run, &proof, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Register { run, observed, at } => {
            transaction.register_act_engine(&run, &observed, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Verify { run, observed } => {
            let record = transaction.verify_act_engine(&run, &observed)?;
            if record.state != bosn_registry::act::ActEngineState::Registered
                || record.execution_claim.is_some()
            {
                return Err(bosn_registry::Error::BadRow(
                    "Act execution requires registered ownership",
                ));
            }
            ActRegistryReply::Verified(Box::new(record))
        }
        ActRegistryCommand::Claim {
            intent,
            observed,
            token,
            at,
        } => ActRegistryReply::Claimed(Box::new(
            transaction.claim_act_execution(&intent, &observed, &token, at)?,
        )),
        ActRegistryCommand::ClaimSpare {
            spare,
            observed,
            from,
            token,
            binding,
            at,
        } => ActRegistryReply::Claimed(Box::new(
            transaction.claim_act_spare(&spare, &observed, &from, &token, &binding, at)?,
        )),
        ActRegistryCommand::VerifyClaimed {
            run,
            observed,
            token,
        } => ActRegistryReply::Verified(Box::new(
            transaction.verify_act_execution_owner(&run, &observed, &token)?,
        )),
        ActRegistryCommand::CleanupClaimed {
            run,
            token,
            outcome,
            at,
        } => {
            transaction.request_act_execution_cleanup(&run, &token, outcome, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Recover { run, observed, at } => {
            transaction.recover_act_engine(&run, &observed, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Execution {
            run,
            token,
            outcome,
            at,
        } => {
            transaction.record_act_execution(&run, &token, outcome, at)?;
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
        ActRegistryCommand::HelperBegin(intent) => {
            transaction.begin_cache_helper(&intent)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::HelperRegister { nonce, id, at } => {
            transaction.register_cache_helper(&nonce, &id, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::HelperFinish { nonce, id, at } => {
            transaction.finish_cache_helper(&nonce, &id, at)?;
            ActRegistryReply::Committed
        }
        ActRegistryCommand::Pending { .. }
        | ActRegistryCommand::MaintenanceLatest
        | ActRegistryCommand::Get { .. }
        | ActRegistryCommand::HelperPending { .. }
        | ActRegistryCommand::StartupInterrupt { .. }
        | ActRegistryCommand::SealStartup => {
            unreachable!("startup/read handled before transaction")
        }
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
    #[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
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
            spare: false,
            creation_profile: Some(bosn_registry::act::ActEngineCreationProfile {
                memory_bytes: 28 << 30,
                storage_bytes: 20 << 30,
                nano_cpus: 2_000_000_000,
                pids: 1024,
                run_tmpfs_bytes: 16 << 20,
                tmp_tmpfs_bytes: 64 << 20,
                tmpfs_policy: bosn_registry::act::ActEngineTmpfsPolicy::StorageExecRunTmpNoexecV1,
                init_command_sha256: "a".repeat(64),
                cache_volume: None,
                cache_coordination: None,
                tool_generation: None,
            }),
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
            let observed = ActEngineObservation {
                name: intent.engine_name(),
                engine_id: "1".repeat(64),
                image_digest: intent.engine_image_digest.clone(),
                labels: intent
                    .required_labels("11111111-2222-4333-8444-555555555555")
                    .unwrap(),
            };
            assert!(
                actor
                    .act_registry(ActRegistryCommand::Verify {
                        run: intent.run_id.clone(),
                        observed: observed.clone()
                    })
                    .await
                    .is_err(),
                "pending creation cannot authorize execution"
            );
            actor
                .act_registry(ActRegistryCommand::Register {
                    run: intent.run_id.clone(),
                    observed: observed.clone(),
                    at: 2.0,
                })
                .await
                .unwrap();
            assert!(matches!(
                actor
                    .act_registry(ActRegistryCommand::Verify {
                        run: intent.run_id.clone(),
                        observed: observed.clone()
                    })
                    .await
                    .unwrap(),
                ActRegistryReply::Verified(_)
            ));
            let mut wrong = observed.clone();
            wrong.engine_id = "2".repeat(64);
            assert!(
                actor
                    .act_registry(ActRegistryCommand::Verify {
                        run: intent.run_id.clone(),
                        observed: wrong
                    })
                    .await
                    .is_err()
            );
            actor
                .act_registry(ActRegistryCommand::Cleanup {
                    run: intent.run_id.clone(),
                    outcome: ActRunOutcome::Failed,
                    at: 3.0,
                })
                .await
                .unwrap();
            assert!(
                actor
                    .act_registry(ActRegistryCommand::Verify {
                        run: intent.run_id.clone(),
                        observed
                    })
                    .await
                    .is_err(),
                "cleanup ownership never authorizes execution"
            );
            actor.stop().await;
            task.await.unwrap();
        });
        let registry = Registry::open_writer(&path).unwrap();
        assert_eq!(
            registry.act_engine(&intent.run_id).unwrap().unwrap().intent,
            intent
        );
    }
    fn startup_fixture() -> ActEngineIntent {
        ActEngineIntent {
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
            spare: false,
            creation_profile: Some(bosn_registry::act::ActEngineCreationProfile {
                memory_bytes: 28 << 30,
                storage_bytes: 20 << 30,
                nano_cpus: 2_000_000_000,
                pids: 1024,
                run_tmpfs_bytes: 16 << 20,
                tmp_tmpfs_bytes: 64 << 20,
                tmpfs_policy: bosn_registry::act::ActEngineTmpfsPolicy::StorageExecRunTmpNoexecV1,
                init_command_sha256: "a".repeat(64),
                cache_volume: None,
                cache_coordination: None,
                tool_generation: None,
            }),
        }
    }
    const STARTUP_OWNER: &str = "11111111-2222-4333-8444-555555555555";
    const STALE_CLAIM: &str = "12345678-1234-4234-8234-123456789abc";
    fn seed_claim(registry: &mut Registry, intent: &ActEngineIntent) -> ActEngineObservation {
        let observed = ActEngineObservation {
            name: intent.engine_name(),
            engine_id: "1".repeat(64),
            image_digest: intent.engine_image_digest.clone(),
            labels: intent.required_labels(STARTUP_OWNER).unwrap(),
        };
        let mut tx = registry.begin_immediate().unwrap();
        tx.begin_act_engine(intent).unwrap();
        tx.register_act_engine(&intent.run_id, &observed, 2.0)
            .unwrap();
        tx.claim_act_execution(intent, &observed, STALE_CLAIM, 3.0)
            .unwrap();
        tx.commit().unwrap();
        observed
    }
    async fn pending(actor: &RegistryActor) -> Vec<ActEngineRecord> {
        let ActRegistryReply::Recovery(page) = actor
            .act_registry(ActRegistryCommand::Pending {
                after_run_id: None,
                limit: 16,
            })
            .await
            .unwrap()
        else {
            panic!("recovery page")
        };
        page.items
    }
    #[test]
    fn startup_writer_fence_recovers_stale_v2_claim_after_reopen() {
        let dir = TemporaryDirectory::new().unwrap();
        let path = dir.path().join("startup.sqlite3");
        let intent = startup_fixture();
        let mut writer = Registry::create_writer(&path, STARTUP_OWNER).unwrap();
        seed_claim(&mut writer, &intent);
        let mut old =
            serde_json::to_value(writer.act_engine(&intent.run_id).unwrap().unwrap()).unwrap();
        old["schema_version"] = serde_json::json!(2);
        old["intent"]
            .as_object_mut()
            .unwrap()
            .remove("creation_profile");
        let mut tx = writer.begin_immediate().unwrap();
        tx.append_event(
            3.0,
            &format!("act.engine.v1:{}", intent.run_id),
            &serde_json::to_string(&old).unwrap(),
        )
        .unwrap();
        tx.commit().unwrap();
        drop(writer);
        let registry = Registry::open_writer(&path).unwrap();
        assert!(
            Registry::open_writer(&path).is_err(),
            "only one recovery writer"
        );
        async_engine::RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let (sender, receiver) = async_engine::channel(16);
                let actor = RegistryActor { sender };
                let task = async_engine::launch(crate::registry_actor(registry, receiver, None));
                for _ in 0..2 {
                    assert_eq!(
                        pending(&actor).await[0].execution_claim.as_deref(),
                        Some(STALE_CLAIM)
                    );
                }
                actor
                    .act_registry(ActRegistryCommand::StartupInterrupt {
                        run: intent.run_id.clone(),
                        at: 4.0,
                    })
                    .await
                    .unwrap();
                let current = pending(&actor).await.remove(0);
                assert_eq!(
                    current.state,
                    bosn_registry::act::ActEngineState::CleanupRequired
                );
                assert_eq!(current.outcome, Some(ActRunOutcome::Interrupted));
                assert_eq!(current.execution, None);
                assert_eq!(current.execution_claim.as_deref(), Some(STALE_CLAIM));
                actor
                    .act_registry(ActRegistryCommand::SealStartup)
                    .await
                    .unwrap();
                assert!(
                    actor
                        .act_registry(ActRegistryCommand::StartupInterrupt {
                            run: intent.run_id.clone(),
                            at: 5.0
                        })
                        .await
                        .is_err()
                );
                assert_eq!(pending(&actor).await.remove(0), current);
                actor.stop().await;
                task.await.unwrap();
            });
        let reopened = Registry::open_writer(&path).unwrap();
        assert_eq!(
            reopened
                .act_engine(&intent.run_id)
                .unwrap()
                .unwrap()
                .outcome,
            Some(ActRunOutcome::Interrupted)
        );
    }
    #[test]
    fn admission_or_seal_irreversibly_closes_startup_and_preserves_live_claim() {
        for close in ["seal", "begin", "claim", "recovery"] {
            let dir = TemporaryDirectory::new().unwrap();
            let path = dir.path().join("fenced.sqlite3");
            let intent = startup_fixture();
            let mut registry = Registry::create_writer(&path, STARTUP_OWNER).unwrap();
            let observed = seed_claim(&mut registry, &intent);
            async_engine::RuntimeBuilder::multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .run(async {
                    let (sender, receiver) = async_engine::channel(16);
                    let actor = RegistryActor { sender };
                    let task =
                        async_engine::launch(crate::registry_actor(registry, receiver, None));
                    let before = pending(&actor).await.remove(0);
                    match close {
                        "seal" => {
                            actor
                                .act_registry(ActRegistryCommand::SealStartup)
                                .await
                                .unwrap();
                        }
                        "begin" => {
                            let mut next = intent.clone();
                            next.run_id = "bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee".into();
                            actor
                                .act_registry(ActRegistryCommand::Begin(next))
                                .await
                                .unwrap();
                        }
                        "recovery" => {
                            assert!(
                                actor
                                    .act_registry(ActRegistryCommand::ToolRecoveryBegin {
                                        run: intent.run_id.clone(),
                                        token: STALE_CLAIM.into(),
                                        created: 4,
                                        expires: 3604,
                                    })
                                    .await
                                    .is_err()
                            );
                        }
                        _ => {
                            assert!(
                                actor
                                    .act_registry(ActRegistryCommand::Claim {
                                        intent: intent.clone(),
                                        observed: observed.clone(),
                                        token: STALE_CLAIM.into(),
                                        at: 4.0
                                    })
                                    .await
                                    .is_err()
                            );
                        }
                    }
                    for _ in 0..2 {
                        assert!(
                            actor
                                .act_registry(ActRegistryCommand::StartupInterrupt {
                                    run: intent.run_id.clone(),
                                    at: 5.0
                                })
                                .await
                                .is_err()
                        );
                        let current = pending(&actor)
                            .await
                            .into_iter()
                            .find(|r| r.intent.run_id == intent.run_id)
                            .unwrap();
                        assert_eq!(current, before);
                    }
                    actor.stop().await;
                    task.await.unwrap();
                });
        }
    }
    #[test]
    fn startup_handles_pending_registered_cleanup_and_terminal_without_success() {
        for initial in ["pending", "registered", "cleanup", "terminal"] {
            let dir = TemporaryDirectory::new().unwrap();
            let path = dir.path().join("states.sqlite3");
            let intent = startup_fixture();
            let mut writer = Registry::create_writer(&path, STARTUP_OWNER).unwrap();
            let observed = ActEngineObservation {
                name: intent.engine_name(),
                engine_id: "1".repeat(64),
                image_digest: intent.engine_image_digest.clone(),
                labels: intent.required_labels(STARTUP_OWNER).unwrap(),
            };
            let mut tx = writer.begin_immediate().unwrap();
            tx.begin_act_engine(&intent).unwrap();
            if initial != "pending" {
                tx.register_act_engine(&intent.run_id, &observed, 2.0)
                    .unwrap();
            }
            if matches!(initial, "cleanup" | "terminal") {
                tx.request_act_cleanup(&intent.run_id, ActRunOutcome::Failed, 3.0)
                    .unwrap();
            }
            if initial == "terminal" {
                tx.finalize_act_cleanup(
                    &intent.run_id,
                    &ActEngineRemovalProof {
                        storage_volume: None,
                        name: intent.engine_name(),
                        engine_id: Some(observed.engine_id),
                    },
                    4.0,
                )
                .unwrap();
            }
            tx.commit().unwrap();
            let before = writer.act_engine(&intent.run_id).unwrap().unwrap();
            async_engine::RuntimeBuilder::multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .run(async {
                    let (sender, receiver) = async_engine::channel(16);
                    let actor = RegistryActor { sender };
                    let task = async_engine::launch(crate::registry_actor(writer, receiver, None));
                    assert!(
                        actor
                            .act_registry(ActRegistryCommand::StartupInterrupt {
                                run: intent.run_id.clone(),
                                at: f64::NAN
                            })
                            .await
                            .is_err()
                    );
                    let result = actor
                        .act_registry(ActRegistryCommand::StartupInterrupt {
                            run: intent.run_id.clone(),
                            at: 5.0,
                        })
                        .await;
                    assert_eq!(result.is_err(), initial == "terminal");
                    if initial != "terminal" {
                        let current = pending(&actor).await.remove(0);
                        assert_eq!(
                            current.state,
                            bosn_registry::act::ActEngineState::CleanupRequired
                        );
                        assert_eq!(current.execution, None);
                        if initial == "cleanup" {
                            assert_eq!(current, before);
                        } else {
                            assert_eq!(current.outcome, Some(ActRunOutcome::Interrupted));
                        }
                    }
                    actor.stop().await;
                    task.await.unwrap();
                });
            if initial == "terminal" {
                assert_eq!(
                    Registry::open_writer(&path)
                        .unwrap()
                        .act_engine(&intent.run_id)
                        .unwrap()
                        .unwrap(),
                    before
                );
            }
        }
    }
}
