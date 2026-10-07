//! Qualified planner outputs stay attached to their concrete execution.

use super::*;

const OUTPUT: &str = r#"{"msg":"CI output evidence","job":"same name","jobID":"plan","jobIdentity":[{"jobID":"caller","matrix":{"lane":"left"}},{"jobID":"plan","matrix":null}],"ciOutputSchema":1,"jobOutputs":{"matrix":"[]"}}"#;

#[test]
fn output_evidence_is_qualified_durable_and_separate_per_caller_matrix() {
    let mut parser = ActParser::default();
    parser.feed(7, OUTPUT);
    parser.feed(8, &OUTPUT.replace("left", "right").replace("[]", "[1]"));
    assert_eq!(parser.tree.jobs().count(), 2);
    let values: Vec<_> = parser
        .tree
        .jobs()
        .map(|job| job.output_evidence.as_ref().unwrap())
        .collect();
    assert_eq!(values[0].seq, 7);
    assert_eq!(values[0].values["matrix"], "[]");
    assert_eq!(values[1].values["matrix"], "[1]");
    assert!(values.iter().all(|value| value.error.is_none()));
    let bytes = serde_json::to_vec(&parser.tree).unwrap();
    let loaded: RunTree = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(loaded, parser.tree);
    assert!(
        loaded
            .jobs()
            .all(|job| job.conclusion != Some(ItemConclusion::Success))
    );
}

#[test]
fn duplicate_or_refused_output_evidence_never_preserves_usable_values() {
    let mut parser = ActParser::default();
    parser.feed(1, OUTPUT);
    parser.feed(2, OUTPUT);
    let evidence = parser
        .tree
        .jobs()
        .next()
        .unwrap()
        .output_evidence
        .as_ref()
        .unwrap();
    assert!(evidence.error.is_some());
    assert!(evidence.values.is_empty());
    for extra in [
        OUTPUT.replace(
            "\"jobOutputs\":{\"matrix\":\"[]\"}",
            "\"jobOutputsError\":\"masked-output\"",
        ),
        OUTPUT.replace("\"ciOutputSchema\":1", "\"ciOutputSchema\":2"),
        OUTPUT.replace("\"ciOutputSchema\":1", "\"ciOutputSchema\":1,\"stage\":\"Main\""),
        OUTPUT.replace("\"jobIdentity\":[{\"jobID\":\"caller\",\"matrix\":{\"lane\":\"left\"}},{\"jobID\":\"plan\",\"matrix\":null}],", ""),
        OUTPUT.replace(
            "\"ciOutputSchema\":1",
            "\"ciOutputSchema\":1,\"raw_output\":true",
        ),
        OUTPUT.replace(
            "\"ciOutputSchema\":1",
            "\"ciOutputSchema\":1,\"jobOutputsError\":\"missing-output\"",
        ),
    ] {
        let mut parser = ActParser::default();
        parser.feed(1, &extra);
        let evidence = parser
            .tree
            .jobs()
            .next()
            .unwrap()
            .output_evidence
            .as_ref()
            .unwrap();
        assert!(evidence.error.is_some(), "{extra}");
        assert!(evidence.values.is_empty(), "{extra}");
    }
}

#[test]
fn duplicate_output_names_and_wrong_shapes_are_malformed() {
    for extra in [
        OUTPUT.replace("\"matrix\":\"[]\"", "\"matrix\":\"[]\",\"matrix\":\"[1]\""),
        OUTPUT.replace("\"matrix\":\"[]\"", "\"matrix\":true"),
        OUTPUT.replace("\"ciOutputSchema\":1", "\"ciOutputSchema\":true"),
    ] {
        let mut parser = ActParser::default();
        parser.feed(1, &extra);
        assert_eq!(parser.tree.malformed_lines, 1, "{extra}");
    }
}

#[test]
fn compiled_act2_output_events_preserve_concrete_matrix_values() {
    // Exact job-result/output event subset from the compiled PR #54 producer.
    // This checks transport, not full workflow coverage or an attestation.
    let fixture = include_str!("../../../tests/fixtures/act/act2-selected-output-events.jsonl");
    let mut parser = ActParser::default();
    for (seq, line) in fixture.lines().enumerate() {
        parser.feed(seq as u64 + 1, line);
    }
    assert_eq!(parser.tree.malformed_lines, 0);
    assert_eq!(parser.tree.jobs().count(), 3);
    for job in parser.tree.jobs() {
        assert_eq!(job.conclusion, Some(ItemConclusion::Success));
        let evidence = job.output_evidence.as_ref().unwrap();
        assert_eq!(evidence.schema_version, 1);
        assert!(evidence.error.is_none());
        let identity = job.identity.as_ref().unwrap().last().unwrap();
        if job.job_id == "plan" {
            assert_eq!(evidence.values["matrix"], r#"{"lane":["left","right"]}"#);
        } else {
            assert_eq!(job.job_id, "consumer");
            assert_eq!(
                evidence.values["lane"],
                identity.matrix["lane"].as_str().unwrap()
            );
        }
    }
}

#[test]
fn empty_output_events_invalidate_prior_evidence() {
    for replacement in [
        "",
        r#","ciOutputSchema":null,"jobOutputs":null,"jobOutputsError":null"#,
    ] {
        let empty = OUTPUT.replace(
            r#","ciOutputSchema":1,"jobOutputs":{"matrix":"[]"}"#,
            replacement,
        );
        let mut parser = ActParser::default();
        parser.feed(1, OUTPUT);
        parser.feed(2, &empty);
        let evidence = parser
            .tree
            .jobs()
            .next()
            .unwrap()
            .output_evidence
            .as_ref()
            .unwrap();
        assert!(evidence.error.is_some());
        assert!(evidence.values.is_empty());
        let mut parser = ActParser::default();
        parser.feed(1, &empty);
        let evidence = parser
            .tree
            .jobs()
            .next()
            .unwrap()
            .output_evidence
            .as_ref()
            .unwrap();
        assert!(evidence.error.is_some());
        assert!(evidence.values.is_empty());
    }
}
