//! Exercise the production controller on a private Docker volume.
use super::*;
use crate::ci::engine::{CONTROL_DEADLINE, engine_image};
use kernal_api::async_engine;

#[test]
#[ignore = "requires isolated Docker"]
fn controller_preserves_warm_actions_and_retires_pressure_cache() {
    live_case(DisposableClass::Actions);
}

#[test]
#[ignore = "requires isolated Docker"]
fn controller_preserves_warm_images_and_retires_pressure_cache() {
    live_case(DisposableClass::Images);
}

#[test]
#[ignore = "requires isolated Docker"]
fn controller_retires_tools_and_preserves_offline_archive() {
    live_case(DisposableClass::Tools);
}

fn live_case(class: DisposableClass) {
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
            let result = exercise(&backend, &id, class).await;
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

async fn exercise(
    backend: &DockerActBackend,
    id: &str,
    class: DisposableClass,
) -> Result<(), String> {
    backend.checked("action proof payload", DockerActBackend::exec(id,
        &format!("mkdir -p /bosn/cache/actcache /bosn/cache/{name}; dd if=/dev/zero of=/bosn/cache/{name}/payload bs=1024 count=128 2>/dev/null", name=class.name())), CONTROL_DEADLINE).await?;
    let mut session = Session::open_class(backend, id, class).await?;
    if matches!(class, DisposableClass::Tools) {
        return exercise_tools(backend, id, session).await;
    }
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
        &format!("test ! -e /bosn/cache/{name}; test ! -e /bosn/cache/.bosn-{name}-retirement-v1.json", name=class.name())), CONTROL_DEADLINE).await?;
    Ok(())
}

async fn exercise_tools(
    backend: &DockerActBackend,
    id: &str,
    mut session: Session,
) -> Result<(), String> {
    let archive = std::env::var("BOSN_TEST_ACT_ARCHIVE")
        .map_err(|_| "BOSN_TEST_ACT_ARCHIVE must name the verified release archive")?;
    let act = crate::ci::engine::act_artifact("amd64").unwrap();
    backend
        .checked(
            "pinned release archive fixture",
            owned(&[
                "cp",
                &archive,
                &format!("{id}:/bosn/cache/tools/current.tgz"),
            ]),
            CONTROL_DEADLINE,
        )
        .await?;
    session.preserve("current.tgz", act.sha256).await?;
    let retained = session.maintain(256 * 1024 * 1024).await?;
    assert!(retained.allocated_before > 65536);
    assert_eq!(retained.allocated_after, session.preserved_bytes);
    assert_eq!(retained.retired_classes, 1);
    let followup = session.maintain(256 * 1024 * 1024).await?;
    assert_eq!(followup.allocated_after, retained.allocated_after);
    assert_eq!(followup.retired_classes, 0);
    backend.checked("preserved archive digest", DockerActBackend::exec(id, &format!("echo '{}  /bosn/cache/.act-maintenance-archive-v1.tgz' | sha256sum -c -; test ! -e /bosn/cache/tools", act.sha256)), CONTROL_DEADLINE).await?;
    session.io.send(b"abort\n").await?;
    let script = crate::ci::engine::maintenance_helper::offline_install_script(act);
    let version = backend
        .checked(
            "offline restart after archive retirement",
            DockerActBackend::exec(id, &script),
            CONTROL_DEADLINE,
        )
        .await?;
    backend.verify_installed_act(id, &version).await?;
    Ok(())
}
