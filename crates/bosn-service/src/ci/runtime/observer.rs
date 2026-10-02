//! Streams one run's engine output into its log, parser and published record.

use super::*;

/// Streams one run's output into its log, parser and published record.
pub(crate) struct RunObserver {
    pub(crate) runtime: CiRuntime,
    pub(crate) id: String,
    pub(crate) log: Option<LogWriter>,
    pub(crate) parser: ActParser,
    /// The `--job` filter, if any: other declared jobs are not part of the run.
    pub(crate) job: Option<String>,
    /// Masks secret values in act output before it is parsed or stored.
    pub(crate) masker: SecretMasker,
    pub(crate) seq: u64,
    pub(crate) last_publish: Instant,
}

impl RunObserver {
    pub(crate) fn append(&mut self, record: LogRecord) {
        let offset = self.log.as_mut().and_then(|log| log.append(&record));
        if let Some(offset) = offset
            && record.seq % INDEX_STRIDE == 1
        {
            let mut state = self.runtime.lock();
            if let Some(slot) = state.runs.get_mut(&self.id) {
                slot.index.push((record.seq, offset));
            }
        }
        if self.last_publish.elapsed() > PUBLISH_INTERVAL {
            self.publish();
        }
    }

    /// Flush the log, then make its records and the current tree visible.
    pub(crate) fn publish(&mut self) {
        if let Some(log) = &mut self.log {
            log.flush();
        }
        self.last_publish = Instant::now();
        let (seq, tree) = (self.seq, self.parser.tree.clone());
        self.runtime.update(&self.id, |slot| {
            slot.record.log_records = seq;
            slot.record.tree = tree;
        });
    }

    pub(crate) fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

impl EngineObserver for RunObserver {
    fn note(&mut self, text: &str) {
        let seq = self.next_seq();
        self.append(LogRecord {
            seq,
            stream: "bosn".into(),
            job: None,
            section: None,
            text: text.into(),
        });
    }
    fn declared(&mut self, listing: &str) {
        let jobs = select_jobs(parse_act_list(listing), self.job.as_deref());
        self.parser = ActParser::new(RunTree::declared(&jobs));
        self.publish();
    }
    fn line(&mut self, line: EngineLine) {
        let seq = self.next_seq();
        let record = match line {
            EngineLine::Stdout(text) => self.parser.feed(seq, &self.masker.mask_text(&text)),
            EngineLine::Stderr(text) => LogRecord {
                seq,
                stream: "stderr".into(),
                job: None,
                section: None,
                text: self.masker.mask_text(&text),
            },
        };
        self.append(record);
    }
}
