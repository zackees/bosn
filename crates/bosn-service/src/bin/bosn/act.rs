//! Deprecated `bosn act` spelling. `run` and `report` route to `bosn ci`,
//! `plan --adapter` to `bosn ci plan --adapter`'s shared implementation;
//! `plan` and `payload` keep their read-only receipts for one release.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde_json::{Value, json};

use super::ci::adapter::{self, adapter_checkout, committed_adapter_file};

pub fn run(mut arguments: impl Iterator<Item = OsString>) {
    let Some(verb) = arguments.next() else {
        fail("expected plan, payload, run, or report")
    };
    if verb == "run" || verb == "report" {
        let alias = format!("bosn act {}", verb.to_string_lossy());
        return super::ci::run(std::iter::once(verb).chain(arguments), Some(&alias));
    }
    let remaining: Vec<OsString> = arguments.collect();
    if verb == "plan" && adapter::requested(&remaining) {
        // The adapter plan moved to `bosn ci plan --adapter` (#375); stdout
        // stays the same receipt, byte for byte.
        eprintln!(
            "bosn: `bosn act plan --adapter` is deprecated and will be removed after one release; use `bosn ci plan --adapter`"
        );
        let receipt = adapter::plan(remaining).unwrap_or_else(|error| fail(error));
        return println!("{receipt}");
    }
    let mut arguments = remaining.into_iter();
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
        let result = crate::bounded_output::bounded_output(&mut command, deadline, limit);
        control
            .cleanup()
            .map_err(|_| "act control directory cleanup failed")?;
        result
    })();
    // Drop the control directory before fail() exits the CLI without unwinding.
    result.unwrap_or_else(|message| fail(message))
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
