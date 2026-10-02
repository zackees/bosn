//! Who may use the UI listener.
//!
//! - The daemon issues single-use **grants** (over its owner-only IPC socket)
//!   that expire in a minute. Redeeming one at `/auth` sets an `HttpOnly;
//!   SameSite=Strict` session cookie; the grant cannot be replayed.
//! - Every request's `Host` must name this loopback listener (DNS-rebinding
//!   defence); every write must carry an `Origin` of this listener (CSRF).
//! - Nothing is persisted: a daemon restart invalidates every session.

use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};

pub const COOKIE: &str = "bosn_ui";
const GRANT_TTL: Duration = Duration::from_secs(60);
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_LIVE: usize = 256;

pub struct Auth {
    port: u16,
    grants: Mutex<BTreeMap<String, Instant>>,
    sessions: Mutex<BTreeMap<String, Instant>>,
}

async fn random_token() -> Result<String, String> {
    let bytes = kernal_api::random::SecureRandom::new(1, Duration::from_secs(3))
        .map_err(|_| "secure random unavailable".to_string())?
        .bytes(32)
        .await
        .map_err(|_| "secure random unavailable".to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Insert with an expiry, evicting expired and (if still full) oldest entries.
fn remember(map: &Mutex<BTreeMap<String, Instant>>, key: String, ttl: Duration) {
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    map.retain(|_, expires| *expires > now);
    while map.len() >= MAX_LIVE {
        let oldest = map.iter().min_by_key(|(_, e)| **e).map(|(k, _)| k.clone());
        match oldest {
            Some(key) => map.remove(&key),
            None => break,
        };
    }
    map.insert(key, now + ttl);
}

impl Auth {
    pub fn new(port: u16) -> Self {
        Self {
            port,
            grants: Mutex::new(BTreeMap::new()),
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// A single-use grant for `/auth?token=...`.
    pub async fn grant(&self) -> Result<String, String> {
        let token = random_token().await?;
        remember(&self.grants, token.clone(), GRANT_TTL);
        Ok(token)
    }

    /// Redeem a grant for a new session value; `None` if unknown, expired or
    /// already used.
    pub async fn redeem(&self, grant: &str) -> Option<String> {
        let valid = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(grant)
            .is_some_and(|expires| expires > Instant::now());
        if !valid {
            return None;
        }
        let session = random_token().await.ok()?;
        remember(&self.sessions, session.clone(), SESSION_TTL);
        Some(session)
    }

    /// The session named by a `Cookie` header, if live.
    pub fn session_ok(&self, cookie_header: Option<&str>) -> bool {
        let Some(value) = cookie_header.and_then(cookie_value) else {
            return false;
        };
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(value)
            .is_some_and(|expires| *expires > Instant::now())
    }

    /// `Host` must be exactly this listener (127.0.0.1 or localhost).
    pub fn host_ok(&self, host: Option<&str>) -> bool {
        host.is_some_and(|h| {
            h == format!("127.0.0.1:{}", self.port) || h == format!("localhost:{}", self.port)
        })
    }

    /// A write must name this listener as its `Origin`; a missing `Origin`
    /// is refused.
    pub fn origin_ok(&self, origin: Option<&str>) -> bool {
        origin.is_some_and(|o| o == self.origin() || o == format!("http://localhost:{}", self.port))
    }

    pub fn set_cookie(session: &str) -> String {
        format!("{COOKIE}={session}; HttpOnly; SameSite=Strict; Path=/")
    }
}

fn cookie_value(header: &str) -> Option<&str> {
    header
        .split(';')
        .map(str::trim)
        .find_map(|pair| pair.strip_prefix(COOKIE)?.strip_prefix('='))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        kernal_api::async_engine::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(future)
    }

    #[test]
    fn grants_are_single_use_and_sessions_come_only_from_redeeming() {
        let auth = Auth::new(4000);
        run(async {
            let grant = auth.grant().await.unwrap();
            assert!(auth.redeem("not-a-grant").await.is_none());
            let session = auth.redeem(&grant).await.expect("first redeem");
            assert!(auth.redeem(&grant).await.is_none(), "grants are single-use");
            let header = format!("other=1; {COOKIE}={session}");
            assert!(auth.session_ok(Some(&header)));
            assert!(!auth.session_ok(Some("bosn_ui=forged")));
            assert!(!auth.session_ok(None));
            assert!(Auth::set_cookie(&session).contains("HttpOnly; SameSite=Strict"));
        });
    }

    #[test]
    fn host_and_origin_must_name_this_listener() {
        let auth = Auth::new(4000);
        assert!(auth.host_ok(Some("127.0.0.1:4000")));
        assert!(auth.host_ok(Some("localhost:4000")));
        assert!(!auth.host_ok(Some("evil.example:4000")), "DNS rebinding");
        assert!(!auth.host_ok(Some("127.0.0.1:4001")));
        assert!(!auth.host_ok(None));
        assert!(auth.origin_ok(Some("http://127.0.0.1:4000")));
        assert!(!auth.origin_ok(Some("http://evil.example")));
        assert!(!auth.origin_ok(None), "writes need an Origin");
    }
}
