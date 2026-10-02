use std::{
    collections::VecDeque,
    future::{Ready, ready},
    sync::Mutex,
};

use kernal_api::async_engine::RuntimeBuilder;

use super::*;

const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Default)]
struct ScriptedTransport {
    responses: Mutex<VecDeque<Result<RemoteSetupResponse, RemoteTransportError>>>,
}
impl ScriptedTransport {
    fn with(response: Result<RemoteSetupResponse, RemoteTransportError>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from([response])),
        }
    }
    fn push(&self, response: Result<RemoteSetupResponse, RemoteTransportError>) {
        self.responses.lock().unwrap().push_back(response);
    }
}
impl SetupRemoteTransport for ScriptedTransport {
    type FetchFuture<'a> = Ready<Result<RemoteSetupResponse, RemoteTransportError>>;

    fn fetch<'a>(&'a self, _locator: &'a str, _max_bytes: usize) -> Self::FetchFuture<'a> {
        ready(
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(RemoteTransportError::Unavailable)),
        )
    }
}

fn source(digest: &str) -> Vec<u8> {
    format!("version = 1\n[app]\nimage = 'registry.example/demo@sha256:{digest}'\n").into_bytes()
}
fn run<T>(future: impl Future<Output = T>) -> T {
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(future)
}
fn cache(root: &Path) -> SetupCache {
    SetupCache::under_state_dir(root).unwrap()
}

#[test]
fn local_refresh_is_bounded_cached_and_offline_reuse_is_explicit() {
    let state = tempfile::tempdir().unwrap();
    let input = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(input.path(), source(DIGEST_A)).unwrap();
    let locator = input.path().to_string_lossy().into_owned();
    let cache = cache(state.path());
    let transport = ScriptedTransport::default();

    let first = run(acquire_setup_document(
        &cache,
        &transport,
        &locator,
        SetupAcquirePolicy::OnlineRefresh,
    ))
    .unwrap();
    assert_eq!(first.provenance.source_kind, SetupSourceKind::LocalFile);
    assert_eq!(
        first.provenance.content_sha256,
        sha256_bytes(&source(DIGEST_A)).to_hex()
    );

    std::fs::write(input.path(), source(DIGEST_B)).unwrap();
    let offline = run(acquire_setup_document(
        &cache,
        &transport,
        &locator,
        SetupAcquirePolicy::OfflineCacheOnly,
    ))
    .unwrap();
    assert_eq!(offline, first);

    let refreshed = run(acquire_setup_document(
        &cache,
        &transport,
        &locator,
        SetupAcquirePolicy::OnlineRefresh,
    ))
    .unwrap();
    assert_eq!(
        refreshed.provenance.content_sha256,
        sha256_bytes(&source(DIGEST_B)).to_hex()
    );
    assert_ne!(
        refreshed.provenance.content_sha256,
        offline.provenance.content_sha256
    );
}

#[test]
fn remote_refresh_never_silently_falls_back_to_cache() {
    let state = tempfile::tempdir().unwrap();
    let cache = cache(state.path());
    let locator = "https://configs.example/bosn.toml?revision=1";
    let transport = ScriptedTransport::with(Ok(RemoteSetupResponse {
        bytes: source(DIGEST_A),
        resolved_locator: Some(locator.into()),
    }));
    let first = run(acquire_setup_document(
        &cache,
        &transport,
        locator,
        SetupAcquirePolicy::OnlineRefresh,
    ))
    .unwrap();
    assert_eq!(first.provenance.source_kind, SetupSourceKind::Https);
    assert_eq!(first.provenance.resolved_locator.as_deref(), Some(locator));

    transport.push(Err(RemoteTransportError::Unavailable));
    assert!(matches!(
        run(acquire_setup_document(
            &cache,
            &transport,
            locator,
            SetupAcquirePolicy::OnlineRefresh
        )),
        Err(SetupAcquireError::TransportUnavailable)
    ));
    let offline = run(acquire_setup_document(
        &cache,
        &transport,
        locator,
        SetupAcquirePolicy::OfflineCacheOnly,
    ))
    .unwrap();
    assert_eq!(offline, first);
}

#[test]
fn offline_cache_is_checked_for_tampering_and_locator_binding() {
    let state = tempfile::tempdir().unwrap();
    let cache = cache(state.path());
    let locator = "https://configs.example/bosn.toml?revision=1";
    let transport = ScriptedTransport::with(Ok(RemoteSetupResponse {
        bytes: source(DIGEST_A),
        resolved_locator: None,
    }));
    run(acquire_setup_document(
        &cache,
        &transport,
        locator,
        SetupAcquirePolicy::OnlineRefresh,
    ))
    .unwrap();
    std::fs::write(cache.record_path(locator), b"not a setup cache record").unwrap();
    assert!(matches!(
        run(acquire_setup_document(
            &cache,
            &transport,
            locator,
            SetupAcquirePolicy::OfflineCacheOnly
        )),
        Err(SetupAcquireError::CacheCorrupt)
    ));

    let other = "https://configs.example/other.toml";
    assert!(matches!(
        run(acquire_setup_document(
            &cache,
            &transport,
            other,
            SetupAcquirePolicy::OfflineCacheOnly
        )),
        Err(SetupAcquireError::OfflineCacheUnavailable)
    ));
}

#[test]
fn provenance_redacts_query_credentials_and_rejects_bad_final_urls() {
    let state = tempfile::tempdir().unwrap();
    let cache = cache(state.path());
    let locator = "https://configs.example/bosn.toml?token=top-secret&revision=1";
    let transport = ScriptedTransport::with(Ok(RemoteSetupResponse {
        bytes: source(DIGEST_A),
        resolved_locator: Some(locator.into()),
    }));
    let document = run(acquire_setup_document(
        &cache,
        &transport,
        locator,
        SetupAcquirePolicy::OnlineRefresh,
    ))
    .unwrap();
    assert!(!document.provenance.requested_locator.contains("top-secret"));
    assert!(
        document
            .provenance
            .requested_locator
            .contains("token=[redacted]")
    );

    let bad = ScriptedTransport::with(Ok(RemoteSetupResponse {
        bytes: source(DIGEST_B),
        resolved_locator: Some("http://configs.example/bosn.toml".into()),
    }));
    assert!(matches!(
        run(acquire_setup_document(
            &cache,
            &bad,
            "https://configs.example/new.toml",
            SetupAcquirePolicy::OnlineRefresh
        )),
        Err(SetupAcquireError::ResolvedLocatorInvalid)
    ));
}

#[test]
fn oversized_and_non_utf8_remote_documents_fail_before_cache_write() {
    let state = tempfile::tempdir().unwrap();
    let cache = cache(state.path());
    let locator = "https://configs.example/bosn.toml";
    let large = ScriptedTransport::with(Ok(RemoteSetupResponse {
        bytes: vec![b'x'; MAX_SETUP_DOCUMENT_BYTES + 1],
        resolved_locator: None,
    }));
    assert!(matches!(
        run(acquire_setup_document(
            &cache,
            &large,
            locator,
            SetupAcquirePolicy::OnlineRefresh
        )),
        Err(SetupAcquireError::InputTooLarge)
    ));
    assert!(matches!(
        run(acquire_setup_document(
            &cache,
            &large,
            locator,
            SetupAcquirePolicy::OfflineCacheOnly
        )),
        Err(SetupAcquireError::OfflineCacheUnavailable)
    ));

    let non_utf8 = ScriptedTransport::with(Ok(RemoteSetupResponse {
        bytes: vec![0xff],
        resolved_locator: None,
    }));
    assert!(matches!(
        run(acquire_setup_document(
            &cache,
            &non_utf8,
            locator,
            SetupAcquirePolicy::OnlineRefresh
        )),
        Err(SetupAcquireError::InputNotUtf8)
    ));
}
