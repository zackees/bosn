//! #405: bosn's end-of-output trap (#398) starts every POSIX-shell `run:`
//! script in the run's copy of a workflow, and act names an unnamed `run:`
//! step after its script. Steps are still named, and announced, as written.

use super::*;

/// Recorded with act 0.2.88 from bosn's copy of the workflow beside it.
const RECORDED: &str =
    include_str!("../../../tests/fixtures/act/act-0.2.88-unnamed-run-steps.jsonl");
const LISTING: &str = "Stage  Job ID  Job name  Workflow name  Workflow file  Events\n\
                       0      a       a         unnamed        unnamed.yml    push  \n";

#[test]
fn unnamed_trapped_run_steps_are_named_and_announced_as_written() {
    let mut parser = ActParser::new(RunTree::declared(&parse_act_list(LISTING)));
    let records: Vec<LogRecord> = RECORDED
        .lines()
        .enumerate()
        .filter_map(|(i, line)| parser.feed(i as u64 + 1, line))
        .collect();
    let names: Vec<&str> = parser
        .tree
        .jobs()
        .flat_map(|job| job.sections.iter().map(|s| s.name.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            "Set up job",
            "echo one",
            "Named",
            "printf 'fatal: no newline' >&2\nexit 3\n",
            "Complete job",
        ]
    );
    for record in &records {
        assert!(
            !record.text.contains("bosn:eol") && !record.text.contains("trap '"),
            "{:?}",
            record.text
        );
    }
    let texts: Vec<&str> = records.iter().map(|r| r.text.as_str()).collect();
    assert!(texts.contains(&"⭐ Run Main echo one"), "{texts:#?}");
    assert!(texts.contains(&"fatal: no newline"), "{texts:#?}");
}
