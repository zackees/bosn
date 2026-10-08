//! A real uncoordinated read-only attachment must prevent retirement.
use super::*;
use kernal_api::async_engine;

#[test]
#[ignore = "requires isolated Docker"]
fn readonly_consumer_refuses_retirement_and_remains_alive() {
    assert_eq!(std::env::var("BOSN_TEST_ISOLATED").as_deref(), Ok("1"));
    async_engine::RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let backend = DockerActBackend::default();
            let volume = format!("bosn-reader-proof-{}", crate::ci::new_uuid().await.unwrap());
            backend
                .checked(
                    "reader proof volume",
                    owned(&["volume", "create", &volume]),
                    CONTROL_DEADLINE,
                )
                .await
                .unwrap();
            let mount = format!("type=volume,source={volume},target=/bosn/cache,readonly");
            let image = crate::ci::engine::engine_image();
            let id = backend
                .checked(
                    "reader proof container",
                    owned(&[
                        "run",
                        "-d",
                        "--network",
                        "none",
                        "--read-only",
                        "--cap-drop",
                        "ALL",
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
            let writer_census = backend
                .require_coordinated_cache_access(&volume, AccessScope::Writers)
                .await;
            let retirement_census = backend
                .require_coordinated_cache_access(&volume, AccessScope::AllAttachments)
                .await;
            let alive = backend
                .checked(
                    "reader remains alive",
                    DockerActBackend::exec(&id, "test -d /bosn/cache"),
                    CONTROL_DEADLINE,
                )
                .await;
            backend
                .checked(
                    "reader proof container cleanup",
                    owned(&["rm", "-f", &id]),
                    CONTROL_DEADLINE,
                )
                .await
                .unwrap();
            backend.confirm_measurement_absent(&id).await.unwrap();
            backend
                .checked(
                    "reader proof volume cleanup",
                    owned(&["volume", "rm", &volume]),
                    CONTROL_DEADLINE,
                )
                .await
                .unwrap();
            assert!(!backend.volume_exists(&volume).await.unwrap());
            writer_census.unwrap();
            assert!(
                retirement_census
                    .unwrap_err()
                    .contains("uncoordinated attachment")
            );
            alive.unwrap();
        });
}
