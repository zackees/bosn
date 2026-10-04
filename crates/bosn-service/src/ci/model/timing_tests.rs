//! Timing contract from recorded act output, including old persisted trees.

use super::*;

const FIXTURE: &str =
    include_str!("../../../tests/fixtures/act/act-0.2.88-matrix-needs-failure.jsonl");
const LIST: &str = "Stage  Job ID  Job name  Workflow name  Workflow file  Events\n\
                   0      a       a         spike          ci.yml         push  \n\
                   1      b       b         spike          ci.yml         push  \n\
                   1      off     Off job   spike          ci.yml         push  \n";

fn parse(list: &str, lines: &str) -> ActParser {
    let mut parser = ActParser::new(RunTree::declared(&parse_act_list(list)));
    for (i, line) in lines.lines().enumerate() {
        parser.feed(i as u64 + 1, line);
    }
    parser
}

#[test]
fn recorded_act_job_and_step_times_reach_the_status_tree() {
    let parser = parse(LIST, FIXTURE);
    let tree = serde_json::to_value(parser.tree).unwrap();
    let job = tree["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["jobs"].as_array().unwrap())
        .find(|job| job["key"] == "spike/a (one)")
        .unwrap();
    assert_eq!(job["started_at"], "2026-10-02T00:51:18Z");
    assert_eq!(job["completed_at"], "2026-10-02T00:51:19Z");
    assert_eq!(job["duration_ms"], 1000);
    let setup = job["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|section| section["id"] == "--setup-job")
        .unwrap();
    assert_eq!(setup["started_at"], "2026-10-02T00:51:18Z");
    assert_eq!(setup["completed_at"], "2026-10-02T00:51:19Z");
}

#[test]
fn malformed_or_reversed_act_times_do_not_create_a_duration() {
    let lines = [
        r#"{"job":"w/a","jobID":"a","msg":"⭐ Run Main a","stepID":["0"],"time":"2026-10-02T00:51:20Z"}"#,
        r#"{"job":"w/a","jobID":"a","msg":"done","stepID":["0"],"stepResult":"success","time":"invalid"}"#,
        r#"{"job":"w/a","jobID":"a","msg":"🏁","jobResult":"success","time":"2026-10-02T00:51:19Z"}"#,
    ];
    let parser = parse("", &lines.join("\n"));
    let job = parser.tree.jobs().next().unwrap();
    assert_eq!(job.started_at.as_deref(), Some("2026-10-02T00:51:20Z"));
    assert_eq!(job.completed_at.as_deref(), Some("2026-10-02T00:51:19Z"));
    assert_eq!(job.duration_ms, None);
    assert_eq!(job.sections[0].completed_at, None);

    let mut old = serde_json::to_value(job).unwrap();
    let object = old.as_object_mut().unwrap();
    object.remove("started_at");
    object.remove("completed_at");
    object.remove("duration_ms");
    let restored: Job = serde_json::from_value(old).unwrap();
    assert_eq!(restored.started_at, None);
    assert_eq!(restored.completed_at, None);
    assert_eq!(restored.duration_ms, None);
}
