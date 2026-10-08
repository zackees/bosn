//! Shared routing identity. Parsing a record does not prove writer exclusion.
use super::{cache_cohort::Namespace, cache_policy::CachePolicy};
use serde::{Deserialize, Serialize};

const MAX_RECORD_BYTES: usize = 4096;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RoutingRecord {
    schema_version: u32,
    namespace: String,
    policy: CachePolicy,
    source_fingerprint: String,
}

impl RoutingRecord {
    /// The caller must establish current inventory and exclusion independently.
    pub fn new(
        namespace: &Namespace,
        policy: CachePolicy,
        source_fingerprint: &str,
    ) -> Result<Self, String> {
        let record = Self {
            schema_version: 1,
            namespace: namespace.as_str().into(),
            policy,
            source_fingerprint: source_fingerprint.into(),
        };
        record.validate(namespace, policy)?;
        Ok(record)
    }

    fn validate(&self, namespace: &Namespace, policy: CachePolicy) -> Result<(), String> {
        if self.schema_version != 1
            || self.namespace != namespace.as_str()
            || self.policy != policy
            || self.source_fingerprint.len() != 64
            || !self
                .source_fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(
                "shared cache route schema, namespace, policy or source identity differs".into(),
            );
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }

    /// An invalid existing publication is an error, never a legacy fallback.
    pub fn parse(bytes: &[u8], namespace: &Namespace, policy: CachePolicy) -> Result<Self, String> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err("shared cache route exceeds bounded record size".into());
        }
        let record: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid shared cache route: {error}"))?;
        record.validate(namespace, policy)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_identity_is_shared_but_cannot_alias_namespace_policy_or_source() {
        let namespace = Namespace::parse("0123456789abcdef").unwrap();
        let policy = CachePolicy::default();
        let record = RoutingRecord::new(&namespace, policy, &"a".repeat(64)).unwrap();
        let encoded = record.encode().unwrap();
        assert_eq!(
            RoutingRecord::parse(&encoded, &namespace, policy).unwrap(),
            record
        );
        let other = Namespace::parse("0123456789abcdee").unwrap();
        assert!(RoutingRecord::parse(&encoded, &other, policy).is_err());
        let mut different = policy;
        different.aggregate_max_bytes += 1;
        assert!(RoutingRecord::parse(&encoded, &namespace, different).is_err());
        let text = String::from_utf8(encoded).unwrap();
        for invalid in [
            text.replace("\"schema_version\":1", "\"schema_version\":2"),
            text.replace(&"a".repeat(64), &"A".repeat(64)),
            text.replace(
                "\"repository_max_bytes\":8589934592",
                "\"repository_max_bytes\":0",
            ),
            text.replacen('{', "{\"unknown\":true,", 1),
            " ".repeat(MAX_RECORD_BYTES + 1),
        ] {
            assert!(RoutingRecord::parse(invalid.as_bytes(), &namespace, policy).is_err());
        }
    }
}
