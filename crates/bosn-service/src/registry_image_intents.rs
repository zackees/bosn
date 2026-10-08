//! Actor-owned preparation intent commits and acknowledgement ordering.

use super::*;

pub(crate) async fn record(
    mut registry: Registry,
    intent: bosn_registry::ImageCreationIntent,
    complete: bool,
    reply: async_engine::OneshotSender<Result<(), Error>>,
) -> Option<Registry> {
    let worker = async_engine::launch_blocking(move || {
        let result = (|| {
            let mut transaction = registry.begin_immediate()?;
            if complete {
                transaction.delete_image_creation_intent(&intent)?;
            } else {
                transaction.put_image_creation_intent(&intent)?;
            }
            transaction.commit()
        })();
        let result = registry.publish_ownership_backup().and(result);
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
