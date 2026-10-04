//! Intent-derived private storage, distinct from the pinned machine cache.
use super::*;
use bosn_core::{ResourceKind, ResourceLabels, Retention, Scope};
use bosn_engine::DockerEngine;
use serde::Deserialize;

fn labels(
    intent: &ActEngineIntent,
    owner: &str,
) -> Result<BTreeMap<String, String>, ActEngineError> {
    Ok(ResourceLabels::new(
        owner,
        ResourceKind::Volume,
        "act-storage",
        &intent.run_id,
        Scope::Spec,
        &intent.run_id,
        &intent.created_at.to_string(),
        Some(Retention::Warm),
    )
    .map_err(|_| ActEngineError("invalid private storage labels".into()))?
    .to_map()
    .into_iter()
    .map(|(key, value)| (key.into(), value))
    .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct DockerVolume {
    pub(super) name: String,
    pub(super) driver: String,
    pub(super) scope: String,
    pub(super) options: Option<BTreeMap<String, String>>,
    pub(super) labels: Option<BTreeMap<String, String>>,
}

fn verify(document: &[u8], intent: &ActEngineIntent, owner: &str) -> Result<(), ActEngineError> {
    let volumes: Vec<DockerVolume> =
        serde_json::from_slice(document).map_err(|e| ActEngineError(e.to_string()))?;
    let [volume] = volumes.as_slice() else {
        return Err(ActEngineError("ambiguous private storage volume".into()));
    };
    if Some(&volume.name) != intent.storage_volume_name().as_ref()
        || volume.driver != "local"
        || volume.scope != "local"
        || volume
            .options
            .as_ref()
            .is_some_and(|options| !options.is_empty())
        || volume.labels.as_ref() != Some(&labels(intent, owner)?)
    {
        return Err(ActEngineError(
            "private storage volume ownership does not match".into(),
        ));
    }
    Ok(())
}

async fn present(engine: &DockerEngine, name: &str) -> Result<bool, ActEngineError> {
    let bytes = storage_control(
        engine,
        vec![
            "volume".into(),
            "ls".into(),
            "--filter".into(),
            format!("name={name}"),
            "--format".into(),
            "{{.Name}}".into(),
        ],
    )
    .await?;
    let output = std::str::from_utf8(&bytes)
        .map_err(|_| ActEngineError("invalid storage volume list encoding".into()))?;
    let names: Vec<_> = output.split_whitespace().collect();
    match names.as_slice() {
        [] => Ok(false),
        [found] if *found == name => Ok(true),
        _ => Err(ActEngineError(
            "ambiguous private storage volume list".into(),
        )),
    }
}

pub(crate) async fn ensure_storage_volume(
    engine: &DockerEngine,
    intent: &ActEngineIntent,
    owner: &str,
) -> Result<(), ActEngineError> {
    let Some(name) = intent.storage_volume_name() else {
        return Ok(());
    };
    if !present(engine, &name).await? {
        let mut args = vec![
            "volume".into(),
            "create".into(),
            "--driver".into(),
            "local".into(),
        ];
        for (key, value) in labels(intent, owner)? {
            args.extend(["--label".into(), format!("{key}={value}")]);
        }
        args.push(name.clone());
        let created = storage_control(engine, args).await?;
        if std::str::from_utf8(&created).ok().map(str::trim) != Some(name.as_str()) {
            return Err(ActEngineError(
                "storage creation returned an unexpected identity".into(),
            ));
        }
    }
    let document = storage_control(engine, vec!["volume".into(), "inspect".into(), name]).await?;
    verify(&document, intent, owner)
}

/// Exact named storage only. Attached volumes are refused by Docker; removal
/// is never forced. A successful exact listing is required before receipt.
pub(crate) async fn remove_storage_volume(
    engine: &DockerEngine,
    intent: &ActEngineIntent,
    owner: &str,
) -> Result<(), ActEngineError> {
    let Some(name) = intent.storage_volume_name() else {
        return Ok(());
    };
    if !present(engine, &name).await? {
        return Ok(());
    }
    let document = storage_control(
        engine,
        vec!["volume".into(), "inspect".into(), name.clone()],
    )
    .await?;
    verify(&document, intent, owner)?;
    super::create::docker_control_budget(
        engine,
        vec!["volume".into(), "rm".into(), name.clone()],
        super::budgets::DELETE,
    )
    .await?;
    if present(engine, &name).await? {
        return Err(ActEngineError(
            "private storage absence is not established".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::act_engine::tests::{OWNER, disk_intent};
    #[test]
    fn private_storage_rejects_foreign_retention_and_options() {
        let intent = disk_intent();
        let doc = serde_json::json!([{"Name":intent.storage_volume_name().unwrap(),"Driver":"local","Scope":"local","Options":null,"Labels":labels(&intent,OWNER).unwrap()}]);
        assert!(verify(&serde_json::to_vec(&doc).unwrap(), &intent, OWNER).is_ok());
        for field in ["name", "registry", "retention"] {
            let mut changed = doc.clone();
            match field {
                "name" => changed[0]["Name"] = serde_json::json!("bosn-ci-cache-v1"),
                "registry" => {
                    changed[0]["Labels"][bosn_core::LABEL_REGISTRY] =
                        serde_json::json!("00000000-0000-4000-8000-000000000000")
                }
                _ => changed[0]["Labels"][bosn_core::LABEL_RETENTION] = serde_json::json!("pinned"),
            }
            assert!(verify(&serde_json::to_vec(&changed).unwrap(), &intent, OWNER).is_err());
        }
        let mut changed = doc;
        changed[0]["Options"] = serde_json::json!({"type":"none","device":"/home","o":"bind"});
        assert!(verify(&serde_json::to_vec(&changed).unwrap(), &intent, OWNER).is_err());
    }
}

#[cfg(all(test, unix))]
mod transport_tests {
    use super::*;
    use crate::act_engine::tests::{OWNER, disk_intent};
    use kernal_api::{async_engine::RuntimeBuilder, platform::fs::TemporaryDirectory};
    const SCRIPT: &str = r#"import pathlib, sys, time
artifact, state, log, name, mode = sys.argv[1:6]
state, log = pathlib.Path(state), pathlib.Path(log)
args = sys.argv[6:]
if args[:2] == ['volume', 'ls']:
    if state.exists(): print(name)
elif args[:2] == ['volume', 'inspect']:
    if mode == 'unreadable': sys.exit(1)
    print(pathlib.Path(artifact).read_text())
elif args[:2] == ['volume', 'rm']:
    log.write_text(args[2])
    if mode == 'slow-remove': time.sleep(11)
    if mode == 'attached': sys.exit(1)
    if mode != 'persists': state.rename(state.with_name('removed'))
    if mode == 'lost_ack': sys.exit(1)
    print(name)
else: sys.exit(2)
"#;
    #[test]
    fn removal_errors_and_uncertain_absence_remain_retryable() {
        for mode in [
            "success",
            "slow-remove",
            "attached",
            "unreadable",
            "lost_ack",
            "persists",
        ] {
            let dir = TemporaryDirectory::new().unwrap();
            let intent = disk_intent();
            let name = intent.storage_volume_name().unwrap();
            let artifact = dir.path().join("inspect.json");
            let state = dir.path().join("present");
            let log = dir.path().join("removed-name");
            let script = dir.path().join("docker.py");
            std::fs::write(&script, SCRIPT).unwrap();
            std::fs::write(&state, "present").unwrap();
            let document = serde_json::json!([{"Name":name,"Driver":"local","Scope":"local","Options":null,"Labels":labels(&intent,OWNER).unwrap()}]);
            std::fs::write(&artifact, serde_json::to_vec(&document).unwrap()).unwrap();
            let engine = DockerEngine::synthetic_for_test(
                "python3",
                [
                    script.to_string_lossy().into_owned(),
                    artifact.to_string_lossy().into_owned(),
                    state.to_string_lossy().into_owned(),
                    log.to_string_lossy().into_owned(),
                    name.clone(),
                    mode.into(),
                ],
            );
            RuntimeBuilder::multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .run(async {
                    let result = remove_storage_volume(&engine, &intent, OWNER).await;
                    assert_eq!(
                        result.is_ok(),
                        matches!(mode, "success" | "slow-remove"),
                        "{mode}"
                    );
                    if mode == "lost_ack" {
                        assert!(!state.exists());
                        assert!(
                            remove_storage_volume(&engine, &intent, OWNER).await.is_ok(),
                            "a successful later list may establish absence"
                        );
                    }
                    if matches!(mode, "attached" | "persists" | "unreadable") {
                        assert!(state.exists());
                    }
                    if mode == "unreadable" {
                        assert!(!log.exists(), "failed inspect must not authorize deletion");
                    } else {
                        assert_eq!(
                            std::fs::read_to_string(&log).unwrap(),
                            name,
                            "only the exact private volume can be removed"
                        );
                    }
                });
        }
    }
}

async fn storage_control(
    engine: &DockerEngine,
    args: Vec<String>,
) -> Result<Vec<u8>, ActEngineError> {
    super::create::docker_control_budget(engine, args, super::budgets::STORAGE_CONTROL).await
}
