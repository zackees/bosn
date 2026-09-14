//! Narrow, engine-backed preparation of a validated setup application's image.
//!
//! This module is deliberately smaller than setup apply.  It may pull one
//! immutable image or build one generated private asset tree, then observes the
//! resulting image ID.  It never creates a container, mounts a workspace,
//! invokes a task, or accepts user-provided Docker argv.

use std::{future::Future, path::PathBuf, pin::Pin};

use bosn_engine::{CommandError, CommandResult, DockerEngine, EngineEvent, RunOptions};
use kernal_api::async_engine::{CancellationToken, Deadline, Sender};

use crate::{SetupPlan, SetupPlanAppSource, materialize::verify_materialized_assets};

/// One semantic Docker action the setup image preparer is permitted to issue.
///
/// This is intentionally not a generic argv type.  The production adapter is
/// the sole conversion to Docker CLI arguments, and validates all plan-derived
/// values before one of these commands can be constructed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupImageCommand {
    Pull {
        image: String,
    },
    Build {
        tag: String,
        asset_root: PathBuf,
        dockerfile_path: PathBuf,
    },
    Inspect {
        image: String,
    },
}

impl SetupImageCommand {
    fn docker_args(&self) -> Vec<String> {
        match self {
            Self::Pull { image } => vec!["image".into(), "pull".into(), image.clone()],
            Self::Build { tag, .. } => vec![
                "build".into(),
                // A local setup app is single-platform. Without this,
                // recent BuildKit releases export an OCI index whose `.Id`
                // differs from the platform image ID Docker stores on the
                // container. The ensure primitive must compare one stable
                // image identity across create and later reuse.
                "--provenance=false".into(),
                "--tag".into(),
                tag.clone(),
                "--file".into(),
                // The Dockerfile and context are relative to a validated,
                // private current directory; neither is caller argv.
                "Dockerfile".into(),
                ".".into(),
            ],
            Self::Inspect { image } => vec![
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                image.clone(),
            ],
        }
    }
}

/// Testable bounded engine boundary for setup image preparation.
///
/// Product code uses the [`DockerEngine`] implementation.  Test doubles see
/// the finite [`SetupImageCommand`] enum instead of untrusted argv.
pub trait SetupImageEngine {
    type StreamFuture<'a>: Future<Output = Result<CommandResult, CommandError>> + Send + 'a
    where
        Self: 'a;

    fn stream<'a>(
        &'a self,
        command: SetupImageCommand,
        options: RunOptions,
        cancellation: &'a CancellationToken,
        events: &'a Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a>;
}

impl SetupImageEngine for DockerEngine {
    type StreamFuture<'a> =
        Pin<Box<dyn Future<Output = Result<CommandResult, CommandError>> + Send + 'a>>;

    fn stream<'a>(
        &'a self,
        command: SetupImageCommand,
        options: RunOptions,
        cancellation: &'a CancellationToken,
        events: &'a Sender<EngineEvent>,
    ) -> Self::StreamFuture<'a> {
        let mut engine = self.with_args(command.docker_args());
        if let SetupImageCommand::Build { asset_root, .. } = command {
            engine = engine.current_dir(asset_root);
        }
        Box::pin(async move { engine.stream(options, Some(cancellation), events).await })
    }
}

/// Whether an image was pulled from an immutable reference or built locally.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparedImageKind {
    PinnedImage { image: String },
    InlineDockerfile { tag: String },
}

/// A successful, inspected image-preparation receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedImage {
    /// Exact document receipt used to derive this operation.
    pub setup_content_sha256: String,
    /// The image source that was prepared.
    pub kind: PreparedImageKind,
    /// Reference inspected after the pull or build.
    pub reference: String,
    /// Observed Docker image ID (`sha256:<64 lowercase hex>`).
    pub observed_identity: String,
}

/// Why image preparation failed without escalating into container execution.
#[derive(Debug)]
pub enum SetupPrepareError {
    InvalidPlan(&'static str),
    AssetIntegrity(crate::SetupMaterializeError),
    Cancelled,
    Deadline,
    Transport(CommandError),
    ActionFailed {
        action: &'static str,
        detail: String,
    },
    MissingIdentity {
        reference: String,
        observed: String,
    },
}

impl std::fmt::Display for SetupPrepareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPlan(reason) => write!(formatter, "invalid setup image plan: {reason}"),
            Self::AssetIntegrity(error) => write!(formatter, "setup build assets changed: {error}"),
            Self::Cancelled => formatter.write_str("setup image preparation was cancelled"),
            Self::Deadline => formatter.write_str("setup image preparation exceeded its deadline"),
            Self::Transport(error) => write!(formatter, "setup image engine transport: {error}"),
            Self::ActionFailed { action, detail } => {
                write!(formatter, "Docker {action} failed: {detail}")
            }
            Self::MissingIdentity {
                reference,
                observed,
            } => write!(
                formatter,
                "Docker did not return a stable image identity for {reference}: {observed:?}"
            ),
        }
    }
}

impl std::error::Error for SetupPrepareError {}

impl From<CommandError> for SetupPrepareError {
    fn from(error: CommandError) -> Self {
        match error {
            CommandError::Cancelled { .. } => Self::Cancelled,
            CommandError::Deadline { .. } => Self::Deadline,
            other => Self::Transport(other),
        }
    }
}

/// Pull or build exactly the application image described by `plan`.
///
/// The deadline and output cap apply to the full prepare-plus-inspect sequence,
/// rather than being reset for each Docker child.  `events` is forwarded to the
/// engine's bounded stream unchanged; a slow or closed consumer aborts and
/// reaps the direct Docker client through `bosn-engine`/`kernal-api`.
pub async fn prepare_setup_image<E: SetupImageEngine>(
    engine: &E,
    plan: &SetupPlan,
    options: RunOptions,
    cancellation: &CancellationToken,
    events: &Sender<EngineEvent>,
) -> Result<PreparedImage, SetupPrepareError> {
    let prepared = validate_plan(plan)?;
    if cancellation.is_cancelled() {
        return Err(SetupPrepareError::Cancelled);
    }
    let deadline = Deadline::after(options.deadline);
    let mut remaining_output = options.output_limit;

    let action = match &prepared {
        ValidatedPlan::Pinned { image } => SetupImageCommand::Pull {
            image: image.clone(),
        },
        ValidatedPlan::Inline {
            tag, asset_root, ..
        } => SetupImageCommand::Build {
            tag: tag.clone(),
            asset_root: asset_root.clone(),
            dockerfile_path: asset_root.join("Dockerfile"),
        },
    };
    let action_name = match &action {
        SetupImageCommand::Pull { .. } => "image pull",
        SetupImageCommand::Build { .. } => "build",
        SetupImageCommand::Inspect { .. } => unreachable!("prepare action is never inspect"),
    };
    let action_result = stream_command(
        engine,
        action,
        deadline,
        &mut remaining_output,
        cancellation,
        events,
    )
    .await?;
    if !action_result.ok() {
        return Err(SetupPrepareError::ActionFailed {
            action: action_name,
            detail: failure_detail(&action_result),
        });
    }

    let reference = prepared.reference().to_owned();
    let inspected = stream_command(
        engine,
        SetupImageCommand::Inspect {
            image: reference.clone(),
        },
        deadline,
        &mut remaining_output,
        cancellation,
        events,
    )
    .await?;
    if !inspected.ok() {
        return Err(SetupPrepareError::ActionFailed {
            action: "image inspect",
            detail: failure_detail(&inspected),
        });
    }
    let observed_identity =
        stable_identity(&inspected.stdout).ok_or_else(|| SetupPrepareError::MissingIdentity {
            reference: reference.clone(),
            observed: String::from_utf8_lossy(&inspected.stdout).trim().to_owned(),
        })?;
    Ok(PreparedImage {
        setup_content_sha256: plan.content_sha256.clone(),
        kind: prepared.kind(),
        reference,
        observed_identity,
    })
}

#[derive(Clone, Debug)]
enum ValidatedPlan {
    Pinned { image: String },
    Inline { tag: String, asset_root: PathBuf },
}

impl ValidatedPlan {
    fn reference(&self) -> &str {
        match self {
            Self::Pinned { image } => image,
            Self::Inline { tag, .. } => tag,
        }
    }

    fn kind(&self) -> PreparedImageKind {
        match self {
            Self::Pinned { image } => PreparedImageKind::PinnedImage {
                image: image.clone(),
            },
            Self::Inline { tag, .. } => PreparedImageKind::InlineDockerfile { tag: tag.clone() },
        }
    }
}

fn validate_plan(plan: &SetupPlan) -> Result<ValidatedPlan, SetupPrepareError> {
    if !valid_hash(&plan.content_sha256) {
        return Err(SetupPrepareError::InvalidPlan(
            "content hash is not canonical sha256 hex",
        ));
    }
    match &plan.app_source {
        SetupPlanAppSource::PinnedImage { image } => {
            if plan.asset_root.is_some() || !valid_pinned_image(image) {
                return Err(SetupPrepareError::InvalidPlan(
                    "pinned image is not canonical",
                ));
            }
            Ok(ValidatedPlan::Pinned {
                image: image.clone(),
            })
        }
        SetupPlanAppSource::InlineDockerfile { dockerfile_path } => {
            let Some(asset_root) = &plan.asset_root else {
                return Err(SetupPrepareError::InvalidPlan(
                    "inline Dockerfile has no asset root",
                ));
            };
            if dockerfile_path != &asset_root.join("Dockerfile") {
                return Err(SetupPrepareError::InvalidPlan(
                    "inline Dockerfile is not the root Dockerfile",
                ));
            }
            verify_materialized_assets(&plan.content_sha256, asset_root)
                .map_err(SetupPrepareError::AssetIntegrity)?;
            Ok(ValidatedPlan::Inline {
                tag: format!("bosn-setup:{}", plan.content_sha256),
                asset_root: asset_root.clone(),
            })
        }
    }
}

async fn stream_command<E: SetupImageEngine>(
    engine: &E,
    command: SetupImageCommand,
    deadline: Deadline,
    remaining_output: &mut usize,
    cancellation: &CancellationToken,
    events: &Sender<EngineEvent>,
) -> Result<CommandResult, SetupPrepareError> {
    if cancellation.is_cancelled() {
        return Err(SetupPrepareError::Cancelled);
    }
    let remaining = deadline.remaining();
    if remaining.is_zero() {
        return Err(SetupPrepareError::Deadline);
    }
    let result = engine
        .stream(
            command,
            RunOptions::streaming(remaining, *remaining_output),
            cancellation,
            events,
        )
        .await?;
    let used = result.stdout.len().saturating_add(result.stderr.len());
    if used > *remaining_output {
        return Err(SetupPrepareError::Transport(CommandError::OutputLimit {
            limit: *remaining_output,
            reaped_pid: None,
            cleanup: None,
        }));
    }
    *remaining_output -= used;
    Ok(result)
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_pinned_image(image: &str) -> bool {
    let Some((name, digest)) = image.rsplit_once("@sha256:") else {
        return false;
    };
    image.len() <= 512
        && !name.is_empty()
        && !name.starts_with('-')
        && !name.contains('@')
        && !image
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        && valid_hash(digest)
}

fn stable_identity(bytes: &[u8]) -> Option<String> {
    let identity = std::str::from_utf8(bytes).ok()?.trim();
    (identity.len() == 71 && identity.starts_with("sha256:") && valid_hash(&identity[7..]))
        .then(|| identity.to_owned())
}

fn failure_detail(result: &CommandResult) -> String {
    let detail = if result.stderr.is_empty() {
        &result.stdout
    } else {
        &result.stderr
    };
    String::from_utf8_lossy(detail)
        .trim()
        .chars()
        .take(4096)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::{Ready, ready},
        sync::Mutex,
        time::Duration,
    };

    use super::*;
    use kernal_api::{
        async_engine::{CancellationSource, RuntimeBuilder, channel},
        hash::sha256_bytes,
        platform::ipc,
    };

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const IDENTITY: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[derive(Default)]
    struct FakeEngine {
        calls: Mutex<Vec<SetupImageCommand>>,
        results: Mutex<VecDeque<Result<CommandResult, CommandError>>>,
    }

    impl FakeEngine {
        fn with_results(
            results: impl IntoIterator<Item = Result<CommandResult, CommandError>>,
        ) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                results: Mutex::new(results.into_iter().collect()),
            }
        }
    }

    impl SetupImageEngine for FakeEngine {
        type StreamFuture<'a> = Ready<Result<CommandResult, CommandError>>;

        fn stream<'a>(
            &'a self,
            command: SetupImageCommand,
            _options: RunOptions,
            _cancellation: &'a CancellationToken,
            _events: &'a Sender<EngineEvent>,
        ) -> Self::StreamFuture<'a> {
            self.calls.lock().unwrap().push(command);
            ready(
                self.results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("configured command result"),
            )
        }
    }

    fn runtime() -> kernal_api::async_engine::Runtime {
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn result(stdout: impl Into<Vec<u8>>) -> Result<CommandResult, CommandError> {
        Ok(CommandResult {
            exit_code: 0,
            stdout: stdout.into(),
            stderr: Vec::new(),
        })
    }

    fn pinned(image: &str) -> SetupPlan {
        SetupPlan {
            source_kind: crate::SetupSourceKind::LocalFile,
            content_sha256: HASH.into(),
            schema_version: 1,
            workspace_root: PathBuf::from("/unused"),
            asset_root: None,
            task_names: Vec::new(),
            app: bosn_core::SetupApp {
                source: bosn_core::SetupSource::PinnedImage(image.into()),
                environment: Default::default(),
                workdir: None,
                command: None,
                mounts: Vec::new(),
            },
            tasks: Default::default(),
            app_source: SetupPlanAppSource::PinnedImage {
                image: image.into(),
            },
            named_volumes: Vec::new(),
            tmpfs: Vec::new(),
            macos_guest: None,
        }
    }

    fn inline_plan() -> (tempfile::TempDir, SetupPlan) {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        ipc::ensure_owner_private_directory(&workspace).unwrap();
        let source = "version = 1\n[app]\ndockerfile = 'FROM scratch'\n[[file]]\npath = 'hello.txt'\ncontent = 'hello'\n";
        let content_sha256 = sha256_bytes(source.as_bytes()).to_hex();
        let resolved = crate::ResolvedSetupDocument {
            document: bosn_core::parse_setup_document_toml(source).unwrap(),
            provenance: crate::SetupProvenance {
                requested_locator: "setup.toml".into(),
                resolved_locator: None,
                content_sha256: content_sha256.clone(),
                schema_version: 1,
                fetched_at_unix_seconds: 0,
                source_kind: crate::SetupSourceKind::LocalFile,
            },
        };
        let materialized = crate::SetupAssetStore::under_state_dir(&state)
            .unwrap()
            .materialize(&resolved, &workspace)
            .unwrap();
        let root = materialized.asset_root().unwrap().to_path_buf();
        (
            temp,
            SetupPlan {
                source_kind: crate::SetupSourceKind::LocalFile,
                content_sha256,
                schema_version: 1,
                workspace_root: materialized.workspace_root().to_path_buf(),
                asset_root: Some(root.clone()),
                task_names: Vec::new(),
                app: materialized.app().clone(),
                tasks: materialized.tasks().clone(),
                app_source: SetupPlanAppSource::InlineDockerfile {
                    dockerfile_path: root.join("Dockerfile"),
                },
                named_volumes: Vec::new(),
                tmpfs: Vec::new(),
                macos_guest: None,
            },
        )
    }

    fn run<E: SetupImageEngine>(
        engine: &E,
        plan: &SetupPlan,
        cancel: &CancellationToken,
    ) -> Result<PreparedImage, SetupPrepareError> {
        let (events, _receiver) = channel(8);
        runtime().run(prepare_setup_image(
            engine,
            plan,
            RunOptions::streaming(Duration::from_secs(1), 4096),
            cancel,
            &events,
        ))
    }

    #[test]
    fn pinned_pull_uses_only_the_exact_immutable_reference_then_inspects_it() {
        let image = format!("registry.example/team/app@sha256:{HASH}");
        let engine =
            FakeEngine::with_results([result(Vec::new()), result(format!("{IDENTITY}\n"))]);
        let cancel = CancellationSource::new();
        let prepared = run(&engine, &pinned(&image), &cancel.token()).unwrap();
        assert_eq!(prepared.reference, image);
        assert_eq!(prepared.observed_identity, IDENTITY);
        assert_eq!(
            SetupImageCommand::Pull {
                image: image.clone()
            }
            .docker_args(),
            vec!["image".into(), "pull".into(), image.clone()]
        );
        assert_eq!(
            *engine.calls.lock().unwrap(),
            vec![
                SetupImageCommand::Pull {
                    image: image.clone()
                },
                SetupImageCommand::Inspect { image },
            ]
        );
    }

    #[test]
    fn cancellation_and_engine_failures_propagate_without_later_commands() {
        let image = format!("example.invalid/app@sha256:{HASH}");
        let cancelled_engine = FakeEngine::default();
        let cancelled = CancellationSource::new();
        cancelled.cancel();
        assert!(matches!(
            run(&cancelled_engine, &pinned(&image), &cancelled.token()),
            Err(SetupPrepareError::Cancelled)
        ));
        assert!(cancelled_engine.calls.lock().unwrap().is_empty());

        let deadline_engine = FakeEngine::with_results([Err(CommandError::Deadline {
            reaped_pid: None,
            cleanup: None,
        })]);
        let active = CancellationSource::new();
        assert!(matches!(
            run(&deadline_engine, &pinned(&image), &active.token()),
            Err(SetupPrepareError::Deadline)
        ));
        assert_eq!(deadline_engine.calls.lock().unwrap().len(), 1);

        let output_engine = FakeEngine::with_results([Err(CommandError::OutputLimit {
            limit: 1,
            reaped_pid: None,
            cleanup: None,
        })]);
        assert!(matches!(
            run(&output_engine, &pinned(&image), &active.token()),
            Err(SetupPrepareError::Transport(
                CommandError::OutputLimit { .. }
            ))
        ));
    }

    #[test]
    fn malformed_image_and_missing_identity_fail_closed_before_or_after_engine() {
        let engine = FakeEngine::default();
        let cancel = CancellationSource::new();
        let malformed = format!("-not-an-image@sha256:{HASH}");
        assert!(matches!(
            run(&engine, &pinned(&malformed), &cancel.token()),
            Err(SetupPrepareError::InvalidPlan(_))
        ));
        assert!(engine.calls.lock().unwrap().is_empty());

        let image = format!("example.invalid/app@sha256:{HASH}");
        let missing = FakeEngine::with_results([result(Vec::new()), result("not-a-digest")]);
        assert!(matches!(
            run(&missing, &pinned(&image), &cancel.token()),
            Err(SetupPrepareError::MissingIdentity { .. })
        ));
    }

    #[test]
    fn inline_build_uses_only_validated_private_root_and_content_addressed_tag() {
        let (_temp, plan) = inline_plan();
        let root = plan.asset_root.clone().unwrap();
        let tag = format!("bosn-setup:{}", plan.content_sha256);
        let engine =
            FakeEngine::with_results([result(Vec::new()), result(format!("{IDENTITY}\n"))]);
        let cancel = CancellationSource::new();
        let prepared = run(&engine, &plan, &cancel.token()).unwrap();
        assert_eq!(prepared.reference, tag);
        assert_eq!(
            SetupImageCommand::Build {
                tag: prepared.reference.clone(),
                asset_root: root.clone(),
                dockerfile_path: root.join("Dockerfile"),
            }
            .docker_args(),
            vec![
                "build".into(),
                "--provenance=false".into(),
                "--tag".into(),
                prepared.reference.clone(),
                "--file".into(),
                "Dockerfile".into(),
                ".".into(),
            ]
        );
        assert_eq!(
            *engine.calls.lock().unwrap(),
            vec![
                SetupImageCommand::Build {
                    tag: prepared.reference.clone(),
                    asset_root: root.clone(),
                    dockerfile_path: root.join("Dockerfile"),
                },
                SetupImageCommand::Inspect {
                    image: prepared.reference,
                },
            ]
        );
    }

    #[test]
    fn tampered_inline_asset_or_path_is_rejected_before_engine_launch() {
        let (_temp, mut plan) = inline_plan();
        let root = plan.asset_root.clone().unwrap();
        std::fs::write(root.join("Dockerfile"), "FROM injected").unwrap();
        let engine = FakeEngine::default();
        let cancel = CancellationSource::new();
        assert!(matches!(
            run(&engine, &plan, &cancel.token()),
            Err(SetupPrepareError::AssetIntegrity(_))
        ));
        assert!(engine.calls.lock().unwrap().is_empty());

        plan.app_source = SetupPlanAppSource::InlineDockerfile {
            dockerfile_path: PathBuf::from("-not-a-dockerfile"),
        };
        assert!(matches!(
            run(&engine, &plan, &cancel.token()),
            Err(SetupPrepareError::InvalidPlan(_))
        ));
        assert!(engine.calls.lock().unwrap().is_empty());
    }
}
