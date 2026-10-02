//! Deprecated `bosn act` spelling. `run` and `report` route to `bosn ci`;
//! `plan` and `payload` keep their read-only receipts for one release.

use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, RecvTimeoutError},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

pub fn run(mut arguments: impl Iterator<Item = OsString>) {
    let Some(verb) = arguments.next() else {
        fail("expected plan, payload, run, or report")
    };
    if verb == "run" || verb == "report" {
        let alias = format!("bosn act {}", verb.to_string_lossy());
        return super::ci::run(std::iter::once(verb).chain(arguments), Some(&alias));
    }
    eprintln!(
        "bosn: `bosn act {}` is deprecated and will be removed after one release; use `bosn ci plan`",
        verb.to_string_lossy()
    );
    if verb == "payload" {
        return payload(arguments);
    }
    if verb != "plan" {
        fail("expected plan, payload, run, or report")
    }
    let remaining: Vec<OsString> = arguments.collect();
    if remaining.iter().any(|flag| flag == "--adapter") {
        return adapter_plan(remaining);
    }
    let mut arguments = remaining.into_iter();
    let mut workspace = None;
    let mut workflow = None;
    let mut event = None;
    let mut mode = None;
    let mut sha = None;
    let mut version = None;
    let mut act_bin = None;
    let mut job = None;
    let mut json_output = false;
    while let Some(flag) = arguments.next() {
        let slot = match flag.to_str() {
            Some("--workspace") => &mut workspace,
            Some("--workflow") => &mut workflow,
            Some("--event") => &mut event,
            Some("--mode") => &mut mode,
            Some("--sha") => &mut sha,
            Some("--act-version") => &mut version,
            Some("--act-bin") => &mut act_bin,
            Some("--job") => &mut job,
            Some("--json") if !json_output => {
                json_output = true;
                continue;
            }
            _ => fail("invalid or duplicate act option"),
        };
        if slot.is_some() {
            fail("duplicate act option")
        }
        *slot = Some(
            arguments
                .next()
                .and_then(|v| v.into_string().ok())
                .unwrap_or_else(|| fail("missing act option value")),
        );
    }
    let workspace = workspace.unwrap_or_else(|| fail("--workspace is required"));
    let workflow = workflow.unwrap_or_else(|| fail("--workflow is required"));
    let event = event.unwrap_or_else(|| fail("--event is required"));
    let mode = mode.unwrap_or_else(|| fail("--mode is required"));
    let sha = sha.unwrap_or_else(|| fail("--sha is required"));
    let version = version.unwrap_or_else(|| fail("--act-version is required"));
    if !matches!(event.as_str(), "pull_request" | "push" | "release") {
        fail("invalid event")
    }
    if !matches!(mode.as_str(), "minimal" | "test" | "full") {
        fail("invalid mode")
    }
    if event == "release" && mode != "full" {
        fail("release requires full mode")
    }
    if event == "push" && mode != "minimal" {
        fail("push requires minimal mode; full validation requires a release event")
    }
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        fail("--sha must be 40 hexadecimal characters")
    }
    if version.is_empty()
        || version.len() > 32
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        fail("invalid act version")
    }
    if let Some(ref job) = job
        && (job.is_empty()
            || job.len() > 128
            || !job
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    {
        fail("invalid job ID")
    }
    let root = PathBuf::from(workspace)
        .canonicalize()
        .unwrap_or_else(|_| fail("workspace does not exist"));
    adapter_checkout(&root, &sha.to_ascii_lowercase()).unwrap_or_else(|error| fail(error));
    let current = committed_adapter_file(&root, &workflow).unwrap_or_else(|error| fail(error));
    let relative = Path::new(&workflow);
    let path = root.join(relative);
    // The generic Bosn surface does not assume each repository's selector or
    // dispatch input names. Its event mapping is an intention until the repo's
    // adapter has furnished and validated the exact event payload.
    let github_event = if event == "release" {
        "workflow_dispatch"
    } else {
        &event
    };
    let act_bin = act_bin.unwrap_or_else(|| "act".to_owned());
    // Preserve workspace-relative --act-bin semantics before moving the child
    // into an empty control directory. Bare names retain PATH lookup.
    let act_path = Path::new(&act_bin);
    let act_bin = if act_path.components().count() > 1 && !act_path.is_absolute() {
        root.join(act_path).into_os_string()
    } else {
        OsString::from(act_bin)
    };
    let version_line = String::from_utf8(sterile_act_output(
        &act_bin,
        &[OsString::from("--version")],
        Duration::from_secs(2),
        4096,
    ))
    .unwrap_or_else(|_| fail("act version output is not UTF-8"));
    if version_line.trim() != format!("act version {version}") {
        fail("act binary does not match --act-version")
    }
    let output = String::from_utf8(sterile_act_output(
        &act_bin,
        &[
            OsString::from("-l"),
            OsString::from("-C"),
            root.clone().into_os_string(),
            OsString::from("-W"),
            path.clone().into_os_string(),
            // Act 0.2.88 discovers gh credentials only when this key is absent.
            OsString::from("--secret"),
            OsString::from("GITHUB_TOKEN="),
        ],
        Duration::from_secs(2),
        1024 * 1024,
    ))
    .unwrap_or_else(|_| fail("act list output is not UTF-8"));
    let jobs = parse_list(&output);
    adapter_checkout(&root, &sha.to_ascii_lowercase())
        .unwrap_or_else(|_| fail("workspace changed during planning"));
    if committed_adapter_file(&root, &workflow)
        .unwrap_or_else(|_| fail("workspace changed during planning"))
        != current
    {
        fail("workspace changed during planning")
    }
    let selected_jobs: Vec<&Value> = jobs
        .iter()
        .filter(|row| job.as_ref().is_none_or(|wanted| row["id"] == *wanted))
        .collect();
    if selected_jobs.is_empty() {
        fail("selected job does not exist in act list")
    }
    let result = json!({"action":"act_plan", "schema_version":1, "workspace":root,
        "workflow":relative, "event":event, "github_event":github_event,
        "mode":mode, "sha":sha.to_ascii_lowercase(), "act_version":version,
        "act_version_source":"caller", "binary_version_verified":true, "fleet_pin_verified":false,
        "event_payload_resolved":false, "source_clean_scope":"Git HEAD and status checked before and after act queries; hidden index flags rejected; ignored files and later changes unchecked",
        "job":job, "jobs":jobs, "selected_jobs":selected_jobs,
        "selection_scope":"workflow declarations; event, mode, matrix, and job if conditions are unresolved",
        "executable":false, "docker_resources_tracked":false,
        "reason":"repository event adapter and isolated Docker ownership are required"});
    if json_output {
        println!("{result}")
    } else {
        println!("act plan: {} {} {} at {}", workflow, event, mode, sha);
        for row in &selected_jobs {
            println!(
                "stage {}: {}",
                row["stage"],
                row["id"].as_str().unwrap_or("")
            );
        }
        println!(
            "execution unavailable: repository event adapter and isolated Docker ownership are required"
        );
    }
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
    bounded_act_output(&mut command, deadline, limit)
        .map_err(|_| "workspace Git observation refused, timed out, or exceeded output ceiling")
}
fn adapter_checkout(root: &Path, sha: &str) -> Result<(), &'static str> {
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
fn committed_adapter_file(root: &Path, relative: &str) -> Result<Vec<u8>, &'static str> {
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
fn adapter_plan(arguments: Vec<OsString>) {
    let values = adapter_options(arguments.into_iter().map(|arg| {
        arg.into_string()
            .unwrap_or_else(|_| fail("adapter options must be UTF-8"))
    }))
    .unwrap_or_else(|error| fail(error));
    let root = PathBuf::from(option(&values, "--workspace").unwrap_or_else(|error| fail(error)))
        .canonicalize()
        .unwrap_or_else(|_| fail("workspace unavailable"));
    let sha = option(&values, "--sha").unwrap_or_else(|error| fail(error));
    adapter_checkout(&root, sha).unwrap_or_else(|error| fail(error));
    let bytes = committed_adapter_file(
        &root,
        option(&values, "--adapter").unwrap_or_else(|error| fail(error)),
    )
    .unwrap_or_else(|error| fail(error));
    let adapter =
        bosn_core::act::parse_act_adapter_json(&bytes).unwrap_or_else(|error| fail(error.0));
    for workflow in adapter
        .workflows
        .pull_request
        .iter()
        .chain(&adapter.workflows.push)
        .chain(&adapter.workflows.release)
    {
        committed_adapter_file(&root, workflow).unwrap_or_else(|error| fail(error));
    }
    let result = declared_adapter_plan(&adapter, &values, sha).unwrap_or_else(|error| fail(error));
    adapter_checkout(&root, sha).unwrap_or_else(|error| fail(error));
    println!("{result}");
}

/// Bosn's checked-in CI selector consumes these exact GitHub event fields.
/// This adapter deliberately supports one repository and leaves execution closed.
fn payload(mut arguments: impl Iterator<Item = OsString>) {
    let mut event = None;
    let mut mode = None;
    let mut sha = None;
    let mut pr_number = None;
    while let Some(flag) = arguments.next() {
        let slot = match flag.to_str() {
            Some("--event") => &mut event,
            Some("--mode") => &mut mode,
            Some("--sha") => &mut sha,
            Some("--pr-number") => &mut pr_number,
            _ => fail("invalid or duplicate payload option"),
        };
        if slot.is_some() {
            fail("duplicate payload option")
        }
        *slot = Some(
            arguments
                .next()
                .and_then(|v| v.into_string().ok())
                .unwrap_or_else(|| fail("missing payload option value")),
        );
    }
    let event = event.unwrap_or_else(|| fail("--event is required"));
    let mode = mode.unwrap_or_else(|| fail("--mode is required"));
    let sha = sha.unwrap_or_else(|| fail("--sha is required"));
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        fail("--sha must be 40 hexadecimal characters")
    }
    let sha = sha.to_ascii_lowercase();
    let (github_event, payload) = match (event.as_str(), mode.as_str()) {
        ("pull_request", "minimal" | "test" | "full") => {
            let number = pr_number
                .as_deref()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or_else(|| fail("pull_request requires positive --pr-number"));
            let labels: Vec<&str> = match mode.as_str() {
                "test" => vec!["ci-test"],
                "full" => vec!["ci-full"],
                _ => vec![],
            };
            (
                "pull_request",
                json!({"action":"synchronize","number":number,
                "repository":{"full_name":"zackees/bosn"},
                "pull_request":{"number":number,"head":{"sha":sha},
                    "labels":labels.iter().map(|name| json!({"name":name})).collect::<Vec<_>>()}}),
            )
        }
        ("push", "minimal") if pr_number.is_none() => (
            "push",
            json!({
            "ref":"refs/heads/main", "after":sha,
            "repository":{"full_name":"zackees/bosn"}}),
        ),
        ("release", "full") if pr_number.is_none() => (
            "workflow_dispatch",
            json!({
            "ref":"refs/heads/main", "inputs":{"tier":"full","commit_sha":sha},
            "repository":{"full_name":"zackees/bosn"}}),
        ),
        _ => fail("unsupported event/mode combination or unexpected --pr-number"),
    };
    println!(
        "{}",
        json!({"schema_version":1,"repository":"zackees/bosn",
        "workflow":".github/workflows/ci.yml","github_event":github_event,
        "mode":mode,"sha":sha,"payload":payload,"executable":false,
        "reason":"event adapter only; isolated Docker engine and job coverage report are required"})
    );
}

/// A private per-query control directory prevents Act's implicit .actrc search
/// from reaching source or the caller's HOME/XDG configuration. Distinct query
/// directories also prevent version-query side effects affecting listing.
struct ActControlDirectory {
    root: PathBuf,
    removed: bool,
}
impl ActControlDirectory {
    fn create() -> std::io::Result<Self> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_nanos();
        for attempt in 0..16 {
            let root = std::env::temp_dir().join(format!(
                "bosn-act-query-{}-{stamp}-{attempt}",
                std::process::id()
            ));
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&root) {
                Ok(()) => {
                    let owned = Self {
                        root,
                        removed: false,
                    };
                    std::fs::create_dir(owned.root.join("home"))?;
                    std::fs::create_dir(owned.root.join("config"))?;
                    std::fs::create_dir(owned.root.join("tmp"))?;
                    return Ok(owned);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("control directory collisions"))
    }
    fn cleanup(&mut self) -> std::io::Result<()> {
        std::fs::remove_dir_all(&self.root)?;
        self.removed = true;
        Ok(())
    }
}
impl Drop for ActControlDirectory {
    fn drop(&mut self) {
        if !self.removed {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
fn sterile_act_output(
    binary: &OsString,
    args: &[OsString],
    deadline: Duration,
    limit: usize,
) -> Vec<u8> {
    let result = (|| {
        let mut control =
            ActControlDirectory::create().map_err(|_| "act control directory unavailable")?;
        let mut command = Command::new(binary);
        command.current_dir(&control.root).args(args).env_clear();
        // Allow only executable lookup and the Windows loader's system paths.
        // Tokens, Docker settings, proxies, loader injection and Act variables
        // are removed by default instead of relying on credential-name guesses.
        let essential = if cfg!(windows) {
            &["PATH", "SystemRoot", "WINDIR", "PATHEXT"][..]
        } else {
            &["PATH"][..]
        };
        for key in essential {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("HOME", control.root.join("home"))
            .env("USERPROFILE", control.root.join("home"))
            .env("XDG_CONFIG_HOME", control.root.join("config"))
            .env("TMPDIR", control.root.join("tmp"))
            .env("TMP", control.root.join("tmp"))
            .env("TEMP", control.root.join("tmp"))
            .env("LC_ALL", "C")
            .env("ACT_DISABLE_VERSION_CHECK", "1")
            .stdin(std::process::Stdio::null());
        let result = bounded_act_output(&mut command, deadline, limit);
        control
            .cleanup()
            .map_err(|_| "act control directory cleanup failed")?;
        result
    })();
    // Drop the control directory before fail() exits the CLI without unwinding.
    result.unwrap_or_else(|message| fail(message))
}

/// Run only the two non-executing Act queries. Pipe readers drain concurrently
/// so neither stdout nor stderr can block the child before the deadline.
fn bounded_act_output(
    command: &mut Command,
    deadline: Duration,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|_| "act executable is unavailable")?;
    let (tx, rx) = mpsc::sync_channel::<Option<(bool, Vec<u8>)>>(8);
    for (is_stdout, mut pipe) in [
        (
            true,
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        ),
        (
            false,
            Box::new(child.stderr.take().unwrap()) as Box<dyn Read + Send>,
        ),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(None);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(Some((is_stdout, chunk[..n].to_vec()))).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    drop(tx);
    let start = Instant::now();
    let mut stdout = Vec::new();
    let mut used = 0_usize;
    let mut eof = 0;
    while eof < 2 {
        let Some(remaining) = deadline.checked_sub(start.elapsed()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("act query timed out");
        };
        match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(None) => eof += 1,
            Ok(Some((is_stdout, bytes))) => {
                used = used.saturating_add(bytes.len());
                if used > limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("act query output limit exceeded");
                }
                if is_stdout {
                    stdout.extend_from_slice(&bytes)
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("act query output ended unexpectedly");
            }
        }
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("act query timed out");
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("act query could not be reaped");
            }
        }
    };
    if !status.success() {
        return Err("act query failed");
    }
    Ok(stdout)
}

fn parse_list(output: &str) -> Vec<Value> {
    let mut lines = output.lines();
    let header = lines
        .next()
        .unwrap_or_else(|| fail("act list has no header"));
    if !header.contains("Stage") || !header.contains("Job ID") {
        fail("act list header is not recognized")
    }
    let mut jobs = Vec::new();
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let stage: u32 = fields
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| fail("act list stage is invalid"));
        let id = fields
            .next()
            .unwrap_or_else(|| fail("act list job ID is missing"));
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            fail("act list job ID is invalid")
        }
        jobs.push(json!({"stage":stage,"id":id}));
    }
    if jobs.is_empty() {
        fail("act list contains no jobs")
    }
    jobs
}

fn fail(message: &str) -> ! {
    eprintln!("bosn act: {message}");
    std::process::exit(2)
}

#[cfg(test)]
mod adapter_plan_tests {
    use super::*;
    fn fixture() -> bosn_core::act::ActAdapterV1 {
        let digest = format!("sha256:{}", "a".repeat(64));
        bosn_core::act::parse_act_adapter_json(&serde_json::to_vec(&json!({
            "schema_version":1,"repository":{"owner":"FastLED","name":"cli"},"default_branch":"main",
            "pins":{"interface_schema":1,"act_version":"0.2.88","act_binary_digest":digest,"engine_manifest_digest":digest,"engine_config_digest":digest,"runner_manifest_digest":digest,"runner_config_digest":digest},
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
