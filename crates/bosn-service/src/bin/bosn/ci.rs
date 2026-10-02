//! `bosn ci`: local CI runs on a daemon-owned isolated engine.
//!
//! Exit codes are a contract for agents: 0 success, 1 workflow failure or
//! engine error, 2 cancelled or timed out, 3 refused or incomplete coverage.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::PathBuf,
    time::{Duration, Instant},
};

use bosn_service::{
    Client,
    ci::{
        CiRequest, RunState, RunnerAction, SubmitOptions,
        provider::{Mode, Provider, Trigger},
    },
};
use kernal_api::async_engine::{Runtime, RuntimeBuilder};
use serde_json::Value;

pub const USAGE: &str = "usage: bosn ci plan [--workspace P] [--provider github] [--workflow F] [--job J] [--trigger pr|push|release] [--mode minimal|test|full] [--sha S] [--json]
   or: bosn ci run <plan options> [--engine act] [--pr-number N] [--timeout-secs N] [--wait [--deadline-ms N]] [--json]
   or: bosn ci list [--workspace P] [--state queued|running|done] [--limit N] [--json]
   or: bosn ci show RUN [--json]
   or: bosn ci logs RUN [--job K] [--step S] [--since-seq N] [--follow] [--json]
   or: bosn ci wait RUN [--deadline-ms N] [--json]
   or: bosn ci cancel RUN [--json]
   or: bosn ci report RUN [--tail N] [--json]
   or: bosn ci retry RUN [--job K] [--json]
   or: bosn ci runners [list|drain|resume|set-limit N|prune-cache [--older-than-secs N] [--max-bytes N]] [--json]
   (every verb accepts --state-dir STATE_DIR)";

const EXIT_REFUSED: i32 = 3;
const POLL: Duration = Duration::from_millis(500);

/// Parsed `--flag value` / `--switch` arguments plus positionals.
struct Flags {
    values: BTreeMap<&'static str, String>,
    switches: BTreeSet<&'static str>,
    positional: Vec<String>,
}

impl Flags {
    fn parse(
        arguments: impl Iterator<Item = OsString>,
        valued: &[&'static str],
        switches: &[&'static str],
    ) -> Result<Self, String> {
        let mut flags = Self {
            values: BTreeMap::new(),
            switches: BTreeSet::new(),
            positional: Vec::new(),
        };
        let mut arguments = arguments.map(|a| a.into_string().map_err(|_| "non-UTF-8 argument"));
        while let Some(argument) = arguments.next() {
            let argument = argument?;
            if let Some(flag) = valued.iter().find(|f| **f == argument) {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{flag} needs a value"))??;
                if flags.values.insert(flag, value).is_some() {
                    return Err(format!("{flag} given twice"));
                }
            } else if let Some(switch) = switches.iter().find(|s| **s == argument) {
                if !flags.switches.insert(switch) {
                    return Err(format!("{switch} given twice"));
                }
            } else if argument.starts_with("--") {
                return Err(format!("unknown option {argument}"));
            } else {
                flags.positional.push(argument);
            }
        }
        Ok(flags)
    }
    fn get(&self, flag: &str) -> Option<&str> {
        self.values.get(flag).map(String::as_str)
    }
    fn number<T: std::str::FromStr>(&self, flag: &str) -> Result<Option<T>, String> {
        self.get(flag)
            .map(|v| v.parse().map_err(|_| format!("{flag} must be a number")))
            .transpose()
    }
    fn has(&self, switch: &str) -> bool {
        self.switches.contains(switch)
    }
    fn json(&self) -> bool {
        self.has("--json")
    }
    fn state_dir(&self) -> PathBuf {
        self.get("--state-dir")
            .map(PathBuf::from)
            .unwrap_or_else(bosn_service::mcp::default_state_dir)
    }
    /// The single run ID positional.
    fn run(&self) -> Result<String, String> {
        match self.positional.as_slice() {
            [run] => Ok(run.clone()),
            _ => Err("expected exactly one RUN".into()),
        }
    }
}

const PLAN_FLAGS: &[&str] = &[
    "--state-dir",
    "--workspace",
    "--provider",
    "--workflow",
    "--job",
    "--trigger",
    "--mode",
    "--sha",
    "--engine",
    "--pr-number",
    "--timeout-secs",
    "--deadline-ms",
];

fn refuse(message: impl std::fmt::Display) -> ! {
    eprintln!("bosn ci: {message}");
    std::process::exit(EXIT_REFUSED)
}

/// Entry point. `alias` names the deprecated spelling that routed here.
pub fn run(mut arguments: impl Iterator<Item = OsString>, alias: Option<&str>) {
    if let Some(alias) = alias {
        eprintln!(
            "bosn: `{alias}` is deprecated and will be removed after one release; use `bosn ci --provider github --engine act`"
        );
    }
    let verb = arguments
        .next()
        .and_then(|v| v.into_string().ok())
        .unwrap_or_else(|| refuse(USAGE));
    let result = match verb.as_str() {
        "plan" => plan(arguments),
        "run" => submit(arguments),
        "list" => list(arguments),
        "show" => show(arguments),
        "logs" => logs(arguments),
        "wait" => wait(arguments),
        "cancel" => simple(arguments, |run| CiRequest::Cancel { run }),
        "report" => report(arguments),
        "retry" => retry(arguments),
        "runners" => runners(arguments),
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(message) => refuse(message),
    }
}

fn submit_options(flags: &Flags) -> Result<SubmitOptions, String> {
    Ok(SubmitOptions {
        workspace: PathBuf::from(flags.get("--workspace").unwrap_or(".")),
        provider: flags.get("--provider").map(Provider::parse).transpose()?,
        engine: flags.get("--engine").map(str::to_string),
        workflow: flags.get("--workflow").map(str::to_string),
        job: flags.get("--job").map(str::to_string),
        trigger: flags.get("--trigger").map(Trigger::parse).transpose()?,
        mode: flags.get("--mode").map(Mode::parse).transpose()?,
        actor: None,
        sha: flags.get("--sha").map(str::to_string),
        pr_number: flags.number("--pr-number")?,
        timeout_secs: flags.number("--timeout-secs")?,
    })
}

/// A runtime and a client whose daemon is known to be running.
fn connect(flags: &Flags) -> Result<(Runtime, Client), String> {
    let state_dir = flags.state_dir();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let client = Client::for_state(&state_dir).map_err(|e| e.to_string())?;
    super::run::ensure_daemon(&runtime, &client, &state_dir)?;
    Ok((runtime, client))
}

fn call(runtime: &Runtime, client: &Client, request: CiRequest) -> Result<Value, String> {
    runtime.run(client.ci(request)).map_err(describe)
}

fn describe(error: bosn_service::Error) -> String {
    match error {
        bosn_service::Error::Ci { code, message } => format!("{code}: {message}"),
        other => format!("daemon request failed: {other}"),
    }
}

fn print(value: &Value, json: bool, text: impl FnOnce(&Value)) {
    if json {
        println!("{value}");
    } else {
        text(value);
    }
}

fn plan(arguments: impl Iterator<Item = OsString>) -> Result<i32, String> {
    let flags = Flags::parse(arguments, PLAN_FLAGS, &["--json"])?;
    let plan = bosn_service::ci::plan(&submit_options(&flags)?).map_err(describe)?;
    print(&plan, flags.json(), |p| {
        for key in [
            "provider",
            "workflow",
            "trigger",
            "mode",
            "event",
            "sha",
            "dirty",
            "executable",
        ] {
            println!("{key}: {}", text(&p[key]));
        }
    });
    Ok(0)
}

fn submit(arguments: impl Iterator<Item = OsString>) -> Result<i32, String> {
    let flags = Flags::parse(arguments, PLAN_FLAGS, &["--json", "--wait"])?;
    let options = submit_options(&flags)?;
    // Refuse a bad checkout or trigger before starting any daemon.
    bosn_service::ci::plan(&options).map_err(describe)?;
    let (runtime, client) = connect(&flags)?;
    let submitted = runtime.run(client.ci_submit(options)).map_err(describe)?;
    let run = submitted["run"].as_str().unwrap_or_default().to_string();
    if !flags.has("--wait") {
        print(&submitted, flags.json(), |s| {
            let how = if s["coalesced"] == true {
                " (joined an identical run)"
            } else {
                ""
            };
            println!("run {run} queued{how}");
            println!("follow: bosn ci logs {run} --follow");
        });
        return Ok(0);
    }
    if !flags.json() {
        eprintln!("run {run} submitted; waiting");
    }
    finish(&runtime, &client, &run, &flags)
}

/// Wait for a run, then print its full record and return its exit code.
fn finish(runtime: &Runtime, client: &Client, run: &str, flags: &Flags) -> Result<i32, String> {
    let deadline = flags
        .number::<u64>("--deadline-ms")?
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    loop {
        let shown = call(
            runtime,
            client,
            CiRequest::Show {
                run: run.into(),
                tree: Some(true),
            },
        )?;
        let done = shown["state"] == "done";
        if done || deadline.is_some_and(|d| Instant::now() >= d) {
            print(&shown, flags.json(), print_tree);
            return Ok(if done {
                shown["exit_code"].as_i64().unwrap_or(1) as i32
            } else {
                2
            });
        }
        std::thread::sleep(POLL);
    }
}

fn wait(arguments: impl Iterator<Item = OsString>) -> Result<i32, String> {
    let flags = Flags::parse(arguments, &["--state-dir", "--deadline-ms"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    finish(&runtime, &client, &flags.run()?, &flags)
}

fn list(arguments: impl Iterator<Item = OsString>) -> Result<i32, String> {
    let flags = Flags::parse(
        arguments,
        &["--state-dir", "--workspace", "--state", "--limit"],
        &["--json"],
    )?;
    let state = match flags.get("--state") {
        None => None,
        Some("queued") => Some(RunState::Queued),
        Some("running") => Some(RunState::Running),
        Some("done") => Some(RunState::Done),
        Some(other) => return Err(format!("unknown state {other:?}")),
    };
    let workspace = flags
        .get("--workspace")
        .map(|w| std::fs::canonicalize(w).map_err(|_| "workspace does not exist".to_string()))
        .transpose()?
        .map(|w| w.to_string_lossy().into_owned());
    let (runtime, client) = connect(&flags)?;
    let listed = call(
        &runtime,
        &client,
        CiRequest::List {
            workspace,
            state,
            limit: flags.number("--limit")?,
        },
    )?;
    print(&listed, flags.json(), |l| {
        for run in l["runs"].as_array().into_iter().flatten() {
            println!(
                "{}  {:<8} {:<10} {}{}  {}  {}",
                text(&run["id"]),
                text(&run["state"]),
                text(&run["conclusion"]),
                &text(&run["sha"]).chars().take(12).collect::<String>(),
                if run["dirty"].is_null() { "" } else { "+dirty" },
                text(&run["workflow"]),
                text(&run["actor"]),
            );
        }
        print_runners(&l["runners"]);
    });
    Ok(0)
}

fn show(arguments: impl Iterator<Item = OsString>) -> Result<i32, String> {
    let flags = Flags::parse(arguments, &["--state-dir"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    let shown = call(
        &runtime,
        &client,
        CiRequest::Show {
            run: flags.run()?,
            tree: Some(true),
        },
    )?;
    print(&shown, flags.json(), print_tree);
    Ok(0)
}

fn logs(arguments: impl Iterator<Item = OsString>) -> Result<i32, String> {
    let flags = Flags::parse(
        arguments,
        &["--state-dir", "--job", "--step", "--since-seq", "--limit"],
        &["--json", "--follow"],
    )?;
    let run = flags.run()?;
    let (runtime, client) = connect(&flags)?;
    let mut since = flags.number::<u64>("--since-seq")?.unwrap_or(0);
    loop {
        let page = call(
            &runtime,
            &client,
            CiRequest::Logs {
                run: run.clone(),
                job: flags.get("--job").map(str::to_string),
                section: flags.get("--step").map(str::to_string),
                since_seq: Some(since),
                limit: flags.number("--limit")?,
                max_bytes: None,
            },
        )?;
        for record in page["records"].as_array().into_iter().flatten() {
            if flags.json() {
                println!("{record}");
            } else {
                println!("{}", text(&record["text"]));
            }
        }
        since = page["next_seq"].as_u64().unwrap_or(since);
        let more = page["more"] == true;
        let one_page = flags.get("--limit").is_some() && !flags.has("--follow");
        if one_page || (!more && (!flags.has("--follow") || page["done"] == true)) {
            if one_page && more {
                eprintln!("more: bosn ci logs {run} --since-seq {since}");
            }
            return Ok(0);
        }
        if !more {
            std::thread::sleep(POLL);
        }
    }
}

fn report(arguments: impl Iterator<Item = OsString>) -> Result<i32, String> {
    let flags = Flags::parse(arguments, &["--state-dir", "--tail"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    let report = call(
        &runtime,
        &client,
        CiRequest::Report {
            run: flags.run()?,
            tail: flags.number("--tail")?,
        },
    )?;
    print(&report, flags.json(), |r| {
        println!("run {}: {}", text(&r["run"]), text(&r["conclusion"]));
        if !r["reason"].is_null() {
            println!("reason: {}", text(&r["reason"]));
        }
        if let Some(failure) = r["first_failure"].as_object() {
            println!(
                "first failure: job {} step {} (exit {})",
                text(&failure["job"]),
                text(&failure["step"]),
                text(&failure["exit_code"])
            );
            for line in failure["tail"].as_array().into_iter().flatten() {
                println!("  | {}", text(line));
            }
        }
        for kind in ["skipped", "unsupported"] {
            let jobs = &r["jobs"][kind];
            if jobs.as_array().is_some_and(|a| !a.is_empty()) {
                println!("{kind}: {jobs}");
            }
        }
    });
    Ok(report["exit_code"].as_i64().map_or(2, |c| c as i32))
}

fn retry(arguments: impl Iterator<Item = OsString>) -> Result<i32, String> {
    let flags = Flags::parse(arguments, &["--state-dir", "--job"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    let retried = call(
        &runtime,
        &client,
        CiRequest::Retry {
            run: flags.run()?,
            job: flags.get("--job").map(str::to_string),
        },
    )?;
    print(&retried, flags.json(), |r| {
        println!("run {} queued", text(&r["run"]))
    });
    Ok(0)
}

/// A verb whose only argument is RUN.
fn simple(
    arguments: impl Iterator<Item = OsString>,
    request: impl FnOnce(String) -> CiRequest,
) -> Result<i32, String> {
    let flags = Flags::parse(arguments, &["--state-dir"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    let reply = call(&runtime, &client, request(flags.run()?))?;
    print(&reply, flags.json(), |r| println!("{r}"));
    Ok(0)
}

fn runners(arguments: impl Iterator<Item = OsString>) -> Result<i32, String> {
    let flags = Flags::parse(
        arguments,
        &["--state-dir", "--older-than-secs", "--max-bytes"],
        &["--json"],
    )?;
    let action = match flags
        .positional
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()[..]
    {
        [] | ["list"] => RunnerAction::List,
        ["drain"] => RunnerAction::Drain,
        ["resume"] => RunnerAction::Resume,
        ["set-limit", limit] => RunnerAction::SetLimit {
            limit: limit.parse().map_err(|_| "set-limit needs a number")?,
        },
        ["prune-cache"] => RunnerAction::PruneCache {
            older_than_secs: flags.number("--older-than-secs")?,
            max_bytes: flags.number("--max-bytes")?,
        },
        _ => return Err(USAGE.into()),
    };
    let (runtime, client) = connect(&flags)?;
    let reply = call(&runtime, &client, CiRequest::Runners { action })?;
    print(&reply, flags.json(), |r| {
        print_runners(&r["runners"]);
        if let Some(pruned) = r["pruned_runs"].as_array() {
            println!("pruned runs: {}", pruned.len());
        }
    });
    Ok(0)
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    }
}

fn print_runners(runners: &Value) {
    println!(
        "runners: {} running, {} queued, limit {}{}",
        text(&runners["running"]),
        text(&runners["queued"]),
        text(&runners["limit"]),
        if runners["drained"] == true {
            " (drained)"
        } else {
            ""
        }
    );
}

fn print_tree(run: &Value) {
    println!(
        "run {} {} {}  sha {}{}  {}",
        text(&run["id"]),
        text(&run["state"]),
        text(&run["conclusion"]),
        text(&run["sha"]),
        if run["dirty"].is_null() {
            ""
        } else {
            " +dirty"
        },
        text(&run["workflow"]),
    );
    if !run["reason"].is_null() {
        println!("  reason: {}", text(&run["reason"]));
    }
    for group in run["tree"]["groups"].as_array().into_iter().flatten() {
        println!("  stage {}", text(&group["name"]));
        for job in group["jobs"].as_array().into_iter().flatten() {
            println!(
                "    {} {}",
                mark(&job["conclusion"], &job["status"]),
                text(&job["key"])
            );
            for section in job["sections"].as_array().into_iter().flatten() {
                println!(
                    "      {} {} {}",
                    mark(&section["conclusion"], &section["status"]),
                    text(&section["stage"]),
                    text(&section["name"])
                );
            }
        }
    }
}

/// Status by symbol as well as word, never colour alone.
fn mark(conclusion: &Value, status: &Value) -> &'static str {
    match (conclusion.as_str(), status.as_str()) {
        (Some("success"), _) => "[ok]",
        (Some("failure"), _) => "[FAIL]",
        (Some("cancelled"), _) => "[cancelled]",
        (Some("skipped"), _) => "[skip]",
        (Some("unsupported"), _) => "[unsupported]",
        (_, Some("in_progress")) => "[..]",
        _ => "[queued]",
    }
}
