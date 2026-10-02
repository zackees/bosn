use super::*;

#[derive(Default)]
struct FakeBackend {
    cancelled: Vec<u64>,
    setup_calls: Vec<(PathBuf, String, SetupAcquirePolicy)>,
    setup_prepare_calls: Vec<SetupPrepareRequest>,
    setup_ensure_calls: Vec<SetupEnsureJobRequest>,
    manifest_ensure_calls: Vec<ManifestEnsureJobRequest>,
    manifest_converge_calls: Vec<ManifestConvergeJobRequest>,
    manifest_app_task_calls: Vec<ManifestAppTaskJobRequest>,
    setup_task_calls: Vec<SetupTaskJobRequest>,
    setup_app_task_calls: Vec<SetupAppTaskJobRequest>,
    daemon_reads: u32,
    setup_prepare_error: bool,
    setup_ensure_error: bool,
    setup_task_error: bool,
}
impl Backend for FakeBackend {
    fn ci(&mut self) -> Box<dyn crate::ci::mcp::CiBackend + '_> {
        Box::new(crate::ci::mcp::Offline)
    }
    fn status(&mut self) -> Result<Status, Error> {
        self.daemon_reads += 1;
        Ok(Status {
            registry_id: "123e4567-e89b-42d3-a456-426614174000".into(),
            schema_version: 5,
            resources: 3,
            leases: 2,
            sessions: 1,
            reconciliation_required: false,
        })
    }
    fn doctor(&mut self) -> Result<DoctorReport, Error> {
        Ok(DoctorReport {
            daemon: "ready".into(),
            registry: "ready".into(),
            engine: "unavailable".into(),
            client_version: None,
            server_version: None,
        })
    }
    fn registry_resources(
        &mut self,
        after: u64,
        _limit: u32,
    ) -> Result<RegistryResourcePage, Error> {
        self.daemon_reads += 1;
        Ok(RegistryResourcePage {
            next: (after == 0).then_some(1),
            records: vec![crate::RegistryResourceDiagnostic {
                id: "managed-image".into(),
                kind: "image".into(),
                name: "bosn-setup-image".into(),
                stack: "setup".into(),
                generation: "sha256:abc".into(),
                state: "active".into(),
                retention: "pinned".into(),
                created_at: 1.0,
                last_used: 2.0,
            }],
        })
    }
    fn setup_ensure_events(
        &mut self,
        after: u64,
        _limit: u32,
    ) -> Result<SetupEnsureEventPage, Error> {
        self.daemon_reads += 1;
        Ok(SetupEnsureEventPage {
            next: (after == 0).then_some(1),
            records: vec![crate::SetupEnsureEventDiagnostic {
                cursor: 7,
                at: 2.0,
                kind: "setup.ensure.succeeded".into(),
                detail: "job_id=7 outcome=succeeded".into(),
            }],
        })
    }
    fn setup_gc_preview(
        &mut self,
        _workspace: PathBuf,
        after: u64,
        _limit: u32,
    ) -> Result<SetupGcPreviewPage, Error> {
        self.daemon_reads += 1;
        Ok(SetupGcPreviewPage {
            next: (after == 0).then_some(1),
            candidates: vec![crate::SetupGcCandidateDiagnostic {
                id: "setup-container:retired".into(),
                name: "bosn-setup-retired".into(),
                generation: "sha256:old".into(),
                token: "sgc1-7465737400".into(),
                reason: "retired_managed_setup_container".into(),
            }],
            counts: crate::SetupGcPreviewCounts::default(),
        })
    }
    fn manifest_volume_gc_preview(
        &mut self,
        _workspace: PathBuf,
        after: u64,
        _limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        self.daemon_reads += 1;
        Ok(ManifestVolumeGcPreviewPage {
            next: (after == 0).then_some(1),
            candidates: vec![crate::ManifestVolumeGcCandidateDiagnostic {
                id: "manifest-volume:old".into(),
                name: "bosn-v-spec-old".into(),
                generation: "sha256:old".into(),
                token: "mvg1-7465737400".into(),
                reason: "retired_manifest_warm_spec_volume".into(),
            }],
            counts: crate::ManifestVolumeGcPreviewCounts::default(),
        })
    }
    fn manifest_volume_release_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        let mut value = self.manifest_volume_gc_preview(workspace, after, limit)?;
        value.candidates[0].token = "mvr1-7465737400".into();
        value.candidates[0].reason = "explicit_durable_manifest_volume_release".into();
        Ok(value)
    }
    fn setup_reconcile_preview(
        &mut self,
        _workspace: PathBuf,
        after: u64,
        _limit: u32,
    ) -> Result<SetupReconcilePreviewPage, Error> {
        self.daemon_reads += 1;
        Ok(SetupReconcilePreviewPage {
            next: (after == 0).then_some(1),
            records: vec![crate::SetupReconcileRecord {
                id: "setup-container:abc".into(),
                name: "bosn-setup-abc".into(),
                generation: "sha256:abc".into(),
                drift: "matching_running".into(),
                repair_token: None,
            }],
        })
    }
    fn setup_reconcile_repair_missing(
        &mut self,
        _workspace: PathBuf,
        _token: String,
    ) -> Result<SetupReconcileMissingRepairResult, Error> {
        self.daemon_reads += 1;
        Ok(SetupReconcileMissingRepairResult {
            repaired: true,
            already_repaired: false,
        })
    }
    fn setup_gc_apply(
        &mut self,
        _workspace: PathBuf,
        _token: String,
    ) -> Result<SetupGcApplyResult, Error> {
        self.daemon_reads += 1;
        Ok(SetupGcApplyResult {
            removed: true,
            reconciled_missing: false,
        })
    }
    fn manifest_volume_gc_apply(
        &mut self,
        _workspace: PathBuf,
        _token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error> {
        self.daemon_reads += 1;
        Ok(ManifestVolumeGcApplyResult {
            removed: true,
            reconciled_missing: false,
        })
    }
    fn manifest_volume_release_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error> {
        self.manifest_volume_gc_apply(workspace, token)
    }
    fn setup_stop_retired(
        &mut self,
        _workspace: PathBuf,
        _token: String,
    ) -> Result<SetupRetiredStopResult, Error> {
        self.daemon_reads += 1;
        Ok(SetupRetiredStopResult {
            stopped: true,
            already_stopped: false,
        })
    }
    fn setup_done(&mut self, _workspace: PathBuf) -> Result<SetupDoneResult, Error> {
        self.daemon_reads += 1;
        Ok(SetupDoneResult {
            uses_completed: 2,
            resources_completed: 1,
        })
    }
    fn setup_adopt(&mut self, _request: SetupAdoptRequest) -> Result<SetupAdoptResult, Error> {
        self.daemon_reads += 1;
        Ok(SetupAdoptResult { adopted: true })
    }
    fn job_status(&mut self, id: u64) -> Result<JobStatus, Error> {
        self.daemon_reads += 1;
        Ok(JobStatus {
            id,
            state: "Running".into(),
            error: None,
        })
    }
    fn job_logs(&mut self, id: u64, after: u64, limit: u32) -> Result<JobLogPage, Error> {
        self.daemon_reads += 1;
        Ok(JobLogPage {
            retained_from: 0,
            next: after + 1,
            gap: false,
            records: vec![crate::JobLogRecord {
                cursor: after,
                line: format!("job={id} limit={limit}"),
            }],
        })
    }
    fn cancel_job(&mut self, id: u64) -> Result<(), Error> {
        self.cancelled.push(id);
        Ok(())
    }
    fn submit_setup_prepare(&mut self, request: SetupPrepareRequest) -> Result<u64, Error> {
        if self.setup_prepare_error {
            return Err(Error::Protocol(
                "credential=https://user:secret@example.invalid",
            ));
        }
        self.setup_prepare_calls.push(request);
        Ok(42)
    }
    fn submit_setup_ensure(&mut self, request: SetupEnsureJobRequest) -> Result<u64, Error> {
        if self.setup_ensure_error {
            return Err(Error::Protocol(
                "credential=https://user:secret@example.invalid",
            ));
        }
        self.setup_ensure_calls.push(request);
        Ok(44)
    }
    fn submit_manifest_ensure(&mut self, request: ManifestEnsureJobRequest) -> Result<u64, Error> {
        self.manifest_ensure_calls.push(request);
        Ok(45)
    }
    fn submit_manifest_converge(
        &mut self,
        request: ManifestConvergeJobRequest,
    ) -> Result<u64, Error> {
        self.manifest_converge_calls.push(request);
        Ok(47)
    }
    fn submit_manifest_app_task(
        &mut self,
        request: ManifestAppTaskJobRequest,
    ) -> Result<u64, Error> {
        self.manifest_app_task_calls.push(request);
        Ok(46)
    }
    fn submit_setup_task(&mut self, request: SetupTaskJobRequest) -> Result<u64, Error> {
        if self.setup_task_error {
            return Err(Error::Protocol(
                "credential=https://user:secret@example.invalid",
            ));
        }
        self.setup_task_calls.push(request);
        Ok(43)
    }
    fn submit_setup_app_task(&mut self, request: SetupAppTaskJobRequest) -> Result<u64, Error> {
        self.setup_app_task_calls.push(request);
        Ok(45)
    }
    fn setup_plan(
        &mut self,
        workspace: PathBuf,
        locator: String,
        policy: SetupAcquirePolicy,
    ) -> Result<SetupPlan, Error> {
        self.setup_calls.push((workspace.clone(), locator, policy));
        Ok(SetupPlan {
            source_kind: SetupSourceKind::LocalFile,
            content_sha256: "a".repeat(64),
            schema_version: 1,
            workspace_root: workspace,
            asset_root: None,
            task_names: vec!["check".into()],
            app: bosn_core::SetupApp {
                source: bosn_core::SetupSource::PinnedImage(
                    "registry.example/demo@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                ),
                environment: Default::default(),
                workdir: None,
                command: None,
                mounts: Vec::new(),
            },
            tasks: [(
                "check".into(),
                bosn_core::SetupTask {
                    command: "true".into(),
                    workdir: None,
                    environment: Default::default(),
                },
            )]
            .into_iter()
            .collect(),
            app_source: SetupPlanAppSource::PinnedImage {
                image: "registry.example/demo@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            },
            named_volumes: Vec::new(),
            tmpfs: Vec::new(),
            host_docker_socket: None,
            macos_guest: None,
        })
    }
}

fn exchange<B: Backend>(input: &str, backend: &mut B) -> Vec<Value> {
    let mut output = Vec::new();
    serve_transport(input.as_bytes(), &mut output, backend).unwrap();
    String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn stdio_initialize_lists_tools_and_calls_status() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_status","arguments":{}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 3);
    assert_eq!(
        replies[0]["result"]["protocolVersion"],
        MCP_PROTOCOL_VERSION
    );
    let names: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "bosn_status",
            "bosn_doctor",
            "bosn_registry_resources",
            "bosn_setup_ensure_events",
            "bosn_setup_gc_preview",
            "bosn_manifest_volume_gc_preview",
            "bosn_manifest_volume_gc_apply",
            "bosn_manifest_volume_release_preview",
            "bosn_manifest_volume_release_apply",
            "bosn_setup_reconcile_preview",
            "bosn_setup_reconcile_repair_missing",
            "bosn_setup_gc_apply",
            "bosn_setup_stop_retired",
            "bosn_setup_done",
            "bosn_setup_adopt",
            "bosn_job_status",
            "bosn_job_logs",
            "bosn_job_cancel",
            "bosn_setup_plan",
            "bosn_compose_plan",
            "bosn_setup_prepare",
            "bosn_setup_ensure",
            "bosn_manifest_ensure",
            "bosn_manifest_converge",
            "bosn_manifest_app_task",
            "bosn_setup_task",
            "bosn_setup_app_task",
            "bosn_ci_plan",
            "bosn_ci_run",
            "bosn_ci_status",
            "bosn_ci_list",
            "bosn_ci_logs",
            "bosn_ci_wait",
            "bosn_ci_cancel",
            "bosn_ci_report",
            "bosn_ci_runners",
        ]
    );
    let resources = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bosn_registry_resources")
        .unwrap();
    assert_eq!(resources["annotations"]["readOnlyHint"], true);
    let doctor = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bosn_doctor")
        .unwrap();
    assert_eq!(doctor["annotations"]["readOnlyHint"], true);
    assert_eq!(doctor["inputSchema"]["additionalProperties"], false);
    assert_eq!(resources["inputSchema"]["additionalProperties"], false);
    let manifest = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bosn_manifest_ensure")
        .unwrap();
    assert_eq!(manifest["annotations"]["readOnlyHint"], false);
    assert_eq!(manifest["inputSchema"]["additionalProperties"], false);
    let manifest_task = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bosn_manifest_app_task")
        .unwrap();
    assert_eq!(manifest_task["annotations"]["readOnlyHint"], false);
    assert_eq!(manifest_task["inputSchema"]["additionalProperties"], false);
    let preview = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bosn_setup_gc_preview")
        .unwrap();
    assert_eq!(preview["annotations"]["readOnlyHint"], true);
    assert_eq!(preview["annotations"]["destructiveHint"], false);
    assert_eq!(preview["inputSchema"]["additionalProperties"], false);
    let prepare = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bosn_setup_prepare")
        .unwrap();
    assert_eq!(prepare["annotations"]["readOnlyHint"], false);
    assert_eq!(prepare["annotations"]["destructiveHint"], false);
    assert_eq!(prepare["annotations"]["idempotentHint"], false);
    assert_eq!(prepare["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        prepare["inputSchema"]["properties"]["deadline_ms"]["maximum"],
        300000
    );
    assert_eq!(
        prepare["inputSchema"]["properties"]["output_limit"]["maximum"],
        8388608
    );
    let ensure = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bosn_setup_ensure")
        .unwrap();
    assert_eq!(ensure["annotations"]["readOnlyHint"], false);
    assert_eq!(ensure["annotations"]["destructiveHint"], false);
    assert_eq!(ensure["annotations"]["idempotentHint"], false);
    assert_eq!(ensure["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        ensure["inputSchema"]["required"],
        json!([
            "workspace",
            "config",
            "policy",
            "deadline_ms",
            "output_limit"
        ])
    );
    assert_eq!(
        ensure["inputSchema"]["properties"]["deadline_ms"]["maximum"],
        300000
    );
    assert_eq!(
        ensure["inputSchema"]["properties"]["output_limit"]["maximum"],
        8388608
    );
    let task = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bosn_setup_task")
        .unwrap();
    assert_eq!(task["annotations"]["readOnlyHint"], false);
    assert_eq!(task["annotations"]["destructiveHint"], false);
    assert_eq!(task["annotations"]["idempotentHint"], false);
    assert_eq!(task["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        task["inputSchema"]["required"],
        json!([
            "workspace",
            "config",
            "policy",
            "task_name",
            "deadline_ms",
            "output_limit",
        ])
    );
    assert_eq!(
        task["inputSchema"]["properties"]["task_name"]["maxLength"],
        64
    );
    assert_eq!(
        task["inputSchema"]["properties"]["deadline_ms"]["maximum"],
        300000
    );
    assert_eq!(
        task["inputSchema"]["properties"]["output_limit"]["maximum"],
        8388608
    );
    assert_eq!(replies[2]["result"]["isError"], false);
    assert_eq!(replies[2]["result"]["structuredContent"]["resources"], 3);
}

#[test]
fn compose_plan_is_discoverable_pure_and_refuses_file_or_engine_inputs() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_compose_plan","arguments":{"document":"services:\n  api:\n    image: alpine:3.21\n"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_compose_plan","arguments":{"document":"services: {}","file":"/private/compose.yaml"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    let tool = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "bosn_compose_plan")
        .unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    assert_eq!(tool["inputSchema"]["required"], json!(["document"]));
    assert_eq!(
        tool["inputSchema"]["properties"]["document"]["maxLength"],
        32768
    );
    assert_eq!(replies[2]["result"]["isError"], false);
    let plan = &replies[2]["result"]["structuredContent"];
    assert_eq!(plan["action"], "compose_plan");
    assert_eq!(plan["applied"], false);
    assert_eq!(plan["document"]["services"]["api"]["image"], "alpine:3.21");
    assert_eq!(replies[3]["result"]["isError"], true);
    assert_eq!(backend.daemon_reads, 0);
    assert!(backend.setup_calls.is_empty());
    assert!(backend.cancelled.is_empty());
}

#[test]
fn doctor_tool_is_no_argument_and_returns_only_stable_fields() {
    let mut backend = FakeBackend::default();
    let success = call_tool(
        json!({"name": "bosn_doctor", "arguments": {}}),
        &mut backend,
    );
    assert_eq!(success["isError"], false);
    let report = &success["structuredContent"];
    assert_eq!(report["daemon"], "ready");
    assert_eq!(report["registry"], "ready");
    assert_eq!(report["engine"], "unavailable");
    assert!(report.get("docker_args").is_none());

    let rejected = call_tool(
        json!({"name": "bosn_doctor", "arguments": {"deadline_ms": 1}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
}

#[test]
fn reconcile_preview_is_read_only_bounded_and_rejects_engine_controls() {
    let mut backend = FakeBackend::default();
    let value = call_tool(
        json!({"name":"bosn_setup_reconcile_preview","arguments":{"workspace":"/private/work","limit":1}}),
        &mut backend,
    );
    assert_eq!(value["structuredContent"]["preview_only"], true);
    assert_eq!(
        value["structuredContent"]["records"][0]["drift"],
        "matching_running"
    );
    let rejected = call_tool(
        json!({"name":"bosn_setup_reconcile_preview","arguments":{"workspace":"/private/work","docker_args":["rm"]}}),
        &mut backend,
    );
    assert!(rejected["isError"].as_bool().unwrap());
    assert_eq!(backend.daemon_reads, 1);
}

#[test]
fn reconcile_missing_repair_requires_confirmation_and_rejects_engine_controls() {
    let mut backend = FakeBackend::default();
    let rejected = call_tool(
        json!({"name":"bosn_setup_reconcile_repair_missing","arguments":{"workspace":"/private/work","candidate_token":"srm1-00","confirm":false}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
    assert_eq!(backend.daemon_reads, 0);
    let rejected = call_tool(
        json!({"name":"bosn_setup_reconcile_repair_missing","arguments":{"workspace":"/private/work","candidate_token":"srm1-00","confirm":true,"docker_args":["rm"]}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
    assert_eq!(backend.daemon_reads, 0);
    let repaired = call_tool(
        json!({"name":"bosn_setup_reconcile_repair_missing","arguments":{"workspace":"/private/work","candidate_token":"srm1-00","confirm":true}}),
        &mut backend,
    );
    assert_eq!(repaired["isError"], false);
    assert_eq!(repaired["structuredContent"]["repaired"], true);
    assert_eq!(backend.daemon_reads, 1);
}

#[test]
fn gc_preview_is_read_only_bounded_and_never_echoes_workspace() {
    let mut backend = FakeBackend::default();
    let value = call_tool(
        json!({"name":"bosn_setup_gc_preview","arguments":{"workspace":"/private/work","limit":1}}),
        &mut backend,
    );
    assert_eq!(value["isError"], false);
    let content = value["structuredContent"].to_string();
    assert!(!content.contains("/private/work"));
    assert_eq!(
        value["structuredContent"]["candidates"][0]["reason"],
        "retired_managed_setup_container"
    );
    let before = backend.daemon_reads;
    let rejected = call_tool(
        json!({"name":"bosn_setup_gc_preview","arguments":{"workspace":"/private/work","force":true}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
    assert_eq!(backend.daemon_reads, before);
}

#[test]
fn durable_volume_release_requires_a_preview_token_and_confirmation() {
    let mut backend = FakeBackend::default();
    let preview = call_tool(
        json!({"name":"bosn_manifest_volume_release_preview","arguments":{"workspace":"/private/work","limit":1}}),
        &mut backend,
    );
    assert_eq!(preview["isError"], false);
    assert_eq!(
        preview["structuredContent"]["candidates"][0]["token"],
        "mvr1-7465737400"
    );
    let rejected = call_tool(
        json!({"name":"bosn_manifest_volume_release_apply","arguments":{"workspace":"/private/work","candidate_token":"mvr1-00","confirm":false}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
    let applied = call_tool(
        json!({"name":"bosn_manifest_volume_release_apply","arguments":{"workspace":"/private/work","candidate_token":"mvr1-00","confirm":true}}),
        &mut backend,
    );
    assert_eq!(applied["isError"], false);
    assert_eq!(applied["structuredContent"]["removed"], true);
}

#[test]
fn stop_retired_requires_token_confirmation_and_rejects_engine_controls() {
    let mut backend = FakeBackend::default();
    let rejected = call_tool(
        json!({"name":"bosn_setup_stop_retired","arguments":{"workspace":"/private/work","candidate_token":"sgc1-00","confirm":false}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
    assert_eq!(backend.daemon_reads, 0);
    let rejected = call_tool(
        json!({"name":"bosn_setup_stop_retired","arguments":{"workspace":"/private/work","candidate_token":"sgc1-00","confirm":true,"docker_args":["stop"]}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
    assert_eq!(backend.daemon_reads, 0);
    let stopped = call_tool(
        json!({"name":"bosn_setup_stop_retired","arguments":{"workspace":"/private/work","candidate_token":"sgc1-00","confirm":true}}),
        &mut backend,
    );
    assert_eq!(stopped["isError"], false);
    assert_eq!(stopped["structuredContent"]["stopped"], true);
}

#[test]
fn setup_done_requires_confirmation_and_has_no_engine_controls() {
    let mut backend = FakeBackend::default();
    let rejected = call_tool(
        json!({"name":"bosn_setup_done","arguments":{"workspace":"/private/work","confirm":false}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
    assert_eq!(backend.daemon_reads, 0);
    let rejected = call_tool(
        json!({"name":"bosn_setup_done","arguments":{"workspace":"/private/work","confirm":true,"docker_args":["rm"]}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
    assert_eq!(backend.daemon_reads, 0);
    let completed = call_tool(
        json!({"name":"bosn_setup_done","arguments":{"workspace":"/private/work","confirm":true}}),
        &mut backend,
    );
    assert_eq!(completed["isError"], false);
    assert_eq!(completed["structuredContent"]["uses_completed"], 2);
    assert_eq!(completed["structuredContent"]["resources_completed"], 1);
}

#[test]
fn gc_apply_requires_token_and_explicit_confirmation() {
    let mut backend = FakeBackend::default();
    let rejected = call_tool(
        json!({"name":"bosn_setup_gc_apply","arguments":{"workspace":"/private/work","candidate_token":"sgc1-00","confirm":false}}),
        &mut backend,
    );
    assert_eq!(rejected["isError"], true);
    assert_eq!(backend.daemon_reads, 0);
    let applied = call_tool(
        json!({"name":"bosn_setup_gc_apply","arguments":{"workspace":"/private/work","candidate_token":"sgc1-00","confirm":true}}),
        &mut backend,
    );
    assert_eq!(applied["isError"], false);
    assert_eq!(applied["structuredContent"]["removed"], true);
    assert_eq!(backend.daemon_reads, 1);
}

#[test]
fn registry_diagnostics_are_bounded_read_only_and_path_safe() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_registry_resources","arguments":{"after":0,"limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_setup_ensure_events","arguments":{"limit":65}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_registry_resources","arguments":{"workspace":"/attacker"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(
        replies[1]["result"]["structuredContent"]["records"][0]["id"],
        "managed-image"
    );
    assert!(
        replies[1]["result"]["structuredContent"]["records"][0]
            .get("workspace")
            .is_none()
    );
    assert_eq!(replies[2]["result"]["isError"], true);
    assert_eq!(replies[3]["result"]["isError"], true);
    // Only the well-formed resource read reached the backend.
    assert_eq!(backend.daemon_reads, 1);
}

#[test]
fn stdio_tool_call_validates_arguments_and_cancels_exact_job() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":"a","method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":"b","method":"tools/call","params":{"name":"bosn_job_cancel","arguments":{"job_id":9}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":"c","method":"tools/call","params":{"name":"bosn_job_logs","arguments":{"job_id":9,"limit":65}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(backend.cancelled, [9]);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(replies[2]["result"]["isError"], true);
}

#[test]
fn manifest_app_task_submits_only_declared_semantic_inputs() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_manifest_app_task","arguments":{"workspace":"/workspace","manifest":"bosn.toml","stack":"app","task_name":"check","deadline_ms":1000,"output_limit":1024}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_manifest_app_task","arguments":{"workspace":"/workspace","manifest":"bosn.toml","stack":"app","task_name":"check","deadline_ms":1000,"output_limit":1024,"command":"id"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(replies[1]["result"]["structuredContent"]["job_id"], 46);
    assert_eq!(replies[2]["result"]["isError"], true);
    assert_eq!(backend.manifest_app_task_calls.len(), 1);
    let call = &backend.manifest_app_task_calls[0];
    assert_eq!(call.manifest, "bosn.toml");
    assert_eq!(call.stack, "app");
    assert_eq!(call.task_name, "check");
}

#[test]
fn manifest_converge_submits_only_whole_manifest_selectors() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_manifest_converge","arguments":{"workspace":"/workspace","manifest":"bosn.toml","deadline_ms":1000,"output_limit":1024}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_manifest_converge","arguments":{"workspace":"/workspace","manifest":"bosn.toml","deadline_ms":1000,"output_limit":1024,"stack":"app"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(replies[1]["result"]["structuredContent"]["job_id"], 47);
    assert_eq!(replies[2]["result"]["isError"], true);
    assert_eq!(backend.manifest_converge_calls.len(), 1);
    let call = &backend.manifest_converge_calls[0];
    assert_eq!(call.manifest, "bosn.toml");
    assert_eq!(call.workspace, PathBuf::from("/workspace"));
}

#[test]
fn oversized_input_is_rejected_without_unbounded_buffering() {
    let mut backend = FakeBackend::default();
    let input = format!("{}\n", "x".repeat(MAX_REQUEST_BYTES + 1));
    let replies = exchange(&input, &mut backend);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["error"]["code"], -32600);
}

#[test]
fn notifications_never_mutate_daemon_jobs() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"bosn_job_cancel","arguments":{"job_id":9}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 1);
    assert!(backend.cancelled.is_empty());
}

#[test]
fn setup_plan_tool_is_read_only_and_uses_only_server_selected_state() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_plan","arguments":{"workspace":"/workspace","config":"/configs/setup.toml","policy":"refresh"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(replies[1]["result"]["structuredContent"]["applied"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"]["source_kind"],
        "local_file"
    );
    assert_eq!(backend.daemon_reads, 0);
    assert!(backend.cancelled.is_empty());
    assert_eq!(backend.setup_calls.len(), 1);
    assert_eq!(backend.setup_calls[0].0, PathBuf::from("/workspace"));
    assert_eq!(backend.setup_calls[0].1, "/configs/setup.toml");
    assert_eq!(backend.setup_calls[0].2, SetupAcquirePolicy::OnlineRefresh);
}

#[test]
fn setup_plan_rejects_malformed_or_ambiguous_arguments_before_execution() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_plan","arguments":{"workspace":"/workspace","config":"/configs/setup.toml"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_setup_plan","arguments":{"workspace":"/workspace","config":"/configs/setup.toml","policy":"refresh","state_dir":"/attacker-selected"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_setup_plan","arguments":{"workspace":"/workspace","config":"/configs/setup.toml","policy":"refresh,offline"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 4);
    assert!(replies[1]["result"]["isError"].as_bool().unwrap());
    assert!(replies[2]["result"]["isError"].as_bool().unwrap());
    assert!(replies[3]["result"]["isError"].as_bool().unwrap());
    assert!(backend.setup_calls.is_empty());
    assert_eq!(backend.daemon_reads, 0);
}

#[test]
fn setup_prepare_submits_all_immutable_semantic_inputs() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"https://configs.example/setup.toml","policy":"offline","deadline_ms":1234,"output_limit":7654321}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"],
        json!({
            "action": "setup_prepare", "submitted": true, "job_id": 42,
        })
    );
    assert_eq!(backend.setup_prepare_calls.len(), 1);
    let request = &backend.setup_prepare_calls[0];
    assert_eq!(request.workspace, PathBuf::from("/workspace"));
    assert_eq!(request.config, "https://configs.example/setup.toml");
    assert_eq!(request.policy, SetupPreparePolicy::Offline);
    assert_eq!(request.deadline, std::time::Duration::from_millis(1234));
    assert_eq!(request.output_limit, 7_654_321);
}

#[test]
fn setup_prepare_rejects_invalid_or_extra_arguments_without_submission() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":0,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":300001,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":8388609}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1,"docker_argv":["rm","-rf","/"]}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 6);
    assert!(
        replies[1..]
            .iter()
            .all(|reply| reply["result"]["isError"] == true)
    );
    assert!(backend.setup_prepare_calls.is_empty());
}

#[test]
fn setup_prepare_daemon_errors_are_redacted() {
    let mut backend = FakeBackend {
        setup_prepare_error: true,
        ..Default::default()
    };
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_prepare","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
        ),
        &mut backend,
    );
    let content = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(content, "native Bosn daemon request failed");
    assert!(!content.contains("secret"));
}

#[test]
fn setup_ensure_submits_all_immutable_semantic_inputs() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"https://configs.example/setup.toml","policy":"offline","deadline_ms":1234,"output_limit":7654321}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"],
        json!({
            "action": "setup_ensure", "submitted": true, "job_id": 44,
        })
    );
    assert_eq!(backend.setup_ensure_calls.len(), 1);
    let request = &backend.setup_ensure_calls[0];
    assert_eq!(request.workspace, PathBuf::from("/workspace"));
    assert_eq!(request.config, "https://configs.example/setup.toml");
    assert_eq!(request.policy, SetupPreparePolicy::Offline);
    assert_eq!(request.deadline, std::time::Duration::from_millis(1234));
    assert_eq!(request.output_limit, 7_654_321);
}

#[test]
fn setup_ensure_rejects_malformed_extra_or_out_of_range_arguments_without_submission() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"other","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":0,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":300001,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":8388609}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1,"container":"attacker-selected"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 8);
    assert!(
        replies[1..]
            .iter()
            .all(|reply| reply["result"]["isError"] == true)
    );
    assert!(backend.setup_ensure_calls.is_empty());
}

#[test]
fn setup_ensure_daemon_errors_are_redacted() {
    let mut backend = FakeBackend {
        setup_ensure_error: true,
        ..Default::default()
    };
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_ensure","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
        ),
        &mut backend,
    );
    let content = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(content, "native Bosn daemon request failed");
    assert!(!content.contains("secret"));
}

#[test]
fn setup_task_submits_every_immutable_input_and_declared_task_name() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"https://configs.example/setup.toml","policy":"offline","task_name":"check-build_2","deadline_ms":1234,"output_limit":7654321}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"],
        json!({
            "action": "setup_task", "submitted": true, "job_id": 43,
        })
    );
    assert_eq!(backend.setup_task_calls.len(), 1);
    let request = &backend.setup_task_calls[0];
    assert_eq!(request.workspace, PathBuf::from("/workspace"));
    assert_eq!(request.config, "https://configs.example/setup.toml");
    assert_eq!(request.policy, SetupPreparePolicy::Offline);
    assert_eq!(request.task_name, "check-build_2");
    assert_eq!(request.deadline, std::time::Duration::from_millis(1234));
    assert_eq!(request.output_limit, 7_654_321);
}

#[test]
fn setup_app_task_submits_only_declared_semantic_inputs() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_app_task","arguments":{"workspace":"/workspace","config":"https://configs.example/setup.toml","policy":"offline","task_name":"check-build_2","deadline_ms":1234,"output_limit":7654321}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(
        replies[1]["result"]["structuredContent"],
        json!({"action": "setup_app_task", "submitted": true, "job_id": 45})
    );
    assert_eq!(backend.setup_app_task_calls.len(), 1);
    let request = &backend.setup_app_task_calls[0];
    assert_eq!(request.workspace, PathBuf::from("/workspace"));
    assert_eq!(request.config, "https://configs.example/setup.toml");
    assert_eq!(request.policy, SetupPreparePolicy::Offline);
    assert_eq!(request.task_name, "check-build_2");
    assert_eq!(request.deadline, std::time::Duration::from_millis(1234));
    assert_eq!(request.output_limit, 7_654_321);
}

#[test]
fn setup_task_rejects_malformed_extra_or_out_of_range_arguments_without_submission() {
    let mut backend = FakeBackend::default();
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"-check","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check/run","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":0,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":300001,"output_limit":1}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":1,"output_limit":8388609}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":1,"output_limit":1,"command":"rm -rf /"}}}"#,
            "\n",
        ),
        &mut backend,
    );
    assert_eq!(replies.len(), 8);
    assert!(
        replies[1..]
            .iter()
            .all(|reply| reply["result"]["isError"] == true)
    );
    assert!(backend.setup_task_calls.is_empty());
}

#[test]
fn setup_task_daemon_errors_are_redacted() {
    let mut backend = FakeBackend {
        setup_task_error: true,
        ..Default::default()
    };
    let replies = exchange(
        concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"bosn_setup_task","arguments":{"workspace":"/workspace","config":"/setup.toml","policy":"refresh","task_name":"check","deadline_ms":1,"output_limit":1}}}"#,
            "\n",
        ),
        &mut backend,
    );
    let content = replies[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(content, "native Bosn daemon request failed");
    assert!(!content.contains("secret"));
}

fn pinned_document() -> String {
    format!(
        "version = 1\n[app]\nimage = 'registry.example/demo@sha256:{}'\n[task.check]\ncommand = 'echo check'\n",
        "a".repeat(64)
    )
}

fn inline_document() -> &'static str {
    "version = 1\n[app]\ndockerfile = 'FROM scratch'\n[task.check]\ncommand = 'echo check'\n[[file]]\npath = 'check.sh'\ncontent = \"#!/bin/sh\\necho check\\n\"\n"
}

fn live_setup_plan(
    state_dir: &std::path::Path,
    workspace: &std::path::Path,
    config: &std::path::Path,
    policy: &str,
) -> Value {
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let client = Client::for_state(state_dir).unwrap();
    let mut backend = DaemonBackend {
        runtime: &runtime,
        client,
        state_dir: state_dir.to_path_buf(),
    };
    let input = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "bosn_setup_plan", "arguments": {
            "workspace": workspace,
            "config": config,
            "policy": policy,
        }},
    }))
    .unwrap();
    let replies = exchange(
        &format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}}\n{input}\n"),
        &mut backend,
    );
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[1]["result"]["isError"], false);
    replies[1]["result"]["structuredContent"].clone()
}

#[test]
fn setup_plan_mcp_local_pinned_and_offline_reuse_never_start_daemon() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let config = config_root.path().join("setup.toml");
    std::fs::write(&config, pinned_document()).unwrap();

    let online = live_setup_plan(state.path(), workspace.path(), &config, "refresh");
    assert_eq!(online["action"], "plan");
    assert_eq!(online["applied"], false);
    assert_eq!(online["source_kind"], "local_file");
    assert_eq!(online["asset_root"], Value::Null);
    assert_eq!(online["task_names"], json!(["check"]));
    assert_eq!(online["app_source"]["kind"], "pinned_image");
    assert!(!state.path().join("registry.sqlite3").exists());

    std::fs::remove_file(&config).unwrap();
    let offline = live_setup_plan(state.path(), workspace.path(), &config, "offline");
    assert_eq!(offline, online);
    assert!(!state.path().join("registry.sqlite3").exists());
}

#[test]
fn setup_plan_mcp_inline_materializes_only_under_server_state() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let config = config_root.path().join("setup.toml");
    std::fs::write(&config, inline_document()).unwrap();

    let plan = live_setup_plan(state.path(), workspace.path(), &config, "refresh");
    assert_eq!(plan["applied"], false);
    assert_eq!(plan["app_source"]["kind"], "inline_dockerfile");
    let asset_root = std::path::Path::new(plan["asset_root"].as_str().unwrap());
    assert!(asset_root.starts_with(state.path()));
    assert!(!asset_root.starts_with(workspace.path()));
    assert_eq!(
        std::fs::read_to_string(asset_root.join("Dockerfile")).unwrap(),
        "FROM scratch"
    );
    assert!(
        std::fs::read_dir(workspace.path())
            .unwrap()
            .next()
            .is_none()
    );
    assert!(!state.path().join("registry.sqlite3").exists());
}
