//! MCP tool descriptors and their input schemas.

use super::*;

pub(crate) fn tools_list() -> Value {
    let mut list = json!({
        "tools": [
            {
                "name": "bosn_status",
                "description": "Read the native Bosn daemon registry status. Does not start a daemon or mutate state.",
                "inputSchema": {"type": "object", "additionalProperties": false},
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_doctor",
                "description": "Run fixed daemon-owned, read-only registry integrity and Docker version checks. It accepts no arguments, never starts a daemon, initializes or migrates a registry, mutates Docker, or returns raw engine output.",
                "inputSchema": {"type": "object", "additionalProperties": false},
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_registry_resources",
                "description": "Read a bounded cursor page of path-safe managed-resource diagnostics from the already-running native Bosn daemon. Does not start a daemon, initialize a registry, or mutate state.",
                "inputSchema": registry_page_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_ensure_events",
                "description": "Read a bounded newest-first cursor page of credential-safe setup ensure and native-manifest lifecycle history from the already-running native Bosn daemon. Does not start a daemon, initialize a registry, or mutate state.",
                "inputSchema": registry_page_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_gc_preview",
                "description": "Preview only future collection candidates for retired Bosn-managed setup containers in one workspace. This never starts a daemon, writes SQLite, calls Docker, stops, deletes, or applies GC. A future apply must recheck all ownership facts.",
                "inputSchema": setup_gc_preview_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {"name":"bosn_manifest_volume_gc_preview","description":"Preview only retired disposable native-manifest volumes in one workspace. Only warm spec-scoped volume generations can appear; machine, stack, and pinned data are protected. Never writes SQLite or calls Docker.","inputSchema":setup_gc_preview_schema(),"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {"name":"bosn_manifest_volume_gc_apply","description":"DESTRUCTIVE: remove exactly one preview-token-bound retired warm spec native-manifest volume after the daemon rechecks registry ownership, exact Docker labels, and empty Docker attachment state. Explicit confirmation required.","inputSchema":setup_gc_apply_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false}},
            {"name":"bosn_manifest_volume_release_preview","description":"Preview durable native-manifest volumes that normal GC protects: stack/machine scope or pinned retention. It never writes state or calls Docker; apply requires one returned opaque token.","inputSchema":setup_gc_preview_schema(),"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {"name":"bosn_manifest_volume_release_apply","description":"DESTRUCTIVE: explicitly release exactly one preview-token-bound durable manifest volume. The daemon rechecks registry uses, leases, sessions, intents, exact Docker labels, and attachments immediately before fixed-name removal. Explicit confirmation required.","inputSchema":setup_gc_apply_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false}},
            {"name":"bosn_setup_reconcile_preview","description":"Read-only compare of durable Bosn setup-container ownership with fixed Docker inspection for one workspace. It never repairs, writes SQLite, creates/starts/stops/removes Docker resources, or accepts engine controls.","inputSchema":setup_gc_preview_schema(),"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {"name":"bosn_setup_reconcile_repair_missing","description":"STATE CHANGE: retire exactly one preview-token-bound active managed setup app only after the daemon rechecks ownership/use protection and fixed Docker inspection still proves it missing. It never starts, creates, stops, removes, or otherwise mutates Docker.","inputSchema":setup_gc_apply_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {
                "name": "bosn_setup_gc_apply",
                "description": "DESTRUCTIVE: remove exactly one retired Bosn-managed setup container using a preview candidate token and explicit confirmation. The daemon rechecks registry ownership and Docker labels before removal.",
                "inputSchema": setup_gc_apply_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_stop_retired",
                "description": "DESTRUCTIVE: stop exactly one running retired Bosn-managed setup container using a preview candidate token and explicit confirmation. It retains the registry record for later GC apply and never removes images, volumes, or containers.",
                "inputSchema": setup_gc_apply_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_done",
                "description": "STATE CHANGE: mark this workspace's active setup registry ownership done. It never calls Docker, stops/removes resources, or accepts engine controls; confirmation is required.",
                "inputSchema": setup_done_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {"name":"bosn_setup_adopt","description":"STATE CHANGE: restore registry ownership only after the daemon proves an existing app has exact Bosn labels, deterministic name, and prepared image identity. Confirmation required; no Docker controls.","inputSchema":setup_adopt_schema(),"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
            {
                "name": "bosn_job_status",
                "description": "Read the state of one native daemon job.",
                "inputSchema": job_id_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_job_logs",
                "description": "Read a bounded, cursor-paginated page of native daemon job logs.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["job_id"],
                    "properties": {
                        "job_id": {"type": "integer", "minimum": 1},
                        "after": {"type": "integer", "minimum": 0, "default": 0},
                        "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MCP_LOG_RECORDS, "default": MAX_MCP_LOG_RECORDS}
                    }
                },
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_job_cancel",
                "description": "Request cancellation of one non-terminal native daemon job. This changes daemon job state but does not delete resources.",
                "inputSchema": job_id_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_plan",
                "description": "Validate, cache, and materialize one Bosn setup document into the MCP server's preselected private state directory. A `.yaml`/`.yml` locator is accepted only for the supported lossless single-service Compose-to-setup subset; all other locators remain TOML-only. Returns an inert receipt only: applied is always false; it does not start a daemon or invoke Docker.",
                "inputSchema": setup_plan_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_compose_plan",
                "description": "Parse, validate, and digest caller-supplied Compose YAML using Bosn's documented subset. This is pure review data: it reads no file, contacts no daemon or Docker engine, writes no state, and never executes Compose.",
                "inputSchema": compose_plan_schema(),
                "annotations": {"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_prepare",
                "description": "Submit one bounded daemon-owned setup image-preparation job. Returns promptly with a durable job ID; poll the existing job tools for outcome and logs. It does not start a daemon, run setup tasks, or accept Docker, mount, or output-path controls.",
                "inputSchema": setup_prepare_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_ensure",
                "description": "Submit one bounded daemon-owned setup application ensure job. Returns promptly with a durable job ID; poll the existing job tools for outcome and logs. The daemon may create an absent app or start a matching stopped app, but refuses foreign or mismatched candidates and never deletes or replaces an app.",
                "inputSchema": setup_ensure_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_manifest_ensure",
                "description": "Submit a bounded daemon-owned ensure of one explicitly named legacy Bosn manifest stack. The supported runtime slice accepts a workspace-contained local TOML manifest, an immutable external image or a workspace Dockerfile (root or nested) whose external images are digest-pinned, declared environment, typed managed volumes, safe workspace binds/workdir, an explicit host Docker socket bind (its resources are outside Bosn supervision), and bounded tmpfs target/ro/rw/size/exec/mode declarations. Dockerfile contexts are copied into daemon-owned content-addressed state; symlinks, other host paths, replacement controls, and arbitrary Docker controls are refused.",
                "inputSchema": manifest_ensure_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_manifest_converge",
                "description": "Submit one bounded daemon-owned convergence of every stack in a workspace-contained legacy Bosn TOML manifest. The current manifest schema has no dependency edges or root selector, so the daemon snapshots the declared stack names and ensures them in deterministic lexical order, one at a time. Each member keeps the normal typed image/build, volumes, workspace mounts/workdir, tmpfs, bounded guest, registry, and rollover safeguards; a later failure leaves earlier successful member records durable. Callers cannot select stack order, Docker arguments, mounts, images, or commands.",
                "inputSchema": manifest_converge_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_manifest_app_task",
                "description": "Submit one named task declared by an already ensured supported manifest stack. The daemon re-reads the manifest and proves exact running ownership before fixed exec; commands, containers, Docker arguments, mounts, and environment controls are refused. Cancellation may leave remote completion unknown.",
                "inputSchema": manifest_app_task_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_task",
                "description": "Submit one bounded daemon-owned setup plan, image-preparation, and declared-task job. Returns promptly with a durable job ID; poll the existing job tools for outcome and logs. It accepts only a named task declared in the setup document, never task commands or engine controls.",
                "inputSchema": setup_task_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            },
            {
                "name": "bosn_setup_app_task",
                "description": "Submit one declared task for execution inside an already ensured Bosn setup application. The daemon re-plans and verifies the exact managed container before a fixed exec; it accepts no command, Docker, or container controls. Returns a durable job ID promptly. Cancelling the local exec client does not establish that the in-container command stopped.",
                "inputSchema": setup_task_schema(),
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
            }
        ]
    });
    if let Some(tools) = list["tools"].as_array_mut() {
        tools.extend(crate::ci::mcp::tools());
    }
    list
}

pub(crate) fn compose_plan_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["document"],
        "properties": {
            "document": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_COMPOSE_DOCUMENT_BYTES, "description": "Complete caller-supplied Compose YAML. Filesystem paths and URLs are not accepted by this tool."}
        }
    })
}

pub(crate) fn setup_plan_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workspace", "config", "policy"],
        "properties": {
            "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES},
            "config": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},
            "policy": {"type": "string", "enum": ["refresh", "offline"], "description": "refresh reads the selected source; offline reuses only its verified cached receipt."}
        }
    })
}

pub(crate) fn setup_prepare_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workspace", "config", "policy", "deadline_ms", "output_limit"],
        "properties": {
            "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES},
            "config": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},
            "policy": {"type": "string", "enum": ["refresh", "offline"]},
            "deadline_ms": {"type": "integer", "minimum": 1, "maximum": 300000},
            "output_limit": {"type": "integer", "minimum": 1, "maximum": 8388608}
        }
    })
}

pub(crate) fn setup_ensure_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workspace", "config", "policy", "deadline_ms", "output_limit"],
        "properties": {
            "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES},
            "config": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},
            "policy": {"type": "string", "enum": ["refresh", "offline"]},
            "deadline_ms": {"type": "integer", "minimum": 1, "maximum": 300000},
            "output_limit": {"type": "integer", "minimum": 1, "maximum": 8388608}
        }
    })
}

pub(crate) fn manifest_ensure_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","manifest","stack","deadline_ms","output_limit"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "manifest":{"type":"string","minLength":1,"maxLength":4096,"description":"Safe relative TOML path beneath workspace; URLs and absolute paths are refused."},
        "stack":{"type":"string","minLength":1,"maxLength":128},
        "deadline_ms":{"type":"integer","minimum":1,"maximum":14400000},
        "output_limit":{"type":"integer","minimum":1,"maximum":67108864}
    }})
}
pub(crate) fn manifest_converge_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","manifest","deadline_ms","output_limit"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "manifest":{"type":"string","minLength":1,"maxLength":4096,"description":"Safe relative TOML path beneath workspace; URLs, dependency selectors, and absolute paths are refused."},
        "deadline_ms":{"type":"integer","minimum":1,"maximum":14400000},
        "output_limit":{"type":"integer","minimum":1,"maximum":67108864}
    }})
}
pub(crate) fn manifest_app_task_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","manifest","stack","task_name","deadline_ms","output_limit"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "manifest":{"type":"string","minLength":1,"maxLength":4096,"description":"Safe relative TOML path beneath workspace; URLs and absolute paths are refused."},
        "stack":{"type":"string","minLength":1,"maxLength":128},
        "task_name":{"type":"string","minLength":1,"maxLength":64,"description":"A declared task belonging to the selected stack."},
        "deadline_ms":{"type":"integer","minimum":1,"maximum":14400000},
        "output_limit":{"type":"integer","minimum":1,"maximum":67108864}
    }})
}

pub(crate) fn setup_task_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["workspace", "config", "policy", "task_name", "deadline_ms", "output_limit"],
        "properties": {
            "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES},
            "config": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},
            "policy": {"type": "string", "enum": ["refresh", "offline"]},
            "task_name": {"type": "string", "minLength": 1, "maxLength": 64, "description": "A declared setup task name: starts alphanumeric and then uses alphanumerics, underscores, or hyphens."},
            "deadline_ms": {"type": "integer", "minimum": 1, "maximum": 300000},
            "output_limit": {"type": "integer", "minimum": 1, "maximum": 8388608}
        }
    })
}

pub(crate) fn job_id_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["job_id"],
        "properties": {"job_id": {"type": "integer", "minimum": 1}}
    })
}

pub(crate) fn registry_page_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "after": {"type": "integer", "minimum": 0, "default": 0, "description": "Opaque offset cursor returned as next; start at 0."},
            "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MCP_REGISTRY_RECORDS, "default": MAX_MCP_REGISTRY_RECORDS}
        }
    })
}
pub(crate) fn setup_gc_preview_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "required": ["workspace"], "properties": {
        "workspace": {"type": "string", "minLength": 1, "maxLength": MAX_MCP_SETUP_STRING_BYTES, "description": "Workspace selector; it is never returned in preview output."},
        "after": {"type": "integer", "minimum": 0, "default": 0},
        "limit": {"type": "integer", "minimum": 1, "maximum": MAX_MCP_REGISTRY_RECORDS, "default": MAX_MCP_REGISTRY_RECORDS}
    }})
}
pub(crate) fn setup_gc_apply_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","candidate_token","confirm"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "candidate_token":{"type":"string","minLength":5,"maxLength":24580},
        "confirm":{"const":true,"description":"Explicit destructive confirmation."}
    }})
}
pub(crate) fn setup_done_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","confirm"],"properties":{
        "workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},
        "confirm":{"const":true,"description":"Explicit state-change confirmation."}
    }})
}
pub(crate) fn setup_adopt_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["workspace","config","policy","deadline_ms","output_limit","confirm"],"properties":{"workspace":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES},"config":{"type":"string","minLength":1,"maxLength":MAX_MCP_SETUP_STRING_BYTES,"description":"An explicit local setup path or HTTPS setup URL. `.yaml`/`.yml` selects only Bosn's supported lossless single-service Compose-to-setup subset; other locators are TOML-only."},"policy":{"type":"string","enum":["refresh","offline"]},"deadline_ms":{"type":"integer","minimum":1,"maximum":300000},"output_limit":{"type":"integer","minimum":1,"maximum":8388608},"confirm":{"const":true}}})
}
