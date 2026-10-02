//! Execution evidence: job results, the runtime report and private evidence files.

use super::*;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActJobResult {
    pub id: String,
    pub name: String,
    pub matrix: Value,
    pub outcome: String,
    pub reason: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct ActRuntimeReport {
    pub schema_version: u32,
    pub run_id: String,
    pub candidate_sha: String,
    pub engine_id: String,
    pub act_manifest_digest: String,
    pub act_config_digest: String,
    pub act_binary_digest: String,
    pub runner_manifest_digest: String,
    pub runner_config_digest: String,
    pub act_docker_image_id: Option<String>,
    pub runner_docker_image_id: Option<String>,
    pub event_sha256: String,
    pub snapshot_sha256: String,
    pub selection_scope: String,
    pub jobs: Vec<ActJobResult>,
    pub execution_success: bool,
    pub outcome: String,
    pub output_sha256: Option<String>,
    pub engine_removed: bool,
    pub failure: Option<String>,
}
/// Act0.2.88 emits jobID, job, matrix and terminal jobResult JSON fields.
/// Unknown, missing and unsupported results cannot become execution success.
pub fn parse_job_results(bytes: &[u8]) -> std::io::Result<Vec<ActJobResult>> {
    let mut jobs: BTreeMap<String, ActJobResult> = BTreeMap::new();
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|b| !b.iter().all(u8::is_ascii_whitespace))
    {
        let value: Value = serde_json::from_slice(line)
            .map_err(|_| error("Act output is not complete JSON-lines evidence"))?;
        let Some(id) = value["jobID"].as_str() else {
            continue;
        };
        let name = value["job"].as_str().unwrap_or(id);
        let matrix = value.get("matrix").cloned().unwrap_or_else(|| json!({}));
        let key = format!("{name}\0{id}\0{matrix}");
        let job = jobs.entry(key).or_insert_with(|| ActJobResult {
            id: id.into(),
            name: name.into(),
            matrix,
            outcome: "incomplete".into(),
            reason: None,
        });
        if value["msg"]
            .as_str()
            .is_some_and(|m| m.contains("Skipping unsupported platform"))
        {
            job.outcome = "unsupported".into();
            job.reason = value["msg"].as_str().map(str::to_owned);
        } else if let Some(result) = value["jobResult"].as_str() {
            if !matches!(result, "success" | "failure" | "skipped" | "cancelled")
                || (job.outcome != "incomplete" && job.outcome != result)
            {
                return Err(error("conflicting or unknown Act job result"));
            }
            job.outcome = result.into();
        }
    }
    if jobs.len() > 4096 {
        return Err(error("Act job inventory exceeds bounded report"));
    }
    Ok(jobs.into_values().collect())
}
pub(super) fn private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
pub(super) fn private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
pub(super) fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
