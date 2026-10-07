//! Typed capability boundary for the digest-verified act executable.

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Capabilities {
    schema_version: u32,
    producer: String,
    version: String,
    capabilities: Vec<String>,
}

pub(super) fn validate(document: &str, expected_version: &str) -> Result<(), String> {
    if document.len() > 16 * 1024 {
        return Err("act capability document exceeds 16 KiB".into());
    }
    let capabilities: Capabilities = serde_json::from_str(document)
        .map_err(|error| format!("invalid act capability document: {error}"))?;
    if capabilities.schema_version != 1
        || capabilities.producer != "act2"
        || capabilities.version != expected_version
    {
        return Err("act capability schema, producer or pinned version mismatch".into());
    }
    for required in ["qualified-job-identity-v1", "step-stage-result-v1"] {
        if !capabilities
            .capabilities
            .iter()
            .any(|feature| feature == required)
        {
            return Err(format!(
                "act lacks required execution capability {required}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate;

    const DOCUMENT: &str = r#"{"schema_version":1,"producer":"act2","version":"candidate","capabilities":["qualified-job-identity-v1","step-stage-result-v1"]}"#;

    #[test]
    fn requires_both_execution_evidence_features() {
        assert!(validate(DOCUMENT, "candidate").is_ok());
        for feature in ["qualified-job-identity-v1", "step-stage-result-v1"] {
            assert!(validate(&DOCUMENT.replace(feature, "unknown-feature"), "candidate").is_err());
        }
    }

    #[test]
    fn rejects_wrong_identity_schema_and_malformed_output() {
        for document in [
            DOCUMENT.replace("candidate", "other"),
            DOCUMENT.replace("act2", "other"),
            DOCUMENT.replace("schema_version\":1", "schema_version\":2"),
            format!("notice\n{DOCUMENT}"),
            format!("{DOCUMENT}\n{DOCUMENT}"),
            DOCUMENT.replace(
                "\"version\":\"candidate\"",
                "\"version\":\"candidate\",\"version\":\"candidate\"",
            ),
        ] {
            assert!(validate(&document, "candidate").is_err(), "{document}");
        }
    }
}
