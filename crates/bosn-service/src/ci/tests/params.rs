//! #430: a run's inputs, matrix filter and env are validated by the daemon,
//! recorded, part of what makes two submissions the same run, and in the
//! event payload act reads.

use super::*;
use crate::ci::params::RunParams;

fn installer(staging: &str) -> SubmitRequest {
    let mut params = RunParams::default();
    params.add_input("release_tag=2.8.25").unwrap();
    params.add_input("mode=candidate").unwrap();
    params
        .add_matrix("target:x86_64-unknown-linux-musl")
        .unwrap();
    params.add_env("PYTEST_ADDOPTS=-s").unwrap();
    SubmitRequest {
        trigger: Trigger::WorkflowCall,
        workflow: ".github/workflows/installer-check.yml".into(),
        job: Some("public-host".into()),
        params,
        ..request(staging, 'a')
    }
}

async fn submit_request(runtime: &CiRuntime, request: SubmitRequest) -> SubmitReply {
    call(
        runtime,
        CiRequest::Submit {
            request: Box::new(request),
        },
    )
    .await
}

#[test]
fn params_are_recorded_and_distinguish_runs() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            hang: true,
            ..Faults::default()
        }));
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 1);
        let staging = new_uuid().await.unwrap();
        staged(&runtime, &staging);
        let first = submit_request(&runtime, installer(&staging)).await;
        let record = runtime.record(&first.run).unwrap();
        assert_eq!(record.event, "workflow_call");
        assert_eq!(record.params, installer("s").params);
        assert_eq!(first.record.record.params, record.params);
        // Another leg of the matrix is another run.
        let staging = new_uuid().await.unwrap();
        staged(&runtime, &staging);
        let mut other = installer(&staging);
        other
            .params
            .matrix
            .insert("target".into(), "aarch64-unknown-linux-musl".into());
        let second = submit_request(&runtime, other).await;
        assert!(!second.coalesced);
        assert_ne!(first.run, second.run);
        // The same leg again joins the first.
        let staging = new_uuid().await.unwrap();
        staged(&runtime, &staging);
        let again = submit_request(&runtime, installer(&staging)).await;
        assert!(again.coalesced);
        assert_eq!(again.run, first.run);
        for run in [&first.run, &second.run] {
            let _: CancelReply = call(&runtime, CiRequest::Cancel { run: run.clone() }).await;
            wait_done(&runtime, run).await;
        }
    });
}

#[test]
fn the_daemon_refuses_a_secret_in_env_and_inputs_without_an_event() {
    let mut secret = installer("aaaaaaaa-bbbb-4ccc-8ddd-000000000000");
    secret.params.env.insert("GH_TOKEN".into(), "x".into());
    assert!(secret.validate().is_err());
    let mut push = installer("aaaaaaaa-bbbb-4ccc-8ddd-000000000000");
    push.trigger = Trigger::Push;
    assert!(push.validate().is_err());
    assert!(
        installer("aaaaaaaa-bbbb-4ccc-8ddd-000000000000")
            .validate()
            .is_ok()
    );
    // A request without params keeps its old wire shape.
    let plain = request("aaaaaaaa-bbbb-4ccc-8ddd-000000000000", 'a');
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("params")
            .is_none()
    );
}
