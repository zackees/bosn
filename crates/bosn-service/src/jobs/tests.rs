//! Admission policy: coalescing, slots, fairness, leases, stalls and retention.
use super::*;

#[test]
fn joins_supersedes_and_never_rejoins_a_cancelling_active_job() {
    let mut jobs = Jobs::new(1);
    assert_eq!(jobs.submit("w", "s", "a").unwrap(), Submission::Started(1));
    assert_eq!(jobs.submit("w", "s", "a").unwrap(), Submission::Joined(1));
    assert_eq!(jobs.submit("w", "s", "b").unwrap(), Submission::Queued(2));
    assert_eq!(jobs.submit("w", "s", "c").unwrap(), Submission::Queued(3));
    assert_eq!(jobs.jobs[&2].state, JobState::Superseded);
    jobs.cancel(1).unwrap();
    assert_eq!(jobs.submit("w", "s", "a").unwrap(), Submission::Queued(4));
    jobs.settle(1, false).unwrap();
    assert_eq!(jobs.jobs[&4].state, JobState::Running);
}

#[test]
fn cap_cursors_and_shutdown_are_bounded() {
    let mut jobs = Jobs::new(1);
    let first = match jobs.submit("a", "s", "x").unwrap() {
        Submission::Started(id) => id,
        _ => unreachable!(),
    };
    let second = match jobs.submit("b", "s", "x").unwrap() {
        Submission::Queued(id) => id,
        _ => unreachable!(),
    };
    assert_eq!(jobs.jobs[&second].state, JobState::Queued);
    jobs.max_logs = 2;
    jobs.log(first, "one".into()).unwrap();
    jobs.log(first, "two".into()).unwrap();
    jobs.log(first, "three".into()).unwrap();
    let (start, records) = jobs.logs(first, 0).unwrap();
    assert_eq!(start, 1);
    assert_eq!(records, vec![(1, "two".into()), (2, "three".into())]);
    jobs.shutdown();
    assert_eq!(jobs.jobs[&second].state, JobState::Cancelled);
    assert!(matches!(jobs.submit("c", "s", "x"), Err(JobError::Closing)));
}
#[test]
fn log_page_reports_eviction_gap_and_bounded_next_cursor() {
    let mut jobs = Jobs::new(1);
    let id = match jobs.submit("w", "s", "d").unwrap() {
        Submission::Started(id) => id,
        _ => unreachable!(),
    };
    jobs.max_logs = 2;
    for line in ["a", "b", "c"] {
        jobs.log(id, line.into()).unwrap();
    }
    let page = jobs.log_page(id, 0, 1).unwrap();
    assert!(page.gap);
    assert_eq!(page.retained_from, 1);
    assert_eq!(page.records, vec![(1, "b".into())]);
    assert_eq!(page.next, 2);
}

#[test]
fn a_follow_lease_expires_for_queued_and_running_jobs_until_polled() {
    let mut jobs = Jobs::new(1);
    let start = Instant::now();
    let lease = Duration::from_secs(30);
    let running = match jobs.submit("a", "s", "x").unwrap() {
        Submission::Started(id) => id,
        other => panic!("{other:?}"),
    };
    let queued = match jobs.submit("b", "s", "x").unwrap() {
        Submission::Queued(id) => id,
        other => panic!("{other:?}"),
    };
    let unleased = match jobs.submit("c", "s", "x").unwrap() {
        Submission::Queued(id) => id,
        other => panic!("{other:?}"),
    };
    jobs.lease(running, lease, start);
    jobs.lease(queued, lease, start);
    assert!(jobs.expired_leases(start + lease).is_empty(), "not yet");

    // Polling one job renews only that job's lease.
    jobs.touch(running, start + Duration::from_secs(20));
    let later = start + Duration::from_secs(31);
    assert_eq!(jobs.expired_leases(later), vec![queued]);
    let much_later = start + Duration::from_secs(51);
    assert_eq!(jobs.expired_leases(much_later), vec![running, queued]);
    assert!(!jobs.expired_leases(much_later).contains(&unleased));

    // A queued job that expires is cancelled before it ever starts.
    jobs.cancel(queued).unwrap();
    assert_eq!(jobs.jobs[&queued].state, JobState::Cancelled);
    // A running one is left to its executor once it is cancelling.
    jobs.cancel(running).unwrap();
    assert!(jobs.expired_leases(much_later).is_empty());
    jobs.settle(running, false).unwrap();
    assert!(jobs.leases.is_empty(), "finished jobs forget their lease");

    // The slot passes to the unleased job, which never expires.
    assert_eq!(jobs.jobs[&unleased].state, JobState::Running);
    assert!(
        jobs.expired_leases(start + Duration::from_secs(3600))
            .is_empty()
    );

    // A second follower can only shorten a lease; a finished job cannot
    // be leased again.
    let next = match jobs.submit("d", "s", "x").unwrap() {
        Submission::Queued(id) => id,
        other => panic!("{other:?}"),
    };
    jobs.lease(next, lease, start);
    jobs.lease(next, Duration::from_secs(5), start);
    assert_eq!(
        jobs.expired_leases(start + Duration::from_secs(6)),
        vec![next]
    );
    jobs.cancel(next).unwrap();
    jobs.lease(next, lease, start);
    assert!(jobs.leases.is_empty());
}

#[test]
fn log_records_are_bounded_before_they_can_exceed_the_ipc_frame() {
    let mut jobs = Jobs::new(1);
    let id = match jobs.submit("w", "s", "d").unwrap() {
        Submission::Started(id) => id,
        _ => unreachable!(),
    };
    assert!(matches!(
        jobs.log(id, "x".repeat(MAX_LOG_LINE_BYTES + 1)),
        Err(JobError::LogTooLarge)
    ));
}

fn started(submission: Submission) -> u64 {
    match submission {
        Submission::Started(id) => id,
        other => panic!("expected a start, got {other:?}"),
    }
}
fn queued(submission: Submission) -> u64 {
    match submission {
        Submission::Queued(id) => id,
        other => panic!("expected a queue, got {other:?}"),
    }
}

#[test]
fn distinct_workspaces_run_in_parallel_up_to_the_runner_cap() {
    // #358 RED: with Jobs::new(1) workspace B stayed Queued while A ran.
    let mut jobs = Jobs::with_policy(SchedulerPolicy {
        runner_slots: 3,
        control_slots: 1,
    });
    let a = started(jobs.submit_class("a", "t", "x", JobClass::Runner).unwrap());
    let b = started(jobs.submit_class("b", "t", "x", JobClass::Runner).unwrap());
    let c = started(jobs.submit_class("c", "t", "x", JobClass::Runner).unwrap());
    let d = queued(jobs.submit_class("d", "t", "x", JobClass::Runner).unwrap());
    assert_eq!(
        [a, b, c].map(|id| jobs.jobs[&id].slot),
        [Some(0), Some(1), Some(2)]
    );
    assert_eq!(jobs.take_started(), vec![a, b, c]);
    // The freed slot index is reused by the next admission.
    jobs.settle(b, true).unwrap();
    assert_eq!(jobs.jobs[&d].state, JobState::Running);
    assert_eq!(jobs.jobs[&d].slot, Some(1));
    assert_eq!(jobs.jobs[&b].slot, None, "a finished job holds no slot");
}

#[test]
fn a_full_runner_lane_never_blocks_a_control_job() {
    let mut jobs = Jobs::with_policy(SchedulerPolicy {
        runner_slots: 1,
        control_slots: 1,
    });
    started(
        jobs.submit_class("a", "act", "x", JobClass::Runner)
            .unwrap(),
    );
    queued(
        jobs.submit_class("b", "act", "x", JobClass::Runner)
            .unwrap(),
    );
    // Another session's ensure starts at once despite the runner queue.
    started(jobs.submit("c", "ensure", "x").unwrap());
    assert_eq!(jobs.load()[&JobClass::Runner], (1, 1));
    assert_eq!(jobs.load()[&JobClass::Control], (0, 1));
}

#[test]
fn admission_is_round_robin_across_workspaces() {
    let mut jobs = Jobs::new(1);
    let first = started(jobs.submit_class("a", "s0", "x", JobClass::Runner).unwrap());
    // Workspace a floods the queue before b and c submit one job each.
    let a: Vec<u64> = (1..=3)
        .map(|n| {
            queued(
                jobs.submit_class("a", &format!("s{n}"), "x", JobClass::Runner)
                    .unwrap(),
            )
        })
        .collect();
    let b = queued(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
    let c = queued(jobs.submit_class("c", "s", "x", JobClass::Runner).unwrap());
    let mut order = Vec::new();
    let mut running = first;
    for _ in 0..5 {
        jobs.settle(running, true).unwrap();
        running = jobs
            .jobs
            .values()
            .find(|job| job.state == JobState::Running)
            .unwrap()
            .id;
        order.push(running);
    }
    assert_eq!(order, vec![b, c, a[0], a[1], a[2]]);
}

#[test]
fn a_cancelled_or_superseded_queue_entry_is_skipped() {
    let mut jobs = Jobs::new(1);
    let a = started(jobs.submit_class("a", "s", "x", JobClass::Runner).unwrap());
    let b = queued(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
    let c = queued(jobs.submit_class("c", "s", "x", JobClass::Runner).unwrap());
    jobs.cancel(b).unwrap();
    jobs.settle(a, true).unwrap();
    assert_eq!(jobs.jobs[&c].state, JobState::Running);
    assert_eq!(jobs.jobs[&c].slot, Some(0));
}

#[test]
fn stalled_reports_running_runner_jobs_without_recent_progress() {
    let mut jobs = Jobs::new(2);
    let quiet = started(jobs.submit_class("a", "s", "x", JobClass::Runner).unwrap());
    let chatty = started(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
    let control = started(jobs.submit("c", "s", "x").unwrap());
    let now = Instant::now();
    let later = now + Duration::from_secs(120);
    jobs.jobs.get_mut(&chatty).unwrap().last_progress = Some(later);
    let after = Duration::from_secs(60);
    assert_eq!(jobs.stalled(later, after, |_| None), vec![quiet]);
    // Docker activity counts as progress too.
    assert!(
        jobs.stalled(later, after, |id| (id == quiet).then_some(later))
            .is_empty()
    );
    // A cancelling job is already being torn down; a control job is not
    // subject to stall teardown.
    jobs.cancel(quiet).unwrap();
    assert!(jobs.stalled(later, after, |_| None).is_empty());
    assert_eq!(jobs.jobs[&control].class, JobClass::Control);
    // A log line is progress.
    jobs.log(chatty, "tick".into()).unwrap();
    assert!(jobs.jobs[&chatty].last_progress.unwrap() >= now);
}

#[test]
fn finished_jobs_are_retained_boundedly() {
    let mut jobs = Jobs::new(1);
    let total = RETAINED_FINISHED_JOBS + 10;
    let mut ids = Vec::new();
    for n in 0..total {
        let id = started(
            jobs.submit_class(&format!("w{n}"), "s", "x", JobClass::Runner)
                .unwrap(),
        );
        jobs.settle(id, true).unwrap();
        ids.push(id);
    }
    // Too young to forget: everything is still pollable.
    assert_eq!(jobs.jobs.len(), total);
    for id in &ids[..10] {
        jobs.jobs.get_mut(id).unwrap().finished_at =
            Some(SystemTime::now() - RETAINED_FINISHED_MIN_AGE * 2);
    }
    let id = started(
        jobs.submit_class("late", "s", "x", JobClass::Runner)
            .unwrap(),
    );
    jobs.settle(id, true).unwrap();
    assert_eq!(jobs.jobs.len(), RETAINED_FINISHED_JOBS + 1);
    assert!(matches!(jobs.job(ids[0]), Err(JobError::Unknown)));
    assert!(jobs.job(ids[10]).is_ok());
    assert_eq!(jobs.snapshot(3).len(), 3);
}

#[test]
fn a_queued_job_whose_follower_is_gone_is_cancelled_not_started() {
    let mut jobs = Jobs::new(1);
    let start = Instant::now();
    let running = started(jobs.submit_class("a", "s", "x", JobClass::Runner).unwrap());
    let gone = queued(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
    let next = queued(jobs.submit_class("c", "s", "x", JobClass::Runner).unwrap());
    jobs.lease(gone, Duration::from_millis(1), start);
    std::thread::sleep(Duration::from_millis(5));
    jobs.settle(running, true).unwrap();
    assert_eq!(jobs.jobs[&gone].state, JobState::Cancelled);
    assert!(jobs.jobs[&gone].started_at.is_none(), "never started");
    assert_eq!(
        jobs.jobs[&next].state,
        JobState::Running,
        "the slot passes on"
    );
    assert_eq!(jobs.take_started(), vec![running, next]);
}
#[test]
fn a_quiet_followers_queued_job_waits_and_starts_when_it_polls_again() {
    let mut jobs = Jobs::new(1);
    let start = Instant::now();
    let running = started(jobs.submit_class("a", "s", "x", JobClass::Runner).unwrap());
    let quiet = queued(jobs.submit_class("b", "s", "x", JobClass::Runner).unwrap());
    let other = queued(jobs.submit_class("c", "s", "x", JobClass::Runner).unwrap());
    jobs.lease(quiet, Duration::from_millis(40), start);
    std::thread::sleep(Duration::from_millis(25));
    // Past half its lease: held back, and the slot goes to the next job.
    jobs.settle(running, true).unwrap();
    assert_eq!(jobs.jobs[&quiet].state, JobState::Queued);
    assert_eq!(jobs.jobs[&other].state, JobState::Running);
    jobs.settle(other, true).unwrap();
    assert_eq!(jobs.jobs[&quiet].state, JobState::Queued, "still quiet");
    // Its follower polls: it starts at once.
    jobs.touch(quiet, Instant::now());
    assert_eq!(jobs.jobs[&quiet].state, JobState::Running);
}
