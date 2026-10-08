//! Registry mutation workers holding retention admission through their commits.
use super::*;

pub(crate) type RecoveryResult = Result<
    (
        managed_retention::gate::Guard,
        managed_retention::image_recovery::Report,
    ),
    String,
>;

pub(crate) async fn recover(
    mut registry: Registry,
    admission: managed_retention::gate::Guard,
    deadline: std::time::Instant,
    reply: async_engine::OneshotSender<RecoveryResult>,
) -> Option<Registry> {
    let worker = async_engine::launch_blocking(move || {
        let result = managed_retention::image_recovery::reconcile_until(
            &DockerEngine::docker(),
            &mut registry,
            deadline,
        );
        (registry, result.map(|report| (admission, report)))
    });
    match worker.await {
        Ok((registry, result)) => {
            let _ = reply.send(result);
            Some(registry)
        }
        Err(_) => {
            let _ = reply.send(Err("image recovery worker stopped".into()));
            None
        }
    }
}

pub(crate) async fn prune(
    mut registry: Registry,
    receipts: Vec<managed_retention::DeletionReceipt>,
    admission: managed_retention::gate::Guard,
    reply: async_engine::OneshotSender<Result<(), Error>>,
) -> Option<Registry> {
    let worker = async_engine::launch_blocking(move || {
        let _admission = admission;
        let result = (|| {
            let mut transaction = registry.begin_immediate()?;
            for receipt in &receipts {
                transaction.delete_removed_ownership(
                    &receipt.labels,
                    &receipt.physical_name,
                    &receipt.physical_id,
                    receipt.observed_at,
                )?;
            }
            transaction.commit()
        })();
        let result = registry.publish_ownership_backup().and(result);
        let result = result.and_then(|()| {
            for receipt in &receipts {
                managed_retention::deletion_intents::acknowledge(receipt)
                    .map_err(|error| bosn_registry::Error::Io(std::io::Error::other(error)))?;
            }
            Ok(())
        });
        (registry, result)
    });
    match worker.await {
        Ok((registry, result)) => {
            let _ = reply.send(result.map_err(Error::Registry));
            Some(registry)
        }
        Err(_) => {
            let _ = reply.send(Err(Error::ActorClosed));
            None
        }
    }
}
