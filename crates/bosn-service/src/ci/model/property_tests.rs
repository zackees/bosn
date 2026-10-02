//! Randomized property test for the act `--json` parser: random workflows
//! (jobs, matrix legs, Pre/Main/Post stages, nested composite steps, ambient
//! noise before a step starts, job-less and malformed lines) whose legs are
//! interleaved at random. Seeded, so a failure names the seed to replay.

use super::*;

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// One generated line and the section its record must be filed under.
struct Line {
    text: String,
    section: Option<String>,
}

/// A job leg's lines, in act's order for that leg.
fn leg_lines(rng: &mut Rng, key: &str, job_id: &str, matrix: Option<u64>) -> Vec<Line> {
    let head = |stage: &str, ids: &[String]| {
        let matrix = matrix.map_or(String::new(), |m| format!(r#","matrix":{{"leg":{m}}}"#));
        format!(
            r#""job":"{key}","jobID":"{job_id}"{matrix},"stage":"{stage}","stepID":{}"#,
            serde_json::to_string(ids).unwrap()
        )
    };
    let mut lines = Vec::new();
    for stage in ["Pre", "Main", "Post"] {
        if stage != "Main" && rng.chance(50) {
            continue;
        }
        for step in 0..=rng.below(3) {
            let mut ids = vec![step.to_string()];
            if rng.chance(25) {
                ids.push(rng.below(2).to_string()); // a nested composite step
            }
            let owner = format!("{stage}:{}", ids.join("/"));
            let head = head(stage, &ids);
            if rng.chance(30) {
                // Ambient noise before the step starts stays with the job.
                lines.push(Line {
                    text: format!(r#"{{{head},"msg":"git probe"}}"#),
                    section: None,
                });
            }
            lines.push(Line {
                text: format!(r#"{{{head},"msg":"⭐ Run {stage} s{step}","step":"s{step}"}}"#),
                section: Some(owner.clone()),
            });
            for n in 0..rng.below(4) {
                lines.push(Line {
                    text: format!(r#"{{{head},"msg":"out {n}","raw_output":true}}"#),
                    section: Some(owner.clone()),
                });
            }
            let result = if rng.chance(20) { "failure" } else { "success" };
            lines.push(Line {
                text: format!(
                    r#"{{{head},"msg":"done","stepResult":"{result}","executionTime":1000000}}"#
                ),
                section: Some(owner),
            });
        }
    }
    let matrix = matrix.map_or(String::new(), |m| format!(r#","matrix":{{"leg":{m}}}"#));
    lines.push(Line {
        text: format!(
            r#"{{"job":"{key}","jobID":"{job_id}"{matrix},"msg":"🏁","jobResult":"success"}}"#
        ),
        section: None,
    });
    lines
}

/// A random workflow: its `act -l` listing and every leg's lines.
fn workflow(rng: &mut Rng) -> (String, Vec<Vec<Line>>) {
    let mut listing =
        String::from("Stage  Job ID  Job name  Workflow name  Workflow file  Events\n");
    let mut legs = Vec::new();
    for job in 0..=rng.below(3) {
        let id = format!("j{job}");
        listing.push_str(&format!("{job}  {id}  {id}  w  ci.yml  push\n"));
        match rng.below(3) {
            0 => {
                for leg in 1..=2 {
                    let key = format!("w/{id}-{leg}");
                    legs.push(leg_lines(rng, &key, &id, Some(leg)));
                }
            }
            _ => legs.push(leg_lines(rng, &format!("w/{id}"), &id, None)),
        }
    }
    (listing, legs)
}

/// Interleave the legs at random, keeping each leg's own order, and sprinkle
/// job-less and malformed lines. Returns the lines and the malformed count.
fn interleave(rng: &mut Rng, mut legs: Vec<Vec<Line>>) -> (Vec<Line>, usize) {
    for leg in &mut legs {
        leg.reverse();
    }
    let (mut out, mut malformed) = (Vec::new(), 0);
    loop {
        let live: Vec<usize> = (0..legs.len()).filter(|&i| !legs[i].is_empty()).collect();
        let Some(&pick) = live.get(rng.below(live.len().max(1) as u64) as usize) else {
            break;
        };
        out.push(legs[pick].pop().unwrap());
        if rng.chance(5) {
            out.push(Line {
                text: r#"{"msg":"a line with no job"}"#.into(),
                section: None,
            });
        }
        if rng.chance(3) {
            malformed += 1;
            out.push(Line {
                text: r#"{"truncated":"#.into(),
                section: None,
            });
        }
    }
    (out, malformed)
}

#[test]
fn every_record_lands_in_exactly_one_section_and_ranges_never_overlap() {
    for seed in 1..=300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let (listing, legs) = workflow(&mut rng);
        let (lines, malformed) = interleave(&mut rng, legs);
        let mut parser = ActParser::new(RunTree::declared(&parse_act_list(&listing)));
        let records: Vec<LogRecord> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                parser
                    .feed(i as u64 + 1, &line.text)
                    .expect("generated lines never carry bosn's mark")
            })
            .collect();
        assert_eq!(parser.tree.malformed_lines, malformed as u64, "seed {seed}");
        for (record, line) in records.iter().zip(&lines) {
            assert_eq!(
                record.section, line.section,
                "seed {seed}: seq {} {:?}",
                record.seq, line.text
            );
            let Some(section) = &record.section else {
                continue;
            };
            let key = record
                .job
                .as_deref()
                .expect("a sectioned record names its job");
            let job = parser
                .tree
                .jobs()
                .find(|j| j.key == key)
                .expect("its job exists");
            let owners: Vec<&Section> = job
                .sections
                .iter()
                .filter(|s| &format!("{}:{}", s.stage, s.id) == section)
                .collect();
            assert_eq!(owners.len(), 1, "seed {seed}: {section} in {key}");
            let (first, last) = (owners[0].first_seq.unwrap(), owners[0].last_seq.unwrap());
            assert!(
                (first..=last).contains(&record.seq),
                "seed {seed}: seq {} outside {section} {first}..={last}",
                record.seq
            );
        }
        for job in parser.tree.jobs() {
            let mut ranges: Vec<_> = job
                .sections
                .iter()
                .filter_map(|s| Some((s.first_seq?, s.last_seq?)))
                .collect();
            ranges.sort();
            for pair in ranges.windows(2) {
                assert!(
                    pair[0].1 < pair[1].0,
                    "seed {seed}: overlap in {}: {pair:?}",
                    job.key
                );
            }
        }
    }
}
