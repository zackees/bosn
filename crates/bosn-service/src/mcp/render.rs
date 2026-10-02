//! Rendering typed daemon replies as MCP tool results.

use super::*;

pub(crate) fn status_json(status: Status) -> Value {
    json!({
        "registry_id": status.registry_id,
        "schema_version": status.schema_version,
        "resources": status.resources,
        "leases": status.leases,
        "sessions": status.sessions,
        "reconciliation_required": status.reconciliation_required,
    })
}

pub(crate) fn doctor_json(report: DoctorReport) -> Value {
    json!({
        "daemon": report.daemon,
        "registry": report.registry,
        "engine": report.engine,
        "client_version": report.client_version,
        "server_version": report.server_version,
    })
}

pub(crate) fn resource_page_json(page: RegistryResourcePage) -> Value {
    let records: Vec<Value> = page
        .records
        .into_iter()
        .map(|record| {
            json!({
                "id": record.id, "kind": record.kind, "name": record.name,
                "stack": record.stack, "generation": record.generation,
                "state": record.state, "retention": record.retention,
                "created_at": record.created_at, "last_used": record.last_used,
            })
        })
        .collect();
    json!({"next": page.next, "records": records})
}

pub(crate) fn setup_ensure_event_page_json(page: SetupEnsureEventPage) -> Value {
    let records: Vec<Value> = page.records.into_iter().map(|record| json!({
        "cursor": record.cursor, "at": record.at, "kind": record.kind, "detail": record.detail,
    })).collect();
    json!({"next": page.next, "records": records})
}
pub(crate) fn setup_gc_preview_json(page: SetupGcPreviewPage) -> Value {
    let candidates: Vec<_> = page.candidates.into_iter().map(|candidate| json!({"id": candidate.id, "name": candidate.name, "generation": candidate.generation, "token":candidate.token, "reason": candidate.reason})).collect();
    json!({"next": page.next, "candidates": candidates, "counts": {"protected_not_retired": page.counts.protected_not_retired, "protected_ambiguous_use": page.counts.protected_ambiguous_use, "protected_lease": page.counts.protected_lease, "protected_session": page.counts.protected_session, "excluded_unmanaged": page.counts.excluded_unmanaged}})
}
pub(crate) fn setup_reconcile_preview_json(page: SetupReconcilePreviewPage) -> Value {
    json!({"preview_only":true,"next":page.next,"records":page.records.into_iter().map(|record| json!({"id":record.id,"name":record.name,"generation":record.generation,"drift":record.drift,"repair_token":record.repair_token})).collect::<Vec<_>>()})
}
pub(crate) fn setup_reconcile_repair_missing_json(
    result: SetupReconcileMissingRepairResult,
) -> Value {
    json!({"repaired":result.repaired,"already_repaired":result.already_repaired})
}
pub(crate) fn setup_gc_apply_json(result: SetupGcApplyResult) -> Value {
    json!({"removed":result.removed,"reconciled_missing":result.reconciled_missing})
}
pub(crate) fn manifest_volume_gc_preview_json(page: ManifestVolumeGcPreviewPage) -> Value {
    json!({"preview_only":true,"next":page.next,"candidates":page.candidates.into_iter().map(|v| json!({"id":v.id,"name":v.name,"generation":v.generation,"token":v.token,"reason":v.reason})).collect::<Vec<_>>(),"counts":{"protected_not_retired":page.counts.protected_not_retired,"protected_policy":page.counts.protected_policy,"protected_ambiguous_use":page.counts.protected_ambiguous_use,"protected_lease":page.counts.protected_lease,"protected_session":page.counts.protected_session,"protected_intent":page.counts.protected_intent,"excluded_unmanaged":page.counts.excluded_unmanaged}})
}
pub(crate) fn manifest_volume_gc_apply_json(result: ManifestVolumeGcApplyResult) -> Value {
    json!({"removed":result.removed,"reconciled_missing":result.reconciled_missing})
}
pub(crate) fn setup_stop_retired_json(result: SetupRetiredStopResult) -> Value {
    json!({"stopped": result.stopped, "already_stopped": result.already_stopped})
}
pub(crate) fn setup_done_json(result: SetupDoneResult) -> Value {
    json!({"uses_completed":result.uses_completed,"resources_completed":result.resources_completed})
}

pub(crate) fn job_json(job: JobStatus) -> Value {
    json!({"job_id": job.id, "state": job.state, "error": job.error})
}

pub(crate) fn log_page_json(page: JobLogPage) -> Value {
    let records: Vec<Value> = page
        .records
        .into_iter()
        .map(|record| json!({"cursor": record.cursor, "line": record.line}))
        .collect();
    json!({
        "retained_from": page.retained_from,
        "next": page.next,
        "gap": page.gap,
        "records": records,
    })
}

pub(crate) fn setup_plan_json(plan: SetupPlan) -> Value {
    let app_source = match plan.app_source {
        SetupPlanAppSource::PinnedImage { image } => {
            json!({"kind": "pinned_image", "image": image})
        }
        SetupPlanAppSource::InlineDockerfile { dockerfile_path } => {
            json!({"kind": "inline_dockerfile", "dockerfile_path": dockerfile_path})
        }
    };
    json!({
        "action": "plan",
        "applied": false,
        "source_kind": match plan.source_kind {
            SetupSourceKind::LocalFile => "local_file",
            SetupSourceKind::Https => "https",
        },
        "content_sha256": plan.content_sha256,
        "schema_version": plan.schema_version,
        "workspace": plan.workspace_root,
        "asset_root": plan.asset_root,
        "task_names": plan.task_names,
        "app_source": app_source,
    })
}

pub(crate) fn compose_plan_json(plan: bosn_core::ComposePlan) -> Value {
    json!({
        "action": "compose_plan",
        "applied": false,
        "version": plan.version,
        "digest": plan.digest,
        "document": plan.document,
        "normalized_json": plan.normalized_json,
    })
}
