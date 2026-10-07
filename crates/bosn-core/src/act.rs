//! Versioned, declaration-only Act planning. This module performs no I/O.
//!
//! Source context and adapter bytes must come from the trusted producer's Git
//! observations, never client authority. Declared cells are requirements, not
//! proof that dynamic workflow conditions or the actual Act graph matched.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub const ACT_ADAPTER_SCHEMA: u32 = 1;
pub const ACT_VERSION: &str = "0.2.89-act2.15";
const MAX_DOCUMENT_BYTES: usize = 1 << 20;
const MAX_CELLS: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryIdentity {
    pub owner: String,
    pub name: String,
}
impl RepositoryIdentity {
    fn valid(&self) -> bool {
        [self.owner.as_str(), self.name.as_str()].iter().all(|s| {
            !s.is_empty()
                && s.len() <= 100
                && !matches!(*s, "." | "..")
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    }
    fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActPins {
    pub interface_schema: u32,
    pub act_version: String,
    pub act_binary_digest: String,
    pub engine_manifest_digest: String,
    pub engine_config_digest: String,
    pub runner_manifest_digest: String,
    pub runner_config_digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActWorkflows {
    pub pull_request: Vec<String>,
    pub push: Vec<String>,
    pub release: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActProofScope {
    LocalLinux,
    GithubOnly,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActRequiredCell {
    pub id: String,
    pub workflow: String,
    pub job: String,
    pub runner: String,
    pub proof_scope: ActProofScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<crate::act_coverage::ActMatrixTuple>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActTiers {
    pub minimal: Vec<String>,
    pub test: Vec<String>,
    pub full: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActPinnedVersion {
    pub name: String,
    pub value: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActReleaseInputs {
    pub candidate_sha: String,
    pub full_mode: String,
    pub full_mode_value: String,
    pub version: Option<ActPinnedVersion>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActAdapterV1 {
    pub schema_version: u32,
    pub repository: RepositoryIdentity,
    pub default_branch: String,
    pub pins: ActPins,
    pub workflows: ActWorkflows,
    pub cells: Vec<ActRequiredCell>,
    pub tiers: ActTiers,
    pub release_inputs: ActReleaseInputs,
    pub permitted_secrets: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActEvent {
    PullRequest,
    Push,
    Release,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActMode {
    Minimal,
    Test,
    Full,
}
/// Trusted producer observations; this is not a wire authorization request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActSourceContext {
    pub repository: RepositoryIdentity,
    pub current_sha: String,
    pub base_sha: Option<String>,
    pub pull_request: Option<ActPullRequestIdentity>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActPullRequestIdentity {
    pub number: u64,
    pub head_sha: String,
    pub base_sha: String,
    pub head_repository: RepositoryIdentity,
    pub base_repository: RepositoryIdentity,
    pub head_ref: String,
    pub base_ref: String,
    pub author_login: String,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ActDeclaredPlan {
    pub schema_version: u32,
    pub event_name: String,
    pub event_payload: Value,
    pub workflows: Vec<String>,
    pub required_cells: Vec<ActRequiredCell>,
    pub declaration_only: bool,
    pub graph_matched: bool,
    /// Required foreign-platform cells stay in required_cells. They are never
    /// transformed into local success by this pure declaration resolver.
    pub github_only_required: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActAdapterError(pub &'static str);
impl std::fmt::Display for ActAdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ActAdapterError {}
fn refuse(message: &'static str) -> ActAdapterError {
    ActAdapterError(message)
}
fn hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|s| hex(s, 64))
}
fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
fn workflow_path(s: &str) -> bool {
    let Some(file) = s.strip_prefix(".github/workflows/") else {
        return false;
    };
    !file.is_empty()
        && file.len() <= 128
        && !file.contains('/')
        && !file.contains('\\')
        && !matches!(file, "." | "..")
        && identifier(file)
        && (file.ends_with(".yml") || file.ends_with(".yaml"))
}
fn unique(values: &[String]) -> Result<BTreeSet<&str>, ActAdapterError> {
    let set: BTreeSet<_> = values.iter().map(String::as_str).collect();
    if values.is_empty() || values.len() > MAX_CELLS || set.len() != values.len() {
        return Err(refuse("empty, duplicate or oversized selector list"));
    }
    Ok(set)
}
impl ActAdapterV1 {
    #[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
    pub fn validate(&self) -> Result<(), ActAdapterError> {
        if self.schema_version != ACT_ADAPTER_SCHEMA
            || self.pins.interface_schema != 1
            || self.pins.act_version != ACT_VERSION
            || !self.repository.valid()
            || !matches!(self.default_branch.as_str(), "main" | "master")
            || !self.permitted_secrets.is_empty()
        {
            return Err(refuse(
                "unsupported adapter identity, schema, version or secrets",
            ));
        }
        for pin in [
            &self.pins.act_binary_digest,
            &self.pins.engine_manifest_digest,
            &self.pins.engine_config_digest,
            &self.pins.runner_manifest_digest,
            &self.pins.runner_config_digest,
        ] {
            if !digest(pin) {
                return Err(refuse("image and binary pins require exact sha256 digests"));
            }
        }
        let mut workflows = BTreeSet::new();
        for list in [
            &self.workflows.pull_request,
            &self.workflows.push,
            &self.workflows.release,
        ] {
            if list.len() > 128 {
                return Err(refuse("oversized event workflow inventory"));
            }
            for path in unique(list)? {
                if !workflow_path(path) {
                    return Err(refuse("unsafe repository-relative workflow path"));
                }
                workflows.insert(path);
            }
        }
        if self.cells.is_empty() || self.cells.len() > MAX_CELLS {
            return Err(refuse("empty or oversized required cell inventory"));
        }
        let mut ids = BTreeSet::new();
        let mut tuples = BTreeSet::new();
        for cell in &self.cells {
            if let Some(matrix) = &cell.matrix {
                matrix.validate().map_err(refuse)?;
                if !tuples.insert((
                    &cell.workflow,
                    &cell.job,
                    &cell.runner,
                    matrix.canonical_json(),
                )) {
                    return Err(refuse("duplicate required physical tuple"));
                }
            }
            if !identifier(&cell.id)
                || !identifier(&cell.job)
                || !identifier(&cell.runner)
                || !workflows.contains(cell.workflow.as_str())
                || !ids.insert(cell.id.as_str())
            {
                return Err(refuse("invalid, duplicate or foreign required cell"));
            }
            if cell.proof_scope == ActProofScope::LocalLinux
                && !matches!(cell.runner.as_str(), "ubuntu-latest" | "ubuntu-22.04")
            {
                return Err(refuse(
                    "local Linux declaration cannot cover a foreign runner",
                ));
            }
        }
        let minimal = unique(&self.tiers.minimal)?;
        let test = unique(&self.tiers.test)?;
        let full = unique(&self.tiers.full)?;
        if !minimal.is_subset(&test)
            || test.len() <= minimal.len()
            || !test.is_subset(&full)
            || full != ids
        {
            return Err(refuse(
                "test must extend minimal; full must require every declared cell",
            ));
        }
        let release = &self.release_inputs;
        if !identifier(&release.candidate_sha)
            || !identifier(&release.full_mode)
            || release.candidate_sha == release.full_mode
            || release.full_mode_value != "full"
            || release.version.as_ref().is_some_and(|v| {
                !identifier(&v.name)
                    || v.name == release.candidate_sha
                    || v.name == release.full_mode
                    || v.value.is_empty()
                    || v.value.len() > 128
                    || !v.value.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+')
                    })
            })
        {
            return Err(refuse("invalid or conflicting release input declarations"));
        }
        Ok(())
    }
}
pub fn parse_act_adapter_json(bytes: &[u8]) -> Result<ActAdapterV1, ActAdapterError> {
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(refuse("adapter exceeds document ceiling"));
    }
    let adapter: ActAdapterV1 = serde_json::from_slice(bytes)
        .map_err(|_| refuse("invalid or unknown adapter schema fields"))?;
    adapter.validate()?;
    Ok(adapter)
}
/// Resolve requirements without claiming to evaluate GitHub or Act workflow ifs.
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
pub fn resolve_act_event(
    adapter: &ActAdapterV1,
    event: ActEvent,
    mode: ActMode,
    source: &ActSourceContext,
) -> Result<ActDeclaredPlan, ActAdapterError> {
    adapter.validate()?;
    if source.repository != adapter.repository
        || !source.repository.valid()
        || !hex(&source.current_sha, 40)
        || source.base_sha.as_ref().is_some_and(|s| !hex(s, 40))
    {
        return Err(refuse("source repository or candidate identity is invalid"));
    }
    let repo = json!({"name":adapter.repository.name, "full_name":adapter.repository.full_name(),
                      "owner":{"login":adapter.repository.owner}, "default_branch":adapter.default_branch});
    let (event_name, event_payload, workflows) = match event {
        ActEvent::PullRequest => {
            let pr = source
                .pull_request
                .as_ref()
                .ok_or_else(|| refuse("PR identity missing"))?;
            if pr.number == 0
                || pr.head_sha != source.current_sha
                || source.base_sha.as_deref() != Some(pr.base_sha.as_str())
                || !hex(&pr.base_sha, 40)
                || pr.base_repository != adapter.repository
                || pr.base_ref != adapter.default_branch
                || !pr.head_repository.valid()
                || !(RepositoryIdentity {
                    owner: pr.author_login.clone(),
                    name: "identity".into(),
                })
                .valid()
                || pr.head_ref.is_empty()
                || pr.head_ref.len() > 255
                || pr
                    .head_ref
                    .bytes()
                    .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            {
                return Err(refuse("PR head/base/repository identity drift"));
            }
            let labels = match mode {
                ActMode::Minimal => vec![],
                ActMode::Test => vec![json!({"name":"ci-test"})],
                ActMode::Full => vec![json!({"name":"ci-full"})],
            };
            (
                "pull_request",
                json!({"action":"opened","number":pr.number,"repository":repo,"pull_request":{"number":pr.number,
                "state":"open","user":{"login":pr.author_login},"labels":labels,
                "head":{"sha":source.current_sha,"ref":pr.head_ref,"repo":{"name":pr.head_repository.name,"full_name":pr.head_repository.full_name(),"owner":{"login":pr.head_repository.owner}}},
                "base":{"sha":pr.base_sha,"ref":adapter.default_branch,"repo":repo}}}),
                &adapter.workflows.pull_request,
            )
        }
        ActEvent::Push => {
            if mode != ActMode::Minimal || source.pull_request.is_some() {
                return Err(refuse("push accepts only minimal without PR identity"));
            }
            let before = source
                .base_sha
                .as_ref()
                .ok_or_else(|| refuse("push base SHA missing"))?;
            (
                "push",
                json!({"repository":repo,"ref":format!("refs/heads/{}",adapter.default_branch),
                "before":before,"after":source.current_sha,"created":false,"deleted":false}),
                &adapter.workflows.push,
            )
        }
        ActEvent::Release => {
            if mode != ActMode::Full || source.pull_request.is_some() {
                return Err(refuse("release accepts only full without PR identity"));
            }
            let mut inputs = BTreeMap::from([
                (
                    adapter.release_inputs.candidate_sha.clone(),
                    source.current_sha.clone(),
                ),
                (
                    adapter.release_inputs.full_mode.clone(),
                    adapter.release_inputs.full_mode_value.clone(),
                ),
            ]);
            if let Some(version) = &adapter.release_inputs.version {
                inputs.insert(version.name.clone(), version.value.clone());
            }
            (
                "workflow_dispatch",
                json!({"repository":repo,"ref":format!("refs/heads/{}",adapter.default_branch),"inputs":inputs}),
                &adapter.workflows.release,
            )
        }
    };
    let selected = match mode {
        ActMode::Minimal => &adapter.tiers.minimal,
        ActMode::Test => &adapter.tiers.test,
        ActMode::Full => &adapter.tiers.full,
    };
    let indexed: BTreeMap<_, _> = adapter.cells.iter().map(|c| (c.id.as_str(), c)).collect();
    let required_cells: Vec<_> = selected
        .iter()
        .map(|id| indexed[id.as_str()].clone())
        .collect();
    let github_only_required = required_cells
        .iter()
        .filter(|c| c.proof_scope == ActProofScope::GithubOnly)
        .map(|c| c.id.clone())
        .collect();
    Ok(ActDeclaredPlan {
        schema_version: 1,
        event_name: event_name.into(),
        event_payload,
        workflows: workflows.clone(),
        required_cells,
        declaration_only: true,
        graph_matched: false,
        github_only_required,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    pub(super) fn adapter_document() -> Value {
        json!({
            "schema_version": 1,
            "repository": {"owner":"FastLED","name":"fbuild"},
            "default_branch": "main",
            "pins": {
                "interface_schema":1,"act_version":ACT_VERSION,
                "act_binary_digest":format!("sha256:{}", "a".repeat(64)),
                "engine_manifest_digest":format!("sha256:{}", "b".repeat(64)),
                "engine_config_digest":format!("sha256:{}", "c".repeat(64)),
                "runner_manifest_digest":format!("sha256:{}", "d".repeat(64)),
                "runner_config_digest":format!("sha256:{}", "e".repeat(64))
            },
            "workflows": {"pull_request":[".github/workflows/ci.yml"],"push":[".github/workflows/ci.yml"],"release":[".github/workflows/release.yml"]},
            "cells":[
                {"id":"lint","workflow":".github/workflows/ci.yml","job":"lint","runner":"ubuntu-22.04","proof_scope":"local_linux"},
                {"id":"tests","workflow":".github/workflows/ci.yml","job":"test","runner":"ubuntu-22.04","proof_scope":"local_linux"},
                {"id":"mac","workflow":".github/workflows/ci.yml","job":"test-mac","runner":"macos-15","proof_scope":"github_only"},
                {"id":"release-native","workflow":".github/workflows/release.yml","job":"native","runner":"ubuntu-22.04","proof_scope":"local_linux"}
            ],
            "tiers":{"minimal":["lint"],"test":["lint","tests"],"full":["lint","tests","mac","release-native"]},
            "release_inputs":{"candidate_sha":"commit_sha","full_mode":"ci_mode","full_mode_value":"full","version":{"name":"version","value":"1.2.3"}},
            "permitted_secrets":[]
        })
    }
    pub(super) fn context() -> ActSourceContext {
        ActSourceContext {
            repository: RepositoryIdentity {
                owner: "FastLED".into(),
                name: "fbuild".into(),
            },
            current_sha: "f".repeat(40),
            base_sha: Some("0".repeat(40)),
            pull_request: Some(ActPullRequestIdentity {
                number: 123,
                head_sha: "f".repeat(40),
                base_sha: "0".repeat(40),
                head_repository: RepositoryIdentity {
                    owner: "contributor".into(),
                    name: "fbuild".into(),
                },
                base_repository: RepositoryIdentity {
                    owner: "FastLED".into(),
                    name: "fbuild".into(),
                },
                head_ref: "feature/fixture".into(),
                base_ref: "main".into(),
                author_login: "actual-contributor".into(),
            }),
        }
    }
    #[test]
    fn declaration_only_payload_binds_exact_candidate_and_literal_label() {
        let adapter: ActAdapterV1 = serde_json::from_value(adapter_document()).unwrap();
        let plan =
            resolve_act_event(&adapter, ActEvent::PullRequest, ActMode::Full, &context()).unwrap();
        assert_eq!(plan.event_name, "pull_request");
        assert_eq!(
            plan.event_payload["pull_request"]["head"]["sha"],
            "f".repeat(40)
        );
        assert_eq!(
            plan.event_payload["pull_request"]["labels"][0]["name"],
            "ci-full"
        );
        assert!(plan.declaration_only);
        assert!(!plan.graph_matched);
    }
}

#[cfg(test)]
mod conformance_tests {
    use super::tests::{adapter_document, context};
    use super::*;

    fn adapter() -> ActAdapterV1 {
        parse_act_adapter_json(&serde_json::to_vec(&adapter_document()).unwrap()).unwrap()
    }
    #[test]
    #[expect(clippy::cognitive_complexity, reason = "baseline, ci.yml#229")]
    fn every_event_mode_combination_is_explicit() {
        for event in [ActEvent::PullRequest, ActEvent::Push, ActEvent::Release] {
            for mode in [ActMode::Minimal, ActMode::Test, ActMode::Full] {
                let mut source = context();
                if event != ActEvent::PullRequest {
                    source.pull_request = None;
                }
                let result = resolve_act_event(&adapter(), event, mode, &source);
                let permitted = event == ActEvent::PullRequest
                    || (event == ActEvent::Push && mode == ActMode::Minimal)
                    || (event == ActEvent::Release && mode == ActMode::Full);
                assert_eq!(result.is_ok(), permitted, "{event:?}/{mode:?}");
                if let Ok(plan) = result {
                    assert!(plan.declaration_only && !plan.graph_matched);
                    assert_eq!(
                        plan.required_cells.len(),
                        match mode {
                            ActMode::Minimal => 1,
                            ActMode::Test => 2,
                            ActMode::Full => 4,
                        }
                    );
                    if event == ActEvent::PullRequest {
                        let labels = plan.event_payload["pull_request"]["labels"]
                            .as_array()
                            .unwrap();
                        assert_eq!(labels.len(), usize::from(mode != ActMode::Minimal));
                        if mode != ActMode::Minimal {
                            assert_eq!(
                                labels[0]["name"],
                                if mode == ActMode::Full {
                                    "ci-full"
                                } else {
                                    "ci-test"
                                }
                            );
                        }
                        assert_eq!(plan.event_payload["action"], "opened");
                        assert!(plan.event_payload.get("before").is_none());
                        assert_eq!(
                            plan.event_payload["pull_request"]["user"]["login"],
                            "actual-contributor"
                        );
                    } else if event == ActEvent::Push {
                        assert_eq!(plan.event_payload["after"], source.current_sha);
                        assert_eq!(plan.event_payload["before"], source.base_sha.unwrap());
                        assert_eq!(plan.event_payload["ref"], "refs/heads/main");
                    } else {
                        assert_eq!(plan.event_name, "workflow_dispatch");
                        assert_eq!(
                            plan.event_payload["inputs"]["commit_sha"],
                            source.current_sha
                        );
                        assert_eq!(plan.event_payload["inputs"]["ci_mode"], "full");
                        assert_eq!(plan.event_payload["inputs"]["version"], "1.2.3");
                    }
                    if mode == ActMode::Full {
                        assert_eq!(plan.github_only_required, ["mac"]);
                        assert!(plan.required_cells.iter().any(|cell| cell.id == "mac"));
                    }
                }
            }
        }
    }
    #[test]
    fn all_nine_repository_identities_are_data_not_bosn_constants() {
        for (owner, name, branch) in [
            ("zackees", "soldr", "main"),
            ("zackees", "kernal-api", "main"),
            ("zackees", "zccache", "main"),
            ("FastLED", "cli", "main"),
            ("zackees", "bosn", "main"),
            ("zackees", "mimalloc-pprof", "main"),
            ("FastLED", "fbuild", "main"),
            ("FastLED", "FastLED", "master"),
            ("zackees", "clud", "main"),
        ] {
            let identity = RepositoryIdentity {
                owner: owner.into(),
                name: name.into(),
            };
            let mut config = adapter();
            config.repository = identity.clone();
            config.default_branch = branch.into();
            let mut source = context();
            source.repository = identity.clone();
            source.pull_request = None;
            let plan =
                resolve_act_event(&config, ActEvent::Push, ActMode::Minimal, &source).unwrap();
            assert_eq!(
                plan.event_payload["repository"]["full_name"],
                format!("{owner}/{name}")
            );
            assert_eq!(plan.event_payload["ref"], format!("refs/heads/{branch}"));
        }
    }
    #[test]
    fn unknown_fields_in_every_nested_schema_and_unknown_enums_refuse() {
        for path in [
            vec![],
            vec!["repository"],
            vec!["pins"],
            vec!["workflows"],
            vec!["tiers"],
            vec!["release_inputs"],
            vec!["release_inputs", "version"],
        ] {
            let mut doc = adapter_document();
            let mut object = &mut doc;
            for key in path {
                object = &mut object[key];
            }
            object["unreviewed_authority"] = json!(true);
            assert!(parse_act_adapter_json(&serde_json::to_vec(&doc).unwrap()).is_err());
        }
        let mut doc = adapter_document();
        doc["cells"][0]["unknown"] = json!(true);
        assert!(parse_act_adapter_json(&serde_json::to_vec(&doc).unwrap()).is_err());
        for value in ["", "extended", "all"] {
            assert!(serde_json::from_value::<ActMode>(json!(value)).is_err());
            assert!(serde_json::from_value::<ActEvent>(json!(value)).is_err());
        }
        doc = adapter_document();
        doc["cells"][0]["proof_scope"] = json!("local_success");
        assert!(parse_act_adapter_json(&serde_json::to_vec(&doc).unwrap()).is_err());
    }
    #[test]
    fn unsafe_selectors_bad_pins_and_incomplete_tiers_refuse() {
        for path in [
            "../ci.yml",
            "/.github/workflows/ci.yml",
            ".github/workflows/../ci.yml",
            ".github/workflows/dir/ci.yml",
            ".github/workflows/ci.yaml/",
            ".github/workflows/ci\\escape.yml",
            "",
        ] {
            let mut config = adapter();
            config.workflows.push = vec![path.into()];
            assert!(config.validate().is_err(), "{path}");
        }
        for case in 0..13 {
            let mut config = adapter();
            match case {
                0 => config.schema_version = 2,
                1 => config.pins.interface_schema = 2,
                2 => config.pins.act_version = "0.2.89".into(),
                3 => config.pins.act_binary_digest = "latest".into(),
                4 => config.pins.engine_manifest_digest = format!("sha256:{}", "A".repeat(64)),
                5 => config.cells.push(config.cells[0].clone()),
                6 => config.cells[0].workflow = ".github/workflows/foreign.yml".into(),
                7 => config.tiers.test = config.tiers.minimal.clone(),
                8 => {
                    let _ = config.tiers.full.pop();
                }
                9 => config.tiers.minimal.push("missing".into()),
                10 => config.workflows.push.push(config.workflows.push[0].clone()),
                11 => config.permitted_secrets.push("TOKEN".into()),
                _ => config.cells[0].runner = "windows-2022".into(),
            }
            assert!(config.validate().is_err(), "case{case}");
        }
    }
    #[test]
    fn unsupported_linux_runner_remains_required_as_github_only() {
        let mut config = adapter();
        config.cells[0].runner = "ubuntu-24.04".into();
        assert!(config.validate().is_err());
        config.cells[0].proof_scope = ActProofScope::GithubOnly;
        let plan =
            resolve_act_event(&config, ActEvent::PullRequest, ActMode::Full, &context()).unwrap();
        assert!(plan.required_cells.iter().any(|cell| cell.id == "lint"));
        assert!(plan.github_only_required.contains(&"lint".into()));
        assert!(!plan.graph_matched);
    }
    #[test]
    fn missing_and_drifting_source_identity_refuses_before_payload_creation() {
        for case in 0..11 {
            let mut source = context();
            match case {
                0 => source.pull_request = None,
                1 => source.base_sha = None,
                2 => source.current_sha = "a".repeat(40),
                3 => source.repository.owner = "foreign".into(),
                4 => source.current_sha = "bad".into(),
                5 => source.pull_request.as_mut().unwrap().number = 0,
                6 => source.pull_request.as_mut().unwrap().base_sha = "a".repeat(40),
                7 => source.pull_request.as_mut().unwrap().base_repository.owner = "foreign".into(),
                8 => source.pull_request.as_mut().unwrap().head_ref.clear(),
                9 => source.pull_request.as_mut().unwrap().author_login.clear(),
                _ => source.pull_request.as_mut().unwrap().base_ref = "foreign-branch".into(),
            }
            assert!(
                resolve_act_event(&adapter(), ActEvent::PullRequest, ActMode::Full, &source)
                    .is_err(),
                "case{case}"
            );
        }
    }
}
