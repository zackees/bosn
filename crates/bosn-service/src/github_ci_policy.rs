//! What the per-run GitHub proxy of `bosn ci run --github-proxy` (#650)
//! lets through, decided before any upstream contact.
//!
//! The job gets a fake `GITHUB_TOKEN` ([`FakeToken`]); the proxy accepts a
//! request only when it carries exactly that token, and then swaps it for
//! the host's real credential. Reads follow the read-only allowlist of
//! [`crate::github_proxy::check_request`] plus a few CI reads scoped to the
//! run's own repository; writes are a closed route+method list, gated by
//! [`GithubWrites`].

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The fake token's fixed prefix. Real GitHub tokens never start with it
/// (they are `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_` or a
/// 40-hex classic token), so it can be told apart without a lookup.
pub const FAKE_TOKEN_PREFIX: &str = "bosn-proxy-";
/// 192 bits of randomness, hex-encoded.
const FAKE_TOKEN_HEX: usize = 48;

/// `--github-writes`: what happens to an allowlisted write.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum GithubWrites {
    /// Refused with 403, like every write outside the allowlist.
    #[default]
    Off,
    /// Logged and answered `204` without being forwarded.
    DryRun,
    /// Forwarded with the real credential.
    On,
}

impl GithubWrites {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "off" => Ok(Self::Off),
            "dry-run" => Ok(Self::DryRun),
            "on" => Ok(Self::On),
            other => Err(format!(
                "--github-writes must be off, dry-run or on, not {other:?}"
            )),
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::DryRun => "dry-run",
            Self::On => "on",
        }
    }
}

/// The per-run fake `GITHUB_TOKEN`: `bosn-proxy-<48 hex>`. It is a
/// capability for this run's proxy only and is masked in the run log like
/// any secret. `Debug` never prints it.
#[derive(Clone, Eq, PartialEq)]
pub struct FakeToken(String);

impl fmt::Debug for FakeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FakeToken(***)")
    }
}

impl FakeToken {
    /// A fresh token from the OS secure random source.
    pub async fn generate() -> Result<Self, String> {
        let bytes = kernal_api::random::SecureRandom::new(1, Duration::from_secs(5))
            .map_err(|_| "secure random unavailable".to_owned())?
            .bytes(FAKE_TOKEN_HEX / 2)
            .await
            .map_err(|_| "secure random unavailable".to_owned())?;
        Ok(Self::from_bytes(&bytes))
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Self(format!("{FAKE_TOKEN_PREFIX}{hex}"))
    }

    #[cfg(test)]
    pub(crate) fn for_test(seed: u8) -> Self {
        Self::from_bytes(&[seed; FAKE_TOKEN_HEX / 2])
    }

    /// The value, for the job's `GITHUB_TOKEN` and the output masker only.
    #[must_use]
    pub fn secret_value(&self) -> &str {
        &self.0
    }

    /// Whether `token` has the fake token's shape (any run's).
    #[must_use]
    pub fn looks_fake(token: &str) -> bool {
        token.strip_prefix(FAKE_TOKEN_PREFIX).is_some_and(|hex| {
            hex.len() == FAKE_TOKEN_HEX
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    }

    /// Constant-time comparison, so response timing says nothing about
    /// how much of a guessed token matched.
    fn matches(&self, presented: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), presented.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

/// What the proxy does with one request.
#[derive(Debug, Eq, PartialEq)]
pub enum Decision {
    /// Send upstream with the real credential.
    Forward,
    /// Answer `204` locally and log it: a write in `--github-writes=dry-run`.
    DryRun,
    /// Answer with this status and reason; nothing goes upstream.
    Refuse(u16, &'static str),
}

/// The per-run policy of a `bosn ci run --github-proxy` proxy.
#[derive(Clone, Debug)]
pub struct CiPolicy {
    pub fake: FakeToken,
    /// The run's own `owner/name`; CI reads and every write are scoped to it.
    pub repository: String,
    pub writes: GithubWrites,
}

/// The largest GraphQL request body accepted (a query, never an upload).
pub const MAX_GRAPHQL_BODY: usize = 64 * 1024;

impl CiPolicy {
    /// Accept only `Authorization: Bearer|token <this run's fake token>`.
    /// A real token, another run's fake token or none is refused: the
    /// container never held a real credential, so it cannot present one,
    /// and a job that sends something else is not using this proxy as
    /// designed.
    pub fn authorize(&self, authorization: Option<&str>) -> Result<(), &'static str> {
        let _ = authorization;
        Ok(()) // RED: stub
    }

    /// Decide one authorized request. `path` is the encoded path without
    /// the query; `body` is the request body (only `POST /graphql` has one).
    pub fn decide(&self, method: &str, path: &str, query: Option<&str>, body: &[u8]) -> Decision {
        let _ = (method, path, query, body);
        Decision::Forward // RED: stub
    }

    /// `/repos/{own}/{rest}` → `rest`, comparing owner and name the way
    /// GitHub does (case-insensitively).
    fn own<'a>(&self, path: &'a str) -> Option<Vec<&'a str>> {
        let mut segments = path.strip_prefix("/repos/")?.split('/');
        let owner = segments.next()?;
        let name = segments.next()?;
        let (own_owner, own_name) = self.repository.split_once('/')?;
        (owner.eq_ignore_ascii_case(own_owner) && name.eq_ignore_ascii_case(own_name))
            .then(|| segments.collect())
    }

    /// The reads the cache janitor needs beyond the read-only allowlist:
    /// the cache list and usage, and pull request state.
    fn is_ci_read(&self, path: &str) -> bool {
        let Some(rest) = self.own(path) else {
            return false;
        };
        matches!(
            rest.as_slice(),
            ["actions", "caches"] | ["actions", "cache", "usage"] | ["pulls"]
        ) || matches!(rest.as_slice(), ["pulls", n] if is_number(n))
    }

    /// `DELETE /repos/{own}/actions/caches/{id}`, or `.../caches?key=...`
    /// (delete by key, optionally narrowed by `ref`).
    fn is_cache_delete(&self, path: &str, query: Option<&str>) -> bool {
        let Some(rest) = self.own(path) else {
            return false;
        };
        match rest.as_slice() {
            ["actions", "caches", id] => is_number(id),
            ["actions", "caches"] => query.is_some_and(|q| {
                q.split('&').any(|pair| {
                    pair.strip_prefix("key=")
                        .is_some_and(|value| !value.is_empty())
                })
            }),
            _ => false,
        }
    }
}

fn is_number(segment: &str) -> bool {
    !segment.is_empty() && segment.len() <= 20 && segment.bytes().all(|b| b.is_ascii_digit())
}

/// A GraphQL request whose document is a query: no `mutation` or
/// `subscription` operation anywhere in it. Deliberately lexical and
/// conservative: a query that merely mentions the word in a string or
/// field name is refused too.
fn graphql_is_query(body: &[u8]) -> bool {
    #[derive(Deserialize)]
    struct Request {
        query: String,
    }
    if body.len() > MAX_GRAPHQL_BODY {
        return false;
    }
    let Ok(request) = serde_json::from_slice::<Request>(body) else {
        return false;
    };
    let lower = request.query.to_ascii_lowercase();
    !lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| word == "mutation" || word == "subscription")
}

#[cfg(test)]
#[path = "github_ci_policy_tests.rs"]
mod tests;
