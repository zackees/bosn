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
        Conclusion, JsonReply, LogsQuery, RunState, RunView, RunnerAction, RunnerStatus,
        SubmitOptions,
        model::{ItemConclusion, ItemStatus},
        provider::{Mode, Provider, Trigger},
    },
};
use kernal_api::async_engine::{Runtime, RuntimeBuilder};

pub const USAGE: &str = "usage: bosn ci plan [--workspace P] [--provider github] [--workflow F] [--job J] [--trigger pr|push|release] [--mode minimal|test|full] [--sha S] [--json]
   or: bosn ci run <plan options> [--engine act] [--pr-number N] [--timeout-secs N] [--github-token] [--wait [--deadline-ms N]] [--json]
   or: bosn ci list [--workspace P] [--state queued|running|done] [--limit N] [--json]
   or: bosn ci show RUN [--json]
   or: bosn ci logs RUN [--job K] [--step S] [--since-seq N] [--follow] [--json]
   or: bosn ci wait RUN [--deadline-ms N] [--json]
   or: bosn ci cancel RUN [--json]
   or: bosn ci report RUN [--tail N] [--json]
   or: bosn ci retry RUN [--job K] [--json]
   or: bosn ci runners [list|drain|resume|set-limit N|prune-cache [--older-than-secs N] [--max-bytes N]] [--json]
   or: bosn ui [--path /ci/runs/RUN] [--print]  (needs `[ui] enabled = true` in <state>/config.toml)
   (every verb accepts --state-dir STATE_DIR)";

const EXIT_REFUSED: i32 = 3;
/// `wait` reached its deadline, or `report` on a run still in progress.
const EXIT_NOT_FINISHED: i32 = 2;
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

const EXIT_ERROR: i32 = 1;

/// A failed verb and the exit code it maps to: 3 for refusals (including
/// invalid arguments), 1 for daemon or transport errors.
struct Failure {
    exit: i32,
    message: String,
}
impl Failure {
    fn error(message: impl Into<String>) -> Self {
        Self {
            exit: EXIT_ERROR,
            message: message.into(),
        }
    }
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            exit: EXIT_REFUSED,
            message,
        }
    }
}
impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

fn refuse(failure: Failure) -> ! {
    eprintln!("bosn ci: {}", failure.message);
    std::process::exit(failure.exit)
}

/// Entry point. `alias` names the deprecated spelling that routed here.
pub fn run(mut arguments: impl Iterator<Item = OsString>, alias: Option<&str>) {
    if let Some(alias) = alias {
        eprintln!(
            "bosn: `{alias}` is deprecated and will be removed after one release; use `bosn ci run|report` (see `bosn ci` usage)"
        );
    }
    let verb = arguments
        .next()
        .and_then(|v| v.into_string().ok())
        .unwrap_or_else(|| refuse(USAGE.into()));
    let result = match verb.as_str() {
        "plan" => plan(arguments),
        "run" => submit(arguments),
        "list" => list(arguments),
        "show" => show(arguments),
        "logs" => logs(arguments),
        "wait" => wait(arguments),
        "cancel" => cancel(arguments),
        "report" => report(arguments),
        "retry" => retry(arguments),
        "runners" => runners(arguments),
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(failure) => refuse(failure),
    }
}

fn submit_options(flags: &Flags) -> Result<SubmitOptions, Failure> {
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
        secrets: if flags.has("--github-token") {
            vec!["github_token".into()]
        } else {
            Vec::new()
        },
    })
}

/// A runtime and a client whose daemon is known to be running.
fn connect(flags: &Flags) -> Result<(Runtime, Client), Failure> {
    let state_dir = flags.state_dir();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .map_err(|e| Failure::error(e.to_string()))?;
    let client = Client::for_state(&state_dir).map_err(|e| Failure::error(e.to_string()))?;
    super::run::ensure_daemon(&runtime, &client, &state_dir).map_err(Failure::error)?;
    Ok((runtime, client))
}

/// Run one typed client call to completion.
fn call<T>(
    runtime: &Runtime,
    request: impl std::future::Future<Output = Result<T, bosn_service::Error>>,
) -> Result<T, Failure> {
    runtime.run(request).map_err(describe)
}

fn describe(error: bosn_service::Error) -> Failure {
    match error {
        bosn_service::Error::Ci { code, message } => Failure {
            exit: if matches!(code.as_str(), "refused" | "invalid_request") {
                EXIT_REFUSED
            } else {
                EXIT_ERROR
            },
            message: format!("{code}: {message}"),
        },
        other => Failure::error(format!("daemon request failed: {other}")),
    }
}

/// `--json` prints the typed reply as JSON; otherwise `text` renders it.
fn print<T: JsonReply>(reply: &T, json: bool, text: impl FnOnce(&T)) {
    if json {
        println!("{}", reply.to_json());
    } else {
        text(reply);
    }
}

fn plan(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(arguments, PLAN_FLAGS, &["--json", "--github-token"])?;
    let plan = bosn_service::ci::plan(&submit_options(&flags)?).map_err(describe)?;
    print(&plan, flags.json(), |p| {
        println!("provider: {}", p.provider.as_str());
        println!("workflow: {}", p.workflow);
        println!("trigger: {} ({})", p.trigger.as_str(), p.event);
        println!("mode: {}", p.mode.as_str());
        println!("sha: {}{}", p.sha, if p.dirty { " +dirty" } else { "" });
        println!("actor: {}", p.actor);
    });
    Ok(0)
}

fn submit(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(
        arguments,
        PLAN_FLAGS,
        &["--json", "--wait", "--github-token"],
    )?;
    let options = submit_options(&flags)?;
    // Refuse a bad checkout or trigger before starting any daemon.
    bosn_service::ci::plan(&options).map_err(describe)?;
    let (runtime, client) = connect(&flags)?;
    let submitted = call(&runtime, client.ci_submit(options))?;
    maybe_start_widget(&runtime, &client, &flags);
    if !flags.has("--wait") {
        print(&submitted, flags.json(), |s| {
            let how = if s.coalesced {
                " (joined an identical run)"
            } else {
                ""
            };
            println!("run {} queued{how}", s.run);
            println!("follow: bosn ci logs {} --follow", s.run);
        });
        return Ok(0);
    }
    if !flags.json() {
        eprintln!("run {} submitted; waiting", submitted.run);
    }
    finish(&runtime, &client, &submitted.run, &flags)
}

/// The CLI's fallback auto-launch: when the daemon reports no widget and
/// this terminal has a desktop, start `bosn-widget` detached (it respects a
/// dismissal itself). Never fails the command.
fn maybe_start_widget(runtime: &Runtime, client: &Client, flags: &Flags) {
    use bosn_service::ci::widget::{AutoLaunch, WidgetPresence};
    let state_dir = flags.state_dir();
    let policy = bosn_service::ci::config::load(&state_dir).map(|c| c.widget.auto_launch);
    if policy == Ok(AutoLaunch::Never) || !super::widget::graphical_session() {
        return;
    }
    let absent = runtime
        .run(client.ci_runners(RunnerAction::List))
        .is_ok_and(|r| r.runners.widget == WidgetPresence::Absent);
    if let (true, Some(binary)) = (absent, super::widget::widget_binary()) {
        let _ = super::widget::spawn_detached(&binary, &state_dir, true);
    }
}

/// Wait for a run, then print its full record and return its exit code
/// (2 when the deadline passes first).
fn finish(runtime: &Runtime, client: &Client, run: &str, flags: &Flags) -> Result<i32, Failure> {
    let deadline = flags
        .number::<u64>("--deadline-ms")?
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    loop {
        let shown = call(runtime, client.ci_show(run.into(), true))?;
        let expired = deadline.is_some_and(|d| Instant::now() >= d);
        if let Some(code) = shown
            .exit_code
            .filter(|_| shown.record.state == RunState::Done)
        {
            print(&shown, flags.json(), print_tree);
            return Ok(code);
        }
        if expired {
            print(&shown, flags.json(), print_tree);
            return Ok(EXIT_NOT_FINISHED);
        }
        std::thread::sleep(POLL);
    }
}

fn wait(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(arguments, &["--state-dir", "--deadline-ms"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    finish(&runtime, &client, &flags.run()?, &flags)
}

fn list(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(
        arguments,
        &["--state-dir", "--workspace", "--state", "--limit"],
        &["--json"],
    )?;
    let state = flags.get("--state").map(RunState::parse).transpose()?;
    let workspace = flags
        .get("--workspace")
        .map(|w| std::fs::canonicalize(w).map_err(|_| "workspace does not exist".to_string()))
        .transpose()?
        .map(|w| w.to_string_lossy().into_owned());
    let (runtime, client) = connect(&flags)?;
    let listed = call(
        &runtime,
        client.ci_list(workspace, state, flags.number("--limit")?),
    )?;
    print(&listed, flags.json(), |l| {
        for view in &l.runs {
            let run = &view.record;
            println!(
                "{}  {:<8} {:<10} {}{}  {}  {}",
                run.id,
                run.state.as_str(),
                run.conclusion.map_or("-", Conclusion::as_str),
                &run.sha[..12.min(run.sha.len())],
                if run.dirty.is_some() { "+dirty" } else { "" },
                run.workflow,
                run.actor,
            );
        }
        print_runners(&l.runners);
    });
    Ok(0)
}

fn show(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(arguments, &["--state-dir"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    let shown = call(&runtime, client.ci_show(flags.run()?, true))?;
    print(&shown, flags.json(), print_tree);
    Ok(0)
}

fn logs(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(
        arguments,
        &["--state-dir", "--job", "--step", "--since-seq", "--limit"],
        &["--json", "--follow"],
    )?;
    let follow = flags.has("--follow");
    // `--limit` without `--follow` is one bounded page (agents page by cursor).
    let one_page = flags.get("--limit").is_some() && !follow;
    let mut query = LogsQuery {
        run: flags.run()?,
        job: flags.get("--job").map(str::to_string),
        section: flags.get("--step").map(str::to_string),
        since_seq: flags.number("--since-seq")?.unwrap_or(0),
        limit: flags.number("--limit")?,
        max_bytes: None,
    };
    let (runtime, client) = connect(&flags)?;
    loop {
        let page = call(&runtime, client.ci_logs(query.clone()))?;
        for record in &page.records {
            if flags.json() {
                println!("{}", record.to_json());
            } else {
                println!("{}", record.text);
            }
        }
        query.since_seq = page.next_seq;
        if one_page {
            if page.more {
                eprintln!(
                    "more: bosn ci logs {} --since-seq {}",
                    query.run, page.next_seq
                );
            }
            return Ok(0);
        }
        if !page.more && (!follow || page.done) {
            return Ok(0);
        }
        if !page.more {
            std::thread::sleep(POLL);
        }
    }
}

fn report(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(arguments, &["--state-dir", "--tail"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    let report = call(
        &runtime,
        client.ci_report(flags.run()?, flags.number("--tail")?),
    )?;
    print(&report, flags.json(), |r| {
        let conclusion = r.conclusion.map_or("not finished", Conclusion::as_str);
        println!("run {}: {conclusion}", r.run);
        if let Some(reason) = &r.reason {
            println!("reason: {reason}");
        }
        if let Some(failure) = &r.first_failure {
            println!(
                "first failure: job {} step {} (exit {})",
                failure.job,
                failure.step.as_deref().unwrap_or("-"),
                failure.exit_code.map_or("-".into(), |c| c.to_string()),
            );
            for line in &failure.tail {
                println!("  | {line}");
            }
        }
        for (kind, jobs) in [
            ("skipped", &r.jobs.skipped),
            ("unsupported", &r.jobs.unsupported),
        ] {
            if !jobs.is_empty() {
                println!("{kind}: {}", jobs.join(", "));
            }
        }
    });
    Ok(report.exit_code.unwrap_or(EXIT_NOT_FINISHED))
}

fn retry(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(arguments, &["--state-dir", "--job"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    let retried = call(
        &runtime,
        client.ci_retry(flags.run()?, flags.get("--job").map(str::to_string)),
    )?;
    print(&retried, flags.json(), |r| println!("run {} queued", r.run));
    Ok(0)
}

/// `bosn ui [--path P] [--print] [--browser]`: a single-use dashboard link,
/// opened in the system browser (the native widget window comes later).
pub fn run_ui(arguments: impl Iterator<Item = OsString>) {
    let result = (|| -> Result<i32, Failure> {
        let flags = Flags::parse(
            arguments,
            &["--state-dir", "--path"],
            &["--print", "--browser"],
        )?;
        let (runtime, client) = connect(&flags)?;
        let grant = call(
            &runtime,
            client.ci_ui_grant(flags.get("--path").map(str::to_string)),
        )?;
        maybe_start_widget(&runtime, &client, &flags);
        if flags.has("--print") {
            println!("{}", grant.url);
            return Ok(0);
        }
        open_url(&grant.url).map_err(|e| Failure::error(format!("cannot open a browser: {e}")))?;
        eprintln!("bosn ui: opened the dashboard in your browser (single-use link)");
        Ok(0)
    })();
    match result {
        Ok(code) => std::process::exit(code),
        Err(failure) => refuse(failure),
    }
}

/// The platform's URL opener, detached from this terminal.
fn open_url(url: &str) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    } else {
        std::process::Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Exit 1 when nothing was cancelled (already finished).
fn cancel(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(arguments, &["--state-dir"], &["--json"])?;
    let (runtime, client) = connect(&flags)?;
    let reply = call(&runtime, client.ci_cancel(flags.run()?))?;
    print(&reply, flags.json(), |r| {
        let what = if r.cancelled {
            "cancelled"
        } else {
            "not cancelled"
        };
        println!("run {} {what} ({})", r.run, r.state.as_str());
    });
    Ok(if reply.cancelled { 0 } else { EXIT_ERROR })
}

fn runners(arguments: impl Iterator<Item = OsString>) -> Result<i32, Failure> {
    let flags = Flags::parse(
        arguments,
        &["--state-dir", "--older-than-secs", "--max-bytes"],
        &["--json"],
    )?;
    let positional: Vec<&str> = flags.positional.iter().map(String::as_str).collect();
    let action = match positional[..] {
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
    let reply = call(&runtime, client.ci_runners(action))?;
    print(&reply, flags.json(), |r| {
        print_runners(&r.runners);
        if let Some(pruned) = &r.pruned_runs {
            println!("pruned runs: {}", pruned.len());
        }
    });
    Ok(0)
}

fn print_runners(runners: &RunnerStatus) {
    println!(
        "runners: {} running, {} queued, limit {}{}",
        runners.running,
        runners.queued,
        runners.limit,
        if runners.drained { " (drained)" } else { "" }
    );
}

fn print_tree(view: &RunView) {
    let run = &view.record;
    println!(
        "run {} {} {}  sha {}{}  {}",
        run.id,
        run.state.as_str(),
        run.conclusion.map_or("-", Conclusion::as_str),
        run.sha,
        if run.dirty.is_some() { " +dirty" } else { "" },
        run.workflow,
    );
    if let Some(reason) = &run.reason {
        println!("  reason: {reason}");
    }
    for group in &run.tree.groups {
        println!("  stage {}", group.name);
        for job in &group.jobs {
            println!("    {} {}", mark(job.conclusion, job.status), job.key);
            for section in &job.sections {
                println!(
                    "      {} {} {}",
                    mark(section.conclusion, section.status),
                    section.stage,
                    section.name
                );
            }
        }
    }
}

/// Status by symbol as well as word, never colour alone.
fn mark(conclusion: Option<ItemConclusion>, status: ItemStatus) -> &'static str {
    match (conclusion, status) {
        (Some(ItemConclusion::Success), _) => "[ok]",
        (Some(ItemConclusion::Failure), _) => "[FAIL]",
        (Some(ItemConclusion::Cancelled), _) => "[cancelled]",
        (Some(ItemConclusion::Skipped), _) => "[skip]",
        (Some(ItemConclusion::Unsupported), _) => "[unsupported]",
        (None, ItemStatus::InProgress) => "[..]",
        (None, _) => "[queued]",
    }
}
