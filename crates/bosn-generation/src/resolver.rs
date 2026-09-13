//! Engine-backed immutable image receipts. Read-only lookup never pulls.

use std::collections::BTreeMap;
use std::time::Duration;

use bosn_engine::{CommandError, CommandResult, DockerEngine, EngineEvent, RunOptions};
use kernal_api::async_engine::{CancellationToken, Deadline, Sender};

use crate::ExternalImageIdentity;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionPolicy {
    ReadOnly,
    PullIfMissing,
}
#[derive(Debug)]
pub enum ResolutionError {
    Transport(CommandError),
    InvalidInput {
        reference: String,
        platform: Option<String>,
    },
    Missing {
        reference: String,
        platform: Option<String>,
    },
    InspectFailed {
        reference: String,
        detail: String,
    },
    Malformed {
        reference: String,
        identity: String,
    },
    PullFailed {
        reference: String,
        detail: String,
    },
    Deadline,
    AutomaticPlatform(String),
}

/// Expand Docker's automatic platform variables using the selected Docker
/// daemon, rather than assuming the host platform.  The same expansion is
/// applied to image references and explicit `FROM --platform` selectors.
pub async fn expand_automatic_platforms_streaming(
    engine: &DockerEngine,
    required: &[ExternalImageIdentity],
    options: RunOptions,
    cancellation: &CancellationToken,
    events: &Sender<EngineEvent>,
) -> Result<Vec<ExternalImageIdentity>, ResolutionError> {
    let needs_platform = required.iter().any(|image| {
        image.reference.contains('$')
            || image
                .platform
                .as_ref()
                .is_some_and(|platform| platform.contains('$'))
    });
    if !needs_platform {
        return Ok(required.to_vec());
    }
    let deadline = Deadline::after(options.deadline);
    let mut remaining = options.output_limit;
    let version = stream_command(
        engine,
        vec![
            "version".into(),
            "--format".into(),
            "{{.Server.Os}}/{{.Server.Arch}}".into(),
        ],
        deadline,
        &mut remaining,
        cancellation,
        events,
    )
    .await?;
    if !version.ok() {
        return Err(ResolutionError::AutomaticPlatform(
            String::from_utf8_lossy(&version.stderr).trim().to_owned(),
        ));
    }
    let platform = String::from_utf8_lossy(&version.stdout).trim().to_owned();
    let mut parts = platform.split('/');
    let (Some(os), Some(arch)) = (parts.next(), parts.next()) else {
        return Err(ResolutionError::AutomaticPlatform(format!(
            "invalid daemon platform {platform:?}"
        )));
    };
    if os.is_empty() || arch.is_empty() || parts.clone().any(str::is_empty) {
        return Err(ResolutionError::AutomaticPlatform(format!(
            "invalid daemon platform {platform:?}"
        )));
    }
    let os = os.to_owned();
    let arch = arch.to_owned();
    let variant = parts.collect::<Vec<_>>().join("/");
    let values = BTreeMap::from([
        ("BUILDPLATFORM", platform.clone()),
        ("TARGETPLATFORM", platform),
        ("BUILDOS", os.clone()),
        ("TARGETOS", os),
        ("BUILDARCH", arch.clone()),
        ("TARGETARCH", arch),
        ("BUILDVARIANT", variant.clone()),
        ("TARGETVARIANT", variant),
    ]);
    required
        .iter()
        .map(|image| {
            Ok(ExternalImageIdentity {
                reference: expand_platform_value(&image.reference, &values)?,
                platform: image
                    .platform
                    .as_ref()
                    .map(|v| expand_platform_value(v, &values))
                    .transpose()?,
                identity: image.identity.clone(),
            })
        })
        .collect()
}

/// Resolve complete immutable receipts for requirements derived from a stack.
/// This is the single composition point for daemon-platform expansion and
/// image inspection/pull policy; callers pass its output directly to
/// `final_generation`/`stack_generation` rather than comparing unexpanded
/// Dockerfile requirements to expanded receipts.
pub async fn resolve_required_images_streaming(
    engine: &DockerEngine,
    required: &[ExternalImageIdentity],
    policy: ResolutionPolicy,
    options: RunOptions,
    cancellation: &CancellationToken,
    events: &Sender<EngineEvent>,
) -> Result<Vec<ExternalImageIdentity>, ResolutionError> {
    let expanded =
        expand_automatic_platforms_streaming(engine, required, options, cancellation, events)
            .await?;
    resolve_images_streaming(engine, &expanded, policy, options, cancellation, events).await
}

fn expand_platform_value(
    value: &str,
    values: &BTreeMap<&str, String>,
) -> Result<String, ResolutionError> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        rest = &rest[index + 1..];
        let (name, consumed) = if let Some(rest) = rest.strip_prefix('{') {
            let Some(end) = rest.find('}') else {
                return Err(ResolutionError::AutomaticPlatform(
                    "unterminated automatic platform argument".into(),
                ));
            };
            (&rest[..end], end + 2)
        } else {
            let end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            (&rest[..end], end)
        };
        if name.is_empty() {
            return Err(ResolutionError::AutomaticPlatform(
                "empty automatic platform argument".into(),
            ));
        }
        let value = values.get(name).ok_or_else(|| {
            ResolutionError::AutomaticPlatform(format!(
                "unknown automatic platform argument {name:?}"
            ))
        })?;
        out.push_str(value);
        rest = &rest[consumed..];
    }
    out.push_str(rest);
    Ok(out)
}
impl std::fmt::Display for ResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ResolutionError {}

pub fn resolve_images(
    engine: &DockerEngine,
    required: &[ExternalImageIdentity],
    policy: ResolutionPolicy,
    options: RunOptions,
) -> Result<Vec<ExternalImageIdentity>, ResolutionError> {
    // A malformed request is rejected as a unit: callers must not see an
    // earlier PullIfMissing mutate state merely because a later entry was
    // malformed.
    for image in required {
        if invalid(image) {
            return Err(ResolutionError::InvalidInput {
                reference: image.reference.clone(),
                platform: image.platform.clone(),
            });
        }
    }
    let mut out = Vec::new();
    for required in required {
        if out
            .iter()
            .any(|image: &ExternalImageIdentity| same(image, required))
        {
            continue;
        }
        let initial = inspect(engine, required, options).map_err(ResolutionError::Transport)?;
        if initial.ok() {
            out.push(receipt(required, &initial.stdout)?);
            continue;
        }
        let detail = String::from_utf8_lossy(&initial.stderr).into_owned();
        if !missing(&detail, &required.reference) {
            return Err(ResolutionError::InspectFailed {
                reference: required.reference.clone(),
                detail,
            });
        }
        if policy == ResolutionPolicy::ReadOnly {
            out.push(ExternalImageIdentity {
                reference: required.reference.clone(),
                platform: required.platform.clone(),
                identity: None,
            });
            continue;
        }
        let mut pull = vec!["pull".into()];
        if let Some(platform) = &required.platform {
            pull.extend(["--platform".into(), platform.clone()]);
        }
        pull.push(required.reference.clone());
        let pulled = engine
            .with_args(pull)
            .capture(options)
            .map_err(ResolutionError::Transport)?;
        if !pulled.ok() {
            return Err(ResolutionError::PullFailed {
                reference: required.reference.clone(),
                detail: String::from_utf8_lossy(&pulled.stderr).into_owned(),
            });
        }
        let after = inspect(engine, required, options).map_err(ResolutionError::Transport)?;
        if after.ok() {
            out.push(receipt(required, &after.stdout)?);
            continue;
        }
        let detail = String::from_utf8_lossy(&after.stderr).into_owned();
        if missing(&detail, &required.reference) {
            return Err(ResolutionError::Missing {
                reference: required.reference.clone(),
                platform: required.platform.clone(),
            });
        }
        return Err(ResolutionError::InspectFailed {
            reference: required.reference.clone(),
            detail,
        });
    }
    Ok(out)
}

/// Resolve external images without blocking the kernel runtime.
///
/// Each Docker child is streamed through the kernel transport, so cancellation
/// reaps the direct client before this future returns. `options.deadline` and
/// `options.output_limit` are budgets for the entire resolution operation,
/// rather than budgets repeated for every inspect/pull/reinspect child.
pub async fn resolve_images_streaming(
    engine: &DockerEngine,
    required: &[ExternalImageIdentity],
    policy: ResolutionPolicy,
    options: RunOptions,
    cancellation: &CancellationToken,
    events: &Sender<EngineEvent>,
) -> Result<Vec<ExternalImageIdentity>, ResolutionError> {
    const MAX_IMAGES: usize = 256;
    const MAX_VALUE_BYTES: usize = 4096;
    if required.len() > MAX_IMAGES {
        return Err(ResolutionError::InvalidInput {
            reference: "too many external images".into(),
            platform: None,
        });
    }
    // Validate the whole request before a later malformed entry can cause an
    // earlier PullIfMissing entry to mutate the image store.
    for image in required {
        if invalid(image)
            || image.reference.len() > MAX_VALUE_BYTES
            || image
                .platform
                .as_ref()
                .is_some_and(|p| p.len() > MAX_VALUE_BYTES)
        {
            return Err(ResolutionError::InvalidInput {
                reference: image.reference.clone(),
                platform: image.platform.clone(),
            });
        }
    }
    if cancellation.is_cancelled() {
        return Err(ResolutionError::Transport(CommandError::Cancelled {
            reaped_pid: None,
            cleanup: None,
        }));
    }

    let deadline = Deadline::after(options.deadline);
    let mut remaining_output = options.output_limit;
    let mut out = Vec::new();
    for image in required {
        if out
            .iter()
            .any(|seen: &ExternalImageIdentity| same(seen, image))
        {
            continue;
        }
        let initial = stream_inspect(
            engine,
            image,
            deadline,
            &mut remaining_output,
            cancellation,
            events,
        )
        .await?;
        if initial.ok() {
            out.push(receipt(image, &initial.stdout)?);
            continue;
        }
        let detail = String::from_utf8_lossy(&initial.stderr).into_owned();
        if !missing_or_platform_absent(&detail, image) {
            return Err(ResolutionError::InspectFailed {
                reference: image.reference.clone(),
                detail,
            });
        }
        if policy == ResolutionPolicy::ReadOnly {
            out.push(ExternalImageIdentity {
                reference: image.reference.clone(),
                platform: image.platform.clone(),
                identity: None,
            });
            continue;
        }
        let mut args = vec!["pull".into()];
        if let Some(platform) = &image.platform {
            args.extend(["--platform".into(), platform.clone()]);
        }
        args.push(image.reference.clone());
        let pulled = stream_command(
            engine,
            args,
            deadline,
            &mut remaining_output,
            cancellation,
            events,
        )
        .await?;
        if !pulled.ok() {
            return Err(ResolutionError::PullFailed {
                reference: image.reference.clone(),
                detail: String::from_utf8_lossy(&pulled.stderr).into_owned(),
            });
        }
        let after = stream_inspect(
            engine,
            image,
            deadline,
            &mut remaining_output,
            cancellation,
            events,
        )
        .await?;
        if after.ok() {
            out.push(receipt(image, &after.stdout)?);
            continue;
        }
        let detail = String::from_utf8_lossy(&after.stderr).into_owned();
        if missing_or_platform_absent(&detail, image) {
            return Err(ResolutionError::Missing {
                reference: image.reference.clone(),
                platform: image.platform.clone(),
            });
        }
        return Err(ResolutionError::InspectFailed {
            reference: image.reference.clone(),
            detail,
        });
    }
    Ok(out)
}

async fn stream_inspect(
    engine: &DockerEngine,
    image: &ExternalImageIdentity,
    deadline: Deadline,
    remaining_output: &mut usize,
    cancellation: &CancellationToken,
    events: &Sender<EngineEvent>,
) -> Result<CommandResult, ResolutionError> {
    let mut args = vec!["image".into(), "inspect".into()];
    if let Some(platform) = &image.platform {
        args.extend(["--platform".into(), platform.clone()]);
    }
    args.extend(["--format".into(), "{{.Id}}".into(), image.reference.clone()]);
    stream_command(
        engine,
        args,
        deadline,
        remaining_output,
        cancellation,
        events,
    )
    .await
}

async fn stream_command(
    engine: &DockerEngine,
    args: Vec<String>,
    deadline: Deadline,
    remaining_output: &mut usize,
    cancellation: &CancellationToken,
    events: &Sender<EngineEvent>,
) -> Result<CommandResult, ResolutionError> {
    let remaining = deadline.remaining();
    if remaining.is_zero() {
        return Err(ResolutionError::Deadline);
    }
    let result = engine
        .with_args(args)
        .stream(
            RunOptions::streaming(remaining, *remaining_output),
            Some(cancellation),
            events,
        )
        .await
        .map_err(ResolutionError::Transport)?;
    let used = result.stdout.len().saturating_add(result.stderr.len());
    if used > *remaining_output {
        return Err(ResolutionError::Deadline);
    }
    *remaining_output -= used;
    Ok(result)
}

fn inspect(
    engine: &DockerEngine,
    required: &ExternalImageIdentity,
    options: RunOptions,
) -> Result<CommandResult, CommandError> {
    let mut args = vec!["image".into(), "inspect".into()];
    if let Some(platform) = &required.platform {
        args.extend(["--platform".into(), platform.clone()]);
    }
    args.extend([
        "--format".into(),
        "{{.Id}}".into(),
        required.reference.clone(),
    ]);
    engine.with_args(args).capture(options)
}
fn receipt(
    required: &ExternalImageIdentity,
    bytes: &[u8],
) -> Result<ExternalImageIdentity, ResolutionError> {
    Ok(ExternalImageIdentity {
        reference: required.reference.clone(),
        platform: required.platform.clone(),
        identity: Some(identity(required, bytes)?),
    })
}
fn same(left: &ExternalImageIdentity, right: &ExternalImageIdentity) -> bool {
    left.reference == right.reference && left.platform == right.platform
}
fn invalid(image: &ExternalImageIdentity) -> bool {
    image.reference.is_empty()
        || image.reference.starts_with('-')
        || image.reference.contains('\0')
        || image.platform.as_ref().is_some_and(|platform| {
            platform.is_empty() || platform.starts_with('-') || platform.contains('\0')
        })
}
fn missing(detail: &str, reference: &str) -> bool {
    let detail = detail.trim();
    detail == format!("Error response from daemon: No such image: {reference}")
        || detail == format!("Error response from daemon: No such object: {reference}")
        || detail == format!("No such image: {reference}")
        || detail == format!("No such object: {reference}")
}
fn missing_or_platform_absent(detail: &str, image: &ExternalImageIdentity) -> bool {
    if missing(detail, &image.reference) {
        return true;
    }
    let Some(platform) = &image.platform else {
        return false;
    };
    detail.trim()
        == format!(
            "Error response from daemon: image with reference {} was found but does not provide the specified platform ({platform})",
            image.reference
        )
}
fn identity(required: &ExternalImageIdentity, bytes: &[u8]) -> Result<String, ResolutionError> {
    let identity = String::from_utf8_lossy(bytes).trim().to_owned();
    if identity.len() == 71
        && identity.starts_with("sha256:")
        && identity[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(identity)
    } else {
        Err(ResolutionError::Malformed {
            reference: required.reference.clone(),
            identity,
        })
    }
}
pub const DEFAULT_OPTIONS: RunOptions = RunOptions {
    deadline: Duration::from_secs(30),
    output_limit: 64 * 1024,
};
