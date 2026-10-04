//! Bounded declarations from the original frozen local reusable-workflow graph.

use std::{
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

use super::{Declared, DeclaredStep, Workflow};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024;
const MAX_CALLS: usize = 128;
const MAX_DEPTH: usize = 32;
const MAX_JOBS: usize = 4096;

struct Reader<'a> {
    source: &'a Path,
    repository: &'a str,
    declared: Declared,
    active: Vec<PathBuf>,
    calls: usize,
    bytes: usize,
    jobs: usize,
}

pub(super) fn declared(source: &Path, workflow: &str, repository: &str) -> Declared {
    let mut reader = Reader {
        source,
        repository,
        declared: Declared::default(),
        active: Vec::new(),
        calls: 0,
        bytes: 0,
        jobs: 0,
    };
    if let Err(error) = reader.visit(Path::new(workflow), &[]) {
        reader.declared.errors.push(error);
    }
    reader.declared
}

pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Reader<'_> {
    fn read(&mut self, relative: &Path) -> Result<(PathBuf, Workflow), String> {
        let mut path = self.source.to_path_buf();
        for component in relative.components() {
            match component {
                Component::CurDir => continue,
                Component::Normal(part) => path.push(part),
                _ => return Err("workflow path escapes the frozen source".into()),
            }
            let metadata = path
                .symlink_metadata()
                .map_err(|_| "workflow path is missing")?;
            if metadata.file_type().is_symlink() {
                return Err("workflow path contains a symlink".into());
            }
        }
        if !path.is_file() || self.active.contains(&path) {
            return Err("workflow is not regular or contains a reusable-call cycle".into());
        }
        let mut bytes = Vec::new();
        File::open(&path)
            .map_err(|_| "workflow cannot be opened")?
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "workflow cannot be read")?;
        self.bytes += bytes.len();
        if bytes.len() > MAX_FILE_BYTES as usize || self.bytes > MAX_TOTAL_BYTES {
            return Err("workflow declaration byte budget exceeded".into());
        }
        let workflow = serde_yaml::from_slice(&bytes).map_err(|_| "workflow cannot be parsed")?;
        Ok((path, workflow))
    }

    fn visit(&mut self, relative: &Path, callers: &[String]) -> Result<(), String> {
        self.calls += 1;
        if self.calls > MAX_CALLS || callers.len() >= MAX_DEPTH {
            return Err("workflow declaration call budget exceeded".into());
        }
        let (path, workflow) = self.read(relative)?;
        self.active.push(path);
        for (id, job) in workflow.jobs {
            self.jobs += 1;
            if self.jobs > MAX_JOBS || !valid_id(&id) {
                self.active.pop();
                return Err("workflow declaration job budget or ID is invalid".into());
            }
            let mut identity = callers.to_vec();
            identity.push(id);
            let key = identity.join("/");
            let reason = super::super::remote_only::reason_in_repository(&job, self.repository);
            let remote = reason.is_some();
            if let Some(reason) = reason {
                self.declared.remote_only.insert(key.clone(), reason);
            }
            let steps = job
                .steps
                .iter()
                .enumerate()
                .filter(|(_, step)| {
                    step.id.as_deref() != Some(super::super::matrix_runner::STEP_ID)
                })
                .map(|(index, step)| DeclaredStep {
                    id: step.id.clone().unwrap_or_else(|| index.to_string()),
                    name: step.display(),
                })
                .collect();
            self.declared.steps.insert(key, steps);
            if let Some(uses) = job.uses {
                self.declared.needs_qualified_identity = true;
                if remote {
                    continue;
                }
                let result = if uses.starts_with("./.github/workflows/")
                    && (uses.ends_with(".yml") || uses.ends_with(".yaml"))
                {
                    self.visit(Path::new(&uses), &identity)
                } else {
                    Err("reusable workflow is remote, dynamic or unsupported".into())
                };
                if let Err(error) = result {
                    self.declared.errors.push(error);
                }
            }
        }
        self.active.pop();
        Ok(())
    }
}
