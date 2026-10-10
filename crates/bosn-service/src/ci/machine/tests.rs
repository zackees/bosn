use super::*;

fn holder(run: &str, pid: u32) -> Holder {
    Holder {
        run: run.into(),
        registry: "0a1b2c3d-4e5f-4a6b-8c7d-000000000001".into(),
        daemon: Daemon {
            boot: "b00t".into(),
            pid,
            start: 7,
        },
    }
}

#[test]
fn this_process_is_alive_and_a_reused_pid_is_not() {
    let me = Daemon::current();
    assert!(me.alive());
    let reused = Daemon {
        start: me.start + 1,
        ..me.clone()
    };
    assert!(!reused.alive(), "same pid, different start time");
    let elsewhere = Daemon {
        boot: "another-boot".into(),
        start: 0,
        ..me
    };
    assert!(elsewhere.alive(), "another boot cannot be checked");
}

#[test]
fn a_claim_round_trips_through_its_labels() {
    let claim = EngineClaim {
        id: "c1".into(),
        engine: "bosn-act-0a1b2c3d-4e5f-4a6b-8c7d-000000000002".into(),
        registry: "r".into(),
        daemon: Daemon::current(),
        created: 42,
    };
    assert_eq!(
        EngineClaim::from_labels("c1", &claim.labels()),
        Ok(claim.clone())
    );
    assert!(claim.ours("r"));
    assert!(!claim.ours("other"));
    let mut foreign = claim.labels();
    foreign.remove(CLAIM_LABEL);
    assert!(EngineClaim::from_labels("c1", &foreign).is_err());
}

#[test]
fn slot_replies_parse_eagerly() {
    assert_eq!(reply::leased("slot 3\n"), Ok(Leased::Slot(3)));
    assert_eq!(reply::leased("retiring"), Ok(Leased::Retiring));
    assert_eq!(reply::leased("full"), Ok(Leased::Full));
    assert!(reply::leased("slot x").is_err());
    let line = holder("run-a", 12).line();
    let survey = reply::survey(&format!("held 0 {line}\nidle 30\nrequested\n")).unwrap();
    assert_eq!(survey.held, vec![(0, holder("run-a", 12))]);
    assert_eq!(survey.idle_secs, 30);
    assert!(survey.requested && !survey.retiring);
    assert!(reply::survey("held 0 short\nidle 1").is_err());
    assert!(
        reply::survey("retiring").is_err(),
        "an idle time is required"
    );
}

/// The in-engine script itself, run by the host's `sh` against a private
/// slot directory: leases are distinct, retirement waits for the last slot,
/// and a retiring engine leases nothing.
#[cfg(target_os = "linux")]
#[test]
fn the_slot_script_leases_and_retires_under_one_lock() {
    if std::process::Command::new("flock")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let script = include_str!("slots.sh")
        .replace("d=/run/bosn-slots", &format!("d={}", dir.path().display()));
    let run = |args: &[&str]| {
        let out = std::process::Command::new("sh")
            .arg("-ec")
            .arg(&script)
            .arg("sh")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let line = holder("run-a", 12).line();
    let mut args = vec!["lease", "2"];
    args.extend(line.split(' '));
    assert_eq!(reply::leased(&run(&args)), Ok(Leased::Preparing));
    run(&["ready"]);
    assert_eq!(reply::leased(&run(&args)), Ok(Leased::Slot(0)));
    assert_eq!(reply::leased(&run(&args)), Ok(Leased::Slot(1)));
    assert_eq!(reply::leased(&run(&args)), Ok(Leased::Full));
    run(&["release", "0"]);
    let survey = reply::survey(&run(&["retire"])).unwrap();
    assert_eq!(survey.held.len(), 1);
    assert!(!survey.retiring, "a held slot keeps the engine");
    run(&["release", "1"]);
    run(&["request"]);
    let survey = reply::survey(&run(&["retire"])).unwrap();
    assert!(survey.held.is_empty() && survey.retiring && survey.requested);
    assert_eq!(reply::leased(&run(&args)), Ok(Leased::Retiring));
}
