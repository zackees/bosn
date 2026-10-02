//! Decoding SQLite rows into registry types.

use super::*;

pub(crate) fn text(r: &Row, i: usize) -> Result<String, Error> {
    match r.get(i) {
        Some(Value::Text(v)) => Ok(v.clone()),
        _ => Err(Error::BadRow("text")),
    }
}
pub(crate) fn real(r: &Row, i: usize) -> Result<f64, Error> {
    let value = match r.get(i) {
        Some(Value::Real(v)) => Ok(*v),
        Some(Value::Integer(v)) => Ok(*v as f64),
        _ => Err(Error::BadRow("real")),
    }?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(Error::BadRow("finite real"))
    }
}
pub(crate) fn integer(r: &Row, i: usize) -> Result<i64, Error> {
    match r.get(i) {
        Some(Value::Integer(v)) => Ok(*v),
        _ => Err(Error::BadRow("integer")),
    }
}
pub(crate) fn optional_real(r: &Row, i: usize) -> Result<Option<f64>, Error> {
    let value = match r.get(i) {
        Some(Value::Null) => Ok(None),
        Some(Value::Real(v)) => Ok(Some(*v)),
        Some(Value::Integer(v)) => Ok(Some(*v as f64)),
        _ => Err(Error::BadRow("optional real")),
    }?;
    if value.is_none_or(f64::is_finite) {
        Ok(value)
    } else {
        Err(Error::BadRow("finite optional real"))
    }
}
pub(crate) fn kind(v: String) -> Result<ResourceKind, Error> {
    match v.as_str() {
        "container" => Ok(ResourceKind::Container),
        "volume" => Ok(ResourceKind::Volume),
        "image" => Ok(ResourceKind::Image),
        "builder" => Ok(ResourceKind::Builder),
        "network" => Ok(ResourceKind::Network),
        _ => Err(Error::BadRow("kind")),
    }
}
pub(crate) fn scope(v: String) -> Result<Scope, Error> {
    match v.as_str() {
        "spec" => Ok(Scope::Spec),
        "stack" => Ok(Scope::Stack),
        "machine" => Ok(Scope::Machine),
        _ => Err(Error::BadRow("scope")),
    }
}
pub(crate) fn state(v: String) -> Result<ResourceState, Error> {
    match v.as_str() {
        "active" => Ok(ResourceState::Active),
        "adopted" => Ok(ResourceState::Adopted),
        "done" => Ok(ResourceState::Done),
        "retired" => Ok(ResourceState::Retired),
        _ => Err(Error::BadRow("state")),
    }
}
pub(crate) fn retention(v: String) -> Result<Retention, Error> {
    match v.as_str() {
        "warm" => Ok(Retention::Warm),
        "pinned" => Ok(Retention::Pinned),
        _ => Err(Error::BadRow("retention")),
    }
}
pub(crate) fn resource(r: &Row) -> Result<Resource, Error> {
    Ok(Resource {
        id: text(r, 0)?,
        kind: kind(text(r, 1)?)?,
        name: text(r, 2)?,
        stack: text(r, 3)?,
        generation: text(r, 4)?,
        scope: scope(text(r, 5)?)?,
        workspace: text(r, 6)?,
        created_at: real(r, 7)?,
        last_used: real(r, 8)?,
        state: state(text(r, 9)?)?,
        retention: retention(text(r, 10)?)?,
    })
}
pub(crate) fn resource_use(r: &Row) -> Result<ResourceUse, Error> {
    Ok(ResourceUse {
        resource_id: text(r, 0)?,
        workspace: text(r, 1)?,
        stack: text(r, 2)?,
        generation: text(r, 3)?,
        last_used: real(r, 4)?,
        state: state(text(r, 5)?)?,
    })
}
pub(crate) fn lease(r: &Row) -> Result<Lease, Error> {
    Ok(Lease {
        id: text(r, 0)?,
        resource_id: text(r, 1)?,
        pid: u32::try_from(integer(r, 2)?).map_err(|_| Error::BadRow("pid"))?,
        proc_start: optional_real(r, 3)?,
        acquired_at: real(r, 4)?,
        heartbeat_at: real(r, 5)?,
        ttl_seconds: real(r, 6)?,
    })
}
pub(crate) fn session(r: &Row) -> Result<ExecutionSession, Error> {
    Ok(ExecutionSession {
        id: text(r, 0)?,
        container_id: text(r, 1)?,
        engine_binary: text(r, 2)?,
        client_pid: u32::try_from(integer(r, 3)?).map_err(|_| Error::BadRow("client pid"))?,
        client_start: optional_real(r, 4)?,
        lease_ids: serde_json::from_str(&text(r, 5)?).map_err(|_| Error::BadRow("lease ids"))?,
    })
}
pub(crate) fn intent(r: &Row) -> Result<VolumeCreationIntent, Error> {
    Ok(VolumeCreationIntent {
        name: text(r, 0)?,
        labels: serde_json::from_str(&text(r, 1)?).map_err(|_| Error::BadRow("labels"))?,
        stack: text(r, 2)?,
        generation: text(r, 3)?,
        scope: scope(text(r, 4)?)?,
        workspace: text(r, 5)?,
    })
}
pub(crate) fn generation(r: &Row) -> Result<Generation, Error> {
    Ok(Generation {
        workspace: text(r, 0)?,
        stack: text(r, 1)?,
        digest: text(r, 2)?,
        created_at: real(r, 3)?,
        superseded_at: optional_real(r, 4)?,
    })
}
pub(crate) fn event(r: &Row) -> Result<Event, Error> {
    Ok(Event {
        id: integer(r, 0)?,
        at: real(r, 1)?,
        kind: text(r, 2)?,
        detail: text(r, 3)?,
    })
}
