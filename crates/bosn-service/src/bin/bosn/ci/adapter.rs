//! `bosn ci plan --adapter` (soldr#3345): the declared adapter plan, read-only.
//! The one implementation behind both `bosn ci plan --adapter` and its
//! deprecated spelling `bosn act plan --adapter` (#375); each caller only
//! chooses how a refusal is reported.

use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde_json::{Value, json};

use crate::bounded_output::bounded_output;

/// Whether a `plan` invocation asks for the adapter plan.
pub fn requested(arguments: &[OsString]) -> bool {
    arguments.iter().any(|flag| flag == "--adapter")
}

// This entry point only describes operator-requested plans. CLI metadata is
// unverified, unlike authenticated producer observations; it confers no runtime
// authorization, merged-candidate proof, binary-pin proof, or graph acceptance.
fn adapter_options(
    arguments: impl IntoIterator<Item = impl Into<String>>,
) -> Result<std::collections::BTreeMap<String, String>, &'static str> {
    let mut arguments = arguments.into_iter().map(Into::into);
    let mut values = std::collections::BTreeMap::new();
    while let Some(flag) = arguments.next() {
        if !matches!(
            flag.as_str(),
            "--adapter"
                | "--workspace"
                | "--event"
                | "--mode"
                | "--sha"
                | "--base-sha"
                | "--repo-owner"
                | "--repo-name"
                | "--pr-number"
                | "--head-owner"
                | "--head-name"
                | "--head-ref"
                | "--base-ref"
                | "--author-login"
                | "--json"
        ) || values.contains_key(&flag)
        {
            return Err("invalid or duplicate adapter plan option");
        }
        let value = if flag == "--json" {
            String::new()
        } else {
            arguments
                .next()
                .filter(|value| !value.is_empty() && !value.starts_with("--"))
                .ok_or("missing adapter plan option value")?
        };
        values.insert(flag, value);
    }
    Ok(values)
}
fn option<'a>(
    values: &'a std::collections::BTreeMap<String, String>,
    name: &str,
) -> Result<&'a str, &'static str> {
    values
        .get(name)
        .map(String::as_str)
        .ok_or("missing required adapter plan option")
}
fn declared_adapter_plan(
    adapter: &bosn_core::act::ActAdapterV1,
    values: &std::collections::BTreeMap<String, String>,
    actual_sha: &str,
) -> Result<Value, &'static str> {
    use bosn_core::act::{
        ActEvent, ActMode, ActPullRequestIdentity, ActSourceContext, RepositoryIdentity,
        resolve_act_event,
    };
    let event: ActEvent = serde_json::from_value(json!(option(values, "--event")?))
        .map_err(|_| "invalid adapter event")?;
    let mode: ActMode = serde_json::from_value(json!(option(values, "--mode")?))
        .map_err(|_| "invalid adapter mode")?;
    if option(values, "--sha")? != actual_sha {
        return Err("requested SHA does not match workspace HEAD");
    }
    let repository = RepositoryIdentity {
        owner: option(values, "--repo-owner")?.into(),
        name: option(values, "--repo-name")?.into(),
    };
    let base_sha = values.get("--base-sha").cloned();
    let pr_flags = [
        "--pr-number",
        "--head-owner",
        "--head-name",
        "--head-ref",
        "--base-ref",
        "--author-login",
    ];
    let pull_request = if event == ActEvent::PullRequest {
        Some(ActPullRequestIdentity {
            number: option(values, "--pr-number")?
                .parse()
                .map_err(|_| "invalid PR number")?,
            head_sha: actual_sha.into(),
            base_sha: base_sha.clone().ok_or("PR requires --base-sha")?,
            head_repository: RepositoryIdentity {
                owner: option(values, "--head-owner")?.into(),
                name: option(values, "--head-name")?.into(),
            },
            base_repository: repository.clone(),
            head_ref: option(values, "--head-ref")?.into(),
            base_ref: option(values, "--base-ref")?.into(),
            author_login: option(values, "--author-login")?.into(),
        })
    } else {
        if pr_flags.iter().any(|flag| values.contains_key(*flag)) {
            return Err("PR metadata is invalid for this event");
        }
        None
    };
    let source = ActSourceContext {
        repository,
        current_sha: actual_sha.into(),
        base_sha,
        pull_request,
    };
    let plan = resolve_act_event(adapter, event, mode, &source).map_err(|error| error.0)?;
    Ok(
        json!({"action":"act_adapter_plan", "schema_version":1, "plan":plan, "source_metadata_verified":false, "source_metadata_scope":"operator-supplied repository and PR metadata; only checkout HEAD and cleanliness observed", "merged_candidate_verified":false, "fleet_pin_verified":false, "binary_pin_verified":false, "executable":false, "docker_resources_tracked":false, "reason":"declarations require authenticated metadata, actual graph matching, and runtime ownership before execution"}),
    )
}
fn adapter_git(root: &Path, args: &[&str]) -> Result<Vec<u8>, &'static str> {
    adapter_git_bounded(root, args, Duration::from_secs(2), 1 << 20)
}
fn adapter_git_bounded(
    root: &Path,
    args: &[&str],
    deadline: Duration,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(root)
        .args(args)
        .stdin(std::process::Stdio::null());
    // The existing concurrent pipe reader bounds both streams and kills/waits
    // this child on deadline or overflow. No unbounded Command::output capture.
    bounded_output(&mut command, deadline, limit)
        .map_err(|_| "workspace Git observation refused, timed out, or exceeded output ceiling")
}
pub fn adapter_checkout(root: &Path, sha: &str) -> Result<(), &'static str> {
    let top = adapter_git(root, &["rev-parse", "--show-toplevel"])?;
    let top = std::str::from_utf8(&top)
        .map_err(|_| "invalid Git root")?
        .trim();
    if Path::new(top)
        .canonicalize()
        .map_err(|_| "Git root unavailable")?
        != root
    {
        return Err("--workspace must be the Git checkout root");
    }
    let head = adapter_git(root, &["rev-parse", "HEAD"])?;
    if std::str::from_utf8(&head)
        .map_err(|_| "invalid Git HEAD")?
        .trim()
        != sha
    {
        return Err("requested SHA does not match workspace HEAD");
    }
    if !adapter_git(root, &["status", "--porcelain", "--untracked-files=all"])?.is_empty() {
        return Err("workspace source differs from workspace HEAD");
    }
    let flags = adapter_git(root, &["ls-files", "-v", "-z"])?;
    if flags
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .any(|record| record[0].is_ascii_lowercase() || record[0] == b'S')
    {
        return Err("workspace index hides tracked file changes");
    }
    Ok(())
}
pub fn committed_adapter_file(root: &Path, relative: &str) -> Result<Vec<u8>, &'static str> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err("adapter source must be relative without traversal");
    }
    let path = root
        .join(relative_path)
        .canonicalize()
        .map_err(|_| "adapter source unavailable")?;
    if !path.starts_with(root) || !path.is_file() {
        return Err("adapter source escapes workspace");
    }
    // Resolve and pin the blob before size and content reads; a HEAD change
    // cannot replace a size-checked blob with another object between commands.
    let object = adapter_git_bounded(
        root,
        &["rev-parse", "--verify", &format!("HEAD:{relative}")],
        Duration::from_secs(2),
        4096,
    )?;
    let object = std::str::from_utf8(&object)
        .map_err(|_| "invalid committed blob identity")?
        .trim();
    if object.len() != 40 || !object.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid committed blob identity");
    }
    let size = adapter_git_bounded(
        root,
        &["cat-file", "-s", object],
        Duration::from_secs(2),
        4096,
    )?;
    let size = std::str::from_utf8(&size)
        .map_err(|_| "invalid committed blob size")?
        .trim()
        .parse::<usize>()
        .map_err(|_| "invalid committed blob size")?;
    if size > 1 << 20 {
        return Err("adapter source exceeds 1 MiB");
    }
    let bytes = adapter_git_bounded(
        root,
        &["cat-file", "blob", object],
        Duration::from_secs(2),
        1 << 20,
    )?;
    if bytes.len() != size {
        return Err("committed blob size changed");
    }
    let file = std::fs::File::open(path).map_err(|_| "adapter source unavailable")?;
    let metadata = file
        .metadata()
        .map_err(|_| "adapter source metadata unavailable")?;
    if !metadata.is_file() || metadata.len() > 1 << 20 {
        return Err("adapter source exceeds 1 MiB or is not regular");
    }
    let mut current = Vec::new();
    file.take((1 << 20) + 1)
        .read_to_end(&mut current)
        .map_err(|_| "adapter source unreadable")?;
    if current != bytes {
        return Err("adapter source differs from workspace HEAD");
    }
    Ok(bytes)
}
/// The adapter plan receipt for `arguments` (the options after `plan`), or
/// the refusal. Reads only the committed checkout; executes nothing.
pub fn plan(arguments: Vec<OsString>) -> Result<Value, &'static str> {
    let arguments = arguments
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "adapter options must be UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let values = adapter_options(arguments)?;
    let root = PathBuf::from(option(&values, "--workspace")?)
        .canonicalize()
        .map_err(|_| "workspace unavailable")?;
    let sha = option(&values, "--sha")?;
    adapter_checkout(&root, sha)?;
    let bytes = committed_adapter_file(&root, option(&values, "--adapter")?)?;
    let adapter = bosn_core::act::parse_act_adapter_json(&bytes).map_err(|error| error.0)?;
    for workflow in adapter
        .workflows
        .pull_request
        .iter()
        .chain(&adapter.workflows.push)
        .chain(&adapter.workflows.release)
    {
        committed_adapter_file(&root, workflow)?;
    }
    let result = declared_adapter_plan(&adapter, &values, sha)?;
    adapter_checkout(&root, sha)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> bosn_core::act::ActAdapterV1 {
        let digest = format!("sha256:{}", "a".repeat(64));
        bosn_core::act::parse_act_adapter_json(&serde_json::to_vec(&json!({
            "schema_version":1,"repository":{"owner":"FastLED","name":"cli"},"default_branch":"main",
            "pins":{"interface_schema":1,"act_version":bosn_core::act::ACT_VERSION,"act_binary_digest":digest,"engine_manifest_digest":digest,"engine_config_digest":digest,"runner_manifest_digest":digest,"runner_config_digest":digest},
            "workflows":{"pull_request":[".github/workflows/ci.yml"],"push":[".github/workflows/ci.yml"],"release":[".github/workflows/ci.yml"]},
            "cells":[{"id":"lint","workflow":".github/workflows/ci.yml","job":"lint","runner":"ubuntu-latest","proof_scope":"local_linux"},{"id":"unit","workflow":".github/workflows/ci.yml","job":"unit","runner":"ubuntu-22.04","proof_scope":"local_linux"},{"id":"win","workflow":".github/workflows/ci.yml","job":"windows","runner":"windows-2022","proof_scope":"github_only"}],
            "tiers":{"minimal":["lint"],"test":["lint","unit"],"full":["lint","unit","win"]},
            "release_inputs":{"candidate_sha":"candidate","full_mode":"coverage","full_mode_value":"full","version":null},"permitted_secrets":[]
        })).unwrap()).unwrap()
    }
    fn options(event: &str, mode: &str) -> std::collections::BTreeMap<String, String> {
        let mut args = vec![
            "--event",
            event,
            "--mode",
            mode,
            "--repo-owner",
            "FastLED",
            "--repo-name",
            "cli",
            "--sha",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--base-sha",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ];
        if event == "pull_request" {
            args.extend([
                "--pr-number",
                "12",
                "--head-owner",
                "external",
                "--head-name",
                "fork",
                "--head-ref",
                "work",
                "--base-ref",
                "main",
                "--author-login",
                "contributor",
            ]);
        }
        adapter_options(args).unwrap()
    }
    #[test]
    fn full_plan_preserves_foreign_cells_and_denies_metadata_authority() {
        let result = declared_adapter_plan(
            &fixture(),
            &options("pull_request", "full"),
            &"a".repeat(40),
        )
        .unwrap();
        assert_eq!(
            result["plan"]["event_payload"]["pull_request"]["labels"][0]["name"],
            "ci-full"
        );
        assert_eq!(
            result["plan"]["required_cells"].as_array().unwrap().len(),
            3
        );
        assert_eq!(result["plan"]["github_only_required"], json!(["win"]));
        assert_eq!(result["plan"]["declaration_only"], true);
        for path in [
            vec!["plan", "graph_matched"],
            vec!["executable"],
            vec!["source_metadata_verified"],
            vec!["merged_candidate_verified"],
            vec!["binary_pin_verified"],
        ] {
            let mut value = &result;
            for key in path {
                value = &value[key];
            }
            assert_eq!(value, false);
        }
        assert_eq!(
            result["plan"]["event_payload"]["pull_request"]["head"]["repo"]["full_name"],
            "external/fork"
        );
        assert_eq!(
            result["plan"]["event_payload"]["pull_request"]["user"]["login"],
            "contributor"
        );
    }
    #[test]
    fn adapter_event_matrix_and_identity_refusals_are_explicit() {
        for event in ["pull_request", "push", "release"] {
            for mode in ["minimal", "test", "full"] {
                let expected = event == "pull_request"
                    || (event == "push" && mode == "minimal")
                    || (event == "release" && mode == "full");
                assert_eq!(
                    declared_adapter_plan(&fixture(), &options(event, mode), &"a".repeat(40))
                        .is_ok(),
                    expected
                );
            }
        }
        let result = declared_adapter_plan(
            &fixture(),
            &options("pull_request", "test"),
            &"a".repeat(40),
        )
        .unwrap();
        assert_eq!(
            result["plan"]["event_payload"]["pull_request"]["labels"][0]["name"],
            "ci-test"
        );
        let release =
            declared_adapter_plan(&fixture(), &options("release", "full"), &"a".repeat(40))
                .unwrap();
        assert_eq!(
            release["plan"]["event_payload"]["inputs"],
            json!({"candidate":"a".repeat(40),"coverage":"full"})
        );
        for missing in [
            "--base-sha",
            "--pr-number",
            "--head-owner",
            "--author-login",
        ] {
            let mut args = options("pull_request", "full");
            args.remove(missing);
            assert!(declared_adapter_plan(&fixture(), &args, &"a".repeat(40)).is_err());
        }
        let mut args = options("push", "minimal");
        args.insert("--pr-number".into(), "12".into());
        assert!(declared_adapter_plan(&fixture(), &args, &"a".repeat(40)).is_err());
        assert!(
            declared_adapter_plan(&fixture(), &options("push", "minimal"), &"b".repeat(40))
                .is_err()
        );
        let mut args = options("pull_request", "full");
        args.insert("--repo-owner".into(), "foreign".into());
        assert!(declared_adapter_plan(&fixture(), &args, &"a".repeat(40)).is_err());
    }
    #[test]
    fn checkout_requires_exact_clean_committed_inputs_and_visible_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        adapter_git(&root, &["init", "--initial-branch=main"]).unwrap();
        std::fs::write(root.join("adapter.json"), b"{}\n").unwrap();
        adapter_git(&root, &["add", "adapter.json"]).unwrap();
        adapter_git(
            &root,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "fixture",
            ],
        )
        .unwrap();
        let head = String::from_utf8(adapter_git(&root, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        let head = head.trim();
        adapter_checkout(&root, head).unwrap();
        assert_eq!(
            committed_adapter_file(&root, "adapter.json").unwrap(),
            b"{}\n"
        );
        assert!(adapter_checkout(&root, &"0".repeat(40)).is_err());
        assert!(committed_adapter_file(&root, "../adapter.json").is_err());
        std::fs::write(root.join("adapter.json"), b"modified").unwrap();
        assert!(adapter_checkout(&root, head).is_err());
        assert!(committed_adapter_file(&root, "adapter.json").is_err());
        std::fs::write(root.join("adapter.json"), vec![b'x'; (1 << 20) + 1]).unwrap();
        assert_eq!(
            committed_adapter_file(&root, "adapter.json").unwrap_err(),
            "adapter source exceeds 1 MiB or is not regular"
        );
        std::fs::write(root.join("large.json"), vec![b'x'; (1 << 20) + 1]).unwrap();
        adapter_git(&root, &["add", "large.json"]).unwrap();
        adapter_git(
            &root,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "oversized fixture",
            ],
        )
        .unwrap();
        assert_eq!(
            committed_adapter_file(&root, "large.json").unwrap_err(),
            "adapter source exceeds 1 MiB"
        );
        let latest_head =
            String::from_utf8(adapter_git(&root, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        adapter_git(
            &root,
            &["update-index", "--assume-unchanged", "adapter.json"],
        )
        .unwrap();
        assert_eq!(
            adapter_checkout(&root, latest_head.trim()).unwrap_err(),
            "workspace index hides tracked file changes"
        );
    }
    #[test]
    fn git_observations_refuse_output_above_capture_budget() {
        let dir = tempfile::tempdir().unwrap();
        adapter_git(dir.path(), &["init", "--initial-branch=main"]).unwrap();
        assert!(
            adapter_git_bounded(
                dir.path(),
                &["rev-parse", "--git-dir"],
                Duration::from_secs(2),
                1
            )
            .is_err()
        );
        assert!(
            adapter_git_bounded(
                dir.path(),
                &["rev-parse", "--git-dir"],
                Duration::ZERO,
                4096
            )
            .is_err()
        );
    }
    #[test]
    fn adapter_options_refuse_duplicates_and_legacy_mixing() {
        assert!(adapter_options(vec!["--adapter", "a", "--adapter", "b"]).is_err());
        assert!(adapter_options(vec!["--adapter", "a", "--act-bin", "act"]).is_err());
        assert!(adapter_options(vec!["--adapter"]).is_err());
    }
}
