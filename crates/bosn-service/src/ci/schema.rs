//! The published JSON Schema of the CI contract (`docs/ci.schema.json`),
//! derived from the typed requests and replies so it cannot drift: a test
//! regenerates it and fails when the committed copy differs.

use schemars::{JsonSchema, SchemaGenerator};
use serde_json::{Map, Value, json};

use super::{
    CancelReply, CiRequest, ErrorReply, ListReply, LogsReply, Plan, RunReport, RunView,
    RunnersReply, SubmitReply, UiGrantReply, events::RunEvent,
};

fn sub<T: JsonSchema>(generator: &mut SchemaGenerator) -> Value {
    serde_json::to_value(generator.subschema_for::<T>()).unwrap_or(Value::Null)
}

/// The whole contract: the request union, one reply per operation, the live
/// event and the error, sharing `$defs`.
pub fn document() -> Value {
    let mut generator = SchemaGenerator::default();
    let replies = json!({
        "submit": sub::<SubmitReply>(&mut generator),
        "list": sub::<ListReply>(&mut generator),
        "show": sub::<RunView>(&mut generator),
        "logs": sub::<LogsReply>(&mut generator),
        "cancel": sub::<CancelReply>(&mut generator),
        "retry": sub::<SubmitReply>(&mut generator),
        "report": sub::<RunReport>(&mut generator),
        "runners": sub::<RunnersReply>(&mut generator),
        "ui_grant": sub::<UiGrantReply>(&mut generator),
        "plan": sub::<Plan>(&mut generator),
    });
    let request = sub::<CiRequest>(&mut generator);
    let event = sub::<RunEvent>(&mut generator);
    let error = sub::<ErrorReply>(&mut generator);
    let definitions: Map<String, Value> = generator.take_definitions(true).into_iter().collect();
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "bosn ci",
        "description": "Requests (`bosn ci`, MCP `bosn_ci_*`, the dashboard API), their typed replies, the live event and the error document. `--json` CLI output is the reply of the matching operation.",
        "x-schema-version": super::SCHEMA_VERSION,
        "type": "object",
        "properties": {
            "request": request,
            "replies": {"type": "object", "properties": replies},
            "event": event,
            "error": error,
        },
        "$defs": definitions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLISHED: &str = "../../docs/ci.schema.json";

    /// `BOSN_UPDATE_SCHEMA=1 cargo test -p bosn-service published_schema`
    /// rewrites the committed copy after an intentional contract change.
    #[test]
    fn published_schema_matches_the_types() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(PUBLISHED);
        let generated = serde_json::to_string_pretty(&document()).unwrap() + "\n";
        if std::env::var_os("BOSN_UPDATE_SCHEMA").is_some() {
            std::fs::write(&path, &generated).unwrap();
        }
        let published = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            published == generated,
            "docs/ci.schema.json is stale; rerun with BOSN_UPDATE_SCHEMA=1"
        );
    }

    #[test]
    fn every_operation_and_its_reply_is_published() {
        let document = document();
        let replies = document["properties"]["replies"]["properties"]
            .as_object()
            .unwrap();
        for op in [
            "submit", "list", "show", "logs", "cancel", "retry", "report", "runners", "ui_grant",
        ] {
            assert!(replies.contains_key(op), "{op}");
        }
        let definitions = document["$defs"].as_object().unwrap();
        for name in [
            "RunView",
            "RunTree",
            "Job",
            "Section",
            "Conclusion",
            "LogRecord",
        ] {
            assert!(definitions.contains_key(name), "{name}");
        }
    }
}
