//! Exercise the production controller on a private Docker volume.
use super::*;
use crate::ci::engine::{CONTROL_DEADLINE, engine_image};
use kernal_api::async_engine;

#[test]
#[ignore = "requires isolated Docker"]
fn controller_preserves_warm_actions_and_retires_pressure_cache() {
    assert_eq!(std::env::var("BOSN_TEST_ISOLATED").as_deref(), Ok("1"));
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let backend = DockerActBackend::default();
            let nonce = crate::ci::new_uuid().await.unwrap();
            let volume = format!("bosn-actions-proof-{nonce}");
            backend
                .checked(
                    "action proof volume",
                    owned(&["volume", "create", &volume]),
                    CONTROL_DEADLINE,
                )
                .await
                .unwrap();
            let mount = format!("type=volume,source={volume},target={ENGINE_CACHE}");
            let image = engine_image();
            let id = backend
                .checked(
                    "action proof helper",
                    owned(&[
                        "run",
                        "-d",
                        "--network",
                        "none",
                        "--read-only",
                        "--cap-drop",
                        "ALL",
                        "--cap-add",
                        "DAC_OVERRIDE",
                        "--mount",
                        &mount,
                        "--entrypoint",
                        "sleep",
                        &image,
                        "300",
                    ]),
                    CONTROL_DEADLINE,
                )
                .await
                .unwrap();
            let result = exercise(&backend, &id).await;
            backend
                .checked(
                    "action proof helper cleanup",
                    owned(&["rm", "-f", &id]),
                    CONTROL_DEADLINE,
                )
                .await
                .unwrap();
            backend.confirm_measurement_absent(&id).await.unwrap();
            backend
                .checked(
                    "action proof volume cleanup",
                    owned(&["volume", "rm", &volume]),
                    CONTROL_DEADLINE,
                )
                .await
                .unwrap();
            assert!(!backend.volume_exists(&volume).await.unwrap());
            result.unwrap();
        });
}

async fn exercise(backend: &DockerActBackend, id: &str) -> Result<(), String> {
    backend.checked("action proof payload", DockerActBackend::exec(id,
        "mkdir -p /bosn/cache/actcache /bosn/cache/actions; dd if=/dev/zero of=/bosn/cache/actions/payload bs=1024 count=128 2>/dev/null"), CONTROL_DEADLINE).await?;
    let mut session = Session::open(backend, id).await?;
    let warm = session.maintain(1024 * 1024).await?;
    assert!(warm.allocated_before > 65536);
    assert_eq!(warm.allocated_before, warm.allocated_after);
    assert_eq!(warm.retired_classes, 0);
    let pressure = session.maintain(65536).await?;
    assert_eq!(pressure.allocated_before, warm.allocated_after);
    assert_eq!(pressure.allocated_after, 0);
    assert_eq!(pressure.retired_classes, 1);
    let empty = session.maintain(65536).await?;
    assert_eq!(empty.allocated_before, 0);
    assert_eq!(empty.retired_classes, 0);
    session.io.send(b"abort\n").await?;
    backend.checked("action proof absence", DockerActBackend::exec(id,
        "test ! -e /bosn/cache/actions; test ! -e /bosn/cache/.bosn-actions-retirement-v1.json"), CONTROL_DEADLINE).await?;
    Ok(())
}
