use super::*;

#[test]
fn invocation_never_names_a_host_socket_and_pins_runners() {
    let args = ActInvocation {
        event: "push".into(),
        workflow: ".github/workflows/ci.yml".into(),
        workflow_overlaid: false,
        job: Some("lint".into()),
        cache_route: crate::ci::cache_cohort::CacheRoute::Legacy(
            crate::ci::cache_cohort::Namespace::parse("0123456789abcdef").unwrap(),
        ),
        secrets: SecretEnv(vec![("GITHUB_TOKEN".into(), "ghp_secretvalue".into())]),
        params: Default::default(),
    }
    .args();
    assert!(args.iter().all(|a| !a.contains("docker.sock")));
    assert!(args.contains(&format!("ubuntu-latest={}", runner_tag())));
    assert!(args.contains(&format!("{ENGINE_CACHE}/actcache/0123456789abcdef")));
    let cache = format!("{ENGINE_CACHE}/actions");
    let flags = ["--action-cache-path", &cache, "--use-new-action-cache"];
    assert!(args.windows(3).any(|w| w == flags), "zackees/clud#1724");
    assert!(args.windows(2).any(|w| w == ["-s", "GITHUB_TOKEN"]));
    assert!(
        args.iter().all(|a| !a.contains("ghp_secretvalue")),
        "no value in argv"
    );
    assert!(args.ends_with(&["-j".to_string(), "lint".to_string()]));
    assert!(RUNNER_IMAGE.contains("@sha256:") && engine_image().contains("@sha256:"));
    assert!(act_artifact("x86_64").is_some() && act_artifact("aarch64").is_none());
    let secrets = SecretEnv(vec![("GITHUB_TOKEN".into(), "ghp_secretvalue".into())]);
    assert!(
        !format!("{secrets:?}").contains("ghp_"),
        "Debug shows names only"
    );
}

/// #424: act plans bosn's rewrite of the workflow and reads rewritten
/// reusable workflows and actions from the overlay; jobs never see it.
#[test]
fn act_reads_rewrites_from_the_overlay() {
    let mut invocation = ActInvocation {
        event: "push".into(),
        workflow: ".github/workflows/ci.yml".into(),
        workflow_overlaid: false,
        job: None,
        cache_route: crate::ci::cache_cohort::CacheRoute::Legacy(
            crate::ci::cache_cohort::Namespace::parse("0123456789abcdef").unwrap(),
        ),
        secrets: SecretEnv::default(),
        params: Default::default(),
    };
    let overlay = format!("{ENGINE_WORK}/overlay");
    let args = invocation.args();
    assert!(
        args.windows(2)
            .any(|w| w == ["-W", ".github/workflows/ci.yml"])
    );
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--workflow-overlay" && w[1] == overlay)
    );
    invocation.workflow_overlaid = true;
    let args = invocation.args();
    let planned = format!("{overlay}/.github/workflows/ci.yml");
    assert!(args.windows(2).any(|w| w[0] == "-W" && w[1] == planned));
    assert!(work_dirs_script().contains(&overlay));
}

#[test]
fn cache_scripts_verify_the_pinned_act_and_save_atomically() {
    let act = act_artifact("x86_64").unwrap();
    let install = install_act_script(act);
    assert!(install.contains(act.sha256) && install.contains(act.url));
    assert!(install.contains("sha256sum -c"));
    let load = load_runner_script();
    assert!(load.contains("docker load") && load.contains(RUNNER_IMAGE));
    assert!(load.contains("mv \"$stage\" \"$tar\""), "atomic rename");
    let reload = reload_runner_script();
    assert!(
        reload.contains(&format!("rm -f {}", runner_tar()))
            && reload.ends_with(&load_runner_body())
    );
    assert!(
        install.contains(act.binary_sha256),
        "the extracted binary is checked too"
    );
    let cache = CacheVolume::machine("11111111-2222-4333-8444-555555555555", 1.0).unwrap();
    assert_eq!(cache.name, CACHE_VOLUME);
    assert!(
        bosn_core::REQUIRED_LABELS
            .iter()
            .all(|k| cache.labels.contains_key(*k))
    );
}

#[test]
fn line_buffer_splits_and_bounds_lines() {
    use lines::Line::{Truncated, Whole};
    let mut b = LineBuffer::default();
    b.push(b"one\ntw");
    assert_eq!(b.drain_lines(), [Whole("one".into())]);
    b.push(b"o\n");
    assert_eq!(b.drain_lines(), [Whole("two".into())]);
    // #563: a ~70 KB act JSON line (a large output-evidence event) arrives whole.
    let event = format!("{{\"msg\":\"{}\"}}", "y".repeat(70 * 1024));
    // It streams in before its newline does, as pipe-sized reads deliver it.
    b.push(event.as_bytes());
    assert_eq!(b.drain_lines(), [], "an unfinished line is held, not cut");
    b.push(b"\n");
    assert_eq!(b.drain_lines(), [Whole(event)]);
    // A line past the bound is cut once; the rest of it, up to its newline, is dropped.
    b.push(&vec![b'x'; MAX_LINE + 5]);
    let cut = b.drain_lines();
    assert!(matches!(&cut[..], [Truncated(text)] if text.len() == MAX_LINE));
    b.push(b"xxxxx\nnext");
    assert_eq!(b.drain_lines(), []);
    assert_eq!(b.finish(), [Whole("next".into())]);
}
