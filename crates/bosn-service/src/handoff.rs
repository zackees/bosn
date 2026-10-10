//! Idle handoff (#509 phase 4): a newer client may restart an older daemon onto
//! the authoritative executable, but only when the daemon itself finds it idle.
//!
//! The daemon decides, not the client: it alone sees its jobs, CI runs and
//! leases, so a busy daemon is never stopped on anyone's behalf. A daemon
//! that predates this operation answers "unknown operation", which a client
//! reads as "not idle-restartable" and leaves the daemon alone.

use super::*;

/// Operation 39: stop this daemon only if it is idle.
pub(crate) const OP_STOP_IF_IDLE: u32 = 39;
/// Reply code for a daemon that refused the handoff because it is busy.
pub(crate) const CODE_BUSY: u32 = 32;

/// What a daemon answered to an idle-handoff request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdleHandoff {
    /// It was idle and is stopping; a fresh daemon may be started once it
    /// no longer answers.
    Stopping,
    /// It is busy (the reason names what) and keeps running.
    Busy(String),
    /// It predates the handoff operation and keeps running.
    Unsupported,
}

/// What keeps this daemon busy, or `None` when it is idle: unfinished jobs,
/// queued or running CI runs, and held leases.
async fn busy_reason(
    actor: &RegistryActor,
    jobs: &JobActor,
    ci: &ci::CiRuntime,
) -> Result<Option<String>, Error> {
    let mut busy = Vec::new();
    let unfinished = jobs.unfinished().await?;
    if unfinished > 0 {
        busy.push(format!("{unfinished} unfinished job(s)"));
    }
    let runs = ci.active_runs();
    if runs > 0 {
        busy.push(format!("{runs} queued or running CI run(s)"));
    }
    let leases = actor.status().await?.leases;
    if leases > 0 {
        busy.push(format!("{leases} held lease(s)"));
    }
    Ok((!busy.is_empty()).then(|| busy.join(", ")))
}

/// Operation 39 on the daemon: stop exactly as operation 3 does when idle,
/// otherwise answer busy and keep running.
pub(crate) async fn stop_if_idle(
    actor: &RegistryActor,
    jobs: &JobActor,
    ci: &ci::CiRuntime,
    stop: &CancellationSource,
) -> ReplyWire {
    match busy_reason(actor, jobs, ci).await {
        Ok(None) => {
            let _ = async_engine::timeout(crate::dispatch::SPARE_CLOSE_DEADLINE, ci.close_spares())
                .await;
            stop.cancel();
            ReplyWire {
                code: 30,
                ..Default::default()
            }
        }
        Ok(Some(reason)) => ReplyWire {
            code: CODE_BUSY,
            job_error: reason,
            ..Default::default()
        },
        Err(_) => ReplyWire {
            code: 3,
            ..Default::default()
        },
    }
}

impl Client {
    /// Ask the daemon to stop if, and only if, it is idle (#509 phase 4).
    pub async fn stop_if_idle(&self) -> Result<IdleHandoff, Error> {
        let deadline = crate::dispatch::SPARE_CLOSE_DEADLINE + IO_DEADLINE;
        match self
            .call_within(Request::operation(OP_STOP_IF_IDLE), deadline)
            .await
        {
            Ok(Reply::Shutdown) => Ok(IdleHandoff::Stopping),
            Ok(Reply::Busy(reason)) => Ok(IdleHandoff::Busy(reason)),
            Ok(_) => Err(Error::Protocol("unexpected handoff response")),
            Err(Error::Protocol("unknown operation")) => Ok(IdleHandoff::Unsupported),
            Err(error) => Err(error),
        }
    }
}
