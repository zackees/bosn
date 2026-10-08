//! Native immutable tool publication for an exclusively held, finished engine.
use super::toolstore_records::*;
use super::{DockerActBackend, ENGINE_CACHE, ENGINE_WORK, owned, process_control::ProcessControl};
use crate::ci::cache_policy::CachePolicy;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const RECIPE: &str = include_str!("toolstore_session.sh");
pub(super) const SOURCE: &str = "/var/lib/docker/volumes/act-toolcache/_data";
const MAX_INSTALLS: usize = 128;

enum State {
    Fresh,
    Preparing(Intent),
    Published(Proof),
}

struct Session {
    io: ProcessControl,
    policy: CachePolicy,
    intent: Intent,
    published: bool,
    reservation: i64,
}

impl DockerActBackend {
    async fn tool_producer_digest(&self, engine: &str) -> Result<String, String> {
        let arch = self
            .checked(
                "tool producer architecture",
                Self::exec(engine, "uname -m"),
                super::CONTROL_DEADLINE,
            )
            .await?;
        let artifact =
            super::act_artifact(&arch).ok_or("unsupported tool producer architecture")?;
        self.checked(
            "tool producer digest",
            Self::exec(
                engine,
                &format!(
                    "printf '%s  %s\\n' {} {ENGINE_WORK}/bin/act | sha256sum -c - >/dev/null",
                    artifact.binary_sha256
                ),
            ),
            super::CONTROL_DEADLINE,
        )
        .await?;
        Ok(artifact.binary_sha256.into())
    }

    /// Copy under a native reader lease. Once copied, the engine's private
    /// volume has independent inodes and no longer needs shared-store liveness.
    pub(super) async fn seed_published_tools(&self, engine: &str) -> Result<bool, String> {
        let marker = format!("{ENGINE_CACHE}/.bosn-tool-enrolled-v1.json");
        let script = format!(
            "[ ! -L {marker} ] || exit 78; if [ ! -e {marker} ]; then printf absent; else [ -f {marker} ] || exit 78; cat {marker}; fi"
        );
        let record = self
            .checked(
                "tool enrollment read",
                Self::exec(engine, &script),
                super::CONTROL_DEADLINE,
            )
            .await?;
        if record == "absent" {
            return Ok(false);
        }
        let proof: Proof =
            serde_json::from_str(&record).map_err(|e| format!("tool enrollment proof: {e}"))?;
        require_producer(
            &proof.intent,
            proof.intent.policy,
            &self.tool_producer_digest(engine).await?,
        )?;
        if proof.schema_version != 1
            || !super::cache_usage::helper::valid_id(&proof.initial_generation)
        {
            return Err("tool enrollment proof is invalid".into());
        }
        let current = self
            .checked(
                "tool warm selection",
                owned(&[
                    "exec",
                    engine,
                    &format!("{ENGINE_WORK}/bin/act"),
                    "cache",
                    "tool-current",
                    "--cache-server-path",
                    STORE,
                    "--max-bytes",
                    &proof.intent.policy.repository_max_bytes.to_string(),
                ]),
                super::PULL_DEADLINE,
            )
            .await?;
        let selected: Selection = serde_json::from_str(&current).map_err(|e| e.to_string())?;
        if selected.schema_version != 1 || !super::cache_usage::helper::valid_id(&selected.id) {
            return Err("warm tool selection is invalid".into());
        }
        let copy = format!(
            "docker volume create act-toolcache >/dev/null; actual=$(docker volume inspect --format '{{{{.Mountpoint}}}}' act-toolcache); test \"$actual\" = {SOURCE}; \
             test -d {SOURCE}; test ! -L {SOURCE}; writers=$(docker ps -q --filter volume=act-toolcache); test -z \"$writers\"; \
             contents=$(ls -A {SOURCE}); test -z \"$contents\"; cp -a {STORE}/.tool-generations-v1/{}/tree/. {SOURCE}/",
            selected.id
        );
        self.checked(
            "tool warm copy",
            owned(&[
                "exec",
                engine,
                "timeout",
                "180",
                &format!("{ENGINE_WORK}/bin/act"),
                "cache",
                "tool-exec",
                "--cache-server-path",
                STORE,
                "--generation",
                &selected.id,
                "--max-bytes",
                &proof.intent.policy.repository_max_bytes.to_string(),
                "--apply",
                "--",
                "sh",
                "-ec",
                &copy,
            ]),
            super::PULL_DEADLINE,
        )
        .await?;
        // Fingerprint the actual private copy before jobs can modify it. BusyBox
        // cp truncates timestamps, so shared object IDs are not private IDs.
        let mut session = self.open_tools(engine, proof.intent.policy).await?;
        let result = session.capture_seed().await;
        let _ = session.io.send(b"abort\n").await;
        result?;
        Ok(true)
    }

    pub(super) async fn maintain_published_tools(
        &self,
        engine: &str,
        policy: CachePolicy,
    ) -> Result<Option<bosn_registry::cache_maintenance::ToolMaintenanceStats>, String> {
        let mut session = self.open_tools(engine, policy).await?;
        let result = session.maintain().await;
        let _ = session.io.send(b"abort\n").await;
        result
    }

    pub(super) async fn publish_tools(
        &self,
        engine: &str,
        policy: CachePolicy,
    ) -> Result<(), String> {
        let mut session = self.open_tools(engine, policy).await?;
        let result = session.save().await;
        let _ = session.io.send(b"abort\n").await;
        result
    }

    async fn open_tools(&self, engine: &str, policy: CachePolicy) -> Result<Session, String> {
        let act_sha256 = self.tool_producer_digest(engine).await?;
        // The lifecycle retains exclusive ownership of this engine after its
        // execution exits; nobody may start a new invocation during this save.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?;
        let expires = time::OffsetDateTime::from_unix_timestamp(now.as_secs() as i64)
            .map_err(|e| e.to_string())?
            .checked_sub(time::Duration::seconds(policy.unused_age_secs as i64))
            .ok_or("tool expiry timestamp overflow")?
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| e.to_string())?;
        let recipe = RECIPE
            .replace("@CACHE@", ENGINE_CACHE)
            .replace("@WORK@", ENGINE_WORK)
            .replace("@PAYLOAD@", &policy.repository_max_bytes.to_string())
            .replace("@EXPIRES@", &expires);
        let mut args = owned(&["exec", "-i", engine, "timeout", "180", "sh", "-c"]);
        args.push(recipe);
        let process = self
            .docker
            .with_args(args)
            .spawn_interactive(Duration::from_secs(15))
            .await
            .map_err(|e| e.to_string())?;
        let mut io = ProcessControl::new(
            process,
            "tool publication",
            b"bosn-tool-end:",
            Duration::from_secs(110),
        )
        .with_total_output_limit(4 * 1024 * 1024)?;
        match io.line().await?.as_slice() {
            b"bosn-tool-ready" => {}
            b"bosn-tool-busy" => return Err("tool publication lease busy".into()),
            _ => return Err("invalid tool publication lease acknowledgement".into()),
        }
        let intent = Intent {
            schema_version: 1,
            nonce: crate::ci::new_uuid().await.map_err(|e| e.to_string())?,
            act_sha256,
            recipe_sha256: kernal_api::hash::sha256_bytes(RECIPE.as_bytes()).to_hex(),
            policy,
        };
        Ok(Session {
            io,
            policy,
            intent,
            published: false,
            reservation: 0,
        })
    }
}

impl Session {
    async fn command(
        &mut self,
        operation: &str,
        argument: Option<&str>,
    ) -> Result<(i32, Vec<u8>), String> {
        let command = match argument {
            Some(value) => {
                if value.contains(['\n', '\r']) {
                    return Err("tool command contains a line separator".into());
                }
                format!("{operation}\n{value}\n")
            }
            None => format!("{operation}\n"),
        };
        self.io.command(command.as_bytes()).await
    }

    async fn raw(&mut self, operation: &str, argument: Option<&str>) -> Result<Vec<u8>, String> {
        let (code, bytes) = self.command(operation, argument).await?;
        if code != 0 {
            return Err(format!(
                "tool {operation} failed ({code}): {}",
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(256)
                    .collect::<String>()
            ));
        }
        Ok(bytes)
    }

    async fn json<T: DeserializeOwned>(
        &mut self,
        operation: &str,
        argument: Option<&str>,
    ) -> Result<T, String> {
        serde_json::from_slice(&self.raw(operation, argument).await?)
            .map_err(|e| format!("tool {operation} receipt: {e}"))
    }

    fn record<T: Serialize>(record: &T) -> Result<String, String> {
        serde_json::to_string(record).map_err(|e| e.to_string())
    }

    fn validate_intent(&self, intent: &Intent) -> Result<(), String> {
        require_producer(intent, self.policy, &self.intent.act_sha256)
    }

    async fn state(&mut self) -> Result<State, String> {
        let state = self.raw("state", None).await?;
        let split = state
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("tool state lacks framing")?;
        let (kind, record) = (&state[..split], &state[split + 1..]);
        match kind {
            b"fresh" if record.iter().all(u8::is_ascii_whitespace) => Ok(State::Fresh),
            b"preparing" => serde_json::from_slice(record)
                .map(State::Preparing)
                .map_err(|e| e.to_string()),
            b"published" => serde_json::from_slice(record)
                .map(State::Published)
                .map_err(|e| e.to_string()),
            _ => Err("tool publication state is invalid".into()),
        }
    }

    async fn maintain(
        &mut self,
    ) -> Result<Option<bosn_registry::cache_maintenance::ToolMaintenanceStats>, String> {
        match self.state().await? {
            State::Fresh => Ok(None),
            State::Preparing(intent) => {
                self.validate_intent(&intent)?;
                Err("tool retention held: enrollment awaits source publication recovery".into())
            }
            State::Published(proof) => {
                self.adopt(proof).await?;
                self.retain(self.policy.aggregate_max_bytes).await.map(Some)
            }
        }
    }

    async fn adopt(&mut self, proof: Proof) -> Result<(), String> {
        self.validate_intent(&proof.intent)?;
        if proof.schema_version != 1
            || !super::cache_usage::helper::valid_id(&proof.initial_generation)
        {
            return Err("tool enrollment proof is invalid".into());
        }
        self.intent = proof.intent;
        self.published = true;
        self.current()
            .await?
            .ok_or("published tool selection is missing")?;
        Ok(())
    }

    async fn save(&mut self) -> Result<(), String> {
        match self.state().await? {
            State::Fresh => {
                if self
                    .raw("installs", None)
                    .await?
                    .iter()
                    .all(u8::is_ascii_whitespace)
                {
                    return Ok(());
                }
                self.raw("begin", Some(&Self::record(&self.intent)?))
                    .await?;
            }
            State::Preparing(intent) => {
                self.validate_intent(&intent)?;
                self.intent = intent;
            }
            State::Published(proof) => self.adopt(proof).await?,
        }
        let block = String::from_utf8(self.raw("filesystem", None).await?)
            .map_err(|e| e.to_string())?
            .trim()
            .parse::<i64>()
            .map_err(|e| e.to_string())?;
        if !(512..=1048576).contains(&block) || !(block as u64).is_power_of_two() {
            return Err("tool filesystem allocation unit is unsupported".into());
        }
        // Reserve payload plus per-entry allocation and generation metadata.
        // This is scoped inode admission, not a filesystem/backing-file quota.
        self.reservation = self
            .policy
            .repository_max_bytes
            .checked_add(block * 100000 * 8)
            .and_then(|bytes| bytes.checked_add(32 * 1024 * 1024))
            .ok_or("tool reservation overflow")?;
        if self.reservation >= self.policy.aggregate_max_bytes {
            return Err("tool policy cannot admit a bounded publication reservation".into());
        }
        if !self.published {
            let initial = self.raw("initial", None).await?;
            let manifest = if initial == b"absent\n\n" || initial == b"absent\n" {
                self.publish_installs().await?
            } else {
                let manifest: Manifest =
                    serde_json::from_slice(&initial).map_err(|e| e.to_string())?;
                Self::validate_manifest(&manifest)?;
                manifest
            };
            if manifest.installs.is_empty() {
                return Ok(());
            }
            let selected = match self.current().await? {
                None => self.update("initialize", &manifest).await?,
                Some(selected) => {
                    let recovered: Snapshot = self.json("generation", None).await?;
                    recovered.validate(STORE, true, self.policy)?;
                    if recovered.id != selected {
                        return Err("tool selection differs from frozen initial manifest".into());
                    }
                    selected
                }
            };
            let proof = Proof {
                schema_version: 1,
                intent: self.intent.clone(),
                initial_generation: selected,
            };
            self.raw("acknowledge", Some(&Self::record(&proof)?))
                .await?;
            self.published = true;
        } else {
            self.retain(self.policy.aggregate_max_bytes - self.reservation)
                .await?;
            let selected: SelectionState = self.json("selection", None).await?;
            let current = Manifest {
                schema_version: selected.schema_version,
                installs: selected.installs.clone(),
            };
            Self::validate_manifest(&current)?;
            if !super::cache_usage::helper::valid_id(&selected.id) {
                return Err("tool successor lacks a verified expected selection".into());
            }
            let baseline = self.seed_baseline(current).await?;
            let manifest = self
                .publish_installs_with_current(&baseline.installs)
                .await?;
            if !manifest.installs.is_empty() {
                self.update(&format!("replace:{}", selected.id), &manifest)
                    .await?;
            }
        }
        self.retain(self.policy.aggregate_max_bytes).await?;
        Ok(())
    }

    fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
        let mut paths = BTreeSet::new();
        if manifest.schema_version != 1
            || manifest.installs.is_empty()
            || manifest.installs.len() > MAX_INSTALLS
            || manifest.installs.iter().any(|install| {
                !install_path(&install.path)
                    || !super::cache_usage::helper::valid_id(&install.object_id)
                    || !paths.insert(&install.path)
            })
        {
            return Err("tool install manifest is invalid or exceeds 128 installs".into());
        }
        Ok(())
    }

    async fn seed_baseline(&mut self, current: Manifest) -> Result<Manifest, String> {
        let seed = self.raw("seed", None).await?;
        if seed == b"absent\n" || seed == b"absent\n\n" {
            return Ok(current);
        }
        let baseline: Manifest = serde_json::from_slice(&seed).map_err(|e| e.to_string())?;
        Self::validate_manifest(&baseline)?;
        Ok(baseline)
    }

    async fn capture_seed(&mut self) -> Result<(), String> {
        let census =
            String::from_utf8(self.raw("installs", None).await?).map_err(|e| e.to_string())?;
        let paths: BTreeSet<_> = census.lines().filter(|p| !p.is_empty()).collect();
        if paths.len() > MAX_INSTALLS || paths.iter().any(|p| !install_path(p)) {
            return Err("warm seed census is invalid".into());
        }
        let mut manifest = Manifest {
            schema_version: 1,
            installs: Vec::new(),
        };
        for path in paths {
            let source = format!("{SOURCE}/{path}");
            let plan: Snapshot = self.json("plan", Some(&source)).await?;
            plan.validate_plan(&source, self.policy)?;
            manifest.installs.push(Install {
                path: path.into(),
                object_id: plan.id,
            });
        }
        Self::validate_manifest(&manifest)?;
        self.raw("record-seed", Some(&Self::record(&manifest)?))
            .await?;
        Ok(())
    }

    async fn publish_installs(&mut self) -> Result<Manifest, String> {
        self.publish_installs_with_current(&[]).await
    }

    async fn publish_installs_with_current(
        &mut self,
        current: &[Install],
    ) -> Result<Manifest, String> {
        let installs =
            String::from_utf8(self.raw("installs", None).await?).map_err(|e| e.to_string())?;
        let paths: BTreeSet<_> = installs.lines().filter(|line| !line.is_empty()).collect();
        if paths.len() > 1024 || paths.iter().any(|path| !install_path(path)) {
            return Err(
                "completed tool source census exceeds bounds or contains invalid paths".into(),
            );
        }
        let mut candidates = Vec::new();
        for path in paths {
            let source = format!("{SOURCE}/{path}");
            let plan: Snapshot = self.json("plan", Some(&source)).await?;
            plan.validate_plan(&source, self.policy)?;
            candidates.push(super::toolstore_selection::Candidate {
                path: path.into(),
                object_id: plan.id,
                bytes: plan.bytes.ok_or("tool plan lacks byte count")?,
                entries: plan.entries.ok_or("tool plan lacks entry count")?,
            });
        }
        let admitted = super::toolstore_selection::choose(
            candidates,
            current,
            self.policy.repository_max_bytes,
        )?;
        let mut manifest = Manifest {
            schema_version: 1,
            installs: Vec::new(),
        };
        for candidate in admitted {
            let usage = self.raw("usage", None).await?;
            let allocated = if usage == b"absent\n\n" || usage == b"absent\n" {
                0
            } else {
                serde_json::from_slice::<Usage>(&usage)
                    .map_err(|e| e.to_string())?
                    .allocated()?
            };
            if allocated > self.policy.aggregate_max_bytes - self.reservation {
                return Err("tool storage admission held: allocated inventory leaves insufficient publication space".into());
            }
            let source = format!("{SOURCE}/{}", candidate.path);
            let report: Snapshot = self
                .json(&format!("object:{}", candidate.object_id), Some(&source))
                .await?;
            report.validate(&source, false, self.policy)?;
            if report.id != candidate.object_id {
                return Err("tool publication differs from its admitted plan".into());
            }
            manifest.installs.push(Install {
                path: candidate.path,
                object_id: report.id,
            });
        }
        Ok(manifest)
    }

    async fn current(&mut self) -> Result<Option<String>, String> {
        let body = self.raw("current", None).await?;
        if body == b"absent\n\n" || body == b"absent\n" {
            return Ok(None);
        }
        let selected: Selection = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        if selected.schema_version != 1 || !super::cache_usage::helper::valid_id(&selected.id) {
            return Err("native tool selection is invalid".into());
        }
        Ok(Some(selected.id))
    }

    async fn update(&mut self, operation: &str, manifest: &Manifest) -> Result<String, String> {
        Self::validate_manifest(manifest)?;
        let update: Update = self.json(operation, Some(&Self::record(manifest)?)).await?;
        update.generation.validate(STORE, true, self.policy)?;
        if update.schema_version != 1
            || !update.selected
            || update.partial
            || !update.error.is_empty()
            || !update.pending_selection.is_empty()
            || self.current().await?.as_deref() != Some(&update.generation.id)
        {
            return Err("tool generation update lacks durable exact selection".into());
        }
        Ok(update.generation.id)
    }

    async fn retain(
        &mut self,
        bound: i64,
    ) -> Result<bosn_registry::cache_maintenance::ToolMaintenanceStats, String> {
        let (code, body) = self.command("retain", Some(&bound.to_string())).await?;
        let report: Retention =
            serde_json::from_slice(&body).map_err(|e| format!("tool retain receipt: {e}"))?;
        let lists = [
            &report.retired_generations,
            &report.protected_generations,
            &report.retired_objects,
            &report.protected_objects,
        ];
        if report.schema_version != 1
            || report.partial
            || !report.error.is_empty()
            || report
                .retired_generations
                .as_ref()
                .is_some_and(|ids| ids.len() > 128)
            || report
                .retired_objects
                .as_ref()
                .is_some_and(|ids| ids.len() > 128)
            || report.stage_retention.schema_version != 1
            || report.stage_retention.partial
            || !report.stage_retention.error.is_empty()
            || lists.iter().any(|list| {
                list.as_ref().is_some_and(|ids| {
                    ids.len() > 1000000
                        || ids
                            .iter()
                            .any(|id| !super::cache_usage::helper::valid_id(id))
                })
            })
            || report
                .stage_retention
                .retired_stages
                .as_ref()
                .is_some_and(|stages| {
                    stages.len() > 128
                        || stages.iter().any(|stage| {
                            stage.len() > 1024 || stage.bytes().any(|b| b.is_ascii_control())
                        })
                })
        {
            return Err(
                "tool retention held: incomplete inventory or protected storage overflow".into(),
            );
        }
        let before = report.before.allocated()?;
        report.stage_retention.after.allocated()?;
        let allocated = report.after.allocated()?;
        if code != 0 || report.protected_overflow || allocated > bound {
            return Err(format!(
                "tool retention held: allocated={allocated} limit={bound}, protected generations={}, protected objects={}",
                report.protected_generations.as_ref().map_or(0, Vec::len),
                report.protected_objects.as_ref().map_or(0, Vec::len)
            ));
        }
        Ok(bosn_registry::cache_maintenance::ToolMaintenanceStats {
            allocated_before: before,
            allocated_after: allocated,
            retired_generations: report
                .retired_generations
                .as_ref()
                .map_or(0, |ids| ids.len() as u32),
            retired_objects: report
                .retired_objects
                .as_ref()
                .map_or(0, |ids| ids.len() as u32),
        })
    }
}

// Exact producer/recipe provenance from the pre-rollover implementation at
// cea9c15e. Its canonical object/generation formats and FD7 enrollment fence
// remain compatible. Keep the original receipt: the current verified native
// binary must still validate selection/payload before warm use or maintenance.
const COMPATIBLE_TOOL_PRODUCER: (&str, &str) = (
    "b4be8d7ef98729ad16a9a6ddba331f1d2b0feb8abd52eb9e5f3a93155fb4f1df",
    "66430164c4f2fb7cf9190fb29b43058b53f72b2b37442916f49f33b57c394b50",
);

fn require_producer(intent: &Intent, policy: CachePolicy, act_sha256: &str) -> Result<(), String> {
    let nonce = intent.nonce.len() == 36
        && intent.nonce.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
            }
        });
    let current = intent.act_sha256 == act_sha256
        && intent.recipe_sha256 == kernal_api::hash::sha256_bytes(RECIPE.as_bytes()).to_hex();
    let compatible = intent.act_sha256 == COMPATIBLE_TOOL_PRODUCER.0
        && intent.recipe_sha256 == COMPATIBLE_TOOL_PRODUCER.1;
    if intent.schema_version != 1
        || !nonce
        || !super::cache_usage::helper::valid_id(act_sha256)
        || !(current || compatible)
        || intent.policy != policy
    {
        return Err(
            "tool enrollment authority differs from verified engine producer or policy".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod authority_tests {
    use super::*;

    const OLD_ACT: &str = COMPATIBLE_TOOL_PRODUCER.0;
    const OLD_RECIPE: &str = COMPATIBLE_TOOL_PRODUCER.1;

    fn previous() -> Intent {
        Intent {
            schema_version: 1,
            nonce: "12345678-1234-1234-1234-123456789abc".into(),
            act_sha256: OLD_ACT.into(),
            recipe_sha256: OLD_RECIPE.into(),
            policy: CachePolicy::default(),
        }
    }

    #[test]
    fn verified_previous_enrollment_survives_recipe_and_native_upgrade() {
        let intent = previous();
        require_producer(&intent, intent.policy, OLD_ACT).unwrap();
        require_producer(&intent, intent.policy, &"a".repeat(64)).unwrap();
        assert_eq!(intent.act_sha256, OLD_ACT);
        assert_eq!(intent.recipe_sha256, OLD_RECIPE);
    }

    #[test]
    fn previous_enrollment_does_not_authorize_unknown_producers_or_changed_policy() {
        let mut intent = previous();
        intent.act_sha256 = "c".repeat(64);
        assert!(require_producer(&intent, intent.policy, OLD_ACT).is_err());
        intent = previous();
        intent.recipe_sha256 = "c".repeat(64);
        assert!(require_producer(&intent, intent.policy, OLD_ACT).is_err());
        intent = previous();
        let mut policy = intent.policy;
        policy.repository_max_bytes /= 2;
        assert!(require_producer(&intent, policy, OLD_ACT).is_err());
        intent.nonce = "not-an-enrollment".into();
        assert!(require_producer(&intent, intent.policy, OLD_ACT).is_err());
    }
}
