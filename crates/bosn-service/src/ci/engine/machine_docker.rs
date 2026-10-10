//! [`MachineEngine`] on the host Docker engine (#544): the claim container,
//! act engine discovery and removal, and the in-engine slot table.

use std::collections::BTreeMap;

use bosn_registry::act::ENGINE_SOCKET_DIR;
use serde::Deserialize;

use super::{
    super::machine::{
        CLAIM_NAME, EngineClaim, EngineIdentity, Foreign, Holder, Leased, MachineEngine, Survey,
        reply,
    },
    BoxFuture, CONTROL_DEADLINE, DockerActBackend, engine_image, owned,
};

const SLOTS: &str = include_str!("../machine/slots.sh");
const ENGINE_NAMESPACE: &str = "label=com.zackees.bosn.act.namespace=act-engine-v1";

/// Anything but the engine's own daemons: an exec session (its parent is
/// outside the container, so its ppid is 0) or a job container's shim.
const BUSY: &str = "ps -o pid,ppid,comm | awk -v self=$$ \
    'NR > 1 && (($2 == 0 && $1 != 1 && $1 != self) || $3 ~ /^containerd-shim/)' | wc -l";

fn absent(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).contains("No such")
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Inspected {
    id: String,
    name: String,
    state: InspectedState,
    config: InspectedConfig,
    host_config: InspectedHost,
    #[serde(default)]
    mounts: Vec<InspectedMount>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectedState {
    running: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectedConfig {
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectedHost {
    #[serde(default)]
    memory: u64,
    #[serde(default)]
    cpu_quota: u64,
    #[serde(default)]
    nano_cpus: u64,
    #[serde(default)]
    pids_limit: Option<u64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectedMount {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    source: String,
    destination: String,
}

impl Inspected {
    fn labels(&self) -> BTreeMap<String, String> {
        self.config.labels.clone().unwrap_or_default()
    }

    fn foreign(self) -> Result<Foreign, String> {
        let labels = self.labels();
        if labels
            .get("com.zackees.bosn.act.namespace")
            .map(String::as_str)
            != Some("act-engine-v1")
        {
            return Err(format!("{} is not a bosn act engine", self.name));
        }
        let mount = |destination: &str| {
            self.mounts
                .iter()
                .find(|mount| mount.destination == destination)
        };
        let created = labels
            .get("com.zackees.bosn.created")
            .and_then(|at| at.parse::<f64>().ok())
            .filter(|at| at.is_finite() && *at >= 0.0)
            .ok_or_else(|| format!("{} has no creation time label", self.name))?;
        Ok(Foreign {
            name: self.name.trim_start_matches('/').to_owned(),
            running: self.state.running,
            identity: EngineIdentity::from_labels(&labels),
            registry: labels
                .get("com.zackees.bosn.registry")
                .cloned()
                .unwrap_or_default(),
            socket_dir: mount(ENGINE_SOCKET_DIR)
                .filter(|mount| mount.kind == "bind")
                .map(|mount| mount.source.clone()),
            storage_volume: mount(crate::act_engine::STORAGE_TARGET)
                .filter(|mount| mount.kind == "volume")
                .and_then(|mount| mount.name.clone()),
            memory_bytes: self.host_config.memory,
            nano_cpus: if self.host_config.nano_cpus > 0 {
                self.host_config.nano_cpus
            } else {
                self.host_config.cpu_quota * 10_000
            },
            pids: self.host_config.pids_limit.unwrap_or(0),
            created: created as u64,
            id: self.id,
        })
    }
}

impl DockerActBackend {
    async fn inspect_container(&self, name: &str) -> Result<Option<Inspected>, String> {
        let result = self
            .run(
                owned(&["container", "inspect", "--format", "{{json .}}", name]),
                CONTROL_DEADLINE,
            )
            .await?;
        if !result.ok() {
            if absent(&result.stderr) {
                return Ok(None);
            }
            return Err(format!(
                "inspect {name}: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            ));
        }
        serde_json::from_slice(&result.stdout)
            .map(Some)
            .map_err(|error| format!("inspect {name}: {error}"))
    }

    async fn slots(&self, engine: &str, args: &[&str]) -> Result<String, String> {
        let mut command = Self::exec(engine, SLOTS);
        command.push("sh".into());
        command.extend(args.iter().map(|arg| (*arg).to_owned()));
        self.checked("engine slot table", command, CONTROL_DEADLINE)
            .await
    }

    async fn removed(&self, what: &str, args: Vec<String>) -> Result<(), String> {
        let result = self.run(args, CONTROL_DEADLINE).await?;
        if result.ok() || absent(&result.stderr) {
            return Ok(());
        }
        Err(format!(
            "{what}: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ))
    }
}

impl MachineEngine for DockerActBackend {
    fn claim(&self) -> BoxFuture<'_, Result<Option<EngineClaim>, String>> {
        Box::pin(async move {
            match self.inspect_container(CLAIM_NAME).await? {
                None => Ok(None),
                Some(found) => EngineClaim::from_labels(&found.id, &found.labels()).map(Some),
            }
        })
    }

    fn create_claim<'a>(&'a self, claim: &'a EngineClaim) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let mut args = owned(&[
                "create",
                "--pull",
                "never",
                "--name",
                CLAIM_NAME,
                "--network",
                "none",
                // The image declares a VOLUME there; a tmpfs keeps the claim
                // from creating an anonymous volume it would leave behind.
                "--tmpfs",
                crate::act_engine::STORAGE_TARGET,
                "--entrypoint",
                "true",
            ]);
            for (key, value) in claim.labels() {
                args.extend(["--label".into(), format!("{key}={value}")]);
            }
            args.push(engine_image());
            let result = self.run(args, CONTROL_DEADLINE).await?;
            if result.ok() {
                return Ok(true);
            }
            let stderr = String::from_utf8_lossy(&result.stderr);
            if stderr.contains("Conflict") || stderr.contains("already in use") {
                return Ok(false);
            }
            Err(format!("engine claim create: {}", stderr.trim()))
        })
    }

    fn remove_claim<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.removed("engine claim remove", owned(&["rm", "-v", id])))
    }

    fn inspect<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<Foreign>, String>> {
        Box::pin(async move {
            self.inspect_container(name)
                .await?
                .map(Inspected::foreign)
                .transpose()
        })
    }

    fn engines(&self) -> BoxFuture<'_, Result<Vec<Foreign>, String>> {
        Box::pin(async move {
            let ids = self
                .checked(
                    "act engine list",
                    owned(&["ps", "-q", "--no-trunc", "--filter", ENGINE_NAMESPACE]),
                    CONTROL_DEADLINE,
                )
                .await?;
            let mut engines = Vec::new();
            for id in ids.lines().map(str::trim).filter(|id| !id.is_empty()) {
                // One removed meanwhile is simply gone.
                if let Some(found) = self.inspect_container(id).await? {
                    engines.push(found.foreign()?);
                }
            }
            Ok(engines)
        })
    }

    fn busy<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let count = self
                .checked(
                    "engine activity",
                    Self::exec(engine, BUSY),
                    CONTROL_DEADLINE,
                )
                .await?;
            count
                .trim()
                .parse::<u32>()
                .map(|n| n > 0)
                .map_err(|_| format!("engine activity: {count:?}"))
        })
    }

    fn remove_engine<'a>(&'a self, engine: &'a Foreign) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.removed("engine remove", owned(&["rm", "-f", &engine.id]))
                .await?;
            if self.inspect_container(&engine.id).await?.is_some() {
                return Err(format!("{} is still present after removal", engine.name));
            }
            if let Some(volume) = &engine.storage_volume {
                self.removed("storage volume remove", owned(&["volume", "rm", volume]))
                    .await?;
                if self.volume_exists(volume).await? {
                    return Err(format!("storage volume {volume} is still present"));
                }
            }
            Ok(())
        })
    }

    fn lease_slot<'a>(
        &'a self,
        engine: &'a str,
        holder: &'a Holder,
    ) -> BoxFuture<'a, Result<Leased, String>> {
        Box::pin(async move {
            let max = super::MAX_SLOTS.to_string();
            let line = holder.line();
            let mut args = vec!["lease", max.as_str()];
            args.extend(line.split(' '));
            reply::leased(&self.slots(engine, &args).await?)
        })
    }

    fn release_slot<'a>(&'a self, engine: &'a str, slot: u16) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.slots(engine, &["release", &slot.to_string()])
                .await
                .map(|_| ())
        })
    }

    fn survey<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<Survey, String>> {
        Box::pin(async move { reply::survey(&self.slots(engine, &["survey"]).await?) })
    }

    fn mark_ready<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.slots(engine, &["ready"]).await.map(|_| ()) })
    }

    fn request_retire<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { self.slots(engine, &["request"]).await.map(|_| ()) })
    }

    fn begin_retire<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let survey = reply::survey(&self.slots(engine, &["retire"]).await?)?;
            Ok(survey.retiring && survey.held.is_empty())
        })
    }
}
