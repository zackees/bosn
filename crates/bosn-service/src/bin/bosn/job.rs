//! `bosn job status|logs|cancel`.

use super::*;

/// Observe or cancel an existing daemon-owned job.  These commands neither
/// start the daemon nor invoke Docker; the authenticated client request is
/// their only side effect.
pub(crate) fn run_job(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let Some(command) = arguments.next() else {
        usage();
    };
    let invocation = match command.to_string_lossy().as_ref() {
        "status" => parse_job_status_arguments(arguments),
        "logs" => parse_job_logs_arguments(arguments),
        "cancel" => parse_job_cancel_arguments(arguments),
        _ => Err(()),
    };
    let invocation = match invocation {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    require_matching_daemon(
        invocation.state_dir(),
        &format!("job {}", invocation.action()),
        invocation.json(),
    );
    let client = match Client::for_state(invocation.state_dir()) {
        Ok(client) => client,
        Err(_) => job_failure(invocation.action(), invocation.json()),
    };
    let runtime = match RuntimeBuilder::current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => job_failure(invocation.action(), invocation.json()),
    };
    match invocation {
        JobInvocation::Status { job_id, json, .. } => {
            let status = match runtime.run(client.job_status(job_id)) {
                Ok(status) => status,
                Err(_) => job_failure("status", json),
            };
            print_job_status(&status, json);
        }
        JobInvocation::Logs {
            job_id,
            after,
            limit,
            json,
            ..
        } => {
            let page = match runtime.run(client.job_logs(job_id, after, limit)) {
                Ok(page) => page,
                Err(_) => job_failure("logs", json),
            };
            print_job_logs(job_id, &page, json);
        }
        JobInvocation::Cancel { job_id, json, .. } => {
            if runtime.run(client.cancel_job(job_id)).is_err() {
                job_failure("cancel", json);
            }
            if json {
                println!(
                    "{}",
                    json!({"action": "job_cancel", "cancelled": true, "job_id": job_id})
                );
            } else {
                println!("job cancelled");
                println!("job_id: {job_id}");
            }
        }
    }
}

pub(crate) enum JobInvocation {
    Status {
        state_dir: PathBuf,
        job_id: u64,
        json: bool,
    },
    Logs {
        state_dir: PathBuf,
        job_id: u64,
        after: u64,
        limit: u32,
        json: bool,
    },
    Cancel {
        state_dir: PathBuf,
        job_id: u64,
        json: bool,
    },
}

impl JobInvocation {
    pub(crate) fn state_dir(&self) -> &std::path::Path {
        match self {
            Self::Status { state_dir, .. }
            | Self::Logs { state_dir, .. }
            | Self::Cancel { state_dir, .. } => state_dir,
        }
    }

    pub(crate) fn json(&self) -> bool {
        match self {
            Self::Status { json, .. } | Self::Logs { json, .. } | Self::Cancel { json, .. } => {
                *json
            }
        }
    }

    pub(crate) fn action(&self) -> &'static str {
        match self {
            Self::Status { .. } => "status",
            Self::Logs { .. } => "logs",
            Self::Cancel { .. } => "cancel",
        }
    }
}

pub(crate) fn parse_job_status_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<JobInvocation, ()> {
    let (state_dir, job_id, json) = parse_job_base_arguments(arguments)?;
    Ok(JobInvocation::Status {
        state_dir,
        job_id,
        json,
    })
}

pub(crate) fn parse_job_cancel_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<JobInvocation, ()> {
    let (state_dir, job_id, json) = parse_job_base_arguments(arguments)?;
    Ok(JobInvocation::Cancel {
        state_dir,
        job_id,
        json,
    })
}

pub(crate) fn parse_job_logs_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<JobInvocation, ()> {
    let mut state_dir = None;
    let mut job_id = None;
    let mut after = None;
    let mut limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--job-id" => set_once_parsed(&mut job_id, arguments.next(), parse_job_id),
            "--after" => set_once_parsed(&mut after, arguments.next(), parse_u64),
            "--limit" => set_once_parsed(&mut limit, arguments.next(), parse_job_log_limit),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(JobInvocation::Logs {
        state_dir: state_dir.ok_or(())?,
        job_id: job_id.ok_or(())?,
        after: after.unwrap_or(0),
        limit: limit.unwrap_or(DEFAULT_JOB_LOG_LIMIT),
        json,
    })
}

pub(crate) fn parse_job_base_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(PathBuf, u64, bool), ()> {
    let mut state_dir = None;
    let mut job_id = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--job-id" => set_once_parsed(&mut job_id, arguments.next(), parse_job_id),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok((state_dir.ok_or(())?, job_id.ok_or(())?, json))
}

pub(crate) fn parse_state_dir(value: std::ffi::OsString) -> Result<PathBuf, ()> {
    let path = PathBuf::from(value);
    (!path.as_os_str().is_empty()).then_some(path).ok_or(())
}

/// A non-negative, finite age gate in seconds. NaN and infinity are refused rather than
/// silently treated as "no gate", which would make everything eligible.
pub(crate) fn parse_ttl_seconds(value: std::ffi::OsString) -> Result<f64, ()> {
    let seconds: f64 = value.to_str().ok_or(())?.parse().map_err(|_| ())?;
    (seconds.is_finite() && seconds >= 0.0)
        .then_some(seconds)
        .ok_or(())
}

pub(crate) fn parse_job_id(value: std::ffi::OsString) -> Result<u64, ()> {
    let id = parse_u64(value)?;
    (id > 0).then_some(id).ok_or(())
}

pub(crate) fn parse_job_log_limit(value: std::ffi::OsString) -> Result<u32, ()> {
    let limit = parse_u64(value)?;
    (1..=u64::from(MAX_JOB_LOG_LIMIT))
        .contains(&limit)
        .then_some(limit as u32)
        .ok_or(())
}

pub(crate) fn job_failure(action: &str, json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action": format!("job_{action}"), "error": "request failed"})
        );
    } else {
        eprintln!("bosn job {action}: request failed");
    }
    std::process::exit(1)
}

pub(crate) fn print_job_status(status: &JobStatus, json: bool) {
    if json {
        println!(
            "{}",
            json!({
                "action": "job_status",
                "job": {"id": status.id, "state": status.state, "error": status.error},
            })
        );
    } else {
        println!("job status");
        println!("job_id: {}", status.id);
        println!("state: {}", status.state);
        if let Some(error) = &status.error {
            println!("error: {error}");
        }
    }
}

pub(crate) fn print_job_logs(job_id: u64, page: &JobLogPage, json: bool) {
    if json {
        let records: Vec<_> = page
            .records
            .iter()
            .map(|record| json!({"cursor": record.cursor, "line": record.line}))
            .collect();
        println!(
            "{}",
            json!({
                "action": "job_logs",
                "job_id": job_id,
                "retained_from": page.retained_from,
                "next": page.next,
                "gap": page.gap,
                "records": records,
            })
        );
    } else {
        println!("job logs");
        println!("job_id: {job_id}");
        println!("retained_from: {}", page.retained_from);
        println!("next: {}", page.next);
        println!("gap: {}", page.gap);
        for record in &page.records {
            println!("{}: {}", record.cursor, record.line);
        }
    }
}
